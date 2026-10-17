"use strict";

/**
 * postinstall — OPTIONAL fast path. Never fails the install.
 *
 * npm links `bin/vaern` to our Node shim, which costs ~40-70ms of interpreter
 * startup before the native binary runs. For a runtime CLI that is a real tax.
 *
 * When npm permits scripts to run, we re-point the bin link directly at the
 * native binary so `vaern` starts at native speed. When scripts are disabled
 * (`npm i --ignore-scripts`), nothing happens and the shim path still works.
 *
 * Guarantees:
 *   - exits 0 no matter what. A failed postinstall breaks the user's install.
 *   - POSIX only (symlink replacement). Windows keeps the shim; creating
 *     symlinks there needs privileges we will not assume.
 *   - opt out with VAERN_NO_LINK=1
 */

const fs = require("node:fs");
const path = require("node:path");
const { createRequire } = require("node:module");

function done(reason) {
  if (process.env.VAERN_DEBUG_INSTALL) {
    process.stdout.write(`vaern postinstall: ${reason}\n`);
  }
  process.exit(0);
}

function main() {
  if (process.env.VAERN_NO_LINK === "1") return done("disabled via VAERN_NO_LINK");
  if (process.platform === "win32") return done("windows — keeping the JS shim");

  const require_ = createRequire(__filename);
  const platformPkg = `vaern-${process.platform}-${process.arch}`;

  let binary;
  try {
    const manifest = require_.resolve(`${platformPkg}/package.json`);
    const dir = path.dirname(manifest);
    const declared = require_(manifest).bin;
    const rel =
      typeof declared === "string"
        ? declared
        : declared && (declared.vaern || Object.values(declared)[0]);
    const candidate = rel ? path.join(dir, rel) : path.join(dir, "bin", "vaern");
    if (!fs.existsSync(candidate)) return done(`no binary in ${platformPkg}`);
    binary = candidate;
  } catch {
    return done(`platform package ${platformPkg} not installed`);
  }

  fs.chmodSync(binary, 0o755);

  // Candidate bin directories: local install, then global prefix.
  const candidates = [
    path.resolve(__dirname, "..", "..", "..", ".bin"), // node_modules/.bin
  ];
  if (process.env.npm_config_prefix) {
    candidates.push(path.join(process.env.npm_config_prefix, "bin"));
  }

  let linked = false;
  for (const binDir of candidates) {
    const link = path.join(binDir, "vaern");
    try {
      if (!fs.existsSync(binDir)) continue;
      const stat = fs.lstatSync(link);
      // Only replace something that currently points at OUR shim.
      const target = stat.isSymbolicLink() ? fs.readlinkSync(link) : link;
      if (!/vaern\.cjs$/.test(target) && !/vaern\.cjs$/.test(path.resolve(binDir, target))) {
        continue;
      }
      fs.rmSync(link, { force: true });
      fs.symlinkSync(binary, link);
      linked = true;
    } catch {
      continue; // read-only fs, permissions, race — all fine, the shim still works
    }
  }
  return done(linked ? `linked ${binary}` : "no replaceable bin link found");
}

try {
  main();
} catch {
  process.exit(0);
}
