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
//! A replay starts no server. Each effect is matched against its record and
//! its recorded outcome is handed back, so a connection has no channel.

use super::{JournalSlot, QuickJsRuntime, ReplaySlot, SharedState, model_error_info, track_effect};
use crate::budget::{Amount, Limits};
use crate::effect_record::Outcome;
use crate::error::{Result, VaernError};
use crate::event::LoopEvent;
use crate::mcp::client::{self, Command, Failure, Launch, McpEvent};
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

/// One connection, from `connect()` until its server has exited.
struct Conn {
    /// `None` in a replay, where no server runs.
    commands: Option<UnboundedSender<Command>>,
    /// The server script's path, which names the server in errors.
    server: String,
    opened: bool,
    run: Option<String>,
    /// The request id of the close in progress.
    closing: Option<u32>,
}

/// The run's connections, shared with the bridges.
#[derive(Default)]
pub(super) struct McpClients {
    conns: HashMap<u32, Conn>,
    /// Connects, calls and closes in flight; they keep the loop alive.
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

/// Starts effect `kind` of request `req_id`: tracked for its journal
/// record, or, in a replay, matched against the recorded one.
fn begin_effect(
    state: &Rc<RefCell<SharedState>>,
    journal: &JournalSlot,
    replay: &ReplaySlot,
    kind: &'static str,
    req_id: u32,
    req: Value,
) {
    if let Some(replay) = replay.borrow_mut().as_mut() {
        replay.dispatch(kind, req_id, &req, None);
    } else if journal.borrow().is_some() {
        track_effect(state, kind, req_id, req);
    }
}

impl QuickJsRuntime {
    /// Registers `__vaern_mcp_connect`, `__vaern_mcp_call_tool` and
    /// `__vaern_mcp_close`. Each returns `null` once its effect is under
    /// way, or the JSON of the error that stopped it before it began.
    pub(super) fn register_mcp_client<'js>(
        &self,
        ctx: &Ctx<'js>,
        globals: &Object<'js>,
        tokio_handle: &tokio::runtime::Handle,
    ) -> Result<()> {
        let (state, journal, budget, replay) = (
            self.state.clone(),
            self.journal.clone(),
            self.budget.clone(),
            self.replay.clone(),
        );
        let (clients, homes, tokio_handle) = (
            self.mcp_clients.clone(),
            self.reserved_homes.clone(),
            tokio_handle.clone(),
        );
        let connect_fn = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, req_id: u32, script: String, nano: f64, tokens: f64| {
                let base = caller_module(&ctx)
                    .and_then(|name| Path::new(&name).parent().map(Path::to_path_buf))
                    .unwrap_or_default();
                let path = base.join(&script);
                let server = path.display().to_string();
                let Ok(source) = std::fs::read(&path) else {
                    let message = format!("no server script at {server}");
                    let action = "Give connect() a path relative to the calling module.";
                    return refused(&mcp_error(
                        "config",
                        "SCRIPT_NOT_FOUND",
                        &server,
                        message,
                        action,
                    ));
                };
                let grant = Amount {
                    nano: nano.max(0.0) as u64,
                    tokens: tokens.max(0.0) as u64,
                };
                let reserved = budget.borrow_mut().reserve(
                    journal.borrow_mut().as_mut(),
                    "mcp.connect",
                    req_id,
                    grant,
                );
                if let Err(err) = reserved {
                    return Some(model_error_info(err).to_string());
                }
                let req = json!({ "script": server, "budget": { "nano_usd": grant.nano, "tokens": grant.tokens },
                                  "script_sha256": crate::journal::sha256_hex(&source) });
                let open = |commands| {
                    let conn = Conn {
                        commands,
                        server: server.clone(),
                        opened: false,
                        run: None,
                        closing: None,
                    };
                    let mut clients = clients.borrow_mut();
                    clients.conns.insert(req_id, conn);
                    clients.pending += 1;
                };
                if replay.borrow().is_some() {
                    begin_effect(&state, &journal, &replay, "mcp.connect", req_id, req);
                    open(None);
                    return None;
                }
                let program = clients.borrow().program.clone();
                let Some(program) = program.or_else(|| std::env::current_exe().ok()) else {
                    let _ = budget.borrow_mut().settle(
                        journal.borrow_mut().as_mut(),
                        "mcp.connect",
                        req_id,
                        Some(Amount::default()),
                    );
                    let message = "the path of the vaern binary is unknown".to_string();
                    return refused(&mcp_error("spawn", "SPAWN_FAILED", &server, message, ""));
                };
                begin_effect(&state, &journal, &replay, "mcp.connect", req_id, req);
                let launch = Launch {
                    program,
                    script: path,
                    home: homes.borrow()[0].clone(),
                    grant: Limits {
                        nano: Some(grant.nano),
                        tokens: Some(grant.tokens),
                    },
                };
                let (commands, rx) = tokio::sync::mpsc::unbounded_channel();
                let events = state.borrow().events_tx.clone();
                let send = move |event| {
                    let _ = events.send(LoopEvent::Mcp(Box::new(event)));
                };
                tokio_handle.spawn(client::run(launch, req_id, send, rx));
                open(Some(commands));
                None
            },
        );

        let (state, journal, budget, replay, clients) = (
            self.state.clone(),
            self.journal.clone(),
            self.budget.clone(),
            self.replay.clone(),
            self.mcp_clients.clone(),
        );
        let call_fn = Function::new(
            ctx.clone(),
            move |req_id: u32, conn_id: u32, tool: String, args: String, cost_nano: f64| {
                let mut clients = clients.borrow_mut();
                let Some(conn) = clients.conns.get(&conn_id).filter(|c| c.closing.is_none()) else {
                    let message = "the connection is closed".to_string();
                    let action = "Connect again with connect().";
                    return refused(&mcp_error("exited", "CLOSED", "", message, action));
                };
                // As with tool(), a call without a declared cost reserves
                // nothing, so it leaves no budget records.
                if cost_nano >= 1.0 {
                    let cost = Amount {
                        nano: cost_nano as u64,
                        tokens: 0,
                    };
                    let reserved = budget.borrow_mut().reserve(
                        journal.borrow_mut().as_mut(),
                        "mcp.call",
                        req_id,
                        cost,
                    );
                    if let Err(err) = reserved {
                        return Some(model_error_info(err).to_string());
                    }
                }
                let args: Value = serde_json::from_str(&args).unwrap_or_else(|_| json!({}));
                let req = json!({ "server": conn.server, "tool": tool, "args": args });
                let sent = match &conn.commands {
                    Some(commands) => commands.send(Command::Call { req_id, tool, args }),
                    None => Ok(()),
                };
                if sent.is_err() {
                    let _ = budget.borrow_mut().settle(
                        journal.borrow_mut().as_mut(),
                        "mcp.call",
                        req_id,
                        Some(Amount::default()),
                    );
                    let message = "the server has stopped".to_string();
                    return refused(&mcp_error("exited", "CLOSED", &conn.server, message, ""));
                }
                begin_effect(&state, &journal, &replay, "mcp.call", req_id, req);
                clients.pending += 1;
                None
            },
        );

        let (state, journal, replay, clients) = (
            self.state.clone(),
            self.journal.clone(),
            self.replay.clone(),
            self.mcp_clients.clone(),
        );
        let close_fn = Function::new(ctx.clone(), move |req_id: u32, conn_id: u32| {
            let mut clients = clients.borrow_mut();
            let Some(conn) = clients
                .conns
                .get_mut(&conn_id)
                .filter(|c| c.closing.is_none())
            else {
                return Some(json!({ "done": true }).to_string());
            };
            conn.closing = Some(req_id);
            if let Some(commands) = &conn.commands {
                let _ = commands.send(Command::Close);
            }
            let req = json!({ "server": conn.server, "conn": conn_id });
            begin_effect(&state, &journal, &replay, "mcp.close", req_id, req);
            clients.pending += 1;
            None
        });

        for (name, bridge) in [
            ("__vaern_mcp_connect", connect_fn),
            ("__vaern_mcp_call_tool", call_fn),
            ("__vaern_mcp_close", close_fn),
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

    pub(super) fn dispatch_mcp_event(&self, event: McpEvent) {
        match event {
            McpEvent::Connected { conn_id, result } => {
                self.mcp_clients.borrow_mut().pending -= 1;
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
                        // A server that never started cannot have spent; one
                        // that started ran its script's top level, which may have.
                        let spawned = !matches!(&failure.error,
                            VaernError::Mcp { stage, .. } if stage == "spawn");
                        let charge = (!spawned).then(Amount::default);
                        let error = failure_info(&failure);
                        let mut info = error.clone();
                        if let Some(reason) = self.settle_mcp("mcp.connect", conn_id, charge) {
                            info["message"] = reason.into();
                        }
                        info["ok"] = false.into();
                        (Outcome::Error(error), info)
                    }
                };
                info["connId"] = conn_id.into();
                self.finish_mcp("mcp.connect", conn_id, outcome, info);
            }
            McpEvent::Called { req_id, result } => {
                self.mcp_clients.borrow_mut().pending -= 1;
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
            McpEvent::Closed { conn_id, closed } => {
                let Some(conn) = self.mcp_clients.borrow_mut().conns.remove(&conn_id) else {
                    return;
                };
                if !conn.opened {
                    // A failed start was settled when it was reported.
                    return;
                }
                // Only a server closed on request reports a spend to settle
                // at; one that went away may have spent anything up to its grant.
                let charge = conn.closing.and(closed.spent);
                let failed = self.settle_mcp("mcp.connect", conn_id, charge);
                let Some(close_id) = conn.closing else {
                    return;
                };
                self.mcp_clients.borrow_mut().pending -= 1;
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
            }
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
                conn.closing = Some(close_id);
                if let Some(commands) = &conn.commands {
                    let _ = commands.send(Command::Close);
                }
                let req = json!({ "server": conn.server, "conn": conn_id, "at": "run.end" });
                begin_effect(
                    &self.state,
                    &self.journal,
                    &self.replay,
                    "mcp.close",
                    close_id,
                    req,
                );
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
