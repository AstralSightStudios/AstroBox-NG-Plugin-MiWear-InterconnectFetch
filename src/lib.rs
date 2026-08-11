use wit_bindgen::FutureReader;

use crate::exports::astrobox::psys_plugin::{event_v3 as event, event_v3::EventType, lifecycle};

pub mod codec;
pub mod fetch;
pub mod handshake;
pub mod interconnect;
pub mod logger;
pub mod persist;
pub mod state;
pub mod stream;
pub mod transfer;
pub mod ui;

wit_bindgen::generate!({
    path: "wit",
    world: "psys-world-v3",
    generate_all,
});

struct MyPlugin;

impl event::Guest for MyPlugin {
    fn on_event(event_type: EventType, event_payload: _rt::String) -> FutureReader<String> {
        tracing::info!(
            "on_event: type={:?} payload_len={}",
            event_type,
            event_payload.len()
        );

        match event_type {
            EventType::InterconnectMessage => {
                let parsed = interconnect::parse_message(&event_payload);
                // Preserve the host's established v3 event lifecycle: finish all
                // handling before resolving this exported event. The protocol is
                // still ACK/event-driven between frames; `block_on` here only
                // drives this one bounded event transaction and does not create a
                // detached worker or a receive loop.
                wit_bindgen::block_on(dispatch_interconnect(
                    &parsed.addr,
                    &parsed.pkg_name,
                    &parsed.data,
                ));
            }
            EventType::PluginMessage => {
                tracing::info!("plugin-message: {}", event_payload);
            }
            EventType::DeviceAction => {
                interconnect::refresh_connected_devices();
                interconnect::refresh_installed_apps();
                for pkg in state::pkg_names() {
                    interconnect::register_for_all_devices(&pkg);
                }
                ui::rerender();
            }
            EventType::Timer => {
                stream::prune_idle();
                transfer::prune_idle();
            }
            EventType::ProviderAction | EventType::DeeplinkAction | EventType::TransportPacket => {}
        }

        immediate_string(String::new())
    }

    fn on_ui_event_v3(
        event_id: _rt::String,
        event: event::Event,
        event_payload: _rt::String,
    ) -> FutureReader<_rt::String> {
        ui::ui_event_processor(event, &event_id, &event_payload);
        immediate_string(String::new())
    }

    fn on_ui_render(element_id: _rt::String) -> FutureReader<()> {
        // Host rendering is synchronous in the current v3 UI ABI. Complete the
        // render first, then return an already-resolving FutureReader; scheduling
        // render itself behind spawn can leave Host and guest waiting on each other.
        ui::render_main_ui(&element_id);
        immediate_unit()
    }

    fn on_card_render(_card_id: _rt::String) -> FutureReader<()> {
        immediate_unit()
    }
}

async fn dispatch_interconnect(addr: &str, pkg: &str, data: &str) {
    state::ensure_app(pkg);

    if !state::is_enabled(pkg) {
        tracing::warn!(
            "interconnect message dropped (disabled): pkg={} addr={} len={}",
            pkg,
            addr,
            data.len()
        );
        return;
    }

    let value: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("failed to parse interconnect data as JSON: {err}");
            return;
        }
    };

    let obj = match value {
        serde_json::Value::Object(map) => map,
        other => {
            tracing::error!("interconnect data is not a JSON object: {}", other);
            return;
        }
    };

    let tag = obj
        .get("tag")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut body = obj.clone();
    body.remove("tag");
    let body_value = serde_json::Value::Object(body);

    match tag.as_str() {
        handshake::HS_TAG => {
            handshake::handle_packet(addr, pkg, &body_value).await;
            state::record_request(pkg, addr, None);
            ui::rerender();
        }
        fetch::FETCH_TAG => {
            fetch::handle_request(addr, pkg, body_value).await;
            ui::rerender();
        }
        fetch::FETCH_ACK_TAG => {
            // Pure flow-control frame: advance the v3 finite-chunk window.
            fetch::handle_ack(addr, pkg, body_value).await;
        }
        stream::FETCH_STREAM_ACK_TAG => {
            fetch::handle_stream_ack(addr, pkg, body_value).await;
        }
        stream::FETCH_STREAM_CANCEL_TAG => {
            fetch::handle_stream_cancel(addr, pkg, body_value).await;
        }
        other => {
            tracing::warn!(
                "unknown interconnect tag: {} (pkg={} addr={})",
                other,
                pkg,
                addr
            );
        }
    }
}

fn immediate_string(value: String) -> FutureReader<String> {
    let (writer, reader) = wit_future::new(String::new);
    wit_bindgen::spawn(async move {
        let _ = writer.write(value).await;
    });
    reader
}

fn immediate_unit() -> FutureReader<()> {
    let (writer, reader) = wit_future::new::<()>(|| ());
    wit_bindgen::spawn(async move {
        let _ = writer.write(()).await;
    });
    reader
}

impl lifecycle::Guest for MyPlugin {
    fn on_load() {
        logger::init();
        tracing::info!("AstroBox MiWear Interconnect Fetch plugin loaded");

        // Restore previously authorized monitored apps from disk first, so
        // the rest of startup (device refresh + re-register) operates on the
        // restored set without flashing an empty UI.
        let restored = persist::load_apps();
        let restored_count = restored.len();
        state::install_loaded_apps(restored);
        tracing::info!("startup: restored {} monitored apps", restored_count);

        // Pre-populate the device cache + installed-app list so the first UI
        // render shows something, then register every restored package on
        // every currently connected device.
        interconnect::refresh_connected_devices();
        interconnect::refresh_installed_apps();
        for pkg in state::pkg_names() {
            let count = interconnect::register_for_all_devices(&pkg);
            tracing::info!("startup register: pkg={} devices={}", pkg, count);
        }
    }
}

export!(MyPlugin);
