//! The server half of `vaern:mcp`: what `vaern mcp serve` does with each
//! line a client writes to its stdin.
//!
//! The script declares one server with `server()`; from then on the runtime
//! answers `server/discover`, `initialize` and `tools/list` itself and hands
//! `tools/call` to the declared `tool()`, whose run is journalled and metered
//! as any tool call is. Replies go to stdout, which in this mode carries the
//! protocol only: the runtime sends `console.log` to stderr instead.
//!
//! The era is chosen per request, as the 2026-07-28 rules allow a server to:
//! a request naming its version in `_meta` is served statelessly, and an
//! `initialize` switches the process to 2025-11-25 for requests that do not.

use super::protocol::{
    self, Era, INVALID_PARAMS, MAX_MESSAGE_BYTES, META_VERSION, METHOD_NOT_FOUND, Message, RpcError,
};
use crate::event::{EventSender, LoopEvent};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Read, Write};

/// What the runtime is doing about MCP serving.
pub enum Host {
    /// A plain `vaern run`: `server()` is refused.
    Off,
    /// `vaern mcp serve`, before the script has called `server()`.
    Waiting,
    Serving(Served),
}

/// What a line of input asks the runtime to do.
#[derive(Debug, PartialEq)]
pub enum Step {
    Reply(Message),
    /// Run tool `name` with the JSON `args`; the script answers through
    /// [`Served::finish`] with the same `req_id`.
    Call {
        req_id: u32,
        name: String,
        args: String,
    },
    Nothing,
}

struct Pending {
    id: Value,
    era: Era,
}

/// A declared server and the requests it is answering.
pub struct Served {
    info: Value,
    instructions: Option<String>,
    tools: Vec<Value>,
    legacy: bool,
    calls: HashMap<u32, Pending>,
    next_call: u32,
}

impl Served {
    /// Builds a server from the declaration `server()` passes on: `name`,
    /// `version`, `instructions` and the tools' definitions.
    pub fn declare(declaration: &Value) -> Served {
        Served {
            info: json!({ "name": declaration["name"], "version": declaration["version"] }),
            instructions: declaration["instructions"].as_str().map(str::to_string),
            tools: declaration["tools"].as_array().cloned().unwrap_or_default(),
            legacy: false,
            calls: HashMap::new(),
            next_call: 1,
        }
    }

    pub fn handle(&mut self, line: &[u8]) -> Step {
        if line.iter().all(u8::is_ascii_whitespace) {
            return Step::Nothing;
        }
        let (id, method, params) = match Message::parse(line) {
            Ok(Message::Request { id, method, params }) => (id, method, params),
            Ok(Message::Notification { method, params }) => {
                if method == "notifications/cancelled" {
                    let id = &params["requestId"];
                    self.calls.retain(|_, pending| pending.id != *id);
                }
                return Step::Nothing;
            }
            // The server sends no requests, so it expects no answers.
            Ok(Message::Result { .. } | Message::Error { .. }) => return Step::Nothing,
            Err(error) => {
                return Step::Reply(Message::Error {
                    id: Value::Null,
                    error,
                });
            }
        };
        let fail = |error: RpcError| {
            Step::Reply(Message::Error {
                id: id.clone(),
                error,
            })
        };
        if method == "initialize" {
            self.legacy = true;
            let result = protocol::initialize_result(&self.info, self.instructions.as_deref());
            return Step::Reply(Message::Result { id, result });
        }
        let era = if self.legacy && params["_meta"].get(META_VERSION).is_none() {
            Era::Legacy
        } else if let Err(error) = protocol::check_modern(&params) {
            return fail(error);
        } else {
            Era::Modern
        };
        let result = match method.as_str() {
            "server/discover" => {
                protocol::discover_result(&self.info, self.instructions.as_deref())
            }
            "tools/list" => match era {
                Era::Modern => json!({ "resultType": "complete", "tools": self.tools,
                                       "ttlMs": 0, "cacheScope": "private" }),
                Era::Legacy => json!({ "tools": self.tools }),
            },
            "ping" if era == Era::Legacy => json!({}),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                if !self.tools.iter().any(|tool| tool["name"] == name) {
                    return fail(RpcError::new(
                        INVALID_PARAMS,
                        format!("Unknown tool: {name}"),
                    ));
                }
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                if !args.is_object() {
                    return fail(RpcError::new(INVALID_PARAMS, "arguments must be an object"));
                }
                let req_id = self.next_call;
                self.next_call += 1;
                self.calls.insert(req_id, Pending { id, era });
                return Step::Call {
                    req_id,
                    name: name.to_string(),
                    args: args.to_string(),
                };
            }
            other => {
                return fail(RpcError::new(
                    METHOD_NOT_FOUND,
                    format!("this server provides tools only; '{other}' is not served"),
                ));
            }
        };
        Step::Reply(Message::Result { id, result })
    }

    /// The reply to call `req_id` from the script's outcome: `{ ok: true,
    /// value }`, or `{ ok: false, kind: "args" | "tool", message }`. `None`
    /// when the client cancelled the request meanwhile.
    pub fn finish(&mut self, req_id: u32, outcome: &Value) -> Option<Message> {
        let Pending { id, era } = self.calls.remove(&req_id)?;
        if outcome["ok"] != true {
            let message = outcome["message"].as_str().unwrap_or("the tool failed");
            if outcome["kind"] == "args" {
                let error = RpcError::new(INVALID_PARAMS, message);
                return Some(Message::Error { id, error });
            }
            let mut result = json!({ "content": [{ "type": "text", "text": message }],
                                     "isError": true });
            if era == Era::Modern {
                result["resultType"] = "complete".into();
            }
            return Some(Message::Result { id, result });
        }
        let value = &outcome["value"];
        let text = match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let mut result = json!({ "content": [{ "type": "text", "text": text }], "isError": false });
        // 2025-11-25 allows only an object here; 2026-07-28 any value.
        let structured = match era {
            Era::Modern => !value.is_string(),
            Era::Legacy => value.is_object(),
        };
        if structured {
            result["structuredContent"] = value.clone();
        }
        if era == Era::Modern {
            result["resultType"] = "complete".into();
        }
        Some(Message::Result { id, result })
    }
}

/// Writes one message to stdout. A closed stdout means the client has gone,
/// and there is no one left to tell.
pub fn emit(message: &Message) {
    let mut out = std::io::stdout().lock();
    let _ = out
        .write_all(message.to_line().as_bytes())
        .and_then(|()| out.flush());
}

/// Reads stdin on a thread of its own and sends each line to the event
/// loop, then `McpInput(None)` at the end. A plain thread, not a blocking
/// task, so that exiting the process never waits on a read.
pub fn read_input(tx: EventSender) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("vaern-mcp-stdin".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            loop {
                let mut line = Vec::new();
                let read = (&mut input)
                    .take(MAX_MESSAGE_BYTES as u64 + 2)
                    .read_until(b'\n', &mut line);
                match read.and_then(|count| protocol::trim_line(line, count, MAX_MESSAGE_BYTES)) {
                    Ok(Some(line)) => {
                        if tx.send(LoopEvent::McpInput(Some(line))).is_err() {
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        eprintln!("vaern mcp: stopped reading requests: {err}");
                        break;
                    }
                }
            }
            let _ = tx.send(LoopEvent::McpInput(None));
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::protocol::{META_CAPABILITIES, UNSUPPORTED_PROTOCOL_VERSION, VERSION_N};

    fn calc() -> Served {
        Served::declare(&json!({ "name": "calc", "version": "1.0.0", "tools": [
            { "name": "add", "description": "", "inputSchema": { "type": "object" } }] }))
    }

    fn modern(id: u64, method: &str, mut params: Value) -> Vec<u8> {
        params["_meta"] = json!({ META_VERSION: VERSION_N, META_CAPABILITIES: {} });
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
            .to_string()
            .into_bytes()
    }

    fn legacy(id: u64, method: &str, params: Value) -> Vec<u8> {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
            .to_string()
            .into_bytes()
    }

    fn result(step: Step) -> Value {
        match step {
            Step::Reply(Message::Result { result, .. }) => result,
            other => panic!("expected a result, got {other:?}"),
        }
    }

    fn error_code(step: Step) -> i64 {
        match step {
            Step::Reply(Message::Error { error, .. }) => error.code,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn test_a_modern_client_lists_and_calls_without_a_handshake() {
        let mut served = calc();
        let discovered = result(served.handle(&modern(1, "server/discover", json!({}))));
        assert_eq!(discovered["_meta"][protocol::META_SERVER]["name"], "calc");
        let listed = result(served.handle(&modern(2, "tools/list", json!({}))));
        assert_eq!(
            (
                listed["tools"][0]["name"].as_str(),
                listed["cacheScope"].as_str()
            ),
            (Some("add"), Some("private"))
        );
        let call = served.handle(&modern(
            3,
            "tools/call",
            json!({ "name": "add", "arguments": { "a": 1 } }),
        ));
        let Step::Call { req_id, name, args } = call else {
            panic!("{call:?}")
        };
        assert_eq!((name.as_str(), args.as_str()), ("add", r#"{"a":1}"#));
        let reply = served
            .finish(req_id, &json!({ "ok": true, "value": [3] }))
            .unwrap();
        let Message::Result { id, result } = reply else {
            panic!()
        };
        assert_eq!(id, 3);
        assert_eq!(result["structuredContent"], json!([3]));
        assert_eq!(result["content"][0]["text"], "[3]");
        assert_eq!(result["resultType"], "complete");
    }

    #[test]
    fn test_a_2025_11_25_client_initializes_once_and_then_sends_plain_requests() {
        let mut served = calc();
        // Before initialize, a request without a version is refused.
        assert_eq!(
            error_code(served.handle(&legacy(1, "tools/list", json!({})))),
            INVALID_PARAMS
        );
        let agreed = result(served.handle(&legacy(
            2,
            "initialize",
            json!({ "protocolVersion": "2025-11-25" }),
        )));
        assert_eq!(agreed["protocolVersion"], "2025-11-25");
        assert!(
            result(served.handle(&legacy(3, "tools/list", json!({}))))
                .get("ttlMs")
                .is_none()
        );
        let Step::Call { req_id, .. } = served.handle(&legacy(
            4,
            "tools/call",
            json!({ "name": "add", "arguments": {} }),
        )) else {
            panic!()
        };
        let Some(Message::Result { result, .. }) =
            served.finish(req_id, &json!({ "ok": true, "value": [3] }))
        else {
            panic!()
        };
        // An array is not structured content before 2026-07-28.
        assert!(result.get("structuredContent").is_none() && result.get("resultType").is_none());
    }

    #[test]
    fn test_older_versions_unknown_tools_and_cancelled_calls() {
        let mut served = calc();
        let mut old = modern(1, "tools/list", json!({}));
        old = String::from_utf8(old)
            .unwrap()
            .replace(VERSION_N, "2024-11-05")
            .into_bytes();
        assert_eq!(
            error_code(served.handle(&old)),
            UNSUPPORTED_PROTOCOL_VERSION
        );
        let unknown = modern(2, "tools/call", json!({ "name": "nosuch" }));
        assert_eq!(error_code(served.handle(&unknown)), INVALID_PARAMS);
        assert_eq!(
            error_code(served.handle(b"{not json")),
            protocol::PARSE_ERROR
        );
        assert_eq!(
            error_code(served.handle(&modern(3, "prompts/list", json!({})))),
            METHOD_NOT_FOUND
        );

        let Step::Call { req_id, .. } =
            served.handle(&modern(4, "tools/call", json!({ "name": "add" })))
        else {
            panic!()
        };
        let cancel = json!({ "jsonrpc": "2.0", "method": "notifications/cancelled",
                             "params": { "requestId": 4 } });
        assert_eq!(served.handle(cancel.to_string().as_bytes()), Step::Nothing);
        assert!(
            served
                .finish(req_id, &json!({ "ok": true, "value": 1 }))
                .is_none()
        );

        let Step::Call { req_id, .. } =
            served.handle(&modern(5, "tools/call", json!({ "name": "add" })))
        else {
            panic!()
        };
        let failed = served.finish(
            req_id,
            &json!({ "ok": false, "kind": "tool", "message": "boom" }),
        );
        let Some(Message::Result { result, .. }) = failed else {
            panic!()
        };
        assert_eq!(
            (
                result["isError"].as_bool(),
                result["content"][0]["text"].as_str()
            ),
            (Some(true), Some("boom"))
        );
    }
}
