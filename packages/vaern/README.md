# vaern

An agent-native JavaScript and TypeScript runtime. It is being built so that capability sandboxing, replayable runs and sovereignty enforcement live in the engine instead of in libraries.

A DevLogix project, licensed MIT.

## Status

v0.0.1. This package installs the `vaern` command, which answers `--version`, `--help` and `doctor`, and ships with its README and documentation. This version contains no native runtime, so `vaern run` is not available.

## Install

```bash
npm install -g @vaernjs/vaern      # global CLI
npx @vaernjs/vaern --version       # without installing
```

## What the CLI does today

| Command | Output |
|---|---|
| `vaern --version` | The package version. |
| `vaern --help` | A usage line and the development plan. |
| `vaern doctor` | Install diagnostics: platform, the platform package it looks for, the Node.js version and whether a native binary was found. |

## Programmatic use

```js
const { binaryPath, hasNativeBinary, vaern } = require("@vaernjs/vaern");

if (hasNativeBinary()) vaern(["run", "app.ts"]);
```

`hasNativeBinary()` returns `false` in this version.

## Repository layout

- `packages/vaern`: this package.
- `scripts/`: release validation and pre-publish gates.
- `.github/workflows/`: continuous integration.

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
npm install
npm run check          # pre-publish validation
npm run pack:dry       # inspect exactly what would be published
```
