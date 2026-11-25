//! `agent()` over MCP handles, and what a spawned server may not do, run
//! through the real binary against a local OpenAI-compatible stub.
//!
//! The gate: an agent whose tools are a connection's handles has every
//! model call, connect, call, close and budget record in the caller's
//! journal, the tool runs in the server's journal, both journals verify,
//! and a replay of the caller reproduces it with no server and no request.
//!
//! Enforcement: a served script cannot read or write the journal home, and
//! cannot make a model call the caller did not grant budget for. A raw
//! `fetch()` to a model URL is journalled but not metered, in a server as
//! anywhere else; that gap is pinned here so it is not mistaken for a
//! guarantee.

use serde_json::{Value, json};
use std::error::Error;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

type TestResult = Result<(), Box<dyn Error>>;
/// A run's id and its journal's records.
type Run = (String, Vec<Value>);

/// One run at a time; see `mcp_serve.rs` for why children are started so.
static RUN: Mutex<()> = Mutex::new(());

/// A chat completions server. A chat that carries a tool result is answered
/// "17 + 25 = 42.", one that offers tools is asked to call `add`, and any
/// other is answered "pong". Every answer reports 40 input and 12 output
/// tokens.
struct Stub {
    url: String,
    asked: Arc<AtomicUsize>,
}

fn stub() -> Result<Stub, Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let asked = Arc::new(AtomicUsize::new(0));
    let count = asked.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            count.fetch_add(1, Ordering::SeqCst);
            let _ = answer(stream);
        }
    });
    Ok(Stub { url, asked })
}

fn answer(mut stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let request: Value = serde_json::from_slice(&body).unwrap_or_default();
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let offered = request["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty());
    let (message, finish) = if messages.iter().any(|m| m["role"] == "tool") {
        (
            json!({ "role": "assistant", "content": "17 + 25 = 42." }),
            "stop",
        )
    } else if offered {
        let call = json!({ "id": "call_1", "type": "function",
                           "function": { "name": "add", "arguments": "{\"a\":17,\"b\":25}" } });
        (
            json!({ "role": "assistant", "content": null, "tool_calls": [call] }),
            "tool_calls",
        )
    } else {
        (json!({ "role": "assistant", "content": "pong" }), "stop")
    };
    let reply = json!({ "choices": [{ "message": message, "finish_reason": finish }],
                        "usage": { "prompt_tokens": 40, "completion_tokens": 12 } })
    .to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
        reply.len()
    )?;
    stream.flush()
}

/// A served script that probes the journal home.
const GUARDED: &str = r#"
import { tool } from "vaern:ai";
import { server } from "vaern:mcp";
import { readFile, writeFile } from "vaern:fs";
const probe = tool({ name: "probe", run: async () => {
  const out = [];
  for (const attempt of [() => writeFile("home/planted.txt", "x"), () => readFile("home/runs/latest", "utf8")]) {
    try { await attempt(); out.push("allowed"); } catch (err) { out.push(err.code); }
  }
  return { out };
} });
server({ name: "guarded", version: "1.0.0", tools: [probe] });
"#;

/// A served script that asks a model through `vaern:ai`, and through a raw
/// `fetch()`.
const ASKER: &str = r#"
import { BudgetError, model, tool } from "vaern:ai";
import { server } from "vaern:mcp";
const url = process.env.VAERN_TEST_MODEL_URL;
const local = model({ baseUrl: url, model: "stub", price: { inputPerMTok: 1, outputPerMTok: 2 } });
const ask = tool({ name: "ask", run: async () => {
  try {
    return { text: (await local.chat({ messages: [{ role: "user", content: "ping" }], maxTokens: 16 })).text };
  } catch (err) {
    if (err instanceof BudgetError) return { refused: err.code };
    throw err;
  }
} });
const raw = tool({ name: "raw", run: async () => {
  const body = JSON.stringify({ model: "stub", messages: [{ role: "user", content: "ping" }] });
  const reply = await fetch(`${url}/chat/completions`, { method: "POST", body, headers: { "content-type": "application/json" } });
  return { text: (await reply.json()).choices[0].message.content };
} });
server({ name: "asker", version: "1.0.0", tools: [ask, raw] });
"#;

fn workspace(test: &str, client: &str) -> Result<PathBuf, Box<dyn Error>> {
    let dir = std::env::temp_dir().join(format!("vaern-agent-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let sandbox = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/sandbox_run");
    std::fs::copy(sandbox.join("mcp_server.ts"), dir.join("mcp_server.ts"))?;
    std::fs::write(dir.join("guarded.ts"), GUARDED)?;
    std::fs::write(dir.join("asker.ts"), ASKER)?;
    std::fs::write(dir.join("client.ts"), client)?;
    Ok(dir)
}

/// Runs `vaern <args>` in `dir` with its own home, telling servers the stub.
fn vaern(dir: &Path, args: &[&str], stub: &Stub) -> Result<Output, Box<dyn Error>> {
    let _one_at_a_time = RUN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    Ok(Command::new(env!("CARGO_BIN_EXE_vaern"))
        .args(args)
        .current_dir(dir)
        .env("VAERN_HOME", dir.join("home"))
        .env("VAERN_TEST_MODEL_URL", &stub.url)
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

/// Every run under `dir`'s home, as (run id, records), the caller first and
/// the servers in the order they started.
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
    runs.sort_by_key(|(_, records)| {
        let served = records[0].get("mcp").is_some();
        (served, records[0]["at_us"].as_u64().unwrap_or_default())
    });
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
fn an_agent_over_mcp_handles_is_journalled_metered_and_replayed() -> TestResult {
    let stub = stub()?;
    let dir = workspace(
        "gate",
        r#"
        import { agent, budget, model } from "vaern:ai";
        import { connect } from "vaern:mcp";
        const local = model({ baseUrl: process.argv[2], model: "stub", price: { inputPerMTok: 1, outputPerMTok: 2 } });
        const calc = await connect({ script: "./mcp_server.ts", budget: { usd: 0.01 }, cost: { add: { usd: 0.001 } } });
        const out = await agent({ model: local, tools: await calc.tools(), maxSteps: 4 }).run("What is 17 + 25?");
        console.log(out.text, out.steps.map((s) => s.type).join(" > "));
        await calc.close();
        console.log(JSON.stringify(budget.remaining()));
        "#,
    )?;
    let recorded = vaern(
        &dir,
        &["run", "--budget-usd", "1", "client.ts", &stub.url],
        &stub,
    )?;
    assert_eq!(
        recorded.status.code(),
        Some(0),
        "{}",
        text(&recorded.stderr)
    );
    // Two model calls at 64 micro-USD each and one add at its declared
    // 0.001; the server spent nothing of its grant.
    assert_eq!(
        text(&recorded.stdout),
        "17 + 25 = 42. model > tool > model\n{\"usd\":0.998872,\"tokens\":null}\n"
    );
    assert_eq!(stub.asked.load(Ordering::SeqCst), 2);

    let recorded_runs = runs(&dir)?;
    assert_eq!(recorded_runs.len(), 2, "one caller and one server run");
    let (caller_id, caller) = &recorded_runs[0];
    let effects: Vec<&str> = kinds(caller, &["model.", "mcp.", "budget."])
        .iter()
        .map(|r| r["kind"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        effects,
        [
            "budget.reserve",
            "mcp.connect",
            "budget.reserve",
            "model.call",
            "budget.settle",
            "budget.reserve",
            "budget.settle",
            "mcp.call",
            "budget.reserve",
            "model.call",
            "budget.settle",
            "budget.settle",
            "mcp.close"
        ]
    );
    let reserved: Vec<&Value> = kinds(caller, &["budget.reserve"])
        .iter()
        .map(|r| &r["for"])
        .collect();
    assert_eq!(
        reserved,
        ["mcp.connect", "model.call", "mcp.call", "model.call"]
    );
    let (server_id, server) = &recorded_runs[1];
    let calls = kinds(server, &["tool.call"]);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["req"]["args"], json!({ "a": 17, "b": 25 }));

    for id in [caller_id, server_id] {
        let verified = vaern(&dir, &["journal", "verify", id], &stub)?;
        assert_eq!(
            verified.status.code(),
            Some(0),
            "{id}: {}",
            text(&verified.stderr)
        );
    }
    let replayed = vaern(&dir, &["replay", caller_id], &stub)?;
    assert_eq!(
        replayed.status.code(),
        Some(0),
        "{}",
        text(&replayed.stderr)
    );
    assert_eq!(text(&replayed.stdout), text(&recorded.stdout));
    assert_eq!(
        stub.asked.load(Ordering::SeqCst),
        2,
        "the replay sent no request"
    );
    assert_eq!(runs(&dir)?.len(), 2, "the replay started no server");
    Ok(())
}

#[test]
fn a_served_script_cannot_read_or_write_the_journal_home() -> TestResult {
    let stub = stub()?;
    let dir = workspace(
        "guarded",
        r#"
        import { connect } from "vaern:mcp";
        const guarded = await connect({ script: "./guarded.ts" });
        const [probe] = await guarded.tools();
        console.log(JSON.stringify(await probe.call({})));
        await guarded.close();
        "#,
    )?;
    let output = vaern(&dir, &["run", "client.ts"], &stub)?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "{\"out\":[\"EPERM\",\"EPERM\"]}\n");
    assert!(!dir.join("home/planted.txt").exists());
    for (id, _) in runs(&dir)? {
        let verified = vaern(&dir, &["journal", "verify", &id], &stub)?;
        assert_eq!(
            verified.status.code(),
            Some(0),
            "{id}: {}",
            text(&verified.stderr)
        );
    }
    Ok(())
}

#[test]
fn a_served_script_spends_only_the_grant_it_was_given() -> TestResult {
    let stub = stub()?;
    let dir = workspace(
        "grant",
        r#"
        import { budget } from "vaern:ai";
        import { connect } from "vaern:mcp";
        // The default grant is nothing. A server that calls models needs
        // tokens granted as well as USD.
        for (const grant of [undefined, { usd: 0.01, tokens: 1000 }]) {
          const asker = await connect({ script: "./asker.ts", budget: grant });
          const [ask, raw] = await asker.tools();
          console.log(grant?.usd ?? 0, JSON.stringify(await ask.call({})), JSON.stringify(await raw.call({})));
          await asker.close();
        }
        console.log(JSON.stringify(budget.remaining()));
        "#,
    )?;
    let output = vaern(&dir, &["run", "--budget-usd", "1", "client.ts"], &stub)?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    // The raw fetch is answered both times: it is not metered.
    assert_eq!(
        text(&output.stdout),
        concat!(
            "0 {\"refused\":\"RESERVE_EXCEEDS_REMAINING\"} {\"text\":\"pong\"}\n",
            "0.01 {\"text\":\"pong\"} {\"text\":\"pong\"}\n",
            "{\"usd\":0.999936,\"tokens\":null}\n",
        )
    );
    // Refused before sending: only the second server's model call and the
    // two raw fetches reached the stub.
    assert_eq!(stub.asked.load(Ordering::SeqCst), 3);

    let runs = runs(&dir)?;
    assert_eq!(runs.len(), 3, "one caller and two server runs");
    let (_, caller) = &runs[0];
    let grants: Vec<(&Value, &Value)> = kinds(caller, &["budget."])
        .iter()
        .map(|r| (&r["kind"], &r["nano_usd"]))
        .collect();
    assert_eq!(
        grants,
        [
            (&"budget.reserve".into(), &0.into()),
            (&"budget.settle".into(), &0.into()),
            (&"budget.reserve".into(), &10_000_000.into()),
            (&"budget.settle".into(), &64_000.into()),
        ]
    );
    for (_, server) in &runs[1..] {
        assert_eq!(
            kinds(server, &["fetch"]).len(),
            1,
            "the raw fetch is journalled"
        );
        let metered: Vec<&Value> = kinds(server, &["budget.reserve"])
            .iter()
            .map(|r| &r["for"])
            .collect();
        assert!(
            metered.iter().all(|kind| *kind == "model.call"),
            "{metered:?}"
        );
    }
    assert_eq!(kinds(&runs[1].1, &["model.call"]).len(), 0);
    assert_eq!(kinds(&runs[2].1, &["model.call"]).len(), 1);
    Ok(())
}
