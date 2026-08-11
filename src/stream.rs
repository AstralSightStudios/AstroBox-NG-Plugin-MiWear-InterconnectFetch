//! Protocol v4 open-ended response streaming.
//!
//! Unlike the finite v2/v3 chunk path, this module never owns the complete
//! response body. It keeps the HTTP input stream open and buffers at most one
//! negotiated ACK window, so live media and arbitrarily large files use bounded
//! memory. Every data frame (including the final marker) participates in the
//! cumulative ACK sequence and can be retransmitted.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use serde_json::{Map, Value};
use waki::Response;

use crate::codec::{self, BodyEncoding};
use crate::interconnect;

pub const FETCH_STREAM_TAG: &str = "fetch-stream";
pub const FETCH_STREAM_ACK_TAG: &str = "fetch-stream-ack";
pub const FETCH_STREAM_CANCEL_TAG: &str = "fetch-stream-cancel";
pub const FETCH_STREAM_ERROR_TAG: &str = "fetch-stream-error";

const STREAM_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound aggregate memory as well as each stream's per-window memory.
const MAX_CONCURRENT_STREAMS: usize = 8;

#[derive(Clone)]
struct StreamFrame {
    seq: usize,
    bytes: Vec<u8>,
    final_frame: bool,
    total_bytes: usize,
}

struct PendingStream {
    source: Response,
    chunk_size: usize,
    encoding: BodyEncoding,
    window: usize,
    next: usize,
    base: usize,
    total_bytes: usize,
    final_queued: bool,
    unacked: VecDeque<StreamFrame>,
    last_activity: Instant,
    retx_base: Option<usize>,
}

#[derive(Default)]
struct Registry {
    streams: HashMap<(String, String, String), PendingStream>,
}

static STATE: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    STATE.get_or_init(|| Mutex::new(Registry::default()))
}

fn key(addr: &str, pkg: &str, id: &str) -> (String, String, String) {
    (addr.to_string(), pkg.to_string(), id.to_string())
}

fn prune(reg: &mut Registry, now: Instant) {
    reg.streams.retain(|k, stream| {
        let alive = now.duration_since(stream.last_activity) <= STREAM_TIMEOUT;
        if !alive {
            tracing::warn!(
                "v4 stream dropped (idle > {}s): addr={} pkg={} id={} ack={}",
                STREAM_TIMEOUT.as_secs(),
                k.0,
                k.1,
                k.2,
                stream.base,
            );
        }
        alive
    });
}

/// Fill only the currently available receive window. Reading stops immediately
/// when the window is full; the next HTTP read is driven by a later peer ACK.
fn fill_window(stream: &mut PendingStream) -> Result<Vec<StreamFrame>, String> {
    let mut sends = Vec::new();
    while stream.unacked.len() < stream.window && !stream.final_queued {
        let chunk = stream
            .source
            .chunk(stream.chunk_size as u64)
            .map_err(|e| format!("read streaming body failed: {e}"))?;

        let frame = match chunk {
            Some(bytes) if !bytes.is_empty() => {
                stream.total_bytes = stream.total_bytes.saturating_add(bytes.len());
                StreamFrame {
                    seq: stream.next,
                    bytes,
                    final_frame: false,
                    total_bytes: stream.total_bytes,
                }
            }
            Some(_) => continue,
            None => {
                stream.final_queued = true;
                StreamFrame {
                    seq: stream.next,
                    bytes: Vec::new(),
                    final_frame: true,
                    total_bytes: stream.total_bytes,
                }
            }
        };
        stream.next += 1;
        stream.unacked.push_back(frame.clone());
        sends.push(frame);
    }
    Ok(sends)
}

async fn flush(
    addr: &str,
    pkg: &str,
    id: &str,
    encoding: BodyEncoding,
    frames: Vec<StreamFrame>,
) -> bool {
    for frame in frames {
        let data = codec::encode(&frame.bytes, encoding)
            .unwrap_or_else(|_| codec::encode(&frame.bytes, BodyEncoding::Base64).unwrap());
        let mut msg = Map::new();
        msg.insert("id".into(), Value::String(id.to_string()));
        msg.insert("seq".into(), Value::from(frame.seq));
        msg.insert("data".into(), Value::String(data));
        msg.insert(
            "crc32".into(),
            Value::String(format!("{:08x}", codec::crc32(&frame.bytes))),
        );
        if frame.final_frame {
            msg.insert("final".into(), Value::Bool(true));
            msg.insert("totalBytes".into(), Value::from(frame.total_bytes));
        }
        if !interconnect::send_json(addr, pkg, FETCH_STREAM_TAG, Value::Object(msg)).await {
            return false;
        }
    }
    true
}

async fn send_stream_error(addr: &str, pkg: &str, id: &str, message: &str) {
    let mut msg = Map::new();
    msg.insert("id".into(), Value::String(id.to_string()));
    msg.insert("message".into(), Value::String(message.to_string()));
    interconnect::send_json(addr, pkg, FETCH_STREAM_ERROR_TAG, Value::Object(msg)).await;
}

/// Register an HTTP response and send the first bounded window. The caller must
/// send the v4 `fetch` stream header before calling this function.
pub async fn begin(
    addr: &str,
    pkg: &str,
    id: &str,
    source: Response,
    chunk_size: usize,
    encoding: BodyEncoding,
    window: usize,
) {
    let now = Instant::now();
    let result = {
        let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        prune(&mut reg, now);
        let stream_key = key(addr, pkg, id);
        if !reg.streams.contains_key(&stream_key) && reg.streams.len() >= MAX_CONCURRENT_STREAMS {
            Err(format!(
                "too many concurrent v4 streams (limit {MAX_CONCURRENT_STREAMS})"
            ))
        } else {
            let mut stream = PendingStream {
                source,
                chunk_size: chunk_size.max(1),
                encoding,
                window: window.max(1),
                next: 0,
                base: 0,
                total_bytes: 0,
                final_queued: false,
                unacked: VecDeque::new(),
                last_activity: now,
                retx_base: None,
            };
            let sends = fill_window(&mut stream);
            if sends.is_ok() {
                reg.streams.insert(stream_key, stream);
            }
            sends
        }
    };

    match result {
        Ok(sends) => {
            tracing::info!(
                "v4 stream begin: addr={} pkg={} id={} window={} primed={}",
                addr,
                pkg,
                id,
                window,
                sends.len(),
            );
            if !flush(addr, pkg, id, encoding, sends).await {
                cancel(addr, pkg, id);
            }
        }
        Err(err) => send_stream_error(addr, pkg, id, &err).await,
    }
}

/// Advance the cumulative stream ACK and read at most enough HTTP bytes to
/// refill the negotiated window. Duplicate ACKs retransmit the current window
/// once per stall point.
pub async fn on_ack(addr: &str, pkg: &str, id: &str, ack: usize) {
    let now = Instant::now();
    let result = (|| -> Result<Option<(BodyEncoding, Vec<StreamFrame>, bool)>, String> {
        let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        prune(&mut reg, now);
        let k = key(addr, pkg, id);
        let Some(stream) = reg.streams.get_mut(&k) else {
            return Ok(None);
        };
        stream.last_activity = now;
        let ack = ack.min(stream.next);
        let mut sends = Vec::new();

        if ack > stream.base {
            stream.base = ack;
            stream.retx_base = None;
            while stream
                .unacked
                .front()
                .map(|frame| frame.seq < ack)
                .unwrap_or(false)
            {
                stream.unacked.pop_front();
            }
            sends = fill_window(stream)?;
        } else if !stream.unacked.is_empty() && stream.retx_base != Some(stream.base) {
            stream.retx_base = Some(stream.base);
            sends.extend(stream.unacked.iter().cloned());
        }

        let encoding = stream.encoding;
        let done = stream.final_queued && stream.unacked.is_empty();
        if done {
            reg.streams.remove(&k);
        }
        Ok(Some((encoding, sends, done)))
    })();

    let (encoding, sends, done) = match result {
        Ok(Some(result)) => result,
        Ok(None) => return,
        Err(err) => {
            cancel(addr, pkg, id);
            send_stream_error(addr, pkg, id, &err).await;
            return;
        }
    };

    if !flush(addr, pkg, id, encoding, sends).await {
        cancel(addr, pkg, id);
        return;
    }
    if done {
        tracing::info!("v4 stream complete: addr={} pkg={} id={}", addr, pkg, id);
    }
}

pub fn cancel(addr: &str, pkg: &str, id: &str) {
    let removed = registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .streams
        .remove(&key(addr, pkg, id))
        .is_some();
    if removed {
        tracing::info!("v4 stream cancelled: addr={} pkg={} id={}", addr, pkg, id);
    }
}

/// Called from host timer events so an abandoned stream is reclaimed even if
/// no subsequent fetch or ACK ever touches the registry.
pub fn prune_idle() {
    let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    prune(&mut reg, Instant::now());
}
