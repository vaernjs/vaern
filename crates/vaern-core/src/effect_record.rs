//! Journal records for effects that leave the process: `fetch`, `vaern:fs`
//! and `vaern:ai` model calls; tool calls use the same shape.
//!
//! Each effect gets an `eid` from one counter in dispatch order, and one
//! record written when it completes: `{eid, dur_us, req, res}` or
//! `{eid, dur_us, req, error}`. Journal order is therefore completion order,
//! and `eid` recovers dispatch order, which a replay needs to hand concurrent
//! results back in the order they arrived.
//!
//! Credentials in the usual places are replaced with `"[redacted]"` before a
//! record is written: the headers listed in [`REDACTED_HEADERS`], and in
//! every recorded URL the password and the query values [`redact_url`]
//! names. A secret carried anywhere else, such as a body, is recorded as
//! sent.

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

/// A query parameter whose name contains one of these, ignoring case, has
/// its value replaced in recorded URLs.
pub const REDACTED_QUERY_NAMES: [&str; 7] = [
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "signature",
    "credential",
];

/// `url` as it is recorded: the password in `user:password@host` and the
/// values of query parameters named in [`REDACTED_QUERY_NAMES`] become
/// `[redacted]`. A URL with neither, or one that does not parse, is returned
/// unchanged, byte for byte.
pub fn redact_url(url: &str) -> String {
    use url::Position;
    let Ok(parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    let secret_name = |pair: &str| {
        let name = pair.split('=').next().unwrap_or(pair).to_ascii_lowercase();
        pair.contains('=') && REDACTED_QUERY_NAMES.iter().any(|word| name.contains(word))
    };
    let query = parsed.query().unwrap_or("");
    if parsed.password().is_none() && !query.split('&').any(secret_name) {
        return url.to_string();
    }

    let mut out = parsed[..Position::BeforeUsername].to_string();
    if parsed.password().is_some() {
        out.push_str(parsed.username());
        out.push_str(":[redacted]@");
    } else {
        out.push_str(&parsed[Position::BeforeUsername..Position::BeforeHost]);
    }
    out.push_str(&parsed[Position::BeforeHost..Position::AfterPath]);
    if parsed.query().is_some() {
        let pairs: Vec<String> = query
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((name, _)) if secret_name(pair) => format!("{name}=[redacted]"),
                _ => pair.to_string(),
            })
            .collect();
        out.push('?');
        out.push_str(&pairs.join("&"));
    }
    if let Some(fragment) = parsed.fragment() {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// A recorded request with its URL redacted as a request recorded today
/// would be. A replay compares the request it builds with this, so a run
/// recorded before URLs were redacted still matches.
pub fn redacted_request(req: &Value) -> std::borrow::Cow<'_, Value> {
    for key in ["url", "base_url"] {
        let Some(url) = req.get(key).and_then(Value::as_str) else {
            continue;
        };
        let redacted = redact_url(url);
        if redacted != url {
            let mut changed = req.clone();
            changed[key] = Value::from(redacted);
            return std::borrow::Cow::Owned(changed);
        }
    }
    std::borrow::Cow::Borrowed(req)
}

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
                    "[redacted]".to_string()
                } else if lower == "location" || lower == "content-location" {
                    redact_url(value)
                } else {
                    value.clone()
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
        "url": redact_url(url),
        "headers": headers_value(headers),
        "body": body.map(bytes_value),
    })
}

pub fn fetch_outcome(result: &Result<FetchResponse>) -> Outcome {
    match result {
        Ok(resp) => Outcome::Response(json!({
            "status": resp.status,
            "status_text": resp.status_text,
            "url": redact_url(&resp.url),
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
            "stage": stage, "url": redact_url(url), "status": status,
            "message": without_url(message, url), "action": action, "sent": sent,
        }),
        VaernError::Fetch {
            stage,
            url,
            message,
            action,
        } => json!({
            "stage": stage, "url": redact_url(url), "message": without_url(message, url),
            "action": action,
        }),
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

/// `message` with any copy of `url` in it redacted like the URL itself.
fn without_url(message: &str, url: &str) -> String {
    let redacted = redact_url(url);
    if url.is_empty() || redacted == url {
        return message.to_string();
    }
    message.replace(url, &redacted)
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
    fn test_recorded_urls_lose_passwords_and_secret_query_values_only() {
        for (url, recorded) in [
            (
                "https://ann:hunter2@example.com/a?x=1#top",
                "https://ann:[redacted]@example.com/a?x=1#top",
            ),
            (
                "https://example.com/v1?api_key=K1&q=two+words&Access-Token=T2&empty=",
                "https://example.com/v1?api_key=[redacted]&q=two+words&Access-Token=[redacted]&empty=",
            ),
            (
                "http://u:p@127.0.0.1:8080/x?X-Amz-Signature=abc&sig",
                "http://u:[redacted]@127.0.0.1:8080/x?X-Amz-Signature=[redacted]&sig",
            ),
        ] {
            assert_eq!(redact_url(url), recorded);
            // Redacting what is already redacted changes nothing.
            assert_eq!(redact_url(recorded), recorded);
        }
        // Nothing to hide: the text is kept exactly, whatever its spelling.
        for url in [
            "HTTP://Example.COM",
            "https://ann@example.com/a?page=2",
            "not a url?token=x",
            "",
        ] {
            assert_eq!(redact_url(url), url);
        }

        // A request recorded before redaction compares equal to today's.
        let raw = json!({ "method": "GET", "url": "https://a:b@h/?token=t", "headers": [], "body": null });
        let today = fetch_request("GET", "https://a:b@h/?token=t", &[], None);
        assert_ne!(raw, today);
        assert_eq!(*redacted_request(&raw), today);
        assert_eq!(*redacted_request(&today), today);
        let model = json!({ "base_url": "https://k:s@h/v1", "model": "m" });
        assert_eq!(
            redacted_request(&model)["base_url"],
            "https://k:[redacted]@h/v1"
        );

        let failed = error_value(&VaernError::Fetch {
            stage: "connect".into(),
            url: "https://a:b@h/".into(),
            message: "could not reach https://a:b@h/".into(),
            action: "retry".into(),
        });
        assert!(!failed.to_string().contains("a:b@"), "{failed}");
    }

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
