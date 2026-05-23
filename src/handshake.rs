use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use serde_json::json;

use crate::interconnect;

/// Reserved tag used by the original interconn-fetch handshake protocol.
pub const HS_TAG: &str = "__hs__";
const HS_TIMEOUT: Duration = Duration::from_millis(3000);

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
}

impl Default for Session {
    fn default() -> Self {
        Self {
            open: false,
            last_seen: Instant::now(),
        }
    }
}

static STATE: OnceLock<Mutex<HandshakeState>> = OnceLock::new();

fn state() -> &'static Mutex<HandshakeState> {
    STATE.get_or_init(|| Mutex::new(HandshakeState::default()))
}

fn touch(addr: &str, pkg: &str, open: Option<bool>) -> bool {
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

/// Handle an incoming handshake packet. Mirrors the JS counter exchange:
///   - any packet with count > 0 marks the session as open
///   - we echo back with `count + 1` while count < 2 so both sides converge
pub fn handle_packet(addr: &str, pkg: &str, count_in: i64) {
    let was_open = is_open(addr, pkg);
    let opened = if count_in > 0 { Some(true) } else { None };
    touch(addr, pkg, opened);

    if !was_open && count_in > 0 {
        tracing::info!(
            "handshake opened: addr={} pkg={} initial_count={}",
            addr,
            pkg,
            count_in
        );
    }

    if count_in < 2 {
        let next = count_in + 1;
        interconnect::send_json(addr, pkg, HS_TAG, json!({ "count": next }));
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
    interconnect::send_json(addr, pkg, HS_TAG, json!({ "count": 0 }));
    // Optimistically mark as open so the immediate response can ship; if the
    // watch never answers we'll time out naturally.
    touch(addr, pkg, Some(true));
}
