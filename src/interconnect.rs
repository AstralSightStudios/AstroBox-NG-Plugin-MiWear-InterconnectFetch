use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::astrobox::psys_host::{device, interconnect, register, thirdpartyapp};
use crate::state::{self, InstalledApp};

/// Minimum interval between automatic render-driven refreshes (devices +
/// installed apps + re-register). Click-heavy users won't hammer the host.
const AUTO_REFRESH_THROTTLE_MS: u128 = 1500;

/// AstroBox NG wraps each interconnect-message event payload in a JSON envelope
/// before handing it to the plugin's `on_event`:
///
/// ```json
/// { "addr": "...", "pkgName": "...", "payloadHex": "...", "payloadText": "..." }
/// ```
///
/// The host already knows the exact `(addr, pkgName)` the message came from, so
/// we read both straight off the envelope. That's what lets several QuickApps
/// talk over fetch at the same time without their messages — or, worse, their
/// responses — being misattributed to a single package.
///
/// The legacy heuristic (most-recently-active enabled package) is kept only as
/// a fallback for older hosts whose envelope omits these fields, or for raw
/// payloads that aren't wrapped at all. That path can't disambiguate concurrent
/// QuickApps, which is exactly the bug the explicit fields fix.
pub struct ParsedMessage {
    pub addr: String,
    pub pkg_name: String,
    pub data: String,
}

pub fn parse_message(payload: &str) -> ParsedMessage {
    let envelope: Option<Value> = serde_json::from_str(payload).ok();

    let data = envelope
        .as_ref()
        .and_then(payload_text_from_envelope)
        .unwrap_or_else(|| payload.to_string());

    // Prefer the host-supplied package name; only fall back to the heuristic
    // when the envelope omits it (older host) so we never collapse concurrent
    // QuickApps onto one package.
    let pkg_name = envelope
        .as_ref()
        .and_then(|v| v.get("pkgName"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| guess_pkg_name(&data));

    // Same for the originating device address — authoritative when present.
    let addr = envelope
        .as_ref()
        .and_then(|v| v.get("addr"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| state::first_device_addr_for(&pkg_name).unwrap_or_default());

    ParsedMessage {
        addr,
        pkg_name,
        data,
    }
}

/// Peel the inner message text out of the host envelope: prefer `payloadText`,
/// fall back to a `payload` field (string or stringified object). Returns
/// `None` when neither key is present so the caller can use the raw payload
/// (older hosts / unwrapped messages).
fn payload_text_from_envelope(json: &Value) -> Option<String> {
    if let Some(text) = json.get("payloadText").and_then(|v| v.as_str()) {
        return Some(text.to_string());
    }
    if let Some(payload_value) = json.get("payload") {
        if let Some(text) = payload_value.as_str() {
            return Some(text.to_string());
        }
        return Some(payload_value.to_string());
    }
    None
}

/// Choose which tracked package this message should be attributed to. We
/// prefer the most-recently-active enabled package, then any enabled package,
/// and finally fall back to the legacy `com.fetch` name so the JS protocol
/// still has somewhere to land if the user hasn't picked a package yet.
fn guess_pkg_name(_data: &str) -> String {
    let apps = state::snapshot_apps();
    let mut enabled: Vec<_> = apps.iter().filter(|a| a.enabled).collect();
    enabled.sort_by(|a, b| b.last_seen_unix_ms.cmp(&a.last_seen_unix_ms));
    if let Some(top) = enabled.first() {
        return top.pkg_name.clone();
    }
    state::HISTORICAL_PKG_NAME.to_string()
}

/// Run the full render-driven refresh: pull the device list, ask each device
/// for its installed third-party apps, and re-register every monitored
/// package on every device. Throttled to avoid spamming the host when the UI
/// rerenders rapidly. Returns `true` when it actually did work.
pub fn auto_refresh() -> bool {
    if !state::try_claim_auto_refresh(AUTO_REFRESH_THROTTLE_MS) {
        return false;
    }
    refresh_connected_devices();
    refresh_installed_apps();
    for pkg in state::pkg_names() {
        register_for_all_devices(&pkg);
    }
    true
}

/// Refresh the cached list of installed third-party apps across every
/// connected device. Apps with the same package name across devices show up
/// once per device so the user can tell where they come from.
pub fn refresh_installed_apps() {
    let devices = state::connected_devices();
    let mut out: Vec<InstalledApp> = Vec::new();
    let mut queried_addrs: HashSet<String> = HashSet::new();
    for (addr, device_name) in devices {
        let result = wit_bindgen::block_on(
            thirdpartyapp::get_thirdparty_app_list(&addr).into_future(),
        );
        match result {
            Ok(apps) => {
                queried_addrs.insert(addr.clone());
                for app in apps {
                    out.push(InstalledApp {
                        addr: addr.clone(),
                        device_name: device_name.clone(),
                        package_name: app.package_name,
                        app_name: app.app_name,
                        version_code: app.version_code,
                    });
                }
            }
            Err(()) => {
                tracing::warn!("failed to list third-party apps on {}", addr);
            }
        }
    }
    out.sort_by(|a, b| {
        a.device_name
            .cmp(&b.device_name)
            .then(a.app_name.cmp(&b.app_name))
            .then(a.package_name.cmp(&b.package_name))
    });
    tracing::info!("refreshed installed apps: total={}", out.len());
    state::set_installed_apps(out.clone());

    prune_uninstalled(&out, &queried_addrs);
}

/// Drop monitored packages whose owning device is currently connected but
/// no longer has the app installed. We deliberately leave entries whose
/// `last_addr` is not connected — that device may simply be off, and we
/// shouldn't forget the user's authorization just because of a power-down.
/// Entries with no `last_addr` (manually added, never seen) are kept until
/// they either get a message or are confirmed missing on all connected
/// devices.
fn prune_uninstalled(installed: &[InstalledApp], queried_addrs: &HashSet<String>) {
    if queried_addrs.is_empty() {
        return;
    }

    let installed_pairs: HashSet<(String, String)> = installed
        .iter()
        .map(|app| (app.addr.clone(), app.package_name.clone()))
        .collect();

    let to_prune: Vec<String> = state::snapshot_apps()
        .into_iter()
        .filter(|entry| match &entry.last_addr {
            // Ground truth available for this device: prune if the app is
            // no longer there.
            Some(addr) if queried_addrs.contains(addr) => {
                !installed_pairs.contains(&(addr.clone(), entry.pkg_name.clone()))
            }
            // Device is offline OR entry was manually added without ever
            // receiving a message — keep the user's authorization either way.
            _ => false,
        })
        .map(|entry| entry.pkg_name)
        .collect();

    for pkg in to_prune {
        tracing::info!("auto-prune monitored pkg {pkg}: no longer installed on its device");
        state::remove_app(&pkg);
    }
}

/// Refresh the cached list of connected device addresses.
pub fn refresh_connected_devices() {
    let result = wit_bindgen::block_on(device::get_connected_device_list().into_future());
    let devices = result
        .into_iter()
        .map(|info| (info.addr, info.name))
        .collect::<Vec<_>>();
    if devices.is_empty() {
        tracing::warn!("no connected devices available");
    } else {
        tracing::info!(
            "refreshed connected devices: count={}, sample={:?}",
            devices.len(),
            devices.first()
        );
    }
    state::set_connected_devices(devices);
}

/// Register a single (addr, pkg_name) interconnect-recv pair with the host.
fn register_pair(addr: &str, pkg_name: &str) -> bool {
    let result =
        wit_bindgen::block_on(register::register_interconnect_recv(addr, pkg_name).into_future());
    match result {
        Ok(()) => {
            tracing::info!("registered interconnect-recv addr={} pkg={}", addr, pkg_name);
            true
        }
        Err(()) => {
            tracing::error!(
                "failed to register interconnect-recv addr={} pkg={}",
                addr,
                pkg_name
            );
            false
        }
    }
}

/// Register the given package on every currently connected device. Returns the
/// number of successful registrations.
pub fn register_for_all_devices(pkg_name: &str) -> usize {
    let devices = state::connected_devices();
    if devices.is_empty() {
        // Try once with an empty address as a "match anything" fallback.
        if register_pair("", pkg_name) {
            return 1;
        }
        return 0;
    }
    let mut ok = 0usize;
    for (addr, _) in devices {
        if register_pair(&addr, pkg_name) {
            ok += 1;
        }
    }
    ok
}

/// Send a JSON message back over QAIC to the same (addr, pkg_name) that we
/// received from.
pub fn send_json(addr: &str, pkg_name: &str, tag: &str, body: Value) {
    let mut payload = Map::<String, Value>::new();
    payload.insert("tag".to_string(), Value::String(tag.to_string()));
    match body {
        Value::Object(map) => {
            for (k, v) in map {
                payload.insert(k, v);
            }
        }
        other => {
            payload.insert("data".to_string(), other);
        }
    }
    let text = Value::Object(payload).to_string();
    tracing::info!(
        "interconnect send: addr={} pkg={} tag={} len={}",
        addr,
        pkg_name,
        tag,
        text.len()
    );
    let result = wit_bindgen::block_on(
        interconnect::send_qaic_message(addr, pkg_name, &text).into_future(),
    );
    if result.is_err() {
        tracing::error!(
            "interconnect send failed: addr={} pkg={} tag={}",
            addr,
            pkg_name,
            tag
        );
    }
}
