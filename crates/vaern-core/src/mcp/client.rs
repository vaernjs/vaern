//! The client half of `vaern:mcp`: starting a server, agreeing a revision
//! with it, and carrying calls and results over its pipes.
//!
//! In v0 a script can start only servers that are scripts run by this same
//! binary under `vaern mcp serve`, so each server carries its own journal,
//! home guard and budget cap. A connection is one Tokio task that owns the
//! child process and its pipes. The JavaScript thread sends it commands and
//! hears back through a callback, so nothing here touches the engine.

use super::protocol::{self, Era, MAX_MESSAGE_BYTES, META_SPENT, Message};
use crate::budget::{Amount, Limits};
use crate::error::{Result, VaernError};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncReadExt, AsyncWrite, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc::UnboundedReceiver;

/// How long a server has to answer `server/discover` or `initialize`.
const NEGOTIATE_WAIT: Duration = Duration::from_secs(10);
/// How long a server has to exit once its input is closed.
pub const EXIT_GRACE: Duration = Duration::from_secs(2);
/// How much of a server's stderr is kept for its errors.
const STDERR_TAIL: usize = 4096;
/// The request id of `tools/list`; negotiation uses 1 and 2.
const LIST_ID: u64 = 3;

/// What to start: this binary, serving `script`.
pub struct Launch {
    pub program: PathBuf,
    pub script: PathBuf,
    /// The caller's home, or `None` when the caller keeps no journal.
    pub home: Option<PathBuf>,
    /// The most the server may spend, passed as its `--budget-*` caps.
    pub grant: Limits,
}

/// What a connection learned while starting.
#[derive(Debug)]
pub struct Hello {
    pub era: Era,
    pub server: Value,
    pub instructions: Option<String>,
    pub tools: Vec<Value>,
    /// The server's own run id, when it keeps a journal.
    pub run: Option<String>,
}

/// A failed start or call, with what the server left behind.
#[derive(Debug)]
pub struct Failure {
    pub error: VaernError,
    /// The JSON-RPC error code when the server refused the request.
    pub rpc: Option<i64>,
    pub exit_code: Option<i32>,
    pub stderr: String,
    /// Whether the request reached the server, so it may have run.
    pub sent: bool,
}

impl Failure {
    fn new(error: VaernError, sent: bool) -> Failure {
        Failure {
            error,
            rpc: None,
            exit_code: None,
            stderr: String::new(),
            sent,
        }
    }
}

/// How a connection ended.
#[derive(Debug, Default, PartialEq)]
pub struct Closed {
    pub exit_code: Option<i32>,
    /// The last spend the server reported; `None` if it never did.
    pub spent: Option<Amount>,
}

/// Work for a connection's task.
pub enum Command {
    Call {
        req_id: u32,
        tool: String,
        args: Value,
    },
    Close,
}

/// What a connection reports, in order: `Connected`, any `Called`, then
/// `Closed` once (also after a failed start).
#[derive(Debug)]
pub enum McpEvent {
    Connected {
        conn_id: u32,
        result: std::result::Result<Hello, Failure>,
    },
    /// The result of `tools/call`: the server's `CallToolResult`.
    Called {
        req_id: u32,
        result: std::result::Result<Value, Failure>,
    },
    Closed {
        conn_id: u32,
        closed: Closed,
    },
}

fn mcp_error(stage: &str, code: &str, server: &str, message: String, action: &str) -> VaernError {
    VaernError::Mcp {
        stage: stage.into(),
        code: code.into(),
        server: server.into(),
        message,
        action: action.into(),
    }
}

/// The last [`STDERR_TAIL`] bytes a server wrote to stderr.
#[derive(Clone, Default)]
struct Tail(Arc<Mutex<VecDeque<u8>>>);

impl Tail {
    fn push(&self, bytes: &[u8]) {
        if let Ok(mut kept) = self.0.lock() {
            kept.extend(bytes);
            let over = kept.len().saturating_sub(STDERR_TAIL);
            kept.drain(..over);
        }
    }

    fn text(&self) -> String {
        match self.0.lock() {
            Ok(kept) => String::from_utf8_lossy(&kept.iter().copied().collect::<Vec<_>>()).into(),
            Err(_) => String::new(),
        }
    }
}

/// The amount a server reported in a result's `_meta`.
pub fn spent_of(result: &Value) -> Option<Amount> {
    let spent = result["_meta"].get(META_SPENT)?;
    Some(Amount {
        nano: spent["nano_usd"].as_u64()?,
        tokens: spent["tokens"].as_u64()?,
    })
}

/// A cap as `--budget-usd` reads it: nano-USD written as exact decimal USD.
fn usd_arg(nano: u64) -> String {
    format!("{}.{:09}", nano / 1_000_000_000, nano % 1_000_000_000)
}

/// A request's params in era `era`: a modern request names its version.
fn params(era: Era, client: &Value, mut params: Value) -> Value {
    if era == Era::Modern {
        params["_meta"] = protocol::request_meta(client);
    }
    params
}

fn exited(server: &str, message: String) -> VaernError {
    mcp_error(
        "exited",
        "SERVER_EXITED",
        server,
        message,
        "Its stderr tail is attached; run it with `vaern mcp serve` on its own to see more.",
    )
}

/// Runs connection `conn_id` until it is closed, its commands end or the
/// server exits, reporting through `send`. The child never outlives it:
/// it is killed if the grace period runs out, or when this task drops.
pub async fn run(
    launch: Launch,
    conn_id: u32,
    send: impl Fn(McpEvent),
    commands: UnboundedReceiver<Command>,
) {
    let server = launch.script.display().to_string();
    let tail = Tail::default();
    let failed = |error: VaernError, exit_code: Option<i32>| Failure {
        exit_code,
        stderr: tail.text(),
        ..Failure::new(error, false)
    };
    let refused = |failure: Failure| {
        let closed = Closed {
            exit_code: failure.exit_code,
            spent: None,
        };
        send(McpEvent::Connected {
            conn_id,
            result: Err(failure),
        });
        send(McpEvent::Closed { conn_id, closed });
    };
    let mut process = tokio::process::Command::new(&launch.program);
    process.arg("mcp").arg("serve");
    match &launch.home {
        Some(home) => process.env("VAERN_HOME", home),
        None => process.arg("--no-journal"),
    };
    process
        .arg("--budget-usd")
        .arg(usd_arg(launch.grant.nano.unwrap_or(0)))
        .arg("--budget-tokens")
        .arg(launch.grant.tokens.unwrap_or(0).to_string())
        .arg(&launch.script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(err) => {
            let message = format!("{} could not be started: {err}", launch.program.display());
            let action = "Check that the vaern binary is still in place and can be run.";
            refused(failed(
                mcp_error("spawn", "SPAWN_FAILED", &server, message, action),
                None,
            ));
            return;
        }
    };
    let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let message = "the server's pipes were not opened".to_string();
        let action = "Retry; this is a fault in the runtime if it repeats.";
        refused(failed(
            mcp_error("spawn", "SPAWN_FAILED", &server, message, action),
            None,
        ));
        return;
    };
    let keep = tail.clone();
    tokio::spawn(async move {
        let mut chunk = [0u8; 1024];
        while let Ok(read @ 1..) = stderr.read(&mut chunk).await {
            keep.push(&chunk[..read]);
        }
    });
    let mut stdout = BufReader::new(stdout);
    let client = json!({ "name": "vaern", "version": env!("CARGO_PKG_VERSION") });
    let started = async {
        let agreed =
            protocol::negotiate(&mut stdout, &mut stdin, &client, NEGOTIATE_WAIT, &server).await?;
        let listed = list_tools(&mut stdout, &mut stdin, agreed.era, &client, &server).await?;
        Ok::<_, VaernError>((agreed, listed))
    };
    let (agreed, listed) = match started.await {
        Ok(started) => started,
        Err(error) => {
            let exit_code = shut(&mut child, stdin).await;
            refused(failed(error, exit_code));
            return;
        }
    };
    let era = agreed.era;
    let hello = Hello {
        era,
        server: agreed.server,
        instructions: agreed.instructions,
        tools: listed["tools"].as_array().cloned().unwrap_or_default(),
        run: agreed.run,
    };
    send(McpEvent::Connected {
        conn_id,
        result: Ok(hello),
    });
    let ended = serve_calls(
        &mut stdout,
        &mut stdin,
        era,
        &client,
        &server,
        commands,
        &send,
    )
    .await;
    let exit_code = shut(&mut child, stdin).await;
    // A call still in flight learns why the server went away, and is never
    // sent again: a tool need not be safe to run twice.
    for req_id in ended.in_flight {
        let message = match (ended.exited, exit_code) {
            (true, Some(code)) => format!("the server exited with code {code} during the call"),
            (true, None) => "the server stopped during the call".to_string(),
            (false, _) => "the connection was closed during the call".to_string(),
        };
        let failure = Failure {
            sent: true,
            ..failed(exited(&server, message), exit_code)
        };
        send(McpEvent::Called {
            req_id,
            result: Err(failure),
        });
    }
    let closed = Closed {
        exit_code,
        spent: ended.spent,
    };
    send(McpEvent::Closed { conn_id, closed });
}

/// `tools/list`, the one request made while starting.
async fn list_tools<R, W>(
    reader: &mut R,
    writer: &mut W,
    era: Era,
    client: &Value,
    server: &str,
) -> Result<Value>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let message = Message::Request {
        id: LIST_ID.into(),
        method: "tools/list".into(),
        params: params(era, client, json!({})),
    };
    protocol::write(writer, &message, server).await?;
    loop {
        let line = protocol::read_line(reader, MAX_MESSAGE_BYTES)
            .await
            .ok()
            .flatten()
            .ok_or_else(|| {
                exited(
                    server,
                    "the server closed its output while listing its tools".into(),
                )
            })?;
        match Message::parse(&line) {
            Ok(Message::Result { id, result }) if id == LIST_ID => return Ok(result),
            Ok(Message::Error { id, error }) if id == LIST_ID => {
                let message = format!("tools/list was refused: {} ({})", error.message, error.code);
                let action = "Check the server script's server() declaration.";
                return Err(mcp_error(
                    "negotiate",
                    "TOOLS_LIST_FAILED",
                    server,
                    message,
                    action,
                ));
            }
            _ => {}
        }
    }
}

/// How [`serve_calls`] ended.
struct Ended {
    /// The server's output ended without a close being asked for.
    exited: bool,
    in_flight: Vec<u32>,
    spent: Option<Amount>,
}

/// Sends each call and routes each answer back by JSON-RPC id, until a
/// close is asked for or the server's output ends.
async fn serve_calls<R, W>(
    reader: &mut R,
    writer: &mut W,
    era: Era,
    client: &Value,
    server: &str,
    mut commands: UnboundedReceiver<Command>,
    send: &impl Fn(McpEvent),
) -> Ended
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut in_flight: HashMap<u64, u32> = HashMap::new();
    let mut next_id = LIST_ID + 1;
    let mut spent = None;
    let ended = |exited: bool, in_flight: HashMap<u64, u32>, spent: Option<Amount>| Ended {
        exited,
        in_flight: in_flight.into_values().collect(),
        spent,
    };
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Call { req_id, tool, args }) => {
                    let id = next_id;
                    next_id += 1;
                    let call = params(era, client, json!({ "name": tool, "arguments": args }));
                    let message = Message::Request { id: id.into(), method: "tools/call".into(), params: call };
                    match protocol::write(writer, &message, server).await {
                        Ok(()) => {
                            in_flight.insert(id, req_id);
                        }
                        Err(error) => send(McpEvent::Called { req_id, result: Err(Failure::new(error, false)) }),
                    }
                }
                Some(Command::Close) | None => return ended(false, in_flight, spent),
            },
            line = protocol::read_line(reader, MAX_MESSAGE_BYTES) => {
                let Ok(Some(line)) = line else {
                    return ended(true, in_flight, spent);
                };
                let (id, answer) = match Message::parse(&line) {
                    Ok(Message::Result { id, result }) => (id, Ok(result)),
                    Ok(Message::Error { id, error }) => (id, Err(error)),
                    _ => continue,
                };
                let Some(req_id) = id.as_u64().and_then(|id| in_flight.remove(&id)) else {
                    continue;
                };
                let result = match answer {
                    Ok(result) => {
                        // The spend is cumulative and answers can arrive out
                        // of order, so the largest seen is the latest.
                        if let Some(now) = spent_of(&result) {
                            spent = Some(spent.map_or(now, |was: Amount| Amount {
                                nano: was.nano.max(now.nano),
                                tokens: was.tokens.max(now.tokens),
                            }));
                        }
                        Ok(result)
                    }
                    Err(error) => {
                        let message = format!("the server refused the call: {} ({})", error.message, error.code);
                        let action = "Check the tool's name and arguments against connection.tools().";
                        let refusal = mcp_error("call", "REFUSED", server, message, action);
                        Err(Failure { rpc: Some(error.code), ..Failure::new(refusal, true) })
                    }
                };
                send(McpEvent::Called { req_id, result });
            }
        }
    }
}

/// Closes the server's input, gives it [`EXIT_GRACE`] to exit, then kills
/// it. Returns its exit code, when it had one.
async fn shut(child: &mut Child, stdin: ChildStdin) -> Option<i32> {
    drop(stdin);
    match tokio::time::timeout(EXIT_GRACE, child.wait()).await {
        Ok(Ok(status)) => status.code(),
        _ => {
            let _ = child.kill().await;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use tokio::io::AsyncWriteExt;

    fn client() -> Value {
        json!({ "name": "vaern", "version": "0" })
    }

    /// Runs `serve_calls` against a fake server that answers each request
    /// with `answer(id, params)`, or not at all for `None`. The fake closes
    /// its output after `lines` answers. Returns the events and how it ended.
    async fn against(
        calls: Vec<Command>,
        lines: usize,
        answer: impl Fn(u64, &Value) -> Option<Value> + Send + 'static,
    ) -> (Vec<McpEvent>, Ended, Vec<Value>) {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let (server_read, mut server_write) = tokio::io::split(server_end);
        let fake = tokio::spawn(async move {
            let mut reader = BufReader::new(server_read);
            let mut asked = Vec::new();
            let mut answers = Vec::new();
            while answers.len() < lines {
                let Ok(Some(line)) = protocol::read_line(&mut reader, 1 << 20).await else {
                    break;
                };
                let Ok(Message::Request { id, params, .. }) = Message::parse(&line) else {
                    continue;
                };
                asked.push(params.clone());
                if let Some(mut reply) = answer(id.as_u64().unwrap_or(0), &params) {
                    reply["jsonrpc"] = "2.0".into();
                    reply["id"] = id;
                    answers.push(format!("{reply}\n"));
                }
                // Answers go out in reverse once all are in, so routing by
                // id is what puts them right.
                if answers.len() == lines {
                    for line in answers.iter().rev() {
                        server_write.write_all(line.as_bytes()).await.unwrap();
                    }
                }
            }
            asked
        });
        let (client_read, mut client_write) = tokio::io::split(client_end);
        let mut reader = BufReader::new(client_read);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for call in calls {
            tx.send(call).unwrap();
        }
        let (events_tx, events_rx) = mpsc::channel();
        let send = move |event: McpEvent| events_tx.send(event).unwrap();
        let ended = serve_calls(
            &mut reader,
            &mut client_write,
            Era::Modern,
            &client(),
            "fake",
            rx,
            &send,
        )
        .await;
        drop(tx);
        drop(client_write);
        let asked = fake.await.unwrap();
        (events_rx.try_iter().collect(), ended, asked)
    }

    fn call(req_id: u32, tool: &str) -> Command {
        Command::Call {
            req_id,
            tool: tool.into(),
            args: json!({ "n": req_id }),
        }
    }

    #[tokio::test]
    async fn test_answers_reach_their_calls_by_id_and_the_spend_is_kept() {
        let (events, ended, asked) = against(vec![call(7, "a"), call(8, "b")], 2, |id, params| {
            Some(
                json!({ "result": { "content": [], "n": params["arguments"]["n"],
                "_meta": { META_SPENT: { "nano_usd": id * 10, "tokens": id } } } }),
            )
        })
        .await;
        let mut got: Vec<(u32, Value)> = events
            .into_iter()
            .map(|event| match event {
                McpEvent::Called {
                    req_id,
                    result: Ok(result),
                } => (req_id, result["n"].clone()),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        got.sort_by_key(|(req_id, _)| *req_id);
        assert_eq!(got, [(7, json!(7)), (8, json!(8))]);
        // Every modern request names the revision.
        assert!(
            asked
                .iter()
                .all(|p| p["_meta"][protocol::META_VERSION] == protocol::VERSION_N)
        );
        assert!(ended.exited && ended.in_flight.is_empty());
        // Id 4's smaller spend is read last; the larger one stands.
        assert_eq!(
            ended.spent,
            Some(Amount {
                nano: 50,
                tokens: 5
            })
        );
    }

    #[tokio::test]
    async fn test_a_refusal_is_typed_and_a_lost_server_leaves_its_calls_in_flight() {
        let (events, ended, _) = against(vec![call(1, "x"), call(2, "y")], 1, |_, params| {
            (params["name"] == "x").then(
                || json!({ "error": { "code": protocol::INVALID_PARAMS, "message": "bad n" } }),
            )
        })
        .await;
        match &events[..] {
            [
                McpEvent::Called {
                    req_id: 1,
                    result: Err(failure),
                },
            ] => {
                assert_eq!(failure.rpc, Some(protocol::INVALID_PARAMS));
                assert!(failure.sent);
                assert!(matches!(&failure.error, VaernError::Mcp { stage, code, .. }
                    if stage == "call" && code == "REFUSED"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(ended.exited);
        assert_eq!(ended.in_flight, [2]);
    }

    #[test]
    fn test_caps_are_written_as_exact_usd_and_the_stderr_tail_is_bounded() {
        assert_eq!(usd_arg(0), "0.000000000");
        assert_eq!(usd_arg(1_250_000_001), "1.250000001");
        let tail = Tail::default();
        tail.push(&vec![b'a'; STDERR_TAIL]);
        tail.push(b"end");
        let text = tail.text();
        assert_eq!(text.len(), STDERR_TAIL);
        assert!(text.ends_with("aend"));
    }

    #[tokio::test]
    async fn test_a_server_that_cannot_be_started_fails_at_spawn_and_closes() {
        let launch = Launch {
            program: PathBuf::from("no-such-vaern-binary"),
            script: PathBuf::from("calc.ts"),
            home: None,
            grant: Limits::default(),
        };
        let (events_tx, events_rx) = mpsc::channel();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        run(launch, 5, move |event| events_tx.send(event).unwrap(), rx).await;
        let events: Vec<McpEvent> = events_rx.try_iter().collect();
        match &events[..] {
            [
                McpEvent::Connected {
                    conn_id: 5,
                    result: Err(failure),
                },
                McpEvent::Closed { conn_id: 5, closed },
            ] => {
                assert!(matches!(&failure.error, VaernError::Mcp { stage, code, .. }
                    if stage == "spawn" && code == "SPAWN_FAILED"));
                assert!(!failure.sent);
                assert_eq!(*closed, Closed::default());
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
