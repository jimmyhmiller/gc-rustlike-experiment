# Benchmark evidence

Current measurements are linked below. Earlier summaries reported conflicting
ratios and mixed heap configurations; those historical summaries no longer serve
as the project's performance status.

The repository contains `bench/perf_vs_rust.py`, `bench/run_suite.py`,
`bench/gen_report.py`, benchmark sources, and previously generated JSON/HTML.
Historical output artifacts do not establish current performance; regenerate them
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

The legacy harnesses were inspected, not rerun for the new comparison. Measure the
chosen production application before deciding which runtime optimization to build.

## Current measured comparison (2026-10-06)

The optimization follow-up and fresh baseline control are in
[comparison/optimization-2026-10-06/REPORT.md](comparison/optimization-2026-10-06/REPORT.md).
The original release AOT/Rust O2/Rust O3/OpenJDK 21 comparison is available in
[comparison/MEASUREMENTS.md](comparison/MEASUREMENTS.md), with
[methodology](comparison/README.md), [profile-supported findings](comparison/INVESTIGATION.md),
and raw per-iteration records. It uses three process forks and explicit in-process
warmup, checks ordered numeric results, and separates workload intervals from
whole-process wall time. These measurements apply to the documented inputs,
representations, host, and collector settings; the historical artifacts above
remain unsuitable for current ratio claims.
