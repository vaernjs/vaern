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
        Err(err) => Outcome::Error(error_value(err)),
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
        } => json!({
            "stage": stage, "url": url, "status": status, "message": message, "action": action,
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
