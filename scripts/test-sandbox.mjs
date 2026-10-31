#!/usr/bin/env node
/**
 * test-sandbox.mjs — run the release binary against the fixtures in
 * tests/sandbox_run/ and assert exact stdout, stderr and exit codes.
 *
 *   cargo build --release
 *   node scripts/test-sandbox.mjs
 *
 * The binary is copied into the sandbox folder first, so the run does not
 * depend on the working directory or on anything else in the repository.
 */
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const repoRoot = path.resolve(import.meta.dirname, "..");
const sandbox = path.join(repoRoot, "tests", "sandbox_run");
const exe = process.platform === "win32" ? "vaern.exe" : "vaern";
const built = path.join(repoRoot, "target", "release", exe);
const binary = path.join(sandbox, exe);

if (!fs.existsSync(built)) {
  console.error(`missing ${built}\nrun: cargo build --release`);
  process.exit(1);
}
fs.copyFileSync(built, binary);
fs.chmodSync(binary, 0o755);

const cases = [
  {
    name: "--version reports 0.0.4",
    args: ["--version"],
    code: 0,
    stdout: "vaern 0.0.4\n",
  },
  {
    name: "hello.ts strips types and prints",
    args: ["run", "hello.ts"],
    code: 0,
    stdout: "Hello from Vaern (Runtime)!\n",
    stderr: "",
  },
  {
    name: "argv_env.ts receives the script argument",
    args: ["run", "argv_env.ts", "sandbox-arg"],
    code: 0,
    stdout: "Arg: sandbox-arg\n",
  },
  {
    name: "env_test.ts reads the host environment",
    args: ["run", "env_test.ts"],
    env: { VAERN_SANDBOX_VALUE: "from-host" },
    code: 0,
    stdout: "Env: from-host\n",
  },
  {
    name: "exit_test.ts exits with 42",
    args: ["run", "exit_test.ts"],
    code: 42,
    stdout: "",
  },
  {
    name: "bad_syntax.ts reports file:line:col without a panic",
    args: ["run", "bad_syntax.ts"],
    code: 1,
    stderrIncludes: ["bad_syntax.ts:1:"],
    stderrExcludes: ["panicked"],
  },
  {
    name: "throws.js reports file:line:col and fails",
    args: ["run", "throws.js"],
    code: 1,
    stderrIncludes: ["Error: boom", "throws.js:1:"],
    stderrExcludes: ["panicked"],
  },
  {
    name: "timer_throws.js fails when a timer callback throws",
    args: ["run", "timer_throws.js"],
    code: 1,
    stdout: "",
    stderrIncludes: ["Error: timer boom", "timer_throws.js:2:"],
    stderrExcludes: ["panicked"],
  },
  {
    name: "no_test_tls.ts finds no built-in TLS test server",
    args: ["run", "no_test_tls.ts"],
    code: 0,
    stdout: "rejected\n",
  },
  {
    name: "body_cap.ts answers an oversized body with 413 before the handler",
    args: ["run", "body_cap.ts"],
    code: 0,
    stdout: "status 413, handler ran 0 times\n",
  },
  {
    name: "heap_limit.js stops at the heap limit with a located error",
    args: ["run", "heap_limit.js"],
    code: 1,
    stdout: "",
    stderrIncludes: ["InternalError: out of memory", "heap_limit.js:3:"],
    stderrExcludes: ["panicked"],
  },
];

const norm = (s) => s.replaceAll("\r\n", "\n");
let failed = 0;

for (const c of cases) {
  const r = spawnSync(binary, c.args, {
    cwd: sandbox,
    encoding: "utf8",
    env: { ...process.env, ...c.env },
  });
  const stdout = norm(r.stdout ?? "");
  const stderr = norm(r.stderr ?? "");
  const problems = [];

  if (r.status !== c.code) problems.push(`exit code ${r.status}, expected ${c.code}`);
  if (c.stdout !== undefined && stdout !== c.stdout) {
    problems.push(`stdout ${JSON.stringify(stdout)}, expected ${JSON.stringify(c.stdout)}`);
  }
  if (c.stderr !== undefined && stderr !== c.stderr) {
    problems.push(`stderr ${JSON.stringify(stderr)}, expected ${JSON.stringify(c.stderr)}`);
  }
  for (const needle of c.stderrIncludes ?? []) {
    if (!stderr.includes(needle)) problems.push(`stderr lacks ${JSON.stringify(needle)}: ${stderr}`);
  }
  for (const needle of c.stderrExcludes ?? []) {
    if (stderr.includes(needle)) problems.push(`stderr contains ${JSON.stringify(needle)}: ${stderr}`);
  }

  if (problems.length === 0) {
    console.log(`PASS  ${c.name}`);
  } else {
    failed += 1;
    console.log(`FAIL  ${c.name}`);
    for (const p of problems) console.log(`      ${p}`);
  }
}

console.log(`\n${cases.length - failed}/${cases.length} sandbox cases passed`);
process.exit(failed === 0 ? 0 : 1);
