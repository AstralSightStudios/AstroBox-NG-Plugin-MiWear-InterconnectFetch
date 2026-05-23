use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::state::AppEntry;

/// Where we persist the monitored-app configuration. The path is intentionally
/// relative so it lands inside whatever working directory the AstroBox host
/// hands the plugin (WASI preopened plugin-data dir).
const CONFIG_PATH: &str = "./miwear-interconn-fetch.config.json";
const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct OnDiskConfig {
    /// Bumped any time we make a backwards-incompatible field change.
    version: u32,
    #[serde(default)]
    monitored: Vec<AppEntry>,
}

/// Load the saved monitored-app list from disk. Returns an empty list if the
/// file is missing or unreadable, so first-run plugins start clean.
pub fn load_apps() -> Vec<AppEntry> {
    let path = Path::new(CONFIG_PATH);
    if !path.exists() {
        tracing::info!("persist: no config file at {} (first run)", CONFIG_PATH);
        return Vec::new();
    }

    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!("persist: failed to read {}: {}", CONFIG_PATH, err);
            return Vec::new();
        }
    };

    let config: OnDiskConfig = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!("persist: failed to parse {}: {}", CONFIG_PATH, err);
            return Vec::new();
        }
    };

    if config.version != CURRENT_VERSION {
        tracing::warn!(
            "persist: ignoring config with version {} (expected {})",
            config.version,
            CURRENT_VERSION
        );
        return Vec::new();
    }

    tracing::info!("persist: loaded {} monitored apps", config.monitored.len());
    config.monitored
}

/// Serialize and atomically replace the on-disk config with the given apps.
pub fn save_apps(apps: &[AppEntry]) {
    let payload = OnDiskConfig {
        version: CURRENT_VERSION,
        monitored: apps.to_vec(),
    };
    let text = match serde_json::to_string_pretty(&payload) {
        Ok(t) => t,
        Err(err) => {
            tracing::error!("persist: failed to serialize config: {}", err);
            return;
        }
    };

    let tmp_path = format!("{}.tmp", CONFIG_PATH);
    if let Err(err) = fs::write(&tmp_path, &text) {
        tracing::error!("persist: failed to write {}: {}", tmp_path, err);
        return;
    }
    if let Err(err) = fs::rename(&tmp_path, CONFIG_PATH) {
        // Some WASI filesystems may not support atomic rename; fall back to
        // a direct overwrite so we don't lose the update entirely.
        tracing::warn!(
            "persist: rename failed ({}), falling back to direct write",
            err
        );
        if let Err(err) = fs::write(CONFIG_PATH, &text) {
            tracing::error!("persist: direct write also failed: {}", err);
            return;
        }
    }
    tracing::debug!("persist: wrote {} ({} bytes)", CONFIG_PATH, text.len());
}
