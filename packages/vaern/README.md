# vaern

**An agent-native JavaScript & TypeScript runtime** — capability sandboxing, tool-calling, MCP, deterministic replay and sovereignty enforcement built into the engine, not bolted on as a library.

A DevLogix project · licensed MIT

---

> ### Status: pre-release placeholder (v0.0.1)
>
> This version reserves the name and provides a working CLI entry point.
>
> Running `vaern` today prints the roadmap. That is expected, not a bug. Run `vaern doctor` for install diagnostics.

---

## Install

```bash
npm install -g @vaernjs/vaern      # global CLI
npx @vaernjs/vaern --version       # without installing
```

From **v0.1.0** this pulls a single prebuilt binary for your platform via
`optionalDependencies`, the same way `bun` and `esbuild` do. Nothing is
downloaded in a `postinstall` script; the platform packages are ordinary npm
packages gated by `os` and `cpu`.

Supported platforms (v0.1.0+):

| Platform | Package |
|---|---|
| Linux x86_64 | `vaern-linux-x64` |
| Linux arm64 | `vaern-linux-arm64` |
| macOS arm64 | `vaern-darwin-arm64` |
| macOS x86_64 | `vaern-darwin-x64` |
| Windows x86_64 | `vaern-win32-x64` |

Install with `--no-optional` or on an unsupported platform and you get the JS
fallback, which explains itself instead of failing silently.

## What vaern will do

```ts
// courier.ts
import { agent, tool } from "vaern:agent";

const traffic = tool({
  name: "traffic",
  schema: { city: "string", at: "string" },
  run: async ({ city, at }) =>
    (await fetch(`https://api.example.pk/v2/${city}?at=${at}`)).json(),
});

const courier = agent({
  name: "courier-copilot",
  model: "any-openai-compatible-endpoint",
  tools: [traffic],
  sandbox: "vaern.toml",                 // capability policy, enforced by the engine
  replay: "deterministic",               // every effect journalled at the engine boundary
  budget: { tokens: 250_000, usd: 5 },   // hard stop — not an alert
});

console.log(await courier.run("Reroute today's Lahore deliveries around the fog."));
```

```bash
vaern run courier.ts
vaern audit export --run 7f3a --format sbp --sign
vaern replay 7f3a --diff
```

## Programmatic use

```js
const { binaryPath, hasNativeBinary, vaern } = require("@vaernjs/vaern");

if (hasNativeBinary()) vaern(["run", "app.ts"]);
```

## Roadmap

| | Milestone |
|---|---|
| **M1** | `vaern run hello.ts` — TypeScript, `fetch`, timers, `console`, HTTP server |
| **M2** | capability enforcer + `vaern.toml`; `node:fs` / `node:http` / `node:path` subset |
| **M3** | `agent()`, `tool()`, MCP client/server, budgets, effect journal, `vaern replay` |
| **M4** | residency egress guard, hash-chained audit, PK-PKI signing, `ur-PK` i18n |
| **M5** | honest public benchmarks (including losses), docs site, external production users |

We publish benchmarks we lose. A runtime that hides a benchmark is hiding something else.

## Licence

MIT License. Copyright (c) 2026 DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED.
See [`LICENSE`](./LICENSE).

## Development

```bash
git clone https://github.com/vaernjs/vaern.git && cd vaern
npm install
npm run check          # pre-publish validation
npm run pack:dry       # inspect exactly what would be published
```

Issues and pull requests welcome — we would rather be corrected now than wrong later.
