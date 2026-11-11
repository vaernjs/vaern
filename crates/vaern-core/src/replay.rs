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
//! Not recorded, so read live: `Date.now()`, `performance.now()`,
//! `process.env`, and the order of timers against effects. A script whose
//! output depends on them can diverge, and the replay reports where.

pub mod plan;

pub use plan::{Plan, RecordedEffect};

use crate::ai::PreparedCall;
use crate::effect_record::{bytes_of, headers_of};
use crate::error::{Result, VaernError};
use crate::event::{FetchCompletion, FsCompletion, LoopEvent, ModelCompletion};
use plan::text;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

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
        other => return Err(format!("a {other} record has no outcome to replay")),
    };
    Ok(vec![event])
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
        }
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
                .is_some_and(|eid| self.dispatched.contains_key(eid))
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
}
