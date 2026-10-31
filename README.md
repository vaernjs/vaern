[![npm version](https://img.shields.io/npm/v/%40vaernjs%2Fvaern.svg)](https://www.npmjs.com/package/@vaernjs/vaern)
[![CI](https://github.com/vaernjs/vaern/actions/workflows/ci.yml/badge.svg)](https://github.com/vaernjs/vaern/actions/workflows/ci.yml)
[![licence](https://img.shields.io/github/license/vaernjs/vaern.svg)](./LICENSE)

# vaern

Vaern is an agent-native JavaScript and TypeScript runtime. It is being built so that capability sandboxing, replayable runs and sovereignty enforcement live in the engine instead of in libraries. Maintained by DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED (DevLogix).

## Status

v0.0.5, an early native runtime. `@vaernjs/vaern` on npm ships the `vaern` binary for Linux, macOS and Windows.
It runs JavaScript and TypeScript files with `console`, timers, `fetch`, an HTTP `serve()`, `process.argv`, `process.env` and `process.exit()`.
TypeScript enums, namespaces and decorators are rejected with a `file:line:col` error, because they emit runtime code that type stripping cannot produce.
Not implemented yet: `node:*` modules, capability sandboxing (`vaern.toml`), `agent()` and `tool()`.

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
