//! Wire codec for fetch response bodies.
//!
//! The plugin can encode the body in several ways and optionally compress it
//! beforehand. Both choices are negotiated through the handshake so the peer
//! (a quickapp running on an RTOS watch) can pick the tradeoff it wants —
//! tiny CPU + bigger wire (hex / none) or smaller wire + heavier decode
//! (base64 / deflate). The legacy single-message `base64 + none` path is the
//! baseline every peer is assumed to support.

use std::io::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyEncoding {
    /// JSON string carries the body bytes as UTF-8 text. Only valid when the
    /// (post-compression) bytes are valid UTF-8 and the response isn't
    /// chunked.
    Text,
    /// Standard base64 (RFC 4648). Default; ~4/3 expansion, moderate decode.
    Base64,
    /// Lowercase hex (`0-9a-f`). 2× expansion but trivial to decode on an MCU
    /// (two table lookups per byte).
    Hex,
}

impl BodyEncoding {
    pub fn as_str(self) -> &'static str {
        match self {
            BodyEncoding::Text => "text",
            BodyEncoding::Base64 => "base64",
            BodyEncoding::Hex => "hex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(BodyEncoding::Text),
            "base64" => Some(BodyEncoding::Base64),
            "hex" => Some(BodyEncoding::Hex),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    /// Raw deflate (RFC 1951), zlib-compatible payload without headers. Good
    /// ratio, heavier decode — favored when bandwidth is precious.
    Deflate,
    /// LZ4 block format. Worse ratio than deflate but much cheaper to decode
    /// on MCU — favored when decode CPU is the bottleneck.
    Lz4,
}

impl Compression {
    pub fn as_str(self) -> &'static str {
        match self {
            Compression::None => "none",
            Compression::Deflate => "deflate",
            Compression::Lz4 => "lz4",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Compression::None),
            "deflate" => Some(Compression::Deflate),
            "lz4" => Some(Compression::Lz4),
            _ => None,
        }
    }
}

/// Encodings this plugin knows how to *produce*. Order is informational only —
/// the peer's preference order wins during negotiation.
pub const SUPPORTED_ENCODINGS: &[BodyEncoding] = &[
    BodyEncoding::Base64,
    BodyEncoding::Hex,
    BodyEncoding::Text,
];

/// Compressions this plugin knows how to *produce*.
pub const SUPPORTED_COMPRESSIONS: &[Compression] = &[
    Compression::None,
    Compression::Deflate,
    Compression::Lz4,
];

/// Bodies smaller than this byte threshold are sent uncompressed even when a
/// compressor was negotiated — the per-call overhead isn't worth it and most
/// short payloads don't compress meaningfully.
pub const COMPRESS_MIN_SIZE: usize = 256;

pub fn compress(data: &[u8], algo: Compression) -> Vec<u8> {
    match algo {
        Compression::None => data.to_vec(),
        Compression::Deflate => {
            use flate2::Compression as FlateLevel;
            use flate2::write::DeflateEncoder;
            let mut enc = DeflateEncoder::new(
                Vec::with_capacity(data.len() / 2 + 16),
                FlateLevel::default(),
            );
            // Writing to a Vec<u8> can't fail; finish() likewise only fails on
            // the underlying writer.
            enc.write_all(data).expect("deflate write");
            enc.finish().expect("deflate finish")
        }
        Compression::Lz4 => lz4_flex::compress(data),
    }
}

/// Encode bytes into a JSON-safe string per the chosen wire encoding.
///
/// Returns `Err` only when `Text` is requested but the bytes aren't valid
/// UTF-8 — callers must fall back to a binary encoding in that case.
pub fn encode(data: &[u8], enc: BodyEncoding) -> Result<String, ()> {
    match enc {
        BodyEncoding::Text => {
            std::str::from_utf8(data).map(|s| s.to_string()).map_err(|_| ())
        }
        BodyEncoding::Base64 => Ok(base64_encode(data)),
        BodyEncoding::Hex => Ok(hex_encode(data)),
    }
}

fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
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

fn hex_encode(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for &b in data {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}
