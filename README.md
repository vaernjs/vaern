[![npm version](https://img.shields.io/npm/v/%40vaernjs%2Fvaern.svg)](https://www.npmjs.com/package/@vaernjs/vaern)
[![CI](https://github.com/vaernjs/vaern/actions/workflows/ci.yml/badge.svg)](https://github.com/vaernjs/vaern/actions/workflows/ci.yml)
[![licence](https://img.shields.io/github/license/vaernjs/vaern.svg)](./LICENSE)

# vaern

Vaern is an agent-native JavaScript and TypeScript runtime with capability sandboxing, deterministic replay and sovereignty enforcement built into the engine. It isolates agent workloads by denying ambient authority by default and journaling boundary effects. Maintained by DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED (DevLogix).

## Status

Pre-release placeholder (v0.0.1).
This release reserves the package name on npm and provides a working CLI entry point.
The native engine binary lands in milestone M1 (v0.1.0).

## Install

```bash
npm install -g @vaernjs/vaern
npx @vaernjs/vaern --version
```

From v0.1.0, npm fetches a platform-specific binary via optional dependencies.
Environments without native binary support fall back to the diagnostics CLI.

## What it will look like

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
  sandbox: "vaern.toml",
  replay: "deterministic",
  budget: { tokens: 250_000, usd: 5 },
});

console.log(await courier.run("Reroute today's Lahore deliveries around the fog."));
```

```bash
vaern run courier.ts
vaern audit export --run 7f3a --format sbp --sign
vaern replay 7f3a --diff
```

## Roadmap

| Milestone | Details |
|---|---|
| **M1** | `vaern run hello.ts` — TypeScript, `fetch`, timers, `console`, HTTP server |
| **M2** | capability enforcer and `vaern.toml` (deny by default); `node:fs`, `node:http`, `node:path` subset |
| **M3** | `agent()`, `tool()`, MCP client and server, budgets, effect journal, `vaern replay` |
| **M4** | sovereignty: residency guard, hash-chained audit, PK-PKI signing, `ur-PK` localization |
| **M5** | honest public benchmarks, documentation site, external production users |

We publish benchmarks we lose. A runtime that hides a benchmark is hiding something else.

## Repository layout

- `packages/vaern`: Distribution package published to npm as `@vaernjs/vaern`.
- `scripts/`: Release validation, pre-publish gates, and version management utilities.
- `.github/workflows/`: Continuous integration pipeline.

## Licence

MIT License. Copyright (c) 2026 DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED.
See [LICENSE](./LICENSE).

## Links

- Repository: [https://github.com/vaernjs/vaern](https://github.com/vaernjs/vaern)
- Issues: [https://github.com/vaernjs/vaern/issues](https://github.com/vaernjs/vaern/issues)
- npm package: [https://www.npmjs.com/package/@vaernjs/vaern](https://www.npmjs.com/package/@vaernjs/vaern)
