use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::codec::{
    BodyEncoding, Compression, SUPPORTED_COMPRESSIONS, SUPPORTED_ENCODINGS,
};
use crate::interconnect;

/// Reserved tag used by the original interconn-fetch handshake protocol.
pub const HS_TAG: &str = "__hs__";
const HS_TIMEOUT: Duration = Duration::from_millis(3000);

/// Protocol version we advertise.
///   v1 — legacy single-message fetch (base64 + no compression).
///   v2 — adds optional response chunking via `fetch-chunk`.
///   v3 — adds optional `encodings` / `compressions` negotiation.
const LOCAL_PROTOCOL_VERSION: u32 = 3;
/// Whether we support emitting chunked fetch responses.
const LOCAL_CHUNK_SUPPORTED: bool = true;
/// Largest binary payload (in bytes) we will pack into a single chunk message
/// before encoding. Chosen to keep the resulting JSON envelope well under
/// typical QAIC message limits even after a worst-case 2× hex expansion.
const LOCAL_MAX_CHUNK_SIZE: usize = 4096;
/// Lower bound applied to any negotiated chunk size — guards against a peer
/// advertising an absurdly small value.
const MIN_CHUNK_SIZE: usize = 256;

#[derive(Default)]
struct HandshakeState {
    /// Per-(addr, pkg) handshake bookkeeping. Mirrors what the JS plugin tracks
    /// inside its `InterHandshake` instance.
    sessions: HashMap<(String, String), Session>,
}

#[derive(Debug)]
struct Session {
    /// `true` when we believe the QuickApp is in the "open" state and is
    /// allowed to send work.
    open: bool,
    /// Last time we exchanged a handshake packet with this peer.
    last_seen: Instant,
    /// Negotiated capabilities, populated once the peer has advertised its
    /// own `caps`. `None` means the peer hasn't told us anything yet, so we
    /// stay in legacy (single-message, base64) mode.
    caps: Option<NegotiatedCaps>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            open: false,
            last_seen: Instant::now(),
            caps: None,
        }
    }
}

/// Raw view of what the peer told us it supports. Parsed straight off the
/// `caps` JSON; merged with our local capabilities below.
#[derive(Debug, Clone)]
struct PeerCaps {
    protocol_version: u32,
    chunked: bool,
    max_chunk_size: usize,
    /// Encodings the peer can *decode*, in their preferred order. Empty means
    /// the peer didn't advertise — treat as v2-or-earlier (base64 only).
    encodings: Vec<BodyEncoding>,
    /// Compressions the peer can *decompress*, in preferred order. Empty means
    /// the peer didn't advertise — treat as `none` only.
    compressions: Vec<Compression>,
}

/// What both sides agreed on for this session. Stored per-session so each
/// peer can independently pick its CPU-vs-bandwidth tradeoff.
#[derive(Debug, Clone)]
pub struct NegotiatedCaps {
    pub protocol_version: u32,
    pub chunked: bool,
    pub chunk_size: usize,
    /// Encodings the peer accepts, in *peer's* preference order (preferred
    /// first). The producer should walk this list and pick the first one it
    /// also supports. Empty ⇒ peer didn't advertise ⇒ assume base64-only
    /// (v1/v2 baseline).
    pub encodings: Vec<BodyEncoding>,
    /// Same for compression. Empty ⇒ `none`-only.
    pub compressions: Vec<Compression>,
}

static STATE: OnceLock<Mutex<HandshakeState>> = OnceLock::new();

fn state() -> &'static Mutex<HandshakeState> {
    STATE.get_or_init(|| Mutex::new(HandshakeState::default()))
}

fn touch(addr: &str, pkg: &str, open: Option<bool>, caps: Option<NegotiatedCaps>) -> bool {
    let mut guard = state().lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    let key = (addr.to_string(), pkg.to_string());

    // Drop any session that timed out so a stale "open" flag doesn't trick
    // later requests into thinking the peer is still talking to us.
    guard.sessions.retain(|_, s| now.duration_since(s.last_seen) <= HS_TIMEOUT);

    let session = guard.sessions.entry(key).or_insert_with(Session::default);
    session.last_seen = now;
    if let Some(open) = open {
        session.open = open;
    }
    if let Some(caps) = caps {
        session.caps = Some(caps);
    }
    session.open
}

pub fn is_open(addr: &str, pkg: &str) -> bool {
    let guard = state().lock().unwrap_or_else(|p| p.into_inner());
    guard
        .sessions
        .get(&(addr.to_string(), pkg.to_string()))
        .map(|s| s.open && s.last_seen.elapsed() <= HS_TIMEOUT)
        .unwrap_or(false)
}

/// Look up the negotiated capabilities for this peer. Returns `None` when no
/// session exists, the session timed out, or the peer never sent `caps` —
/// callers should treat that as the v1 baseline.
pub fn negotiated_caps(addr: &str, pkg: &str) -> Option<NegotiatedCaps> {
    let guard = state().lock().unwrap_or_else(|p| p.into_inner());
    let session = guard.sessions.get(&(addr.to_string(), pkg.to_string()))?;
    if !session.open || session.last_seen.elapsed() > HS_TIMEOUT {
        return None;
    }
    session.caps.clone()
}

/// Convenience for the chunk-size-only callers. `None` ⇒ no chunking.
pub fn negotiated_chunk_size(addr: &str, pkg: &str) -> Option<usize> {
    let caps = negotiated_caps(addr, pkg)?;
    if caps.chunked && caps.chunk_size > 0 {
        Some(caps.chunk_size)
    } else {
        None
    }
}

/// Handle an incoming handshake packet. Mirrors the JS counter exchange:
///   - any packet with count > 0 marks the session as open
///   - we echo back with `count + 1` while count < 2 so both sides converge
///
/// New in v2: chunking negotiation via `caps`.
/// New in v3: encoding / compression negotiation via `caps.encodings` and
///            `caps.compressions` arrays (peer preference order preserved).
/// Peers that omit `caps` keep the legacy single-message base64 behaviour.
pub fn handle_packet(addr: &str, pkg: &str, packet: &Value) {
    let count_in = packet.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
    let peer_caps = parse_caps(packet.get("caps"));
    let negotiated = peer_caps.map(negotiate);

    let was_open = is_open(addr, pkg);
    let opened = if count_in > 0 { Some(true) } else { None };
    touch(addr, pkg, opened, negotiated.clone());

    if !was_open && count_in > 0 {
        tracing::info!(
            "handshake opened: addr={} pkg={} count={} chunked={} chunk_size={} encs={:?} comps={:?}",
            addr,
            pkg,
            count_in,
            negotiated.as_ref().map(|c| c.chunked).unwrap_or(false),
            negotiated.as_ref().map(|c| c.chunk_size).unwrap_or(0),
            negotiated
                .as_ref()
                .map(|c| c.encodings.iter().map(|e| e.as_str()).collect::<Vec<_>>())
                .unwrap_or_default(),
            negotiated
                .as_ref()
                .map(|c| c.compressions.iter().map(|x| x.as_str()).collect::<Vec<_>>())
                .unwrap_or_default(),
        );
    }

    if count_in < 2 {
        let next = count_in + 1;
        interconnect::send_json(
            addr,
            pkg,
            HS_TAG,
            json!({
                "count": next,
                "caps": local_caps_value(),
            }),
        );
    }
}

/// Make sure the handshake is open before allowing a request to flow.
/// If we have no session yet, kick one off by sending a count=0 packet and
/// optimistically assume it will succeed (the watch side replies very
/// quickly in practice; the JS version doesn't actually await the response).
pub fn ensure_open(addr: &str, pkg: &str) {
    if is_open(addr, pkg) {
        return;
    }
    tracing::info!("handshake bootstrap: addr={} pkg={}", addr, pkg);
    interconnect::send_json(
        addr,
        pkg,
        HS_TAG,
        json!({
            "count": 0,
            "caps": local_caps_value(),
        }),
    );
    // Optimistically mark as open so the immediate response can ship; if the
    // watch never answers we'll time out naturally. Caps stay `None` until the
    // peer actually replies with their own — that means the immediately-sent
    // response uses the legacy single-message base64 path, which is the only
    // safe assumption when we don't know what the peer can handle.
    touch(addr, pkg, Some(true), None);
}

fn parse_caps(v: Option<&Value>) -> Option<PeerCaps> {
    let obj = v?.as_object()?;
    Some(PeerCaps {
        protocol_version: obj
            .get("version")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u32,
        chunked: obj
            .get("chunk")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        max_chunk_size: obj
            .get("maxChunkSize")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
        encodings: parse_string_list(obj.get("encodings"), BodyEncoding::parse),
        compressions: parse_string_list(obj.get("compressions"), Compression::parse),
    })
}

fn parse_string_list<T>(v: Option<&Value>, parse_one: fn(&str) -> Option<T>) -> Vec<T> {
    v.and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.as_str().and_then(parse_one))
                .collect()
        })
        .unwrap_or_default()
}

fn negotiate(peer: PeerCaps) -> NegotiatedCaps {
    let version = peer.protocol_version.min(LOCAL_PROTOCOL_VERSION);
    let chunked = LOCAL_CHUNK_SUPPORTED && peer.chunked && version >= 2;
    let chunk_size = if chunked {
        let peer_max = if peer.max_chunk_size == 0 {
            LOCAL_MAX_CHUNK_SIZE
        } else {
            peer.max_chunk_size
        };
        peer_max.min(LOCAL_MAX_CHUNK_SIZE).max(MIN_CHUNK_SIZE)
    } else {
        0
    };

    // Keep only those entries the local side actually implements, preserving
    // the peer's preference order. v<3 peers won't have sent these arrays at
    // all — that's fine, the producer falls back to base64 + none in that
    // case.
    let encodings: Vec<BodyEncoding> = peer
        .encodings
        .into_iter()
        .filter(|e| SUPPORTED_ENCODINGS.contains(e))
        .collect();
    let compressions: Vec<Compression> = peer
        .compressions
        .into_iter()
        .filter(|c| SUPPORTED_COMPRESSIONS.contains(c))
        .collect();

    NegotiatedCaps {
        protocol_version: version,
        chunked,
        chunk_size,
        encodings,
        compressions,
    }
}

fn local_caps_value() -> Value {
    let encodings: Vec<&'static str> = SUPPORTED_ENCODINGS.iter().map(|e| e.as_str()).collect();
    let compressions: Vec<&'static str> =
        SUPPORTED_COMPRESSIONS.iter().map(|c| c.as_str()).collect();
    json!({
        "version": LOCAL_PROTOCOL_VERSION,
        "chunk": LOCAL_CHUNK_SUPPORTED,
        "maxChunkSize": LOCAL_MAX_CHUNK_SIZE,
        "encodings": encodings,
        "compressions": compressions,
    })
}
