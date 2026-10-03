# Benchmark evidence

This assessment makes no current performance-ratio claim. Earlier summaries
reported conflicting ratios and mixed heap configurations. They no longer serve
as the project's status documentation.

The repository contains `bench/perf_vs_rust.py`, `bench/run_suite.py`,
`bench/gen_report.py`, benchmark sources, and previously generated JSON/HTML.
Existing output artifacts do not establish current performance; regenerate them
for a recorded checkout and environment before drawing conclusions.

`run_suite.py` compiles gc-rust/Rust/Go/Java versions of nbody, spectralnorm,
fannkuchredux, and binarytrees. It checks numeric outputs before hyperfine timing.
Its output comparison includes float normalization, not a general bit-identical
checksum guarantee. Read its argument/build configuration before changing inputs;
gc-rust inputs are hardcoded in the sources.

For a publishable result, record CPU/OS, toolchain versions, commit and dirty
state, compiler optimization settings, native runtime profile, heap settings,
inputs, correctness checks, warmup, repetitions, raw samples, RSS, and pauses.
Report JVM startup/warmup treatment. Separate throughput, latency, and allocation
workloads, and explain differences in algorithms or resource lifetime.

These harnesses were inspected, not rerun in the current assessment. Measure the
chosen production application before deciding which runtime optimization to build.
