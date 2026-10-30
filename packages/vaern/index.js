"use strict";

/**
 * Programmatic API (minimal by design for v0).
 *
 * Most consumers want the CLI. This module exists so tools, editors and CI can
 * locate the native binary without re-implementing the resolution logic.
 */

const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");
const { createRequire } = require("node:module");

const require_ = createRequire(__filename);
const pkg = require_("./package.json");

const EXE = process.platform === "win32" ? ".exe" : "";
const PLATFORM_PKG = `@vaernjs/vaern-${process.platform}-${process.arch}`;

/** Package name that should carry the binary for this platform, e.g. `@vaernjs/vaern-linux-x64`. */
const platformPackage = PLATFORM_PKG;

/** Absolute path to the native binary, or null if this release has none. */
function binaryPath() {
  if (process.env.VAERN_BINARY && fs.existsSync(process.env.VAERN_BINARY)) {
    return path.resolve(process.env.VAERN_BINARY);
  }
  try {
    const manifest = require_.resolve(`${PLATFORM_PKG}/package.json`);
    const dir = path.dirname(manifest);
    const declared = require_(manifest).bin;
    const rel =
      typeof declared === "string"
        ? declared
        : declared && (declared.vaern || Object.values(declared)[0]);
    const candidate = rel ? path.join(dir, rel) : path.join(dir, "bin", `vaern${EXE}`);
    return fs.existsSync(candidate) ? candidate : null;
  } catch {
    return null;
  }
}

/** True when a native binary is available for this platform. */
function hasNativeBinary() {
  return binaryPath() !== null;
}

/**
 * Spawn the vaern binary. Returns the ChildProcess so callers control stdio.
 * Throws if no native binary is present.
 */
function vaern(args = [], options = {}) {
  const bin = binaryPath();
  if (!bin) {
    throw new Error(
      `vaern: no native binary for ${process.platform}-${process.arch}. ` +
        `Install the optional platform package \`${PLATFORM_PKG}\`, or set $VAERN_BINARY.`
    );
  }
  return spawn(bin, args, { stdio: "inherit", ...options });
}

module.exports = { version: pkg.version, platformPackage, binaryPath, hasNativeBinary, vaern };
