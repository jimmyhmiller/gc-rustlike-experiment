# Compiler and GC optimization results — 2026-10-06

The original binarytrees workload improved from 270.94 ms to 18.50 ms. JVM parity is not yet achieved across the suite. Scalar, array and spectralnorm meet or beat the Java medians at these inputs; trees, fannkuch and nbody still need work.

## Measurements

| Workload | Baseline gc-rust ms | Optimized gc-rust ms | Rust O3 ms | Java ms | Speedup | gc-rust / Java |
|---|---:|---:|---:|---:|---:|---:|
| nbody | 133.606 | 118.160 | 26.493 | 43.384 | 1.13× | 2.72× |
| spectralnorm | 37.336 | 36.784 | 34.263 | 61.454 | 1.02× | 0.60× |
| fannkuchredux | 24.486 | 22.723 | 12.870 | 16.037 | 1.08× | 1.42× |
| binarytrees | 270.945 | 18.501 | 68.878 | 9.119 | 14.65× | 2.03× |
| scalar | 39.219 | 36.209 | 36.411 | 41.461 | 1.08× | 0.87× |
| array | 6.654 | 5.924 | 3.972 | 6.281 | 1.12× | 0.94× |
| nbody_objects (diagnostic) | — | 114.321 | 26.259 | 43.596 | — | 2.62× |

Lower time is better. Ratios below 1 mean gc-rust was faster than Java. These are medians of 30 measured iterations across three fresh processes, following 20 discarded warmup iterations per process. See [raw final records](results.json), [fresh baseline control](baseline-control.json), [machine-readable summary](summary.json) and [validation](validation.json).

Both runs used the Apple M2 Max host, Rust O3, OpenJDK 21 with G1, and gc-rust with one collector worker, a 16 MiB nursery and 256 MiB per tenured semispace. Baseline gc-rust is an isolated release build of `35b8157` using its default LLVM O2; optimized gc-rust uses default O3. No original benchmark source changed: all six sets of source hashes match the control. The final raw records identify the pre-commit revision plus exact compiler/runtime source hashes and dirty state. The new object-array nbody case is a separately labeled diagnostic, not a replacement for the original structure-of-arrays case.

Every warmup and measured iteration passed complete ordered numeric comparison: integers exactly, floating point with relative tolerance 1e-7 and absolute tolerance 1e-8. Timed workload intervals include initialization and checksum output, exclude VM startup and timing-record printing, and include reclamation that happens before return. Rust uses Rc and recursively destroys tree nodes; Java and gc-rust have different traced layouts and reclamation schedules. These results are workload comparisons, not equal memory budgets or identical machine representations. Detailed limits are in the [methodology](../README.md).

## Changes

- Tenured card/object indexing now scans only newly initialized ranges and resets with arena epochs. Empty remembered sets avoid indexing. Out-of-order TLAB initialization remains correctly covered.
- Fixed-layout allocation uses validated descriptors and generated owning-mutator nursery TLAB fast paths. Capacity, overflow, stress mode and arena epochs guard the path; runtime refill and collecting fallbacks remain. Site counters and initialized extents stay exact.
- Dirty cards use atomic bitmap words, preserving concurrent mark composition.
- Optimized reference locals have private working slots and traced mirrors. Relocation epochs trigger refresh after collecting or parking calls. Native returns refresh registered mirrors after the mutator becomes RUNNING, including callbacks that collect.
- A conservative call-graph analysis removes relocation checks and frames only from proven nonrelocating functions. Functions without incoming references, captures or indirect roots defer frame registration until the first reference assignment. Full-debug builds keep entry registration and editable roots.
- Release LLVM uses O3 and propagates optimization failures. O3 alone did not explain the major tree gain in the tuning runs.

The tree forks still allocate exactly 96,665,730 objects and 3,866,628,960 bytes across warmup plus measurement, with 230 minor collections and no major collection. Median total collection pause time per full process dropped from 3363.05 ms to 134.33 ms. The optimization did not reduce the benchmark's allocation work. Heap access sequential consistency remains unchanged.

## Validation and remaining work

The final checkout passed 532 workspace tests (8 ignored), 18 native ThreadSanitizer executions across fixtures and real apps in normal and stress modes, and seven instrumented runtime tests. New regressions exercise exact allocation accounting, mixed native/generated allocations, multiworker relocation, recursive root retention, native callbacks, card-word boundaries, TLAB holes, late dirty cards and arena resets. Release compilation and all seven cross-language comparisons passed.

Remaining priorities are private-array ownership and bounds optimization for numeric kernels, followed by tree object layout and the remaining allocation/root overhead. Repeated checks and sequentially consistent accesses are visible in generated numeric code, but their individual contributions need controlled measurements before changing them. The object-array nbody variant still trails Java; layout alone does not close that gap. JVM parity remains an open performance objective.

Intermediate records retain tuning history: [allocation/index stage](allocation-stage.json), [root-frame stage](root-frames-stage.json), [lazy-index stage](lazy-index-stage.json), and [root mirrors before effect analysis](root-mirrors-before-effects.json). The latter exposed a spectralnorm regression from unnecessary relocation checks; effect analysis removed it before final validation. These are intermediate builds with their own source hashes, not measurements of the final checkout. The earlier [object-layout diagnostic](nbody-objects.json) is also a separate stage.
