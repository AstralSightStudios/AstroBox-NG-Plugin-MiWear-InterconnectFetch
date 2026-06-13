use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Map, Value};
use waki::{Client, Method};

use crate::codec::{self, BodyEncoding, Compression, COMPRESS_MIN_SIZE};
use crate::handshake::{self, NegotiatedCaps};
use crate::interconnect;
use crate::state;
use crate::transfer;

/// Tag used for the fetch request/response exchange (matches the JS client).
pub const FETCH_TAG: &str = "fetch";
/// Tag used to carry chunked response data. Only emitted when the peer
/// negotiated chunking via the v2+ handshake `caps`. Legacy peers never see
/// this tag and continue receiving single-message responses.
pub const FETCH_CHUNK_TAG: &str = "fetch-chunk";
/// Tag the peer uses to acknowledge received chunks (v3 ACK flow control).
/// Carries `{ id, ack }` where `ack` is the next contiguous chunk index the
/// peer still needs. Drives the sliding window in `transfer.rs`. Only seen when
/// both sides negotiated `caps.ack`.
pub const FETCH_ACK_TAG: &str = "fetch-ack";
/// Last-resort guard for legacy single-message responses. If negotiated caps
/// are missing, large binary responses would otherwise become one huge JSON
/// `fetch` frame and can wedge the host UI / QAIC transport. Normal large
/// responses must use negotiated chunking instead.
const MAX_UNCHUNKED_WIRE_LEN: usize = 16 * 1024;

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
    /// Mirrors the JS `raw` option. When true we hand back the raw bytes
    /// instead of trying to decode them as UTF-8 text.
    #[serde(default)]
    pub raw: Option<bool>,
}

struct FetchResponse {
    ok: bool,
    status: u16,
    status_text: &'static str,
    headers: Map<String, Value>,
    body_bytes: Vec<u8>,
    /// True when the assembled bytes should be treated as binary by the peer;
    /// false means "decode as UTF-8". Independent of any wire encoding.
    raw: bool,
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
    handshake::ensure_open(addr, pkg);

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

    match perform_request(&method, &url, options.headers, options.body, raw_mode) {
        Ok(resp) => {
            let status = resp.status;
            match send_response(addr, pkg, id.as_deref(), resp) {
                Ok(()) => state::record_result(pkg, true, Some(format!("HTTP {}", status))),
                Err(err) => state::record_result(pkg, false, Some(err)),
            }
        }
        Err(err) => {
            tracing::error!("fetch error: {err}");
            state::record_result(pkg, false, Some(err.clone()));
            send_error(addr, pkg, id.as_deref(), &err);
        }
    }
}

fn perform_request(
    method: &str,
    url: &str,
    headers: Option<HashMap<String, Value>>,
    body: Option<String>,
    raw: bool,
) -> Result<FetchResponse, String> {
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

    let mut headers = Map::new();
    for (name, value) in resp_headers_raw.iter() {
        let key = name.as_str().to_string();
        let val = value.to_str().unwrap_or("").to_string();
        headers.insert(key, Value::String(val));
    }

    // Auto-promote to raw when the bytes aren't valid UTF-8, so binary content
    // still survives the JSON hop even if the caller forgot to set raw=true.
    let raw_flag = raw || std::str::from_utf8(&body_bytes).is_err();
    let ok = (200..300).contains(&status);

    Ok(FetchResponse {
        ok,
        status,
        status_text: status_text(status),
        headers,
        body_bytes,
        raw: raw_flag,
    })
}

/// Composed transfer plan for one response. Built from negotiated caps plus
/// per-body heuristics, then handed to the encoder + sender.
struct TransferPlan {
    compression: Compression,
    /// Bytes after compression (or original if `compression == None`). These
    /// are what gets chunked and wire-encoded.
    payload: Vec<u8>,
    /// Original body length in bytes, *before* compression. Reported to the
    /// peer so it can size its receive buffer.
    original_bytes: usize,
    /// Wire encoding to use for `payload`.
    encoding: BodyEncoding,
    /// `Some(chunk_size)` when the response will be split across multiple
    /// frames; chunk_size is in bytes of `payload` per chunk.
    chunk_size: Option<usize>,
    /// In-flight window for ACK-paced chunk delivery. `0` ⇒ the peer can't ACK,
    /// so we fall back to the legacy un-paced blast. Only meaningful when
    /// `chunk_size` is `Some`.
    ack_window: usize,
}

fn build_plan(resp: &FetchResponse, caps: Option<&NegotiatedCaps>) -> TransferPlan {
    let original_bytes = resp.body_bytes.len();
    let compression = pick_compression(caps, original_bytes);
    let payload = codec::compress(&resp.body_bytes, compression);

    // Decide chunking from the *post-compression* size — that's what actually
    // moves over the wire.
    let chunk_size = caps
        .and_then(|c| if c.chunked { Some(c.chunk_size) } else { None })
        .filter(|cs| payload.len() > *cs);

    let encoding = pick_encoding(caps, &payload, resp.raw, chunk_size.is_some(), compression);

    // Pace chunk delivery with ACKs whenever we're actually chunking and the
    // peer negotiated a window. Otherwise stay on the legacy blast path.
    let ack_window = if chunk_size.is_some() {
        caps.map(|c| c.ack_window).unwrap_or(0)
    } else {
        0
    };

    TransferPlan {
        compression,
        payload,
        original_bytes,
        encoding,
        chunk_size,
        ack_window,
    }
}

/// Choose the compressor to apply. Defaults to `None` whenever the peer
/// didn't advertise a list (v1/v2 baseline) or the body is too small to be
/// worth compressing.
fn pick_compression(caps: Option<&NegotiatedCaps>, body_len: usize) -> Compression {
    let Some(caps) = caps else {
        return Compression::None;
    };
    if caps.compressions.is_empty() || body_len < COMPRESS_MIN_SIZE {
        return Compression::None;
    }
    // Peer's first preference that we also implement wins. Falls back to
    // `None`, which is also always implicitly supported.
    caps.compressions
        .iter()
        .copied()
        .next()
        .unwrap_or(Compression::None)
}

/// Choose the wire encoding. Rules:
///   - `text` is only viable when the payload is valid UTF-8 AND we're not
///     chunking (we don't split across UTF-8 code points). Compressed payloads
///     are binary, so they never qualify.
///   - Otherwise honour the peer's preference order over {base64, hex}.
///   - If the peer never advertised an `encodings` list, fall back to the v1
///     baseline: `text` for plain UTF-8 text bodies, `base64` for everything
///     else.
fn pick_encoding(
    caps: Option<&NegotiatedCaps>,
    payload: &[u8],
    raw: bool,
    will_chunk: bool,
    compression: Compression,
) -> BodyEncoding {
    let text_viable = !will_chunk
        && !raw
        && compression == Compression::None
        && std::str::from_utf8(payload).is_ok();

    let peer_encs = caps.map(|c| c.encodings.as_slice()).unwrap_or(&[]);

    if peer_encs.is_empty() {
        // v1 / v2 peers: text or base64, exactly like before.
        return if text_viable {
            BodyEncoding::Text
        } else {
            BodyEncoding::Base64
        };
    }

    for &enc in peer_encs {
        match enc {
            BodyEncoding::Text if text_viable => return BodyEncoding::Text,
            BodyEncoding::Base64 | BodyEncoding::Hex => return enc,
            _ => continue,
        }
    }

    // Peer's list didn't contain anything we can satisfy under current
    // constraints (e.g. only `text` but body is binary or chunked). Base64 is
    // the universal fallback every peer is required to handle.
    BodyEncoding::Base64
}

fn send_response(
    addr: &str,
    pkg: &str,
    id: Option<&str>,
    resp: FetchResponse,
) -> Result<(), String> {
    let caps = handshake::negotiated_caps(addr, pkg);
    let plan = build_plan(&resp, caps.as_ref());

    // `chunk_size` is `Copy`, so matching on it doesn't borrow `plan` — the
    // chunked arm can take ownership and hand the payload off to `transfer`.
    match plan.chunk_size {
        Some(cs) => {
            send_chunked(addr, pkg, id, &resp, plan, cs);
            Ok(())
        }
        None => send_unchunked(addr, pkg, id, &resp, &plan),
    }
}

fn send_unchunked(
    addr: &str,
    pkg: &str,
    id: Option<&str>,
    resp: &FetchResponse,
    plan: &TransferPlan,
) -> Result<(), String> {
    // Encoding `Text` can't fail at this point: pick_encoding only returns it
    // when the payload was checked to be valid UTF-8 and not compressed.
    let encoded = codec::encode(&plan.payload, plan.encoding)
        .unwrap_or_else(|_| codec::encode(&plan.payload, BodyEncoding::Base64).unwrap());

    if encoded.len() > MAX_UNCHUNKED_WIRE_LEN {
        tracing::error!(
            "refusing oversized unchunked fetch response: pkg={} addr={} id={} encoded={} original={} enc={} comp={}",
            pkg,
            addr,
            id.unwrap_or(""),
            encoded.len(),
            plan.original_bytes,
            plan.encoding.as_str(),
            plan.compression.as_str(),
        );
        let message =
            "response too large for unchunked interconnect frame; complete FetchBridge handshake with chunk=true";
        send_error(addr, pkg, id, message);
        return Err(message.to_string());
    }

    let mut resp_obj = Map::new();
    resp_obj.insert("ok".into(), Value::Bool(resp.ok));
    resp_obj.insert("status".into(), Value::from(resp.status));
    resp_obj.insert("statusText".into(), Value::String(resp.status_text.into()));
    resp_obj.insert("headers".into(), Value::Object(resp.headers.clone()));
    resp_obj.insert("body".into(), Value::String(encoded));
    resp_obj.insert("raw".into(), Value::Bool(resp.raw));

    // Only annotate non-default codec choices so v1/v2 peers continue to see
    // exactly the wire shape they used to. This keeps the doc-stated promise
    // that omitting `caps` keeps you on the legacy path.
    if plan.encoding != legacy_encoding_for(resp.raw) {
        resp_obj.insert(
            "bodyEncoding".into(),
            Value::String(plan.encoding.as_str().into()),
        );
    }
    if plan.compression != Compression::None {
        resp_obj.insert(
            "compression".into(),
            Value::String(plan.compression.as_str().into()),
        );
        resp_obj.insert("originalBytes".into(), Value::from(plan.original_bytes));
    }

    interconnect::send_json(
        addr,
        pkg,
        FETCH_TAG,
        wrap_with_id(id, "resp", Value::Object(resp_obj)),
    );
    Ok(())
}

fn send_chunked(
    addr: &str,
    pkg: &str,
    id: Option<&str>,
    resp: &FetchResponse,
    plan: TransferPlan,
    chunk_size: usize,
) {
    let total_bytes = plan.payload.len();
    let chunk_count = total_bytes.div_ceil(chunk_size);
    let ack_paced = plan.ack_window > 0;

    tracing::info!(
        "fetch chunked: pkg={} addr={} id={} original={} compressed={} chunk_size={} chunks={} enc={} comp={} ack_window={}",
        pkg,
        addr,
        id.unwrap_or(""),
        plan.original_bytes,
        total_bytes,
        chunk_size,
        chunk_count,
        plan.encoding.as_str(),
        plan.compression.as_str(),
        plan.ack_window,
    );

    // Header: keep every v1 field, then append v2 chunking metadata and any
    // v3 codec annotations. Old peers never opt into chunking so they never
    // get here in the first place.
    let mut resp_obj = Map::new();
    resp_obj.insert("ok".into(), Value::Bool(resp.ok));
    resp_obj.insert("status".into(), Value::from(resp.status));
    resp_obj.insert("statusText".into(), Value::String(resp.status_text.into()));
    resp_obj.insert("headers".into(), Value::Object(resp.headers.clone()));
    resp_obj.insert("body".into(), Value::String(String::new()));
    resp_obj.insert("raw".into(), Value::Bool(resp.raw));
    resp_obj.insert("chunked".into(), Value::Bool(true));
    // `totalBytes` keeps its v2 meaning: payload as it appears on the wire
    // (= compressed size, because that's what the peer needs to buffer).
    resp_obj.insert("totalBytes".into(), Value::from(total_bytes));
    resp_obj.insert("chunkSize".into(), Value::from(chunk_size));
    resp_obj.insert("chunkCount".into(), Value::from(chunk_count));
    resp_obj.insert(
        "bodyEncoding".into(),
        Value::String(plan.encoding.as_str().into()),
    );
    if plan.compression != Compression::None {
        resp_obj.insert(
            "compression".into(),
            Value::String(plan.compression.as_str().into()),
        );
        // `originalBytes` is the uncompressed size — handy for the peer to
        // size its post-decompression buffer up front.
        resp_obj.insert("originalBytes".into(), Value::from(plan.original_bytes));
    }
    // Tell the peer this transfer is ACK-paced so it knows to emit `fetch-ack`.
    // Optional field; legacy peers (which never negotiate `ack`) never see it.
    if ack_paced {
        resp_obj.insert("ack".into(), Value::Bool(true));
    }
    interconnect::send_json(
        addr,
        pkg,
        FETCH_TAG,
        wrap_with_id(id, "resp", Value::Object(resp_obj)),
    );

    // Header is out (compat rule #1). Now ship the body.
    if ack_paced {
        // ACK-paced path: register the transfer and prime the first window.
        // `transfer` ships at most `window` chunks now and resumes from
        // `handle_ack` as the peer acknowledges — bounding in-flight bytes and
        // yielding control back to the host between bursts.
        transfer::begin(
            addr,
            pkg,
            id,
            plan.payload,
            chunk_size,
            plan.encoding,
            plan.ack_window,
        );
        return;
    }

    // Legacy un-paced path: the peer can't ACK, so blast every chunk in one go,
    // exactly like v2. Fine for the modest responses such peers receive; large
    // ones are why v3 added ACK pacing above.
    for (seq, chunk) in plan.payload.chunks(chunk_size).enumerate() {
        let encoded = codec::encode(chunk, plan.encoding)
            .unwrap_or_else(|_| codec::encode(chunk, BodyEncoding::Base64).unwrap());
        let mut msg = Map::new();
        if let Some(id) = id {
            msg.insert("id".to_string(), Value::String(id.to_string()));
        }
        msg.insert("seq".to_string(), Value::from(seq));
        msg.insert("total".to_string(), Value::from(chunk_count));
        msg.insert("data".to_string(), Value::String(encoded));
        interconnect::send_json(addr, pkg, FETCH_CHUNK_TAG, Value::Object(msg));
    }
}

/// Handle a peer `fetch-ack` frame: `{ id?, ack }`. `ack` is the next
/// contiguous chunk index the peer still needs. Drives the sliding window so
/// the next batch of chunks goes out (see `transfer::on_ack`).
pub fn handle_ack(addr: &str, pkg: &str, body: Value) {
    handshake::record_activity(addr, pkg);
    let id = body.get("id").and_then(|v| v.as_str());
    let ack = body.get("ack").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    tracing::debug!(
        "fetch-ack: pkg={} addr={} id={} ack={}",
        pkg,
        addr,
        id.unwrap_or(""),
        ack
    );
    transfer::on_ack(addr, pkg, id, ack);
}

fn send_error(addr: &str, pkg: &str, id: Option<&str>, message: &str) {
    let resp_value = json!({
        "ok": false,
        "status": 0,
        "statusText": message,
        "headers": {},
        "body": "",
        "raw": false,
    });
    interconnect::send_json(addr, pkg, FETCH_TAG, wrap_with_id(id, "resp", resp_value));
}

fn wrap_with_id(id: Option<&str>, key: &str, value: Value) -> Value {
    let mut payload = Map::new();
    payload.insert(key.to_string(), value);
    if let Some(id) = id {
        payload.insert("id".to_string(), Value::String(id.to_string()));
    }
    Value::Object(payload)
}

/// The encoding a v1 peer would have produced for this body: `text` for
/// UTF-8 text responses, `base64` for raw / binary. Used to decide whether to
/// annotate `bodyEncoding` on the wire — annotating the legacy choice would
/// just be noise that a strict v1 parser shouldn't even see.
fn legacy_encoding_for(raw: bool) -> BodyEncoding {
    if raw {
        BodyEncoding::Base64
    } else {
        BodyEncoding::Text
    }
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
