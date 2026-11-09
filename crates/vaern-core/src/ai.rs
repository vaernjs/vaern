//! Native half of `vaern:ai`: chat completions against any OpenAI-compatible
//! endpoint, cloud or localhost.
//!
//! The call is made here rather than with `fetch` from JavaScript so that the
//! runtime owns every step a budget or a journal depends on: it sets the
//! output cap the provider must honour, it sees the reply before the script
//! does, and it computes the charge. No provider is named anywhere; the base
//! URL and key come from the script or its environment.
//!
//! Money is integer nano-USD throughout. A price is given in USD per million
//! tokens, so one nano-USD per token equals 0.001 USD per million tokens, the
//! finest price this module resolves.

use crate::error::{Result, VaernError};
use crate::fetch::{FetchOptions, HttpClient};
use serde_json::{Map, Value, json};
use std::time::Duration;

/// Output cap sent when neither the model nor the call sets one.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 1024;

/// Longest provider error body quoted in a `ModelError`.
const ERROR_EXCERPT_BYTES: usize = 500;

/// A model as configured by `model({...})`. `Debug` omits the key, so a
/// logged or panicking value cannot print it.
#[derive(Clone)]
pub struct ModelConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub input_nano_per_token: u64,
    pub output_nano_per_token: u64,
    pub max_output_tokens: u64,
    pub timeout: Duration,
}

impl std::fmt::Debug for ModelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("input_nano_per_token", &self.input_nano_per_token)
            .field("output_nano_per_token", &self.output_nano_per_token)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// A call ready to send: the endpoint, the request body (without the key),
/// and the output cap written into it.
#[derive(Debug, Clone)]
pub struct PreparedCall {
    pub endpoint: String,
    pub body: Value,
    pub max_tokens: u64,
    pub config: ModelConfig,
    /// The schema the reply must match, when the call asked for one.
    pub schema: Option<Value>,
    /// Read the reply as server-sent events; see `ai_stream`.
    pub stream: bool,
}

/// What the provider answered.
#[derive(Debug, Clone)]
pub struct ModelReply {
    pub status: u16,
    /// The response body exactly as received.
    pub raw: String,
    pub text: Option<String>,
    pub finish_reason: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub tool_calls: Value,
}

fn model_error(stage: &str, url: &str, status: u16, message: String, action: &str) -> VaernError {
    VaernError::Model {
        stage: stage.to_string(),
        url: url.to_string(),
        status,
        message,
        action: action.to_string(),
    }
}

/// Builds a call from the JSON the prelude sends: the model's configuration
/// and one call's options.
pub fn prepare(config_json: &str, call_json: &str) -> Result<PreparedCall> {
    let bad = |message: String| {
        model_error(
            "config",
            "",
            0,
            message,
            "Fix the options passed to model() or to the call.",
        )
    };
    let config: Value = serde_json::from_str(config_json)
        .map_err(|err| bad(format!("invalid model options: {err}")))?;
    let call: Value = serde_json::from_str(call_json)
        .map_err(|err| bad(format!("invalid call options: {err}")))?;

    let text = |key: &str| config.get(key).and_then(Value::as_str).map(str::to_string);
    let base_url = text("baseUrl")
        .filter(|u| !u.is_empty())
        .ok_or_else(|| bad("baseUrl is required (or set VAERN_AI_BASE_URL)".to_string()))?;
    let parsed = url::Url::parse(&base_url)
        .map_err(|err| bad(format!("baseUrl '{base_url}' is not a URL: {err}")))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(bad(format!("baseUrl '{base_url}' must use http or https")));
    }
    let model = text("model")
        .filter(|m| !m.is_empty())
        .ok_or_else(|| bad("model is required".to_string()))?;
    let price = |key: &str| -> Result<u64> {
        let per_mtok = config.get(key).and_then(Value::as_f64).unwrap_or(0.0);
        if !per_mtok.is_finite() || per_mtok < 0.0 || per_mtok > 1.0e9 {
            return Err(bad(format!(
                "{key} must be a price in USD per million tokens, 0 or more"
            )));
        }
        // USD per million tokens × 1e9 nano-USD per USD ÷ 1e6 tokens.
        Ok((per_mtok * 1000.0).round() as u64)
    };
    let positive = |value: Option<&Value>, key: &str, default: u64| -> Result<u64> {
        match value {
            None | Some(Value::Null) => Ok(default),
            Some(v) => v
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or_else(|| bad(format!("{key} must be a whole number above 0"))),
        }
    };
    let max_output_tokens = positive(
        config.get("maxOutputTokens"),
        "maxOutputTokens",
        DEFAULT_MAX_OUTPUT_TOKENS,
    )?;
    let timeout_ms = positive(config.get("timeoutMs"), "timeoutMs", 60_000)?;
    let config = ModelConfig {
        base_url: base_url.clone(),
        model: model.clone(),
        api_key: text("apiKey").filter(|k| !k.is_empty()),
        input_nano_per_token: price("inputPerMTok")?,
        output_nano_per_token: price("outputPerMTok")?,
        max_output_tokens,
        timeout: Duration::from_millis(timeout_ms),
    };

    let messages = call
        .get("messages")
        .filter(|m| m.as_array().is_some_and(|a| !a.is_empty()))
        .cloned()
        .ok_or_else(|| bad("messages must be a non-empty array".to_string()))?;
    let max_tokens = positive(call.get("maxTokens"), "maxTokens", max_output_tokens)?;
    let mut body = Map::new();
    body.insert("model".to_string(), Value::from(model));
    body.insert("messages".to_string(), messages);
    // Always sent: the reservation counts on the provider stopping here.
    body.insert("max_tokens".to_string(), Value::from(max_tokens));
    for (from, to) in [
        ("temperature", "temperature"),
        ("responseFormat", "response_format"),
        ("tools", "tools"),
    ] {
        if let Some(value) = call.get(from).filter(|v| !v.is_null()) {
            body.insert(to.to_string(), value.clone());
        }
    }
    let schema = call.get("schema").filter(|s| !s.is_null()).cloned();
    if let Some(schema) = &schema {
        if body.contains_key("response_format") {
            return Err(bad(
                "pass either schema or responseFormat, not both".to_string()
            ));
        }
        crate::ai_schema::check_schema(schema)
            .map_err(|err| bad(format!("schema at {}: {}", err.path, err.message)))?;
        let name = match call.get("schemaName") {
            None | Some(Value::Null) => "reply".to_string(),
            Some(Value::String(name)) if !name.is_empty() => name.clone(),
            Some(_) => return Err(bad("schemaName must be a non-empty string".to_string())),
        };
        body.insert(
            "response_format".to_string(),
            json!({
                "type": "json_schema",
                "json_schema": { "name": name, "schema": schema, "strict": true },
            }),
        );
    }
    let stream = call.get("stream") == Some(&Value::Bool(true));
    if stream {
        // A streamed reply is handed on as text fragments; it is not
        // assembled into JSON or tool calls.
        if schema.is_some() || body.contains_key("tools") {
            return Err(bad(
                "stream() takes neither schema nor tools; use chat() for those".to_string(),
            ));
        }
        body.insert("stream".to_string(), Value::Bool(true));
        // Without this the final event has no usage and the call is charged
        // its estimate.
        body.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );
    }
    Ok(PreparedCall {
        endpoint: format!("{}/chat/completions", base_url.trim_end_matches('/')),
        body: Value::Object(body),
        max_tokens,
        config,
        schema,
        stream,
    })
}

/// Sends `call` and parses the reply.
pub async fn send(client: &HttpClient, call: &PreparedCall) -> Result<ModelReply> {
    let mut headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];
    if let Some(key) = &call.config.api_key {
        headers.push(("authorization".to_string(), format!("Bearer {key}")));
    }
    let options = FetchOptions {
        method: "POST".to_string(),
        headers,
        body: Some(call.body.to_string().into_bytes()),
        timeout: Some(call.config.timeout),
        max_body_bytes: crate::fetch::MAX_RESPONSE_BODY_BYTES,
    };
    let response = crate::fetch::execute_fetch_pooled(client, &call.endpoint, options, None)
        .await
        .map_err(|err| {
            let message = match err {
                VaernError::Fetch { stage, message, .. } => format!("{stage}: {message}"),
                other => other.to_string(),
            };
            model_error(
                "connect",
                &call.endpoint,
                0,
                message,
                "Check that the model server is running and baseUrl is right.",
            )
        })?;
    parse_reply(&call.endpoint, response.status, response.body)
}

/// Reads an OpenAI-style chat completion response.
pub fn parse_reply(endpoint: &str, status: u16, body: Vec<u8>) -> Result<ModelReply> {
    let raw = String::from_utf8_lossy(&body).into_owned();
    if !(200..300).contains(&status) {
        let mut excerpt = raw.clone();
        if excerpt.len() > ERROR_EXCERPT_BYTES {
            let mut cut = ERROR_EXCERPT_BYTES;
            while !excerpt.is_char_boundary(cut) {
                cut -= 1;
            }
            excerpt.truncate(cut);
        }
        return Err(model_error(
            "http",
            endpoint,
            status,
            format!("the model server answered {status}: {excerpt}"),
            "Check the model name, the key and the server's log.",
        ));
    }
    let parse_error = |message: &str| {
        model_error(
            "parse",
            endpoint,
            status,
            message.to_string(),
            "The server is not OpenAI-compatible at this URL; check baseUrl.",
        )
    };
    let json: Value =
        serde_json::from_str(&raw).map_err(|_| parse_error("the reply is not JSON"))?;
    let choice = json
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| parse_error("the reply has no choices"))?;
    let message = choice
        .get("message")
        .ok_or_else(|| parse_error("the reply has no message"))?;
    let usage = json.get("usage");
    let tokens = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
    Ok(ModelReply {
        status,
        text: message
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string),
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        input_tokens: tokens("prompt_tokens"),
        output_tokens: tokens("completion_tokens"),
        tool_calls: message.get("tool_calls").cloned().unwrap_or(Value::Null),
        raw,
    })
}

/// The charge for a reply, in nano-USD. Without reported usage the caller
/// keeps whatever it reserved.
pub fn charge_nano(config: &ModelConfig, input_tokens: u64, output_tokens: u64) -> u64 {
    input_tokens
        .saturating_mul(config.input_nano_per_token)
        .saturating_add(output_tokens.saturating_mul(config.output_nano_per_token))
}

/// Input tokens assumed before a call: a third of the request's UTF-8 bytes,
/// rounded up, plus 16 per message for the provider's framing. Tokenizers
/// average three to four bytes per token on prose, so this errs high.
pub fn estimate_input_tokens(call: &PreparedCall) -> u64 {
    let bytes = call.body.to_string().len() as u64;
    let messages = call
        .body
        .get("messages")
        .and_then(Value::as_array)
        .map_or(0, |m| m.len() as u64);
    bytes
        .div_ceil(3)
        .saturating_add(messages.saturating_mul(16))
}

/// The most a call can cost before it is sent: the input estimate and the
/// full output cap, at the model's prices.
pub fn reserve_nano(call: &PreparedCall) -> u64 {
    charge_nano(&call.config, estimate_input_tokens(call), call.max_tokens)
}

/// The reply's text parsed as JSON and checked against the call's schema.
/// `Ok(None)` when the call asked for no schema.
pub fn structured(
    call: &PreparedCall,
    reply: &ModelReply,
) -> std::result::Result<Option<Value>, crate::ai_schema::SchemaMismatch> {
    let Some(schema) = &call.schema else {
        return Ok(None);
    };
    let not_json = |message: String| crate::ai_schema::SchemaMismatch {
        path: "$".to_string(),
        message,
    };
    let text = reply
        .text
        .as_deref()
        .ok_or_else(|| not_json("the reply has no text".to_string()))?;
    let value: Value = serde_json::from_str(text)
        .map_err(|err| not_json(format!("the reply is not JSON: {err}")))?;
    crate::ai_schema::validate(&value, schema)?;
    Ok(Some(value))
}

/// The journal's `req` for a call: everything sent except the key.
pub fn request_record(call: &PreparedCall) -> Value {
    json!({
        "base_url": call.config.base_url,
        "model": call.config.model,
        "body": call.body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: &str) -> String {
        format!(r#"{{"baseUrl": "http://127.0.0.1:11434/v1/", "model": "m"{extra}}}"#)
    }

    #[test]
    fn test_prepare_sets_the_output_cap_and_keeps_the_key_out_of_the_body() {
        let call = prepare(
            &config(r#", "apiKey": "sk-1", "maxOutputTokens": 64, "inputPerMTok": 0.15, "outputPerMTok": 0.6"#),
            r#"{"messages": [{"role": "user", "content": "hi"}], "temperature": 0}"#,
        )
        .unwrap();
        assert_eq!(call.endpoint, "http://127.0.0.1:11434/v1/chat/completions");
        assert_eq!(call.body["max_tokens"], 64);
        assert_eq!(call.body["temperature"], 0);
        assert!(!call.body.to_string().contains("sk-1"));
        assert!(!request_record(&call).to_string().contains("sk-1"));
        assert!(!format!("{call:?}").contains("sk-1"));
        assert_eq!(call.config.input_nano_per_token, 150);
        assert_eq!(call.config.output_nano_per_token, 600);
        // 1000 input and 100 output tokens at 0.15 and 0.6 USD per million.
        assert_eq!(charge_nano(&call.config, 1000, 100), 150_000 + 60_000);

        let narrower = prepare(&config(""), r#"{"messages": [{}], "maxTokens": 8}"#).unwrap();
        assert_eq!(narrower.body["max_tokens"], 8);
        let default = prepare(&config(""), r#"{"messages": [{}]}"#).unwrap();
        assert_eq!(default.body["max_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
    }

    #[test]
    fn test_prepare_refuses_bad_options_with_a_config_error() {
        for (cfg, call) in [
            (r#"{"model": "m"}"#.to_string(), r#"{"messages": [{}]}"#),
            (
                r#"{"baseUrl": "ftp://x", "model": "m"}"#.to_string(),
                r#"{"messages": [{}]}"#,
            ),
            (config(""), r#"{"messages": []}"#),
            (config(r#", "inputPerMTok": -1"#), r#"{"messages": [{}]}"#),
            (config(""), r#"{"messages": [{}], "maxTokens": 0}"#),
        ] {
            match prepare(&cfg, call) {
                Err(VaernError::Model { stage, .. }) => assert_eq!(stage, "config"),
                other => panic!("expected a config error for {cfg} {call}, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_replies_are_parsed_and_failures_typed() {
        let body = br#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#;
        let reply = parse_reply("u", 200, body.to_vec()).unwrap();
        assert_eq!(reply.text.as_deref(), Some("ok"));
        assert_eq!(
            (reply.input_tokens, reply.output_tokens),
            (Some(3), Some(1))
        );
        assert_eq!(reply.raw.as_bytes(), body);

        let stage = |status, body: &[u8]| match parse_reply("u", status, body.to_vec()) {
            Err(VaernError::Model { stage, .. }) => stage,
            other => panic!("expected a model error, got {other:?}"),
        };
        assert_eq!(stage(500, b"boom"), "http");
        assert_eq!(stage(200, b"not json"), "parse");
        assert_eq!(stage(200, b"{}"), "parse");
    }
}
