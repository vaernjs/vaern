#!/usr/bin/env node
/**
 * test-examples.mjs — automated test suite executing all repository examples.
 *
 * Runs each example program through the native Vaern runtime and asserts
 * that expected standard output is produced.
 */
import { execFile, execFileSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { promisify } from "node:util";

const repoRoot = path.resolve(import.meta.dirname, "..");

function findVaernBinary() {
  const candidates = [
    path.join(repoRoot, "target", "release", process.platform === "win32" ? "vaern.exe" : "vaern"),
    path.join(repoRoot, "target", "debug", process.platform === "win32" ? "vaern.exe" : "vaern"),
  ];

  for (const bin of candidates) {
    if (fs.existsSync(bin)) return bin;
  }
  return null;
}

const binary = findVaernBinary();
if (!binary) {
  console.error("test-examples: vaern runtime binary not found in target/release or target/debug");
  process.exit(1);
}

const tests = [
  {
    name: "hello.ts",
    args: ["run", path.join(repoRoot, "examples", "hello.ts")],
    expected: "Hello from Vaern!",
  },
  {
    name: "timers.ts",
    args: ["run", path.join(repoRoot, "examples", "timers.ts")],
    expected: "interval tick: 2",
  },
  {
    name: "modules/main.ts",
    args: ["run", path.join(repoRoot, "examples", "modules", "main.ts")],
    expected: "sum: 42",
  },
  {
    name: "fetch-json.ts",
    args: ["run", path.join(repoRoot, "examples", "fetch-json.ts")],
    expected: "fetched package: vaern v0.1.0 (hyper+rustls)",
  },
  {
    name: "echo-server.ts",
    args: ["run", path.join(repoRoot, "examples", "echo-server.ts"), "--test"],
    expected: "echo test response: hello vaern",
  },
  {
    name: "audit-demo.ts",
    args: ["run", path.join(repoRoot, "examples", "audit-demo.ts")],
    expected: '"status": "verified"',
  },
];

let failed = 0;
for (const t of tests) {
  try {
    const stdout = execFileSync(binary, t.args, {
      cwd: repoRoot,
      encoding: "utf8",
      timeout: 10000,
    });
    if (!stdout.includes(t.expected)) {
      console.error(`FAIL  ${t.name}: expected output containing "${t.expected}", received:\n${stdout}`);
      failed += 1;
    } else {
      console.log(`OK    ${t.name}`);
    }
  } catch (err) {
    console.error(`FAIL  ${t.name}: execution threw error: ${err.message}`);
    failed += 1;
  }
}

// agent-budget-replay.ts needs an OpenAI-compatible server. This stub asks
// for the add tool once, then answers. The run is then replayed with the
// stub closed: the replay must print the same output without a server.
async function agentBudgetReplay() {
  const server = http.createServer((req, res) => {
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => {
      const answered = JSON.parse(body).messages.some((m) => m.role === "tool");
      const call = { id: "call_1", type: "function", function: { name: "add", arguments: '{"a":17,"b":25}' } };
      const message = answered
        ? { role: "assistant", content: "17 + 25 = 42." }
        : { role: "assistant", content: null, tool_calls: [call] };
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify({
        choices: [{ message, finish_reason: answered ? "stop" : "tool_calls" }],
        usage: { prompt_tokens: 40, completion_tokens: 12 },
      }));
    });
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const baseUrl = `http://127.0.0.1:${server.address().port}/v1`;
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "vaern-examples-"));
  const options = { cwd: repoRoot, env: { ...process.env, VAERN_HOME: home }, encoding: "utf8", timeout: 10000 };
  const run = promisify(execFile);
  try {
    const demo = path.join(repoRoot, "examples", "agent-budget-replay.ts");
    const recorded = await run(binary, ["run", demo, baseUrl, "stub"], options);
    server.closeAllConnections();
    server.close();
    const expected = "answer: 17 + 25 = 42.\nsteps: model > tool > model\nrefused before sending: ";
    if (!recorded.stdout.startsWith(expected)) {
      throw new Error(`expected output starting "${expected}", received:\n${recorded.stdout}`);
    }
    const replayed = await run(binary, ["replay", "latest"], options);
    if (replayed.stdout !== recorded.stdout || !replayed.stderr.includes("reproduced")) {
      throw new Error(`replay printed:\n${replayed.stdout}${replayed.stderr}`);
    }
  } finally {
    server.close();
    fs.rmSync(home, { recursive: true, force: true });
  }
}

try {
  await agentBudgetReplay();
  console.log("OK    agent-budget-replay.ts (run, then replay with the server closed)");
} catch (err) {
  console.error(`FAIL  agent-budget-replay.ts: ${err.message}`);
  failed += 1;
}

const total = tests.length + 1;
if (failed > 0) {
  console.error(`\ntest-examples: ${failed} of ${total} test(s) failed.`);
  process.exit(1);
}

console.log(`\ntest-examples: all ${total} examples passed cleanly.`);

