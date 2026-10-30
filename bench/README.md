# Benchmarks

Runtime execution performance measurements for Vaern.

## Performance Table

All measurements recorded on Microsoft Windows 11 Pro (x86_64) with 11th Gen Intel(R) Core(TM) i7-11850H @ 2.50GHz (8 cores, 16 threads), 16 GB RAM.

| Benchmark | Command | Date | Cold Start | Warm Start | RSS | Status |
|---|---|---|---|---|---|---|
| Startup & greeting | `vaern run examples/hello.ts` | 2026-10-29 | 27.1 ms | 18.0 ms | 16.2 MB | Win vs Node (68.3 ms) |
| HTTPS Fetch & parse | `vaern run examples/fetch-json.ts` | 2026-10-29 | 41.5 ms | 26.8 ms | 24.1 MB | Baseline |
| Arithmetic loop (1M) | `vaern run bench/compute-loop.ts` | 2026-10-29 | 38.0 ms | 28.5 ms | 18.5 MB | Loss vs Node (9.8 ms warm) |
| Parallel HTTP (100 req) | `vaern run tests/e2e/serve_concurrent.ts` | 2026-10-29 | 58.2 ms | 44.0 ms | 28.6 MB | Baseline (p99 < 50ms) |

## Reproduction Commands

### 1. Hello Startup

```bash
hyperfine --warmup 3 'target/release/vaern run examples/hello.ts'
```

### 2. HTTPS Fetch

```bash
hyperfine --warmup 3 'target/release/vaern run examples/fetch-json.ts'
```

### 3. Compute Loop Comparison (Loss)

Vaern interprets bytecode without a JIT compiler to maintain predictable memory and sandboxing boundaries. Node.js V8 JIT compiles tight loops to machine code, outperforming Vaern on long-running arithmetic loops.

```bash
hyperfine --warmup 3 'target/release/vaern run bench/compute-loop.ts' 'node bench/compute-loop.ts'
```

### 4. Parallel HTTP Concurrency

```bash
target/release/vaern run tests/e2e/serve_concurrent.ts
```
