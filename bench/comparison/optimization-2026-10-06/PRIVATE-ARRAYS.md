# Private-array and safepoint optimization milestone

Original nbody now beats the JVM in the three-fork comparison: **40.24 ms versus 43.85 ms**, down from 118.16 ms at the previous milestone. Fannkuch is approximately at parity (16.56 versus 16.49 ms, a 0.4% difference); trees still need work. The separately labeled object-array nbody diagnostic retains a large gap.

| Workload | Previous gc-rust ms | gc-rust ms | Java ms | Rust O3 ms | gc-rust / Java |
|---|---:|---:|---:|---:|---:|
| nbody | 118.16 | 40.237 | 43.851 | 26.619 | 0.918 |
| spectralnorm | 36.784 | 36.824 | 62.066 | 35.187 | 0.593 |
| fannkuchredux | 22.723 | 16.563 | 16.49 | 13.182 | 1.004 |
| binarytrees | 18.501 | 18.55 | 9.397 | 70.454 | 1.974 |
| scalar | 36.209 | 38.979 | 44.466 | 38.866 | 0.877 |
| array | 5.924 | 4.893 | 6.306 | 4.004 | 0.776 |
| nbody_objects | 114.321 | 113.656 | 43.543 | 26.687 | 2.61 |

The machine, heap settings, inputs, source hashes and timing methodology match the previous milestone: three process forks, 20 discarded warmups and 10 samples per fork; complete ordered numeric checks on every iteration. All benchmark sources remain unchanged. See [raw records](private-array-results.json), [summary](private-array-summary.json), [validation](private-array-validation.json) and the [methodology](../README.md). Host frequency variation remains visible in the scalar/Rust control; small ratio differences do not establish a reliable ordering.

The compiler now proves private scalar-array origins through aliases and direct calls/returns, specializes accesses only when all origins are unescaped, propagates compatible immutable lengths and integer constants, and supplies access-scoped alias metadata only for disjoint private origin sets. Heap/capture/foreign/unknown escapes retain shared SC accesses. Helpers are closed to external callers for these whole-program proofs; native declarations and execution entry retain their ABI. Full-debug code keeps generic editable references.

Small induction-proven loops coalesce polls into function entry while other loops retain header polls. When the remaining body is also proven nonrelocating, entry frames exist only on the slow polling path and trace only incoming references. They are unlinked after relocation refresh and before the bounded body. Bounds checks are removed only through proven real backing-array lengths. No fast-math reassociation is enabled.

Removed invariant.load from dynamic array-length loads: immutable object metadata does not justify a physical-memory invariance claim across arena reuse. Static length facts instead derive from object origins. Alias tests include overlapping parameter origins and differing/nullable lengths; the relocation fixture retains private aliases across native allocations and a concurrently collecting sibling mutator.

Passed 542 workspace tests (eight ignored), 20 native ThreadSanitizer runs including the new private-array fixture, seven runtime sanitizer tests, release compilation and all seven cross-language workloads. Production next targets are enum allocation layout and automatic GC worker scheduling; JVM parity remains open until those tree results and remaining comparisons are resolved.
