[![npm version](https://img.shields.io/npm/v/%40vaernjs%2Fvaern.svg)](https://www.npmjs.com/package/@vaernjs/vaern)
[![CI](https://github.com/vaernjs/vaern/actions/workflows/ci.yml/badge.svg)](https://github.com/vaernjs/vaern/actions/workflows/ci.yml)
[![licence](https://img.shields.io/github/license/vaernjs/vaern.svg)](./LICENSE)

# vaern

Vaern is an agent-native JavaScript and TypeScript runtime. It is being built so that capability sandboxing, replayable runs and sovereignty enforcement live in the engine instead of in libraries. Maintained by DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED (DevLogix).

## Status

v0.0.7, an early native runtime. `@vaernjs/vaern` on npm ships the `vaern` binary for Linux, macOS and Windows.
It runs JavaScript and TypeScript files with `console`, timers, `fetch`, an HTTP `serve()`, whole-file reads and writes from `vaern:fs`, `process.argv`, `process.env` and `process.exit()`, and imports ES module packages already installed in `node_modules`.
TypeScript keeps its line and column numbers in stack traces. Enums, namespaces and decorators are rejected with a `file:line:col` error, because they emit runtime code that type stripping cannot produce.
`vaern:ai` provides `model()` for OpenAI-compatible chat endpoints, on a cloud service or on your own machine, with `tool()` and `agent()`. Budgets are checked by the runtime: no model call or tool effect is dispatched unless the reserve fits the remaining budget. Amounts are estimates from the prices the script gives; provider billing may drift.
Runs are replayable: every effect is recorded in a hash-chained journal, and `vaern replay` reproduces the transcript. [docs/journal.md](./docs/journal.md) says what a replay does and does not reproduce.
Not implemented yet: `node:*` modules, CommonJS packages, capability sandboxing (`vaern.toml`) and MCP.
[docs/subset.md](./docs/subset.md) lists every API that works, its limits, and what is absent.

## Install

```bash
npm install -g @vaernjs/vaern
npx @vaernjs/vaern --version
```

npm installs a prebuilt binary for your platform through optional dependencies: Linux x64 and arm64, macOS x64 and arm64, Windows x64.
Other platforms, or installs that skip optional dependencies, fall back to a diagnostics CLI (`vaern doctor`).

## Example

```ts
// latest.ts
interface Manifest {
  name: string;
  version: string;
}

const res = await fetch("https://registry.npmjs.org/@vaernjs/vaern/latest");
const pkg = (await res.json()) as Manifest;
console.log(`${pkg.name} ${pkg.version}, HTTP ${res.status}`);
```

```bash
vaern run latest.ts
```

## Repository layout

- `crates/vaern-core`: Engine: runtime, TypeScript type stripping, module loader, `fetch`, HTTP server.
- `crates/vaern-cli`: The `vaern` command-line binary.
- `packages/vaern`: Distribution package published to npm as `@vaernjs/vaern`.
- `packages/vaern-<os>-<arch>`: Platform packages `@vaernjs/vaern-<os>-<arch>` that carry the native binary, built and published by CI.
- `tests/e2e`, `examples`, `bench`: End-to-end fixtures, runnable examples and benchmarks.
- `scripts/`: Release validation, pre-publish gates, and version management utilities.
- `.github/workflows/`: Continuous integration and the npm release pipeline.

## Licence

MIT License. Copyright (c) 2026 DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED.
See [LICENSE](./LICENSE).

## Links

- Website: <a href="https://vaernjs.tech">vaernjs.tech</a>
- DevLogix: <a href="https://devlogix.com.pk">devlogix.com.pk</a>
- Repository: <a href="https://github.com/vaernjs/vaern">github.com/vaernjs/vaern</a>
- Issues: <a href="https://github.com/vaernjs/vaern/issues">github.com/vaernjs/vaern/issues</a>
- npm: <a href="https://www.npmjs.com/package/@vaernjs/vaern">@vaernjs/vaern</a>
