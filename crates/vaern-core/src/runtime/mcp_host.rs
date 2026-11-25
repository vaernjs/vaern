//! The runtime's side of `vaern:mcp` connections: the bridges behind
//! `connect()`, a handle's `call()` and `close()`, and what is done with
//! each event a connection reports.
//!
//! Each of the three is an effect, as a fetch or a model call is. It is
//! reserved in the budget before anything is sent, journalled as
//! `mcp.connect`, `mcp.call` or `mcp.close`, and settled when it completes.
//! A connection's budget is its grant: the server's caps, reserved in this
//! run when it starts and settled at what the server reports it spent.
//! When the server is lost, its spend is unknown and the grant is kept.
//!
//! A server that exits without being closed is recorded as `mcp.exit`, and
//! the connection's next call starts it again, at most [`RESTART_LIMIT`]
//! times. Each start reserves its own grant and has its own `mcp.connect`
//! record. A call in flight when the server went away is never sent again.
//!
//! A replay starts no server. Each effect is matched against its record and
//! its recorded outcome is handed back, so a connection has no channel. A
//! recorded exit is handed back where the run saw it, so a replayed call
//! restarts the server exactly where the run did.

use super::{BudgetSlot, JournalSlot, QuickJsRuntime, ReplaySlot, SharedState, track_effect};
use crate::budget::{Amount, Limits};
use crate::effect_record::Outcome;
use crate::error::{Result, VaernError};
use crate::event::LoopEvent;
use crate::mcp::client::{self, Closed, Command, Failure, Launch, McpEvent};
use rquickjs::{Ctx, Function, Object};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// Request ids for the closes made at the end of a run, above any the
/// script's counter reaches.
const AUTO_CLOSE_IDS: u32 = 1 << 31;
/// How many times a connection's server is started again after it exits.
const RESTART_LIMIT: u32 = 3;

/// One connection, from `connect()` until it is closed.
struct Conn {
    /// `None` in a replay, where no server runs, and while the server is
    /// lost.
    commands: Option<UnboundedSender<Command>>,
    /// The server script's path, which names the server in errors.
    server: String,
    /// What a restart starts again.
    path: PathBuf,
    grant: Amount,
    timeout_ms: u64,
    /// The request id of the `connect()` or restart that started the
    /// current server. It keys that start's grant and its record.
    start: u32,
    /// The current server has started and its grant is still reserved.
    opened: bool,
    /// How the server ended when it went away without being closed.
    lost: Option<Closed>,
    restarts: u32,
    run: Option<String>,
    /// The request id of the close in progress.
    closing: Option<u32>,
}

/// The run's connections, shared with the bridges.
#[derive(Default)]
pub(super) struct McpClients {
    conns: HashMap<u32, Conn>,
    /// Each server start's connection.
    starts: HashMap<u32, u32>,
    /// Each call in flight's connection.
    calls: HashMap<u32, u32>,
    /// Connects, restarts, calls and closes in flight; they keep the loop
    /// alive.
    pub(super) pending: usize,
    /// The binary to start servers with; this one unless a test sets it.
    pub(super) program: Option<PathBuf>,
    next_auto_close: u32,
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

/// An MCP error as the script and the journal see it.
fn error_info(error: &VaernError) -> Value {
    match error {
        VaernError::Mcp {
            stage,
            code,
            server,
            message,
            action,
        } => json!({ "stage": stage, "code": code, "server": server,
                     "message": message, "action": action }),
        other => json!({ "stage": "journal", "code": "JOURNAL", "server": "",
                         "message": other.to_string(), "action": "Check the journal directory." }),
    }
}

fn failure_info(failure: &Failure) -> Value {
    let mut info = error_info(&failure.error);
    info["sent"] = failure.sent.into();
    if let Some(code) = failure.exit_code {
        info["exit_code"] = code.into();
    }
    if !failure.stderr.is_empty() {
        info["stderr"] = failure.stderr.clone().into();
    }
    if let Some(rpc) = failure.rpc {
        info["rpc"] = rpc.into();
    }
    info
}

/// The script or module that called into the bridge: the first frame up
/// the stack that is not one of the runtime's own preludes.
fn caller_module(ctx: &Ctx<'_>) -> Option<String> {
    (0..64)
        .filter_map(|level| ctx.script_or_module_name(level))
        .filter_map(|atom| atom.to_string().ok())
        .find(|name| name != "eval_script")
}

fn refused(error: &VaernError) -> Option<String> {
    Some(error_info(error).to_string())
}

/// The `mcp.connect` request for starting `path`: what it runs and with
/// what, and the digest a replay checks the script against.
fn start_request(
    path: &Path,
    server: &str,
    grant: Amount,
    timeout_ms: u64,
) -> std::result::Result<Value, VaernError> {
    let Ok(source) = std::fs::read(path) else {
        let message = format!("no server script at {server}");
        let action = "Give connect() a path relative to the calling module.";
        return Err(mcp_error(
            "config",
            "SCRIPT_NOT_FOUND",
            server,
            message,
            action,
        ));
    };
    Ok(
        json!({ "script": server, "budget": { "nano_usd": grant.nano, "tokens": grant.tokens },
               "timeout_ms": timeout_ms, "script_sha256": crate::journal::sha256_hex(&source) }),
    )
}

/// What the bridges share.
#[derive(Clone)]
struct Bridges {
    state: Rc<RefCell<SharedState>>,
    journal: JournalSlot,
    budget: BudgetSlot,
    replay: ReplaySlot,
    clients: Rc<RefCell<McpClients>>,
    homes: Rc<RefCell<[Option<PathBuf>; 2]>>,
    tokio: tokio::runtime::Handle,
}

impl Bridges {
    /// Reserves `amount` for effect `kind` of `req_id`, or returns why not.
    fn reserve(&self, kind: &'static str, req_id: u32, amount: Amount) -> Option<String> {
        let reserved = self.budget.borrow_mut().reserve(
            self.journal.borrow_mut().as_mut(),
            kind,
            req_id,
            amount,
        );
        reserved
            .err()
            .map(|err| super::model_error_info(err).to_string())
    }

    /// Gives back a reserve whose effect never started.
    fn release(&self, kind: &'static str, req_id: u32) {
        let mut journal = self.journal.borrow_mut();
        let budget = &mut self.budget.borrow_mut();
        // Nothing was spent, so there is nothing a failure here could lose.
        let _ = budget.settle(journal.as_mut(), kind, req_id, Some(Amount::default()));
    }

    /// Starts effect `kind` of request `req_id`: tracked for its journal
    /// record, or, in a replay, matched against the recorded one, whose
    /// `eid` is returned.
    fn begin(&self, kind: &'static str, req_id: u32, req: Value) -> Option<u64> {
        if let Some(replay) = self.replay.borrow_mut().as_mut() {
            return Some(replay.dispatch(kind, req_id, &req, None));
        }
        if self.journal.borrow().is_some() {
            track_effect(&self.state, kind, req_id, req);
        }
        None
    }

    fn replaying(&self) -> bool {
        self.replay.borrow().is_some()
    }

    /// Starts the server for request `start`. Returns the channel its task
    /// reads commands from; `None` in a replay.
    fn launch(
        &self,
        start: u32,
        path: &Path,
        grant: Amount,
        timeout_ms: u64,
    ) -> std::result::Result<Option<UnboundedSender<Command>>, VaernError> {
        if self.replaying() {
            return Ok(None);
        }
        let program = self.clients.borrow().program.clone();
        let Some(program) = program.or_else(|| std::env::current_exe().ok()) else {
            let message = "the path of the vaern binary is unknown".to_string();
            let server = path.display().to_string();
            return Err(mcp_error("spawn", "SPAWN_FAILED", &server, message, ""));
        };
        let launch = Launch {
            program,
            script: path.to_path_buf(),
            home: self.homes.borrow()[0].clone(),
            grant: Limits {
                nano: Some(grant.nano),
                tokens: Some(grant.tokens),
            },
            call_timeout: Duration::from_millis(timeout_ms),
        };
        let (commands, rx) = tokio::sync::mpsc::unbounded_channel();
        let events = self.state.borrow().events_tx.clone();
        let send = move |event| {
            let _ = events.send(LoopEvent::Mcp(Box::new(event)));
        };
        self.tokio.spawn(client::run(launch, start, send, rx));
        Ok(Some(commands))
    }

    /// Starts connection `conn_id`'s server again for call `req_id`, which
    /// then waits on the new server. Returns the error that stopped it.
    fn restart(&self, conn_id: u32, req_id: u32) -> Option<String> {
        let (path, server, grant, timeout_ms, restarts, exit_code) = {
            let clients = self.clients.borrow();
            let conn = clients.conns.get(&conn_id)?;
            let exit_code = conn.lost.as_ref().and_then(|lost| lost.exit_code);
            let (path, server) = (conn.path.clone(), conn.server.clone());
            (
                path,
                server,
                conn.grant,
                conn.timeout_ms,
                conn.restarts,
                exit_code,
            )
        };
        if restarts >= RESTART_LIMIT {
            let message = format!(
                "the server has exited and was already started again {RESTART_LIMIT} times"
            );
            let action = "Find why it exits from the stderr tail on the earlier errors, then connect() again.";
            let mut info = error_info(&mcp_error(
                "exited",
                "RESTART_LIMIT",
                &server,
                message,
                action,
            ));
            if let Some(code) = exit_code {
                info["exit_code"] = code.into();
            }
            return Some(info.to_string());
        }
        let mut req = match start_request(&path, &server, grant, timeout_ms) {
            Ok(req) => req,
            Err(error) => return refused(&error),
        };
        req["conn"] = conn_id.into();
        req["restart"] = (restarts + 1).into();
        if let Some(refusal) = self.reserve("mcp.connect", req_id, grant) {
            return Some(refusal);
        }
        let commands = match self.launch(req_id, &path, grant, timeout_ms) {
            Ok(commands) => commands,
            Err(error) => {
                self.release("mcp.connect", req_id);
                return refused(&error);
            }
        };
        self.begin("mcp.connect", req_id, req);
        let mut clients = self.clients.borrow_mut();
        clients.starts.insert(req_id, conn_id);
        clients.pending += 1;
        if let Some(conn) = clients.conns.get_mut(&conn_id) {
            conn.commands = commands;
            conn.start = req_id;
            conn.opened = false;
            conn.lost = None;
            conn.restarts += 1;
        }
        None
    }

    /// `__vaern_mcp_connect`: reserves the grant and starts the server.
    fn connect(
        &self,
        ctx: &Ctx<'_>,
        req_id: u32,
        script: &str,
        grant: Amount,
        timeout_ms: u64,
    ) -> Option<String> {
        let base = caller_module(ctx)
            .and_then(|name| Path::new(&name).parent().map(Path::to_path_buf))
            .unwrap_or_default();
        let path = base.join(script);
        let server = path.display().to_string();
        let req = match start_request(&path, &server, grant, timeout_ms) {
            Ok(req) => req,
            Err(error) => return refused(&error),
        };
        if let Some(refusal) = self.reserve("mcp.connect", req_id, grant) {
            return Some(refusal);
        }
        let commands = match self.launch(req_id, &path, grant, timeout_ms) {
            Ok(commands) => commands,
            Err(error) => {
                self.release("mcp.connect", req_id);
                return refused(&error);
            }
        };
        self.begin("mcp.connect", req_id, req);
        let conn = Conn {
            commands,
            server,
            path,
            grant,
            timeout_ms,
            start: req_id,
            opened: false,
            lost: None,
            restarts: 0,
            run: None,
            closing: None,
        };
        let mut clients = self.clients.borrow_mut();
        clients.conns.insert(req_id, conn);
        clients.starts.insert(req_id, req_id);
        clients.pending += 1;
        None
    }

    /// `__vaern_mcp_call_tool`: restarts a lost server, reserves the call's
    /// cost and sends it.
    fn call(
        &self,
        req_id: u32,
        conn_id: u32,
        tool: String,
        args: &str,
        cost_nano: f64,
        abort_id: u32,
    ) -> Option<String> {
        let lost = {
            let clients = self.clients.borrow();
            let Some(conn) = clients.conns.get(&conn_id).filter(|c| c.closing.is_none()) else {
                let message = "the connection is closed".to_string();
                let action = "Connect again with connect().";
                return refused(&mcp_error("exited", "CLOSED", "", message, action));
            };
            conn.lost.is_some()
        };
        if lost && let Some(refusal) = self.restart(conn_id, req_id) {
            return Some(refusal);
        }
        // As with tool(), a call without a declared cost reserves nothing,
        // so it leaves no budget records.
        if cost_nano >= 1.0 {
            let cost = Amount {
                nano: cost_nano as u64,
                tokens: 0,
            };
            if let Some(refusal) = self.reserve("mcp.call", req_id, cost) {
                return Some(refusal);
            }
        }
        let args: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
        let (req, sent) = {
            let clients = self.clients.borrow();
            let conn = clients.conns.get(&conn_id)?;
            let req = json!({ "server": conn.server, "tool": tool, "args": args });
            let sent = match &conn.commands {
                Some(commands) => commands.send(Command::Call { req_id, tool, args }).is_ok(),
                None => true,
            };
            (req, sent)
        };
        if !sent {
            if cost_nano >= 1.0 {
                self.release("mcp.call", req_id);
            }
            let message = "the server has stopped".to_string();
            let server = req["server"].as_str().unwrap_or_default();
            return refused(&mcp_error("exited", "CLOSED", server, message, ""));
        }
        let eid = self.begin("mcp.call", req_id, req);
        if let (Some(eid), Some(replay)) = (eid, self.replay.borrow_mut().as_mut()) {
            replay.watch_abort(eid, abort_id);
        }
        let mut clients = self.clients.borrow_mut();
        clients.calls.insert(req_id, conn_id);
        clients.pending += 1;
        None
    }

    /// `__vaern_mcp_close`: asks the server to exit. A lost server has
    /// already exited, so its close is answered with how it ended.
    fn close(&self, req_id: u32, conn_id: u32) -> Option<String> {
        let mut clients = self.clients.borrow_mut();
        let Some(conn) = clients
            .conns
            .get_mut(&conn_id)
            .filter(|c| c.closing.is_none())
        else {
            return Some(json!({ "done": true }).to_string());
        };
        let live = self.replay.borrow().is_none();
        let req = begin_close(&self.state, live, conn, conn_id, req_id, None);
        drop(clients);
        self.begin("mcp.close", req_id, req);
        self.clients.borrow_mut().pending += 1;
        None
    }

    /// `__vaern_mcp_cancel`: stops waiting on call `req_id`.
    fn cancel(&self, req_id: u32) {
        let clients = self.clients.borrow();
        let conn = clients
            .calls
            .get(&req_id)
            .and_then(|conn_id| clients.conns.get(conn_id));
        if let Some(commands) = conn.and_then(|conn| conn.commands.as_ref()) {
            let _ = commands.send(Command::Cancel { req_id });
        }
    }
}

/// Marks `conn` as closing by request `close_id` and asks its server to
/// exit; a lost server's end is reported at once. A replay hands the close
/// back from its record, so `live` is false there and nothing is sent.
/// Returns the close's record request.
fn begin_close(
    state: &Rc<RefCell<SharedState>>,
    live: bool,
    conn: &mut Conn,
    conn_id: u32,
    close_id: u32,
    at: Option<&str>,
) -> Value {
    conn.closing = Some(close_id);
    if live && let Some(lost) = &conn.lost {
        let event = McpEvent::Closed {
            start: conn.start,
            closed: lost.clone(),
            unanswered: Vec::new(),
        };
        let _ = state
            .borrow()
            .events_tx
            .send(LoopEvent::Mcp(Box::new(event)));
    } else if let Some(commands) = conn.commands.as_ref().filter(|_| live) {
        let _ = commands.send(Command::Close);
    }
    let mut req = json!({ "server": conn.server, "conn": conn_id });
    if conn.start != conn_id {
        req["start"] = conn.start.into();
    }
    if let Some(at) = at {
        req["at"] = at.into();
    }
    req
}

impl QuickJsRuntime {
    /// Registers the `vaern:mcp` client bridges. `connect`, `call_tool` and
    /// `close` return `null` once their effect is under way, or the JSON of
    /// the error that stopped it before it began.
    pub(super) fn register_mcp_client<'js>(
        &self,
        ctx: &Ctx<'js>,
        globals: &Object<'js>,
        tokio_handle: &tokio::runtime::Handle,
    ) -> Result<()> {
        let bridges = Bridges {
            state: self.state.clone(),
            journal: self.journal.clone(),
            budget: self.budget.clone(),
            replay: self.replay.clone(),
            clients: self.mcp_clients.clone(),
            homes: self.reserved_homes.clone(),
            tokio: tokio_handle.clone(),
        };
        let b = bridges.clone();
        let connect_fn = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  req_id: u32,
                  script: String,
                  nano: f64,
                  tokens: f64,
                  timeout_ms: f64| {
                let grant = Amount {
                    nano: nano.max(0.0) as u64,
                    tokens: tokens.max(0.0) as u64,
                };
                b.connect(&ctx, req_id, &script, grant, timeout_ms.max(1.0) as u64)
            },
        );
        let b = bridges.clone();
        let call_fn = Function::new(
            ctx.clone(),
            move |req_id: u32,
                  conn_id: u32,
                  tool: String,
                  args: String,
                  cost_nano: f64,
                  abort_id: u32| {
                b.call(req_id, conn_id, tool, &args, cost_nano, abort_id)
            },
        );
        let b = bridges.clone();
        let close_fn = Function::new(ctx.clone(), move |req_id: u32, conn_id: u32| {
            b.close(req_id, conn_id)
        });
        let b = bridges.clone();
        let cancel_fn = Function::new(ctx.clone(), move |req_id: u32| b.cancel(req_id));
        let b = bridges;
        let restarts_fn = Function::new(ctx.clone(), move |conn_id: u32| {
            b.clients
                .borrow()
                .conns
                .get(&conn_id)
                .map_or(0, |conn| conn.restarts)
        });
        for (name, bridge) in [
            ("__vaern_mcp_connect", connect_fn),
            ("__vaern_mcp_call_tool", call_fn),
            ("__vaern_mcp_close", close_fn),
            ("__vaern_mcp_cancel", cancel_fn),
            ("__vaern_mcp_restarts", restarts_fn),
        ] {
            let registered = bridge.and_then(|bridge| globals.set(name, bridge));
            registered.map_err(|err| VaernError::EngineInit {
                reason: format!("failed to register {name}: {err}"),
            })?;
        }
        Ok(())
    }

    fn settle_mcp(
        &self,
        kind: &'static str,
        req_id: u32,
        charge: Option<Amount>,
    ) -> Option<String> {
        let mut journal = self.journal.borrow_mut();
        let settled = self
            .budget
            .borrow_mut()
            .settle(journal.as_mut(), kind, req_id, charge);
        settled.err().map(|err| err.to_string())
    }

    /// Hands `info` to the script's waiter for `info.reqId`, recording
    /// `outcome` first; a record that cannot be written fails the request.
    fn finish_mcp(&self, kind: &'static str, req_id: u32, outcome: Outcome, mut info: Value) {
        if let Some(err) = self.record_effect(kind, req_id, outcome) {
            info = error_info(&err);
            info["ok"] = false.into();
        }
        info["reqId"] = req_id.into();
        let delivered = self.context.with(|ctx| {
            ctx.globals()
                .get::<_, Function>("__vaern_dispatch_mcp")
                .and_then(|dispatch| dispatch.call::<_, ()>((info.to_string(),)))
                .is_ok()
        });
        if !delivered {
            let unsettled = self.context.with(|ctx| {
                Self::reject_undelivered(&ctx, "__vaern_reject_mcp", "vaern:mcp request", req_id)
            });
            self.note_unsettled(unsettled);
        }
    }

    /// Records an effect the script is not waiting on. A record that cannot
    /// be written ends the run, since the journal no longer holds it.
    fn record_mcp(&self, kind: &'static str, req_id: u32, outcome: Outcome) {
        let failed = self.record_effect(kind, req_id, outcome);
        self.note_unsettled(failed.map(|err| err.to_string()));
    }

    pub(super) fn dispatch_mcp_event(&self, event: McpEvent) {
        match event {
            McpEvent::Connected { start, result } => self.on_connected(start, result),
            McpEvent::Called { req_id, result } => self.on_called(req_id, result),
            McpEvent::Closed {
                start,
                closed,
                unanswered,
            } => {
                for (req_id, failure) in unanswered {
                    self.on_called(req_id, Err(failure));
                }
                self.on_closed(start, closed);
            }
        }
    }

    fn on_connected(&self, start: u32, result: std::result::Result<client::Hello, Failure>) {
        let conn_id = {
            let mut clients = self.mcp_clients.borrow_mut();
            clients.pending -= 1;
            clients.starts.get(&start).copied()
        };
        let Some(conn_id) = conn_id else { return };
        // A restart has no waiter: the call that started it waits on its own
        // answer.
        let restart = start != conn_id;
        let (outcome, mut info) = match result {
            Ok(hello) => {
                if let Some(conn) = self.mcp_clients.borrow_mut().conns.get_mut(&conn_id) {
                    conn.opened = true;
                    conn.run.clone_from(&hello.run);
                }
                let res = json!({ "protocol_version": hello.era.version(), "server": hello.server,
                                  "instructions": hello.instructions, "tools": hello.tools,
                                  "server_run": hello.run });
                let info = json!({ "ok": true, "protocolVersion": hello.era.version(),
                                   "server": hello.server, "tools": hello.tools,
                                   "instructions": hello.instructions });
                (Outcome::Response(res), info)
            }
            Err(failure) => {
                // A server that never started cannot have spent; one that
                // started ran its script's top level, which may have.
                let spawned = !matches!(&failure.error,
                    VaernError::Mcp { stage, .. } if stage == "spawn");
                let charge = (!spawned).then(Amount::default);
                let error = failure_info(&failure);
                let mut info = error.clone();
                if let Some(reason) = self.settle_mcp("mcp.connect", start, charge) {
                    info["message"] = reason.into();
                }
                info["ok"] = false.into();
                // A connection whose first start failed was never handed to
                // the script; one whose restart failed is lost again.
                let mut clients = self.mcp_clients.borrow_mut();
                if restart {
                    if let Some(conn) = clients.conns.get_mut(&conn_id) {
                        conn.commands = None;
                        conn.lost = Some(Closed {
                            exit_code: failure.exit_code,
                            spent: None,
                        });
                    }
                } else {
                    clients.conns.remove(&conn_id);
                }
                (Outcome::Error(error), info)
            }
        };
        if restart {
            self.record_mcp("mcp.connect", start, outcome);
            return;
        }
        info["connId"] = conn_id.into();
        self.finish_mcp("mcp.connect", start, outcome, info);
    }

    fn on_called(&self, req_id: u32, result: std::result::Result<Value, Failure>) {
        {
            let mut clients = self.mcp_clients.borrow_mut();
            clients.pending -= 1;
            clients.calls.remove(&req_id);
        }
        let (outcome, mut info, charge) = match result {
            Ok(result) => {
                let spent = client::spent_of(&result);
                let res = json!({ "content": result["content"],
                                  "structured": result.get("structuredContent"),
                                  "is_error": result["isError"] == true,
                                  "spent_nano_usd": spent.map(|s| s.nano),
                                  "spent_tokens": spent.map(|s| s.tokens) });
                let info = json!({ "ok": true, "result": {
                    "content": result["content"], "isError": result["isError"] == true,
                    "structuredContent": result.get("structuredContent") } });
                // A tool reports no usage: its declared cost stays charged.
                (Outcome::Response(res), info, None)
            }
            Err(failure) => {
                let error = failure_info(&failure);
                let mut info = error.clone();
                info["ok"] = false.into();
                let charge = (!failure.sent).then(Amount::default);
                (Outcome::Error(error), info, charge)
            }
        };
        if let Some(reason) = self.settle_mcp("mcp.call", req_id, charge) {
            info = json!({ "ok": false, "stage": "budget", "code": "LEDGER",
                           "server": "", "message": reason, "action": "" });
        }
        self.finish_mcp("mcp.call", req_id, outcome, info);
    }

    fn on_closed(&self, start: u32, closed: Closed) {
        let Some(conn_id) = self.mcp_clients.borrow().starts.get(&start).copied() else {
            return;
        };
        // Calls the server never read fail unsent. A replay hands back
        // their records instead.
        if self.replay.borrow().is_none() {
            let (unread, server) = {
                let clients = self.mcp_clients.borrow();
                let server = clients.conns.get(&conn_id).map(|c| c.server.clone());
                let mut unread: Vec<u32> = clients
                    .calls
                    .iter()
                    .filter(|(_, conn)| **conn == conn_id)
                    .map(|(req_id, _)| *req_id)
                    .collect();
                unread.sort_unstable();
                (unread, server.unwrap_or_default())
            };
            for req_id in unread {
                let message = "the server stopped before the call reached it".to_string();
                let error = mcp_error("exited", "SERVER_EXITED", &server, message, "");
                let failure = Failure {
                    error,
                    rpc: None,
                    exit_code: closed.exit_code,
                    stderr: String::new(),
                    sent: false,
                };
                self.on_called(req_id, Err(failure));
            }
        }
        let mut clients = self.mcp_clients.borrow_mut();
        let Some(conn) = clients.conns.get_mut(&conn_id).filter(|c| c.start == start) else {
            return;
        };
        let opened = std::mem::take(&mut conn.opened);
        conn.commands = None;
        if let Some(close_id) = conn.closing {
            let Some(conn) = clients.conns.remove(&conn_id) else {
                return;
            };
            clients.starts.retain(|_, owner| *owner != conn_id);
            clients.pending -= 1;
            drop(clients);
            // Only a server closed on request reports a spend to settle at;
            // one that went away may have spent anything up to its grant.
            // A lost server's grant was settled when it was lost.
            let failed = opened
                .then(|| self.settle_mcp("mcp.connect", start, closed.spent))
                .flatten();
            let spent = closed.spent;
            let res = json!({ "server_run": conn.run, "exit_code": closed.exit_code,
                              "spent_nano_usd": spent.map(|s| s.nano),
                              "spent_tokens": spent.map(|s| s.tokens) });
            let info = match failed {
                None => json!({ "ok": true, "exitCode": closed.exit_code }),
                Some(reason) => json!({ "ok": false, "stage": "budget", "code": "LEDGER",
                                        "server": conn.server, "message": reason, "action": "" }),
            };
            self.finish_mcp("mcp.close", close_id, Outcome::Response(res), info);
            return;
        }
        if !opened {
            // A failed start was settled and recorded when it was reported.
            return;
        }
        // The server went away on its own. The exit is recorded, as a replay
        // needs it to restart the server where the run did.
        conn.lost = Some(closed.clone());
        let req = json!({ "server": conn.server, "conn": conn_id, "start": start });
        let res = json!({ "server_run": conn.run, "exit_code": closed.exit_code });
        drop(clients);
        let failed = self.settle_mcp("mcp.connect", start, None);
        self.note_unsettled(failed);
        if self.replay.borrow().is_none() && self.journal.borrow().is_some() {
            track_effect(&self.state, "mcp.exit", start, req);
            self.record_mcp("mcp.exit", start, Outcome::Response(res));
        }
    }

    /// Closes every connection the script left open and waits for the
    /// servers to exit, so that a run never ends with a child still alive.
    /// A server that ignores its closed input is killed after the grace
    /// period; one still starting is killed when the runtime drops.
    pub(super) fn close_mcp_connections(&self) {
        let waiting = {
            let mut clients = self.mcp_clients.borrow_mut();
            let open: Vec<u32> = clients.conns.keys().copied().collect();
            for conn_id in open {
                let close_id = AUTO_CLOSE_IDS + clients.next_auto_close;
                let Some(conn) = clients
                    .conns
                    .get_mut(&conn_id)
                    .filter(|c| c.closing.is_none())
                else {
                    continue;
                };
                let live = self.replay.borrow().is_none();
                let req = begin_close(&self.state, live, conn, conn_id, close_id, Some("run.end"));
                if let Some(replay) = self.replay.borrow_mut().as_mut() {
                    replay.dispatch("mcp.close", close_id, &req, None);
                } else if self.journal.borrow().is_some() {
                    track_effect(&self.state, "mcp.close", close_id, req);
                }
                clients.next_auto_close += 1;
                clients.pending += 1;
            }
            !clients.conns.is_empty()
        };
        if !waiting {
            return;
        }
        // A replay hands the recorded closes back; there is nothing to wait on.
        if self.replay.borrow().is_some() {
            if let Some(replay) = self.replay.borrow_mut().as_mut() {
                replay.begin_turn();
            }
            loop {
                let next = self
                    .replay
                    .borrow_mut()
                    .as_mut()
                    .and_then(|r| r.next_event());
                match next {
                    Some(LoopEvent::Mcp(event)) => self.dispatch_mcp_event(*event),
                    Some(_) => {}
                    None => return,
                }
            }
        }
        let deadline = Instant::now() + client::EXIT_GRACE + Duration::from_secs(3);
        while !self.mcp_clients.borrow().conns.is_empty() {
            let wait = deadline.saturating_duration_since(Instant::now());
            match self.events_rx.recv_timeout(wait) {
                Ok(LoopEvent::Mcp(event)) => self.dispatch_mcp_event(*event),
                // The script has ended; nothing else it started is delivered.
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
}
