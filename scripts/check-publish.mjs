#!/usr/bin/env node
/**
 * check-publish.mjs — pre-publish gate. Runs from `prepublishOnly`.
 *
 * Catches the mistakes a two-person team makes at 1am:
 *   - version drift between the main package and platform packages
 *   - missing LICENSE / README
 *   - publishing from a dirty git tree
 *   - publishing over an existing version
 *
 * Exit 1 blocks the publish.
 */
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const errors = [];
const warnings = [];
const repoRoot = path.resolve(import.meta.dirname, "..");
const pkgDir = path.resolve(import.meta.dirname, "..", "packages", "vaern");

function readJson(p) {
  try {
    return JSON.parse(fs.readFileSync(p, "utf8"));
  } catch (e) {
    errors.push(`cannot read ${path.relative(repoRoot, p)}: ${e.message}`);
    return null;
  }
}

const main = readJson(path.join(pkgDir, "package.json"));
if (!main) {
  console.error(errors.join("\n"));
  process.exit(1);
}

/* 1. required files ------------------------------------------------ */
for (const f of ["LICENSE", "README.md"]) {
  const p = path.join(pkgDir, f);
  if (!fs.existsSync(p)) errors.push(`missing ${f} in packages/vaern/`);
  else if (fs.statSync(p).size < 200) warnings.push(`${f} looks like a stub (${fs.statSync(p).size} bytes)`);
}
for (const f of ["bin/vaern.cjs", "index.js", "index.d.ts", "scripts/postinstall.cjs"]) {
  if (!fs.existsSync(path.join(pkgDir, f))) errors.push(`missing ${f} (declared in package.json)`);
}

/* 2. no placeholders ---------------------------------------------- */
const raw = fs.readFileSync(path.join(pkgDir, "package.json"), "utf8");
for (const bad of ["TODO", "FIXME", "example.com"]) {
  if (raw.includes(bad)) errors.push(`package.json still contains placeholder "${bad}"`);
}

/* 3. required metadata -------------------------------------------- */
for (const f of ["name", "version", "description", "license", "repository", "bin", "files"]) {
  if (main[f] === undefined) errors.push(`package.json missing required field "${f}"`);
}
if (main.license !== "MIT") errors.push(`license is "${main.license}", expected MIT`);
if (!main.publishConfig?.access) errors.push('publishConfig.access must be "public"');
if (main.private === true) errors.push("package.json has private:true — cannot publish");

/* 4. version lockstep with platform packages ---------------------- */
const pkgsDir = path.join(repoRoot, "packages");
const platformPkgs = fs
  .readdirSync(pkgsDir)
  .filter((d) => d.startsWith("vaern-") && fs.statSync(path.join(pkgsDir, d)).isDirectory());

for (const d of platformPkgs) {
  const p = readJson(path.join(pkgsDir, d, "package.json"));
  if (!p) continue;
  if (p.version !== main.version) {
    errors.push(`${d} is at ${p.version} but vaern is at ${main.version} — versions must be in lockstep`);
  }
}

// optionalDependencies must pin the exact same version, and every entry must exist on disk
const opt = main.optionalDependencies ?? {};
for (const [name, range] of Object.entries(opt)) {
  if (range !== main.version) {
    errors.push(`optionalDependencies["${name}"] = ${range}, expected exactly ${main.version}`);
  }
  if (!fs.existsSync(path.join(pkgsDir, name))) {
    errors.push(`optionalDependencies["${name}"] has no packages/${name}/ directory`);
  }
}

/* 5. is this version already published? --------------------------- */
try {
  const out = execFileSync("npm", ["view", main.name, "versions", "--json"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "ignore"],
  });
  const published = JSON.parse(out || "[]");
  if (published.includes(main.version)) {
    errors.push(`${main.name}@${main.version} is ALREADY published. npm never allows reusing a version number — bump it.`);
  }
} catch {
  warnings.push("could not reach the registry to check for version conflicts (offline?)");
}

/* 6. dirty git tree ------------------------------------------------ */
try {
  const status = execFileSync("git", ["status", "--porcelain"], {
    cwd: repoRoot,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "ignore"],
  });
  if (status.trim()) {
    warnings.push(`working tree is dirty:\n${status.trim().split("\n").slice(0, 8).join("\n")}`);
  }
} catch {
  warnings.push("not a git repository — cannot verify tree cleanliness");
}

/* ---------------------------------------------------------------- */
for (const w of warnings) console.warn(`  warn  ${w}`);
for (const e of errors) console.error(`  FAIL  ${e}`);

if (errors.length) {
  console.error(`\ncheck-publish: ${errors.length} error(s), ${warnings.length} warning(s) — publish blocked.`);
  process.exit(1);
}
console.log(
  `check-publish: OK — ${main.name}@${main.version} (${platformPkgs.length} platform package(s), ${warnings.length} warning(s))`
);
