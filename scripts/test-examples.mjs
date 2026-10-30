#!/usr/bin/env node
/**
 * test-examples.mjs — automated test suite executing all repository examples.
 *
 * Runs each example program through the native Vaern runtime and asserts
 * that expected standard output is produced.
 */
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

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

if (failed > 0) {
  console.error(`\ntest-examples: ${failed} of ${tests.length} test(s) failed.`);
  process.exit(1);
}

console.log(`\ntest-examples: all ${tests.length} examples passed cleanly.`);

