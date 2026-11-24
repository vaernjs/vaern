//! `vaern replay`: runs a recorded script again with every effect served
//! from its journal, then checks that it printed the same transcript and
//! ended the same way.
//!
//! Nothing leaves the process during a replay. Each effect the script
//! starts is matched against the recorded effect with the same `eid`, which
//! is dispatch order: same kind, same request bytes. Results are handed back
//! in the order the journal recorded them completing, so concurrent effects
//! resolve as they did. A tool's `run()` runs again, and the effects it makes
//! are served the same way. Budgets are recomputed from the caps and ledger
//! balances the run recorded; no ledger is written.
//!
//! An MCP server is never started: `connect()`, its calls and its close are
//! served from their records, and the server script's SHA-256 must match.
//!
//! Not recorded, so read live: `Date.now()`, `performance.now()`,
//! `process.env`, and the order of timers against effects. A script whose
//! output depends on them can diverge, and the replay reports where.
//!
//! One timer-driven order is kept: an effect the script stopped with an
//! `AbortSignal` is held until the script aborts that signal again, since
//! handing its recorded failure back sooner would reject the call with a
//! default reason before the script's own abort ran.

pub mod plan;

pub use plan::{Plan, RecordedEffect};

use crate::ai::PreparedCall;
use crate::budget::Amount;
use crate::effect_record::{bytes_of, headers_of};
use crate::error::{Result, VaernError};
use crate::event::{FetchCompletion, FsCompletion, LoopEvent, ModelCompletion};
use crate::mcp::client::{Closed, Failure, Hello, McpEvent};
use crate::mcp::protocol::Era;
use plan::text;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

/// How much of each side a divergence shows.
const SHOWN_BYTES: usize = 200;

/// What a replay that matched its recording reproduced.
#[derive(Debug)]
pub struct Summary {
    pub run_id: String,
    pub console: usize,
    pub exit_code: i32,
}

/// A replay in progress; the runtime's bridges consult it instead of
/// performing effects.
pub struct Replay {
    plan: Plan,
    next_eid: u64,
    /// Matched effects waiting for their turn: request id and model call.
    dispatched: HashMap<u64, (u32, Option<PreparedCall>)>,
    cursor: usize,
    ready: VecDeque<LoopEvent>,
    /// The recorded turn being handed back in this loop turn, once one is.
    batch: Option<Option<u64>>,
    printed: usize,
    divergence: Option<VaernError>,
    /// The abort signal each started effect was given, by `eid`.
    signals: HashMap<u64, u32>,
    /// Signals the script has aborted during this replay.
    aborted: HashSet<u32>,
}

/// `text` cut to [`SHOWN_BYTES`] on a character boundary, as a JSON string.
fn shown(text: &str) -> String {
    let mut cut = text.len().min(SHOWN_BYTES);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let more = if cut < text.len() { "…" } else { "" };
    format!("{}{more}", Value::from(&text[..cut]))
}

fn console_line(fd: u8, line: &str) -> String {
    let stream = if fd == 2 { "stderr" } else { "stdout" };
    format!("{stream} {}", shown(line))
}

fn outcome_text(code: i32, error: Option<&str>) -> String {
    match error {
        Some(error) => format!("exit code {code} with error {}", shown(error)),
        None => format!("exit code {code}"),
    }
}

/// The error an effect recorded, rebuilt so the script receives it again.
fn recorded_error(kind: &str, error: &Value) -> VaernError {
    let field = |key: &str| text(error, key);
    match (kind, error.get("stage").is_some()) {
        ("fetch", true) => VaernError::Fetch {
            stage: field("stage"),
            url: field("url"),
            message: field("message"),
            action: field("action"),
        },
        ("fs", true) => VaernError::Fs {
            stage: field("stage"),
            code: field("code"),
            path: field("path"),
            message: field("message"),
            action: field("action"),
        },
        ("model.call", true) => VaernError::Model {
            stage: field("stage"),
            url: field("url"),
            status: error["status"]
                .as_u64()
                .and_then(|s| u16::try_from(s).ok())
                .unwrap_or(0),
            message: field("message"),
            action: field("action"),
            // Runs recorded before the field existed kept every reserve.
            sent: error["sent"].as_bool().unwrap_or(true),
        },
        _ => VaernError::Replayed {
            message: field("message"),
        },
    }
}

/// The events that hand a recorded effect's outcome back to the script.
fn completion_events(
    rec: &RecordedEffect,
    req_id: u32,
    call: Option<PreparedCall>,
) -> std::result::Result<Vec<LoopEvent>, String> {
    let res = rec.record.get("res");
    let error = || recorded_error(&rec.kind, &rec.record["error"]);
    let bytes = |value: &Value| bytes_of(value).ok_or("a recorded body does not decode");
    let event = match rec.kind.as_str() {
        "fetch" => LoopEvent::FetchDone(FetchCompletion {
            req_id,
            result: match res {
                Some(res) => Ok(crate::fetch::FetchResponse {
                    status: res["status"].as_u64().unwrap_or(0) as u16,
                    status_text: text(res, "status_text"),
                    headers: headers_of(&res["headers"]),
                    url: text(res, "url"),
                    body: bytes(&res["body"])?,
                }),
                None => Err(error()),
            },
        }),
        "fs" => LoopEvent::FsDone(FsCompletion {
            req_id,
            result: match res {
                Some(res) if res.get("bytes").is_some() => {
                    Ok(crate::fs::FsOutput::Bytes(bytes(&res["bytes"])?))
                }
                Some(res) => match res.get("text").and_then(Value::as_str) {
                    Some(t) => Ok(crate::fs::FsOutput::Text(t.to_string())),
                    None => Ok(crate::fs::FsOutput::Written),
                },
                None => Err(error()),
            },
        }),
        "model.call" => {
            let call = call.ok_or("the model call was not kept")?;
            let Some(res) = res else {
                return Ok(vec![model_done(req_id, call, Err(error()))]);
            };
            let status = res["status"].as_u64().unwrap_or(0) as u16;
            let body = text(res, "body");
            if call.stream {
                let (deltas, reply) = crate::ai_stream::replay(status, &body)?;
                let mut events: Vec<LoopEvent> = deltas
                    .into_iter()
                    .map(|delta| LoopEvent::ModelDelta(req_id, delta))
                    .collect();
                events.push(model_done(req_id, call, Ok(reply)));
                return Ok(events);
            }
            let reply = crate::ai::parse_reply(&call.endpoint, status, body.into_bytes());
            model_done(req_id, call, reply)
        }
        "mcp.connect" | "mcp.call" | "mcp.close" => {
            LoopEvent::Mcp(Box::new(mcp_event(rec, req_id)?))
        }
        other => return Err(format!("a {other} record has no outcome to replay")),
    };
    Ok(vec![event])
}

/// What an MCP connection reported, rebuilt from its record.
fn mcp_event(rec: &RecordedEffect, req_id: u32) -> std::result::Result<McpEvent, String> {
    let record = &rec.record;
    let error = &record["error"];
    let failure = || Failure {
        error: VaernError::Mcp {
            stage: text(error, "stage"),
            code: text(error, "code"),
            server: text(error, "server"),
            message: text(error, "message"),
            action: text(error, "action"),
        },
        rpc: error["rpc"].as_i64(),
        exit_code: error["exit_code"]
            .as_i64()
            .and_then(|c| i32::try_from(c).ok()),
        stderr: text(error, "stderr"),
        sent: error["sent"].as_bool().unwrap_or(true),
    };
    let spent = |res: &Value| {
        Some(Amount {
            nano: res["spent_nano_usd"].as_u64()?,
            tokens: res["spent_tokens"].as_u64()?,
        })
    };
    let res = record.get("res");
    Ok(match rec.kind.as_str() {
        "mcp.connect" => McpEvent::Connected {
            conn_id: req_id,
            result: match res {
                Some(res) => Ok(Hello {
                    era: match res["protocol_version"].as_str() {
                        Some(crate::mcp::protocol::VERSION_N1) => Era::Legacy,
                        _ => Era::Modern,
                    },
                    server: res["server"].clone(),
                    instructions: res["instructions"].as_str().map(String::from),
                    tools: res["tools"].as_array().cloned().unwrap_or_default(),
                    run: res["server_run"].as_str().map(String::from),
                    spent: None,
                }),
                None => Err(failure()),
            },
        },
        "mcp.call" => McpEvent::Called {
            req_id,
            result: match res {
                Some(res) => {
                    let mut result = serde_json::json!({ "content": res["content"], "isError": res["is_error"] });
                    if !res["structured"].is_null() {
                        result["structuredContent"] = res["structured"].clone();
                    }
                    Ok(result)
                }
                None => Err(failure()),
            },
        },
        _ => {
            let conn = record["req"]["conn"]
                .as_u64()
                .and_then(|c| u32::try_from(c).ok());
            let res = res.ok_or("the close has no outcome")?;
            McpEvent::Closed {
                conn_id: conn.ok_or("the close names no connection")?,
                closed: Closed {
                    exit_code: res["exit_code"]
                        .as_i64()
                        .and_then(|c| i32::try_from(c).ok()),
                    spent: spent(res),
                },
            }
        }
    })
}

fn model_done(req_id: u32, call: PreparedCall, result: Result<crate::ai::ModelReply>) -> LoopEvent {
    LoopEvent::ModelDone(Box::new(ModelCompletion {
        req_id,
        call,
        result,
    }))
}

impl Replay {
    pub fn new(plan: Plan) -> Replay {
        Replay {
            plan,
            next_eid: 0,
            dispatched: HashMap::new(),
            cursor: 0,
            ready: VecDeque::new(),
            batch: None,
            printed: 0,
            divergence: None,
            signals: HashMap::new(),
            aborted: HashSet::new(),
        }
    }

    /// Notes that effect `eid` was started with abort signal `signal`; 0 is
    /// none.
    pub fn watch_abort(&mut self, eid: u64, signal: u32) {
        if signal != 0 {
            self.signals.insert(eid, signal);
        }
    }

    /// Notes that the script aborted `signal`.
    pub fn note_abort(&mut self, signal: u32) {
        self.aborted.insert(signal);
    }

    /// Whether effect `eid` failed by abort in the recording and the
    /// script has not yet aborted its signal in this replay.
    fn held(&self, eid: u64) -> bool {
        let Some(signal) = self.signals.get(&eid) else {
            return false;
        };
        let stopped = self.plan.effects.get(&eid).is_some_and(|rec| {
            rec.record["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(crate::ai::ABORTED))
        });
        stopped && !self.aborted.contains(signal)
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    fn diverge(&mut self, seq: u64, expected: String, actual: String) {
        if self.divergence.is_none() {
            self.divergence = Some(VaernError::ReplayDivergence {
                run_id: self.plan.run_id.clone(),
                seq,
                expected,
                actual,
            });
        }
    }

    /// Matches an effect the script started and returns its `eid`. An
    /// effect past the end of the recording was still in flight when the
    /// run ended, and is never answered.
    pub fn dispatch(
        &mut self,
        kind: &str,
        req_id: u32,
        req: &Value,
        call: Option<PreparedCall>,
    ) -> u64 {
        let eid = self.next_eid;
        self.next_eid += 1;
        let Some(rec) = self.plan.effects.get(&eid) else {
            return eid;
        };
        let recorded = rec
            .record
            .get("req")
            .map(|req| crate::effect_record::redacted_request(req).to_string())
            .unwrap_or_default();
        let actual = req.to_string();
        if rec.kind != kind || recorded != actual {
            let (seq, expected) = (rec.seq, format!("{} {}", rec.kind, shown(&recorded)));
            self.diverge(seq, expected, format!("{kind} {}", shown(&actual)));
        } else if kind != "tool.call" {
            self.dispatched.insert(eid, (req_id, call));
        }
        eid
    }

    /// Checks one console write against the transcript. `false` means it
    /// is not part of the recording and must not be printed.
    pub fn console(&mut self, fd: u8, line: &str) -> bool {
        if self.divergence.is_some() {
            return false;
        }
        match self.plan.console.get(self.printed) {
            Some((_, rfd, rtext)) if *rfd == fd && rtext == line => {
                self.printed += 1;
                return true;
            }
            Some((seq, rfd, rtext)) => {
                let (seq, expected) = (*seq, console_line(*rfd, rtext));
                self.diverge(seq, expected, console_line(fd, line));
            }
            None => {
                let seq = self.plan.end.0;
                self.diverge(
                    seq,
                    "the end of the transcript".into(),
                    console_line(fd, line),
                );
            }
        }
        false
    }

    /// Starts a loop turn: outcomes the run handed over in one turn are
    /// handed back together, before the script's promise callbacks run.
    pub fn begin_turn(&mut self) {
        self.batch = None;
    }

    /// The next recorded outcome to hand back in this turn, once the script
    /// has started the effect it belongs to. A streamed reply's fragments
    /// are handed back with its completion.
    pub fn next_event(&mut self) -> Option<LoopEvent> {
        if let Some(event) = self.ready.pop_front() {
            return Some(event);
        }
        if self.divergence.is_some() {
            return None;
        }
        let eid = *self.plan.completions.get(self.cursor)?;
        if self.held(eid) {
            return None;
        }
        let turn = self.plan.effects.get(&eid)?.record["turn"].as_u64();
        if self
            .batch
            .is_some_and(|batch| batch.is_none() || batch != turn)
        {
            return None;
        }
        let (req_id, call) = self.dispatched.remove(&eid)?;
        self.batch = Some(turn);
        self.cursor += 1;
        let rec = self.plan.effects.get(&eid)?;
        match completion_events(rec, req_id, call) {
            Ok(events) => self.ready.extend(events),
            Err(reason) => {
                let (seq, expected) = (rec.seq, format!("{} effect {eid}", rec.kind));
                self.diverge(
                    seq,
                    expected,
                    format!("a record that cannot be replayed: {reason}"),
                );
            }
        }
        self.ready.pop_front()
    }

    /// True when an outcome can be handed back now.
    pub fn has_work(&self) -> bool {
        !self.ready.is_empty()
            || self
                .plan
                .completions
                .get(self.cursor)
                .is_some_and(|eid| self.dispatched.contains_key(eid) && !self.held(*eid))
    }

    /// Records that the script is waiting with nothing left to run.
    pub fn stall(&mut self) {
        let next = self.plan.completions.get(self.cursor);
        let (seq, expected) = match next.and_then(|eid| self.plan.effects.get(eid)) {
            Some(rec) => (
                rec.seq,
                format!("the {} effect the run started next", rec.kind),
            ),
            None => (self.plan.end.0, "the run to end".to_string()),
        };
        let actual = "the script waiting on an effect the recording never answered".into();
        self.diverge(seq, expected, actual);
    }

    /// Stops the replay at something it cannot serve. Returns the message
    /// the script's call fails with.
    pub fn refuse(&mut self, what: &str) -> String {
        let refusal = VaernError::Replay {
            code: "UNSUPPORTED".into(),
            message: format!(
                "{what} cannot run under vaern replay: incoming requests are not recorded"
            ),
            action: "Replay runs that do not serve requests.".into(),
        };
        let message = refusal.to_string();
        if self.divergence.is_none() {
            self.divergence = Some(refusal);
        }
        message
    }

    pub fn take_divergence(&mut self) -> Option<VaernError> {
        self.divergence.take()
    }

    /// Compares how the replay ended with the recording.
    pub fn finish(&mut self, outcome: Result<i32>) -> Result<Summary> {
        if let Some(divergence) = self.divergence.take() {
            return Err(divergence);
        }
        let (code, error) = match outcome {
            Ok(code) => (code, None),
            Err(
                err @ (VaernError::Replay { .. }
                | VaernError::ReplayDivergence { .. }
                | VaernError::ReplaySourceChanged { .. }),
            ) => return Err(err),
            Err(err) => (1, Some(err.to_string())),
        };
        let plan = &self.plan;
        let gone = |seq: u64, expected: String, actual: &str| VaernError::ReplayDivergence {
            run_id: plan.run_id.clone(),
            seq,
            expected,
            actual: actual.to_string(),
        };
        if let Some((seq, fd, line)) = plan.console.get(self.printed) {
            return Err(gone(*seq, console_line(*fd, line), "the replay ending"));
        }
        let missed = plan
            .completions
            .get(self.cursor)
            .and_then(|e| plan.effects.get(e));
        if let Some(rec) = missed {
            let expected = format!("{} effect {}", rec.kind, rec.record["eid"]);
            return Err(gone(rec.seq, expected, "the replay ending without it"));
        }
        let (seq, recorded_code, recorded_error) = &plan.end;
        if (*recorded_code, recorded_error.as_deref()) != (code, error.as_deref()) {
            let expected = outcome_text(*recorded_code, recorded_error.as_deref());
            return Err(gone(*seq, expected, &outcome_text(code, error.as_deref())));
        }
        Ok(Summary {
            run_id: plan.run_id.clone(),
            console: self.printed,
            exit_code: code,
        })
    }
}

/// Prints how a replay ended to stderr and returns the process exit code:
/// 0 when it reproduced the recording, 1 otherwise.
pub fn report(outcome: &Result<Summary>) -> i32 {
    match outcome {
        Ok(summary) => {
            eprintln!(
                "vaern replay: run {} reproduced: {} console records, exit code {}",
                summary.run_id, summary.console, summary.exit_code
            );
            0
        }
        Err(err) => {
            eprintln!("{err}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shown_text_is_cut_on_a_character_boundary() {
        let long = "é".repeat(150);
        let cut = shown(&long);
        assert!(cut.ends_with('…') && cut.len() <= SHOWN_BYTES + 6, "{cut}");
        assert_eq!(shown("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn test_recorded_errors_come_back_with_their_fields() {
        let error = serde_json::json!({ "stage": "open", "code": "ENOENT", "path": "x",
                                        "message": "no such file", "action": "Check it." });
        let rebuilt = recorded_error("fs", &error).to_string();
        assert_eq!(
            rebuilt,
            "fs open failed for 'x' (ENOENT): no such file. Check it."
        );
        let plain = recorded_error("fetch", &serde_json::json!({ "message": "boom" }));
        assert_eq!(plain.to_string(), "boom");
    }

    #[test]
    fn test_recorded_mcp_outcomes_come_back_as_the_client_reported_them() {
        let rec = |kind: &str, record: Value| RecordedEffect {
            seq: 3,
            kind: kind.into(),
            record,
        };
        let failed = rec(
            "mcp.connect",
            serde_json::json!({ "error": {
            "stage": "start", "code": "SERVER_EXITED", "server": "./s.ts", "message": "it exited",
            "action": "Run it.", "sent": true, "exit_code": 2, "stderr": "boom" } }),
        );
        let Ok(McpEvent::Connected {
            conn_id: 7,
            result: Err(failure),
        }) = mcp_event(&failed, 7)
        else {
            panic!("a failed connect comes back as a failure");
        };
        assert_eq!(
            (failure.exit_code, failure.stderr.as_str()),
            (Some(2), "boom")
        );
        assert!(
            matches!(failure.error, VaernError::Mcp { ref code, .. } if code == "SERVER_EXITED")
        );

        let legacy = rec(
            "mcp.connect",
            serde_json::json!({ "res": {
            "protocol_version": crate::mcp::protocol::VERSION_N1, "server": { "name": "s" },
            "instructions": null, "tools": [], "server_run": "r1" } }),
        );
        let Ok(McpEvent::Connected {
            result: Ok(hello), ..
        }) = mcp_event(&legacy, 1)
        else {
            panic!("a connect comes back as its hello");
        };
        assert_eq!((hello.era, hello.run.as_deref()), (Era::Legacy, Some("r1")));

        let call = rec(
            "mcp.call",
            serde_json::json!({ "res": {
            "content": [], "structured": null, "is_error": false } }),
        );
        let Ok(McpEvent::Called {
            result: Ok(result), ..
        }) = mcp_event(&call, 2)
        else {
            panic!("a call comes back as its result");
        };
        assert!(result.get("structuredContent").is_none());

        let lost = rec(
            "mcp.close",
            serde_json::json!({ "req": { "server": "./s.ts", "conn": 1 },
            "res": { "exit_code": null, "spent_nano_usd": null, "spent_tokens": null } }),
        );
        let Ok(McpEvent::Closed { conn_id: 1, closed }) = mcp_event(&lost, 9) else {
            panic!("a close comes back for its connection");
        };
        assert_eq!((closed.exit_code, closed.spent), (None, None));
    }
}
