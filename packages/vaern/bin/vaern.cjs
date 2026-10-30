#!/usr/bin/env node
"use strict";

/**
 * vaern — CLI entry point.
 *
 * Two modes, chosen at runtime:
 *
 *   NATIVE     A platform package (@vaernjs/vaern-<os>-<arch>) shipped a real binary.
 *              We exec it and get out of the way, forwarding stdio, signals
 *              and the exit code. This is the normal path from v0.0.4.
 *
 *   FALLBACK   No binary for this platform (an unsupported platform, or
 *              optional dependencies skipped with `npm i --omit=optional`).
 *              We serve a small built-in CLI so the command never dies with
 *              "command not found".
 *
 * Resolution order for the native binary:
 *   1. $VAERN_BINARY               (explicit override — used by CI and by `cargo run` during dev)
 *   2. @vaernjs/vaern-<os>-<arch>   (the normal npm install path)
 *   3. ../vendor/vaern             (repo-local build dropped in by `just build`)
 *   4. FALLBACK
 *
 * NOTE ON STARTUP COST: this file is a Node script, so it costs ~40-70ms of
 * interpreter startup before the native binary runs. `scripts/postinstall.cjs`
 * removes that cost by re-pointing the bin link straight at the native binary
 * when npm allows scripts to run. Both paths must stay working.
 */

const { spawn } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");
const { createRequire } = require("node:module");

const require_ = createRequire(__filename);
const pkg = require_("../package.json");

const EXE = process.platform === "win32" ? ".exe" : "";
const PLATFORM_PKG = `@vaernjs/vaern-${process.platform}-${process.arch}`;

/* ------------------------------------------------------------------ */
/* native binary resolution                                            */
/* ------------------------------------------------------------------ */

function firstExisting(candidates) {
  for (const c of candidates) {
    try {
      if (c && fs.statSync(c).isFile()) return c;
    } catch {
      /* not there, keep looking */
    }
  }
  return null;
}

function resolveNativeBinary() {
  // 1. explicit override
  if (process.env.VAERN_BINARY) {
    const p = path.resolve(process.env.VAERN_BINARY);
    if (fs.existsSync(p)) return p;
    process.stderr.write(
      `vaern: $VAERN_BINARY is set but does not exist: ${p}\n`
    );
  }

  // 2. platform package installed by npm as an optionalDependency
  try {
    const manifest = require_.resolve(`${PLATFORM_PKG}/package.json`);
    const dir = path.dirname(manifest);
    const declared = require_(manifest).bin;
    const rel =
      typeof declared === "string"
        ? declared
        : declared && (declared.vaern || Object.values(declared)[0]);
    const found = firstExisting([
      rel ? path.join(dir, rel) : null,
      path.join(dir, "bin", `vaern${EXE}`),
      path.join(dir, `vaern${EXE}`),
    ]);
    if (found) return found;
  } catch {
    /* platform package not installed */
  }

  // 3. repo-local build (developer convenience)
  return firstExisting([
    path.join(__dirname, "..", "vendor", `vaern${EXE}`),
    path.join(__dirname, "..", "..", "..", "target", "release", `vaern${EXE}`),
  ]);
}

/* ------------------------------------------------------------------ */
/* native mode                                                         */
/* ------------------------------------------------------------------ */

function execNative(binary, args) {
  const child = spawn(binary, args, {
    stdio: "inherit",
    // Never let Node inject anything into the child's environment.
    env: process.env,
  });

  // Forward termination signals so `vaern serve` shuts down cleanly on Ctrl-C.
  const signals = ["SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT", "SIGUSR1", "SIGUSR2"];
  for (const sig of signals) {
    if (sig in require("node:os").constants.signals || process.platform !== "win32") {
      process.on(sig, () => {
        try {
          child.kill(sig);
        } catch {
          /* child already gone */
        }
      });
    }
  }

  child.on("error", (err) => {
    process.stderr.write(`vaern: failed to execute ${binary}: ${err.message}\n`);
    process.exit(127);
  });

  child.on("close", (code, signal) => {
    if (signal) {
      // Die the way the child died so shells and CI see the right cause.
      try {
        process.kill(process.pid, signal);
      } catch {
        process.exit(1);
      }
      return;
    }
    process.exit(code === null ? 1 : code);
  });
}

/* ------------------------------------------------------------------ */
/* fallback mode                                                       */
/* ------------------------------------------------------------------ */

const BANNER = `vaern ${pkg.version} — an agent-native JavaScript & TypeScript runtime
VaernJS https://vaernjs.tech · a DevLogix project https://devlogix.com.pk · licensed MIT
`;

const ROADMAP = `
  M1  vaern run hello.ts — TypeScript, fetch, timers, console, HTTP server (shipping in 0.0.x)
  M2  capability enforcer + vaern.toml (deny by default)
  M3  agent(), tool(), MCP client/server, budgets, effect journal, vaern replay
  M4  sovereignty: residency guard, hash-chained audit, PK-PKI signing, ur-PK i18n
  M5  honest public benchmarks, docs, first external production users

Website:     https://vaernjs.tech
DevLogix:    https://devlogix.com.pk
Repository:  ${pkg.repository ? String(pkg.repository.url).replace(/^git\+/, "") : "https://github.com/vaernjs/vaern"}
Issues:      ${pkg.bugs ? pkg.bugs.url : ""}

The native runtime ships in the platform package ${PLATFORM_PKG}. This message
means it is not installed. Run \`vaern doctor\` for details.
`;

function fallback(args) {
  const a = args[0];

  if (a === undefined || a === "--help" || a === "-h" || a === "help") {
    process.stdout.write(`${BANNER}\nUsage: vaern <command> [options]\n${ROADMAP}`);
    process.exit(0);
  }
  if (a === "--version" || a === "-v" || a === "-V" || a === "version") {
    process.stdout.write(`${pkg.version}\n`);
    process.exit(0);
  }
  if (a === "roadmap") {
    process.stdout.write(ROADMAP);
    process.exit(0);
  }
  if (a === "doctor") {
    process.stdout.write(`${BANNER}
platform:        ${process.platform}-${process.arch}
expected package: ${PLATFORM_PKG}
node:            ${process.version}
native binary:   NOT FOUND
$VAERN_BINARY:   ${process.env.VAERN_BINARY || "(unset)"}

Native binaries are published for linux-x64, linux-arm64, darwin-x64,
darwin-arm64 and win32-x64. If your platform is listed, optional dependencies
were probably skipped: reinstall without --omit=optional / --no-optional, or
point $VAERN_BINARY at a vaern binary.
`);
    process.exit(0);
  }

  process.stderr.write(
    `${BANNER}
vaern: unknown command '${a}'

The native runtime binary is not installed for ${process.platform}-${process.arch}.
Expected platform package: ${PLATFORM_PKG}.

Try:   vaern doctor      show install diagnostics
       vaern roadmap     what is being built and when
       vaern --help      usage
`
  );
  process.exit(1);
}

/* ------------------------------------------------------------------ */

const binary = resolveNativeBinary();
if (binary) execNative(binary, process.argv.slice(2));
else fallback(process.argv.slice(2));
