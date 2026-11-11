//! Journal records for effects that leave the process: `fetch`, `vaern:fs`
//! and `vaern:ai` model calls; tool calls use the same shape.
//!
//! Each effect gets an `eid` from one counter in dispatch order, and one
//! record written when it completes: `{eid, dur_us, req, res}` or
//! `{eid, dur_us, req, error}`. Journal order is therefore completion order,
//! and `eid` recovers dispatch order, which a replay needs to hand concurrent
//! results back in the order they arrived.
//!
//! Secrets never reach the journal: the request and response headers listed
//! in [`REDACTED_HEADERS`] are stored as `"[redacted]"`.

use crate::ai::ModelReply;
use crate::error::{Result, VaernError};
use crate::fetch::FetchResponse;
use crate::fs::{FsOp, FsOutput, WriteMode};
use serde_json::{Map, Value, json};
use std::time::Instant;

/// Headers whose values are replaced before they are recorded.
pub const REDACTED_HEADERS: [&str; 6] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
];

/// An effect that has been dispatched and not yet recorded.
pub struct PendingEffect {
    pub eid: u64,
    pub started: Instant,
    pub req: Value,
}

impl PendingEffect {
    /// The record's fields once the effect has completed with `outcome`.
    pub fn finish(self, outcome: Outcome) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("eid".to_string(), Value::from(self.eid));
        let dur = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        fields.insert("dur_us".to_string(), Value::from(dur));
        fields.insert("req".to_string(), self.req);
        match outcome {
            Outcome::Response(res) => fields.insert("res".to_string(), res),
            Outcome::Error(err) => fields.insert("error".to_string(), err),
        };
        fields
    }
}

/// How an effect ended, as recorded.
pub enum Outcome {
    Response(Value),
    Error(Value),
}

/// Bytes as `{"utf8": text}` when they are valid UTF-8, else `{"b64": …}`.
pub fn bytes_value(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(text) => json!({ "utf8": text }),
        Err(_) => json!({ "b64": base64(bytes) }),
    }
}

/// The bytes behind a [`bytes_value`], or `None` when it is not one.
pub fn bytes_of(value: &Value) -> Option<Vec<u8>> {
    if let Some(text) = value.get("utf8").and_then(Value::as_str) {
        return Some(text.as_bytes().to_vec());
    }
    unbase64(value.get("b64")?.as_str()?)
}

/// Header pairs as recorded. A redacted value stays `"[redacted]"`.
pub fn headers_of(value: &Value) -> Vec<(String, String)> {
    let pair = |pair: &Value| {
        let name = pair.get(0)?.as_str()?;
        Some((name.to_string(), pair.get(1)?.as_str()?.to_string()))
    };
    value
        .as_array()
        .map(|pairs| pairs.iter().filter_map(pair).collect())
        .unwrap_or_default()
}

/// Header pairs in order, secrets redacted.
pub fn headers_value(headers: &[(String, String)]) -> Value {
    Value::Array(
        headers
            .iter()
            .map(|(name, value)| {
                let lower = name.to_ascii_lowercase();
                let shown = if REDACTED_HEADERS.contains(&lower.as_str()) {
                    "[redacted]"
                } else {
                    value.as_str()
                };
                json!([name, shown])
            })
            .collect(),
    )
}

pub fn fetch_request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Value {
    json!({
        "method": method,
        "url": url,
        "headers": headers_value(headers),
        "body": body.map(bytes_value),
    })
}

pub fn fetch_outcome(result: &Result<FetchResponse>) -> Outcome {
    match result {
        Ok(resp) => Outcome::Response(json!({
            "status": resp.status,
            "status_text": resp.status_text,
            "url": resp.url,
            "headers": headers_value(&resp.headers),
            "body": bytes_value(&resp.body),
        })),
        Err(err) => Outcome::Error(error_value(err)),
    }
}

pub fn fs_request(op: &FsOp) -> Value {
    match op {
        FsOp::Read { path, utf8 } => json!({
            "op": if *utf8 { "read_utf8" } else { "read" },
            "path": path,
        }),
        FsOp::Write { path, data, mode } => json!({
            "op": match mode {
                WriteMode::Truncate => "write",
                WriteMode::CreateNew => "write_new",
                WriteMode::Append => "append",
            },
            "path": path,
            "len": data.len(),
            "sha256": crate::journal::sha256_hex(data),
        }),
    }
}

pub fn fs_outcome(result: &Result<FsOutput>) -> Outcome {
    match result {
        Ok(FsOutput::Bytes(bytes)) => Outcome::Response(json!({ "bytes": bytes_value(bytes) })),
        Ok(FsOutput::Text(text)) => Outcome::Response(json!({ "text": text })),
        Ok(FsOutput::Written) => Outcome::Response(json!({})),
        Err(err) => Outcome::Error(error_value(err)),
    }
}

/// A model call's result. `charged` is the nano-USD the call was charged:
/// from reported usage, or the reservation when the server reported none.
pub fn model_outcome(result: &Result<ModelReply>, charged: u64) -> Outcome {
    match result {
        Ok(reply) => Outcome::Response(json!({
            "status": reply.status,
            "body": reply.raw,
            "usage": {
                "input_tokens": reply.input_tokens,
                "output_tokens": reply.output_tokens,
            },
            "charged_nano_usd": charged,
        })),
        Err(err) => {
            let mut error = error_value(err);
            if let Some(fields) = error.as_object_mut() {
                fields.insert("charged_nano_usd".to_string(), charged.into());
            }
            Outcome::Error(error)
        }
    }
}

/// An error as recorded: every field a replay needs to hand the script the
/// same error again.
fn error_value(err: &VaernError) -> Value {
    match err {
        VaernError::Model {
            stage,
            url,
            status,
            message,
            action,
            sent,
        } => json!({
            "stage": stage, "url": url, "status": status, "message": message, "action": action,
            "sent": sent,
        }),
        VaernError::Fetch {
            stage,
            url,
            message,
            action,
        } => json!({ "stage": stage, "url": url, "message": message, "action": action }),
        VaernError::Fs {
            stage,
            code,
            path,
            message,
            action,
        } => json!({
            "stage": stage, "code": code, "path": path, "message": message, "action": action,
        }),
        other => json!({ "message": other.to_string() }),
    }
}

/// Standard base64 with padding (RFC 4648).
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(TABLE[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The inverse of [`base64`]; `None` for text it could not have produced.
fn unbase64(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let digit = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for quad in text.as_bytes().chunks(4) {
        let pad = quad.iter().rev().take_while(|c| **c == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut n = 0u32;
        for c in &quad[..4 - pad] {
            n = (n << 6) | u32::from(digit(*c)?);
        }
        n <<= 6 * pad as u32;
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_matches_rfc_4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected);
        }
    }

    #[test]
    fn test_recorded_bytes_decode_to_what_was_recorded() {
        for len in 0..40usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 200) as u8).collect();
            assert_eq!(bytes_of(&bytes_value(&bytes)), Some(bytes));
        }
        assert_eq!(bytes_of(&json!({ "b64": "Zm9=v" })), None);
        assert_eq!(bytes_of(&json!({ "b64": "a===" })), None);
    }

    #[test]
    fn test_bytes_are_text_when_they_can_be() {
        assert_eq!(bytes_value(b"hi"), json!({ "utf8": "hi" }));
        assert_eq!(bytes_value(&[0xff, 0x00]), json!({ "b64": "/wA=" }));
    }

    #[test]
    fn test_secret_headers_are_redacted_whatever_their_case() {
        let headers = vec![
            ("Authorization".to_string(), "Bearer sk-live".to_string()),
            ("X-Api-Key".to_string(), "k".to_string()),
            ("Set-Cookie".to_string(), "s=1".to_string()),
            ("Accept".to_string(), "*/*".to_string()),
        ];
        let recorded = headers_value(&headers).to_string();
        assert!(!recorded.contains("sk-live"), "{recorded}");
        assert!(!recorded.contains("\"k\""), "{recorded}");
        assert!(!recorded.contains("s=1"), "{recorded}");
        assert!(recorded.contains(r#"["Accept","*/*"]"#), "{recorded}");
        assert!(
            recorded.contains(r#"["Authorization","[redacted]"]"#),
            "{recorded}"
        );
    }
}
