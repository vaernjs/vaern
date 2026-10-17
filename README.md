[![npm version](https://img.shields.io/npm/v/%40vaernjs%2Fvaern.svg)](https://www.npmjs.com/package/@vaernjs/vaern)
[![CI](https://github.com/vaernjs/vaern/actions/workflows/ci.yml/badge.svg)](https://github.com/vaernjs/vaern/actions/workflows/ci.yml)
[![licence](https://img.shields.io/github/license/vaernjs/vaern.svg)](./LICENSE)

# vaern

Vaern is an agent-native JavaScript and TypeScript runtime. It is being built so that capability sandboxing, replayable runs and sovereignty enforcement live in the engine instead of in libraries. Maintained by DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED (DevLogix).

## Status

v0.0.0, the initial package. `@vaernjs/vaern` installs a `vaern` command with a working entry point that answers `--version`, `--help` and `doctor`. This version contains no native runtime, so `vaern run` is not available.

## Install

```bash
npm install -g @vaernjs/vaern
npx @vaernjs/vaern --version
```

## Repository layout

- `packages/vaern`: Distribution package published to npm as `@vaernjs/vaern`.
- `scripts/`: Release validation, pre-publish gates, and version management utilities.
- `.github/workflows/`: Continuous integration pipeline.

## Licence

MIT License. Copyright (c) 2026 DEVLOGIX TECHNOLOGY (PRIVATE) LIMITED.
See [LICENSE](./LICENSE).

## Links

- Website: <a href="https://vaernjs.tech">vaernjs.tech</a>
- DevLogix: <a href="https://devlogix.com.pk">devlogix.com.pk</a>
- Repository: <a href="https://github.com/vaernjs/vaern">github.com/vaernjs/vaern</a>
- Issues: <a href="https://github.com/vaernjs/vaern/issues">github.com/vaernjs/vaern/issues</a>
- npm: <a href="https://www.npmjs.com/package/@vaernjs/vaern">@vaernjs/vaern</a>
