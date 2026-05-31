//! Windowed, ACK-paced delivery of chunked fetch responses (v3).
//!
//! ## Why this module exists
//!
//! The v2 chunking path used to blast every `fetch-chunk` frame back-to-back
//! inside a single `on_event` call (a tight `for` loop, each frame shipped via
//! a blocking `send_qaic_message`). On a large response that floods the QAIC /
//! BLE send queue far faster than the watch can drain it. Because the plugin
//! never yields control back to the host *between* frames, the host can't pump
//! the transport, the queue never drains, and the whole transfer wedges — the
//! "no peer ACK ⇒ deadlock" bug.
//!
//! v3 fixes it with an application-level **cumulative ACK + sliding window**:
//!
//!   * The sender keeps at most `window` chunks in flight (sent but not yet
//!     acknowledged), then *returns* from `on_event`.
//!   * The peer replies with a `fetch-ack` carrying the next contiguous chunk
//!     index it still needs (= count of chunks it has received in order).
//!   * Each ACK that advances the window unblocks the next batch of chunks.
//!
//! This bounds in-flight bytes and, crucially, hands control back to the host
//! between bursts so the transport actually drains. Loss is recovered with a
//! go-back-N retransmit issued at most once per stall point, driven off the
//! duplicate ACKs a gap produces (the underlying QAIC/BLE link is reliable, so
//! this is a safety net rather than the common case).
//!
//! Peers that do not advertise `caps.ack` never reach this module — the sender
//! keeps using the legacy un-paced path for them (see `fetch::send_chunked`).

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use serde_json::{Map, Value};

use crate::codec::{self, BodyEncoding};
use crate::fetch::FETCH_CHUNK_TAG;
use crate::interconnect;

/// A transfer with no ACK activity for this long is presumed dead (the peer
/// went away or abandoned the fetch) and dropped, so its buffered payload can't
/// leak forever. Matches the handshake's liveness window in spirit.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

/// One in-flight chunked response. Keyed by `(addr, pkg, id)` so concurrent
/// fetches from the same peer don't collide.
struct PendingTransfer {
    /// Post-compression bytes, sliced into `chunk_size` pieces on demand. Held
    /// until fully acked so we can retransmit without re-running the request.
    payload: Vec<u8>,
    chunk_size: usize,
    chunk_count: usize,
    /// Wire encoding for each chunk's `data` field (base64 or hex; never text —
    /// chunked payloads are always binary).
    encoding: BodyEncoding,
    /// Max chunks allowed in flight (sent but not yet cumulatively acked).
    window: usize,
    /// First not-yet-acked chunk index = the peer's cumulative ACK value. The
    /// peer has confirmed receipt of every chunk with `seq < base`.
    base: usize,
    /// Next chunk index to put on the wire.
    next: usize,
    /// Last time the peer ACKed (or we started); drives the idle timeout.
    last_activity: Instant,
    /// The `base` value we last issued a go-back-N retransmit for. Lets us
    /// retransmit a stalled window exactly once per stall point — `base` is
    /// monotonic, so a fresh stall always has a new value and the chatty stream
    /// of duplicate ACKs a single loss produces can't trigger a retransmit
    /// storm.
    retx_base: Option<usize>,
}

#[derive(Default)]
struct Registry {
    transfers: HashMap<(String, String, String), PendingTransfer>,
}

static STATE: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    STATE.get_or_init(|| Mutex::new(Registry::default()))
}

fn key(addr: &str, pkg: &str, id: Option<&str>) -> (String, String, String) {
    (
        addr.to_string(),
        pkg.to_string(),
        id.unwrap_or("").to_string(),
    )
}

/// Drop transfers that haven't seen an ACK within `TRANSFER_TIMEOUT`. Called
/// whenever we touch the registry so a vanished peer can't pin memory.
fn prune(reg: &mut Registry, now: Instant) {
    reg.transfers.retain(|k, t| {
        let alive = now.duration_since(t.last_activity) <= TRANSFER_TIMEOUT;
        if !alive {
            tracing::warn!(
                "transfer dropped (idle > {}s): addr={} pkg={} id={} base={}/{}",
                TRANSFER_TIMEOUT.as_secs(),
                k.0,
                k.1,
                k.2,
                t.base,
                t.chunk_count,
            );
        }
        alive
    });
}

/// Advance `next` as far as the window and chunk count allow, collecting the
/// chunk byte-ranges to ship. Returns `(seq, bytes)` pairs to encode + send
/// *after* the registry lock is released — we never run a blocking
/// `send_qaic_message` while holding the lock.
fn pump(t: &mut PendingTransfer) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    while t.next < t.base + t.window && t.next < t.chunk_count {
        let start = t.next * t.chunk_size;
        let end = (start + t.chunk_size).min(t.payload.len());
        out.push((t.next, t.payload[start..end].to_vec()));
        t.next += 1;
    }
    out
}

/// Encode and ship the collected chunks. Runs outside the registry lock.
fn flush(
    addr: &str,
    pkg: &str,
    id: Option<&str>,
    encoding: BodyEncoding,
    chunk_count: usize,
    sends: Vec<(usize, Vec<u8>)>,
) {
    for (seq, bytes) in sends {
        // `encode` only fails for Text, which never reaches a chunked transfer;
        // base64 is the universal fallback either way.
        let data = codec::encode(&bytes, encoding)
            .unwrap_or_else(|_| codec::encode(&bytes, BodyEncoding::Base64).unwrap());
        let mut msg = Map::new();
        if let Some(id) = id {
            msg.insert("id".to_string(), Value::String(id.to_string()));
        }
        msg.insert("seq".to_string(), Value::from(seq));
        msg.insert("total".to_string(), Value::from(chunk_count));
        msg.insert("data".to_string(), Value::String(data));
        interconnect::send_json(addr, pkg, FETCH_CHUNK_TAG, Value::Object(msg));
    }
}

/// Register a new chunked transfer and ship the first window of chunks. The
/// caller must already have sent the `tag:"fetch"` header frame (including
/// `ack:true`) so the peer knows to acknowledge.
///
/// `payload` is the post-compression body; `chunk_size`, `encoding` and
/// `window` come straight from the negotiated transfer plan.
pub fn begin(
    addr: &str,
    pkg: &str,
    id: Option<&str>,
    payload: Vec<u8>,
    chunk_size: usize,
    encoding: BodyEncoding,
    window: usize,
) {
    let chunk_size = chunk_size.max(1);
    let window = window.max(1);
    let chunk_count = payload.len().div_ceil(chunk_size);
    if chunk_count == 0 {
        return;
    }

    let now = Instant::now();
    let (chunk_count, sends) = {
        let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        prune(&mut reg, now);

        let mut transfer = PendingTransfer {
            payload,
            chunk_size,
            chunk_count,
            encoding,
            window,
            base: 0,
            next: 0,
            last_activity: now,
            retx_base: None,
        };
        let sends = pump(&mut transfer);
        let cc = transfer.chunk_count;
        // Insert (replacing any stale transfer reusing this id) and keep going.
        reg.transfers.insert(key(addr, pkg, id), transfer);
        (cc, sends)
    };

    tracing::info!(
        "transfer begin: addr={} pkg={} id={} chunks={} window={} primed={}",
        addr,
        pkg,
        id.unwrap_or(""),
        chunk_count,
        window,
        sends.len(),
    );

    flush(addr, pkg, id, encoding, chunk_count, sends);
}

/// Handle a peer `fetch-ack`. `ack` is the next contiguous chunk index the peer
/// still needs (i.e. it has received every chunk with `seq < ack`).
///
///   * `ack > base` ⇒ the window slid forward; ship the next batch.
///   * no forward progress, but chunks are still outstanding ⇒ the peer is
///     stalled on a gap. Replay the window from `base` (go-back-N) — the peer
///     buffers out-of-order chunks, so this fills the hole and it re-acks past
///     it. Done at most once per stall point (`retx_base`) so the burst of
///     duplicate ACKs a single loss produces can't trigger a retransmit storm.
///   * `base` reaching `chunk_count` ⇒ fully delivered; drop the transfer.
///
/// Unknown ids are ignored (a late ACK for a finished/timed-out transfer).
pub fn on_ack(addr: &str, pkg: &str, id: Option<&str>, ack: usize) {
    let now = Instant::now();
    let result = {
        let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        prune(&mut reg, now);

        let k = key(addr, pkg, id);
        let Some(t) = reg.transfers.get_mut(&k) else {
            return;
        };
        t.last_activity = now;
        let ack = ack.min(t.chunk_count);

        let mut sends = Vec::new();
        if ack > t.base {
            t.base = ack;
            sends = pump(t);
        } else if t.next > t.base && t.retx_base != Some(t.base) {
            // Peer is still waiting on `base` while we have unacked chunks out:
            // assume the gap chunk was dropped and replay the window. Tagging
            // `retx_base` means the follow-up duplicate ACKs for this same gap
            // are no-ops until `base` actually advances.
            t.retx_base = Some(t.base);
            t.next = t.base;
            sends = pump(t);
        }

        let encoding = t.encoding;
        let chunk_count = t.chunk_count;
        let done = t.base >= t.chunk_count;
        if done {
            reg.transfers.remove(&k);
        }
        (encoding, chunk_count, sends, done)
    };

    let (encoding, chunk_count, sends, done) = result;
    flush(addr, pkg, id, encoding, chunk_count, sends);

    if done {
        tracing::info!(
            "transfer complete: addr={} pkg={} id={} chunks={}",
            addr,
            pkg,
            id.unwrap_or(""),
            chunk_count,
        );
    }
}
