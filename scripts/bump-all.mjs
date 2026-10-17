#!/usr/bin/env node
/**
 * bump-all.mjs — lockstep version bump for the whole npm family.
 *
 *   node scripts/bump-all.mjs 0.1.0
 *   node scripts/bump-all.mjs 0.2.0-canary.1
 *
 * Sets the version in every packages/vaern* packages and rewrites the
 * optionalDependencies map in packages/vaern/package.json to match exactly.
 * Every package in the family must ship at the same version — a mismatch makes
 * `npm i vaern` fail to resolve its binaries.
 */
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const version = process.argv[2];
if (!version) {
  console.error("usage: node scripts/bump-all.mjs <version>   e.g. 0.1.0  0.2.0-canary.1");
  process.exit(1);
}
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z-.]+)?(\+[0-9A-Za-z-.]+)?$/.test(version)) {
  console.error(`"${version}" is not valid semver`);
  process.exit(1);
}

const repoRoot = path.resolve(import.meta.dirname, "..");
const pkgsDir = path.join(repoRoot, "packages");
const isPrerelease = version.includes("-");

const dirs = fs
  .readdirSync(pkgsDir)
  .filter((d) => d.startsWith("vaern") && fs.statSync(path.join(pkgsDir, d)).isDirectory());

const changed = [];
for (const d of dirs) {
  const file = path.join(pkgsDir, d, "package.json");
  const pkg = JSON.parse(fs.readFileSync(file, "utf8"));
  const before = pkg.version;
  pkg.version = version;

  if (pkg.optionalDependencies) {
    for (const name of Object.keys(pkg.optionalDependencies)) {
      pkg.optionalDependencies[name] = version;
    }
  }
  if (pkg.publishConfig) {
    // Prereleases must never become `latest` — `npm i vaern` would install a canary.
    pkg.publishConfig.tag = isPrerelease ? "canary" : "latest";
  }

  fs.writeFileSync(file, JSON.stringify(pkg, null, 2) + "\n");
  changed.push(`${d}: ${before} -> ${version}${pkg.publishConfig ? ` (tag: ${pkg.publishConfig.tag})` : ""}`);
}

console.log(changed.map((c) => `  ${c}`).join("\n"));
console.log(`\nbumped ${changed.length} package(s) to ${version}`);

// Convenience: create the annotated git tag the release workflow triggers on.
try {
  const dirty = execFileSync("git", ["status", "--porcelain"], { cwd: repoRoot, encoding: "utf8" });
  if (!dirty.trim()) {
    execFileSync("git", ["tag", "-a", `v${version}`, "-m", `vaern v${version}`], { cwd: repoRoot, stdio: "inherit" });
    console.log(`created tag v${version} — push with: git push origin main --follow-tags`);
  } else {
    console.log("working tree dirty — commit the bump first, then tag manually:");
    console.log(`  git commit -am "release: v${version}" && git tag -a v${version} -m "vaern v${version}"`);
  }
} catch {
  console.log("(not a git repo — tagging skipped)");
}
