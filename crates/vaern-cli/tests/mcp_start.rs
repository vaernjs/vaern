//! How long `connect()` may wait for a server to start, run by the real
//! binary against servers that stop answering part way through.
//!
//! The whole start runs to one deadline, the connection's `timeoutMs` and
//! never less than 5 s. These pin down that a server which answers
//! `server/discover` and then stops fails the start at `tools_list`, that
//! one which never answers fails it at `negotiate`, both as `McpError`
//! stage `timeout` within the deadline plus a little, and that the failed
//! connect is recorded and replays without starting a server.

use serde_json::Value;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;
use std::time::{Duration, Instant};

type TestResult = Result<(), Box<dyn Error>>;
/// A run's id and its journal's records.
type Run = (String, Vec<Value>);

/// One run at a time; see `mcp_serve.rs` for why children are started so.
static RUN: Mutex<()> = Mutex::new(());

/// What a start may take beyond its deadline: killing the server and
/// ending the caller's run.
const SLACK: Duration = Duration::from_secs(3);

/// Answers `server/discover`, then never runs again. The busy wait holds
/// the script's top level until the probe is queued, and the loop turn
/// that answers it runs its timers next, before `tools/list` can arrive.
const AFTER_PROBE: &str = r#"
import { tool } from "vaern:ai";
import { server } from "vaern:mcp";
server({ name: "stuck", version: "1.0.0", tools: [tool({ name: "ping", run: () => ({}) })] });
const until = Date.now() + 200; while (Date.now() < until) {}
setTimeout(() => { for (;;) {} }, 0);
"#;

/// Declares a server and never returns from its top level.
const BEFORE_PROBE: &str = r#"
import { tool } from "vaern:ai";
import { server } from "vaern:mcp";
server({ name: "stuck", version: "1.0.0", tools: [tool({ name: "ping", run: () => ({}) })] });
for (;;) {}
"#;

/// Connects with `timeoutMs` and prints how the start failed.
fn client(script: &str, timeout_ms: u64) -> String {
    format!(
        r#"
        import {{ connect }} from "vaern:mcp";
        try {{
            await connect({{ script: "./{script}", timeoutMs: {timeout_ms} }});
            console.log("connected");
        }} catch (err) {{
            console.log(err.name, err.stage, err.code, err.message.match(/step \w+/)?.[0]);
        }}
        "#
    )
}

fn workspace(test: &str, client: &str) -> Result<PathBuf, Box<dyn Error>> {
    let dir = std::env::temp_dir().join(format!("vaern-start-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("after_probe.ts"), AFTER_PROBE)?;
    std::fs::write(dir.join("before_probe.ts"), BEFORE_PROBE)?;
    std::fs::write(dir.join("client.ts"), client)?;
    Ok(dir)
}

/// Runs the binary and says how long it ran, timed once this test holds
/// the one-at-a-time lock, so another test's run is not counted.
fn vaern(dir: &Path, args: &[&str]) -> Result<(Output, Duration), Box<dyn Error>> {
    let _one_at_a_time = RUN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let began = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_vaern"))
        .args(args)
        .current_dir(dir)
        .env("VAERN_HOME", dir.join("home"))
        .output()?;
    Ok((output, began.elapsed()))
}

/// Every run's id and records under `dir`'s home, the caller first.
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
            let mut records = Vec::new();
            for line in std::fs::read_to_string(&path)?.lines() {
                let framed: Value = serde_json::from_str(line)?;
                records.push(framed["r"].clone());
            }
            runs.push((id, records));
        }
    }
    runs.sort_by_key(|(_, records)| records[0].get("mcp").is_some());
    Ok(runs)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// Runs the client, checks it failed with `expected` no sooner than
/// `deadline` and no later than `deadline` plus [`SLACK`], and that the
/// failed connect is recorded and replays without starting a server.
fn fails_in_time(
    test: &str,
    script: &str,
    timeout_ms: u64,
    deadline: Duration,
    expected: &str,
) -> TestResult {
    let dir = workspace(test, &client(script, timeout_ms))?;
    let (output, took) = vaern(&dir, &["run", "client.ts"])?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), expected);
    assert!(
        took >= deadline && took < deadline + SLACK,
        "the start failed after {took:?}, deadline {deadline:?}"
    );

    let recorded = runs(&dir)?;
    assert_eq!(recorded.len(), 2, "one caller and one server run");
    let connects: Vec<&Value> = recorded[0]
        .1
        .iter()
        .filter(|r| r["kind"] == "mcp.connect")
        .collect();
    assert_eq!(connects.len(), 1);
    assert_eq!(connects[0]["error"]["code"], "TIMEOUT");

    let (replayed, _) = vaern(&dir, &["replay", &recorded[0].0])?;
    assert_eq!(
        replayed.status.code(),
        Some(0),
        "{}",
        text(&replayed.stderr)
    );
    assert_eq!(text(&replayed.stdout), expected);
    assert_eq!(runs(&dir)?.len(), 2, "the replay started no server");
    Ok(())
}

#[test]
fn a_server_that_stops_after_the_probe_fails_the_start_at_its_timeout() -> TestResult {
    fails_in_time(
        "after-probe",
        "after_probe.ts",
        6000,
        Duration::from_millis(6000),
        "McpError timeout TIMEOUT step tools_list\n",
    )
}

#[test]
fn a_server_that_never_answers_fails_the_start_at_the_floor() -> TestResult {
    fails_in_time(
        "before-probe",
        "before_probe.ts",
        1000,
        Duration::from_secs(5),
        "McpError timeout TIMEOUT step negotiate\n",
    )
}
