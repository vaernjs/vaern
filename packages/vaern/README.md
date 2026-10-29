# vaern

An agent-native JavaScript and TypeScript runtime. Vaern runs TypeScript directly, with `fetch` and an HTTP server built in, on an embedded QuickJS-ng engine driven by Rust and Tokio. It is being built so that capability sandboxing, replayable runs and sovereignty enforcement live in the engine instead of in libraries.

A DevLogix project, licensed MIT.

## Status

v0.0.3, the first native runtime. `vaern run <file>` executes JavaScript and TypeScript with types stripped at load time, and provides relative ES module imports, `console`, `setTimeout`, `setInterval`, `queueMicrotask`, `fetch`, `serve()` from `vaern:http`, `process.argv`, `process.env` and `process.exit()`. This npm package carries the JavaScript CLI only, so the native binary is built from source. Not in this version: capability sandboxing (`vaern.toml`), `agent()` and `tool()`, and `node:*` modules.

## Install

```bash
npm install -g @vaernjs/vaern
git clone https://github.com/vaernjs/vaern.git && cd vaern
cargo build --release
export VAERN_BINARY="$PWD/target/release/vaern"
vaern --version
```

With `VAERN_BINARY` set, the `vaern` command runs the native binary. Without it, the command answers `--version`, `--help` and `doctor` only.

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

## Programmatic use

```js
const { binaryPath, hasNativeBinary, vaern } = require("@vaernjs/vaern");

if (hasNativeBinary()) vaern(["run", "app.ts"]);
```

## Licence

MIT License. Copyright (c) 2026 DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED.
See [`LICENSE`](./LICENSE).

## Links

- Website: <a href="https://vaernjs.tech">vaernjs.tech</a>
- DevLogix: <a href="https://devlogix.com.pk">devlogix.com.pk</a>
- Repository: <a href="https://github.com/vaernjs/vaern">github.com/vaernjs/vaern</a>
- Issues: <a href="https://github.com/vaernjs/vaern/issues">github.com/vaernjs/vaern/issues</a>
- npm: <a href="https://www.npmjs.com/package/@vaernjs/vaern">@vaernjs/vaern</a>

## Development

```bash
git clone https://github.com/vaernjs/vaern.git && cd vaern
cargo build --release
npm run check          # pre-publish validation
npm run pack:dry       # inspect exactly what would be published
```
