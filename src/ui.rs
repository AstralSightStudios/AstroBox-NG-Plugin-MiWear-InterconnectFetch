use serde::Deserialize;

use crate::astrobox::psys_host::ui_v3 as ui;
use crate::exports::astrobox::psys_plugin::event_v3 as event;
use crate::interconnect;
use crate::state::{self, AppEntry, InstalledApp};

const EVENT_ADD_PKG_INPUT: &str = "input:add-pkg";
const EVENT_ADD_PKG_SUBMIT: &str = "action:add-pkg.submit";
const EVENT_REFRESH_DEVICES: &str = "action:devices.refresh";
const EVENT_CLEAR_NOTICE: &str = "action:notice.clear";
const PKG_TOGGLE_PREFIX: &str = "toggle:pkg:";
const PKG_REMOVE_PREFIX: &str = "action:pkg.remove:";
const PKG_REREGISTER_PREFIX: &str = "action:pkg.reregister:";
const PKG_PICK_PREFIX: &str = "action:pkg.pick:";

// Dark palette. We deliberately avoid card backgrounds / borders so the UI
// inherits the host's surface and feels native instead of "boxed".
const COLOR_TEXT_PRIMARY: &str = "#f4f4f5";
const COLOR_TEXT_SECONDARY: &str = "#a1a1aa";
const COLOR_TEXT_MUTED: &str = "#71717a";
const COLOR_TEXT_ACCENT: &str = "#60a5fa";
const COLOR_TEXT_DANGER: &str = "#f87171";
const COLOR_TEXT_SUCCESS: &str = "#4ade80";
const COLOR_DIVIDER: &str = "#27272a";
const COLOR_BTN_PRIMARY_BG: &str = "#2563eb";
const COLOR_BTN_GHOST_BG: &str = "#27272a";
const COLOR_BTN_DANGER_BG: &str = "#3f1d1d";

#[derive(Default, Deserialize)]
struct UiPayload {
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    checked: Option<bool>,
}

pub fn render_main_ui(element_id: &str) {
    state::set_root(element_id);
    rerender();
}

pub fn ui_event_processor(_evtype: event::Event, event_id: &str, payload_raw: &str) {
    let payload: UiPayload = serde_json::from_str(payload_raw).unwrap_or_default();

    if let Some(pkg) = event_id.strip_prefix(PKG_PICK_PREFIX) {
        state::ensure_app(pkg);
        let count = interconnect::register_for_all_devices(pkg);
        state::set_notice(format!(
            "已开始监听 {}（在 {} 台设备上注册接收器）",
            pkg, count
        ));
        rerender();
        return;
    }

    if let Some(pkg) = event_id.strip_prefix(PKG_TOGGLE_PREFIX) {
        let enabled = payload.checked.unwrap_or(true);
        state::set_enabled(pkg, enabled);
        state::set_notice(format!(
            "已{}快应用 {} 的联网能力",
            if enabled { "启用" } else { "禁用" },
            pkg
        ));
        rerender();
        return;
    }

    if let Some(pkg) = event_id.strip_prefix(PKG_REMOVE_PREFIX) {
        state::remove_app(pkg);
        state::set_notice(format!("已从列表移除 {}", pkg));
        rerender();
        return;
    }

    if let Some(pkg) = event_id.strip_prefix(PKG_REREGISTER_PREFIX) {
        let count = interconnect::register_for_all_devices(pkg);
        state::set_notice(format!("已为 {} 在 {} 台设备上重新注册接收器", pkg, count));
        rerender();
        return;
    }

    match event_id {
        EVENT_ADD_PKG_INPUT => {
            state::set_pending_add(payload.value.unwrap_or_default());
        }
        EVENT_ADD_PKG_SUBMIT => {
            let pkg = state::take_pending_add().trim().to_string();
            if pkg.is_empty() {
                state::set_notice("请输入要监听的快应用包名".to_string());
            } else {
                state::ensure_app(&pkg);
                let count = interconnect::register_for_all_devices(&pkg);
                state::set_notice(format!("已新增监听 {}，注册了 {} 台设备", pkg, count));
            }
            rerender();
        }
        EVENT_REFRESH_DEVICES => {
            // Manual refresh bypasses the throttle so the user always sees
            // the freshest snapshot they explicitly asked for.
            interconnect::refresh_connected_devices();
            interconnect::refresh_installed_apps();
            let pkgs = state::pkg_names();
            for pkg in &pkgs {
                interconnect::register_for_all_devices(pkg);
            }
            let device_count = state::connected_devices().len();
            state::set_notice(format!(
                "已刷新：连接 {} 台设备 · 共 {} 个第三方应用 · 重注册 {} 个已监听包",
                device_count,
                state::installed_apps().len(),
                pkgs.len()
            ));
            // Skip the throttled auto-refresh in the same tick.
            render_without_auto_refresh();
        }
        EVENT_CLEAR_NOTICE => {
            state::clear_notice();
            rerender();
        }
        _ => {}
    }
}

pub fn rerender() {
    // Every UI render also triggers a (throttled) device + installed-apps
    // refresh and re-registers all monitored packages on every device, so the
    // user always sees an up-to-date snapshot without manually pressing a
    // refresh button.
    interconnect::auto_refresh();
    render_without_auto_refresh();
}

pub fn render_without_auto_refresh() {
    let Some(root) = state::root() else {
        return;
    };
    ui::render(&root, build_root());
}

fn build_root() -> ui::Element {
    let apps = state::snapshot_apps();
    let devices = state::connected_devices();
    let installed = state::installed_apps();
    let pending_input = state_pending_add();
    let notice = state_notice();

    let mut root = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .width_full()
        .padding(28)
        .gap(24);

    root = root.child(header_section(devices.len(), apps.len(), installed.len()));

    if let Some(text) = notice {
        root = root.child(notice_line(&text));
    }

    root = root.child(divider());
    root = root.child(device_section(&devices));
    root = root.child(divider());
    root = root.child(installed_apps_section(&installed));
    root = root.child(divider());
    root = root.child(add_pkg_section(&pending_input));
    root = root.child(divider());
    root = root.child(apps_section(&apps));

    root
}

fn divider() -> ui::Element {
    ui::Element::new(ui::ElementType::Div, None)
        .width_full()
        .height(1)
        .bg(COLOR_DIVIDER)
}

fn header_section(device_count: usize, monitored: usize, installed: usize) -> ui::Element {
    let title = ui::Element::new(ui::ElementType::P, Some("米环互联 Fetch"))
        .size(28)
        .text_color(COLOR_TEXT_PRIMARY);

    let subtitle = ui::Element::new(
        ui::ElementType::P,
        Some("为手表端快应用提供 HTTP 联网能力，按包名授权访问。打开页面即自动刷新设备并重新注册接收器。"),
    )
    .size(14)
    .text_color(COLOR_TEXT_SECONDARY);

    let stats_row = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Row)
        .gap(16)
        .margin_top(4)
        .child(stat_pill(&format!("已连接设备 {}", device_count)))
        .child(stat_pill(&format!("设备应用 {}", installed)))
        .child(stat_pill(&format!("已监听 {}", monitored)));

    ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .gap(6)
        .child(title)
        .child(subtitle)
        .child(stats_row)
}

fn stat_pill(text: &str) -> ui::Element {
    ui::Element::new(ui::ElementType::P, Some(text))
        .size(12)
        .text_color(COLOR_TEXT_MUTED)
}

fn notice_line(text: &str) -> ui::Element {
    let label = ui::Element::new(ui::ElementType::P, Some(text))
        .size(13)
        .text_color(COLOR_TEXT_ACCENT)
        .flex_grow(1.0);

    let close_btn = text_button("收起", COLOR_TEXT_MUTED).on(ui::Event::Click, EVENT_CLEAR_NOTICE);

    ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Row)
        .align_center()
        .gap(12)
        .child(label)
        .child(close_btn)
}

fn section_title(text: &str) -> ui::Element {
    ui::Element::new(ui::ElementType::P, Some(text))
        .size(18)
        .text_color(COLOR_TEXT_PRIMARY)
}

fn section_hint(text: &str) -> ui::Element {
    ui::Element::new(ui::ElementType::P, Some(text))
        .size(12)
        .text_color(COLOR_TEXT_MUTED)
}

fn device_section(devices: &[(String, String)]) -> ui::Element {
    let mut col = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .gap(10)
        .child(section_title("已连接的设备"));

    if devices.is_empty() {
        col = col.child(section_hint(
            "当前没有连接的设备。请先在 AstroBox 中连接小米手环 / 手表，再点击下方按钮刷新。",
        ));
    } else {
        for (addr, name) in devices {
            col = col.child(device_row(name, addr));
        }
    }

    col = col.child(
        ui::Element::new(ui::ElementType::Div, None)
            .margin_top(6)
            .child(primary_button("刷新设备并重新注册").on(ui::Event::Click, EVENT_REFRESH_DEVICES)),
    );

    col
}

fn device_row(name: &str, addr: &str) -> ui::Element {
    let name_el = ui::Element::new(
        ui::ElementType::P,
        Some(if name.is_empty() { "未命名设备" } else { name }),
    )
    .size(14)
    .text_color(COLOR_TEXT_PRIMARY);

    let addr_el = ui::Element::new(ui::ElementType::P, Some(addr))
        .size(12)
        .text_color(COLOR_TEXT_MUTED);

    ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .gap(2)
        .child(name_el)
        .child(addr_el)
}

fn installed_apps_section(apps: &[InstalledApp]) -> ui::Element {
    let mut col = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .gap(10)
        .child(section_title("设备上的快应用"))
        .child(section_hint(
            "下面是当前已连接设备上所有第三方快应用。点击「监听」即开始为它转发联网请求。",
        ));

    if apps.is_empty() {
        col = col.child(section_hint("尚未读到设备应用列表，请确认设备已连接。"));
        return col;
    }

    for (i, app) in apps.iter().enumerate() {
        if i > 0 {
            col = col.child(thin_divider());
        }
        col = col.child(installed_app_row(app));
    }

    col
}

fn installed_app_row(app: &InstalledApp) -> ui::Element {
    let monitored = state::is_monitored(&app.package_name);

    let name_el = ui::Element::new(
        ui::ElementType::P,
        Some(if app.app_name.is_empty() {
            app.package_name.as_str()
        } else {
            app.app_name.as_str()
        }),
    )
    .size(14)
    .text_color(COLOR_TEXT_PRIMARY);

    let meta = format!(
        "{}  ·  v{}  ·  {}",
        app.package_name,
        app.version_code,
        if app.device_name.is_empty() {
            app.addr.as_str()
        } else {
            app.device_name.as_str()
        }
    );
    let meta_el = ui::Element::new(ui::ElementType::P, Some(&meta))
        .size(11)
        .text_color(COLOR_TEXT_MUTED);

    let info_col = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .gap(2)
        .child(name_el)
        .child(meta_el)
        .flex_grow(1.0);

    let action = if monitored {
        ui::Element::new(ui::ElementType::P, Some("已在监听"))
            .size(12)
            .text_color(COLOR_TEXT_SUCCESS)
    } else {
        primary_button("监听").on(
            ui::Event::Click,
            &format!("{}{}", PKG_PICK_PREFIX, app.package_name),
        )
    };

    ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Row)
        .align_center()
        .gap(16)
        .padding_top(6)
        .padding_bottom(6)
        .child(info_col)
        .child(action)
}

fn add_pkg_section(pending: &str) -> ui::Element {
    let input = ui::Element::new(ui::ElementType::Input, None)
        .prop("placeholder", "手动输入包名，例如 com.your.app")
        .prop("value", pending)
        .flex_grow(1.0)
        .padding(10)
        .text_color(COLOR_TEXT_PRIMARY)
        .on(ui::Event::Input, EVENT_ADD_PKG_INPUT);

    let submit = primary_button("添加").on(ui::Event::Click, EVENT_ADD_PKG_SUBMIT);

    let row = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Row)
        .gap(10)
        .align_center()
        .child(input)
        .child(submit);

    ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .gap(10)
        .child(section_title("手动添加包名"))
        .child(section_hint(
            "若设备应用列表里没有目标快应用，可在此手动填写包名后添加。",
        ))
        .child(row)
}

fn apps_section(apps: &[AppEntry]) -> ui::Element {
    let mut col = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .gap(10)
        .child(section_title("当前监听的快应用"))
        .child(section_hint(
            "下列快应用已被授权通过本插件联网。关闭开关即立即禁用，点击「移除」则停止监听。",
        ));

    if apps.is_empty() {
        col = col.child(section_hint(
            "尚未添加监听项。从上方列表中点「监听」，或手动填入包名后添加。",
        ));
        return col;
    }

    for (i, entry) in apps.iter().enumerate() {
        if i > 0 {
            col = col.child(thin_divider());
        }
        col = col.child(app_row(entry));
    }

    col
}

fn thin_divider() -> ui::Element {
    ui::Element::new(ui::ElementType::Div, None)
        .width_full()
        .height(1)
        .bg(COLOR_DIVIDER)
        .opacity(0.6)
}

fn app_row(entry: &AppEntry) -> ui::Element {
    let pkg_label = ui::Element::new(ui::ElementType::P, Some(&entry.pkg_name))
        .size(15)
        .text_color(COLOR_TEXT_PRIMARY);

    let status_color = if entry.enabled {
        COLOR_TEXT_SUCCESS
    } else {
        COLOR_TEXT_DANGER
    };
    let status_label = ui::Element::new(
        ui::ElementType::P,
        Some(if entry.enabled { "已允许联网" } else { "已禁用" }),
    )
    .size(12)
    .text_color(status_color);

    let stats = format!(
        "请求 {} · 成功 {} · 失败 {}",
        entry.request_count, entry.success_count, entry.error_count
    );
    let stats_el = ui::Element::new(ui::ElementType::P, Some(&stats))
        .size(12)
        .text_color(COLOR_TEXT_SECONDARY);

    let last_status = entry
        .last_status
        .clone()
        .unwrap_or_else(|| "尚未发起请求".to_string());
    let last_status_el = ui::Element::new(
        ui::ElementType::P,
        Some(&format!("最近状态: {}", last_status)),
    )
    .size(11)
    .text_color(COLOR_TEXT_MUTED);

    let last_url = entry
        .last_url
        .clone()
        .unwrap_or_else(|| "—".to_string());
    let last_url_el = ui::Element::new(
        ui::ElementType::P,
        Some(&format!("最近 URL: {}", last_url)),
    )
    .size(11)
    .text_color(COLOR_TEXT_MUTED);

    let last_seen_el = ui::Element::new(
        ui::ElementType::P,
        Some(&format!(
            "最近活动: {}  ·  设备: {}",
            format_time(entry.last_seen_unix_ms),
            entry.last_addr.clone().unwrap_or_else(|| "—".to_string())
        )),
    )
    .size(11)
    .text_color(COLOR_TEXT_MUTED);

    let info_col = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .gap(4)
        .child(
            ui::Element::new(ui::ElementType::Div, None)
                .flex()
                .flex_direction(ui::FlexDirection::Row)
                .align_center()
                .gap(10)
                .child(pkg_label)
                .child(status_label),
        )
        .child(stats_el)
        .child(last_status_el)
        .child(last_url_el)
        .child(last_seen_el)
        .flex_grow(1.0);

    let switch = ui::Element::new(ui::ElementType::Switch, None)
        .prop("checked", if entry.enabled { "true" } else { "false" })
        .on(
            ui::Event::Change,
            &format!("{}{}", PKG_TOGGLE_PREFIX, entry.pkg_name),
        );

    let reregister_btn = ghost_button("重新注册").on(
        ui::Event::Click,
        &format!("{}{}", PKG_REREGISTER_PREFIX, entry.pkg_name),
    );

    let remove_btn = danger_button("移除").on(
        ui::Event::Click,
        &format!("{}{}", PKG_REMOVE_PREFIX, entry.pkg_name),
    );

    let actions = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .gap(8)
        .align_end()
        .child(switch)
        .child(reregister_btn)
        .child(remove_btn);

    ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Row)
        .align_start()
        .gap(16)
        .padding_top(8)
        .padding_bottom(8)
        .child(info_col)
        .child(actions)
}

fn primary_button(text: &str) -> ui::Element {
    ui::Element::new(ui::ElementType::Button, Some(text))
        .padding_top(8)
        .padding_bottom(8)
        .padding_left(16)
        .padding_right(16)
        .radius(6)
        .bg(COLOR_BTN_PRIMARY_BG)
        .text_color(COLOR_TEXT_PRIMARY)
        .size(13)
}

fn ghost_button(text: &str) -> ui::Element {
    ui::Element::new(ui::ElementType::Button, Some(text))
        .padding_top(6)
        .padding_bottom(6)
        .padding_left(12)
        .padding_right(12)
        .radius(6)
        .bg(COLOR_BTN_GHOST_BG)
        .text_color(COLOR_TEXT_SECONDARY)
        .size(12)
}

fn danger_button(text: &str) -> ui::Element {
    ui::Element::new(ui::ElementType::Button, Some(text))
        .padding_top(6)
        .padding_bottom(6)
        .padding_left(12)
        .padding_right(12)
        .radius(6)
        .bg(COLOR_BTN_DANGER_BG)
        .text_color(COLOR_TEXT_DANGER)
        .size(12)
}

fn text_button(text: &str, color: &str) -> ui::Element {
    ui::Element::new(ui::ElementType::Button, Some(text))
        .without_default_styles()
        .text_color(color)
        .size(12)
}

fn format_time(unix_ms: Option<u128>) -> String {
    match unix_ms {
        None => "—".to_string(),
        Some(ms) => {
            let secs = ms / 1000;
            let h = (secs / 3600) % 24;
            let m = (secs / 60) % 60;
            let s = secs % 60;
            format!("{:02}:{:02}:{:02} UTC", h, m, s)
        }
    }
}

fn state_pending_add() -> String {
    state::with_state(|s| s.pending_add_pkg.clone())
}

fn state_notice() -> Option<String> {
    state::with_state(|s| s.last_notice.clone())
}
