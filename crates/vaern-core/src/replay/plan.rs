//! Reads a sealed run's journal into what a replay needs: the script, its
//! sources, seed and budget, every effect by `eid`, completion order, the
//! transcript and the outcome.
//!
//! The journal is verified first and read again after; the two reads must
//! agree on the final hash, so a replay never works from records that did
//! not verify.

use crate::budget::{Amount, Limits};
use crate::error::{Result, VaernError};
use crate::module_loader::{FsModuleLoader, ModuleLoader};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

/// One recorded effect: the record that holds its request and outcome.
pub struct RecordedEffect {
    /// Where it is in the journal.
    pub seq: u64,
    /// `fetch`, `fs`, `model.call` or `tool.call`.
    pub kind: String,
    pub record: Value,
}

/// A sealed run, read and verified: what a replay needs from its journal.
pub struct Plan {
    /// The run's directory name under `runs/`.
    pub run_id: String,
    /// The entry script as `vaern run` was given it.
    pub script: String,
    pub argv: Vec<String>,
    /// The four words that seeded `Math.random`.
    pub seed: [u32; 4],
    /// The `--budget-usd` and `--budget-tokens` caps.
    pub caps: Limits,
    /// Each named ledger's balance when the run opened it.
    pub ledgers: HashMap<String, Amount>,
    /// Every module the run loaded, entry first: path and SHA-256.
    pub modules: Vec<(String, String)>,
    /// Effects by `eid`.
    pub effects: HashMap<u64, RecordedEffect>,
    /// `eid`s of fetch, fs and model effects in the order they completed.
    pub completions: Vec<u64>,
    /// The transcript: `seq`, fd and text of each console record.
    pub console: Vec<(u64, u8, String)>,
    /// `run.end`: its `seq`, the exit code and the error, if the run failed.
    pub end: (u64, i32, Option<String>),
}

fn malformed(message: String) -> VaernError {
    VaernError::Replay {
        code: "MALFORMED".into(),
        message,
        action: "Replay a run recorded by this version of vaern; `vaern journal verify` checks the file."
            .into(),
    }
}

/// A string field, or empty.
pub(crate) fn text(record: &Value, key: &str) -> String {
    record
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn seed_of(hex: &str) -> Option<[u32; 4]> {
    let mut seed = [0u32; 4];
    for (i, word) in seed.iter_mut().enumerate() {
        *word = u32::from_str_radix(hex.get(i * 8..i * 8 + 8)?, 16).ok()?;
    }
    (hex.len() == 32).then_some(seed)
}

/// A run that cannot be replayed because record `seq` holds only the start
/// of content the script received.
fn truncated(seq: u64, what: &str) -> VaernError {
    VaernError::Replay {
        code: "TRUNCATED".into(),
        message: format!(
            "record {seq} holds only the first {} bytes of {what}, so the run cannot be reproduced from its journal",
            crate::journal::MAX_RECORDED_BYTES
        ),
        action: "The journal still verifies and names the full length and SHA-256; only runs whose bodies and console writes fit the limit can be replayed."
            .into(),
    }
}

impl Plan {
    /// Reads the journal at `path`. The run must verify and be sealed: an
    /// interrupted run has no outcome to compare with.
    pub fn load(path: &Path) -> Result<Plan> {
        let run_id = path
            .parent()
            .and_then(Path::file_name)
            .map(|id| id.to_string_lossy().into_owned())
            .unwrap_or_default();
        let verified = crate::journal::verify(path)?;
        if !verified.sealed {
            return Err(VaernError::Replay {
                code: "UNSEALED".into(),
                message: format!(
                    "run '{run_id}' has no run.end record, so it has no outcome to compare"
                ),
                action: "Replay a run that finished; `vaern journal verify` shows a run's state."
                    .into(),
            });
        }
        let content = std::fs::read_to_string(path).map_err(|err| VaernError::Journal {
            stage: "read".into(),
            path: path.display().to_string(),
            reason: err.to_string(),
            action: "Check the disk and retry.".into(),
        })?;
        let mut plan = Plan {
            run_id,
            script: String::new(),
            argv: Vec::new(),
            seed: [0; 4],
            caps: Limits::default(),
            ledgers: HashMap::new(),
            modules: Vec::new(),
            effects: HashMap::new(),
            completions: Vec::new(),
            console: Vec::new(),
            end: (0, 0, None),
        };
        let mut seeded = false;
        let mut last_hash = String::new();
        let mut count = 0u64;
        for line in content.lines() {
            let framed: Value = serde_json::from_str(line)
                .map_err(|_| malformed("a journal line is not JSON".into()))?;
            last_hash = text(&framed, "h");
            count += 1;
            let r = framed.get("r").cloned().unwrap_or(Value::Null);
            let seq = r.get("seq").and_then(Value::as_u64).unwrap_or_default();
            let eid = r.get("eid").and_then(Value::as_u64);
            match (
                r.get("kind").and_then(Value::as_str).unwrap_or_default(),
                eid,
            ) {
                ("run.start", _) => {
                    plan.script = text(&r, "script");
                    plan.argv = r["argv"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default();
                    let caps = &r["budget_caps"];
                    plan.caps = Limits {
                        nano: caps.get("nano_usd").and_then(Value::as_u64),
                        tokens: caps.get("tokens").and_then(Value::as_u64),
                    };
                    if let Some(seed) = seed_of(&text(&r, "random_seed")) {
                        plan.seed = seed;
                        seeded = true;
                    }
                }
                ("module.load", _) => plan.modules.push((text(&r, "path"), text(&r, "sha256"))),
                ("console", _) => {
                    if r.get("truncated").is_some() {
                        return Err(truncated(seq, "a console write"));
                    }
                    let fd = if r["fd"].as_u64() == Some(2) { 2 } else { 1 };
                    plan.console.push((seq, fd, text(&r, "text")));
                }
                ("ledger.open", _) => {
                    let get = |key: &str| r.get(key).and_then(Value::as_u64).unwrap_or(0);
                    let spent = Amount {
                        nano: get("spent_nano_usd"),
                        tokens: get("spent_tokens"),
                    };
                    plan.ledgers.insert(text(&r, "name"), spent);
                }
                (kind @ ("fetch" | "fs" | "model.call" | "tool.call"), Some(eid)) => {
                    if crate::effect_record::outcome_is_truncated(&r) {
                        return Err(truncated(seq, "an effect's result"));
                    }
                    if kind != "tool.call" {
                        plan.completions.push(eid);
                    }
                    let kind = kind.to_string();
                    plan.effects.insert(
                        eid,
                        RecordedEffect {
                            seq,
                            kind,
                            record: r,
                        },
                    );
                }
                ("run.end", _) => {
                    let code = r["exit_code"].as_i64().and_then(|c| i32::try_from(c).ok());
                    let error = r.get("error").and_then(Value::as_str).map(String::from);
                    plan.end = (seq, code.unwrap_or(1), error);
                }
                _ => {}
            }
        }
        // The file is read twice; the second read must be what was verified.
        if count != verified.records || last_hash != verified.final_hash {
            return Err(malformed("the journal changed while it was read".into()));
        }
        if !seeded || plan.script.is_empty() {
            return Err(malformed(
                "run.start has no script or random seed; the run was recorded by an older vaern"
                    .into(),
            ));
        }
        Ok(plan)
    }

    /// Fails before any code runs if a source the run loaded has changed.
    pub fn check_sources(&self) -> Result<()> {
        for (path, recorded) in &self.modules {
            let current = match FsModuleLoader.load(Path::new(path)) {
                Ok(source) => crate::journal::sha256_hex(source.as_bytes()),
                Err(_) => "missing".to_string(),
            };
            if &current != recorded {
                return Err(VaernError::ReplaySourceChanged {
                    path: path.clone(),
                    recorded: recorded.clone(),
                    current,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Journal;
    use serde_json::json;

    fn fields(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn test_seeds_are_four_hex_words() {
        assert_eq!(
            seed_of("0000000100000002fffffffe0000000a"),
            Some([1, 2, 0xffff_fffe, 10])
        );
        assert_eq!(seed_of("00000001"), None);
        assert_eq!(seed_of("0000000100000002fffffffe0000000g"), None);
        assert_eq!(seed_of("0000000100000002fffffffe0000000a00"), None);
    }

    #[test]
    fn test_a_console_write_over_the_limit_is_recorded_cut_and_refuses_a_replay() {
        let root = tempfile::tempdir().unwrap();
        let limit = crate::journal::MAX_RECORDED_BYTES;
        let mut journal = Journal::create(&root.path().join(".vaern")).unwrap();
        journal
            .append("run.start", fields(json!({ "script": "main.js" })))
            .unwrap();
        // At the limit a write is recorded whole; one byte over, it is cut.
        let whole = "c".repeat(limit);
        let over = "c".repeat(limit + 1);
        journal.console(1, &whole).unwrap();
        let seq = journal.console(2, &over).unwrap();
        journal.seal(0, None).unwrap();
        let path = journal.path().to_path_buf();

        let lines: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["r"].clone())
            .collect();
        assert!(lines[1].get("truncated").is_none());
        assert_eq!(lines[1]["text"].as_str().unwrap().len(), limit);
        assert_eq!(lines[2]["text"].as_str().unwrap().len(), limit);
        assert_eq!(lines[2]["truncated"]["len"], limit + 1);
        assert_eq!(
            lines[2]["truncated"]["sha256"],
            crate::journal::sha256_hex(over.as_bytes())
        );
        assert!(crate::journal::verify(&path).unwrap().sealed);

        match Plan::load(&path) {
            Err(VaernError::Replay { code, message, .. }) => {
                assert_eq!(code, "TRUNCATED");
                assert!(message.contains(&format!("record {seq} ")), "{message}");
                assert!(message.contains("a console write"), "{message}");
            }
            other => panic!("{:?}", other.map(|plan| plan.run_id)),
        }
    }

    #[test]
    fn test_a_sealed_run_is_read_and_changed_sources_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("main.js");
        std::fs::write(&script, "console.log(1);\n").unwrap();
        let script = script.display().to_string();
        let sha =
            crate::journal::sha256_hex(FsModuleLoader.load(Path::new(&script)).unwrap().as_bytes());
        let mut journal = Journal::create(&root.path().join(".vaern")).unwrap();
        for (kind, record) in [
            (
                "run.start",
                json!({ "script": script, "argv": ["vaern", script],
                                  "random_seed": "0000000100000002000000030000000a",
                                  "budget_caps": { "nano_usd": 5, "tokens": null } }),
            ),
            ("module.load", json!({ "path": script, "sha256": sha })),
            (
                "ledger.open",
                json!({ "name": "team", "spent_nano_usd": 7, "spent_tokens": 3 }),
            ),
            (
                "tool.call",
                json!({ "eid": 0, "req": { "name": "t", "args": {} } }),
            ),
            ("fs", json!({ "eid": 2, "dur_us": 1, "req": {}, "res": {} })),
            (
                "fetch",
                json!({ "eid": 1, "dur_us": 1, "req": {}, "res": {} }),
            ),
        ] {
            journal.append(kind, fields(record)).unwrap();
        }
        journal.console(1, "1\n").unwrap();
        let path = journal.path().to_path_buf();

        let unsealed = Plan::load(&path).err().unwrap();
        assert!(matches!(unsealed, VaernError::Replay { ref code, .. } if code == "UNSEALED"));

        journal.seal(3, None).unwrap();
        let plan = Plan::load(&path).unwrap();
        assert_eq!(plan.argv, ["vaern", script.as_str()]);
        assert_eq!(plan.seed, [1, 2, 3, 10]);
        assert_eq!(plan.caps.nano, Some(5));
        assert_eq!(plan.caps.tokens, None);
        assert_eq!(plan.ledgers["team"], Amount { nano: 7, tokens: 3 });
        // Completion order is journal order; tools are matched, not delivered.
        assert_eq!(plan.completions, [2, 1]);
        assert_eq!(plan.effects[&0].kind, "tool.call");
        assert_eq!(plan.console, [(6, 1, "1\n".to_string())]);
        assert_eq!((plan.end.0, plan.end.1, plan.end.2.is_none()), (7, 3, true));

        plan.check_sources().unwrap();
        std::fs::write(&script, "console.log(2);\n").unwrap();
        let changed = plan.check_sources().err().unwrap();
        assert!(
            matches!(changed, VaernError::ReplaySourceChanged { ref path, .. } if *path == script)
        );
    }
}
