[![npm version](https://img.shields.io/npm/v/%40vaernjs%2Fvaern.svg)](https://www.npmjs.com/package/@vaernjs/vaern)
[![CI](https://github.com/vaernjs/vaern/actions/workflows/ci.yml/badge.svg)](https://github.com/vaernjs/vaern/actions/workflows/ci.yml)
[![licence](https://img.shields.io/github/license/vaernjs/vaern.svg)](./LICENSE)

# vaern

Vaern is an agent-native JavaScript and TypeScript runtime. It is being built so that capability sandboxing, replayable runs and sovereignty enforcement live in the engine instead of in libraries. Maintained by DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED (DevLogix).

## Status

v0.0.3, the first native runtime. `vaern run <file>` executes JavaScript and TypeScript on an embedded QuickJS-ng engine, with TypeScript types stripped by SWC at load time, and provides relative ES module imports, `console`, `setTimeout`, `setInterval`, `queueMicrotask`, `fetch`, an HTTP `serve()` from `vaern:http`, `process.argv`, `process.env` and `process.exit()`. The npm package for this version carries the JavaScript CLI only, so the native binary is built from source. Not in this version: capability sandboxing (`vaern.toml`), `agent()` and `tool()`, and `node:*` modules.

## Install

```bash
git clone https://github.com/vaernjs/vaern.git && cd vaern
cargo build --release
./target/release/vaern --version
```

`npm install -g @vaernjs/vaern` installs the JavaScript CLI. It runs the native binary when `VAERN_BINARY` points at one, and otherwise answers `--version`, `--help` and `doctor` only.

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
- `tests/e2e`: End-to-end fixtures run through the binary.
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
