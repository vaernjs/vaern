# vaern

**An agent-native JavaScript & TypeScript runtime.** Vaern runs TypeScript directly, with `fetch` and an HTTP server built in, on an embedded QuickJS-ng engine driven by Rust and Tokio. Capability sandboxing, tool-calling, MCP and replayable runs are being built into the engine itself.

[VaernJS](https://vaernjs.tech) is a [DevLogix](https://devlogix.com.pk) project · licensed MIT

---

> ### Status: v0.0.4, early native runtime
>
> This release ships the native `vaern` binary for Linux, macOS and Windows.
> The API surface is small and will change before v1.0.

**Works today**

- `vaern run <file>`: JavaScript and TypeScript, with types stripped at load time
- Relative ES module imports
- `console`, `setTimeout` / `setInterval`, `queueMicrotask`
- `process.argv`, `process.env`, `process.exit()`
- `fetch` with `Headers`, `Response`, `AbortController`, redirects and timeouts (hyper and rustls)
- `serve()` from `vaern:http`, an HTTP server with Web-standard `Request` / `Response` and streamed bodies
- Uncaught exceptions, unhandled rejections and throwing timers exit non-zero with a `file:line:col` location

**Not implemented yet**

- `node:*` modules and npm package imports
- Capability sandboxing (`vaern.toml`)
- `agent()`, `tool()`, MCP, budgets and `vaern replay`

TypeScript `enum`, `namespace` and decorators are rejected with a located error, because they emit runtime code that type stripping cannot produce.

---

## Install

```bash
npm install -g @vaernjs/vaern      # global CLI
npx @vaernjs/vaern --version       # without installing
```

npm installs one prebuilt binary for your platform through `optionalDependencies`, the same way `esbuild` does. Nothing is downloaded in a `postinstall` script; the platform packages are ordinary npm packages gated by `os` and `cpu`.

| Platform | Package |
|---|---|
| Linux x86_64 | `@vaernjs/vaern-linux-x64` |
| Linux arm64 | `@vaernjs/vaern-linux-arm64` |
| macOS arm64 | `@vaernjs/vaern-darwin-arm64` |
| macOS x86_64 | `@vaernjs/vaern-darwin-x64` |
| Windows x86_64 | `@vaernjs/vaern-win32-x64` |

Linux binaries need glibc 2.35 or newer. On an unsupported platform, or when optional dependencies are skipped, `vaern` falls back to a small diagnostics CLI. Run `vaern doctor` there to see why.

## Quick start

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

## Links

- Website: <a href="https://vaernjs.tech">vaernjs.tech</a>
- DevLogix: <a href="https://devlogix.com.pk">devlogix.com.pk</a>
- Repository: <a href="https://github.com/vaernjs/vaern">github.com/vaernjs/vaern</a>
- Issues: <a href="https://github.com/vaernjs/vaern/issues">github.com/vaernjs/vaern/issues</a>
- npm: <a href="https://www.npmjs.com/package/@vaernjs/vaern">@vaernjs/vaern</a>

## Licence

MIT License. Copyright (c) 2026 DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED ([DevLogix](https://devlogix.com.pk)).
See [`LICENSE`](./LICENSE).

## Development

```bash
git clone https://github.com/vaernjs/vaern.git && cd vaern
cargo build --release
npm run check          # pre-publish validation
npm run pack:dry       # inspect exactly what would be published
```

Issues and pull requests welcome. We would rather be corrected now than wrong later.
