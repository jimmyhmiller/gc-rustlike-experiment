# Measured performance gaps

Host: macOS-26.5.2-arm64-arm-64bit-Mach-O, arm64. Revision: `daad37f6db480445c6df2e65c4208d2270511f84`.
Rust: rustc 1.96.0 (ac68faa20 2026-05-25). JVM: openjdk version "21.0.1" 2023-10-17.
Three process forks; 20 warmups and 10 measured iterations per fork.
See [methodology and representation differences](README.md); timings are milliseconds.

| Workload | gc-rust | Rust O2 | Rust O3 | JVM G1 | gc-rust / Rust O3 | gc-rust / JVM |
|---|---:|---:|---:|---:|---:|---:|
| nbody | 157.373 | 96.946 | 31.766 | 52.036 | 4.95× | 3.02× |
| spectralnorm | 42.954 | 39.839 | 40.290 | 71.945 | 1.07× | 0.60× |
| fannkuchredux | 29.258 | 33.144 | 13.193 | 20.551 | 2.22× | 1.42× |
| binarytrees | 303.038 | 81.678 | 81.049 | 10.282 | 3.74× | 29.47× |
| scalar | 43.791 | 43.496 | 44.664 | 50.127 | 0.98× | 0.87× |
| array | 6.604 | 4.026 | 4.008 | 6.445 | 1.65× | 1.02× |

## Variation and memory

Ranges below are medians of individual process forks, not confidence intervals. RSS includes warmup.

| Workload | Language | Fork median range (ms) | Median peak RSS (MiB) |
|---|---|---:|---:|
| nbody | gcr | 157.218–157.491 | 2.2 |
| nbody | rust | 96.419–97.654 | 1.6 |
| nbody | java | 50.929–52.075 | 52.3 |
| spectralnorm | gcr | 42.903–43.428 | 2.8 |
| spectralnorm | rust | 39.282–40.387 | 1.6 |
| spectralnorm | java | 71.141–72.237 | 51.5 |
| fannkuchredux | gcr | 29.067–29.329 | 2.1 |
| fannkuchredux | rust | 31.864–33.741 | 1.6 |
| fannkuchredux | java | 18.550–20.829 | 47.8 |
| binarytrees | gcr | 299.261–310.822 | 233.8 |
| binarytrees | rust | 81.141–84.090 | 3.6 |
| binarytrees | java | 9.727–10.566 | 231.4 |
| scalar | gcr | 43.099–44.650 | 2.1 |
| scalar | rust | 43.302–43.843 | 1.5 |
| scalar | java | 49.961–52.714 | 42.6 |
| array | gcr | 6.597–6.745 | 2.3 |
| array | rust | 4.022–4.111 | 1.6 |
| array | java | 6.343–6.521 | 44.5 |

## Collector contribution

Full-process counters include all warmup and measured iterations. Pause share divides total pauses by summed workload intervals; it is not a measured-iteration-only fraction.

| Workload | Minor / major collections (first fork) | Allocated bytes per iteration | Pause share across forks | Maximum pause across forks (ms) |
|---|---:|---:|---:|---:|
| nbody | 0 / 0 | 480 | 0.0–0.0% | 0.000 |
| spectralnorm | 0 / 0 | 24,104 | 0.0–0.0% | 0.000 |
| fannkuchredux | 0 / 0 | 320 | 0.0–0.0% | 0.000 |
| binarytrees | 230 / 0 | 128,887,632 | 54.3–54.9% | 34.677 |
| scalar | 0 / 0 | 32 | 0.0–0.0% | 0.000 |
| array | 0 / 0 | 8,248 | 0.0–0.0% | 0.000 |
