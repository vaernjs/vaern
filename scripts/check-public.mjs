#!/usr/bin/env node
/**
 * check-public.mjs — public repository gate and anti-slop enforcement.
 *
 * Enforces:
 *   1. Zero internal-only creator files tracked by git.
 *   2. .gitignore coverage for internal patterns and core.hooksPath configured.
 *   3. Anti-slop contract across all tracked files (no buzzwords, no emoji,
 *      no exclamation-mark headlines, no dead allows or unhandled panics).
 *   4. Root README structure and required sections.
 *   5. README wording: no plans, milestone tables, pre-release wording or
 *      local URLs, and the website and DevLogix links present.
 *   6. Legal entity casing.
 *
 * Exit 0: all checks passed.
 * Exit 1: public gate blocked.
 */
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const errors = [];
const warnings = [];
const repoRoot = path.resolve(import.meta.dirname, "..");

function git(args) {
  try {
    return execFileSync("git", args, { cwd: repoRoot, encoding: "utf8" }).trim();
  } catch (err) {
    errors.push(`git ${args.join(" ")} failed: ${err.message}`);
    return "";
  }
}

/* 1. Internal-only files check -------------------------------------- */
const trackedFiles = git(["ls-files"])
  .split("\n")
  .map((f) => f.trim())
  .filter(Boolean);

const internalPatterns = [
  /^docs\/internal\//i,
  /^DECISIONS\.md$/i,
  /.*strategy.*\.md$/i,
  /.*notes-to-start.*\.md$/i,
  /.*naming-research.*\.md$/i,
  /.*naming-round.*\.py$/i,
  /^verify.*\.py$/i,
  /^check_names\.py$/i,
  /^\.env(?!\.example$)/i,
  /.*\.pem$/i,
  /.*\.key$/i,
  /.*\.tgz$/i,
  /(^|\/)target\//i,
  /(^|\/)vendor\//i,
  /(^|\/)node_modules\//i,
  /^packages\/vaern-[^/]+\/bin\/vaern/i,
  /^(CODE_OF_CONDUCT|CONTRIBUTING|SECURITY)\.md$/i,
];

for (const file of trackedFiles) {
  for (const pattern of internalPatterns) {
    if (pattern.test(file)) {
      errors.push(`internal-only file tracked in git: ${file}`);
    }
  }
}

/* 2. Git hooks and .gitignore check --------------------------------- */
const hooksPath = git(["config", "core.hooksPath"]);
if (hooksPath !== ".githooks") {
  const current = hooksPath || "(unset)";
  errors.push(`git core.hooksPath is "${current}"; expected ".githooks"`);
}

const gitignorePath = path.join(repoRoot, ".gitignore");
if (!fs.existsSync(gitignorePath)) {
  errors.push("missing .gitignore file");
} else {
  const gitignoreContent = fs.readFileSync(gitignorePath, "utf8");
  const requiredIgnores = [
    "docs/internal/",
    "DECISIONS.md",
    "*strategy*.md",
    "*notes-to-start*.md",
    "*naming-research*.md",
    "*naming-round*.py",
    "verify*.py",
    "check_names.py",
    ".env*",
    "*.pem",
    "*.key",
    "*.tgz",
    "target/",
    "vendor/",
    "node_modules/",
    "packages/vaern-*/bin/vaern*",
  ];
  for (const req of requiredIgnores) {
    if (!gitignoreContent.includes(req)) {
      errors.push(`.gitignore missing coverage for: ${req}`);
    }
  }
}

/* 3. Anti-slop contract on tracked files ---------------------------- */
const emojiRegex =
  /[\u{1F300}-\u{1F9FF}\u{2600}-\u{26FF}\u{2700}-\u{27BF}\u{1F1E6}-\u{1F1FF}\u{1FA70}-\u{1FAFF}]/u;

const buzzwordRegexes = [
  /\bdelve\b/i,
  /\bseamless\b/i,
  /\bseamlessly\b/i,
  /\bcutting-edge\b/i,
  /\brevolutionary\b/i,
  /\bunleash\b/i,
  /\bsupercharge\b/i,
  /\bas an ai\b/i,
  /\bit is important to note\b/i,
];

const selfPath = path.relative(repoRoot, fileURLToPath(import.meta.url)).replace(/\\/g, "/");

function fileURLToPath(url) {
  return new URL(url).pathname.replace(/^\/([a-zA-Z]:)/, "$1");
}

for (const file of trackedFiles) {
  const abs = path.join(repoRoot, file);
  if (!fs.existsSync(abs) || fs.statSync(abs).isDirectory()) continue;

  const content = fs.readFileSync(abs, "utf8");
  const lines = content.split("\n");

  // Skip self and check-publish pattern lists from false positive matching
  const isCheckScript = file === selfPath || file === "scripts/check-publish.mjs";

  lines.forEach((line, index) => {
    const lineNum = index + 1;

    // A. Emoji check
    if (emojiRegex.test(line)) {
      errors.push(`${file}:${lineNum}: anti-slop violation: emoji detected`);
    }

    // B. Exclamation mark in markdown headline
    if (/^#+\s+.*!/.test(line)) {
      errors.push(`${file}:${lineNum}: anti-slop violation: exclamation mark in headline`);
    }

    // C. Banned buzzwords
    if (!isCheckScript) {
      for (const pattern of buzzwordRegexes) {
        if (pattern.test(line)) {
          errors.push(`${file}:${lineNum}: anti-slop violation: banned buzzword matching ${pattern}`);
        }
      }
      if (/\b(TODO|FIXME)\b/.test(line)) {
        errors.push(`${file}:${lineNum}: anti-slop violation: unfinished marker (TODO/FIXME)`);
      }
    }

    // D. Rust code anti-slop rules
    if (file.endsWith(".rs")) {
      if (/#!?\[allow\(/.test(line)) {
        errors.push(`${file}:${lineNum}: code violation: #[allow(...)] found`);
      }
      if (!file.includes("test") && /\b(unwrap|expect)\(/.test(line)) {
        errors.push(`${file}:${lineNum}: code violation: unwrap/expect outside tests`);
      }
      if (/\b(todo!|unimplemented!|dbg!)/.test(line)) {
        errors.push(`${file}:${lineNum}: code violation: debug or unfinished macro found`);
      }
    }
  });
}

/* 4. Root README structure verification ----------------------------- */
const rootReadmePath = path.join(repoRoot, "README.md");
if (!fs.existsSync(rootReadmePath)) {
  errors.push("missing root README.md");
} else {
  const readme = fs.readFileSync(rootReadmePath, "utf8");

  // Check top badges: max 3 badges, specific allowed types
  const badgeMatches = readme.match(/\[!\[[^\]]*\]\([^)]+\)\]\([^)]+\)/g) || [];
  if (badgeMatches.length > 3) {
    errors.push(`README.md has ${badgeMatches.length} badges (maximum allowed is 3)`);
  }
  for (const b of badgeMatches) {
    if (/discord|codecov|contributor/i.test(b)) {
      errors.push(`README.md has disallowed badge: ${b}`);
    }
  }

  // Required sections, in this order. Other sections may sit between them,
  // because what a release can show differs from version to version.
  const requiredSections = [
    "## Status",
    "## Install",
    "## Repository layout",
    "## Licence",
    "## Links",
  ];

  const actualSections = readme
    .split("\n")
    .filter((line) => line.startsWith("## "))
    .map((line) => line.trim());

  let matched = 0;
  for (const section of actualSections) {
    if (section === requiredSections[matched]) matched++;
  }
  if (matched !== requiredSections.length) {
    errors.push(
      `README.md must contain the sections [${requiredSections.join(", ")}] in that order, got [${actualSections.join(", ")}]`
    );
  }

  // Pitch paragraph check: max 3 sentences, contains required maintainer string
  const pitchParagraphMatch = readme.match(/# vaern\s*\n\s*\n([\s\S]*?)(?=\n## )/);
  if (!pitchParagraphMatch) {
    errors.push("README.md missing initial pitch paragraph directly below '# vaern'");
  } else {
    const pitch = pitchParagraphMatch[1].trim();
    const sentences = pitch
      .split(/(?<=[.?!])\s+/)
      .map((s) => s.trim())
      .filter((s) => s.length > 0);
    if (sentences.length > 3) {
      errors.push(`README.md pitch paragraph has ${sentences.length} sentences (max 3 allowed)`);
    }
    if (!pitch.includes("Maintained by DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED (DevLogix).")) {
      errors.push(
        'README.md pitch missing exact maintainer string: "Maintained by DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED (DevLogix)."'
      );
    }
  }

  // Licence section check
  if (!readme.includes("Copyright (c) 2026 DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED")) {
    errors.push('README.md Licence section missing "Copyright (c) 2026 DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED"');
  }
}

/* 5. README wording ------------------------------------------------- */
// The READMEs state what ships in this version. Plans, milestone tables and
// pre-release wording go stale with every release, so they are rejected
// instead of kept in step. Local URLs mean nothing to a reader on npm.
const readmeFiles = ["README.md", "packages/vaern/README.md"];
const readmeRules = [
  [/placeholder/i, "pre-release wording"],
  [/reserves the (package )?name/i, "pre-release wording"],
  [/coming soon/i, "pre-release wording"],
  [/lands in milestone/i, "pre-release wording"],
  [/^#+\s*Roadmap\b/i, "roadmap section"],
  [/\*\*M[1-5]\*\*/, "milestone table"],
  [/what it will look like/i, "future-tense section"],
  [/deterministic replay/i, 'unimplemented feature; describe the goal as "replayable runs"'],
  [/replay --diff/i, "unimplemented command"],
  [/localhost|127\.0\.0\.1/i, "local URL; link https://vaernjs.tech instead"],
];
const requiredLinks = [
  '<a href="https://vaernjs.tech">vaernjs.tech</a>',
  '<a href="https://devlogix.com.pk">devlogix.com.pk</a>',
];

for (const file of readmeFiles) {
  const abs = path.join(repoRoot, file);
  if (!fs.existsSync(abs)) {
    errors.push(`missing ${file}`);
    continue;
  }
  const content = fs.readFileSync(abs, "utf8");

  content.split("\n").forEach((line, index) => {
    for (const [pattern, reason] of readmeRules) {
      if (pattern.test(line)) {
        errors.push(`${file}:${index + 1}: ${reason} (matches ${pattern})`);
      }
    }
  });

  const links = content.split(/\n(?=## )/).find((s) => s.startsWith("## Links"));
  if (!links) {
    errors.push(`${file} missing section "## Links"`);
    continue;
  }
  for (const link of requiredLinks) {
    if (!links.includes(link)) {
      errors.push(`${file} Links section missing ${link}`);
    }
  }
}

/* 6. Legal name casing check ---------------------------------------- */
for (const file of trackedFiles) {
  if (file === selfPath) continue;
  const abs = path.join(repoRoot, file);
  if (!fs.existsSync(abs) || fs.statSync(abs).isDirectory()) continue;
  const content = fs.readFileSync(abs, "utf8");

  // Title-case is strictly forbidden
  if (/DevLogix Technology \(Private\)/.test(content)) {
    errors.push(`${file}: forbidden title-case legal name "DevLogix Technology (Private)" detected`);
  }
}

/* ------------------------------------------------------------------ */
if (errors.length) {
  for (const e of errors) console.error(`  FAIL  ${e}`);
  console.error(`\ncheck-public: ${errors.length} error(s) — repository not ready for public release.`);
  process.exit(1);
}

for (const w of warnings) console.warn(`  warn  ${w}`);
console.log(`check-public: OK — 0 errors across ${trackedFiles.length} tracked files.`);
