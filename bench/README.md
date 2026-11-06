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

## Unreleased hardening against 0.0.6

Re-run on 2026-11-07 on the same machine, after the fetch body cap, the pool timer and the streaming backpressure change. The 0.0.6 binary was built from the `v0.0.6` tag in the same session; each row is one hyperfine call with 0.0.6 listed first, using the commands below, and startup and parallel HTTP were repeated with the new build first. Absolute times are lower than on 2026-11-04 for both builds; compare within a row only.

| Benchmark | Unreleased | 0.0.6 | Comparison | Status |
|---|---|---|---|---|
| Startup and greeting | 10.7 ms ± 1.1 | 10.1 ms ± 0.6 | `node -e 0`: 39.3 ms | **Loss, +0.6 ms** (+0.7 ms with the new build first) |
| Loopback fetch and JSON parse | 16.1 ms ± 3.7 | 17.3 ms ± 5.8 | | Within noise |
| Arithmetic loop (1M) | 38.2 ms ± 1.4 | 38.8 ms ± 2.3 | Node: 86.9 ms | Within noise; win vs Node |
| Arithmetic loop (50M) | 1.415 s ± 0.043 | 1.412 s ± 0.045 | Node: 0.262 s | **Loss, 5.4x slower than Node**; unchanged from 0.0.6 |
| Parallel HTTP (100 requests) | 50.0 ms ± 3.7 | 48.2 ms ± 3.4 | | Within noise (−1.2% with the new build first) |
| Idle HTTP server, CPU over 3 s | 0.0 ms | 0.0 ms | | Unchanged |

The startup loss has not been profiled. The likely cause is the larger JavaScript prelude evaluated at every start (the managed-header list, the stream wait map and the `TextDecoder` option checks); the fetch client and its pool timer are built only on the first fetch, so `hello.ts` does not pay for them.

Binary size, stripped: 16,166,912 bytes (0.0.6: 16,103,936, +62,976), 438,272 bytes over the 15 MiB budget.

### With fat LTO

The release profile then gained `lto = "fat"` and `codegen-units = 1`. The same comparison, same session, 0.0.6 listed first:

| Benchmark | Unreleased, LTO | 0.0.6 | Comparison | Status |
|---|---|---|---|---|
| Startup and greeting | 10.4 ms ± 1.3 | 10.9 ms ± 0.8 | | Within noise (−0.4 ms with the new build first); the loss above is gone |
| Loopback fetch and JSON parse | 14.2 ms ± 3.0 | 15.0 ms ± 2.5 | | Within noise |
| Arithmetic loop (1M) | 38.9 ms ± 1.7 | 39.9 ms ± 2.3 | | Within noise |
| Arithmetic loop (50M) | 1.348 s ± 0.026 | 1.409 s ± 0.052 | Node: 0.249 s | 4% faster than 0.0.6; **loss, 5.4x slower than Node** |
| Parallel HTTP (100 requests) | 47.2 ms ± 2.7 | 45.4 ms ± 3.7 | | Within noise |
| Idle HTTP server, CPU over 3 s | 0.0 ms | 0.0 ms | | Unchanged |

Binary size, stripped: 9,427,456 bytes (0.0.6: 16,103,936, −6,676,480), 6,301,184 bytes under the 15 MiB budget. The cost is build time: a cold `cargo build --release` takes about 10 minutes on this machine instead of about 2.

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
