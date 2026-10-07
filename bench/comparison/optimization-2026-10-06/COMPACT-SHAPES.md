# Compact enum shapes and root lifetimes

The original tree workload is now **13.42 ms versus Java 9.36 ms** in three
forks, down from 18.55 ms in the private-array milestone. JVM parity is still
open for trees and fannkuch; this checkpoint is not a completion claim.

| Workload | gcr ms | Rust O3 ms | Java 21 G1 ms |
|---|---:|---:|---:|
| nbody | 40.181 | 26.755 | 43.589 |
| spectralnorm | 36.509 | 35.060 | 61.956 |
| fannkuchredux | 17.168 | 12.943 | 16.363 |
| binarytrees | 13.423 | 70.416 | 9.365 |
| scalar | 39.367 | 38.879 | 44.824 |
| array | 4.860 | 4.033 | 6.454 |
| nbody_objects (separate diagnostic) | 120.520 | 26.873 | 43.940 |

Same sources and inputs, three fresh processes, 20 warmups and 10 measured
iterations each, one GC worker, 16 MB nursery and 256 MB tenured space. All
ordered numeric outputs match. Source hashes match the preceding milestone;
implementation hashes in `compact-shape-results.json` match this checkpoint.

Reference-enum tags occupy the existing Full header's low 32-bit word. Empty
variants use a header-only collector shape; payload variants keep their union
shape. Source type IDs, names, object identity and exact allocation counts are
preserved. A tree leaf uses 16 bytes and a node 32, instead of 40 for either.
Each 30-iteration tree process still performs **96,665,730 constructions**;
allocated bytes fall from 3,866,628,960 to **2,314,735,680**. Dropping dead local
roots reduces promoted bytes from 102,681,568 after compact storage alone to
**36,377,568**, and collector pause time from around 88–91 ms to around 33 ms
per process.

The pause coordinator closes generated allocation windows before resumption;
compiled capacity checks force native epoch validation after reset. Constructor
fast paths initialize the header once and clear only payload bytes not fully
written by scalar/reference fields. Inline-value payloads retain conservative
clearing. All fields are initialized before extent publication. First use of an
allocation site uses the native allocator to grow its counters, keeping the
steady fast path free of native calls. Root liveness handles branch successors,
loop fixed points, break, continue, return and assignment; full-debug retains
lexical roots. Field addresses preserve pointer provenance with ordinary GEP.

New code links against `gcr_runtime_main_v2`, ensuring a custom older archive
cannot silently lack the window-invalidation contract. The legacy startup entry
is retained for older generated programs. A dedicated archive regression checks
the link-time rejection.

Validation: 547 workspace tests passed, eight ignored; 22 instrumented native
runs and seven runtime tests passed ThreadSanitizer. After startup symbol
versioning, four archive/reachability gates passed and the complete three-fork
comparison was rebuilt and rerun. See `compact-shape-validation.json` for the
exact sequencing. Normal/stress JIT, native and full-debug enum fixtures check
parallel moving GC, physical sizes, inline-reference payloads and nominal IDs;
collector tests retain maximal tags through copying; snapshots distinguish dead
optimized roots from full-debug lexical roots.

Unconditional root reload, global inline hints and one-level recursive expansion
were measured and rejected; their raw records and patches are retained. A
counter-disabled diagnostic isolated roughly 1 ms of profiling overhead. It is
explicitly excluded from parity claims; all checkpoint comparisons keep exact
counters enabled. Fannkuch regressed modestly relative to the preceding stage;
root-slot selection is a follow-up target. Assembly inspection identifies stack
save/restore on leaf paths as the next tree target.
