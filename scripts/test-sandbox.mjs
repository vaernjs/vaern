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
// Fixtures that write files do so under out/, recreated empty for every run.
fs.rmSync(path.join(sandbox, "out"), { recursive: true, force: true });
fs.mkdirSync(path.join(sandbox, "out"));
// Package fixtures live in resolve/packages because node_modules is never
// tracked; they are installed by copying them into place for each run.
const installed = path.join(sandbox, "resolve", "node_modules");
fs.rmSync(installed, { recursive: true, force: true });
fs.cpSync(path.join(sandbox, "resolve", "packages"), installed, { recursive: true });
// A linked package, as pnpm installs them. "junction" is ignored outside
// Windows, where it lets the link be made without extra privileges.
fs.symlinkSync(path.join(installed, "greeter"), path.join(installed, "linked-greeter"), "junction");
// Files at and one byte past the vaern:fs read cap, made at run time so the
// repository carries no 64 MiB fixture.
const readCap = 64 * 1024 * 1024;
for (const [name, size] of [["exact.bin", readCap], ["over.bin", readCap + 1]]) {
  const file = path.join(sandbox, "out", name);
  fs.writeFileSync(file, "");
  fs.truncateSync(file, size);
}

const aiOffline =
  '{"usd":0.001,"tokens":50}\n' +
  "RESERVE_EXCEEDS_REMAINING tokens 141 50\n" +
  "42\n" +
  "args arguments at $.n: expected integer, found string\n";

const cases = [
  {
    name: "--version reports 0.0.7",
    args: ["--version"],
    code: 0,
    stdout: "vaern 0.0.7\n",
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
    name: "parse_error.js reports the SyntaxError name, message and location",
    args: ["run", "parse_error.js"],
    code: 1,
    stdout: "",
    stderrIncludes: ["SyntaxError: unexpected token in expression", "parse_error.js:2:"],
    stderrExcludes: ["panicked"],
  },
  {
    name: "accessor.ts rejects an auto-accessor at its location",
    args: ["run", "accessor.ts"],
    code: 1,
    stdout: "",
    stderrIncludes: ["unsupported TypeScript feature 'accessor' in 'accessor.ts:2:3'", "getter and setter"],
    stderrExcludes: ["panicked", "SyntaxError"],
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
    name: "console_error.js prints an Error with its frames and cause",
    args: ["run", "console_error.js"],
    code: 0,
    stdout:
      "Error: boom\n    at <anonymous> (console_error.js:2:44)\n" +
      "Caused by: TypeError: inner\n    at <anonymous> (console_error.js:2:44)\n",
    stderr: "",
  },
  {
    name: "url_subset.js parses URLs the way the WHATWG parser does",
    args: ["run", "url_subset.js"],
    code: 0,
    stdout:
      "user p%40ss example.com:8443 /b ?x=1 #top\n" +
      "[::1] 3000 http://[::1]:3000\n" +
      "https://example.com/dir/up?q=a%20b\n" +
      "https://example.com/%7Efoo\n" +
      "?tag=b&q=x+y%26z b 2\n" +
      "v https://example.com/?k=v\n" +
      "https://example.com/new%20path?k=v\n" +
      "false true\n" +
      "a=1&b=two+words\n",
    stderr: "",
  },
  {
    name: "managed_headers.js refuses framing headers on fetch and on served responses",
    args: ["run", "managed_headers.js"],
    code: 0,
    stdout:
      "TypeError: fetch: header 'transfer-encoding' is set by the runtime and cannot be supplied; remove it\n" +
      "500\n",
    stderrIncludes: [
      "[ERROR] GET /: TypeError: serve: handler response: header 'content-length' is set by the runtime",
    ],
    stderrExcludes: ["panicked"],
  },
  {
    name: "multiline_stack.ts reports the source line under multi-line types",
    args: ["run", "multiline_stack.ts"],
    code: 1,
    stdout: "at fail (multiline_stack.ts:12:29)\n",
    stderrIncludes: ["Error: failed uncaught", "multiline_stack.ts:12:29", "multiline_stack.ts:21:1"],
    stderrExcludes: ["panicked"],
  },
  {
    name: "fs_subset.ts writes, appends and reads files with typed errors",
    args: ["run", "fs_subset.ts"],
    code: 0,
    stdout:
      '"line one\\nline two\\n" 18\n' +
      "FsError open EEXIST true\n" +
      "FsError open ENOENT true\n" +
      "FsError open EISDIR true\n",
    stderr: "",
  },
  {
    name: "fs_limits.ts reads a file at the cap and refuses one a byte over it",
    args: ["run", "fs_limits.ts"],
    code: 0,
    stdout: "67108864\nFsError EFBIG\n",
    stderr: "",
  },
  {
    name: "resolve/linked.ts imports a package through a directory link",
    args: ["run", "resolve/linked.ts"],
    code: 0,
    stdout: "hello, link\n",
    stderr: "",
  },
  {
    name: "resolve/main.ts imports packages from node_modules through exports and main",
    args: ["run", "resolve/main.ts"],
    code: 0,
    stdout: "hello, vaern 1.2.3 5\ntrue\n",
    stderr: "",
  },
  {
    name: "resolve/missing_package.js lists every node_modules directory tried",
    args: ["run", "resolve/missing_package.js"],
    code: 1,
    stdout: "",
    stderrIncludes: [
      "module resolution failed for 'not-installed'",
      `tried: ${path.join(sandbox, "resolve", "node_modules", "not-installed")}\n`,
      `tried: ${path.join(sandbox, "node_modules", "not-installed")}\n`,
      "Vaern does not download packages.",
    ],
    stderrExcludes: ["panicked", "runtime exception"],
  },
  {
    name: "heap_limit.js stops at the heap limit with a located error",
    args: ["run", "heap_limit.js"],
    code: 1,
    stdout: "",
    stderrIncludes: ["InternalError: out of memory", "heap_limit.js:3:"],
    stderrExcludes: ["panicked"],
  },
  {
    name: "ai_offline.ts refuses a model call over budget and runs tools, journalled",
    args: ["run", "ai_offline.ts"],
    env: { VAERN_HOME: "out/home" },
    code: 0,
    stdout: aiOffline,
    stderr: "",
  },
  {
    name: "replay latest reproduces ai_offline.ts from its journal",
    args: ["replay", "latest"],
    env: { VAERN_HOME: "out/home" },
    code: 0,
    stdout: aiOffline,
    stderrIncludes: ["reproduced: 4 console records, exit code 0"],
  },
  {
    name: "replay of an unknown run id fails with the next step",
    args: ["replay", "no-such-run"],
    env: { VAERN_HOME: "out/home" },
    code: 1,
    stdout: "",
    stderrIncludes: ["'no-such-run' is not a run id", "or `latest`"],
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
