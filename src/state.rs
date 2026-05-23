use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use serde::{Deserialize, Serialize};

use crate::persist;

/// The package name used by the original interconn-fetch JS plugin. It is no
/// longer auto-registered — kept only as a placeholder string for UI hints.
pub const HISTORICAL_PKG_NAME: &str = "com.fetch";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppEntry {
    pub pkg_name: String,
    pub enabled: bool,
    #[serde(default)]
    pub request_count: u64,
    #[serde(default)]
    pub success_count: u64,
    #[serde(default)]
    pub error_count: u64,
    #[serde(default)]
    pub last_seen_unix_ms: Option<u128>,
    #[serde(default)]
    pub last_addr: Option<String>,
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub last_url: Option<String>,
}

impl AppEntry {
    pub fn new(pkg_name: &str) -> Self {
        Self {
            pkg_name: pkg_name.to_string(),
            enabled: true,
            request_count: 0,
            success_count: 0,
            error_count: 0,
            last_seen_unix_ms: None,
            last_addr: None,
            last_status: None,
            last_url: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct InstalledApp {
    pub addr: String,
    pub device_name: String,
    pub package_name: String,
    pub app_name: String,
    pub version_code: u32,
}

#[derive(Default)]
pub struct PluginState {
    pub apps: HashMap<String, AppEntry>,
    pub root_element_id: Option<String>,
    pub last_notice: Option<String>,
    pub pending_add_pkg: String,
    /// Cached list of (addr, name) for connected devices, refreshed lazily.
    pub connected_devices: Vec<(String, String)>,
    /// All third-party apps the host currently reports installed on each
    /// connected device. Refreshed lazily during UI render.
    pub installed_apps: Vec<InstalledApp>,
    /// Unix-ms timestamp of the last auto-refresh cycle. Used to throttle
    /// repeated render-driven refreshes so rapid UI interactions don't spam
    /// the host.
    pub last_auto_refresh_ms: u128,
    /// Sequence number to make sure UI re-renders reflect outside changes.
    pub render_tick: u64,
}

static STATE: OnceLock<Mutex<PluginState>> = OnceLock::new();

pub fn with_state<R>(f: impl FnOnce(&mut PluginState) -> R) -> R {
    let mutex = STATE.get_or_init(|| Mutex::new(PluginState::default()));
    let mut guard = mutex.lock().unwrap_or_else(|p| p.into_inner());
    f(&mut guard)
}

pub fn ensure_app(pkg_name: &str) {
    let inserted = with_state(|state| {
        let before = state.apps.contains_key(pkg_name);
        state
            .apps
            .entry(pkg_name.to_string())
            .or_insert_with(|| AppEntry::new(pkg_name));
        !before
    });
    if inserted {
        persist_now();
    }
}

/// Bulk-load app entries from disk. Used at startup before any UI render.
pub fn install_loaded_apps(entries: Vec<AppEntry>) {
    with_state(|state| {
        for entry in entries {
            state.apps.insert(entry.pkg_name.clone(), entry);
        }
        state.render_tick = state.render_tick.wrapping_add(1);
    });
}

pub fn is_enabled(pkg_name: &str) -> bool {
    with_state(|state| {
        state
            .apps
            .get(pkg_name)
            .map(|e| e.enabled)
            .unwrap_or(true)
    })
}

pub fn record_request(pkg_name: &str, addr: &str, url: Option<&str>) {
    let now_ms = now_unix_ms();
    with_state(|state| {
        let entry = state
            .apps
            .entry(pkg_name.to_string())
            .or_insert_with(|| AppEntry::new(pkg_name));
        entry.request_count = entry.request_count.saturating_add(1);
        entry.last_seen_unix_ms = Some(now_ms);
        entry.last_addr = Some(addr.to_string());
        if let Some(url) = url {
            entry.last_url = Some(url.to_string());
        }
        state.render_tick = state.render_tick.wrapping_add(1);
    });
    persist_now();
}

pub fn record_result(pkg_name: &str, ok: bool, status: Option<String>) {
    with_state(|state| {
        let entry = state
            .apps
            .entry(pkg_name.to_string())
            .or_insert_with(|| AppEntry::new(pkg_name));
        if ok {
            entry.success_count = entry.success_count.saturating_add(1);
        } else {
            entry.error_count = entry.error_count.saturating_add(1);
        }
        if let Some(status) = status {
            entry.last_status = Some(status);
        }
        state.render_tick = state.render_tick.wrapping_add(1);
    });
    persist_now();
}

/// Snapshot all monitored apps and ask the persist layer to flush them to
/// disk. Called whenever the monitored-app set changes.
pub fn persist_now() {
    let entries = snapshot_apps();
    persist::save_apps(&entries);
}

pub fn set_enabled(pkg_name: &str, enabled: bool) {
    with_state(|state| {
        let entry = state
            .apps
            .entry(pkg_name.to_string())
            .or_insert_with(|| AppEntry::new(pkg_name));
        entry.enabled = enabled;
        state.render_tick = state.render_tick.wrapping_add(1);
    });
    persist_now();
}

pub fn remove_app(pkg_name: &str) {
    let removed = with_state(|state| {
        let removed = state.apps.remove(pkg_name).is_some();
        state.render_tick = state.render_tick.wrapping_add(1);
        removed
    });
    if removed {
        persist_now();
    }
}

pub fn snapshot_apps() -> Vec<AppEntry> {
    with_state(|state| {
        let mut list: Vec<AppEntry> = state.apps.values().cloned().collect();
        list.sort_by(|a, b| a.pkg_name.cmp(&b.pkg_name));
        list
    })
}

pub fn pkg_names() -> Vec<String> {
    with_state(|state| {
        let mut list: Vec<String> = state.apps.keys().cloned().collect();
        list.sort();
        list
    })
}

pub fn set_notice(msg: impl Into<String>) {
    with_state(|state| {
        state.last_notice = Some(msg.into());
        state.render_tick = state.render_tick.wrapping_add(1);
    });
}

pub fn clear_notice() {
    with_state(|state| {
        state.last_notice = None;
    });
}

pub fn set_pending_add(value: String) {
    with_state(|state| state.pending_add_pkg = value);
}

pub fn take_pending_add() -> String {
    with_state(|state| std::mem::take(&mut state.pending_add_pkg))
}

pub fn set_root(element_id: &str) {
    with_state(|state| state.root_element_id = Some(element_id.to_string()));
}

pub fn root() -> Option<String> {
    with_state(|state| state.root_element_id.clone())
}

pub fn set_connected_devices(devices: Vec<(String, String)>) {
    with_state(|state| state.connected_devices = devices);
}

pub fn connected_devices() -> Vec<(String, String)> {
    with_state(|state| state.connected_devices.clone())
}

pub fn set_installed_apps(apps: Vec<InstalledApp>) {
    with_state(|state| state.installed_apps = apps);
}

pub fn installed_apps() -> Vec<InstalledApp> {
    with_state(|state| state.installed_apps.clone())
}

pub fn is_monitored(pkg: &str) -> bool {
    with_state(|state| state.apps.contains_key(pkg))
}

/// Returns `true` and updates the timestamp when the caller is allowed to run
/// an auto-refresh cycle. We throttle to once per `min_interval_ms` so rapid
/// UI interactions don't spam the host with device / app queries.
pub fn try_claim_auto_refresh(min_interval_ms: u128) -> bool {
    let now = now_unix_ms();
    with_state(|state| {
        if now.saturating_sub(state.last_auto_refresh_ms) < min_interval_ms {
            return false;
        }
        state.last_auto_refresh_ms = now;
        true
    })
}

pub fn first_device_addr_for(pkg_name: &str) -> Option<String> {
    with_state(|state| {
        if let Some(entry) = state.apps.get(pkg_name) {
            if let Some(addr) = entry.last_addr.clone() {
                return Some(addr);
            }
        }
        state.connected_devices.first().map(|(a, _)| a.clone())
    })
}

pub fn now_unix_ms() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
