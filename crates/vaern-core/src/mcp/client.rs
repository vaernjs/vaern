//! The client half of `vaern:mcp`: starting a server, agreeing a revision
//! with it, and carrying calls and results over its pipes.
//!
//! In v0 a script can start only servers that are scripts run by this same
//! binary under `vaern mcp serve`, so each server carries its own journal,
//! home guard and budget cap. Each start of a server is one Tokio task that
//! owns the child process and its pipes. The JavaScript thread sends it
//! commands and hears back through a callback, so nothing here touches the
//! engine.
//!
//! A call that is not answered within the connection's timeout, or that the
//! script aborts, is given up here: the server is sent
//! `notifications/cancelled` for it and the call fails. The server may still
//! have run it, so it counts as sent.
//!
//! The whole start, from spawning the server through negotiation to its
//! `tools/list`, runs under one deadline: the connection's timeout, never
//! less than [`START_FLOOR`]. A server that stops answering at any step
//! fails the start there and is killed, rather than leaving `connect()`
//! pending on a process that will never reply.

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
use tokio::time::Instant;

/// How long a server has to answer `server/discover` or `initialize`, each.
/// The start deadline can cut either short.
const NEGOTIATE_WAIT: Duration = Duration::from_secs(10);
/// The least time a start is given, so a short call timeout does not fail
/// a healthy server that is slow to boot. Starts take tens of milliseconds.
pub const START_FLOOR: Duration = Duration::from_secs(5);
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
    /// How long each call may wait for its answer, and how long the start
    /// may take in all (at least [`START_FLOOR`]).
    pub call_timeout: Duration,
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
    /// What the server had spent once its script's top level had run.
    pub spent: Option<Amount>,
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
#[derive(Debug, Default, Clone, PartialEq)]
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
    /// Stops waiting on call `req_id`, which the script aborted.
    Cancel {
        req_id: u32,
    },
    Close,
}

/// What a server's task reports, in order: `Connected`, any `Called`, then
/// `Closed` once (also after a failed start). `start` is the request id of
/// the `connect()` or restart that started this server.
#[derive(Debug)]
pub enum McpEvent {
    Connected {
        start: u32,
        result: std::result::Result<Hello, Failure>,
    },
    /// The result of `tools/call`: the server's `CallToolResult`.
    Called {
        req_id: u32,
        result: std::result::Result<Value, Failure>,
    },
    /// The server has gone. Calls it never answered come with it, so the
    /// connection is seen as gone in the same step that fails them.
    Closed {
        start: u32,
        closed: Closed,
        unanswered: Vec<(u32, Failure)>,
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

/// A step of the start that can run out of time.
#[derive(Clone, Copy)]
enum StartStep {
    Negotiate,
    ToolsList,
}

impl StartStep {
    fn name(self) -> &'static str {
        match self {
            StartStep::Negotiate => "negotiate",
            StartStep::ToolsList => "tools_list",
        }
    }

    fn awaited(self) -> &'static str {
        match self {
            StartStep::Negotiate => "server/discover or initialize",
            StartStep::ToolsList => "tools/list",
        }
    }
}

fn start_timeout(server: &str, step: StartStep, waited: Duration) -> VaernError {
    let message = format!(
        "the server did not finish starting within {} ms: step {} got no answer to {}",
        waited.as_millis(),
        step.name(),
        step.awaited()
    );
    let action =
        "Raise timeoutMs in connect(), or make the server script finish its top level sooner.";
    mcp_error("timeout", "TIMEOUT", server, message, action)
}

/// Whether negotiation gave up on its own, both probes unanswered, before
/// the start deadline came.
fn probes_unanswered(error: &VaernError) -> bool {
    matches!(error, VaernError::Mcp { stage, code, .. } if stage == "negotiate" && code == "TIMEOUT")
}

/// Runs the server started by request `start` until it is closed, its
/// commands end or it exits, reporting through `send`. The child never
/// outlives it: it is killed if the grace period runs out, or when this
/// task drops.
pub async fn run(
    launch: Launch,
    start: u32,
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
            start,
            result: Err(failure),
        });
        let unanswered = Vec::new();
        send(McpEvent::Closed {
            start,
            closed,
            unanswered,
        });
    };
    let start_wait = launch.call_timeout.max(START_FLOOR);
    let deadline = Instant::now() + start_wait;
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
    // Each step runs to the same deadline. A step that ran out of time ends
    // with the server killed (`true`); any other failure lets it exit on its
    // own, so its exit code and stderr say why.
    let timed_out = |step| (start_timeout(&server, step, start_wait), true);
    let outcome = async {
        let negotiated =
            protocol::negotiate(&mut stdout, &mut stdin, &client, NEGOTIATE_WAIT, &server);
        let agreed = match tokio::time::timeout_at(deadline, negotiated).await {
            Ok(Ok(agreed)) => agreed,
            Ok(Err(error)) if probes_unanswered(&error) => {
                let error = start_timeout(&server, StartStep::Negotiate, NEGOTIATE_WAIT * 2);
                return Err((error, true));
            }
            Ok(Err(error)) => return Err((error, false)),
            Err(_) => return Err(timed_out(StartStep::Negotiate)),
        };
        let listed = list_tools(&mut stdout, &mut stdin, agreed.era, &client, &server);
        match tokio::time::timeout_at(deadline, listed).await {
            Ok(Ok(listed)) => Ok((agreed, listed)),
            Ok(Err(error)) => Err((error, false)),
            Err(_) => Err(timed_out(StartStep::ToolsList)),
        }
    }
    .await;
    let (agreed, listed) = match outcome {
        Ok(started) => started,
        Err((error, hung)) => {
            let exit_code = match hung {
                true => stop(&mut child).await,
                false => shut(&mut child, stdin).await,
            };
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
        spent: spent_of(&listed),
    };
    let spent = hello.spent;
    send(McpEvent::Connected {
        start,
        result: Ok(hello),
    });
    let peer = Peer {
        era,
        client: &client,
        server: &server,
        timeout: launch.call_timeout,
    };
    let ended = serve_calls(&mut stdout, &mut stdin, &peer, spent, commands, &send).await;
    let exit_code = shut(&mut child, stdin).await;
    // A call still in flight learns why the server went away, and is never
    // sent again: a tool need not be safe to run twice.
    let mut unanswered = Vec::new();
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
        unanswered.push((req_id, failure));
    }
    let closed = Closed {
        exit_code,
        spent: ended.spent,
    };
    send(McpEvent::Closed {
        start,
        closed,
        unanswered,
    });
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

/// Who a connection talks to, and in which era.
struct Peer<'a> {
    era: Era,
    client: &'a Value,
    server: &'a str,
    timeout: Duration,
}

/// Tells the server to stop request `id`. Its answer, if one still comes,
/// is dropped because the id is no longer in flight.
async fn cancel<W: AsyncWrite + Unpin>(writer: &mut W, peer: &Peer<'_>, id: u64, reason: &str) {
    let params = json!({ "requestId": id, "reason": reason });
    let note = Message::Notification {
        method: "notifications/cancelled".into(),
        params: self::params(peer.era, peer.client, params),
    };
    // A server that cannot be written to has gone; its exit is reported
    // when its output ends.
    let _ = protocol::write(writer, &note, peer.server).await;
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
    peer: &Peer<'_>,
    mut spent: Option<Amount>,
    mut commands: UnboundedReceiver<Command>,
    send: &impl Fn(McpEvent),
) -> Ended
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut in_flight: HashMap<u64, (u32, Instant)> = HashMap::new();
    let mut next_id = LIST_ID + 1;
    let ended = |exited: bool, in_flight: HashMap<u64, (u32, Instant)>, spent| Ended {
        exited,
        in_flight: in_flight.into_values().map(|(req_id, _)| req_id).collect(),
        spent,
    };
    loop {
        let due = in_flight.values().map(|(_, due)| *due).min();
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Call { req_id, tool, args }) => {
                    let id = next_id;
                    next_id += 1;
                    let call = params(peer.era, peer.client, json!({ "name": tool, "arguments": args }));
                    let message = Message::Request { id: id.into(), method: "tools/call".into(), params: call };
                    match protocol::write(writer, &message, peer.server).await {
                        Ok(()) => {
                            in_flight.insert(id, (req_id, Instant::now() + peer.timeout));
                        }
                        Err(error) => send(McpEvent::Called { req_id, result: Err(Failure::new(error, false)) }),
                    }
                }
                Some(Command::Cancel { req_id }) => {
                    let found = in_flight.iter().find(|(_, (call, _))| *call == req_id).map(|(id, _)| *id);
                    // Already answered: the answer stands.
                    let Some(id) = found else { continue };
                    in_flight.remove(&id);
                    cancel(writer, peer, id, crate::ai::ABORTED).await;
                    let message = format!("the call was {}", crate::ai::ABORTED);
                    let error = mcp_error("call", "ABORTED", peer.server, message, crate::ai::ABORTED_ACTION);
                    send(McpEvent::Called { req_id, result: Err(Failure::new(error, true)) });
                }
                Some(Command::Close) | None => return ended(false, in_flight, spent),
            },
            () = tokio::time::sleep_until(due.unwrap_or_else(Instant::now)), if due.is_some() => {
                let now = Instant::now();
                let late: Vec<u64> = in_flight.iter().filter(|(_, (_, due))| *due <= now).map(|(id, _)| *id).collect();
                for id in late {
                    let Some((req_id, _)) = in_flight.remove(&id) else { continue };
                    let waited = peer.timeout.as_millis();
                    cancel(writer, peer, id, &format!("no answer within {waited} ms")).await;
                    let message = format!("no answer within {waited} ms; the server was told to stop the call");
                    let action = "Raise timeoutMs in connect(), or make the tool answer sooner.";
                    let error = mcp_error("timeout", "TIMEOUT", peer.server, message, action);
                    send(McpEvent::Called { req_id, result: Err(Failure::new(error, true)) });
                }
            }
            line = protocol::read_line(reader, MAX_MESSAGE_BYTES) => {
                let Ok(Some(line)) = line else {
                    return ended(true, in_flight, spent);
                };
                let (id, answer) = match Message::parse(&line) {
                    Ok(Message::Result { id, result }) => (id, Ok(result)),
                    Ok(Message::Error { id, error }) => (id, Err(error)),
                    _ => continue,
                };
                let Some((req_id, _)) = id.as_u64().and_then(|id| in_flight.remove(&id)) else {
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
                        let refusal = mcp_error("call", "REFUSED", peer.server, message, action);
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

/// Kills a server that stopped answering while starting. Closing its input
/// first would only add [`EXIT_GRACE`]: a script stuck in a loop never reads
/// it. A killed server has no exit code worth reporting.
async fn stop(child: &mut Child) -> Option<i32> {
    let _ = child.kill().await;
    None
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
        let client = client();
        let peer = Peer {
            era: Era::Modern,
            client: &client,
            server: "fake",
            timeout: Duration::from_secs(30),
        };
        let ended = serve_calls(&mut reader, &mut client_write, &peer, None, rx, &send).await;
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

    #[tokio::test]
    async fn test_a_late_or_aborted_call_fails_and_the_server_is_told_to_stop_it() {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        // The fake server reads everything and answers nothing.
        let fake = tokio::spawn(async move {
            let mut reader = BufReader::new(server_end);
            let mut heard = Vec::new();
            while let Ok(Some(line)) = protocol::read_line(&mut reader, 1 << 20).await {
                if let Ok(message) = Message::parse(&line) {
                    heard.push(message);
                }
            }
            heard
        });
        let (client_read, mut client_write) = tokio::io::split(client_end);
        let mut reader = BufReader::new(client_read);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for command in [
            call(1, "slow"),
            call(2, "stopped"),
            Command::Cancel { req_id: 2 },
        ] {
            tx.send(command).unwrap();
        }
        let close = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let _ = close.send(Command::Close);
        });
        let (events_tx, events_rx) = mpsc::channel();
        let send = move |event: McpEvent| events_tx.send(event).unwrap();
        let client = client();
        let peer = Peer {
            era: Era::Legacy,
            client: &client,
            server: "fake",
            timeout: Duration::from_millis(50),
        };
        let ended = serve_calls(&mut reader, &mut client_write, &peer, None, rx, &send).await;
        // The fake reads until both halves of its pipe are gone.
        drop((tx, client_write, reader));
        let failed: Vec<(u32, String, bool)> = events_rx
            .try_iter()
            .map(|event| match event {
                McpEvent::Called {
                    req_id,
                    result: Err(failure),
                } => (req_id, format!("{}", failure.error), failure.sent),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(failed.len(), 2, "{failed:?}");
        assert_eq!(failed[0].0, 2);
        assert!(failed[0].1.contains(crate::ai::ABORTED), "{}", failed[0].1);
        assert_eq!(failed[1].0, 1);
        assert!(
            failed[1].1.contains("no answer within 50 ms"),
            "{}",
            failed[1].1
        );
        assert!(failed.iter().all(|(_, _, sent)| *sent));
        assert!(!ended.exited && ended.in_flight.is_empty());
        let cancelled: Vec<Value> = fake
            .await
            .unwrap()
            .into_iter()
            .filter_map(|message| match message {
                Message::Notification { method, params } if method == "notifications/cancelled" => {
                    Some(params["requestId"].clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(cancelled, [json!(5), json!(4)]);
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
            call_timeout: Duration::from_secs(1),
        };
        let (events_tx, events_rx) = mpsc::channel();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        run(launch, 5, move |event| events_tx.send(event).unwrap(), rx).await;
        let events: Vec<McpEvent> = events_rx.try_iter().collect();
        match &events[..] {
            [
                McpEvent::Connected {
                    start: 5,
                    result: Err(failure),
                },
                McpEvent::Closed {
                    start: 5, closed, ..
                },
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
