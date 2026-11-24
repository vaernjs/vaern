//! Drives `vaern mcp serve` as an MCP client would: JSON-RPC lines on the
//! child's stdin, one reply line per request on its stdout.
//!
//! What these pin down is the process contract, which unit tests on the
//! protocol cannot see: stdout carries nothing but protocol lines, both
//! supported versions are served by the same binary, an older version is
//! refused with the spec's error, and closing stdin ends the server cleanly.

use serde_json::{Value, json};
use std::error::Error;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

type TestResult = Result<(), Box<dyn Error>>;

const N: &str = "2026-07-28";
const N1: &str = "2025-11-25";

fn sandbox_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/sandbox_run")
}

fn journal_home(test: &str) -> Result<PathBuf, Box<dyn Error>> {
    let dir = std::env::temp_dir().join(format!("vaern-mcp-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// A running `vaern mcp serve mcp_server.ts` and the lines it has written.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Server {
    fn start(home: &std::path::Path) -> Result<Server, Box<dyn Error>> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_vaern"))
            .args(["mcp", "serve", "mcp_server.ts"])
            .current_dir(sandbox_dir())
            .env("VAERN_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Ok(Server {
            child,
            stdin,
            lines,
        })
    }

    /// Sends one request and returns the reply, which must carry its id.
    fn ask(&mut self, id: u64, method: &str, params: Value) -> Result<Value, Box<dyn Error>> {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let line = self.lines.recv_timeout(Duration::from_secs(20))?;
        let reply: Value = serde_json::from_str(&line)
            .map_err(|err| format!("stdout line is not JSON ({err}): {line}"))?;
        assert_eq!(reply["id"], id, "{reply}");
        Ok(reply)
    }

    fn send(&mut self, message: &Value) -> TestResult {
        let stdin = self.stdin.as_mut().ok_or("stdin closed")?;
        writeln!(stdin, "{message}")?;
        stdin.flush()?;
        Ok(())
    }

    /// Closes stdin, as a client does at shutdown, and collects the rest.
    fn finish(mut self) -> Result<(Output, Vec<String>), Box<dyn Error>> {
        drop(self.stdin.take());
        let output = self.child.wait_with_output()?;
        Ok((output, self.lines.try_iter().collect()))
    }
}

fn modern(mut params: Value, version: &str) -> Value {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    params
}

#[test]
fn a_modern_client_discovers_lists_and_calls() -> TestResult {
    let home = journal_home("modern")?;
    let mut server = Server::start(&home)?;

    let discovered = server.ask(1, "server/discover", modern(json!({}), N))?;
    let result = &discovered["result"];
    assert_eq!(result["supportedVersions"], json!([N, N1]), "{discovered}");
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "calc"
    );

    let listed = server.ask(2, "tools/list", modern(json!({}), N))?;
    let tools = listed["result"]["tools"].as_array().ok_or("no tools")?;
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(names, ["add", "fail"]);
    assert_eq!(tools[0]["inputSchema"]["required"], json!(["a", "b"]));

    let call = |name: &str, args: Value| modern(json!({ "name": name, "arguments": args }), N);
    let added = server.ask(3, "tools/call", call("add", json!({ "a": 2, "b": 3 })))?;
    assert_eq!(added["result"]["structuredContent"], json!({ "sum": 5 }));
    assert_eq!(added["result"]["isError"], false);
    assert_eq!(added["result"]["resultType"], "complete");

    let failed = server.ask(4, "tools/call", call("fail", json!({})))?;
    assert_eq!(failed["result"]["isError"], true, "{failed}");
    assert!(
        failed["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains("no luck")),
        "{failed}"
    );

    let bad = server.ask(5, "tools/call", call("add", json!({ "a": "two" })))?;
    assert_eq!(bad["error"]["code"], -32602, "{bad}");
    let unknown = server.ask(6, "tools/call", call("nope", json!({})))?;
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");

    let (output, rest) = server.finish()?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(rest.is_empty(), "unexpected stdout: {rest:?}");
    // console.log in a tool went to stderr and left stdout to the protocol.
    assert!(text(&output.stderr).contains("adding 2 and 3"));
    Ok(())
}

#[test]
fn a_2025_11_25_client_initializes_and_calls() -> TestResult {
    let home = journal_home("legacy")?;
    let mut server = Server::start(&home)?;

    let init = server.ask(
        1,
        "initialize",
        json!({ "protocolVersion": N1, "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" } }),
    )?;
    assert_eq!(init["result"]["protocolVersion"], N1, "{init}");
    assert_eq!(init["result"]["serverInfo"]["name"], "calc");
    server.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;

    let listed = server.ask(2, "tools/list", json!({}))?;
    assert_eq!(listed["result"]["tools"][0]["name"], "add", "{listed}");
    assert!(listed["result"].get("resultType").is_none());

    let added = server.ask(
        3,
        "tools/call",
        json!({ "name": "add", "arguments": { "a": 1, "b": 1 } }),
    )?;
    assert_eq!(added["result"]["structuredContent"], json!({ "sum": 2 }));

    let (output, _) = server.finish()?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    Ok(())
}

#[test]
fn an_older_protocol_version_is_refused_with_the_supported_list() -> TestResult {
    let home = journal_home("old")?;
    let mut server = Server::start(&home)?;
    let refused = server.ask(1, "tools/list", modern(json!({}), "2024-11-05"))?;
    assert_eq!(refused["error"]["code"], -32022, "{refused}");
    assert_eq!(refused["error"]["data"]["supported"], json!([N, N1]));
    assert_eq!(refused["error"]["data"]["requested"], "2024-11-05");
    let (output, _) = server.finish()?;
    assert_eq!(output.status.code(), Some(0));
    Ok(())
}

#[test]
fn server_is_refused_outside_mcp_serve() -> TestResult {
    let home = journal_home("plain")?;
    let output = Command::new(env!("CARGO_BIN_EXE_vaern"))
        .args(["run", "mcp_server.ts"])
        .current_dir(sandbox_dir())
        .env("VAERN_HOME", &home)
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    let stderr = text(&output.stderr);
    assert!(stderr.contains("McpError"), "{stderr}");
    assert!(stderr.contains("vaern mcp serve"), "{stderr}");
    Ok(())
}

#[test]
fn serving_a_script_that_declares_nothing_fails() -> TestResult {
    let home = journal_home("nothing")?;
    let output = Command::new(env!("CARGO_BIN_EXE_vaern"))
        .args(["mcp", "serve", "hello.ts"])
        .current_dir(sandbox_dir())
        .env("VAERN_HOME", &home)
        .stdin(Stdio::null())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("without calling server()"), "{stderr}");
    Ok(())
}

#[test]
fn a_served_run_is_not_replayed() -> TestResult {
    let home = journal_home("replay")?;
    let mut server = Server::start(&home)?;
    server.ask(1, "tools/list", modern(json!({}), N))?;
    server.finish()?;
    let replayed = Command::new(env!("CARGO_BIN_EXE_vaern"))
        .args(["replay", "latest"])
        .current_dir(sandbox_dir())
        .env("VAERN_HOME", &home)
        .output()?;
    assert_eq!(replayed.status.code(), Some(1));
    let stderr = text(&replayed.stderr);
    assert!(stderr.contains("MCP_SERVED"), "{stderr}");
    Ok(())
}
