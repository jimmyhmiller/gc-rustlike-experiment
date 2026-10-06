# Release AOT comparison against Rust and the JVM

Run from the repository root:

```sh
cargo build --workspace --release
python3 bench/comparison/run.py
```

Requires macOS (`/usr/bin/time -l` RSS units are bytes), Python 3, clang,
rustc, javac, and java. No language workloads run concurrently. Generated
sources and binaries live under `target/performance-comparison`.

The harness retains upstream source headers, changes input sizes, renames the
entry point, and adds an in-process monotonic clock wrapper. Each fresh process
runs 20 discarded warmup iterations and 10 measured iterations. There are three
fresh process forks per language; order is shuffled with a recorded fixed seed.
Every iteration must match the complete ordered numeric output sequence:
integers exactly, floats with relative tolerance 1e-7 and absolute tolerance
1e-8. A failure stops measurement. No best-of selection is used. Raw warmup and
measured timings, signatures, commands, GC counters, process wall times, RSS,
source hashes, toolchain versions, and revision are retained in results.json.

Timed intervals include workload initialization, computation, checksum output,
and cleanup that happens before the workload returns. They exclude the timing
record output. GC may occur in later iterations; full-process GC counters include
warmup. Rust recursively frees trees before returning; tracing GCs may postpone
reclamation. This is the cost of repeated workloads, not identical reclamation
schedules. RSS is the peak across warmup plus measured iterations, not live heap.
`peak_heap_bytes` is a collection-time high-water mark; zero with no collections
is not evidence of zero heap usage. Process wall time includes **all** iterations
and VM startup; it must not be reported as startup latency alone.

Rust defaults to `-C opt-level=2 -C panic=abort` (use `--rust-opt-level 3` for O3); gc-rust now uses its default LLVM O3 AOT
build with its release runtime. Java uses OpenJDK with `-Xms16m -Xmx512m
-XX:+UseG1GC` and an English locale. gc-rust uses a 16 MiB nursery, 256 MiB
per tenured semispace, and one collector worker. These are explicit configurations,
not identical memory budgets. Inherited GCR runtime settings are cleared for
execution. Build time is excluded. The clock C shim uses CLOCK_MONOTONIC through
FFI, with both clock calls outside the workload body. It changes no runtime
memory semantics.

## Interpretation limits

The scalar and array kernels in `kernels/` use matching algorithms and signed
64-bit arithmetic. Intermediate values fit i64. Array initialization, five million
read/modify/write operations over 1024 entries, and checksum reduction are timed.
The scalar recurrence performs ten million dependent steps. Inputs are constants
in all three sources. Output consumption prevents dead-code elimination; inspect
profiles before inferring a specific cause from a ratio.

The four existing suite ports are **whole-program comparisons**, not equivalent
machine representations:

- nbody: gc-rust uses structure-of-arrays; Rust uses inline structs; Java uses
  objects, and versions cache/update fields differently.
- spectralnorm: gc-rust reuses a scratch array; Rust and Java allocate scratch
  arrays repeatedly. Arithmetic/index types also differ.
- fannkuchredux: Rust uses a different permutation algorithm and fixed-size
  storage. gc-rust uses i64 arrays, Java i32 arrays.
- binarytrees: Rust uses Rc and deterministic recursive destruction; Java uses
  nullable object fields; gc-rust uses a traced enum with allocated leaf values.
  Differences in node layout, retention, and root handling are part of the result.

Use the measurements to choose investigations, not to claim language-wide speed.
Warmup is finite; inspect per-fork warmup trends before treating JVM results as
stationary. Host frequency changes, other processes, and collector scheduling can
still affect results. No confidence intervals or universal performance claim are
implied by medians. See ../../bench/suite/README.md for source attribution.

Additional controls: `--rust-opt-level 3`, `--workers 8`, `--nursery-mb 64`,
`--languages gcr` (or rust/java), `--cases binarytrees`, and `--output PATH`.
Selective language runs check within-process signatures; when used as a tuning
comparison, also verify their signatures against the all-language baseline.
Run `python3 bench/comparison/summarize.py` after recording results.json and
rust-o3.json. Run `python3 bench/comparison/profile.py` separately for macOS CPU
sampling and a collection log. See [findings and priorities](INVESTIGATION.md).


The saved initial investigation used gc-rust O2; its historical records remain
unchanged. The optimization follow-up records a fresh checkout of that baseline
and the changed compiler/runtime separately in
[optimization-2026-10-06](optimization-2026-10-06/REPORT.md).
`nbody_objects` is a separate diagnostic variant using a Body object array in
gc-rust, preserving the original nbody benchmark. It helps distinguish source
layout choices from compiler/runtime improvements; its results must be labeled
separately.
