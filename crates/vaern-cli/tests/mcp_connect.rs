//! `connect()` from a script run by the real binary: the server is a
//! second `vaern` process, and both runs keep journals under one home.
//!
//! These pin down that each connect, call and close is an effect in the
//! caller's journal, that the server's grant is reserved before it starts
//! and settled at what it reported, that a connection left open is closed
//! when the run ends, and that a refused connect starts nothing. The
//! lifecycle tests pin down that a crash fails the call in flight with a
//! typed error and is recorded, that the next call restarts the server at
//! most three times, and that a late or aborted call is cancelled while the
//! connection carries on. Each of these runs also replays without starting
//! a server.

use serde_json::Value;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;

type TestResult = Result<(), Box<dyn Error>>;
/// A run's id and its journal's records.
type Run = (String, Vec<Value>);

/// One run at a time; see `mcp_serve.rs` for why children are started so.
static RUN: Mutex<()> = Mutex::new(());

/// A server whose tools exit the process, answer late, or answer at once.
const FLAKY: &str = r#"
import { tool } from "vaern:ai";
import { server } from "vaern:mcp";
const crash = tool({ name: "crash", run: () => { console.error("going down"); process.exit(3); } });
const slow = tool({ name: "slow", run: () => new Promise((done) => setTimeout(() => done({ late: true }), 1000)) });
const ping = tool({ name: "ping", run: () => ({ pong: true }) });
server({ name: "flaky", version: "1.0.0", tools: [crash, slow, ping] });
"#;

fn workspace(test: &str, client: &str) -> Result<PathBuf, Box<dyn Error>> {
    let dir = std::env::temp_dir().join(format!("vaern-connect-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let sandbox = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/sandbox_run");
    std::fs::copy(sandbox.join("mcp_server.ts"), dir.join("mcp_server.ts"))?;
    std::fs::write(dir.join("flaky.ts"), FLAKY)?;
    std::fs::write(dir.join("client.ts"), client)?;
    Ok(dir)
}

fn run(dir: &Path, args: &[&str]) -> Result<Output, Box<dyn Error>> {
    let _one_at_a_time = RUN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    Ok(Command::new(env!("CARGO_BIN_EXE_vaern"))
        .arg("run")
        .args(args)
        .arg("client.ts")
        .current_dir(dir)
        .env("VAERN_HOME", dir.join("home"))
        .output()?)
}

fn records(journal: &Path) -> Result<Vec<Value>, Box<dyn Error>> {
    let text = std::fs::read_to_string(journal)?;
    let mut records = Vec::new();
    for line in text.lines() {
        let framed: Value = serde_json::from_str(line)?;
        records.push(framed["r"].clone());
    }
    Ok(records)
}

/// Every run under `dir`'s home, as (run id, records), the caller first.
fn runs(dir: &Path) -> Result<Vec<Run>, Box<dyn Error>> {
    let mut runs = Vec::new();
    for entry in std::fs::read_dir(dir.join("home/runs"))? {
        let path = entry?.path().join("journal.jsonl");
        if path.is_file() {
            let id = path
                .parent()
                .and_then(Path::file_name)
                .map(|id| id.to_string_lossy().into_owned())
                .unwrap_or_default();
            runs.push((id, records(&path)?));
        }
    }
    runs.sort_by_key(|(_, records)| records[0].get("mcp").is_some());
    Ok(runs)
}

fn kinds<'a>(records: &'a [Value], prefixes: &[&str]) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|r| {
            let kind = r["kind"].as_str().unwrap_or_default();
            prefixes.iter().any(|prefix| kind.starts_with(prefix))
        })
        .collect()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

#[test]
fn connects_calls_and_closes_with_every_effect_journalled_and_metered() -> TestResult {
    let dir = workspace(
        "metered",
        r#"
        import { connect } from "vaern:mcp";
        import { budget } from "vaern:ai";
        const calc = await connect({ script: "./mcp_server.ts", budget: { usd: 0.5 } });
        const [add, fail] = await calc.tools();
        console.log(calc.name, calc.protocolVersion, JSON.stringify(budget.remaining()));
        console.log(JSON.stringify(await add.call({ a: 2, b: 3 })));
        try { await fail.call({}); } catch (err) { console.log(`${err.name} ${err.stage}: ${err.message}`); }
        await calc.close();
        console.log(JSON.stringify(budget.remaining()));
        "#,
    )?;
    let output = run(&dir, &["--budget-usd", "1"])?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "calc 2026-07-28 {\"usd\":0.5,\"tokens\":null}\n{\"sum\":5}\nToolError run: Error: no luck\n{\"usd\":1,\"tokens\":null}\n"
    );

    let runs = runs(&dir)?;
    assert_eq!(runs.len(), 2, "one caller and one server run");
    let (_, caller) = &runs[0];
    let effects = kinds(caller, &["mcp.", "budget."]);
    let shape: Vec<&str> = effects
        .iter()
        .map(|r| r["kind"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        shape,
        [
            "budget.reserve",
            "mcp.connect",
            "mcp.call",
            "mcp.call",
            "budget.settle",
            "mcp.close"
        ]
    );
    // The grant is reserved before the server starts and settled at what it
    // reported, which is nothing: its tools make no model calls.
    assert_eq!(effects[0]["for"], "mcp.connect");
    assert_eq!(effects[0]["nano_usd"], 500_000_000);
    assert_eq!(effects[1]["req"]["budget"]["nano_usd"], 500_000_000);
    assert_eq!(effects[1]["res"]["protocol_version"], "2026-07-28");
    assert_eq!(effects[2]["req"]["tool"], "add");
    assert_eq!(effects[2]["res"]["structured"]["sum"], 5);
    assert_eq!(effects[3]["res"]["is_error"], true);
    assert_eq!(
        (&effects[4]["rid"], &effects[4]["nano_usd"]),
        (&0.into(), &0.into())
    );
    assert_eq!(effects[5]["res"]["exit_code"], 0);

    // The server's own run is named, served the calls as tool effects, and
    // ended before the caller did.
    let (server_id, server) = &runs[1];
    assert_eq!(effects[1]["res"]["server_run"], server_id.as_str());
    assert_eq!(effects[5]["res"]["server_run"], server_id.as_str());
    assert_eq!(kinds(server, &["tool.call"]).len(), 2);
    assert_eq!(server.last().map(|r| &r["kind"]), Some(&"run.end".into()));
    Ok(())
}

#[test]
fn a_connection_left_open_is_closed_when_the_run_ends() -> TestResult {
    let dir = workspace(
        "left-open",
        r#"
        import { connect } from "vaern:mcp";
        const calc = await connect({ script: "./mcp_server.ts" });
        const [add] = await calc.tools();
        console.log(JSON.stringify(await add.call({ a: 1, b: 1 })));
        "#,
    )?;
    let output = run(&dir, &[])?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "{\"sum\":2}\n");
    let runs = runs(&dir)?;
    let closes = kinds(&runs[0].1, &["mcp.close"]);
    assert_eq!(closes.len(), 1);
    assert_eq!(closes[0]["req"]["at"], "run.end");
    assert_eq!(closes[0]["res"]["exit_code"], 0);
    assert_eq!(
        runs[1].1.last().map(|r| &r["kind"]),
        Some(&"run.end".into())
    );
    Ok(())
}

#[test]
fn a_refused_connect_starts_nothing() -> TestResult {
    let dir = workspace(
        "refused",
        r#"
        import { connect } from "vaern:mcp";
        for (const options of [{ script: "./missing.ts" }, { script: "./mcp_server.ts", budget: { usd: 2 } }]) {
            try { await connect(options); } catch (err) { console.log(err.name, err.code); }
        }
        "#,
    )?;
    let output = run(&dir, &["--budget-usd", "1"])?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "McpError SCRIPT_NOT_FOUND\nBudgetError RESERVE_EXCEEDS_REMAINING\n"
    );
    let runs = runs(&dir)?;
    assert_eq!(runs.len(), 1, "no server was started");
    assert!(kinds(&runs[0].1, &["mcp."]).is_empty());
    Ok(())
}

fn replay(dir: &Path, run_id: &str) -> Result<Output, Box<dyn Error>> {
    let _one_at_a_time = RUN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    Ok(Command::new(env!("CARGO_BIN_EXE_vaern"))
        .args(["replay", run_id])
        .current_dir(dir)
        .env("VAERN_HOME", dir.join("home"))
        .output()?)
}

#[test]
fn a_replay_starts_no_server_and_diverges_when_the_script_changes() -> TestResult {
    let dir = workspace(
        "replay",
        r#"
        import { connect } from "vaern:mcp";
        import { budget } from "vaern:ai";
        const calc = await connect({ script: "./mcp_server.ts", budget: { usd: 0.25 } });
        const [add, fail] = await calc.tools();
        console.log(calc.name, calc.protocolVersion, calc.instructions ?? "", JSON.stringify(budget.remaining()));
        console.log(JSON.stringify(await add.call({ a: 4, b: 5 })));
        try { await fail.call({}); } catch (err) { console.log(`${err.name} ${err.stage}: ${err.message}`); }
        const left = await connect({ script: "./mcp_server.ts" });
        console.log((await left.tools()).length);
        await calc.close();
        console.log(JSON.stringify(budget.remaining()));
        "#,
    )?;
    let recorded = run(&dir, &["--budget-usd", "1"])?;
    assert_eq!(
        recorded.status.code(),
        Some(0),
        "{}",
        text(&recorded.stderr)
    );
    let before = runs(&dir)?;
    assert_eq!(before.len(), 3, "one caller and two server runs");
    let caller = before[0].0.clone();

    let replayed = replay(&dir, &caller)?;
    assert_eq!(
        replayed.status.code(),
        Some(0),
        "{}",
        text(&replayed.stderr)
    );
    assert_eq!(text(&replayed.stdout), text(&recorded.stdout));
    assert!(
        text(&replayed.stderr).contains("reproduced: 5 console records, exit code 0"),
        "{}",
        text(&replayed.stderr)
    );
    assert_eq!(runs(&dir)?.len(), 3, "the replay started no server");

    // The server script is pinned by its digest: a changed server is a
    // divergence at the connect, not a silent replay of a stale answer.
    let server = dir.join("mcp_server.ts");
    let source = std::fs::read_to_string(&server)?;
    std::fs::write(&server, format!("{source}\n// changed\n"))?;
    let diverged = replay(&dir, &caller)?;
    assert_eq!(diverged.status.code(), Some(1));
    assert_eq!(text(&diverged.stdout), "");
    let stderr = text(&diverged.stderr);
    assert!(stderr.contains("diverged at record"), "{stderr}");
    assert!(stderr.contains("expected mcp.connect"), "{stderr}");
    assert_eq!(runs(&dir)?.len(), 3, "a diverged replay started no server");
    Ok(())
}

/// Replays the caller's run and checks it reproduced without a server.
fn replays_alone(dir: &Path, recorded: &Output) -> TestResult {
    let before = runs(dir)?;
    let replayed = replay(dir, &before[0].0)?;
    assert_eq!(
        replayed.status.code(),
        Some(0),
        "{}",
        text(&replayed.stderr)
    );
    assert_eq!(text(&replayed.stdout), text(&recorded.stdout));
    assert_eq!(
        runs(dir)?.len(),
        before.len(),
        "the replay started no server"
    );
    Ok(())
}

#[test]
fn a_crashed_server_fails_its_call_and_restarts_at_most_three_times() -> TestResult {
    let dir = workspace(
        "crash",
        r#"
        import { connect } from "vaern:mcp";
        const flaky = await connect({ script: "./flaky.ts" });
        const [crash, , ping] = await flaky.tools();
        try { await crash.call({}); } catch (err) {
            console.log(err.name, err.stage, err.code, err.exitCode, JSON.stringify(err.stderr));
        }
        console.log(JSON.stringify(await ping.call({})), flaky.restarts);
        for (let i = 0; i < 3; i++) { try { await crash.call({}); } catch (err) { console.log(err.code, flaky.restarts); } }
        try { await ping.call({}); } catch (err) { console.log(err.name, err.stage, err.code, err.exitCode); }
        await flaky.close();
        "#,
    )?;
    let output = run(&dir, &[])?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        concat!(
            "McpError exited SERVER_EXITED 3 \"going down\\n\"\n",
            "{\"pong\":true} 1\n",
            "SERVER_EXITED 1\nSERVER_EXITED 2\nSERVER_EXITED 3\n",
            "McpError exited RESTART_LIMIT 3\n",
        )
    );
    let runs = runs(&dir)?;
    assert_eq!(runs.len(), 5, "one caller and four server starts");
    let effects = kinds(&runs[0].1, &["mcp."]);
    let shape: Vec<&str> = effects
        .iter()
        .map(|r| r["kind"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        shape,
        [
            "mcp.connect",
            "mcp.call",
            "mcp.exit",
            "mcp.connect",
            "mcp.call",
            "mcp.call",
            "mcp.exit",
            "mcp.connect",
            "mcp.call",
            "mcp.exit",
            "mcp.connect",
            "mcp.call",
            "mcp.exit",
            "mcp.close"
        ]
    );
    // The call in flight is never sent again, and counts as sent.
    assert_eq!(effects[1]["error"]["code"], "SERVER_EXITED");
    assert_eq!(effects[1]["error"]["sent"], true);
    assert_eq!(effects[2]["res"]["exit_code"], 3);
    let restarts: Vec<&Value> = effects
        .iter()
        .filter(|r| r["kind"] == "mcp.connect")
        .map(|r| &r["req"]["restart"])
        .collect();
    assert_eq!(restarts, [&Value::Null, &1.into(), &2.into(), &3.into()]);
    // The fifth call was refused before anything started, so it has no record.
    assert_eq!(effects[13]["res"]["exit_code"], 3);
    replays_alone(&dir, &output)
}

#[test]
fn a_late_or_aborted_call_is_cancelled_and_the_connection_carries_on() -> TestResult {
    let dir = workspace(
        "late",
        r#"
        import { connect } from "vaern:mcp";
        const flaky = await connect({ script: "./flaky.ts", timeoutMs: 200 });
        const [, slow, ping] = await flaky.tools();
        try { await slow.call({}); } catch (err) { console.log(err.name, err.stage, err.code); }
        const stop = new AbortController();
        const aborted = slow.call({}, { signal: stop.signal });
        setTimeout(() => stop.abort(), 20);
        try { await aborted; } catch (err) { console.log(err.name); }
        console.log(JSON.stringify(await ping.call({})), flaky.restarts);
        await flaky.close();
        "#,
    )?;
    let output = run(&dir, &[])?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "McpError timeout TIMEOUT
AbortError
{\"pong\":true} 0
"
    );
    let runs = runs(&dir)?;
    let calls = kinds(&runs[0].1, &["mcp.call"]);
    let failed: Vec<(&Value, &Value)> = calls
        .iter()
        .map(|r| (&r["error"]["code"], &r["error"]["sent"]))
        .collect();
    assert_eq!(
        failed,
        [
            (&"TIMEOUT".into(), &true.into()),
            (&"ABORTED".into(), &true.into()),
            (&Value::Null, &Value::Null)
        ]
    );
    assert_eq!(
        runs[0].1.iter().filter(|r| r["kind"] == "mcp.exit").count(),
        0
    );
    replays_alone(&dir, &output)
}
