//! MCP wire protocol: JSON-RPC 2.0 messages framed one per line, and the
//! version rules of the two revisions the runtime speaks.
//!
//! `2026-07-28` (N) is stateless: every request names its version and the
//! client's capabilities in `_meta`, and `server/discover` describes the
//! server. `2025-11-25` (N-1) agrees a version once, in `initialize`. A
//! server here serves both on one stdio process, choosing per request; a
//! client probes with `server/discover` and falls back to `initialize`, as
//! the 2026-07-28 stdio rules describe. Anything older is refused.
//!
//! This module only builds and reads messages. Pipes, processes and the
//! routing of responses to callers belong to the client and server modules.

use crate::error::{Result, VaernError};
use serde_json::{Map, Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The current revision, served statelessly.
pub const VERSION_N: &str = "2026-07-28";
/// The previous revision, agreed through `initialize`.
pub const VERSION_N1: &str = "2025-11-25";
/// Every revision the runtime accepts, newest first.
pub const SUPPORTED: [&str; 2] = [VERSION_N, VERSION_N1];

pub const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
pub const META_CLIENT: &str = "io.modelcontextprotocol/clientInfo";
pub const META_SERVER: &str = "io.modelcontextprotocol/serverInfo";
/// Vendor keys a vaern server adds to `_meta`, ignored by other clients:
/// its run id on the discover and initialize results, and its cumulative
/// spend on each `tools/call` result.
pub const META_RUN: &str = "dev.vaern/run";
pub const META_SPENT: &str = "dev.vaern/spent";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// The longest message read off a pipe: the response cap `fetch` uses,
/// since a message is held whole in memory.
pub const MAX_MESSAGE_BYTES: usize = crate::fetch::MAX_RESPONSE_BODY_BYTES;

/// Which revision's rules a connection follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Era {
    Modern,
    Legacy,
}

impl Era {
    pub fn version(self) -> &'static str {
        match self {
            Era::Modern => VERSION_N,
            Era::Legacy => VERSION_N1,
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> RpcError {
        RpcError {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn to_value(&self) -> Value {
        let mut error = json!({ "code": self.code, "message": self.message });
        if let Some(data) = &self.data {
            error["data"] = data.clone();
        }
        error
    }
}

/// One JSON-RPC message.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Result {
        id: Value,
        result: Value,
    },
    Error {
        id: Value,
        error: RpcError,
    },
}

impl Message {
    /// Reads one message. The error is the one to answer it with.
    pub fn parse(line: &[u8]) -> std::result::Result<Message, RpcError> {
        let value: Value = serde_json::from_slice(line)
            .map_err(|err| RpcError::new(PARSE_ERROR, format!("not JSON: {err}")))?;
        let Some(fields) = value.as_object() else {
            return Err(RpcError::new(
                INVALID_REQUEST,
                "a message must be an object",
            ));
        };
        if fields.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(RpcError::new(INVALID_REQUEST, "jsonrpc must be \"2.0\""));
        }
        let id = fields.get("id").cloned();
        let params = fields.get("params").cloned().unwrap_or(Value::Null);
        if let Some(method) = fields.get("method").and_then(Value::as_str) {
            let method = method.to_string();
            return Ok(match id {
                Some(id) => Message::Request { id, method, params },
                None => Message::Notification { method, params },
            });
        }
        let id = id.unwrap_or(Value::Null);
        if let Some(result) = fields.get("result") {
            return Ok(Message::Result {
                id,
                result: result.clone(),
            });
        }
        match fields.get("error") {
            Some(error) => Ok(Message::Error {
                id,
                error: RpcError {
                    code: error["code"].as_i64().unwrap_or(0),
                    message: error["message"].as_str().unwrap_or_default().to_string(),
                    data: error.get("data").cloned(),
                },
            }),
            None => Err(RpcError::new(
                INVALID_REQUEST,
                "a message needs method, result or error",
            )),
        }
    }

    /// The message as one line. JSON escapes every newline inside strings,
    /// so the only newline is the terminator the transport requires.
    pub fn to_line(&self) -> String {
        let value = match self {
            Message::Request { id, method, params } => {
                json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
            }
            Message::Notification { method, params } => {
                json!({ "jsonrpc": "2.0", "method": method, "params": params })
            }
            Message::Result { id, result } => {
                json!({ "jsonrpc": "2.0", "id": id, "result": result })
            }
            Message::Error { id, error } => {
                json!({ "jsonrpc": "2.0", "id": id, "error": error.to_value() })
            }
        };
        let mut line = value.to_string();
        line.push('\n');
        line
    }
}

/// Reads one line of at most `limit` bytes, without its line ending.
/// `Ok(None)` is the end of the stream.
pub async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    // Two more than the limit leaves room for a CRLF ending.
    let read = (&mut *reader)
        .take(limit as u64 + 2)
        .read_until(b'\n', &mut line)
        .await?;
    trim_line(line, read, limit)
}

/// The end of a line read: `read` bytes of `line`, at most `limit + 2` of
/// them, with the line ending removed. `Ok(None)` when nothing was read.
pub fn trim_line(mut line: Vec<u8>, read: usize, limit: usize) -> std::io::Result<Option<Vec<u8>>> {
    if read == 0 {
        return Ok(None);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    if line.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("a message is longer than {limit} bytes"),
        ));
    }
    Ok(Some(line))
}

/// Checks a modern request's `_meta`. The error is the response to send.
pub fn check_modern(params: &Value) -> std::result::Result<(), RpcError> {
    let meta = &params["_meta"];
    let Some(version) = meta[META_VERSION].as_str() else {
        return Err(RpcError::new(
            INVALID_PARAMS,
            format!("_meta[\"{META_VERSION}\"] is required"),
        ));
    };
    if version != VERSION_N {
        return Err(RpcError {
            code: UNSUPPORTED_PROTOCOL_VERSION,
            message: "Unsupported protocol version".to_string(),
            data: Some(json!({ "supported": SUPPORTED, "requested": version })),
        });
    }
    if !meta[META_CAPABILITIES].is_object() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            format!("_meta[\"{META_CAPABILITIES}\"] is required"),
        ));
    }
    Ok(())
}

/// The `server/discover` result. Lists are not cached: a restarted server
/// may run a changed script.
pub fn discover_result(server: &Value, instructions: Option<&str>) -> Value {
    let mut result = json!({
        "resultType": "complete",
        "supportedVersions": SUPPORTED,
        "capabilities": { "tools": {} },
        "ttlMs": 0,
        "cacheScope": "private",
        "_meta": { META_SERVER: server },
    });
    if let Some(text) = instructions {
        result["instructions"] = Value::from(text);
    }
    result
}

/// The `initialize` result. Whatever the client asked for, the legacy era
/// is `2025-11-25`; a client that cannot speak it disconnects.
pub fn initialize_result(server: &Value, instructions: Option<&str>) -> Value {
    let mut result = json!({
        "protocolVersion": VERSION_N1,
        "capabilities": { "tools": {} },
        "serverInfo": server,
    });
    if let Some(text) = instructions {
        result["instructions"] = Value::from(text);
    }
    result
}

/// The `_meta` a modern request carries.
pub fn request_meta(client: &Value) -> Value {
    json!({ META_VERSION: VERSION_N, META_CAPABILITIES: {}, META_CLIENT: client })
}

/// What the client and server agreed.
#[derive(Debug, Clone, PartialEq)]
pub struct Negotiated {
    pub era: Era,
    pub server: Value,
    pub instructions: Option<String>,
    /// The server's run id, when it is a vaern server keeping a journal.
    pub run: Option<String>,
}

fn negotiate_error(server: &str, code: &str, message: String) -> VaernError {
    VaernError::Mcp {
        stage: "negotiate".to_string(),
        code: code.to_string(),
        server: server.to_string(),
        message,
        action: "Serve the server with a vaern release that speaks 2026-07-28 or 2025-11-25."
            .to_string(),
    }
}

/// Waits for the response to request `id`, answering any request the
/// server sends meanwhile with method-not-found. `None` is a timeout.
async fn response<R, W>(
    reader: &mut R,
    writer: &mut W,
    id: u64,
    wait: Duration,
    server: &str,
) -> Result<Option<Message>>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let gone = |reason: String| VaernError::Mcp {
        stage: "exited".to_string(),
        code: "SERVER_EXITED".to_string(),
        server: server.to_string(),
        message: reason,
        action: "Run the server with `vaern mcp serve` on its own to see why it stopped."
            .to_string(),
    };
    let waited = tokio::time::timeout(wait, async {
        loop {
            let line = read_line(reader, MAX_MESSAGE_BYTES)
                .await
                .map_err(|err| gone(err.to_string()))?
                .ok_or_else(|| gone("the server closed its output while starting".into()))?;
            match Message::parse(&line) {
                Ok(Message::Result { id: got, .. } | Message::Error { id: got, .. })
                    if got.as_u64() != Some(id) => {}
                Ok(Message::Request { id: asked, .. }) => {
                    let refusal = Message::Error {
                        id: asked,
                        error: RpcError::new(METHOD_NOT_FOUND, "the client serves no requests"),
                    };
                    write(writer, &refusal, server).await?;
                }
                Ok(message @ (Message::Result { .. } | Message::Error { .. })) => {
                    return Ok(message);
                }
                Ok(Message::Notification { .. }) | Err(_) => {}
            }
        }
    })
    .await;
    waited.map_or(Ok(None), |message| message.map(Some))
}

pub async fn write<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Message,
    server: &str,
) -> Result<()> {
    writer
        .write_all(message.to_line().as_bytes())
        .await
        .and(writer.flush().await)
        .map_err(|err| VaernError::Mcp {
            stage: "exited".to_string(),
            code: "SERVER_EXITED".to_string(),
            server: server.to_string(),
            message: format!("the server's input is closed: {err}"),
            action: "Run the server with `vaern mcp serve` on its own to see why it stopped."
                .to_string(),
        })
}

/// Agrees a revision with a freshly started server. Request ids 1 and 2
/// are used; the caller numbers its own requests from 3.
pub async fn negotiate<R, W>(
    reader: &mut R,
    writer: &mut W,
    client: &Value,
    wait: Duration,
    server: &str,
) -> Result<Negotiated>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let probe = Message::Request {
        id: Value::from(1),
        method: "server/discover".to_string(),
        params: json!({ "_meta": request_meta(client) }),
    };
    write(writer, &probe, server).await?;
    match response(reader, writer, 1, wait, server).await? {
        Some(Message::Result { result, .. }) => {
            let versions = result["supportedVersions"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if versions.iter().any(|v| v == VERSION_N) {
                return Ok(Negotiated {
                    era: Era::Modern,
                    server: result["_meta"][META_SERVER].clone(),
                    instructions: result["instructions"].as_str().map(str::to_string),
                    run: result["_meta"][META_RUN].as_str().map(str::to_string),
                });
            }
            if !versions.iter().any(|v| v == VERSION_N1) {
                let offered = Value::Array(versions).to_string();
                return Err(negotiate_error(
                    server,
                    "UNSUPPORTED_VERSION",
                    format!("the server supports {offered}; this runtime speaks {SUPPORTED:?}"),
                ));
            }
        }
        Some(Message::Error { error, .. }) if error.code == UNSUPPORTED_PROTOCOL_VERSION => {
            let offered = error
                .data
                .as_ref()
                .map_or(Value::Null, |d| d["supported"].clone());
            let listed = offered.as_array().cloned().unwrap_or_default();
            if !listed.iter().any(|v| v == VERSION_N1) {
                return Err(negotiate_error(
                    server,
                    "UNSUPPORTED_VERSION",
                    format!("the server supports {offered}; this runtime speaks {SUPPORTED:?}"),
                ));
            }
        }
        // Any other answer, or none in time, is a server from before
        // 2026-07-28; the fallback must not depend on one error code.
        _ => {}
    }

    let initialize = Message::Request {
        id: Value::from(2),
        method: "initialize".to_string(),
        params: json!({ "protocolVersion": VERSION_N1, "capabilities": {}, "clientInfo": client }),
    };
    write(writer, &initialize, server).await?;
    let result = match response(reader, writer, 2, wait, server).await? {
        Some(Message::Result { result, .. }) => result,
        Some(Message::Error { error, .. }) => {
            return Err(negotiate_error(
                server,
                "INITIALIZE_FAILED",
                format!("initialize was refused: {} ({})", error.message, error.code),
            ));
        }
        _ => {
            return Err(negotiate_error(
                server,
                "TIMEOUT",
                format!("no answer to server/discover or initialize within {wait:?}"),
            ));
        }
    };
    let version = result["protocolVersion"].as_str().unwrap_or_default();
    if version != VERSION_N1 {
        return Err(negotiate_error(
            server,
            "UNSUPPORTED_VERSION",
            format!("the server offered '{version}'; this runtime speaks {SUPPORTED:?}"),
        ));
    }
    let initialized = Message::Notification {
        method: "notifications/initialized".to_string(),
        params: Value::Object(Map::new()),
    };
    write(writer, &initialized, server).await?;
    Ok(Negotiated {
        era: Era::Legacy,
        server: result["serverInfo"].clone(),
        instructions: result["instructions"].as_str().map(str::to_string),
        run: result["_meta"][META_RUN].as_str().map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    /// A fake server on an in-memory pipe: `answer` maps each request it
    /// reads to its reply, `None` for silence. Returns every message read.
    async fn against(
        answer: impl Fn(&str, &Value) -> Option<Value> + Send + 'static,
    ) -> (Result<Negotiated>, Vec<Message>) {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let (server_read, mut server_write) = tokio::io::split(server_end);
        let fake = tokio::spawn(async move {
            let mut reader = BufReader::new(server_read);
            let mut seen = Vec::new();
            while let Ok(Some(line)) = read_line(&mut reader, 1 << 20).await {
                let message = Message::parse(&line).unwrap();
                if let Message::Request { id, method, params } = &message
                    && let Some(mut reply) = answer(method, params)
                {
                    reply["jsonrpc"] = "2.0".into();
                    reply["id"] = id.clone();
                    let line = format!(
                        "{reply}
"
                    );
                    server_write.write_all(line.as_bytes()).await.unwrap();
                }
                seen.push(message);
            }
            seen
        });
        let (client_read, mut client_write) = tokio::io::split(client_end);
        let mut reader = BufReader::new(client_read);
        let client = json!({ "name": "vaern", "version": "0" });
        let wait = Duration::from_millis(300);
        let outcome = negotiate(&mut reader, &mut client_write, &client, wait, "fake").await;
        drop(client_write);
        drop(reader);
        (outcome, fake.await.unwrap())
    }

    fn methods(seen: &[Message]) -> Vec<String> {
        seen.iter()
            .filter_map(|m| match m {
                Message::Request { method, .. } | Message::Notification { method, .. } => {
                    Some(method.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn code_of(outcome: Result<Negotiated>) -> String {
        match outcome {
            Err(VaernError::Mcp { code, .. }) => code,
            other => panic!("expected an MCP error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_a_modern_server_is_spoken_to_statelessly() {
        let (outcome, seen) = against(|method, _| {
            (method == "server/discover").then(|| {
                json!({ "result": discover_result(&json!({ "name": "calc", "version": "1" }), None) })
            })
        })
        .await;
        let agreed = outcome.unwrap();
        assert_eq!(
            (agreed.era, agreed.server["name"].as_str()),
            (Era::Modern, Some("calc"))
        );
        assert_eq!(methods(&seen), ["server/discover"]);
        let Message::Request { params, .. } = &seen[0] else {
            panic!()
        };
        assert_eq!(params["_meta"][META_VERSION], VERSION_N);
    }

    #[tokio::test]
    async fn test_a_2025_11_25_server_is_initialized_after_the_probe_fails() {
        let (outcome, seen) = against(|method, params| match method {
            "server/discover" => Some(json!({ "error": { "code": -32601, "message": "no" } })),
            "initialize" => {
                assert_eq!(params["protocolVersion"], VERSION_N1);
                Some(json!({ "result": initialize_result(&json!({ "name": "old", "version": "1" }), None) }))
            }
            _ => None,
        })
        .await;
        assert_eq!(outcome.unwrap().era, Era::Legacy);
        assert_eq!(
            methods(&seen),
            ["server/discover", "initialize", "notifications/initialized"]
        );
    }

    #[tokio::test]
    async fn test_a_silent_probe_falls_back_to_initialize() {
        let (outcome, _) = against(|method, _| {
            (method == "initialize").then(|| {
                json!({ "result": initialize_result(&json!({ "name": "old", "version": "1" }), None) })
            })
        })
        .await;
        assert_eq!(outcome.unwrap().era, Era::Legacy);
    }

    #[tokio::test]
    async fn test_older_revisions_are_refused_with_a_typed_error() {
        let (outcome, _) = against(|method, _| match method {
            "server/discover" => Some(json!({ "error": { "code": -32602, "message": "bad" } })),
            "initialize" => Some(json!({ "result": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "serverInfo": { "name": "older", "version": "1" } } })),
            _ => None,
        })
        .await;
        assert_eq!(code_of(outcome), "UNSUPPORTED_VERSION");

        // A modern server that lists neither revision is not initialized.
        let (outcome, seen) = against(|_, _| {
            Some(
                json!({ "error": { "code": UNSUPPORTED_PROTOCOL_VERSION, "message": "no",
                "data": { "supported": ["2027-01-01"], "requested": VERSION_N } } }),
            )
        })
        .await;
        assert_eq!(code_of(outcome), "UNSUPPORTED_VERSION");
        assert_eq!(methods(&seen), ["server/discover"]);
    }

    #[test]
    fn test_a_server_checks_each_modern_request_for_its_version() {
        let meta =
            |version: &str| json!({ "_meta": { META_VERSION: version, META_CAPABILITIES: {} } });
        assert!(check_modern(&meta(VERSION_N)).is_ok());
        let refused = check_modern(&meta("2024-11-05")).unwrap_err();
        assert_eq!(refused.code, UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(
            refused.data.unwrap(),
            json!({ "supported": SUPPORTED, "requested": "2024-11-05" })
        );
        assert_eq!(check_modern(&json!({})).unwrap_err().code, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn test_messages_are_one_line_each_and_long_lines_are_refused() {
        let message = Message::Result {
            id: 1.into(),
            result: json!({ "text": "a\nb" }),
        };
        let line = message.to_line();
        assert_eq!(line.matches('\n').count(), 1);
        assert_eq!(Message::parse(line.trim_end().as_bytes()).unwrap(), message);
        assert_eq!(Message::parse(b"{").unwrap_err().code, PARSE_ERROR);
        assert_eq!(
            Message::parse(br#"{"id":1}"#).unwrap_err().code,
            INVALID_REQUEST
        );

        let mut short = BufReader::new(&b"0123456789\r\nabc"[..]);
        assert_eq!(
            read_line(&mut short, 10).await.unwrap().unwrap(),
            b"0123456789"
        );
        assert_eq!(read_line(&mut short, 10).await.unwrap().unwrap(), b"abc");
        assert!(read_line(&mut short, 10).await.unwrap().is_none());
        let mut long = BufReader::new(&b"01234567890\n"[..]);
        assert!(read_line(&mut long, 10).await.is_err());
    }
}
