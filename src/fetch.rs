use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Value, json};
use waki::{Client, Method};

use crate::handshake;
use crate::interconnect;
use crate::state;

/// Tag used for the fetch request/response exchange (matches the JS client).
pub const FETCH_TAG: &str = "fetch";

#[derive(Debug, Deserialize)]
pub struct FetchRequest {
    pub id: Option<String>,
    pub url: String,
    #[serde(default)]
    pub options: Option<FetchOptions>,
}

#[derive(Debug, Default, Deserialize)]
pub struct FetchOptions {
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub headers: Option<HashMap<String, Value>>,
    #[serde(default)]
    pub body: Option<String>,
    /// Mirrors the JS `raw` option. When true we hand back the raw bytes as
    /// base64 instead of trying to decode them as UTF-8 text.
    #[serde(default)]
    pub raw: Option<bool>,
}

pub fn handle_request(addr: &str, pkg: &str, body: Value) {
    let req: FetchRequest = match serde_json::from_value::<FetchRequest>(body) {
        Ok(r) => r,
        Err(err) => {
            tracing::error!("invalid fetch payload: {err}");
            return;
        }
    };

    let id = req.id.clone();
    let url = req.url.clone();
    state::record_request(pkg, addr, Some(&url));

    let options = req.options.unwrap_or_default();
    let method = options
        .method
        .as_deref()
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let raw_mode = options.raw.unwrap_or(false);

    tracing::info!(
        "fetch begin: pkg={} addr={} id={} method={} url={}",
        pkg,
        addr,
        id.as_deref().unwrap_or(""),
        method,
        url
    );

    let resp_value = match perform_request(&method, &url, options.headers, options.body, raw_mode) {
        Ok(v) => {
            state::record_result(pkg, true, Some(format!("HTTP {}", status_from(&v))));
            v
        }
        Err(err) => {
            let message = err.to_string();
            tracing::error!("fetch error: {message}");
            state::record_result(pkg, false, Some(message.clone()));
            json!({
                "ok": false,
                "status": 0,
                "statusText": message,
                "headers": {},
                "body": "",
                "raw": false,
            })
        }
    };

    let mut payload = serde_json::Map::new();
    payload.insert("resp".to_string(), resp_value);
    if let Some(id) = id {
        payload.insert("id".to_string(), Value::String(id));
    }
    interconnect::send_json(addr, pkg, FETCH_TAG, Value::Object(payload));
    handshake::ensure_open(addr, pkg);
}

fn perform_request(
    method: &str,
    url: &str,
    headers: Option<HashMap<String, Value>>,
    body: Option<String>,
    raw: bool,
) -> Result<Value, String> {
    let client = Client::new();
    let parsed_method = parse_method(method);
    let mut req = client.request(parsed_method, url);

    if let Some(headers) = headers {
        for (k, v) in headers {
            let value = match v {
                Value::String(s) => s,
                other => other.to_string(),
            };
            let name = match waki::header::HeaderName::try_from(k.as_str()) {
                Ok(name) => name,
                Err(err) => {
                    tracing::warn!("skip header {}: {}", k, err);
                    continue;
                }
            };
            req = req.header(name, value);
        }
    }

    if let Some(body) = body {
        req = req.body(body.into_bytes());
    }

    let response = req.send().map_err(|e| format!("send failed: {e}"))?;

    let status = response.status_code();
    let resp_headers_raw = response.headers().clone();
    let body_bytes = response
        .body()
        .map_err(|e| format!("read body failed: {e}"))?;

    let mut header_map = serde_json::Map::new();
    for (name, value) in resp_headers_raw.iter() {
        let key = name.as_str().to_string();
        let val = value.to_str().unwrap_or("").to_string();
        header_map.insert(key, Value::String(val));
    }

    let (body_value, raw_flag) = if raw {
        // Emit raw bytes as base64 so binary content survives the JSON hop.
        (Value::String(base64_encode(&body_bytes)), true)
    } else {
        match String::from_utf8(body_bytes.clone()) {
            Ok(text) => (Value::String(text), false),
            Err(_) => (Value::String(base64_encode(&body_bytes)), true),
        }
    };

    let ok = (200..300).contains(&status);

    Ok(json!({
        "ok": ok,
        "status": status,
        "statusText": status_text(status),
        "headers": Value::Object(header_map),
        "body": body_value,
        "raw": raw_flag,
    }))
}

fn parse_method(method: &str) -> Method {
    match method.to_ascii_uppercase().as_str() {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "DELETE" => Method::Delete,
        "HEAD" => Method::Head,
        "PATCH" => Method::Patch,
        "OPTIONS" => Method::Options,
        "CONNECT" => Method::Connect,
        "TRACE" => Method::Trace,
        other => Method::Other(other.to_string()),
    }
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

fn status_from(v: &Value) -> u16 {
    v.get("status").and_then(|s| s.as_u64()).unwrap_or(0) as u16
}

/// Minimal base64 encoder so we don't pull in the `base64` crate just for this.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(((data.len() + 2) / 3) * 4);
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | (data[i + 2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        out.push(ALPHABET[(n & 0x3F) as usize] as char);
        i += 3;
    }
    let rem = data.len() - i;
    if rem == 1 {
        let n = (data[i] as u32) << 16;
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        out.push('=');
    }
    out
}
