#!/usr/bin/env node
/**
 * idle-cpu.mjs — CPU time an idle `vaern:http` server burns.
 *
 *   node bench/idle-cpu.mjs [binary ...]
 *
 * Runs bench/idle-server.ts with each binary (default target/release/vaern),
 * samples the process CPU time one second after start and again three
 * seconds later, and prints the CPU spent in that idle window. Startup cost
 * falls outside the window. A runtime that sleeps until work arrives spends
 * close to zero; one that polls on a fixed tick does not.
 */
import { execFileSync, spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const repoRoot = path.resolve(import.meta.dirname, "..");
const exe = process.platform === "win32" ? "vaern.exe" : "vaern";
const binaries = process.argv.length > 2
  ? process.argv.slice(2)
  : [path.join(repoRoot, "target", "release", exe)];
const script = path.join(repoRoot, "bench", "idle-server.ts");
const WINDOW_MS = 3000;

/** Total user + system CPU time of `pid`, in milliseconds. */
function cpuMs(pid) {
  if (process.platform === "win32") {
    const out = execFileSync("powershell", [
      "-NoProfile",
      "-Command",
      `(Get-Process -Id ${pid}).TotalProcessorTime.TotalMilliseconds`,
    ], { encoding: "utf8" });
    return Number(out.trim().replace(",", "."));
  }
  if (process.platform === "linux") {
    const fields = fs.readFileSync(`/proc/${pid}/stat`, "utf8").split(") ")[1].split(" ");
    const ticks = Number(fields[11]) + Number(fields[12]);
    const hz = Number(execFileSync("getconf", ["CLK_TCK"], { encoding: "utf8" }));
    return (ticks * 1000) / hz;
  }
  const out = execFileSync("ps", ["-o", "time=", "-p", String(pid)], { encoding: "utf8" }).trim();
  const [min, sec] = out.split(":");
  return (Number(min) * 60 + Number(sec)) * 1000;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

for (const binary of binaries) {
  if (!fs.existsSync(binary)) {
    console.error(`missing ${binary}\nrun: cargo build --release`);
    process.exit(1);
  }
  // The server outlives the sampling window by a wide margin, so the second
  // sample never reads an exited (or reused) process id.
  const child = spawn(binary, ["run", script, "8"], { stdio: "ignore" });
  const exited = new Promise((resolve) => child.on("exit", resolve));
  await sleep(1000);
  const before = cpuMs(child.pid);
  await sleep(WINDOW_MS);
  const after = cpuMs(child.pid);
  await exited;
  const spent = after - before;
  console.log(
    `${binary}: ${spent.toFixed(1)} ms CPU in ${WINDOW_MS / 1000} s idle ` +
      `(${((spent / WINDOW_MS) * 100).toFixed(2)}% of one core)`
  );
}
