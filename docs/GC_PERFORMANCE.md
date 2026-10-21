# GC performance evidence

## Pointerless copy tasks, 2026-10-20

Evacuation omits payload scan tasks for layouts with no fixed/interior references
and no nonempty reference-valued tail. Copy/publication and independent native
channel-owner tasks remain unchanged. The contended-copy regression now covers
both pointerless and reference-bearing objects under Compact/Full headers.

Three alternating depth-18 tree process pairs against the preflight-optimized
runtime use identical compiler, wrapper and heap settings. All outputs and
allocation/collection counts match. Median paired total-time ratio is 0.9325
(pairs 0.9325/0.9623/0.9238), about 6.7% lower; measured-iteration ratio is 0.9339.
Across-process median total time is 7067.9/6596.0 ms, including five warm-up and
ten measured iterations. RSS is similar; no general memory reduction is claimed.

Short reference-bearing controls suggested an eight-worker payload regression.
A longer nine-pair, 33-collection confirmation has median ratio 0.9827, with
individual ratios 0.9265–1.1317. This does not establish a stable payload slowdown
or universal speedup/neutrality. Exact live-byte checks pass throughout.
Rejected scan-inline and minor-edge-reload candidates remain reverted.

Raw paired samples, binaries, source identities, controls and validation logs:
`target/gc-leaf-tasks/`. The performance pad records final validation and limits.

## Remembered-set promotion preflight, 2026-10-20

Near-capacity promotion preflight now enumerates remembered old objects rather
than walking the entire old generation. It preserves native old-owner channel
edges and leaves dirty cards intact for evacuation. A sparse-card regression
checks bounded old-object scanning, unreachable old owners, cyclic young
survivors, exact capacity and card preservation.

Three alternating native baseline/candidate process pairs on macOS use the same
compiler, depth-18 tree wrapper, one GC worker, 16 MiB nursery and 256 MiB tenured
semispace. Each process runs five warm-up and ten measured iterations; complete
ordered checksums match. All processes allocate 1,024,983,105 objects and perform
1,463 minor plus eight major collections. The median paired total workload-time
ratio is 0.4965; median paired measured-iteration ratio is 0.7931. Across-process
median total time falls from 13,254.6 to 6,550.2 ms; the largest observed GC pause
falls from 157.27 to 79.50 ms. Total time includes warm-up and captures expensive
collection-bearing iterations that iteration medians obscure.

Peak RSS remains around 593 MiB. These observations establish an improvement
for this promotion-heavy workload, not a general throughput or latency budget.
Raw samples, outputs, metrics, binary/source identities and validation logs:
`target/gc-quick-improvement/`. The project performance pad records the review
and final validation. Two raw-pointer fixtures now mark their old-to-young
initialization writes, matching the existing minor-collection barrier contract.

## Thread census retained backing, 2026-10-19

A standalone program linked against the normal runtime build observed requested
registry backing of 1,024 bytes with 128 registrations, 48 bytes after retiring
to three registrations and collecting, and zero after final retirement. All
128 external state observers remained alive during these measurements. The
heap had no other registry allocations, so this isolates census storage;
it does not measure ThreadState controls or process RSS.

Allocator-level accounting includes the staged replacement until old backing is
physically freed. Sparse maintenance moves Arc entries without moving states or
changing diagnostic identities. No per-object accounting was added. CPU and
pause-time speedup are not inferred from this retained-storage observation.
Evidence: `target/gc-validation/2026-10-19-provenance/thread-census-accounting-validation/census-observation.json`
and provenance hashes in `verification.json`.

## Wide scalar record compilation, 2026-10-19

Restricting last-use root liveness to direct reference locals avoids copying
scalar-only live sets. Root clearing also visits dead references directly.
Two order-reversed diagnostic pairs on the macOS debug compiler measured
512-field unoptimized LLVM emission at 0.542/0.053 seconds and 1,024 fields at
2.220/0.110 seconds (baseline/isolated liveness candidate). The candidate also
completed an 8,190-field case in approximately four seconds, where the original
regression exceeded its 30-second limit on macOS and Linux. These microcases
do not establish general compile-time budgets or Linux speedup ratios.

The final applied change adds direct dead-root emission and is verified by
bounded wide-record compilation, stress-root lifetime checks and focused
Linux/macOS debug/release gates. Timings: `target/gc-validation/2026-10-19-provenance/wide-struct-investigation/paired-diagnostic.json`;
final source and correctness evidence: sibling `root-liveness-validation/verification.json`.

## Sustained queued-cycle abandonment — 2026-10-19

Two native Linux phases of 300 seconds passed 361,004 waves: 182,924 semispace and
178,080 generational. Each wave leaves 512 messages queued, drops every channel
owner root without close/receive calls, collects the unreachable cycles and
checks the fixed 163,824-byte managed live set. Physical native buffer retirement
is awaited outside managed entry with a bounded deadline. All 184,834,048 abandoned
channel lifetimes returned native queue-buffer bytes to zero. Each wave also
checks the persistent tree and allocates/checks/frees a 64 KiB FFI buffer.

RSS high-water was 3868/3920 KiB. Collection-phase P99 durations were
631.879/656.513 microseconds; maxima were 4.360/3.555 milliseconds. These timers sum
the live-queue and dead-owner collection phases and exclude the subsequent native
retirement wait, so they are neither end-to-end wave times nor isolated pauses.
The full datasets, counters and source/harness identities verified. Evidence:
`target/gc-validation/2026-10-19-provenance/channel-abandonment-soak-linux/sustained300/`.
This covers queued-cycle operating behavior for these heaps and host conditions;
other resource lifecycles, longer service lifetimes, application budgets and the
remaining operation/embedding audits remain open.

## Sustained channel lifecycle observations — 2026-10-19

Each native Linux mode ran for 300 seconds with four GC workers and 512 channels
per wave. Messages referring to their owners survived live collection, then were
validated/drained before dead-owner collection. All waves returned to the fixed
163,824-byte managed live set and zero native queue-buffer bytes.

| Mode | Waves | RSS high-water KiB | Collection phases P50 us | P99 us | Maximum ms |
| --- | --- | --- | --- | --- | --- |
| Semispace | 360,936 | 3940 | 212.189 | 448.054 | 3.701 |
| Generational | 333,705 | 4000 | 301.717 | 582.867 | 3.757 |

Each duration sums the live-queue and dead-owner collection phases, including
coordination/native maintenance; it is not a single GC call, pause or request
latency. Generational phases explicitly request minor then major collection.
RSS window medians increased in steps, ending at 3940/3976 KiB; no long-term
plateau is claimed. Registry maxima were 84 bytes and final teardown counters
32 bytes, excluding Arc/control and allocator overhead. These are workload
observations without a baseline speed comparison or application budget claim.

All 694,641 raw records and both completion records verified, with exact
nearest-rank quantiles and matching harness/source identities. Evidence:
`target/gc-validation/2026-10-19-provenance/channel-operation-soak-linux/sustained300/`.

## Continuous single-runtime native churn — 2026-10-19

Each native Linux phase ran for 300 seconds with one persistent runtime, a checked
4095-node live tree, 512 bound native atomic cells and a checked/freed 64 KiB FFI
buffer per wave. Four GC workers were requested. All payload/edge/live-byte and
release checks passed. Samples stream out of the child; its memory does not retain
all timing records. /proc RSS observations are separate from tracked allocations.

| Mode | Checked waves | RSS high-water KiB | GC call P50 us | GC call P99 us | Maximum ms |
| --- | --- | --- | --- | --- | --- |
| Semispace | 2,179,281 | 3708 | 59.181 | 127.750 | 3.734 |
| Generational | 1,949,474 | 4208 | 69.891 | 128.601 | 3.791 |

These are exact nearest-rank quantiles over retained call durations, including
coalesced calls and native maintenance. Construction, graph checking and logging
are outside that timer. They are not isolated GC pauses or application-request
latencies. Both RSS medians plateau from the 90-second window onward. Registry
maxima remain 32/167,968 bytes in every 30-second window; both are 32 bytes after
context teardown while the heap/root remains held. Registry counters exclude
Arc/control allocations and allocator overhead. No general latency/RSS budget,
complete accounting claim or production certification follows from this workload.

All 476 runtime source bytes/modes, binary and harness hashes were checked; raw
compressed logs were parsed and verified end-to-end after retrieval. Evidence:
`target/gc-validation/2026-10-19-provenance/operation-soak-linux/sustained300/`
(`verification.json`, `harness-sources.tar.gz`, per-mode raw/RSS data and reports).
The project performance pad shows RSS window medians and the scoped result table.


## Bounded local work sharing — 2026-10-19

A fresh macOS profile showed queue publication/handoff mutex waits. Retaining
one additional bounded batch per active worker reduces shared publications,
while full batch granularity avoids fragmenting narrow graphs. An earlier eager
variant slowed the cyclic workload 2.74× at eight workers and was rejected; its
source and measurements remain retained. Explicit worker counts remain supported.

Four alternating baseline/candidate pairs per worker/workload compare the applied
batched revision with retained release binaries. The standalone tree/cycle
harness checks every payload/edge and exact live bytes after each collection.
The large-payload fixture checks exact live bytes. All 72 macOS processes pass.

| Workload | 1 worker | 4 workers | 8 workers |
| --- | --- | --- | --- |
| Tree, median paired candidate/baseline | 0.993 | 0.757 | 0.609 |
| Cyclic graph | 1.015 | 1.003 | 1.004 |
| Sustained large payload, 8 pairs / 33 collections per process | 0.994 | 1.010 | 1.002 |

The sustained payload comparison adds 48 passing processes after the short
nine-cycle result showed a possible slowdown. Ratios summarize matched process
pairs, not a quotient of across-process medians. These workloads establish no
application latency/RSS budget or universal speedup. Active scratch adds one
128-entry allocation on first preparation; warm reuse remains allocation-free.

Native Linux broader validation passes all 176 processes, including same-binary
controls, unrestricted placement and a fixed-L3 diagnostic cohort. Results are
mixed: the unrestricted four-worker tree ratio is 1.244 in that short cohort,
while a subsequent 16-pair placement-sampled comparison gives 0.963. Identical
binary comparisons themselves range 0.566–1.382 in the latter run. Thread CPU
observations perturb execution and show placement variation; they do not prove
causation. Fixed-L3 comparisons are diagnostic, not a production affinity policy.
No Linux throughput-change or regression-free claim is established.

Evidence under `target/gc-validation/2026-10-19-provenance/`:
`work-sharing-batched-broad-macos/`, `work-sharing-batched-payload-sustained/`,
`work-sharing-batched-linux-broad/`, `work-sharing-placement/`, exact source
archives and binary/build hashes. The applied source matches all 476 candidate
input bytes/modes. Full applied release validation passed, including both
workspaces/platform sanitizer suites, native Linux compiler/JIT, big-endian,
Miri and all 72 seeded soak cases. Source/archive identity is retained in
`work-sharing-full-verification.json`. This does not close the Linux throughput
uncertainty or application latency/resource budgets.


## Exported allocation storage — 2026-10-19

The allocator cursor and heap allocation window now use sole-owned raw allocation
storage so exported aliases survive owner moves. Eight alternating baseline/candidate
process pairs at each of 1/4/8 mutators compare retained release binaries with
identical allocation harness sources. Every process checks exact object counts;
each path has five samples per process. All 48 processes pass. Full correctness
gates completed before measurement; binary hashes and both source archives are
retained. One-minute host load ranged from 3.41 to 7.34 on this workstation.

| Mutators | Allocation | Median paired candidate/baseline | Baseline median ms | Candidate median ms |
| --- | --- | --- | --- | --- |
| 1 | Shared cursor | 0.971 | 3.796 | 3.650 |
| 1 | TLAB | 0.992 | 3.367 | 3.342 |
| 4 | Shared cursor | 0.951 | 102.619 | 99.026 |
| 4 | TLAB | 0.978 | 3.870 | 3.788 |
| 8 | Shared cursor | 0.999 | 568.731 | 564.397 |
| 8 | TLAB | 0.980 | 4.182 | 4.095 |

Paired ratios are calculated within matching process pairs and then summarized;
they are not the quotient of the two across-process medians. The median ratios
show no slowdown in this comparison. Shared-cursor contention has wider variation;
these measurements do not establish a universal speedup or application latency/RSS
budget. The baseline retains the repaired owner-move defect and serves only as
historical performance comparison; object counts validate each measured execution.

Evidence: `target/gc-validation/2026-10-19-provenance/abi-storage-benchmark/`
(`prepared.json`, `measurements.json`, raw logs, binaries and source archives).
The subsequent deterministic stripe-fixture repair changes test construction only;
production allocator/runtime bytes in the measured candidate match the applied fix.


## Reusable mutator SATB publication — 2026-10-18

Active-cycle SATB flushes now synchronously publish borrowed buffer contents,
then clear the local vector while retaining its backing allocation. Previously
every full flush allocated a replacement before the deduplication queue could
discard repeated values. Unique values still enter bounded queue chunks, decoded
in fixed stack batches outside the queue mutex; no local buffer reference escapes.
Owned public chunks and standalone publication still transfer their allocations.
Both pre-write logging and safepoint flushes use the reusable path.

The runtime-path regression warms and drains one unique publication, then checks
64 full duplicate-only flushes for zero native allocation/deallocation traffic,
empty buffers and one unique value. Restoring replacement-buffer publication
fails that check. A second regression checks 513 distinct tagged references,
null/out-of-source rejection, repeated cycle reset, stable local storage and
standalone allocation transfer. All 13 focused optimized SATB tests pass after
source restoration. The negative control records 64 allocations/deallocations
and 131,072 allocated bytes across 64 duplicate-only flushes, versus zero traffic
on the reusable path. No application throughput/latency speedup is claimed.

Full local validation passed: **1044 debug and 1043 release workspace passes**,
15 tooling tests, one armed detector, 50 generated native plus 413 runtime
ThreadSanitizer executions, and 728 big-endian executions; zero failures. Totals
overlap; ten/eleven workspace ignores remain. Both new tests ran in debug/release,
TSAN and all four big-endian profile/worker configurations. All 467 source/archive
entries and modes and the restored fault-control source hash were verified;
diff check passed. See `verification.json` and `gates/report.json` in the evidence
directory below.

Operation/embedding audits, remaining native accounting, concurrent publication
coverage, hosted CI/platform/soak evidence and numeric budgets remain open.
Overall production readiness is unfinished.

Evidence: `target/gc-validation/2026-10-18-satb-buffer-reuse/`.

## Published canonical access — 2026-10-18

Published old aliases now use an acquire forwarding load and lock only the
canonical destination stripe. Previously they acquired the source stripe and
then reacquired the ordered source/destination pair. A publication racing the
initial check also needs only the destination after releasing the source.
Forwarding is immutable until a stopped-world swap, which cannot overlap the
caller's non-safepointing access. The copier retains its ordered pair through
publication, so destination locking still excludes copy completion and guarded
payload access. Marker-only claims retain the existing retry path.

A bounded Compact/Full/offset-header regression holds a distinct source stripe,
resolves and writes canonical storage, verifies the destination lock is held,
checks old payload remains unchanged and detects native allocation traffic.
Restoring source locking times out; removing the destination guard fails the
protection assertion. Both controls fail as intended, source restoration and all
six focused reference tests pass. Full local gates passed: **1042 debug and
1041 release workspace passes**, 15 tooling tests, one armed detector, 50 generated
native plus 411 runtime ThreadSanitizer executions, and 720 big-endian executions;
zero failures. Totals overlap; ten/eleven workspace ignores remain. The new test
ran in debug/release, TSAN and all four big-endian profile/worker configurations.
All 467 source/archive entries and modes and the restored fault-control source
hash were verified; diff check passed. This reduces lock acquisitions; no
throughput or latency improvement is claimed without a matched measurement.

Operation/embedding audits, remaining native accounting, concurrent publication
coverage, hosted CI/platform/soak evidence and numeric budgets remain open.
Overall production readiness is unfinished.

Evidence: `target/gc-validation/2026-10-18-canonical-access/`.

## Published forwarding fast path — 2026-10-18

An acquire load now returns an already-published forwarding destination without
acquiring the source stripe. Publication remains a release store after destination
initialization. Marker-only claims still enter the locked retry path; forwarding
remains immutable until the stopped-world swap.

New bounded regressions cover source-lock bypass for Compact, Full and offset
headers, allocation-free destination reuse, and a paused publication race. Removing
the fast path causes the held-stripe test to time out. Treating a marker-only claim
as a published destination fails the publication test. Both controls failed as
expected; source was restored and all five focused reference tests passed.

Eight alternating process pairs per workload compare saved release binaries with
identical benchmark harnesses. Across 64 processes, every graph validates payloads,
aliases, destination membership and live bytes. Median paired candidate/baseline
cycle ratios are **0.377/0.373 for 65,536/262,144 shared references**, approximately
62–63% lower total cycle time on top of the batched scanner. Distinct-child ratios
are **0.999/1.000**; RSS ratios range from 1.0004 to 1.0023. These are warmed
collector-only cycles without competing mutators. Application throughput, tail
latency, startup, pause budgets and general performance acceptance remain open.

Full local validation passed: **1037 debug and 1036 release workspace passes**,
15 tooling tests, one armed detector, 50 generated native plus 406 runtime
ThreadSanitizer executions, and 668 big-endian executions; zero failures.
Suite/filter totals overlap; ten/eleven workspace ignores remain. Both new tests
ran in debug/release, ThreadSanitizer and all four big-endian profile/worker
combinations. All 467 source/archive entries and modes, benchmark binaries,
harness/script and raw timing logs were verified; diff check passes.
Evidence: `target/gc-validation/2026-10-18-forwarded-fast-path/`
(`verification.json`, `benchmark.json`, `fault-controls.json`, `gates/report.json`).

Operation/embedding audits, remaining native accounting, forced transition
coverage, hosted CI/platform/soak evidence and numeric performance budgets remain
open. Overall production readiness is unfinished.

## Bounded concurrent reference snapshots — 2026-10-18

Concurrent discovery now loads up to 32 reference slots under one canonical
owner-lock acquisition, releases the guard, then decodes/copies children. The
bounded snapshots cover fixed fields, interior references and variable tails.
Object geometry remains immutable, parent rewrites remain deferred to final STW,
and SATB/final rescanning preserve edges changed between batches. Scratch is
bounded stack storage; no guard crosses a child copy or pointer-policy callback.

New regressions cover batch boundaries across Compact/Full headers and
identity/tagged policies, raw-neighbor preservation, nulls and shared children.
A direct pre-final-scan test forces owner/child stripe collision, checks zero
native scratch allocations, and gives the final partial batch its only child.
Omitting that tail fails; retaining the owner guard across copying deadlocks and
fails the bounded test. Source restoration and both regressions pass.

Eight alternating process pairs per workload compare matched-harness saved
baseline/candidate release binaries. Every collection validates all payloads and
live-byte counts; two warmups precede nine timed cycles. All 64 runs pass (576
measured plus 128 warmup cycles). Median paired candidate/baseline cycle ratios:
**0.639/0.631 for 65,536/262,144 shared references**, and **0.924/0.918 for distinct
children**. RSS ratios are 1.000/1.000 and 0.999/0.999. The observed reductions are
approximately 36–37% and 8%. These are warmed collector-only total-cycle timings
without competing mutators; no application throughput, mutator/tail latency,
startup, pause reduction or numerical-budget claim follows. Host observations and
raw samples are retained. A post-measurement cfg(test)-only tail-oracle change is
identified separately; measured production source/binaries are unchanged.

Full local validation passed: **1035 debug and 1034 release workspace passes**,
one separately armed detector, 15 tooling tests, 50 generated native plus 404
runtime sanitizer executions, and 660 big-endian executions; zero failures.
Filter/suite totals overlap; ten/eleven workspace ignores remain. Both new tests
appear in debug/release, ThreadSanitizer, and all four big-endian profile/worker
combinations. All 467 current source/archive entries and modes, both benchmark
archives/binaries, harness/script and raw timing logs were verified. Diff check
passes. Evidence: `target/gc-validation/2026-10-18-concurrent-scan/` (`verification.json`,
`benchmark.json`, `benchmark-scope.json`, `fault-controls.json`, `gates/report.json`).
`baseline-matched/` is the comparison baseline; `baseline/` is initial discovery
material and is not used in the paired result.

Remaining operation/embedding audits, total native accounting, forced transition
coverage, hosted CI/platform/soak evidence and numerical performance budgets are
still open. Overall production readiness remains unfinished.


## Rooted aggregate-source reuse and contended relocation — 2026-10-18

Aggregate try-writes now use existing ANF value-local storage directly. Its native
address is stable; reference interiors are already indirect caller-frame roots.
The nonparking runtime operation only reads it and does not retain the pointer.
Constants still receive staging. The contended fallback loads the RHS after
acquisition, preserving collector-updated references. An unoptimized emission
regression fails on the old redundant staging alloca/copy; normal/stress JIT
behavior also passes.

A bounded native regression forces a failed try and a blocked acquisition,
installs a real indirect shadow frame, performs moving collection, then checks
both the relocated destination and two updated RHS references. UnsafeCell models
collector-written value storage; a guard unlinks the frame before its stack
storage retires. Only the owner's address crosses threads for atomic blocked-
state observation, with lifetime bounded by its join. The fixture does not
execute generated fallback code under this rendezvous; emission/integration
checks cover generated code separately. Caller-error controls remove indirect
roots or reuse stale RHS bits. Production controls return a stale base or
reintroduce staging. All four fail, with source restored after each.

Final verification: **1031 workspace passes, zero failures, ten ignored**;
515 optimized runtime units plus nine doctests, armed detector, 49 optimized
compiler integrations, 648 big-endian executions, 50 generated native and 401
runtime ThreadSanitizer executions. Filters/extra checks overlap. All 166 source
pins and archive entries match, as do benchmark compiler/executable/runtime/
fixture/codegen hashes. Diff check passes. Evidence:
`target/gc-validation/2026-10-18-aggregate-source/report.json`.

All 128 benchmark outputs are correct. Racing aggregate/scalar and million-write
reference-bearing workloads have median paired wall ratios 0.980 and 0.993;
RSS ratios 1.005 and 1.000. Both compilers link identical runtime. No collections
occurred in these timings; racing retries/allocations and host activity differ.
The emission check proves removal of redundant staging, while these timings do
not establish universal speedup, active-copy throughput, pause reduction or
numerical budgets. No new whole-runtime Miri claim is made.

Full generated/runtime concurrent integration, embedding/root/shutdown audit,
total native accounting, wider platforms/soaks and numerical budgets remain open.
Production readiness remains unfinished.

## Concurrent aggregate replacement — 2026-10-18

Generated inline aggregate stores now try a non-safepointing transaction before
the rooted legacy fallback. The runtime takes the aggregate stripe and canonical
ObjectAccess, logs displaced references, copies untraced gaps and atomically
replaces traced words, marks nursery cards at canonical slots, then linearizes
before releasing canonical exclusion. Sorted constant per-value reference tables
avoid scanning unrelated owner fields. A contended attempt changes neither
payload nor SC order; the fallback reloads the ANF RHS after possible relocation.
Reads also retain canonical exclusion through their SC event so old/new aliases
remain ordered even when their aggregate stripes differ. Legacy ABI remains.

A real concurrent cycle pauses discovery before an overwritten child is found.
The write must finish through an old parent alias before copying resumes. Tests
check canonical snapshots, displaced-child SATB retention, destination card state
before final rescan, later nursery promotion and untouched neighboring fields.
A separate failed-try reference checks payload/order/allocation effects. Four
fault controls fail: absent SATB, absent card marking, old-alias storage and a
disabled active-copy path. Initial hook/coverage mistakes remain archived: a
shared child triggered a repeated rendezvous, and final-major card rebuilding
masked the first missing-card control. A one-shot hook and pre-rescan assertion
repair the test without removing the shared edge or later promotion check.

Final verification: **1029 workspace passes, zero failures, ten ignored**;
514 optimized runtime units plus nine doctests and separately armed detector,
48 optimized compiler integrations, 644 big-endian executions, 50 generated
native and 400 runtime ThreadSanitizer executions. Filters/extra checks overlap.
All 166 source pins/archive entries and benchmark compiler/executable/runtime/
fixture/codegen hashes match. Diff check passes. Evidence:
`target/gc-validation/2026-10-18-aggregate-write/report.json`.

All 128 benchmark outputs are correct. Racing aggregate/scalar and million-write
idle workloads have median paired wall ratios 1.008 and 0.974; RSS ratios 1.010
and 0.993. Both compilers link identical runtime; the baseline is the preceding
aggregate-read compiler. No collection occurred in these timings. Racing retry
counts/allocations and recorded host activity differ. These observations do not
establish universal speedup, active-copy throughput, pause reduction or numeric
budgets; active-copy correctness/progress comes from deterministic tests.

Full generated/runtime concurrent integration, embedding/root/shutdown audit,
total native accounting, wider platforms/soaks and numerical budgets remain open.
No new whole-runtime Miri claim is made. Production readiness is unfinished.

## Concurrent aggregate reads and target layout — 2026-10-18

Generated inline aggregate field reads now call `ai_managed_read`. The runtime
acquires the aggregate stripe before canonical ObjectAccess, retains both through
the native snapshot copy, then releases both before returning. Uncontended reads
can complete while copying continues. Contention retains rooted blocking and
relocation revalidation. Aggregate stores and the legacy lock/unlock ABI remain.
No borrowed access guard escapes the runtime operation.

The native-buffer integration exposed a target-layout mismatch: modules had no
host data layout before optimization. LLVM folded a value-enum raw-field address
to offset 12, while native emission used offset 16. Modules now install the host
triple and data layout before generation/optimization. The mixed i32/i64/reference
regression returns wrong fields (81) if that setup is removed. Before/after IR
and the initial integration failures are retained.

An actual-cycle regression pauses child discovery, mutates canonical parent
scalars, and requires both old/new-alias snapshots to complete before copying
resumes, preserving the reference and canonical scalar values. Three deliberate
faults fail: whole-cycle waiting, bypassed canonical access, omitted target
layout. The big-endian gate runs this test directly under QEMU with external
bounds rather than reexecuting the foreign binary.

Final verification: **1027 debug workspace passes; 1026 optimized workspace
passes plus the separately armed detector; zero failures**. Debug/release ignores
are ten/eleven. Optimized runtime: 512 units plus nine doctests. Also passed
42 focused debug integrations, 636 big-endian test executions, 50 generated native
and 398 runtime ThreadSanitizer executions (filters overlap). All 166 source pins
and archive entries match, along with both benchmark compilers/executables,
fixtures, codegen source and runtime library. Diff check passes. Evidence:
`target/gc-validation/2026-10-18-aggregate-read/report.json`.

All 128 benchmark outputs are correct. Racing aggregate/scalar and million-read
idle workloads have median paired wall ratios 0.997 and 0.942; RSS ratios 1.005
and 1.007. No collections occurred in those measurements. Racing CAS retries
and allocation counts differ, and host activity is recorded. These observations
do not isolate one change's causal cost or establish active-copy throughput,
pause reduction or numeric budgets. Active-copy progress comes from the separate
paused-copy regression. No new whole-runtime Miri claim is made.

Full generated/runtime concurrent integration, aggregate-store progress during
copying, total native accounting, embedding, broader platforms/soaks and numerical
budgets remain open. Overall production readiness remains unfinished.

## Prepare concurrent access locks before copying — 2026-10-18

First use of the 256 concurrent object-access stripes allocated 256 native mutex
backings on this host, even after an empty concurrent cycle. Collection now
prepares all stripes once per heap before publishing Copying. Preparation stays
inside cycle timing; ordinary STW heaps leave unused locks cold. No preparation
check is added to the mutator access path.

The regression starts with cold locks, runs an actual empty concurrent cycle,
then checks repeated preparation and every stripe for zero allocator traffic.
It fails on old source with 256 allocations. Missing/partial preparation faults
also fail. An isolated probe using the exact production lock implementation
measures 256 allocations / 16,384 requested bytes for first preparation and zero
allocation/free traffic for later preparation plus all locks. This front-loads
cost rather than eliminating it: small first cycles may prepare unused stripes.
These are host Rust allocator observations, not RSS or total native accounting.

Final verification: **1025 workspace passes, zero failures, ten ignored**;
511 optimized runtime units plus nine doctests, armed detector, five optimized
enum/managed-memory integrations, 632 big-endian executions, 50 generated native
and 397 runtime ThreadSanitizer executions. Filters overlap. All 166 source pins
and archive entries match; probe-source and release compiler/runtime hashes
match. Diff check passes. Evidence:
`target/gc-validation/2026-10-18-copy-locks/report.json`.

No fresh application performance, active-copy throughput, pause reduction or
whole-runtime Miri claim is made. Native mutex backing remains outside registry-
buffer metrics. Total native accounting, generated/runtime concurrent integration,
embedding, broader platforms/soaks and numerical budgets remain open.

## Long-execution native pressure adaptation — 2026-10-18

A released native-memory spike previously left an oversized collection trigger
until execution shutdown. The same-entry regression fails on the old code:
after freeing an 8 MiB FFI spike, 1.25 MiB of channel growth schedules no collection.
Allocation checks now lower obsolete headroom to the larger of 1 MiB and twice
the sum of current accounted demand and the pending request. They never raise
unserviced budgets; pending large replacements preserve serviced headroom.
Concurrent checks may conservatively remove headroom. Metrics remain unchanged.

Thirteen focused pressure tests pass, covering both heap modes, buffer/registry
ledgers, pending requests, retained observers and concurrent coalescing. Three
deliberate faults fail. Final verification: **1024 workspace passes, zero
failures, ten ignored**; 510 optimized runtime units plus nine doctests, armed
detector, 12 optimized integrations, 628 big-endian test executions, 50 generated
native and 396 runtime ThreadSanitizer executions. Platform/race filters overlap.
All 119 runtime/gate and 161 application source pins match, along with compiler,
runtime, saved binaries, baseline report and fixture hashes. Diff check passes.

All 480 original and 64 follow-up application runs are correct. Original cohort
median paired wall ratios span 0.916–1.065 and peak-RSS ratios 0.993–1.016. The
slowest original case (routes, one worker, 1 MiB nursery) has a later 31-pair wall
ratio of 0.993 and RSS ratio 1.000. Both series are retained; recorded host
activity prevents a causal speedup or zero-cost claim. No numerical performance
budgets or new whole-runtime Miri guarantee are established.
Evidence: `target/gc-validation/2026-10-18-live-pressure/report.json`.

Record/control and other metadata accounting, remaining embedding/concurrent
integration, broader platform/soak evidence and numerical budgets remain open.
Overall production readiness remains unfinished.

## Global and handoff backing accounting — 2026-10-18

The handoff allocation probe matches its two production backing allocations to
ledger bytes exactly. Instrumentation's cold mutex is initialized before the
measured region. Root backing remains charged across execution shutdown and
releases with the heap; independently staged buffers do not retain the heap.

All 480 original application runs pass. Paired cohort wall ratios span
0.904–1.089; peak-RSS ratios span 0.980–1.009. Collection/allocation medians did
not increase; one parallel cohort used one fewer minor collection. Host activity
is retained. The slowest original cohort was rerun alone on the same binaries
and fixtures after the heavy gates. All 64 follow-up runs pass, with 31 measured
alternating pairs after warmup. The median of paired wall ratios is 1.0215;
per-variant median times are 27.68/27.31 ms (current/baseline). The peak-RSS
paired ratio is 0.9994. Both the initial
8.9% and follow-up 2.1% costs remain reported. These observations do not establish
zero cost, universal speedups or numerical budgets. Evidence:
`target/gc-validation/2026-10-18-root-storage/apps/report.json`,
`app-comparisons.json`, `performance-review.json` and `followup/report.json`.

## Permanent root publication observations — 2026-10-18

Growth and obsolete backing disposal now run BLOCKED outside collector/registry
locks. Deterministic pauses complete another collection while the new source is
visible exactly once. Commit and spare-capacity publication allocate/free nothing.
Permanent-source vector backing is charged through physical free at heap release.
This verifies collector availability and accounting, not universal lower latency.

All 480 application runs are correct against the pressure-epoch checkpoint.
Seven measured pairs follow warmup for 30 cohorts. Paired median wall ratios:
auto 0.958–1.041; one worker 0.971–1.040; four workers 0.971–1.044. Peak-RSS
ratios span 0.992–1.012. Host activity is recorded; these observations do not
establish numerical budgets or universal speedups. Evidence:
`target/gc-validation/2026-10-18-permanent-roots/apps/report.json`
and `app-comparisons.json`.

## Pressure policy across executions — 2026-10-18

A reused runtime no longer retains a trigger sized for a released 8 MiB native
FFI footprint. A real re-entry test now services a later roughly 1 MiB channel
workload. The boundary adjustment runs after retirement drain, only lowers
headroom, preserves retained demand and does not alter collection counts/peaks.
The minimum-threshold path avoids the service mutex. General within-execution
shrink coverage and complete native accounting remain unfinished.

All 480 correctness-checked application runs pass against the teardown checkpoint.
Seven measured pairs follow warmup for 30 cohorts. Paired median wall ratios:
auto 0.997–1.031; one worker 0.972–1.042; four workers 0.919–1.012. Peak-RSS
ratios span 0.983–1.018. Background activity is recorded; these observations do
not establish numerical budgets or universal improvements. Evidence:
`target/gc-validation/2026-10-18-pressure-epochs/apps/report.json`
and `app-comparisons.json`.

## Registry teardown observations — 2026-10-18

Shutdown waits through every collector forwarding write, then detaches registry
ownership with swaps that neither allocate nor free. Record retirement, backing
disposal and native drain run outside registry/coordinator locks with callers
BLOCKED. One deterministic test prevents detachment while exclusion is held;
another pauses disposal and completes a separate collection with registries empty
and unlocked. This verifies collector coordination, not faster total shutdown.

All 480 corrected-implementation application runs pass against the scoped-root
checkpoint. Seven measured pairs follow warmup for 30 cohorts. Paired median wall
ratios: auto 0.964–1.039; one worker 0.933–1.033; four workers 0.984–1.033.
Peak-RSS ratios span 0.977–1.014. Background activity is recorded; these observations
do not establish numerical budgets or universal improvements. The superseded
draft is archived separately with relocation notes under
`target/gc-validation/2026-10-18-registry-teardown-initial/`.
Final evidence: `target/gc-validation/2026-10-18-registry-teardown/apps/report.json`
and `app-comparisons.json`.

## Scoped-root registration observations — 2026-10-18

Pinned stack descriptors and an intrusive registry eliminate metadata allocation
and disposal from scoped extra-root registration/removal. A Heap-path allocator
probe covers 1024 registration/scan/removal cycles with zero allocations/frees.
List mutation remains serialized with enumeration, and collector exclusion still
protects late parallel forwarding writes to borrowed slots.

All 480 correctness-checked application runs pass against the preceding
registry-pressure checkpoint. Seven measured pairs follow warmup in 30 cohorts.
Paired median wall ratios: auto 0.920–1.016; one worker 0.987–1.034; four workers
0.951–1.024. Paired peak-RSS ratios span 0.984–1.016. Background host activity is
recorded; these observations do not establish a universal improvement or budgets.
Evidence: `target/gc-validation/2026-10-18-scoped-roots/apps/report.json`
and `app-comparisons.json`.

## Registry pressure observations — 2026-10-18

Pressure decisions and post-GC thresholds now include actual registry backing
bytes alongside buffer bytes. Atomic construction and thread spawn service
accumulated demand; channel/FFI requests use the same combined ledger. Retained
backing affects threshold adaptation. The trigger is not a hard memory cap and
record/control allocations remain outside accounting.

All 480 correctness-checked paired application runs pass against the completed
registry-accounting checkpoint. Seven measured pairs follow warmup for 30
workload/worker/nursery cohorts. Median paired wall ratios: auto 0.987–1.021;
one worker 0.989–1.043; four workers 0.964–1.026. Paired peak-RSS ratios span
0.994–1.006. Background activity is recorded; these observations do not establish
numerical budgets or universal improvements. Evidence:
`target/gc-validation/2026-10-18-registry-pressure/apps/report.json`
and `app-comparisons.json`.

## Registry backing accounting observations — 2026-10-18

Actual allocator-requested table/vector backing bytes are now counted separately
from channel buffers. Accounting adds atomic counter updates and shared counter
ownership; it does not claim a throughput improvement. Record/control allocations
and native-pressure integration remain outside this checkpoint.

All 480 correctness-checked application runs pass. Seven measured pairs follow
warmup for 30 workload/worker/nursery cohorts. Median paired wall ratios span
0.985–1.021 with automatic workers, 0.962–1.055 with one worker and 0.956–1.030
with four workers. Paired peak-RSS ratios span 0.996–1.037. Background host activity
is recorded; these are observations rather than performance guarantees. The
initial benchmark command incorrectly supplied an rlib to the native linker and
failed before measurements. The completed series links the runtime staticlib.
Evidence: `target/gc-validation/2026-10-18-registry-accounting/apps-final/report.json`
and `app-comparisons.json`.

## Component-specific admission observations — 2026-10-18

Publication now prepares only exhausted backing components and leaves other
tables untouched. The shared ignored profile measures production publication in
both the saved baseline and candidate under `cfg(test)`, with identical profiling
source. Setup prepares native inputs; synthetic terminal completion records avoid
OS-thread creation. Constructors, fixture preparation and teardown are excluded.
The fixture has exclusive access and performs zero collections. Allocation counts
and bytes are from the publishing thread's Rust allocator probe.

Nine cases have seven alternating process pairs, with eight measured rounds plus
warmup per process: 126 process executions and 1008 measured publications.
Times below are medians of per-process medians, in microseconds:

| Exhausted component | Existing records | Baseline µs | Candidate µs |
|---|---:|---:|---:|
| Atomic owner index | 64 | 1.917 | 0.584 |
| Atomic owner index | 4096 | 99.521 | 1.250 |
| Atomic owner index | 32768 | 883.750 | 7.167 |
| Channel raw membership | 64 | 3.750 | 0.375 |
| Channel raw membership | 4096 | 190.396 | 0.354 |
| Channel raw membership | 32768 | 1690.230 | 0.688 |
| Completion pending list | 64 | 2.084 | 0.250 |
| Completion pending list | 4096 | 90.375 | 0.251 |
| Completion pending list | 32768 | 828.438 | 0.271 |

Atomic-owner growth uses one backing allocation instead of two. Channel-raw and
completion-pending growth use one instead of three. At 32768 existing records,
allocated bytes change from 4,849,688 to 524,304 for atomic owners, 5,636,120 to
44 for channel raw membership, and 3,276,840 to 16 for completion pending lists.
These are isolated growth costs, not whole-GC or whole-program speedups. The old
implementation rehashed/copied unrelated tables; the new path avoids that work.

The baseline adds only the same profiling module and a standalone workspace
manifest entry; its original Rust source hashes are verified. The initial fixture
attempt hit the parked-root check during untimed setup, produced no accepted
measurements and was corrected before comparison. Final executable inputs are
pinned explicitly. The profile is ignored in ordinary tests.

The correctness-checked application series has 480 runs including warmup and seven
measured pairs across 30 cohorts. Wall-ratio ranges: auto 0.991–1.038, worker one
0.983–1.044, worker four 0.984–1.045. Paired OS peak-RSS ratios span 0.979–1.003.
This checks wider workloads without promising a universal speedup. Numerical
budgets and wider platforms/soaks remain open. Evidence:
`target/gc-validation/2026-10-18-component-admission/profile.json` and
`apps/report.json`.

## Off-pause registry compaction — 2026-10-18

All three native registries now stage sparse backing outside collector exclusion
and registry locks, commit with allocation-free moves after revalidation, and free
old/unused backing after unlock. Mutator callers stay BLOCKED from GC through
maintenance. An initial extra resume/park transition was removed; intermediate
measurements and binaries remain archived. Full OS/TLS completion is also
published separately, removing state-mutex contention from terminal GC probes.

The final identical-source atomic-owner profile separates stopped-world telemetry,
complete GC-call latency and GC plus native teardown. Five cases use 32/4096/32768
dead owners, with zero or 64 raw survivors; seven alternating pairs each contain
eight measured rounds plus warmup. Median paired ratios (candidate/baseline):

| Dead | Raw | STW pause | GC call | Complete teardown |
|---:|---:|---:|---:|---:|
| 32 | 0 | 0.832 | 0.980 | 0.952 |
| 4096 | 0 | 1.013 | 1.018 | 1.028 |
| 32768 | 0 | 1.015 | 1.018 | 1.011 |
| 4096 | 64 | 0.964 | 0.991 | 0.997 |
| 32768 | 64 | 0.972 | 0.983 | 0.975 |

These are observations, not a universal speedup. The references establish that
blocked native allocation/disposal does not hold collector exclusion; this normal
allocator profile does not quantify worst-case allocator stalls or commit lock
contention. The atomic profile does not isolate completion-publication costs.
Heap remains 4496 inline bytes and completion records remain 128 bytes in both
comparison libraries; new flags use existing padding on this target.

Final application comparison: 480 correct alternating process executions,
including warmup; seven measured pairs across 30 cohorts. Worker-policy wall
ratio ranges are auto 0.959–1.025, one 0.888–1.019, four 0.981–1.010. Paired OS
peak-RSS ratios span 0.994–1.014. The baseline uses saved completion-checkpoint
binaries with the same compiler/application sources. Intermediate cohorts and
libraries are retained separately, rather than discarded. Numerical budgets,
wider platforms/soaks, component-specific admission and completion startup/cleanup
profiles remain required. Evidence:
`target/gc-validation/2026-10-18-registry-compaction/profile.json`,
`apps-joined/report.json` and their archived inputs.

## Completion metadata retirement costs and scope — 2026-10-18

Completion records now carry a prepared typed ownership link and its one-time
initialization state. The existing native retirement service gains a completion
mailbox; a completion-only execution now starts that service during admission,
even if it has not allocated channels or atomic cells. This adds native metadata
and can add an OS worker to such executions. Teardown drains completion jobs.

The regression establishes allocation/deallocation-free record transfer during
a non-compacting sweep and continued collection while native destruction stalls.
It does not establish a numerical pause reduction, throughput gain, startup cost,
or RSS budget. The earlier buffer/cell benchmarks do not measure this completion
change. A serial, alternating application comparison against the saved pre-change
binaries completed 480 correct runs (including warmup), with seven measured pairs
per cohort across 30 workload/worker/nursery combinations. Median paired wall
ratios range 0.954–1.085; median paired OS peak-RSS ratios range 0.984–1.025.
Worker-specific wall ranges: auto 0.954–1.021, one 0.970–1.085, four 0.971–1.044.
All 143 benchmark source files and compiler/runtime artifacts match their recorded
hashes. These are end-to-end observations, not a universal performance budget.
Completion-specific admission, bulk cleanup and retained-observer benchmarks
remain required to isolate this change's startup, metadata and destruction costs.
Full metadata accounting/admission and off-pause registry shrinking remain open.
Evidence: `target/gc-validation/2026-10-18-completion-retirement/`.

## Cross-epoch pressure accounting — 2026-10-18

Old-epoch published backing was previously ignored when a newer worker existed.
A regression fails before the fix. All epochs now share lazy pending activity;
pressure drains before deciding whether to collect and retunes only at an idle
publication snapshot. The actual heap regression avoids a redundant pressure
collection while another GC proceeds during the native wait.

Seven alternating same-source/settings pairs compare the previous verified
runtime. Final construction/close ratios are 1.049/0.981/0.955 at capacities
1/4,097/65,536. One-slot collection ratios at 32/4,096/32,768 controls are
1.205/0.957/1.033; complete cleanup is 1.216/0.990/1.109. Large backing is
0.999 collection / 1.042 cleanup; atomic-constructor control is 1.051. Earlier
observations remain in before-helper-cleanup. The largest complete-cleanup case
is about 11% higher in this run. Waiting correctly for old work is not comparable
to the old incorrect early return. No universal speedup or latency budget is proven.

Heap inline size grows 4,480→4,496 bytes on this host. Shared synchronization is
lazy and prepared before collection; prepared accounting has zero allocator
traffic. No epoch list or per-retirement allocation is added. Full metadata and
OS-thread accounting remain open. All 480 baseline, 960 initial paired and 960
final paired application runs are correct. Final wall ratios: auto 0.957–1.026,
worker 1 0.969–1.047, worker 4 0.945–1.051.

Evidence: `target/gc-validation/2026-10-18-retirement-epochs/`.

## Channel control retirement — 2026-10-18

Whole channel controls now transfer through typed prepublished ownership links.
The native worker destroys their synchronization/scan metadata and inline backing.
Large backing is released in a native batch covered by the pending controller
jobs, avoiding a second mailbox round. Foreign epochs retain ordinary publication.
Atomic cells share the same typed core and prepared worker-epoch source.

Seven alternating pairs use identical sources/settings. One-slot collection
ratios at 32/4,096/32,768 controls are **0.903/0.582/0.483**; complete-cleanup
ratios are **2.919/1.034/0.999**. At capacity 4,097 and 256 controls, collection is
**0.681**, cleanup **1.026**. Population is outside these timers; cleared global
owner slots remain root work. Cleanup includes finish_threads. Construction/close
loops exclude final context teardown and give ratios **1.079/1.043/1.077** at
capacities 1/4,097/65,536. The atomic-constructor control is 1.071 in the final
run, versus 0.998 in the earlier run. Ratios are medians of paired ratios.

Control payload grows 208→232 bytes on this host. First small-channel admission
now prepares the retirement service if it was not already running. Small-batch
teardown and constructor costs remain; no universal performance budget is proven.
The queued-backing prototype is retained: its bulk cleanup was 7–9% higher;
releasing same-worker backing directly brings final bulk cleanup close to parity.

All 480 baseline, 960 prototype paired, and 960 final paired application runs are
correct. Final wall-ratio ranges: auto 0.982–1.061, worker 1 0.952–1.043,
worker 4 0.973–1.017. Native metadata/OS-thread accounting and broader tail/RSS
budgets remain open. Evidence: `target/gc-validation/2026-10-18-channel-retirement/`.

## Atomic record retirement — 2026-10-18

Dead atomic records now hand off to the existing native retirement worker using
prepared intrusive links. Publication and batch merging allocate nothing. A
sweep reference excludes capacity shrinking to separately verify zero record
allocator traffic; registry shrinking remains collector work.

Seven alternating pairs at 32/4,096/32,768 cells use identical profile sources,
eight fresh-context rounds after warm-up, and source-matched optimized runtimes.
At 4,096/32,768 cells, median paired collection ratios are **0.839/0.775**;
constructor ratios are **1.193/1.144** and collection-plus-context-teardown ratios
**1.424/1.144**. At 32 cells the respective ratios are **1.352**, **4.492** and
**9.293**. Constructor timing includes first-cell worker preparation; collection
timing excludes preparation and validation. Total teardown waits for native
release, with arena destruction excluded equally. Paired-ratio medians need not
match quotients of independent medians. The record grows from 8 to 32 payload
bytes; observed Arc allocation grows from 24 to 48 requested bytes on this host.

All 480 baseline, 960 initial paired, 64 focused follow-up, and 960 final paired
application runs are correct. Final cohort wall ratios: auto 0.979–1.033,
worker 1 0.976–1.067, worker 4 0.862–1.050. The initial four-worker CSV / 1 MiB
nursery ratio was 1.155 and is retained; a 31-pair follow-up on the same binaries
and fixtures has median paired ratio 0.976. Host activity is uncontrolled and
recorded. Neither the cause of the outlier nor universal performance budgets
are established. Full native metadata/OS-thread accounting remains open.

Evidence: `target/gc-validation/2026-10-18-atomic-retirement/`.

## Allocation-free collection caller parking — 2026-10-18

Caller parking no longer constructs two temporary vectors while registrations
are RUNNING. An ownership chain in existing thread records covers any number of
same-OS-thread registrations. Allocator probes verify zero allocation/free
traffic during parking, nested parking and restoration at 0/1/17/257 records.
The original 17-registration case requests 448 bytes across six allocation or
reallocation events and frees one temporary buffer. Platform safepoint locks
are prepared independently before measuring restoration.

On this 64-bit host, ThreadState grows from 528 to 536 bytes. This trades one
link per registration for eliminating per-parking backing allocations; it is
not evidence of a universal wall-time speedup or a numerical latency budget.
Evidence: `target/gc-validation/2026-10-18-caller-parking/`.

## Cooperative handoff storage — 2026-10-18

Native handoff growth now prepares root/pool backing while BLOCKED, publishes
using reserved capacity, and reserves future return for every allocated slot.
The fast reuse path remains allocation-free. This removes allocator work from
RUNNING admission and child-side return; no universal speedup is claimed.

The actual spawn/bind profile compares the same example and optimization flags
against the previous verified handoff-retirement runtime. Seven alternating
pairs, eight measured fresh-context rounds after warmup, give paired ratios
**0.980/1.034** at 32/256 threads. Median loop times are **0.3521→0.3785 ms /
3.2456→3.2582 ms**. Paired ratios are medians of per-trial ratios, so they need
not equal ratios of the two independent medians. Context setup/shutdown, joins
and value checks are outside timing; child startup/return can overlap admission.
Native mutex preparation is paid during context setup, which the application
measurements below include.

A restored isolated project snapshot matches all 89 runtime Rust source hashes
from the previous checkpoint. Its completed 480-run application report supplies
the baseline binaries. The final identical-compiler comparison passes all 960
processes. Across workload/nursery cohorts, paired median wall ratios range
auto **0.958–1.070**, one worker **0.955–1.031**, four workers **0.972–1.069**.
These observations do not establish throughput, pause, tail or RSS budgets.

Evidence: `target/gc-validation/2026-10-18-handoff-storage/`. The baseline project,
source archives, baseline provenance, compiler/runtime hashes and raw paired
samples are retained alongside the final checkpoint report. Full native
metadata accounting/admission and off-pause cleanup remain open.

## Prepared completion catalogue — 2026-10-18

Prepared FIFO nodes remove B-tree growth/removal allocation from the monitor
lock. An initial node-mutex prototype doubled isolated scan time and requested
128 bytes in two node allocations. The final links use the existing monitor's
exclusion protocol: one 48-byte node allocation, plus 24 bytes of inline record
growth (72→96 bytes). These are allocator requests/type sizes, not RSS or full
native-memory accounting; allocator rounding and removed B-tree storage differ.

Seven alternating pairs with identical 96-byte synthetic non-joining records
measure native catalogue locking, scans and prepared enqueue/remove churn:

| Operation | Records / rounds | Old median ms | New median ms | Paired ratio |
| --- | --- | ---: | ---: | ---: |
| Scan | 64 / 1024 | 0.3359 | 0.4116 | 1.226 |
| Scan | 4096 / 1024 | 20.3379 | 23.7473 | 1.165 |
| Enqueue/remove | 64 / 1024 | 2.8094 | 1.6623 | 0.588 |
| Enqueue/remove | 4096 / 128 | 39.0147 | 13.2998 | 0.343 |

Updates improve and no longer allocate under the monitor. Scans remain bounded
at 64 weak records per batch but are slower; native joining, worker scheduling
and GC are excluded from this benchmark. Source/harness/binary snapshots retain
both the rejected mutex prototype and final implementation.

The actual fresh-context spawn/bind profile, seven alternating pairs and eight
measured rounds after warmup, gives final paired ratios **1.019/0.995** at
32/256 threads. Median loop times are **0.4286→0.4211 ms / 3.5804→3.6027 ms**.
Setup/shutdown, joins and value checks are outside timing; child startup can
overlap the loop. These observations establish neither a universal speedup nor
performance budgets. All 960 final application runs pass. Across workload/nursery cohorts, paired
median wall ratios range auto 0.921–1.028, one worker 0.917–1.045, four workers
0.921–1.027. These are observations, not universal performance budgets.

Evidence: `target/gc-validation/2026-10-18-reaper-catalogue/`. `catalogue-profile.json`
records isolated operations; `profile.json` records real spawn/bind; `apps-budget-final/`
contains the final application source archive and fresh-process observations.

## Reaper probe batches — 2026-10-18

Replaced the worker's per-batch Vec with fixed stack storage for 64 weak
completion records. An allocator-probed reference reproduces traffic in the
previous code (one allocation/64 bytes for the one-entry case) and confirms zero
allocation/deallocation for new snapshots and expired-weak batch disposal.
Catalogue insertion/removal allocation remains separate unfinished work.

An isolated native catalogue benchmark includes locking, exclusive-cursor
iteration and weak-borrow creation/disposal. It scans 64 or 4096 synthetic
non-joining records 1024 times. Seven alternating old/new pairs give paired
median ratios **0.741 / 0.705**, with median times **0.447→0.331 ms /
28.263→19.969 ms**. This isolates batching; it does not measure OS joins, worker
scheduling, GC pauses or end-to-end application speedups. Saved source, harness
and binary hashes make that scope explicit.

Evidence: `target/gc-validation/2026-10-18-reaper-batches/profile.json` and
`profile.py`. The candidate copies the exact production module plus the probe
harness. The baseline extracts its previous Vec expression into the same helper
shape. Full application and runtime validation is recorded in the checkpoint
report and [status](STATUS.md).

## Thread completion admission — 2026-10-18

`thread_admission_profile` measures actual raw spawn plus owner binding in fresh
runtime contexts, with eight measured rounds after warmup. Context setup,
shutdown, joins and result checks are outside timing; child startup can overlap
the admission loop. Seven alternating baseline/candidate pairs at 32 and 256
threads give final paired median ratios **1.057 / 1.050**. Median loop times
are **0.4406→0.4824 ms / 3.7305→3.9822 ms**. Cooperative native setup therefore
has a measured admission cost, roughly one microsecond per thread in these
runs. Earlier preserved comparisons vary; these are observations, not budgets.
The combined native creation phase avoids redundant state transitions while
preserving collection progress at notice, spawn and record allocation.

The final correctness-checked application matrix passes all 960 fresh processes
using the frozen compiler and previous atomic-registration checkpoint binaries.
Across workload/nursery cohorts, paired median wall-time ratios range:
automatic workers **0.979–1.036**, one worker **0.965–1.053**, four workers
**0.977–1.032**. These measurements cover startup, CSV, build-plan and routing
workloads with 1/4/8/16 MiB nurseries. They establish neither universal speedups
nor tail-latency guarantees. Native metadata accounting and explicit admission,
throughput, pause, tail and RSS budgets remain open.

Raw profile, binary/source hashes, application source archive and observations:
`target/gc-validation/2026-10-18-completion-registry/`. The application report is
`apps-final/report.json`; paired cohort observations are
`apps-final-comparison.json`. Earlier profiles and application runs are retained.

## Cooperative atomic registration and stale-stage replanning — 2026-10-18

Native atomic-cell construction previously allocated while RUNNING and could
grow registry tables under a collector-visible lock. Cell allocation and table
staging/disposal now run BLOCKED and off-lock. Publication rechecks current
capacity after collection or concurrent registration and moves live entries
without cloning cells. Heap initialization prepares the registry mutex backend.
The compiler already treats the constructor as relocating; scalar atomic
load/store/CAS/fetch-add remain nonrelocating and keep their existing paths.

Binding reserves space for every unbound record and preserves that reservation
through collection, so a newly allocated wrapper can bind without allocating or
parking. The owner index is a dense Vec: binding is one-shot and sweeping is the
only removal. Collection visits bound owners independently of unused capacity
reserved for raw execution-owned cells. This adds reserved key storage (eight
bytes per reserved key on these 64-bit targets); full native metadata accounting
and admission remain open. Burst retention stays bounded by live records, and
shutdown releases the backing.

An additional race was reproduced in both atomic and existing channel growth:
GC could retire records during staging, after which an oversized historical-peak
replacement was installed. Both new regressions fail before repair. Shared
`RegistryStage` retains the allocation request separately from rounded container
capacity. Install rejects requests above twice the current next-entry growth
target; existing BLOCKED off-lock paths discard/replan them. Insufficient stages
still retry after competing registration. Ordinary commit stays allocation-free.

New references check mutex preparation, zero allocator traffic during transfer
and late binding after sparse maintenance, and oversized-stage rejection after
retiring 257 owners. Native-phase tests force major/minor collection during cell
allocation, staging, disposal and a stale-stage retry; they check exact counters,
moved witness roots, native values and late binding. Minor retry setup supplies
fresh young work rather than counting an empty-nursery no-op. Seven deliberate
fault controls fail as intended, including RUNNING allocation, lock-held staging,
missing reservation, cloned live cells and historical-stage acceptance.

The reproducible `atomic_registration_profile` measures fresh native raw-cell
constructor loops, excluding RuntimeContext setup/shutdown and validating every
cell value outside timing. Seven alternating pairs at 1K/32K cells, sixteen
measured rounds after warmup, give paired ratios 1.718/1.818. Median loop times
are 0.0545→0.0940 ms and 1.726→3.111 ms: about 39–42 ns additional per raw cell.
This is a throughput cost for cooperative allocation, not a constructor speedup;
it excludes managed-wrapper construction/binding. No hot scalar-operation path
changed. A final 960-run native application comparison passes all independent
output/telemetry checks; cohort paired wall ratios are 0.960–1.041 (automatic),
0.978–1.048 (one) and 0.960–1.049 (four). Fresh-process wall time includes telemetry
and the time wrapper with warm filesystem caches. No universal budget is claimed.

Final validation: 920 workspace passes, zero failures and nine ignores; optimized
runtime 411 unit tests and nine doctests plus the armed bad-header detector;
twelve selected optimized integrations; 268 s390x checks; 50 generated-program
and 286 runtime ThreadSanitizer executions without reports (some filter overlap).
Compact-enum stress passes in 31.84 seconds with unchanged deadlines. All 134
profiled source hashes and both constructor-profile binary hashes match after
validation.

Reproduce the constructor profile with:

```sh
cargo build --release -p gcrust-rt --example atomic_registration_profile
GCR_GC_VERIFY=1 GCR_GC_POISON=1 target/release/examples/atomic_registration_profile 32768 16
```

Evidence: `target/gc-validation/2026-10-18-atomic-registry/report.json`. Earlier
measurements and gate logs before the stale-budget repair remain as intermediate
evidence. Completion-registry growth, coordinated off-pause shrinking/destruction,
constructor-cost optimization, full native accounting/admission, platform/soak
coverage and numerical startup/throughput/pause/tail/RSS budgets remain open.

## Resolve completed copy retries before shared publication — 2026-10-18

A BUSY-slot retry can become redundant as soon as its worker publishes the
claimed object. Returning those retries to the shared frontier needlessly wakes
helpers and breaks bounded local object continuations. After publication, the
collector now rewrites slots whose source forwarding header is complete and
removes those finished tasks. BUSY and unclaimed sources remain queued; no worker
waits on a copy owner. Already-updated and non-pointer slots need no further
visit, matching the ordinary slot visitor. Object/channel tasks preserve their
order. Both batch completion and streamed publication use this path. Atomic
claims, shared queue, active-worker accounting and notification predicates stay
unchanged.

The new `copy_graph_profile` example checks every node/edge/payload, cycle or tree
shape, exact live bytes and collection count outside timed GC. It covers one-root
cyclic and alias-free tree graphs at 1K/32K/262K nodes, automatic/one/four workers,
sixteen timed collections after warmup. A final 252-process checked comparison
uses seven alternating pairs per configuration; 180 normal-mode processes use
five pairs with verification/poisoning disabled. Both variants use identical
example/compiler options and preserved runtime inputs.

At 262K cyclic nodes, checked automatic copying improves from 45.52 to 7.22 ms
(median paired ratio 0.157); normal mode improves from 49.72 to 7.50 ms (0.151).
Checked forced-one/four ratios are 0.851/0.301. Alias-free tree controls are
mixed: checked 0.970–1.032, normal 0.969–1.024. These measurements do not establish
universal latency or memory budgets.

A 588-process root-enrollment matrix includes repeated registrations and
small/large sets. Its largest small-set forced-four ratio, 1.155, was followed
by a 225-process three-way repeat covering five configurations: candidate ratios
0.980–1.018, identical-baseline controls 0.988–1.050. Root timings include payload
validation, unlike the graph profile's GC-only timer. All 960 application runs
pass independent output/telemetry checks; cohort paired wall ratios range
0.951–1.053 (automatic), 0.950–1.033 (one) and 0.949–1.039 (four). Application wall
times include telemetry/time-wrapper costs and use warm filesystem caches.

Three new references pass under forced one/four workers: BUSY/unclaimed/forwarded
slots and task ordering, uncollected-generation boundaries, and Full/Compact
streamed alias graphs across nursery promotion and repeated major collections.
Six restored fault controls fail as intended. Resolution itself allocates
nothing. Inline-queue and store-only-claim prototypes were rejected after
measurements; their source/results remain in separate evidence directories.

Final validation: 915 workspace passes, zero failures and nine ignores; optimized
runtime 406 unit tests and nine doctests plus the armed bad-header detector;
twelve selected optimized integrations; 252 s390x checks; 50 generated-program
and 280 runtime ThreadSanitizer executions without reports. Compact-enum stress
passes in 32.01 seconds with unchanged deadlines. All 131 profiled source hashes
match after validation, and final graph binary hashes match both primary profiles.

Reproduce a checked graph run with:

```sh
cargo build --release -p gcrust-rt --example copy_graph_profile
GCR_GC_VERIFY=1 GCR_GC_POISON=1 target/release/examples/copy_graph_profile 262144 16 cycle
GCR_GC_WORKERS=4 target/release/examples/copy_graph_profile 262144 16 tree
```

Evidence: `target/gc-validation/2026-10-18-resolved-retries/report.json`.
Shared-queue allocation, off-pause registry maintenance/shrinking, native metadata
accounting/admission, additional platforms and long soaks, and numerical
startup/throughput/pause/tail/RSS budgets remain open. The extra retry-list pass on
alias-free work remains a possible optimization target.

## Reserve known global-root cardinality — 2026-10-18

Root-heavy minor collections exposed repeated hash-table growth during root
address enrollment. Global backing slots are distinct and always enumerated, so
the collector now reserves their known count before inserting addresses. Up to
sixteen slots retain the allocation-free inline path. Additional root sources
can still grow normally; repeated registrations deduplicate as before. Storage
remains job-local and is released after collection rather than retained in a new
cache. Automatic worker selection is unchanged: a 100-process policy sweep
found that adding workers did not improve this many-short-root-tasks workload.

The new `root_work_profile` runtime example aliases one fresh young leaf through
many distinct global slots. Every round checks all rewritten slots, payload,
exact promoted bytes, empty nursery and minor collection count. Only collection
is timed; initialization and validation are outside the samples. A final
168-process alternating comparison covers 16/4K/64K/1M slots, automatic/one/four
workers and seven pairs, sixteen timed collections after warmup. With verification
and poisoning enabled, automatic 1M-root collections improve from 54.95 to
23.89 ms (median paired ratio 0.435); 64K roots from 3.06 to 1.27 ms (0.413).
A further 120-process comparison disables verification and poisoning: automatic
1M-root collections improve from 52.93 to 22.22 ms (paired ratio 0.421), and 64K
from 3.00 to 1.23 ms (0.408). A 126-process small-set repeat uses 64 timed
collections per process and an identical-baseline control: candidate ratios
0.987/0.968 (automatic/one), sham ratios 1.001/1.001. The original shorter small
sample's 1.132 automatic ratio is retained in the evidence.

All 960 paired native application executions pass output and telemetry checks.
Cohort median wall ratios range 0.927–1.035 (automatic), 0.964–1.025 (one) and
0.924–1.134 (four). The 1.134 forced-four CSV outlier was repeated in a 63-process
three-way control: candidate/baseline 0.998 and identical-baseline/baseline 0.975,
with individual candidate ratios 0.797–1.241. These measurements establish no
universal latency or memory budget. Native application wall times include the
telemetry/time wrapper and use warm filesystem caches.

Five root-slot references pass at forced one/four workers. Allocation checks
prove one initial allocation and no enrollment growth for known large counts,
zero allocation for small counts, and preservation of additional/repeated root
sources. Two deliberate faults (omitted reservation and allocating on the inline
path) fail these checks. Evidence:
`target/gc-validation/2026-10-18-root-work/`.

Final validation: 912 workspace passes, zero failures and nine ignores; optimized
runtime 403 unit tests and nine doctests plus the armed bad-header detector;
twelve selected optimized integrations; 240 s390x checks; 50 generated-program
and 277 runtime ThreadSanitizer executions without reports. Compact-enum stress
passes in 32.23 seconds with unchanged deadlines. All 128 profiled source hashes
match after validation.

Reproduce an individual checked profile with:

```sh
cargo build --release -p gcrust-rt --example root_work_profile
GCR_GC_VERIFY=1 GCR_GC_POISON=1 target/release/examples/root_work_profile 65536 16
```

Set `GCR_GC_WORKERS=1` or `4` for explicit-policy controls. Root deduplication is
still serial; other root shapes, native-edge-heavy work, remaining collector
allocations, native metadata accounting, platform coverage and numerical
performance budgets remain open.

## Automatic workers account for remembered-object scans — 2026-10-17

Minor collections previously sized automatic parallelism from nursery occupancy
alone, even when scanning a large dirty old graph. Worker selection now adds the
traced bytes of each distinct remembered object: fixed references, interior
pointers and variable value tails. Raw byte tails contribute nothing; arithmetic
saturates. The existing card-overlap traversal and object deduplication supply
the estimate without another heap walk. Explicit worker overrides and automatic
operation on one CPU skip counting altogether. Major collection sizing is
unchanged.

The reproducible `remembered_work_profile` runtime example checks every old node,
shared promoted leaf, raw payload, remaining field and collection count. A
75-process policy sweep selected representative workloads, followed by a final
126-process comparison with seven alternating pairs per layout/policy and sixteen
measured collections after warmup. For automatic workers, 4 MiB of remembered
references improves from 4.83 to 4.00 ms (median paired ratio 0.833), and 32 MiB
from 25.74 to 11.50 ms (0.451). Small scans have ratio 1.030. Forced-worker controls
are mixed, including 1.071 for the large four-worker case; these observations
establish no universal timing budget. Earlier implementations and measurements
are retained alongside the final candidate.

All 960 paired application executions pass independent output and telemetry
checks against the previous source-occupancy policy. Median paired wall ratios
across application/nursery cohorts range 0.917–1.052 for automatic workers,
0.931–1.055 for forced one and 0.934–1.030 for forced four. Fresh-process timings
include telemetry and the external time wrapper, with warm filesystem caches;
these ranges include host/run variation.

Six policy references pass under automatic and forced one/four workers. Eight
fault controls detect ignored remembered work, duplicate-card counting, omitted
fixed/interior/value-tail references, raw-byte counting, arithmetic wrapping and
unnecessary estimation for policies that do not use it. Evidence:
`target/gc-validation/2026-10-17-remembered-work/report.json`.

Final validation: 911 workspace passes, zero failures and nine ignores; optimized
runtime 402 unit tests and nine doctests plus the armed bad-header detector;
twelve selected optimized integrations; 236 s390x checks; 50 generated-program
and 276 runtime ThreadSanitizer executions without reports. Compact-enum stress
passes in 32.11 seconds with unchanged deadlines. All 127 profiled source hashes
match after validation.

Root/native-edge-heavy workloads, additional platforms and long runs, numerical
latency/RSS budgets, remaining collector allocation work and native metadata
accounting remain open.

Reproduce a single correctness-checked profile with:

```sh
cargo build --release -p gcrust-rt --example remembered_work_profile
GCR_GC_VERIFY=1 GCR_GC_POISON=1 target/release/examples/remembered_work_profile 32768 16 16
```

Set `GCR_GC_WORKERS=1` or `4` for explicit-policy controls. The example reports
only timed collection samples after warmup; initialization and graph validation
are outside those samples. The saved `profile.py` orchestrates the alternating
baseline/candidate comparison using preserved binaries.

## Automatic worker sizing from application evidence — 2026-10-17

A native application baseline exposed excessive automatic GC parallelism: a
1 MiB nursery selected up to eight participants above the former 256 KiB cutoff.
Automatic selection now uses one participant per complete 2 MiB of collected
source, with at least one, capped by available CPUs/eight. Explicit
`GCR_GC_WORKERS` overrides retain their previous path.

The new `scripts/benchmark_apps.py` builds CSV aggregation, scheduling and routing
apps and validates every output against independent models. It preserves native
binaries, source/input archives and hashes, build times, raw telemetry/output,
per-process OS peak RSS and host context. Nonzero exits fail; timeout cleanup
terminates/reaps its process group. Trials rotate configurations and alternate
baseline/current order for each configuration, with a tested parity invariant.
Fresh-process wall time includes telemetry and the external time wrapper, under
warm filesystem caches. Runtime pause estimates remain per-run estimates.

The main baseline passed 240 executions; paired baseline/current measurement
passed 960 across automatic/1/4 GC workers and 1/4/8/16 MiB nurseries. Workloads:
48,000 CSV rows, 1,600 scheduling tasks and a 512-node/3,511-edge graph. At 1 MiB,
automatic CSV paired wall ratios are 0.644/0.625 (one/four application workers),
with median peak RSS about 0.55/0.58 MiB lower. At 4 MiB, ratios are 0.836/0.752;
at 8 MiB, 0.936/0.888. At 16 MiB they are 0.995/0.983. Explicit-worker controls
range 0.915–1.179; timings include run-to-run variation and establish no universal
budget. The source-occupancy heuristic still needs root/remembered-set-heavy and
other workload coverage.

Three policy references pass at forced one/four workers, including actual helper
pool initialization for explicit overrides; six fault controls fail as intended.
Eight harness tests pass. The post-profile source delta adds only a cfg(test)
reference; production bodies were verified identical. Compiler/runtime inputs
are frozen under the evidence directory's `tools/`. Final validation: 908 workspace passes, zero failures, nine ignores; optimized
runtime 399 units and nine doctests plus the armed detector; twelve selected
optimized integrations; 224 s390x checks; 50 native and 273 runtime
ThreadSanitizer executions without reports. Compact-enum stress passed in 35.37
seconds with unchanged deadlines. Eight Python harness tests pass. Source hashes
and the test-only post-profile delta are verified.

The explicit-four-worker CSV outlier (1.179) was checked in a separate 63-run
three-way control: median current/baseline 1.035, identical-baseline/baseline
1.013, with large individual spreads. This does not establish tail guarantees.
Evidence: `target/gc-validation/2026-10-17-auto-workers/report.json`.

## Running application comparisons

Build the compiler using the LLVM setup in the repository README, then keep a
compiler copy for GC comparisons. Each output directory must be new.

```sh
cp target/release/gcr target/app-benchmark-gcr
cargo build --release -p gcrust-rt --target-dir target/app-benchmark-runtime
python3 scripts/benchmark_apps.py --gcr target/app-benchmark-gcr \
  --runtime-lib target/app-benchmark-runtime/release/libgcrust_rt.a \
  --output target/app-baseline
```

After changing the runtime, rebuild its archive and reuse the same compiler copy:

```sh
cargo build --release -p gcrust-rt --target-dir target/app-benchmark-runtime
python3 scripts/benchmark_apps.py --gcr target/app-benchmark-gcr \
  --runtime-lib target/app-benchmark-runtime/release/libgcrust_rt.a \
  --baseline target/app-baseline/report.json --nursery-mb 1 4 8 16 \
  --output target/app-comparison
```

The runner rejects changed baseline binaries, a different compiler, application
source changes, incorrect output, malformed telemetry and pressure workloads that
never collect. Automatic policy leaves `GCR_GC_WORKERS` unset. macOS uses external
`time -l` RSS bytes; Linux uses `time -v` KiB converted to bytes. Linux parsing is
unit-tested; the application measurements above ran on macOS. Samples include
telemetry and wrapper overhead. Wall extrema are observed samples, not a tail SLA;
GC pause quantiles retain their runtime error bound.

```sh
python3 -m unittest discover -s scripts -p test_benchmark_apps.py
```

## Inline copy claims — 2026-10-17

Evacuation keeps up to eight pending heap-borrowed claims in a 480-byte job-local
container. Extra claims reserve the previous 28 KiB vector once. The prefix stays
in place; inline and spilled segments drain directly into the copy loop. Claims
never enter reusable worker storage. Published headers retain their forwarding
state; partially consumed drains and unwinding restore every unpublished header.
No new unsafe operations or runtime dependencies were introduced.

Six references pass at one/four workers with poisoning and verification, plus
29 focused parallel collector tests (two existing ignores). Tests cover ordering,
repeat drains, direct drop, partial/unwind rollback, zero allocator traffic for
eight claims, and one full spill reservation reused within a job. Seven restored
fault controls fail, including wrong spill-address offset in moving collection.

Earlier combined-drain and prefix-migrating prototypes regressed spill-boundary
timing and were replaced. Small-root profiles now use 4,096 collections; large
and channel cases use 64, identically for baseline/candidate. Two final segmented
700-process matrices pass payload/live-size/count checks. The first has no ratio
above 1.10; confirmation has a forced-four-worker outlier at 1.417. A focused
21-pair three-way control gives median candidate/baseline 0.966 and identical-
baseline/baseline 0.996, with substantial spread in both. These results do not
establish a stable latency budget or prove the cause of outliers. Earlier samples
and source remain available. Concurrent CPU-intensive work was also observed.

Final validation: 905 workspace passes, zero failures, nine ignores; optimized
runtime 396 units and nine doctests plus the armed detector; twelve selected
optimized integrations; 212 s390x checks; 50 native and 270 runtime
ThreadSanitizer executions without reports. Workspace compact-enum stress passed
in 35.20 seconds with unchanged deadlines. Source hashes verified. Evidence:
`target/gc-validation/2026-10-17-inline-claims/`. Larger jobs still allocate claim
backing. Shared queues, other collector allocation, registry shrinking, metadata
accounting/admission and application pause/RSS/tail budgets remain open.

## Reusable worker metadata — 2026-10-17

Four evacuation vectors (task batch, discovered work, allocation layouts and
reserved addresses) now use a thread-local lease. Every return/unwind clears
logical contents. Heap-borrowed copy claims remain scoped to the current job.
Idle participants initialize only their task batch. Nested collections and
collections after cache TLS destruction use uncached local storage.

Eight one/four-worker poisoned/verified references cover exact cold requests and
zero warm allocator traffic, unwind cleanup, thread isolation, nested leases,
actual nested pointer-policy collection, actual TLS-teardown collection and
cross-heap Full/Compact reuse. Seven restored fault controls fail as intended.
On this target retained Rust buffer requests total 22 KiB per active OS thread,
or 2 KiB for a thread that remains idle; TLS headers and OS costs are additional.
Caches die with their OS thread; coordinator
storage may outlive individual heaps. Native metadata accounting remains open.

Two 644-process matrices use identical baseline/candidate workloads and check
payloads, live sizes and collection counts. First paired ratios: roots
0.780–1.122, channels 0.823–1.200. Repeat: roots 0.796–1.056, channels
0.842–1.114. The first matrix's two >1.15 channel outliers became 1.019/1.028
on repeat. Both sets are retained; local timing does not establish a universal
budget. Copy-claim, shared-queue and other collector allocations remain.

Final validation: 899 workspace passes, zero failures, nine ignores; optimized
runtime 390 units and nine doctests plus the armed detector; twelve selected
optimized integrations; 188 s390x checks; 50 native and 264 runtime
ThreadSanitizer executions without reports. Workspace compact-enum stress passed
in 31.23 seconds with unchanged deadlines. Final source hashes verified. Evidence:
`target/gc-validation/2026-10-17-worker-scratch/`.

## Inline root enrollment — 2026-10-17

Parallel evacuation now keeps up to sixteen unique writable root addresses
inline before spilling once into its existing hash set. This removes the
root-deduplication table allocation for small sets; it does not remove the shared
work queue or worker buffers. Distinct slots holding the same object remain
distinct work items. Repeated registration retains storage proportional to
unique slots rather than total visits.

Four references cover exact address sets and iterator hints, zero inline Rust
allocator traffic, no further allocation for repeated spilled addresses, and
Full/Compact alias-slot relocation around the spill boundary. They pass with
one/four workers, verification and poisoning. Six restored fault controls fail
as intended.

644 final profile processes check moving payloads, exact live size and collection
counts, using identical baseline/candidate workloads. Small-root paired ratios
range 0.544–1.049, large/repeated-root 0.961–1.022, channel controls 0.877–1.105.
These are local results, not universal budgets. An initial non-inlined candidate
regressed large sets and was corrected; a sorted-vector prototype was rejected
for visit-proportional storage and duplicate-heavy slowdown. Evidence is retained.

Final validation: 891 workspace passes, zero failures, nine ignores; optimized
runtime 382 unit tests and nine doctests plus the armed detector; twelve selected
optimized integrations; 156 s390x checks; 50 native and 256 runtime
ThreadSanitizer executions without reports. Compact-enum stress passed in 31.19
seconds with unchanged deadlines. Final source hashes were verified. Evidence:
`target/gc-validation/2026-10-17-root-enrollment/`.
Registry shrinking during collection remains open: it bounds sparse scans and
preserves allocation-free owner binding, so deleting it is insufficient.

## Raw-channel remembered references — 2026-10-17

Minor evacuation now omits clean unbound queue slots. Completed minor copying
clears their remembered marker without retiring their execution-owned records.
Diagnostics, preflight scans and major collections retain complete roots; heap
cards preserve young children of old queued messages. Young sends re-arm the
native queue marker. A poisoned/verified one/four-worker reference checks these
contracts, FIFO payloads, backing release and retained registration. Test-only
thread-local observation checks the real collector selects the proper root mode.
Seven independently restored fault controls fail as expected.

504 local baseline/candidate profile processes use identical workload source,
seven alternating pairs, raw/bound queues and one/four workers. Raw minor paired
ratios are 0.274–0.636; raw major 0.942–1.071. Bound controls are mixed (major
0.931–1.267, minor 0.916–0.995). These are local workload results, not production
budgets or a universal speedup.

Final validation: 887 workspace passes, zero failures, nine ignores; optimized
runtime 378 units and nine doctests plus the armed detector; twelve selected
optimized integrations; 140 s390x checks; 50 native and 252 runtime
ThreadSanitizer executions without reports. Workspace compact-enum stress passed
in 31.81 seconds with unchanged deadlines. Final source hashes were verified.
The profile source differs from final source only by test-only instrumentation.
The broader GC objective remains active.

Evidence: `target/gc-validation/2026-10-17-channel-unbound/`.

## Remembered references in owned channels — 2026-10-17

Minor conditional snapshots omit queues with no newly committed direct nursery
references. Marker clearing occurs during the existing weak-owner sweep after
copy jobs finish. An included-record count pass lets entirely clean snapshots
allocate nothing. Major/read-only snapshots keep their previous single pass.
Raw unbound queues remain conservative. Heap cards retain young children of old
queued messages independently of the direct-reference marker.

252 final processes provide seven alternating baseline/candidate pairs per
configuration using the unchanged owner/queue example and the previous verified
binary. Exact collection counts, generations, backing and every queued payload
are checked. Minor timing includes one young-leaf admission/validation per cycle.
The initial extra major count pass regressed timings; it was removed, with the
initial samples retained separately.

| Owners | Queues/owner | Major ratio, 1 worker | Major ratio, 4 workers | Minor ratio, 1 worker | Minor ratio, 4 workers |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 1 | 1.101 | 0.970 | — | — |
| 256 | 1 | 1.008 | 0.972 | 1.037 | 0.809 |
| 2048 | 1 | 1.016 | 0.942 | 0.959 | 0.638 |
| 1 | 1024 | 1.006 | 1.033 | 1.050 | 0.928 |
| 64 | 16 | 0.980 | 0.967 | 0.972 | 0.782 |

These are median paired time ratios against the previous contiguous-metadata
checkpoint. Results are mixed: some large parallel minor workloads improve,
while some single-worker and major cases regress. No universal speedup,
application/startup/maximum-pause/RSS budget is established. Exact samples,
initial iteration, binary provenance, matching sources, controls and gate logs:
`target/gc-validation/2026-10-17-channel-remembered/`.

## Channel owner metadata — 2026-10-17

Sorted snapshots feed contiguous channel storage, with exact reservation by
distinct owner and a lookup map only for copy-activated source owners. Temporary
snapshot storage is freed before allocating that lookup map. Initial groups use
an exact-size iterator; individually allocated per-owner vectors are gone.
Sorting and a bounded number of metadata allocations remain inside the pause.

Scoped Rust allocator measurements for 1024 queues and distinct owners:

| Constructor | Old calls | New calls | Old requested bytes | New requested bytes | Old retained bytes | New retained bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Conditional source groups | 1055 | 5 | 221120 | 92176 | 67600 | 67600 |
| Read-only edges | 1035 | 3 | 192460 | 83976 | 100360 | 59400 |
| Old minor-GC owners | 1053 | 3 | 167820 | 57344 | 40960 | 32768 |

Realloc contributes its requested new layout and superseded old layout; these
figures are not physical memory traffic, allocator overhead or RSS. Shared-owner
conditional storage retains 8300 bytes for 1024 queues versus the original 8372.
The rejected linked prototype retained 67600 there. An earlier contiguous
iteration retained 67592 bytes for all-old distinct owners; the source-only map
reduces that to 32768 and a regression rejects the earlier footprint.

378 final processes provide seven rotated/reversed three-version runs per
configuration at 1/4 workers. Every run checks exact collection counts, backing
accounting and every queued payload. Major timing covers 64 repeated full
collections after warmup. Minor timing first tenures all owners/messages, then
includes one young leaf admission/validation per collection. Setup and final
execution teardown are excluded.

| Owners | Queues/owner | Major ratio, 1 worker | Major ratio, 4 workers | Minor ratio, 1 worker | Minor ratio, 4 workers |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 1 | 1.015 | 1.010 | — | — |
| 256 | 1 | 0.657 | 0.829 | 0.613 | 0.779 |
| 2048 | 1 | 0.676 | 0.772 | 0.650 | 0.758 |
| 1 | 1024 | 0.844 | 0.902 | 0.741 | 0.817 |
| 64 | 16 | 0.779 | 0.841 | 0.645 | 0.822 |

Candidate/baseline median paired time ratios span 0.657–1.015 for major and
0.613–0.822 for minor cases. Results remain local; no application throughput,
maximum-pause, RSS or startup budget is certified. An earlier compact-enum stress
timeout and near-limit saved-executable replay remain evidence of timing margin
risk; final unchanged deadlines pass. Exact timings, allocation requests,
source/binary hashes, rejected variants, fixture corrections and logs:
`target/gc-validation/2026-10-17-channel-root-groups/`.

## Demand-driven registry staging — 2026-10-17

Demand staging removes the speculative planning lock from ordinary construction.
Actual shortage triggers an extra native BLOCKED transition. Seven rotated and
reversed three-version runs per capacity give 63 processes using saved original
and planning binaries and the unchanged construction/empty-close example.
First use is included; final execution teardown is excluded.

| Slots | Paired demand/planning | Paired demand/original |
| ---: | ---: | ---: |
| 1 | 0.969 | 1.078 |
| 4097 | 1.017 | 1.032 |
| 131072 | 1.084 | 1.186 |

The small case improves about 3% over planning, while still costing more than
the original allocator-in-RUNNING path. Larger cases were slower in this sample.
No general speedup, application throughput, maximum pause or RSS budget follows.
Locked metadata migration, collector-side shrinking and other native domains
remain open. Exact samples, marginal medians, paired ratios, binary hashes and
provenance: `target/gc-validation/2026-10-17-channel-registry-demand/`.

## Staged channel registry growth — 2026-10-17

Seven alternating baseline/candidate pairs at three capacities provide 42
construction/empty-close processes. The unchanged example includes accumulating
raw registry insertions and checks backing accounting, but excludes final
execution teardown. The baseline binary was built and saved before this change;
binary hashes, samples and source hashes are in
`target/gc-validation/2026-10-17-channel-registry-growth/`.

| Slots | Median paired candidate/baseline ratio |
| ---: | ---: |
| 1 | 1.092 |
| 4097 | 0.990 |
| 131072 | 0.968 |

The one-slot case regresses about 9%; larger samples are noisy. Staging and
off-lock native disposal remove allocator work from RUNNING registration, but
add planning/transition work and transient metadata capacity. Migration still
holds the registry lock while moving O(live registry size) entries. This is no
general speedup or numerical application/pause/RSS budget. Collector-side
shrinking and native metadata accounting remain open.

## Channel synchronization preparation — 2026-10-17

Every capacity now completes native control synchronization while BLOCKED.
Construction baseline is the previous verified batched-retirement binary, checked
against its report. The operation baseline is rebuilt with that checkpoint's
runtime/native-retirement modules and exact restoration; public example code is
unchanged. Seven alternating pairs at five construction capacities give 70
processes. First use is separate from repeated construction/empty close.

| Slots | Before steady ns/pair | Prepared steady ns/pair | Paired ratio | Before first µs | Prepared first µs |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 477.33 | 315.79 | 0.932 | 5.167 | 4.875 |
| 4096 | 861.58 | 883.76 | 1.062 | 7.666 | 12.333 |
| 4097 | 1205.33 | 1152.28 | 1.199 | 30.167 | 54.625 |
| 8193 | 1707.41 | 1217.91 | 0.955 | 28.167 | 30.458 |
| 131072 | 1972.82 | 861.00 | 0.423 | 51.458 | 26.667 |

42 additional processes time 20000 send/receive pairs after collection setup at
three capacities. The operation binary also runs collections outside that timer;
this table makes no collection-cost claim. All processes check accounting and
zero final backing. Samples vary substantially; marginal medians need not equal
median paired ratios. No general speedup or numerical application budget is proved.

| Slots | Before ns/pair | Prepared ns/pair | Median paired ratio |
| ---: | ---: | ---: | ---: |
| 8 | 118.97 | 145.56 | 1.001 |
| 8193 | 256.42 | 296.12 | 1.155 |
| 131072 | 256.10 | 121.47 | 0.798 |

Allocator probes independently establish first primitive access without registry
warming and uncontended public-operation behavior: no Rust allocations or frees
for send/receive/sender/diagnostic paths, and no allocations during close. Close is
checked in a separate allocator-probe scope. These are first-use placement and
progress properties, not an OS allocation/latency bound for every operation.

Initial validation caught an atom-test dependency cycle: its held concurrent scan
waited for a now-BLOCKED one-slot constructor. The repaired test releases that
scan first, reloads roots and coordinates RUNNING readiness between eight cycles.
Twenty repeated debug/optimized positives and two restored watchdog controls pass.
A separate compact-enum timeout was not reproduced in its saved-binary replay
(5.859 s under the unchanged 60 s deadline) or final unchanged integration.

Evidence/replay, initial failures and all samples:
`target/gc-validation/2026-10-17-channel-sync/`. Registry/index allocation and
remaining native memory/pause/RSS/application budgets remain open.


## Grouped retirement versus individual handoffs — 2026-10-17

Baseline is the verified previous native-only worker with its pressure-policy
repair already included, not the older collector-thread deallocation baseline.
Archived baseline binary hashes are checked against that report; original source
and provenance are retained. The current candidate includes notification
pre-initialization. Seven alternating pairs at five construction capacities give
70 processes; two dead-channel batch cases add 28. Exact backing peaks and zero
final bytes are checked. Earlier candidate samples are retained.

| Slots | Individual steady ns/pair | Grouped steady ns/pair | Median paired ratio | Individual first µs | Grouped first µs |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 278.41 | 272.98 | 0.995 | 3.334 | 4.667 |
| 4096 | 617.20 | 606.17 | 0.964 | 5.292 | 6.541 |
| 4097 | 815.14 | 793.88 | 0.967 | 20.500 | 19.958 |
| 8193 | 636.23 | 628.98 | 1.004 | 17.250 | 22.375 |
| 131072 | 814.78 | 790.36 | 0.974 | 15.250 | 16.750 |

Collection timing excludes population, startup and clearing owner roots. Complete
cleanup also includes execution shutdown/worker join. Backing is empty; committed
payload pages and RSS are not measured. Marginal medians need not match median
paired ratios. These are local samples with variation, not application budgets.

| Slots | Queues | Individual collection µs | Grouped collection µs | Paired collection ratio | Individual total µs | Grouped total µs | Paired total ratio |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4097 | 256 | 83.333 | 74.667 | 0.904 | 123.292 | 140.209 | 1.148 |
| 131072 | 32 | 16.208 | 17.333 | 1.066 | 51.209 | 53.416 | 1.104 |

Tests independently observe one handoff for 64 swept/shutdown buffers, plus a
separate final-observer handoff. Grouping publishes later: the 256-buffer current
median retains 8,390,656 charged bytes at collection return versus 3,572,584 for
individual handoffs; both end at zero after cleanup. Less mailbox contention
trades against overlap. Bounded group sizing/overlap, full workload throughput,
pause/total cleanup budgets, startup and RSS need further work.

The current-thread Rust allocator probe checks building/publishing 64 reserved
nodes without any worker available to initialize notification for it: zero
allocations/deallocations. A cold-notification control fails, supported by local
std pthread Condvar source inspection. This establishes the handoff's Rust
allocation property on tested targets; it does not establish zero allocation for
all GC metadata, OS internals or every channel operation.

Evidence and replay: `target/gc-validation/2026-10-17-retirement-batches/`.


## Native retirement costs and pressure repair — 2026-10-17

These are unstable local samples, not a broad speedup or production latency
budget. Nearby host load observations were roughly 17–20 on 12 active CPUs.
Seven alternating pre-worker/current optimized pairs at five capacities produce
70 construction/empty-close processes. First use is timed separately; steady
loops include registry insertion and exclude managed wrappers/final teardown.
Every process checks exact peak backing and zero final backing/pressure GCs.
Earlier candidates, shorter loops and samples are retained, including their
variation. Marginal medians need not equal the median of paired ratios.

| Slots | Before first µs | After first µs | Before steady ns/pair | After steady ns/pair | Median paired steady ratio |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 3.625 | 3.708 | 310.14 | 480.42 | 1.097 |
| 4096 | 8.584 | 13.375 | 710.92 | 655.64 | 0.930 |
| 4097 | 8.333 | 26.500 | 852.39 | 1063.90 | 1.224 |
| 8193 | 6.958 | 25.750 | 647.44 | 1288.47 | 1.559 |
| 131072 | 6.083 | 26.000 | 756.02 | 2026.37 | 2.671 |

Another 28 processes provide seven alternating pairs for two dead-channel batches.
Empty backing is populated before timing while owner roots keep every allocation
live. Roots are cleared before the collection timer starts; complete cleanup
also includes execution shutdown/worker join. Exact backing peaks and zero final
bytes are checked. Payload pages/RSS are not measured. These samples do not close
performance budgets; per-buffer handoffs and shutdown remain optimization targets.

| Slots | Queues | Before collection µs | After collection µs | Paired collection ratio | Before total µs | After total µs | Paired total ratio |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4097 | 256 | 100.083 | 202.916 | 2.159 | 101.708 | 287.583 | 3.002 |
| 131072 | 32 | 27.333 | 40.334 | 1.011 | 29.042 | 111.291 | 4.356 |

Deterministic stalled-worker tests establish that collection can finish while
native release is paused, charges remain until release, and pressure/shutdown
waits allow unrelated GC. They do not establish average throughput or hard
maximum pauses. Next work: batch preallocated handoffs at registry retirement
without payload destruction/allocation under GC; preserve native-observer
ownership and measure both pause and total costs.

Initial broad validation found pressure growth inflation from pending garbage.
The unchanged fixture initially peaked at 4,980,768 bytes optimized / 5,505,056
debug, beyond its existing 1,310,720-byte assertion. Serializing pressure service
and adapting after deferred release repairs it: identical source now peaks at
786,464 native/JIT normally, 524,320 under stress, with zero final bytes. Both
before/after binaries, metrics and source hashes are retained. Hard admission and
other native resource domains remain open. Evidence/replay and all samples:
`target/gc-validation/2026-10-17-native-retirement/`.


## Sparse channel roots — 2026-10-17

The saved pre-change runtime scans capacity; the current runtime scans occupancy.
Seven alternating optimized pairs for nine cases produce 126 actual processes.
Each process times 128 full single-worker collections of the same live queue graph
and, after validating/draining it, 20000 send/receive pairs on an empty queue.
Each timed operation pair checks success and returned pointer identity. Queued
payloads/FIFO order and zero final native backing are checked outside timing.

| Slots | Messages | Before collection µs | After collection µs | Median paired collection ratio | Median paired send/receive ratio |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 8 | 0 | 1.372 | 1.035 | 0.734291 | 1.026 |
| 8 | 1 | 1.397 | 1.051 | 0.743263 | 1.025 |
| 8 | 7 | 1.482 | 1.460 | 0.985070 | 1.011 |
| 8193 | 0 | 346.739 | 1.048 | 0.002970 | 0.994 |
| 8193 | 1 | 350.325 | 1.092 | 0.003117 | 1.007 |
| 8193 | 7 | 345.482 | 1.498 | 0.004327 | 1.011 |
| 131072 | 0 | 5962.285 | 1.021 | 0.000171 | 1.009 |
| 131072 | 1 | 5998.323 | 1.068 | 0.000176 | 0.971 |
| 131072 | 7 | 6604.616 | 1.624 | 0.000254 | 1.033 |

The reference test checks exact FIFO slot visitation for empty, full, partial and
wrapped rings. The runtime model checks actual operations, moving values in both
heap modes, and scanning with the state mutex held. Five restored fault controls
all fail. Capacity-dependent work is removed for sparse queues; large dense queue
throughput, contention, tail latency, RSS and admission budgets remain unproven.
Evidence, exact binary hashes, source snapshots and replay helpers:
`target/gc-validation/2026-10-17-channel-scan/`.


## Channel construction and empty close — 2026-10-17

Saved pre-change channel functions versus current optimized construction/close,
seven alternating pairs per capacity (98 processes). The loop includes registry
insertion; it excludes managed wrappers and final execution teardown. Queues are
empty by construction. Every process checks exact peak backing bytes, zero final
backing bytes and no pressure collections. The dedicated reference test checks
all slots are initialized to zero at capacities 1, 4096, 4097, 8193 and 131072.

| Capacity (8-byte slots) | Baseline median ns/pair | Current median ns/pair | Median paired current/baseline |
| ---: | ---: | ---: | ---: |
| 1 | 255.68 | 267.86 | 1.043 |
| 8 | 255.81 | 264.11 | 1.043 |
| 512 | 326.02 | 332.77 | 1.029 |
| 4096 | 626.26 | 623.74 | 1.014 |
| 4097 | 535.39 | 571.77 | 1.088 |
| 8193 | 560.67 | 602.17 | 1.076 |
| 131072 | 1014.31 | 1084.62 | 1.053 |

Large backing is allocated and released while BLOCKED; reservations stay charged
until actual release. Explicit zeroed allocation avoids a separate initialization
pass. An initial iterator-based candidate had a much larger local 1 MiB ratio;
both candidate profiles are retained. These measurements do not establish
application throughput, contention, maximum pauses, RSS or hard admission limits.
Evidence and replay: `target/gc-validation/2026-10-17-channel-buffers/`.


## Native FFI allocation/release — 2026-10-17

Same optimized binary, legacy v1 allocation/free versus current v2 owned and
accounted tokens. Seven alternating pairs per size produce 98 processes. Each
process checks full zero initialization outside timing, first/last bytes during
repetition, exact iteration count and zero outstanding bytes/pressure collections
afterward. The loop includes payload access and validation; these are local
steady-state samples, not isolated allocator or end-to-end C-call guarantees.

| Payload bytes | Legacy median ns/pair | Current median ns/pair | Median paired current/legacy |
| ---: | ---: | ---: | ---: |
| 0 | 21.7 | 46.7 | 2.173 |
| 8 | 14.8 | 38.2 | 2.519 |
| 512 | 25.1 | 53.0 | 2.088 |
| 32728 | 394.4 | 443.3 | 1.167 |
| 32768 | 410.5 | 474.9 | 1.176 |
| 32769 | 920.2 | 996.1 | 1.082 |
| 1048576 | 6935.5 | 6530.6 | 0.968 |

Ownership is separate from the payload so a power-of-two request keeps its
original allocator geometry. The initial prefix experiment changed that shape
and showed a large-request timing cliff; initial artifacts are retained. Final
samples show small-operation accounting/ownership cost and near-parity 1 MiB
allocation/free in this environment. They do not attribute every timing change
to one optimization or establish application, contention, RSS or tail budgets.
The shared ledger no longer takes the channel-registry lock for FFI operations;
monotone peak updates avoid an unnecessary RMW below an existing peak.

Large native phases run BLOCKED. Functional tests separately prove collection
finishes while a native phase is paused, pressure preserves owners/live buffers,
and charges remain through deallocation. Source, environment, saved before/after
early-return binaries, fault controls and replay helpers:
`target/gc-validation/2026-10-17-ffi-buffers/`.

## Cooperative FFI copies — 2026-10-17

New versioned helpers bound scalar SC work to 32 KiB pieces and immutable
String copies to 256 KiB pieces, polling and reloading the owner between pieces.
The noncollecting leaf threshold remains 32 KiB. Small paths are separate from
outlined rooting/copying paths; ARM64 disassembly confirms bounded scalar paths
tail-branch to legacy helpers rather than entering the large helper.

Seven alternating pairs per case compare legacy and versioned extern helpers
in the same optimized runtime binary. All 448 processes across 32 cases check
complete byte output, canaries/NUL, iteration count and independent checksum
models. No collection is requested in these throughput profiles; an assertion
also checks that no collection occurred. Scalar profiles cover 1/8/64-bit
storage, input/output directions and the 32 KiB boundary. String cases cover
both 32 KiB and 256 KiB boundaries. Functional tests additionally cover every
supported scalar width, UTF-8 and collections requested during copying.

| Operation | Bytes | Bits | Median paired new/legacy time |
| --- | ---: | ---: | ---: |
| string | 8 | 8 | 1.059 |
| string | 1048576 | 8 | 1.061 |
| in | 1048576 | 8 | 1.006 |
| out | 1048576 | 8 | 1.001 |
| in | 1048576 | 64 | 1.004 |
| out | 1048576 | 64 | 1.009 |

The initial implementation cost about 20–30% for tiny scalar copies. Separating
bounded leaf paths from outlined large paths reduced the measured cost; the
final ARM64 entry-point assembly is retained. A 32 KiB immutable String piece
cost about 22% at 1 MiB. A 256 KiB piece reduced that measured cost to about 6%
while retaining bounded per-piece work and a tested mid-copy collection point.
The scalar work bound was not enlarged. Earlier binaries, source variants and
profiles remain available; timing results are local samples, not a universal
latency or throughput guarantee.

These measurements exclude complete foreign-call transitions, native buffer
allocation/free and C work. They establish no tail-pause, whole-application or
RSS budget. Native allocation accounting/admission and latency remain open.
Evidence, environment, binary hash, independent model/replay script, all samples
and source snapshots: `target/gc-validation/2026-10-17-ffi-copy/`.

## Profile-guided string-range copying — 2026-10-17

A native sample of the CSV workload found 741/753 active-worker samples in
whole-input UTF-8 validation under `ai_str_substring`; main/reaper/helper threads
were mostly waiting. Each small field range rescanned the complete file buffer.
The range helper now checks endpoints, owns the selected bytes and validates
that copy before any moving allocation. Work scales with copied bytes, not
surrounding input. Ordinary native construction takes `&str`; concat/join retain
a private UTF-8-preserving buffer path with an explicit validity/lifetime contract.
No unchecked Rust text, header cache or per-string metadata is introduced.

Seven alternating native runtime pairs, default adaptive GC workers, a 1 MiB
nursery and 64 MiB tenured spaces were measured for four 5000-row quoted-Unicode
CSV files. All 28 timed runs matched an independent grouping model and identical
per-configuration allocation counts/bytes. One app worker: baseline median
1510.26 ms, candidate 23.08 ms; median paired time ratio 0.01531 (range
0.01472–0.01618). Four app workers: 431.44 ms to 17.51 ms; paired ratio 0.04072
(range 0.03864–0.04173). This is about 65x/25x for this specific local workload,
including startup, I/O, parsing/grouping, managed threads and shutdown. The same
compiler links distinct clean-built runtimes, with per-file source hashes and
different native substring instruction sequences verified before measurement.

Construction controls compare complete large ASCII/Unicode outputs over 2000
concat/join operations. Seven alternating pairs gave median candidate/baseline
ratios 1.002 (ASCII concat), 1.017 (ASCII join), 0.994 (Unicode concat), 1.004
(Unicode join); ranges and all 56 timed runs are retained. An initial design
rescanned already-valid concat/join output and regressed Unicode construction
about 3.9x; it was rejected. Complete valid UTF-8 sequences/separators preserve
validity under concatenation, so the final path avoids that redundant scan.

Evidence/replay: `target/gc-validation/2026-10-17-string-allocation/`, including
sample, raw app/construction results, checksums/model, library/assembly/source
hashes, both source snapshots and rejected variants. These local measurements
are not general application, tail-latency, RSS or platform release budgets.

## Variable-length runtime allocation — 2026-10-17

`crates/gcrust-rt/examples/runtime_varlen_profile.rs` profiles the native runtime
entry with header/count/zero/checksum checks enabled, small nursery pressure and
matched allocation/collection counts. It includes byte/value tails, flattened
interior references and a large-object control. Run
`cargo run --release -p gcrust-rt --example runtime_varlen_profile -- 7`.
Each timed sample excludes context construction/destruction; these are allocation
loop measurements, not full application/startup/RSS or tail-pause budgets.

Seven alternating baseline/candidate process pairs, seven samples per case per
process and excluded warmups were measured on Apple M2 Max, Rust 1.96.0. Both
single-worker and default adaptive worker selection were measured. Under default
selection, median candidate/baseline time ratios were 0.596 (zero-byte tails),
0.741 (64-byte tails), 0.946 (1024-byte tails), 0.782 (16-value tails), and 0.507
(a shape with 64 interior references). Every paired ratio for these five cases
was below one. The large 8200-byte control was mixed (0.866–1.033, median 0.954);
no consistent benefit is claimed for the large path. Single-worker ratios and
all raw samples are recorded separately; they are not substituted for defaults.

A separate native `gcr-csvreport` workload parsed/grouped four 5000-row quoted
Unicode CSV files, with default adaptive GC workers, a 1 MiB nursery and 64 MiB
tenured spaces. Seven alternating pairs for one/four application workers all
matched an independent grouping model and identical per-configuration compiled
allocation counts/bytes. The same compiler linked distinct, freshly built runtime
archives; disassembly confirms the local branch only in the candidate. Median
paired candidate/baseline wall-time ratios were 0.999 (one worker, range
0.980–1.009) and 1.005 (four workers, range 0.999–1.008). No consistent application
speedup was observed. Timings include startup, I/O, parsing/grouping, threads and
shutdown; the microbenchmarks exclude context setup/teardown. Both results are
retained rather than substituting the allocation-loop gain for an app result.

The candidate reuses immutable layout validation and avoids compiled-frame
publication for successful non-collecting local requests. The retained checked
geometry, structural fallback and collecting/stress/large paths are correctness
requirements. Evidence/replay, CPU/tool versions, binary hashes, identical
benchmark-source hash, matched checksums/collection counts and both source
snapshots: `target/gc-validation/2026-10-17-varlen-fast/`. Microbenchmark gains do
not establish general application throughput, latency or production budgets.

## Native orphan completion cleanup

One execution-owned native worker starts at first owned-thread admission. GC
indexes weak owner-dead records; the worker probes at most 64 weak entries per
batch and retries unfinished/contended entries with adaptive 1–100 ms polling.
The backlog scales with known orphans and drains without future GC. A rejected
fixed queue demonstrably stranded overflow until another collection. Native joins
and panic-payload destruction occur outside collector/registry locks. Shutdown
joins the worker, including TLS cleanup and late admissions, before code unloading.
Ordinary join entry bodies and Thread ABI source match the prior checkpoint.

Structural/resource evidence includes a 257-entry backlog with observed batches
<=64, 128 actual unobserved spawn lifetimes ending with empty managed/native
record/admission storage, GC progress during child/worker TLS waits, independent
heap shutdown, native panic reentry/fatal cleanup and final Heap Arc release.
Final gates pass 771 debug and 771 optimized checks including armed detector,
nine remaining intentional ignores; 284 runtime units + nine doctests; eighteen
poisoned/verified checks; twelve fault controls; sanitizer 36 native / 133 runtime
executions with no reports. All 365 input hashes match. Evidence:
`target/gc-validation/2026-10-16-orphan-reaper/`.

No throughput, pause, tail-latency or whole-process RSS improvement is claimed.
Costs include one native worker/stack per execution using owned threads, a weak
index proportional to known orphans and bounded temporary scan storage. Blocking
native cleanup can delay that execution's worker while GC and other heaps
progress. Old-generation weak owners conservatively survive minor GC, so native
resource pressure and generation-dependent retention need measured collection
policy/budgets alongside application measurements.

## Conditional channel owners

Safe channels now permit collection of abandoned queue/reference cycles. Native
operations add a scoped owner root before parking; they read the weak owner from
stable native storage without a registry lookup. Construction registers and binds
a control to its GC wrapper. Copy worklists activate queue scans on owner discovery;
uncollected/already-forwarded owners seed groups. Concurrent copying scans native
queues only during its final pause. Promotion preflights and snapshots also build
owner-edge views. Sparse native indices shrink after collection. These add owner
bookkeeping and temporary graph metadata; no application speed, pause, or RSS
improvement is claimed. Correctness/resource evidence: 758 debug workspace
checks and 758 optimized checks including the armed detector, nine remaining
intentional ignores; 272 optimized runtime units plus nine doctests; seven
poisoned/verified ownership checks; twelve fault controls; 34 native sanitizer
runs / 121 runtime executions, no reports. Final hashes cover 362 inputs.
Source, controls and logs: `target/gc-validation/2026-10-16-channel-ownership/`.
Large-capacity queue scan costs and application/native/RSS/latency budgets remain
unmeasured.

## Native atomic owners and burst metadata

AtomicI64 now allocates a GC reference wrapper and weakly binds its stable native
cell to that identity. GC can release cells after their last safe alias dies;
unbound raw cells retain execution lifetime. Ordinary native SC operations still
use the same source bodies, with no allocation/safepoint/registry lookup. Codegen
shares the nonrelocating contract; construction remains allocating. Wrapper field
reads retain normal GC barriers, so no whole-operation speed claim is made.

Sparse atomic/completion indices shrink after retirement. This avoids scanning
historical managed-owner capacity on later GCs; compaction itself can rehash
survivors as amortized maintenance. A 10,000-owner burst with three raw-history
sizes checks metadata returns toward surviving handles, and retained-heap
shutdown releases empty table storage. 128 real cell lifetimes reclaim native
ownership before root shutdown; 10,000 raw records invoke one weak-owner callback.
These are structural/storage results, not measured throughput/latency/RSS gains.

Final source passes 749 optimized workspace checks including armed detector,
266 debug runtime units plus nine doctests, five poisoned/verified checks and the
expanded sanitizer gate: 32 native runs / 108 runtime executions. All 359 source
hashes match; Thread ABI/native atomic operation fingerprints match prior source.
The previous full debug workspace (741) predates this follow-up. Evidence:
`target/gc-validation/2026-10-16-atomic-ownership/`.

## Failed admission and reusable closure roots

An owning handoff token releases its global env root and returns the slot even
before the child has a registered owner. Successful transfer disarms it before
publishing vacancy, preventing later destruction from clearing a reused slot.
128 failed admissions across semi-space/generational heaps retain one reusable
slot and no envs after collection. This bounds handoff roots, not raw native
completion history, whose execution lifetime remains documented.

One temporary heap Arc clone bridges handoff to registered ownership per spawn.
Thread ABI and allocation entry-point source bodies match prior fingerprints;
no throughput/RSS improvement is claimed. Final gates pass 741 debug workspace
checks, 262 optimized runtime units including detector plus nine doctests,
three poisoned/verified tests, and the full sanitizer gate (30 native / 104 runtime
executions). All 357 source/test hashes are unchanged. Prior full optimized
workspace: 740 checks before this follow-up. Evidence:
`target/gc-validation/2026-10-16-admission-handoff/`.

## Runtime registration ownership

Main contexts and children share one owning constructor. Registration retirement
precedes boxed poll storage on normal return and setup unwind. There are no
additional owning Arc fields in RuntimeContext, and the child transfers its
existing heap Arc into the owner. Cleanup failure is fatal before code/storage
can retire; failed deregistration keeps coordinator exclusion through abort.
The bounded child-failure regression proves registry/env cleanup before collecting.

The Thread ABI and allocation entry-point source bodies are byte-for-byte unchanged
from the coordinator-unwind source archive (fingerprints retained). No measured
throughput or RSS improvement is claimed. Final source passes 740 optimized
workspace checks including armed detector, 261 debug runtime units plus nine
doctests, two poisoned/verified tests, and the full sanitizer gate: 30 native runs
and 103 runtime executions. All 357 source/test hashes are unchanged across gates.
Evidence: `target/gc-validation/2026-10-16-runtime-retirement/`.

## Joined completion metadata ownership

Safe Thread spawn now binds its native record weakly to the traced Thread wrapper.
GC relocates owners after copying and before source reset, preserving every live
alias. Once native join has finished and the managed owner is collected, native
record and shutdown-admission bookkeeping can retire. Dead owners are cleared
permanently, so arena reuse cannot resurrect a native record. GC uses try-lock
inspection and never performs OS joins/finalizers while mutators are stopped.

The registry separates records, managed-owner keys and weak shutdown admissions.
Hash-indexed binding avoids linear lookup, and per-GC owner work excludes retained
raw history. The worklist is drained outside the lock and keeps accepting newly
admitted descendants. It uses no mutable-vector cursor that pruning could shift.
A 10,000-record legacy history test invokes only one managed-owner callback;
128 real spawns with 256 collections leave native records, admissions and owner
indices empty, with bounded map capacity. These are structural/resource evidence,
not measured throughput or whole-process RSS improvements.

Validation: 735 full debug workspace checks; 256 optimized runtime unit checks
including armed detector plus nine doctests; 17 integrations; six poisoned/verified
ownership regressions; full sanitizer gate (30 native/99 runtime executions).
All 355 non-Markdown input hashes stayed unchanged across final gates. Matching
source, fault controls and logs:
`target/gc-validation/2026-10-16-completion-ownership/`.
The prior 727-check full optimized workspace predates this source.

Unfinished and never-joined records still retain execution lifetime, as do legacy
raw handles. Native AtomicI64/channel retention, automatic native reaping,
retirement-unwind/poison handling, RSS/latency budgets and other readiness audits
remain open. See [threads.md](threads.md) for raw borrowing/managed-owner contracts.

## Bounded lifetime telemetry and streamed full traces

Each heap retains the most recent 1,024 events, exposed by `Heap::gc_events()`
and `Heap::gc_log_jsonl()`. These are recent snapshots, oldest first, and their
first sequence number may be nonzero. Lifetime collection counts, total/max
pause, elapsed, reclaimed and promoted bytes are maintained independently; they
do not reset or drop contributions when events leave the window. Count/time/byte
totals saturate at u64::MAX and then represent lower bounds.

Pause quantiles use exact nearest rank while every event fits in the window.
Before the first eviction, all original samples seed fixed integer histograms;
subsequent samples update them. The four histograms (combined plus each kind)
have 3,776 u128 counters apiece, allocated only on rollover. Their bucket storage
is fixed at 241,664 bytes per heap, independent of collection count; event-window
and trace-buffer storage are also bounded. This is a native metadata bound, not
an RSS or total process-memory budget.

Each power-of-two interval has 64 equal-width buckets. Long-run p50/p99 report
the selected bucket's upper endpoint, capped by the observed maximum: the integer
nanosecond estimate overstates a positive exact lifetime quantile by less than
1/64 of its value; zero stays exact. JSON exposes `gc_pause_quantile_error_ratio`: zero for exact values,
0.015625 for histogram estimates. Human summaries and benchmark labels identify
estimates. Millisecond formatting rounds separately (four decimals in JSON,
three in text); the histogram bound applies before that rounding. If even the
u128 sample census overflows, quantiles fall back to the recorded maximum and
the ratio is -1 (no certified relative error bound). Exact long-run quantiles can
be computed offline from the full trace.

`GCR_GC_LOG=<path>` opens before user execution and streams every event through
a bounded 8 KiB runtime buffer. The JSON-lines schema is unchanged; normal completion flushes
the tail. It does not reconstruct a full trace from the recent snapshot at exit.
Embedders can call `Heap::start_gc_log(path)` or `set_gc_log_writer(writer)` before
the first collection, then `flush_gc_log()` explicitly. Late/repeated
configuration returns an error before opening a replacement file. Writer
callbacks must not reenter the managed runtime or heap telemetry. Trace I/O is
synchronous and opt-in, so a slow sink can delay collection; ordinary allocation
paths gain no tracing work. The sink's own storage policy is its owner's concern.

The first write/flush error persists; later collection still updates metrics but
stops attempting trace writes. Runtime-owned buffered bytes are discarded without
I/O on failure, preventing an implicit retry when the heap is dropped. JIT/AOT trace-creation errors prevent user entry;
exit-time failures are visible errors. Shared finalization attempts both metrics
output and trace flush, so either sink's failure cannot suppress the other sink.
`GCR_METRICS_FILE` write errors are now reported instead of silently discarded.

Validation includes rollover/storage bounds, independent sorted quantile oracles
across the u64 range, census-overflow fallback, concurrent full-stream ordering,
persistent sink failures and finalization independence, plus real JIT/native
stress runs whose lifetime metrics agree with offline full-log analysis. Five
negative controls fail when eviction/seeding/error propagation/full-log
preservation are broken; the failed-buffer teardown regression also reproduces
the old implicit retry. Broader gate results and matching source are recorded
in `target/gc-validation/2026-10-16-bounded-telemetry/`.

Final gates passed: 727 optimized workspace checks including the separately armed
detector, 250 debug runtime unit checks plus nine doctests, 14 debug integrations,
and the full sanitizer gate (30 native runs, 93 runtime test executions). All 354
non-Markdown validation-input hashes stayed unchanged across the final gates.
The 716-check full debug workspace checkpoint predates this change; this is not
a new full debug workspace gate. No measured throughput/RSS improvement is
claimed from this resource-bound repair. Long-running operating budgets remain
open.


## Allocation-profile arithmetic and integration checkpoint

Allocation-site accounting now saturates at `u64::MAX` across native updates,
generated fixed-object TLAB updates, live/retired thread merges and text/JSON
report totals. Saturated values are lower bounds. The 16-byte counter ABI and
owner-local updates remain; generated allocation adds no locks, atomics or
native calls. This change has not been benchmarked as a performance improvement.

Boundary regressions seed near-limit counters, check exact-limit and overflowing
updates, verify every live/retired partition and cross-site totals, and execute
actual generated TLAB allocation before and after LLVM optimization. Restoring
old native/generated arithmetic makes the corresponding regression fail.

The full workspace exposed a stale safepoint fixture that expected unnecessary
majors from dead nursery pressure. It now promotes a live ~800 KB array, releases
it, and retains a ~320 KB young array in a 1 MB old space. Required promotion
therefore forces a major while an allocation-free worker spins. Both array
payload checks and the assertions requiring minor and major collections remain.

Validation: 716 full-workspace debug checks (nine intentional ignores); 242
optimized runtime unit checks including the separately armed detector, nine
optimized runtime doctests, and the optimized generated-counter boundary test;
30 instrumented native runs and 85 runtime sanitizer test executions. This is
not a full optimized-workspace checkpoint. Sources, initial fixture failure,
negative controls and final logs are retained under
`target/gc-validation/2026-10-16-allocation-profile-saturation/`.

## GC telemetry arithmetic limits

Per-event pause/elapsed nanoseconds and cumulative GC duration, reclaimed-byte
and promoted-byte totals now saturate at `u64::MAX`. A saturated value is a lower
bound. This prevents large duration conversions from wrapping to zero and totals
from panicking in debug or wrapping in optimized builds. Allocation-site counters
are covered by the subsequent end-to-end saturation repair below.

Two boundary regressions independently reproduce the old conversion and summation
failures. Current validation: 241 debug runtime unit checks, nine passing runtime
doctests, and all five optimized telemetry checks. This arithmetic-only follow-up
does not claim a new sanitizer or performance result. Source and logs:
`target/gc-validation/2026-10-16-telemetry-overflow/`.

## Combined sampled heap occupancy

`peak_heap_bytes` now reports a per-heap maximum of active old-space plus nursery
occupancy observed at one stopped-world census. Sampling precedes major/minor
reclamation, occurs at both concurrent pauses, and accompanies inspection pauses.
Metrics emission reuses its existing allocation-profile inspection pause, so a
run without collections still reports its allocated occupancy. The benchmark
catalog labels this value **sampled heap peak**; Rust callers can read the
retained sample through `Heap::sampled_heap_peak_bytes()`.

The sample includes bump/TLAB reservation slack. Inactive copy-arena capacity,
native allocations and RSS require separate accounting. It is not a continuous
physical-memory peak. Older artifacts used maximum per-event collected-space
occupancy; comparisons require matching definitions.

Regressions check two 24-byte objects across generations report 48 bytes, retain
that sample after reclamation, cover no-GC runs, and capture actual source-arena
growth during concurrent copying. Omitting nursery occupancy or the final
concurrent sample fails its regression. Evidence:
`target/gc-validation/2026-10-16-combined-heap-peak/`.

## Telemetry semantics

Concurrent `pause_ns` now sums the two world-pause coordination intervals;
copying while mutators can run is excluded. The additive `elapsed_ns` GC-log
field retains whole concurrent cycle duration. For stopped-world events,
`elapsed_ns` equals the work charged to `pause_ns`; a minor preceding/following
its separately recorded major does not double-count that major. Machine metrics
and the `gcr bench` catalog include additive `gc_elapsed_total_ms`.

Text and JSON p50/p99 use nearest rank: `ceil(sample_count * percentile) - 1`.
For 100 samples of 1–100 ms, p50 is 50 ms and p99 is 99 ms. Empty machine-metric
samples yield zero. A real concurrent-cycle test inserts delay during copying
and verifies it contributes to elapsed time while remaining outside pause time.
Restoring either the whole-cycle pause calculation or old rank indexing fails
its regression. Source/log evidence:
`target/gc-validation/2026-10-16-gc-telemetry/`.

The subsequent bounded-telemetry repair removes event-history growth with
collection count. Native/RSS and continuous occupancy budgets require separate
accounting and remain in the readiness ledger and project pad.

## Promotion capacity under old-space pressure

The normal occupancy bound remains the first check. When it does not fit,
reachable-young preflight avoids major collection caused only by dead nursery
occupancy. A rooted 4080-byte old object in a 4096-byte arena plus an unrooted
24-byte nursery object now completes one minor with **zero majors**. A remembered
two-node young cycle exactly filling 64 bytes of headroom likewise needs no
major despite larger garbage occupancy. Necessary-major cases still collect.

The preceding-major fallback now marks whole-graph nursery liveness to avoid
retaining old garbage through dead young wrappers. This fixes false exhaustion;
it is not a measured speedup. Preflight/marking time is included in the minor
pause metric without double-counting the separately recorded major. These slow
paths use native mark sets, whose large-workload pause and memory budgets remain
unmeasured. Evidence: `target/gc-validation/2026-10-16-promotion-liveness/`.

## Oversized generational allocation

Objects larger than nursery capacity now try tenured storage directly. They
scavenge the nursery and reclaim tenured storage only after exhaustion, preserving
dead-young-owner cleanup and initial constructor remembered-set publication.
The explicit runtime regressions use a 1 KiB nursery and 4–16 KiB tenured arenas:

| Checked scenario | Minor collections | Major collections |
| --- | ---: | ---: |
| Three oversized fixed/byte/reference-array allocations with room | 0 | 0 |
| Two byte buffers requiring tenured reclamation | 0 | 1 |
| Dead nursery wrapper retaining the prior old buffer | 1 | 1 |
| Explicit stress with a nursery survivor and available old capacity | 1 | 0 |

Counters are asserted by `runtime::large_allocation_tests`; constructor-only
and later-card old-to-young edges also survive subsequent minor/major collection.
These are deterministic work-count results, not elapsed-time measurements.
Source and gate evidence: `target/gc-validation/2026-10-16-oversized-allocation/`.

## Idle aggregate access

The correctness-checked `aggregate-idle` workload measures 250,000 two-word
aggregate updates on one RUNNING mutator with an idle collector. Runtime calls
cross black-box function pointers; both payload words, released lock state,
exact live bytes and zero collections are checked after each sample. Nine
samples are retained, with the first two excluded from the median.

```sh
python3 scripts/benchmark_gc.py --output target/gc-bench/aggregate-idle.json \
  --cases aggregate-idle --workers 1 --trials 3
```

The runner's default remains the original three copy/allocation workloads.
Aggregate-idle requires one worker because it measures the uncontended path.
Three paired comparisons alternate process order and each use three separate
processes per variant. The control removes only the idle nonblocking acquisition
branch, retaining the original rooted blocking path. Source hashes confirm that
only `runtime/managed.rs` differs within each pair; the fast-path source was
restored and its hash matches the archived measured source.

| Pair | Always-blocking median ms | Idle fast-path median ms | Paired speedup |
| --- | ---: | ---: | ---: |
| 1 | 12.320 | 5.467 | 2.254× |
| 2 | 12.278 | 5.499 | 2.233× |
| 3 | 12.207 | 5.523 | 2.210× |

Median paired speedup: **2.233×**. One-minute load ranged **16.0–22.2 on 12 CPUs**;
these are consistent results under recorded contention, not quiet-host evidence.
The workload excludes active copying, contention, generated-expression overhead,
application latency and RSS. Those budgets remain open.

Raw samples, logs and source archives:
`target/gc-bench/2026-10-16-idle-aggregate-pair-{1,2,3}-{control,fast}.json`.
Analysis: `target/gc-bench/2026-10-16-idle-aggregate-analysis.json`.
The extended parser reproduced 54 retained old/new workload records and rejected
inconsistent medians and unsupported aggregate worker counts. After restoration,
the complete debug runtime suite passed 214 tests, zero failures, four ignores.

Host: Apple M2 Max, 12 logical CPUs, arm64. Compiler: rustc 1.96.0 (ac68faa20 2026-05-25).

Unless specified otherwise, three independent processes per workload/worker setting; tables report the median of their medians. Each copy process records nine GC events (two warmup); allocation records five samples per mode. All workloads verify exact object/live-byte counts. Allocation timings include barrier release through joins and retirement.

## Validated allocation requests

The previous TLAB path rechecked the same descriptor/length in nested wrappers. Heap-bound requests now validate canonical metadata and size once; typed object requests preserve the header match. Requests select the current arena at allocation time, so collection cannot leave a cached arena pointer stale. Large/stress allocations retain the shared allocator path. Input rejection, post-GC reuse, zeroing, header/count initialization and large objects have regression coverage.

| Mutators | Objects per sample | Before ms | After ms | Speedup |
| --- | --- | --- | --- | --- |
| 1 | 250,000 | 8.614 | 3.257 | 2.64× |
| 2 | 500,000 | 8.930 | 3.458 | 2.58× |
| 4 | 1,000,000 | 8.908 | 3.647 | 2.44× |
| 8 | 2,000,000 | 10.082 | 3.993 | 2.53× |

The allocation-only comparison is 2.44–2.64× faster locally. It preserves validation rather than accepting unchecked public descriptors. These are allocator measurements, not an application-wide speedup.

## Queue notifications

The work queue notifies on empty-to-nonempty publication or quiescence. Collector job failures now abort locally under the documented collection failure policy; they do not return to the queue. Workers waiting on the condition variable need no additional broadcast while work is already queued. A bounded chain/tree model checks complete, duplicate-free traversal and termination at 1/2/4/8 workers.

| GC workers | Small before ms | Small after ms | Payload before ms | Payload after ms |
| --- | --- | --- | --- | --- |
| 1 | 5.404 | 5.395 | 2.123 | 2.124 |
| 2 | 3.779 | 3.719 | 1.388 | 1.442 |
| 4 | 3.740 | 3.494 | 1.390 | 1.085 |
| 8 | 9.288 | 8.650 | 1.021 | 0.989 |

The four-worker small-object median improves by about 7%, and its payload median by about 22%. The two-worker payload median is about 4% slower in this sweep; not every result improves. Small differences need more controlled repetition. Eight-worker small-object copying remains about 2.5× slower than four workers, while eight workers help the large-payload case. This scaling bottleneck remains open; the default policy has not been capped to hide it.

## Reproduction and artifacts

The runner is [scripts/benchmark_gc.py](../scripts/benchmark_gc.py). Example:

```sh
python3 scripts/benchmark_gc.py --output target/gc-bench/repeat.json \
  --workers 1 2 4 8 --trials 3
```

Run after other builds/tests finish. The script refuses to overwrite an existing report. It retains raw samples, all logs, exact commands, compiler/build settings, source hashes and a runtime-source archive. Source drift or a failed workload marks the report failed. To reproduce an uncommitted source state, check out the recorded base revision and overlay the associated sources.tar.gz before running with the recorded compiler/settings.

- [inventory-baseline](../target/gc-bench/2026-10-16-inventory-baseline.json): source snapshot and logs in 2026-10-16-inventory-baseline-logs/.
- [allocation-request](../target/gc-bench/2026-10-16-allocation-request.json): source snapshot and logs in 2026-10-16-allocation-request-logs/.
- [queue-wakeup](../target/gc-bench/2026-10-16-queue-wakeup.json): source snapshot and logs in 2026-10-16-queue-wakeup-logs/.

These local microbenchmarks do not establish application startup, tail latency, peak RSS, heterogeneous workload scaling or a production performance budget. Those remain requirements in [GC_READINESS.md](GC_READINESS.md).

## Current correctness checkpoint: collection failure boundaries

A fresh isolated 36-run sweep after the read-barrier, custom-header and collection
failure repairs retains the same trial/worker/sample/count-check method. This is
a cumulative checkpoint comparison, not attribution to one change.

| Workers | Small prior ms | Small current ms | Payload prior ms | Payload current ms | TLAB prior ms | TLAB current ms |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 5.395 | 5.67 | 2.124 | 2.117 | 3.172 | 3.576 |
| 2 | 3.719 | 3.661 | 1.442 | 1.39 | 3.415 | 3.623 |
| 4 | 3.494 | 4.025 | 1.085 | 1.265 | 3.675 | 3.91 |
| 8 | 8.65 | 9.284 | 0.989 | 0.998 | 3.941 | 4.105 |

The four-worker small-object and payload medians are about 15% and 17% slower
than the earlier queue-wakeup snapshot. TLAB medians are about 4–13% slower than
that snapshot, while remaining about 2.28–2.46× faster than the original inventory
baseline. Eight-worker small-object scaling remains poor. The differences need
controlled paired investigation; these measurements do not establish that the
new correctness checkpoint preserves performance. Do not attribute them solely
to panic boundaries: multiple source changes separate these snapshots.

Artifact: [failure-boundary](../target/gc-bench/2026-10-16-failure-boundary.json),
with 36 successful runs, raw logs and `sources.tar.gz` in
`2026-10-16-failure-boundary-logs/`. Runner log:
`/tmp/gcr-failure-performance.log`. All tests/builds completed before this sweep.

## Adaptive queue batches and bounded streaming scans

A two-second sample of 1,000 correctness-checked small-object collections with
eight workers shows dominant mutex wait/drop stacks, including WorkQueue take
and finish. Evidence: `/tmp/gcr-queue-profile.txt`, workload log
`/tmp/gcr-queue-profile-run.log`. Profiling uses the ignored copy test with
`GCR_GC_PROFILE_COLLECTIONS=1000`; ordinary sweeps explicitly clear that variable
and retain nine events.

The queue now shares a scarce frontier among configured workers and takes up to
128 tasks as it grows. Large scans publish during traversal without retiring
their producer. Small fixed-field scan specialization avoids streaming checks
inside the common callback. Local claim/discovery/layout/address buffers are
bounded at 512 entries; global work/root storage still scales with the graph.
Contracts and functional coverage are in [SAFETY.md](SAFETY.md).

A native-byte decoder shift experiment showed no consistent gain across before,
candidate and restored controls; it was reverted. The expanded extraction
regression remains. Artifacts: `2026-10-16-decoder-{before,shift,after-control}.json`.
The intermediate streaming variants were measurement experiments, not separate
production claims; their source archives and logs are retained in target/gc-bench.

### Paired historical source comparison

Three pairs alternate fixed/adaptive order (A/B, B/A, A/B) and rotate worker
order. Each of the six sweeps has 12 successful workload processes. The control
differs only in taking a fixed maximum of 32 tasks; bounded streaming, capacities,
scan specialization and all correctness repairs remain identical. Tables show
the median of three process medians per variant. Every process checks exact
object/live-byte counts. Adaptive source hashes match the restored final source;
control archives differ by exactly the one batch-rule line.

**These are load-qualified workstation measurements.** Host context recorded
load averages roughly 16–28 on 12 logical CPUs and other active processes. Small
copy improved in every pair at 2/4/8 workers, but these data do not certify a
quiet-machine speedup, application budget or elimination of scaling regressions.

| Workers | Small fixed ms | Small adaptive ms | Ratio | Payload fixed ms | Payload adaptive ms | TLAB fixed ms | TLAB adaptive ms |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 5.86 | 5.535 | 1.06 | 2.11 | 2.204 | 3.816 | 3.584 |
| 2 | 3.917 | 3.039 | 1.29 | 1.445 | 1.362 | 3.81 | 3.917 |
| 4 | 3.706 | 3.178 | 1.17 | 1.215 | 1.032 | 3.926 | 3.958 |
| 8 | 9.933 | 4.476 | 2.22 | 0.965 | 0.894 | 5.611 | 4.348 |

One-worker payload copying is about 4% slower in this comparison. TLAB allocation
code did not change; its varying timings help show the background-load limits.
Eight-worker small-object copying remains slower than four workers. Quiet paired
confirmation, workload latency/RSS budgets and further scaling work remain open.

Artifacts: `target/gc-bench/2026-10-16-batch-pair-{1,2,3}-{fixed,adaptive}.json`,
with raw logs and source archives alongside each report. The runner now emits
schema 2 with load averages and top CPU process names (no argv) before/after each
child, outside timed workloads. Context-sampling failures are recorded explicitly.

Earlier 108-run fixed/current sweeps (`2026-10-16-streaming-fixed-control.json`
and `2026-10-16-streaming-final.json`) completed count checks but the latter became
visibly CPU-contended during another project's multi-core tests. They are
retained in full and are **not** used as a clean comparative performance claim.
The interleaved comparison retains host context instead of silently discarding
unfavorable timing samples.

### Scoped access performance remains unmeasured

The source snapshots above precede the scoped concurrent-access, exclusive copy
claim and physical-arena card-marking repairs. The new current-source collector baseline below covers the later
snapshot, while generated scoped-access overhead remains unmeasured. Quiet
comparison and explicit application performance budgets remain open.

## Current baseline and bounded local continuations

The current source baseline completed 36 correctness-checked processes at 1/2/4/8 workers. One-minute load was 3.74–4.49 on 12 CPUs, with recorded CPU-heavy background work. Artifact: [current-access-baseline](../target/gc-bench/2026-10-16-current-access-baseline.json). This baseline includes the preceding access/publication fixes; it measures stopped-world copy and host allocation, not generated access overhead.

A fresh eight-worker profile still showed substantial queue mutex lock/unlock time: `/tmp/gcr-current-access-profile.txt`, with a checked 1,000-collection workload in `/tmp/gcr-current-profile-workload.log`. Combining finish and take under one lock did not produce a repeatable benefit and consistently slowed the two-worker payload case in three pairs. That implementation was reverted. Its 72 processes, raw results and source archives remain at `2026-10-16-handoff-pair-{1,2,3}-{control,handoff}.json`.

The retained implementation keeps a published-object continuation locally, bounded by 128 tasks and 32 KiB of copied work. Excess work is published; large-copy batches and BUSY slot retries go through the shared queue. The active producer remains counted until its local continuation drains. Copy claims finish publication before a continuation begins. No unused copying-space tail or worker-count cap is introduced. An initial half-frontier split fragmented small batches and was replaced with the bounded intact policy; its screen is retained as `2026-10-16-local-continuation-screen.json`.

Three interleaved control/candidate pairs completed 72 processes. Each pair differs only in `crates/gcrust-rt/src/gc/parallel.rs`; all six runtime-source archive hashes and all workload count/live-byte checks were verified. Artifacts: `target/gc-bench/2026-10-16-budget-continuation-pair-{1,2,3}-{control,continuation}.json`. Raw analysis: [paired-analysis](../target/gc-bench/2026-10-16-budget-continuation-analysis.json).

| Workload | Workers | Control median ms | Continuation median ms | Median paired control/new ratio | Pairs improved |
| --- | --- | --- | --- | --- | --- |
| Small graph | 1 | 6.068 | 5.697 | 1.050 | 2/3 |
| Small graph | 2 | 7.976 | 7.431 | 1.073 | 2/3 |
| Small graph | 4 | 4.972 | 4.595 | 1.298 | 3/3 |
| Small graph | 8 | 5.346 | 3.847 | 1.132 | 3/3 |
| Large payload | 1 | 2.503 | 2.288 | 1.053 | 2/3 |
| Large payload | 2 | 3.219 | 1.591 | 1.393 | 3/3 |
| Large payload | 4 | 1.965 | 2.141 | 1.169 | 2/3 |
| Large payload | 8 | 1.581 | 1.726 | 0.972 | 1/3 |

Ratios are computed within each pair and then summarized; they are not the quotient of the two across-pair medians. Small-object copying improved in all three pairs at four and eight workers; median paired ratios were 1.298 and 1.132. One-/two-worker small-object and large-payload results were mixed. The payload median across processes was slower at four/eight workers, with inconsistent pair directions.

During this comparison, background load rose to 42.10–74.91 on 12 CPUs. Unchanged allocation controls also moved substantially (including multi-fold swings), so these data do not establish a quiet-host, application-wide or regression-free performance result. Quiet paired confirmation, generated access overhead, latency/RSS budgets and eight-worker small-object scaling remain open. The bounded continuation is retained as a correctness-checked optimization candidate with this measured limitation.

Correctness gates for the retained source: 209 debug runtime unit tests; 209 optimized runtime unit tests including explicit detector; nine compile-fail doctests; eight selected optimized integration tests. ThreadSanitizer passed all 11 parallel-module and 32 collector test executions (43 executions, 40 distinct names), without reports. The chain/tree/star model uses the production continuation at 1/2/4/8 workers and verifies exact single visitation and quiescence. Source hash `8e0d3a538363c0d808e21a59f335136f27503fd63aeb17705a150d264da2a25e` matches the restored final `parallel.rs` after all experiments. Gate results: `/tmp/gcr-budget-continuation-gates.json`; logs `/tmp/gcr-budget-continuation-*.log`. The full workspace/native sanitizer gates were not rerun for this queue-only change.

## Bounded string-copy checkpoint (2026-10-17)

Concat/join native staging now checks each result extent against the largest
managed arena before growing its buffer. This rejects impossible repeated-alias
outputs with a controlled diagnostic. It does not guarantee allocation under a
live-set constraint or impose an aggregate native-memory budget.

Copies poll in chunks of at most 32 KiB. Array/separator roots survive polls;
each selected array element is independently rooted before a possible park,
preserving the observed value if another writer replaces its slot. No managed
source slice spans a poll. Large native Vec growth runs in a blocking region
with only native storage borrowed. Large managed output copying roots its
private, initialized partial result and reloads it after each poll, acquiring
object access for each write. Partial UTF-8 sequences remain private until
construction finishes; the collector treats this payload as opaque bytes.

Focused verifier/poison regressions and four negative controls cover preparation
progress before managed allocation, moved source/output pointers, slot
replacement, and output extent arithmetic. Broad checkpoint validation passed (811 debug/optimized including detector,
52 s390x checks, sanitizer 38 native/170 runtime); evidence is in target/gc-validation/2026-10-17-bounded-string-copy/.
Managed allocation zeroing and OS allocation are not bounded by the copy chunk,
so this is no hard real-time guarantee. Other native loops, total producer
budgets, metadata/stacks, embedding and platform audits remain open.

## Polling string scans (2026-10-17, verified checkpoint)

The initial equality/hash polling design made every caller check relocation.
Seven alternating native compiler/runtime pairs measured small-key hash/equality
about 14–19% slower; this design was rejected. Retained records are under
`target/gc-validation/2026-10-17-string-scans/rejected-unguarded/`.
The refined compiler guards the String byte count, sends bounded small scans
to legacy noncollecting symbols, and uses versioned polling symbols with caller
root reloads only for large scans. This also preserves old compiled callers'
noncollecting ABI. Final paired time ratios for small hash/equal/mismatch: 1.027/1.042/1.034;
large: 0.998/1.018/0.963. CSV one/four workers: 0.996/1.037. All 84 timed
scan processes and 28 CSV processes match independent expected outputs.
This compares complete rebuilt compiler/runtime pairs against the archived
previous source, with seven alternating pairs per configuration and excluded
warmup. The residual small-key cost is retained, not described as a speedup.

Gates passed: 819 debug/optimized including detector; runtime 325 units + nine
doctests; 20 targeted integrations; five poison regressions; four fault
controls; actual s390x 56 selected checks; sanitizer 40 native/175 runtime
executions without reports. All 383 hashes match; 423 current files archived.
The new generated fixture executes under the sanitizer, stress off/on. Measured
compiler/runtime source hashes still match. Work per scan poll is bounded;
legacy compatibility behavior, other native loops, resource/admission budgets
and general application/RSS/pause/platform guarantees remain outside this
checkpoint.

## Native String preparation (2026-10-17, verified checkpoint)

A shared rooted range copier now serves substring, printing and host paths/text.
Large validation holds only native bytes and runs BLOCKED; small selected
substring work remains proportional to output size. Paired native benchmarks
cover nine-byte and 144-KiB ASCII/Unicode slices at a nonzero source offset,
complete-output FNV/checksum references, existing constructor controls and the
quoted-Unicode CSV workload. The same saved compiler generates both runtime
variants; its codegen input hash is checked against the previous checkpoint.
Fresh distinct runtime archives and all copied runtime-source hashes are checked.

An initial implementation measured small substring time ratios 1.033/1.041,
large 0.995/0.995, and CSV one/four workers 1.003/0.992. Assembly retained an
extra copy_range call on the small path; a normal inline hint for that shared
helper was tested. Prior source/artifacts and raw samples remain in
target/gc-validation/2026-10-17-owned-string-inputs/before-inline/.
The inline hint left the helper call in place; no causal gain is attributed to
it. The final outlined large path preserves an explicit native ownership/work
boundary. Seven alternating pairs per case: small ASCII/Unicode substring time
ratios 1.036/1.023; large 1.001/1.004; constructor controls 1.015/1.012/1.000/0.998
for ASCII concat/join and Unicode concat/join. CSV one/four workers: 0.953/1.011.
All 56 substring, 56 construction and 28 CSV timed outputs match independent
models; CSV managed allocation counts/bytes are identical per configuration.
Ranges, previous variants, source snapshots, library/binary/assembly hashes and
all samples are retained. The small selected-copy cost remains about 2–4%.

Gates passed: 825 debug/optimized including detector; runtime 331 units + nine
doctests; 20 targeted integrations; six poison regressions; five causal controls;
actual s390x 60 checks; sanitizer 40 native/181 runtime executions, no reports.
All 385 hashes match; 425 current files archived. Measured runtime sources match
final implementation. Production ARM64 assembly retains the BLOCKED transition
before large validation. Bounded copy work is not a hard real-time guarantee or
a complete native-memory admission policy.


## Channel sibling parking: native handoff scope, 2026-10-19

The correctness repair extends native condition-variable parking to sibling
registrations. One fixed AOT compiler links identical ordered handoff programs
against the previous verified FFI runtime and the repaired runtime. Explicit
benchmark-only C affinity calls verify each thread's assigned physical core.
Both archives/compiler are hashed against their validated build artifacts; all
runs check message order and sum and report zero major/minor collections.

Six reversed-order pairs of 200,000 messages per placement/capacity give paired
candidate/prior elapsed geomeans:

| Placement | Capacity | Elapsed ratio | Scope |
| --- | ---: | ---: | --- |
| Same core | 1 | 1.0068 | About +0.7% in these samples |
| Same core | 64 | 1.0052 | About +0.5% in these samples |
| Separate cores | 64 | 0.9960 | About -0.4% in these samples |
| Separate cores | 1 | Inconclusive | Individual ratios 0.686–1.334 |

A longer separate-core capacity-one confirmation uses 1,000,000 messages and
eight reversed-order pairs. Its geomean is 1.0269; a 20,000-resample paired
percentile bootstrap (seed 0) gives 95% interval 0.9944–1.0587. This leaves the
cost uncertain and does not establish a general speed or resource budget.

`target/gc-validation/2026-10-19-provenance/channel-siblings-validation/`
retains `handoff-benchmark.json`/`.tar.gz` for per-thread pinning,
`handoff-confirmation.json`/`.tar.gz` plus the bootstrap summary, and the initial
process-affinity-only run as `handoff-initial-process-affinity.*`. The latter is
not used to claim a regression or improvement. Raw GC, wall/CPU/RSS and load
samples and benchmark sources/binaries are included. A first affinity fixture's
unsupported extern-block syntax failure is also retained; corrected fixtures
use the language's supported extern-function declaration syntax.
