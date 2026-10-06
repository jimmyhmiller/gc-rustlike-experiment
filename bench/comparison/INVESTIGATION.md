# Performance investigation, 2026-10-06

This describes the baseline at `35b8157`, including historical source locations
and optimization priorities. Implemented fixes and new measurements are in the
[optimization follow-up](optimization-2026-10-06/REPORT.md).

The largest measured gap is allocation-heavy binary trees. The strongest concrete
runtime finding is repeated full-tenured indexing during minor GC. Scalar
arithmetic is competitive; managed array access and optimized numeric/permutation
ports need separate attention. [Measurements](MEASUREMENTS.md) contain timings,
fork variation, memory, and GC counters. [Raw records](results.json) retain every
iteration. Additional Rust O3 runs are in [rust-o3.json](rust-o3.json).

## 1. Minor GC repeatedly walks the entire occupied tenured space

`crates/gcrust-rt/src/gc/parallel.rs:98` builds an object-start index **before**
examining dirty cards and starting evacuation workers. The implementation at
`crates/gcrust-rt/src/gc/heap.rs:2530` walks initialized tenured objects, then
forward-fills a table covering all cards. This work happens even when the dirty
set is tiny. Cost scales with accumulated promoted objects, including objects
that are dead but not yet reclaimed by a major collection.

The [sample profile](tree-profile.txt) identifies index construction, heap walking,
and allocation size/header validation as the leading stacks. Index closure plus
index function account for 669 of 2491 top-of-stack samples; heap walking accounts
for another 286, although the profile captures only three seconds of one process.
This is sampled evidence, not a complete-run CPU accounting.

The [GC log](tree-gc-log.jsonl) corroborates growth: median pause for the first
20 collections is 3.01 ms; the last 20 is 30.05 ms. There are no major collections
in this run. This is accumulation between major collections, not evidence of an
unbounded memory leak. The tree workload should therefore be described as repeated
warm execution; it has not reached a stationary heap state.

Baseline: 303.038 ms per tree workload, 230 minors, about 54% of full-process
workload time in GC pauses. Matched checksum/input sensitivity runs:

| Configuration | Median (ms) | Minor collections | Total pauses (ms) | Peak RSS (MiB) |
|---|---:|---:|---:|---:|
| 16 MiB nursery, 1 worker, three forks | 303.038 | 230 per fork | 3865.65 in first fork | 233.8 median |
| 16 MiB nursery, 8 workers, one fork | 311.181 | 230 | 3932.90 | 233.8 |
| 64 MiB nursery, 1 worker, one fork | 141.492 | 57 | 734.04 | 207.4 |

The 8-worker setting matches this host's default worker count. These additional
single-fork runs are diagnostic, not precise estimates of tuning gains. A larger
nursery avoids many index rebuilds and reduces premature promotion in this case.
It does not repair indexing complexity, and its RSS effect is workload-specific.

**First implementation priority:** maintain object-start metadata as objects are
allocated/promoted, rebuilding it when a major GC relocates the tenured space.
The design must account for TLAB holes, initialized ranges, object/card overlap,
large variable-size objects, and parallel promotion publication. Test those
invariants, including race and relocation stress, rather than removing the safety
checks or skipping dirty-card roots.

## 2. Allocation hot path spends time validating and publishing roots

The same profile shows substantial time in `checked_allocation_size`,
`accepts_header`, `Tlab::alloc`, and `alloc_with_published_frame`, in addition to
zeroing allocations. The tree program allocates approximately 3.22 million objects
and 128.89 MB per workload iteration (40 bytes per object on average). Java object
layout and Rust Rc ownership differ, so a collector-only speed claim would be
misleading.

**Second priority:** measure and specialize fixed-layout allocation fast paths
while retaining overflow/alignment checks and the allocator/root contract. Audit
root liveness to avoid unnecessary promotions. Neither reducing validation nor
changing roots is justified merely by a profile; each needs a sound invariant
and focused correctness tests.

## 3. Managed array operations inhibit ordinary memory optimization

The matched i64 array kernel takes 6.604 ms versus Rust O3's 4.008 ms (1.65×) and
Java's 6.445 ms (1.02×), with no collections. The matched scalar recurrence is
approximately equal to Rust. This isolates a memory/loop-code difference much
better than the differently represented nbody ports.

`src/codegen.rs:662` and `:680` emit sequentially consistent managed loads/stores.
`array_get_unchecked` still performs an actual backing-array bounds check, and
loop headers retain a safepoint poll. The [nbody assembly](nbody-assembly.txt)
contains 43 static LDAR/STLR instructions across workload, advance, and energy;
these are static instruction sites, not execution counts. This confirms atomic
code generation; it does **not** quantify the fraction of the gap caused by it.

**Third priority:** inspect LLVM optimization remarks and dependence/vectorization
barriers. Establish a proper thread-confined memory model/ownership proof before
eliding managed atomic accesses for private storage. Preserve shared-memory safety.
Ordinary Vec and Map should remain unsynchronized, with concurrent variants
separate; the benchmark changes no collection semantics.

## 4. Compiler optimization level and representation matter

Rust O3 improves nbody from 96.95 to 31.77 ms and fannkuch from 33.14 to 13.19 ms.
gc-rust currently exposes O2 AOT, so O2 alone substantially understates those
practical release gaps. Keep both baselines. The existing nbody and fannkuch
algorithms/layouts differ; construct matched representation variants and inspect
optimized IR before attributing these changes to one compiler pass.

Spectralnorm is within 7% of Rust O3 and faster than this JVM port. gc-rust reuses
scratch storage where the other ports allocate it, so this is a useful application
result, not evidence that all numeric loops are faster.

## Reproduction and validation

```sh
cargo build --workspace --release
python3 bench/comparison/run.py
python3 bench/comparison/run.py --languages rust --rust-opt-level 3 --output bench/comparison/rust-o3.json
python3 bench/comparison/run.py --cases binarytrees --languages gcr --forks 1 --workers 8 --output bench/comparison/workers8.json
python3 bench/comparison/run.py --cases binarytrees --languages gcr --forks 1 --nursery-mb 64 --output bench/comparison/nursery64.json
python3 bench/comparison/profile.py
python3 bench/comparison/summarize.py
```

All six workloads passed ordered numeric comparisons for every baseline iteration.
Every additional tuning/O3 iteration was also checked against the recorded baseline
signature. Integer order/length/large-integer mismatches and float tolerance checks
were exercised directly. Python syntax checks and the release workspace build passed.
No compiler or runtime source was changed. Startup-only, p99 application latency,
Map/String/value-array layouts, actual CSV/graph app throughput, larger heaps with
major collections, and broader machines remain unmeasured.
