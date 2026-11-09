//! Reservation budgets: no model call or tool effect is dispatched unless
//! its reserve fits the remaining budget.
//!
//! A reserve is the most a call can cost: for a model call, the input
//! estimate plus the full output cap that is sent to the provider; for a
//! tool, its declared cost. It is written durably before the call is
//! dispatched, and settled with the actual charge when the call returns.
//! Amounts are estimates in nano-USD and tokens; a provider's bill can
//! differ.
//!
//! An unnamed budget lives for one run, recorded in the run's journal. A
//! named budget also keeps a ledger in `<home>/budgets/<name>.jsonl`, in the
//! journal's chained format, so spending carries across runs. On load, a
//! reserve without a settle (a crash mid-call) counts as fully spent. One
//! process holds a ledger at a time, through a lock on `<name>.lock` that
//! the OS releases if the process dies.

use crate::error::{Result, VaernError};
use crate::journal::Journal;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Limits for one run. `None` leaves a unit unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    pub nano: Option<u64>,
    pub tokens: Option<u64>,
}

/// An amount in both units.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Amount {
    pub nano: u64,
    pub tokens: u64,
}

struct Ledger {
    journal: Journal,
    // Held for the run; dropping it releases the lock.
    _lock: std::fs::File,
}

/// A run's budget.
pub struct Budget {
    limits: Limits,
    spent: Amount,
    open: HashMap<u64, Amount>,
    next_rid: u64,
    ledger: Option<Ledger>,
}

fn budget_error(code: &str, message: String, action: &str) -> VaernError {
    VaernError::Budget {
        code: code.to_string(),
        unit: None,
        reserve: 0,
        remaining: 0,
        message,
        action: action.to_string(),
    }
}

/// A ledger name: 1 to 64 of `[A-Za-z0-9_-]`, so it is a plain file name.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn amount_of(record: &Value) -> Amount {
    let get = |key: &str| record.get(key).and_then(Value::as_u64).unwrap_or(0);
    Amount {
        nano: get("nano_usd"),
        tokens: get("tokens"),
    }
}

impl Budget {
    /// A budget for this run only.
    pub fn new(limits: Limits) -> Budget {
        Budget {
            limits,
            spent: Amount::default(),
            open: HashMap::new(),
            next_rid: 0,
            ledger: None,
        }
    }

    /// A budget whose spending is kept in the named ledger under `home`.
    pub fn named(home: &Path, name: &str, limits: Limits) -> Result<Budget> {
        if !valid_name(name) {
            return Err(budget_error(
                "INVALID",
                format!("'{name}' is not a budget name"),
                "Use 1 to 64 letters, digits, '_' or '-'.",
            ));
        }
        let dir = home.join("budgets");
        let ledger_error = |err: std::io::Error| {
            budget_error(
                "LEDGER",
                format!("cannot open the ledger in {}: {err}", dir.display()),
                "Check that the directory is writable.",
            )
        };
        std::fs::create_dir_all(&dir).map_err(ledger_error)?;
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(format!("{name}.lock")))
            .map_err(ledger_error)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(budget_error(
                    "LEDGER_BUSY",
                    format!("another run is using the budget '{name}'"),
                    "Wait for it to finish, or use a different budget name.",
                ));
            }
            Err(std::fs::TryLockError::Error(err)) => return Err(ledger_error(err)),
        }
        let path = dir.join(format!("{name}.jsonl"));
        let journal = Journal::resume(&path)?;
        // Settled reserves count their charge; unsettled ones their reserve.
        let text = std::fs::read_to_string(&path).map_err(ledger_error)?;
        let mut reserves: HashMap<(String, u64), Amount> = HashMap::new();
        let mut spent = Amount::default();
        for line in text.lines() {
            let Some(record) = serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|l| l.get("r").cloned())
            else {
                continue;
            };
            let key = (
                record["run"].as_str().unwrap_or_default().to_string(),
                record["rid"].as_u64().unwrap_or_default(),
            );
            match record["kind"].as_str() {
                Some("budget.reserve") => {
                    reserves.insert(key, amount_of(&record));
                }
                Some("budget.settle") => {
                    reserves.remove(&key);
                    let charge = amount_of(&record);
                    spent.nano = spent.nano.saturating_add(charge.nano);
                    spent.tokens = spent.tokens.saturating_add(charge.tokens);
                }
                _ => {}
            }
        }
        for unsettled in reserves.values() {
            spent.nano = spent.nano.saturating_add(unsettled.nano);
            spent.tokens = spent.tokens.saturating_add(unsettled.tokens);
        }
        Ok(Budget {
            limits,
            spent,
            open: HashMap::new(),
            next_rid: 0,
            ledger: Some(Ledger {
                journal,
                _lock: lock,
            }),
        })
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    fn lower_to(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// What is left after spending and the reserves still open. `None` for
    /// a unit without a limit.
    pub fn remaining(&self) -> Limits {
        let committed = self.open.values().fold(self.spent, |sum, r| Amount {
            nano: sum.nano.saturating_add(r.nano),
            tokens: sum.tokens.saturating_add(r.tokens),
        });
        Limits {
            nano: self.limits.nano.map(|l| l.saturating_sub(committed.nano)),
            tokens: self
                .limits
                .tokens
                .map(|l| l.saturating_sub(committed.tokens)),
        }
    }

    /// Reserves `amount` for an effect of `kind`, or refuses it. The reserve
    /// is on disk when this returns; the returned id settles it.
    pub fn reserve(
        &mut self,
        journal: Option<&mut Journal>,
        run: &str,
        kind: &str,
        amount: Amount,
    ) -> Result<u64> {
        let remaining = self.remaining();
        for (unit, reserve, left) in [
            ("usd", amount.nano, remaining.nano),
            ("tokens", amount.tokens, remaining.tokens),
        ] {
            if let Some(left) = left.filter(|left| reserve > *left) {
                let shown = |n: u64| match unit {
                    "usd" => format!("{:.9} USD", n as f64 / 1e9),
                    _ => format!("{n} tokens"),
                };
                return Err(VaernError::Budget {
                    code: "RESERVE_EXCEEDS_REMAINING".to_string(),
                    unit: Some(unit.to_string()),
                    reserve,
                    remaining: left,
                    message: format!(
                        "the {kind} reserves {}, and {} remain",
                        shown(reserve),
                        shown(left)
                    ),
                    action: "Lower maxTokens, shorten the request, or raise the budget."
                        .to_string(),
                });
            }
        }
        let rid = self.next_rid;
        let fields = |extra: Value| {
            let mut fields = Map::new();
            fields.insert("rid".into(), rid.into());
            fields.insert("run".into(), run.into());
            fields.insert("for".into(), kind.into());
            fields.insert("nano_usd".into(), amount.nano.into());
            fields.insert("tokens".into(), amount.tokens.into());
            if let Value::Object(extra) = extra {
                fields.extend(extra);
            }
            fields
        };
        if let Some(ledger) = self.ledger.as_mut() {
            ledger
                .journal
                .append_durable("budget.reserve", fields(Value::Null))?;
        }
        if let Some(journal) = journal {
            let left = json!({ "remaining_nano_usd": remaining.nano, "remaining_tokens": remaining.tokens });
            journal.append_durable("budget.reserve", fields(left))?;
        }
        self.next_rid += 1;
        self.open.insert(rid, amount);
        Ok(rid)
    }

    /// Settles reserve `rid` with the actual `charge`, which may exceed the
    /// reserve when the estimate ran low; the next reserve sees it.
    pub fn settle(
        &mut self,
        journal: Option<&mut Journal>,
        run: &str,
        rid: u64,
        charge: Amount,
    ) -> Result<()> {
        if self.open.remove(&rid).is_none() {
            return Ok(());
        }
        self.spent.nano = self.spent.nano.saturating_add(charge.nano);
        self.spent.tokens = self.spent.tokens.saturating_add(charge.tokens);
        let mut fields = Map::new();
        fields.insert("rid".into(), rid.into());
        fields.insert("run".into(), run.into());
        fields.insert("nano_usd".into(), charge.nano.into());
        fields.insert("tokens".into(), charge.tokens.into());
        if let Some(ledger) = self.ledger.as_mut() {
            ledger.journal.append("budget.settle", fields.clone())?;
        }
        if let Some(journal) = journal {
            journal.append("budget.settle", fields)?;
        }
        Ok(())
    }
}

/// The lower of two limits, where `None` is no limit.
fn lower(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

fn lowered(a: Limits, b: Limits) -> Limits {
    Limits {
        nano: lower(a.nano, b.nano),
        tokens: lower(a.tokens, b.tokens),
    }
}

/// A run's budget as the runtime holds it: the caps from the command line,
/// where named ledgers live, the budget once one applies, and the reserve
/// behind each call in flight.
#[derive(Default)]
pub struct RunBudget {
    caps: Limits,
    home: Option<PathBuf>,
    budget: Option<Budget>,
    name: Option<String>,
    run: Option<String>,
    dispatched: bool,
    open: HashMap<(&'static str, u32), (u64, Amount)>,
}

impl RunBudget {
    /// Caps a script cannot raise, and the home for named ledgers.
    pub fn configure(&mut self, caps: Limits, home: Option<PathBuf>) {
        self.caps = caps;
        self.home = home;
    }

    /// Ledger records name the run: the journal's id, or one made up when
    /// the journal is off, so two runs never share reserve ids.
    fn run_id(&mut self, journal: Option<&Journal>) -> String {
        self.run
            .get_or_insert_with(|| {
                journal
                    .map(|j| j.run_id().to_string())
                    .filter(|id| !id.is_empty())
                    .unwrap_or_else(|| {
                        format!("{}-p{}", crate::journal::run_id_now(), std::process::id())
                    })
            })
            .clone()
    }

    /// Applies a `budget()` call and returns what remains. Limits only go
    /// down: a script can lower its budget, never raise it above the caps
    /// or an earlier call.
    pub fn request(&mut self, name: Option<&str>, limits: Limits) -> Result<Limits> {
        let invalid = |message: String| {
            budget_error(
                "INVALID",
                message,
                "Call budget() once, before the first model call or tool run, and use one name per run.",
            )
        };
        if self.budget.is_none() && self.dispatched {
            return Err(invalid(
                "budget() was called after a model call or tool run had started".to_string(),
            ));
        }
        match (name, self.name.as_deref()) {
            (Some(name), Some(current)) if name != current => {
                return Err(invalid(format!(
                    "this run already uses the budget '{current}'"
                )));
            }
            (Some(name), None) if self.budget.is_some() => {
                return Err(invalid(format!(
                    "the budget '{name}' must be named in the first budget() call"
                )));
            }
            (Some(name), None) => {
                let Some(home) = self.home.as_deref() else {
                    return Err(invalid(format!(
                        "the budget '{name}' needs a ledger directory; `vaern run` provides one"
                    )));
                };
                let budget = Budget::named(home, name, lowered(self.caps, limits))?;
                self.budget = Some(budget);
                self.name = Some(name.to_string());
            }
            _ => match self.budget.as_mut() {
                Some(budget) => budget.lower_to(lowered(budget.limits(), limits)),
                None => self.budget = Some(Budget::new(lowered(self.caps, limits))),
            },
        }
        Ok(self.remaining())
    }

    /// What remains; `None` per unit without a limit.
    pub fn remaining(&self) -> Limits {
        match &self.budget {
            Some(budget) => budget.remaining(),
            None => self.caps,
        }
    }

    /// Reserves `amount` for call `req_id` of `kind` before it is
    /// dispatched. Without a budget or caps there is nothing to reserve.
    pub fn reserve(
        &mut self,
        journal: Option<&mut Journal>,
        kind: &'static str,
        req_id: u32,
        amount: Amount,
    ) -> Result<()> {
        self.dispatched = true;
        if self.budget.is_none() {
            if self.caps == Limits::default() {
                return Ok(());
            }
            self.budget = Some(Budget::new(self.caps));
        }
        let run = self.run_id(journal.as_deref());
        let Some(budget) = self.budget.as_mut() else {
            return Ok(());
        };
        let rid = budget.reserve(journal, &run, kind, amount)?;
        self.open.insert((kind, req_id), (rid, amount));
        Ok(())
    }

    /// Settles call `req_id` with its actual charge. `None`, for a reply
    /// without usage or a failed call, keeps the whole reserve charged.
    pub fn settle(
        &mut self,
        journal: Option<&mut Journal>,
        kind: &'static str,
        req_id: u32,
        charge: Option<Amount>,
    ) -> Result<()> {
        let Some((rid, reserved)) = self.open.remove(&(kind, req_id)) else {
            return Ok(());
        };
        let run = self.run_id(journal.as_deref());
        match self.budget.as_mut() {
            Some(budget) => budget.settle(journal, &run, rid, charge.unwrap_or(reserved)),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        nano: Some(1000),
        tokens: Some(100),
    };

    fn amount(nano: u64, tokens: u64) -> Amount {
        Amount { nano, tokens }
    }

    fn refusal(err: VaernError) -> (String, Option<String>, u64, u64) {
        match err {
            VaernError::Budget {
                code,
                unit,
                reserve,
                remaining,
                ..
            } => (code, unit, reserve, remaining),
            other => panic!("not a budget error: {other}"),
        }
    }

    #[test]
    fn test_reserves_count_until_settled_and_overruns_carry_forward() {
        let mut budget = Budget::new(LIMITS);
        let first = budget
            .reserve(None, "r", "model.call", amount(600, 40))
            .unwrap();
        assert_eq!(
            budget.remaining(),
            Limits {
                nano: Some(400),
                tokens: Some(60)
            }
        );
        let err = budget
            .reserve(None, "r", "model.call", amount(401, 1))
            .unwrap_err();
        assert_eq!(
            refusal(err),
            (
                "RESERVE_EXCEEDS_REMAINING".into(),
                Some("usd".into()),
                401,
                400
            )
        );
        let err = budget
            .reserve(None, "r", "model.call", amount(1, 61))
            .unwrap_err();
        assert_eq!(refusal(err).1, Some("tokens".into()));

        // The estimate ran low: the actual charge stands.
        budget.settle(None, "r", first, amount(700, 30)).unwrap();
        assert_eq!(
            budget.remaining(),
            Limits {
                nano: Some(300),
                tokens: Some(70)
            }
        );
        budget.settle(None, "r", first, amount(700, 30)).unwrap();
        assert_eq!(budget.remaining().nano, Some(300), "settled twice");
        assert!(
            budget
                .reserve(None, "r", "tool.call", amount(300, 0))
                .is_ok()
        );

        let unlimited = Budget::new(Limits::default());
        assert_eq!(unlimited.remaining(), Limits::default());
    }

    #[test]
    fn test_named_ledger_carries_spending_and_counts_unsettled_reserves() {
        let home = tempfile::tempdir().unwrap();
        {
            let mut budget = Budget::named(home.path(), "nightly", LIMITS).unwrap();
            let settled = budget
                .reserve(None, "run1", "model.call", amount(500, 50))
                .unwrap();
            budget
                .settle(None, "run1", settled, amount(200, 20))
                .unwrap();
            // Crashes before this one settles.
            budget
                .reserve(None, "run1", "model.call", amount(300, 10))
                .unwrap();

            let busy = Budget::named(home.path(), "nightly", LIMITS).err().unwrap();
            assert_eq!(refusal(busy).0, "LEDGER_BUSY");
        }
        let budget = Budget::named(home.path(), "nightly", LIMITS).unwrap();
        assert_eq!(
            budget.remaining(),
            Limits {
                nano: Some(500),
                tokens: Some(70)
            }
        );
        let ledger = home.path().join("budgets/nightly.jsonl");
        assert_eq!(crate::journal::verify(&ledger).unwrap().records, 3);

        let other = Budget::named(home.path(), "other", LIMITS).unwrap();
        assert_eq!(other.remaining(), LIMITS);
        let bad = Budget::named(home.path(), "../x", LIMITS).err().unwrap();
        assert_eq!(refusal(bad).0, "INVALID");
    }

    #[test]
    fn test_run_budgets_only_go_down_and_must_come_first() {
        let caps = Limits {
            nano: Some(500),
            tokens: None,
        };
        let mut run = RunBudget::default();
        run.configure(caps, None);
        let asked = Limits {
            nano: Some(9000),
            tokens: Some(50),
        };
        assert_eq!(
            run.request(None, asked).unwrap(),
            Limits {
                nano: Some(500),
                tokens: Some(50)
            }
        );
        let raised = Limits {
            nano: Some(800),
            tokens: Some(40),
        };
        assert_eq!(run.request(None, raised).unwrap().nano, Some(500));
        assert_eq!(run.remaining().tokens, Some(40));
        let named = run.request(Some("late"), Limits::default()).unwrap_err();
        assert_eq!(refusal(named).0, "INVALID");

        let mut capped = RunBudget::default();
        capped.configure(caps, None);
        capped
            .reserve(None, "model.call", 1, amount(400, 0))
            .unwrap();
        assert_eq!(
            refusal(
                capped
                    .reserve(None, "model.call", 2, amount(101, 0))
                    .unwrap_err()
            )
            .0,
            "RESERVE_EXCEEDS_REMAINING"
        );
        // No usage reported: the reserve stays charged.
        capped.settle(None, "model.call", 1, None).unwrap();
        assert_eq!(capped.remaining().nano, Some(100));
        // The caps applied at the first reserve, so budget() may still lower them.
        assert!(capped.request(None, caps).is_ok());

        let mut late = RunBudget::default();
        late.reserve(None, "model.call", 1, amount(1, 1)).unwrap();
        assert_eq!(refusal(late.request(None, caps).unwrap_err()).0, "INVALID");
    }

    #[test]
    fn test_a_torn_ledger_line_is_dropped_and_an_edited_one_refused() {
        let home = tempfile::tempdir().unwrap();
        {
            let mut budget = Budget::named(home.path(), "b", LIMITS).unwrap();
            budget
                .reserve(None, "r", "model.call", amount(100, 1))
                .unwrap();
        }
        let ledger = home.path().join("budgets/b.jsonl");
        let mut text = std::fs::read_to_string(&ledger).unwrap();
        text.push_str("{\"h\":\"00");
        std::fs::write(&ledger, &text).unwrap();
        let budget = Budget::named(home.path(), "b", LIMITS).unwrap();
        assert_eq!(budget.remaining().nano, Some(900));
        drop(budget);
        assert!(!crate::journal::verify(&ledger).unwrap().torn_tail);

        let edited = std::fs::read_to_string(&ledger)
            .unwrap()
            .replace("\"nano_usd\":100", "\"nano_usd\":1");
        std::fs::write(&ledger, edited).unwrap();
        let err = Budget::named(home.path(), "b", LIMITS).err().unwrap();
        assert!(matches!(err, VaernError::JournalTampered { .. }), "{err}");
    }
}
