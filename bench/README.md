# Benchmarks

Measurements for Vaern 0.0.6 against 0.0.5, including the runs Vaern loses.
Machine data and raw values are in [bench.json](bench.json).

All runs: Microsoft Windows 11 Pro (x86_64), 11th Gen Intel(R) Core(TM) i7-11850H @ 2.50GHz (8 cores, 16 threads), 16 GB RAM, 2026-11-04.
Wall time is the hyperfine mean ± σ with `-N` and 3 warmups, over 30 runs for startup and fetch, 20 for the others and 10 for the 50M loop. Each table row comes from one hyperfine call with 0.0.5 listed first; a second session with 0.0.6 first gave the same verdicts except where noted. Peak RSS is the largest peak working set over 5 runs.

| Benchmark | 0.0.6 | 0.0.5 | Comparison | Peak RSS (0.0.6 / 0.0.5) | Status |
|---|---|---|---|---|---|
| Startup and greeting | 15.7 ms | 14.4 ms | `node -e 0`: 42.2 ms | 7.9 / 8.0 MB | **Loss vs 0.0.5, +1.3 ms**; win vs Node |
| Loopback fetch and JSON parse | 24.0 ms | 23.1 ms | | 10.3 / 10.4 MB | Within noise (σ 8.6 and 4.9 ms) |
| Arithmetic loop (1M) | 42.6 ms | 42.7 ms | Node: 83.2 ms | 8.2 / 8.4 MB | Win vs Node (Node startup dominates) |
| Arithmetic loop (50M) | 1.401 s | 1.383 s | Node: 0.252 s | | **Loss, 5.6x slower than Node**; within noise of 0.0.5 |
| Parallel HTTP (100 requests) | 50.0 ms | 50.1 ms | | 17.4 / 19.6 MB | Same time, 2.2 MB less memory |
| Idle HTTP server, CPU over 3 s | 0.0 ms | 0.0 ms | | | Unchanged |

The startup loss held in both orders (+1.1 ms with 0.0.6 first). It has not been profiled yet; the startup prelude grew in this release with `URL`, `TextDecoder` and `vaern:fs`. With 0.0.6 first, the 50M loop read 8% slower than 0.0.5, which did not hold with 0.0.5 first, so it is reported as noise.

Binary size, stripped: 16,103,936 bytes (0.0.5: 15,860,224, +243,712). This is 375,296 bytes over the 15 MiB budget.

## Reproduction commands

Build first with `cargo build --release`. To compare two versions, pass both binaries to the same hyperfine call.

### Startup

```bash
hyperfine -N --warmup 3 --runs 30 'target/release/vaern run examples/hello.ts' 'node -e 0'
```

### Loopback fetch

```bash
hyperfine -N --warmup 3 --runs 30 'target/release/vaern run examples/fetch-json.ts'
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
