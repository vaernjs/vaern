# Benchmarks

Measurements for Vaern 0.0.5 against 0.0.4, including the runs Vaern loses.
Machine data and raw values are in [bench.json](bench.json).

All runs: Microsoft Windows 11 Pro (x86_64), 11th Gen Intel(R) Core(TM) i7-11850H @ 2.50GHz (8 cores, 16 threads), 16 GB RAM, 2026-10-31.
Wall time is the hyperfine mean ± σ with `-N`, 3 warmups and 20 runs. Peak RSS is the largest peak working set over 5 runs.

| Benchmark | 0.0.5 | 0.0.4 | Comparison | Peak RSS (0.0.5) | Status |
|---|---|---|---|---|---|
| Startup and greeting | 11.6 ms | 14.5 ms | `node -e 0`: 39.8 ms | 8.0 MB | Win |
| Loopback fetch and JSON parse | 14.4 ms | 33.7 ms | | 10.3 MB | Win vs 0.0.4 |
| Arithmetic loop (1M) | 40.0 ms | 45.8 ms | Node: 107.9 ms | 8.4 MB | Win (Node startup dominates) |
| Arithmetic loop (50M) | 1.347 s | 1.410 s | Node: 0.245 s | | **Loss, 5.5x slower than Node** |
| Parallel HTTP (100 requests) | 47.6 ms | 50.2 ms | | 19.7 MB | Within noise |
| Idle HTTP server, CPU over 3 s | 0.0 ms | 78.1 ms | | | Win vs 0.0.4 |

Binary size, stripped: 15,851,008 bytes (0.0.4: 16,304,640). This is 122,368 bytes over the 15 MiB budget.

## Reproduction commands

Build first with `cargo build --release`.

### Startup

```bash
hyperfine -N --warmup 3 --runs 20 'target/release/vaern run examples/hello.ts' 'node -e 0'
```

### Loopback fetch

```bash
hyperfine -N --warmup 3 --runs 20 'target/release/vaern run examples/fetch-json.ts'
```

### Compute loops

At 1M iterations Node's process startup outweighs the loop. At 50M the loop dominates and Node wins: Vaern interprets QuickJS bytecode without a JIT, while V8 compiles the loop to machine code.

```bash
hyperfine -N --warmup 3 --runs 20 'target/release/vaern run bench/compute-loop.ts' 'node bench/compute-loop.ts'
```

```bash
hyperfine -N --warmup 2 --runs 10 'target/release/vaern run bench/compute-heavy.ts' 'node bench/compute-heavy.ts'
```

### Parallel HTTP

```bash
hyperfine -N --warmup 3 --runs 20 'target/release/vaern run tests/e2e/serve_concurrent.ts'
```

### Idle server CPU

Pass any number of binaries to compare them. Windows reports process CPU time in 15.6 ms steps.

```bash
node bench/idle-cpu.mjs target/release/vaern
```
