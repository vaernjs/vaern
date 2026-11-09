//! Streamed chat completions for `vaern:ai`: one POST, read as server-sent
//! events, with each text delta handed on as it arrives.
//!
//! This does not go through `fetch`, which reads a body whole. A model
//! endpoint is called directly, so redirects are not followed; a 3xx is
//! reported as an `http` failure like any other non-2xx status.
//!
//! The raw event text is kept as the reply's body. The journal records it
//! as received, so the deltas a script saw can be derived again from it.

use crate::ai::{ModelReply, PreparedCall};
use crate::error::{Result, VaernError};
use crate::fetch::HttpClient;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use hyper::{Method, Request};
use serde_json::Value;

/// Splits a `text/event-stream` body into the payloads of its `data:`
/// lines. OpenAI-compatible servers send one `data:` line per event, so
/// multi-line events are not joined.
#[derive(Default)]
pub struct SseReader {
    pending: Vec<u8>,
}

impl SseReader {
    /// Takes the next bytes of the body; returns the payloads of the lines
    /// they complete.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut payloads = Vec::new();
        let mut start = 0;
        while let Some(offset) = self.pending[start..].iter().position(|b| *b == b'\n') {
            let line = String::from_utf8_lossy(&self.pending[start..start + offset]);
            if let Some(data) = line.trim_end_matches('\r').strip_prefix("data:") {
                payloads.push(data.trim_start().to_string());
            }
            start += offset + 1;
        }
        self.pending.drain(..start);
        payloads
    }
}

/// What the events of one stream add up to.
#[derive(Default)]
pub struct StreamState {
    pub text: String,
    pub finish_reason: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub done: bool,
}

impl StreamState {
    /// Applies one event payload. Returns the text delta it carried, or the
    /// stage and message of an error the server sent in the stream.
    pub fn apply(
        &mut self,
        data: &str,
    ) -> std::result::Result<Option<String>, (&'static str, String)> {
        if data == "[DONE]" {
            self.done = true;
            return Ok(None);
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|_| ("parse", "a stream event is not JSON".to_string()))?;
        if let Some(error) = event.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| error.to_string(), str::to_string);
            return Err(("http", format!("the model server sent an error: {message}")));
        }
        if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
            self.input_tokens = usage.get("prompt_tokens").and_then(Value::as_u64);
            self.output_tokens = usage.get("completion_tokens").and_then(Value::as_u64);
        }
        let choice = event.get("choices").and_then(|c| c.get(0));
        if let Some(reason) = choice
            .and_then(|c| c.get("finish_reason"))
            .and_then(Value::as_str)
        {
            self.finish_reason = Some(reason.to_string());
        }
        let delta = choice
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty());
        Ok(delta.map(|d| {
            self.text.push_str(d);
            d.to_string()
        }))
    }
}

fn stream_error(call: &PreparedCall, stage: &str, status: u16, message: String) -> VaernError {
    let action = match stage {
        "connect" => "Check that the model server is running and baseUrl is right.",
        "config" => "Fix the options passed to model().",
        _ => "The server's stream is not OpenAI-compatible at this URL; check baseUrl.",
    };
    VaernError::Model {
        stage: stage.to_string(),
        url: call.endpoint.clone(),
        status,
        message,
        action: action.to_string(),
    }
}

/// Sends a streaming call. `on_delta` receives each text fragment in order;
/// the returned reply holds the whole text, the usage from the final event
/// and the raw event text as received.
pub async fn send(
    client: &HttpClient,
    call: &PreparedCall,
    on_delta: impl Fn(String),
) -> Result<ModelReply> {
    let uri: hyper::Uri = call
        .endpoint
        .parse()
        .map_err(|err| stream_error(call, "config", 0, format!("invalid endpoint URL: {err}")))?;
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "text/event-stream");
    if let Some(key) = &call.config.api_key {
        request = request.header(AUTHORIZATION, format!("Bearer {key}"));
    }
    // The builder's error names the bad header only, never its value.
    let request = request
        .body(Full::new(Bytes::from(call.body.to_string())).boxed())
        .map_err(|err| stream_error(call, "config", 0, format!("invalid request: {err}")))?;

    let timeout = call.config.timeout;
    let response = tokio::time::timeout(timeout, client.request(request))
        .await
        .map_err(|_| stream_error(call, "connect", 0, "no response within timeoutMs".into()))?
        .map_err(|err| stream_error(call, "connect", 0, err.to_string()))?;
    let status = response.status().as_u16();
    let ok = (200..300).contains(&status);
    let mut body = response.into_body();
    let mut raw = Vec::new();
    let mut reader = SseReader::default();
    let mut state = StreamState::default();
    loop {
        let frame = tokio::time::timeout(timeout, body.frame())
            .await
            .map_err(|_| {
                stream_error(
                    call,
                    "connect",
                    status,
                    "no stream event within timeoutMs".into(),
                )
            })?;
        let Some(frame) = frame else { break };
        let frame = frame.map_err(|err| {
            stream_error(call, "connect", status, format!("the stream broke: {err}"))
        })?;
        let Ok(chunk) = frame.into_data() else {
            continue;
        };
        if raw.len() + chunk.len() > crate::fetch::MAX_RESPONSE_BODY_BYTES {
            return Err(stream_error(
                call,
                "parse",
                status,
                "the stream is larger than the 256 MiB response limit".into(),
            ));
        }
        raw.extend_from_slice(&chunk);
        if !ok {
            continue;
        }
        for data in reader.push(&chunk) {
            match state.apply(&data) {
                Ok(Some(delta)) => on_delta(delta),
                Ok(None) => {}
                Err((stage, message)) => return Err(stream_error(call, stage, status, message)),
            }
        }
    }
    if !ok {
        return crate::ai::parse_reply(&call.endpoint, status, raw);
    }
    if !state.done && state.finish_reason.is_none() {
        return Err(stream_error(
            call,
            "parse",
            status,
            "the stream ended before the reply was finished".into(),
        ));
    }
    Ok(ModelReply {
        status,
        raw: String::from_utf8_lossy(&raw).into_owned(),
        text: Some(state.text),
        finish_reason: state.finish_reason,
        input_tokens: state.input_tokens,
        output_tokens: state.output_tokens,
        tool_calls: Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_events_split_across_chunks_are_read_whole() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\r\n\r\n\
                    : keep-alive\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"lo é\"},\"finish_reason\":\"stop\"}]}\n\n\
                    data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n\
                    data: [DONE]\n\n";
        // Every split point, including inside the two-byte é.
        for cut in 1..body.len() {
            let (a, b) = body.as_bytes().split_at(cut);
            let mut reader = SseReader::default();
            let mut state = StreamState::default();
            let mut deltas = Vec::new();
            for data in reader.push(a).into_iter().chain(reader.push(b)) {
                if let Some(delta) = state.apply(&data).unwrap() {
                    deltas.push(delta);
                }
            }
            assert_eq!(deltas, ["Hel", "lo é"], "cut at {cut}");
            assert_eq!(state.text, "Hello é");
            assert_eq!(state.finish_reason.as_deref(), Some("stop"));
            assert_eq!(
                (state.input_tokens, state.output_tokens),
                (Some(7), Some(2))
            );
            assert!(state.done);
        }
    }

    #[test]
    fn test_errors_in_the_stream_are_reported_with_a_stage() {
        let mut state = StreamState::default();
        assert_eq!(state.apply("{oops").unwrap_err().0, "parse");
        let (stage, message) = state
            .apply(r#"{"error":{"message":"context too long"}}"#)
            .unwrap_err();
        assert_eq!(stage, "http");
        assert!(message.contains("context too long"));
    }
}
