# GC production readiness and performance

## Current main — three compiler fixes adopted, 2026-10-20

The requested stack-handling, backward argument-inference and callable-expression
repairs are on main. Valid nested programs lower on 2 MiB Rust worker stacks.
Ordinary successful generic calls use no speculative argument lowering; backward
constraints retry only after discarding a failed function attempt. Function values
can be called directly, including immediate closures, factories, fields, indexes,
blocks and conditionals. Callee evaluation precedes left-to-right arguments, and
ANF keeps captured references rooted across allocating arguments. Invalid calls
and lexical shadowing produce source diagnostics.

The frozen combined source has 564 inputs. Full Mac debug passes 1,263 tests
(five ignored); release passes 1,262 (six ignored). Full Linux debug passes
1,258 (five ignored), release 1,257 (six ignored). Both hosts' documentation
profiles pass nine tests with five ignored each. Linux explicit concurrent JIT
checks pass in both profiles, and its forced release detector passes. The exact
post-adoption main source guard passes; runtime sources are unchanged. Canonical
main debug/release binaries and runtime archives rebuilt successfully; the main
2 MiB regression and both profiles’ callable JIT/native integration checks pass.
Artifact hashes and the post-build source guard are retained. Evidence: `callable-expressions-validation/` under
`target/gc-validation/2026-10-19-provenance/`.

Nine paired Linux release comparisons of the lazy inference checkpoint measured
lowering ratios 0.873 / 0.891 for flat literal/local calls and 0.926 / 0.906 / 0.842
at nesting depths 8/16/32 versus the pre-stack main checkpoint. Parsing/resolution
is excluded and IR destruction included. These are compiler component results,
not application runtime or production SLO guarantees. The eager memoized variant
was rejected (2.19x for local calls and 43.87x at depth 32). The final combined
comparison passes six workloads: median ratios 0.923 / 0.947 / 0.953 for literal,
local and named-closure calls, and 0.950 / 0.945 / 0.858 for depths 8/16/32.
Raw paired samples and commands are in `callable-expressions-validation/`.

Language behavior is documented in [language.md](language.md#callable-expressions-and-argument-inference).
Rust AST consumers should read [core-ir.md](core-ir.md#surface-ast-ownership-for-rust-consumers)
for the `Box<ExprKind>` to immutable `Arc<ExprKind>` migration and copy-on-write
editing. Serialized AST JSON retains its shape.

Worker failure outcomes, resource cleanup and limits, shutdown guarantees and
remaining release requirements are still open. The sections below retain earlier
checkpoint evidence; their test counts describe those checkpoints.

## Language compiler repairs — adopted and validated, 2026-10-20

Main now implements transparent aliases with lexical name resolution, generic
substitution, nominal identity and iterative cycle diagnostics. Trait bounds
are checked in explicit alias uses, inferred constructors, associated calls and
patterns. Inference preserves unresolved result shapes and defers only proofs
that involve registered placeholders. Ground uses still check every bound;
proofs under generic assumptions remain scoped. Only ground proofs without
assumptions enter the bounded shared cache.

The adopted source also repairs unannotated Vec construction, HashMap inference
blocked by premature bound checks, precise trait/empty-array diagnostics, and
lookup that skipped nearer type declarations or generic binders when selecting
a trait. Focused checks pass 273 compiler units, 25 API tests and 177 alias CLI
scenarios in both profiles. The frozen combined candidate passed full workspace
debug (1,246 passed, five ignored) and release (1,245 passed, six ignored), with
post-run source guards. All eight real application tests and 42 example program
tests pass within those runs.

The alias checkpoint source was adopted exactly. Latest main, including the
declaration repair below, passes both complete workspace profiles. Candidate evidence is in
`target/gc-validation/2026-10-19-provenance/type-alias-combined-shadowing-validation/`;
main adoption and source evidence are in `type-alias-main-adoption-validation/`.
Main runtime inputs are unchanged.

Early SROA/InstCombine before the complete O3 pipeline repairs quadratic LLVM
IPSCCP memory growth on a 4,096-field value. An isolated JIT peak fell from
roughly 15 GB to 377 MB while preserving output. Optimized IR is verified. The
regression enforces 1 GiB RSS and the existing 30-second compiler deadline.
The earlier main-only memory-fix checkpoint passed 200 compiler units and the
wide-value regression in debug/release, plus 32 Python helper checks; evidence
is in `wide-pipeline-main-validation/`. The source collector now includes LLDB
tools and detects changes to their archived bytes.

The duplicate-field/variant/parameter declaration repair is applied to main.
It passes 275 compiler units, 25 API tests and three CLI diagnostic tests in
both profiles. Main full release passes 1,248 tests with six ignored; full debug
passes 1,249 with five ignored. Both post-run source guards pass. Evidence is in `duplicate-declarations-main-validation/`. CLI
diagnostics cover 30 malformed-source invocations and preserve existing output.

Earlier paired release CLI timings for the alias-bound checkpoint had median
after/before ratios 1.0149, 1.0250 and 1.0230 for 1, 32 and 128 alias/function
pairs. All paired ranges include regressions and 1; these measurements establish
neither neutrality nor application SLOs and predate the final inference repairs.
The production-readiness goal remains open. Current main checks, remaining
runtime accounting, cross-platform validation and application performance gates
are not claimed complete.

## Accounted thread registration owners — adopted, 2026-10-20

A separate runtime candidate replaces unaccounted `Arc<ThreadState>` backing
with stable allocator-accounted strong/weak registration handles. It includes
main's current compiler fixes. The record stays charged through weak-only
ownership, while the last strong owner destroys the payload and its buffers.
The 16 runtime paths are adopted into main; all compiler, runtime and tooling
inputs match the frozen validated candidate exactly.

The frozen 562-input candidate passes full Linux debug (1,249 passed / five
ignored), release (1,248 / six), both documentation profiles (nine / five each),
two explicit concurrent JIT checks and one release detector check. Linux
ThreadSanitizer passes 1,314 checks; Miri passes 42 checks across three seeds.
The public Rust ownership/migration example also compiles and executes on Mac.
The generated Linux sanitizer corpus passes all 50 fixture/application runs,
with source guards before and after. Both Mac documentation profiles pass
(nine tests, five ignored each). Fresh full Mac workspace checks pass: debug
1,254 / five ignored; release 1,253 / six. The public embedding API migration is documented in
[threads.md](threads.md#native-registration-handles). Canonical main debug/release binaries and runtime archives rebuild successfully;
main runtime tests pass 657 / five ignored in debug and 656 / six in release.
The post-adoption 562-input source guard passes.

Nine paired Linux release measurements give candidate/baseline median ratios
of 1.0246 for strong clone/drop, 0.9997 for weak upgrade/drop and 1.0641 for
registration/retirement. Registration overhead remains open. No application
performance guarantee or complete native-memory accounting is established;
allocator overhead and opaque platform synchronization backing remain outside
these requested-allocation measurements.

One Mac run was discarded after shared Cargo outputs and zero archive timestamps
reused another candidate's compiler test binary. The replacement run verifies
all source bytes/modes, refreshes input timestamps and clears the affected
package outputs. Its compiler binary executes the expected 275 unit tests.
Evidence: `thread-record-inline-validation/`, `thread-record-inline-benchmark/`
and `thread-record-inline-workspace-*-fresh-tests.log`, under
`target/gc-validation/2026-10-19-provenance/`.

## Recursive lowering stack — historical candidate evidence, adopted above

Canonical main aborts while lowering parser-accepted programs on a normal
2 MiB Rust worker stack: a plain generic-call chain at depth 48 or a
block-wrapped chain at depth 24. Parsing and name resolution complete first.
A separate four-file candidate checks the recursive expression boundary before
entering its large dispatch frame and grows the stack on the same thread only
when needed. It preserves the parser's source nesting limit; runtime crate
sources are unchanged. The compiler adds stacker 0.1.25 / psm 0.1.32.

The explicit 2 MiB regression passes on Mac for checking and lowering, including
invalid-type errors. Linux debug and release compiler suites each pass all 276
unit tests, with frozen source guards. Full Mac workspace checks are running;
several native executables encountered independent pre-main launch delays.
Their samples show `_dyld_start + 0`, 96 KiB footprints and no application
execution. The repair is now adopted as recorded above. Historical evidence: `lowering-stack-validation/`
and `lowering-stack-*-tests.log` in the provenance directory above.

## Argument inference — rejected eager candidate and adopted lazy successor

The first candidate allows later function arguments to constrain earlier empty
arrays, generic constructors and record literals, but repeats speculative
lowering exponentially: 15 probe requests at depth four, 255 at eight and 4,095
at twelve. That candidate is not adopted.

A separate memoized version retains immutable shared expression nodes and
compares the complete local, layout and monomorphizer context before reusing a
probe result. Both successes and failures are scoped to equal contexts. Real
checking and emission remain in source order. Type-only probes cannot skip
errors tolerated by outer let-discovery. Copy-on-write qualification keeps
shared syntax independent when modified.

This prototype passes all 279 compiler units on Mac and both Linux profiles.
Linux API, alias and CLI diagnostic integrations pass 30 tests in each profile,
including 188 alias CLI invocations per profile. The scaling matrix counts all
expression lowering and covers plain, block, arithmetic, right-nested and named
closure calls, including invalid programs. At depth 24 the plain valid chain
uses 1,153 expression lowerings; 12 nested named closures use 759. These are
polynomial results for the tested matrix, not a universal complexity proof.
Full-state snapshot costs and whole-workspace composition remain to be measured
before adoption. Source and logs: `function-argument-memo-validation/` and
`function-argument-memo-*-tests.log` in the same provenance directory.

Immediate closure invocation and other function-valued expressions are now
implemented and validated in the combined main checkpoint above.

## Array-length operand types — fixed, 2026-10-20

The original compiler accepts array_len on scalar, string, record, unit and
invalid generic operands during check, then passes the wrong representation
toward LLVM. Lowering now requires a runtime array and reports a source error.
A failing original regression and six original CLI acceptance observations are
preserved. All six categories now fail normally in check, native-build and JIT
entry points, without creating native output or producing a Rust panic.
Supported dynamic/fixed/generic/unit/reference arrays and user function
shadowing remain covered by execution checks.

Debug and release workspace all-target checks are warning-free; each profile
passes 200 compiler unit checks and 43 diagnostic/type/FFI/metadata/unit-value
integrations. Canonical compiler binaries are rebuilt. The compiler's now-
exhaustive IR match replaces an unreachable fallback; unused locals are removed.
Runtime sources/manifests and both profiles' exact rlib/static archives are
unchanged from the fully gated compact ABI runtime below. Current manifest is
553 inputs with four compiler/test paths changed after that runtime checkpoint.

At this earlier array-length checkpoint, parsed type aliases were a separate
language gap: array, scalar and signature alias examples were rejected by the
original compiler too. The later adopted compiler repairs described above close
that gap, including transparent alias normalization, generic substitution,
nominal identity and cycle diagnostics. The array-length evidence below records
the earlier checkpoint rather than the current alias support.
Evidence: `target/gc-validation/2026-10-19-provenance/array-len-type-validation/`
and `array-len-type-audit/`.

## Stable ABI backing — applied and verified, 2026-10-20

The original ordinary runtime misses the 24-byte allocation window and two/three
8-byte arena cursors (40/48 bytes per Heap), plus 120 bytes of runtime Thread
ABI storage while its context remains alive. Sole raw owners now retain their
allocator inside actual nonzero backing allocations. Exported payload pointers
remain valid across owner moves. An independent guard physically frees backing
and releases its charge after payload destruction, including unwind. Zero-sized
values allocate no control and execute their destructor exactly once, including
the tested unwind case.

Native Heap/arena layouts stay unchanged: 4,784/128 bytes on macOS and
3,344/120 on Linux; public Thread ABI stays 120 bytes. Physical allocation and
reallocation counts are unchanged. Actual owner headers add 24/32 requested
bytes per Heap and 32/40 per complete runtime context; those extra bytes are
also charged and released. Thread backing ends at context destruction, while
held Heap backing stays charged until final release. The first allocator-beside-
pointer variant and its larger native layouts remain in comparison evidence.

The exact 553-input candidate passes 1,303 verified/poisoned Mac runtime checks,
15 strict Miri runs, 154 integrations, 72 mutation cases / 18,432 cycles and
96 normal-library controls. Linux passes 2,628 native checks, 1,304 Rust/std TSan
checks, 2,350 compiler/workspace/JIT checks, 1,052 selected emulated s390x checks
and 96 normal-library controls. Fresh main passes another 154 integrations and
96 ownership controls exactly matching the candidate. Canonical debug/release
artifacts are rebuilt; 240 ordinary caller/stripe/metadata/layout/ABI controls
and strict same-pin metadata/code/data/relocation/normalized-symbol comparisons
against a fresh reference pass. These counts overlap candidate coverage.

Three-build timing compares seven rounds per case in 168 Mac processes. At
1,024 objects, compact/baseline warm-major ratios remain 1.0229
[1.0061, 1.0453] for semispace and 1.0147 [1.0093, 1.0332] for generational.
The first unpinned Linux comparison has large correlated setup/collection shifts
and remains preserved. A further 168-process comparison pins every child to the
same initially allowed logical CPU 1, without changing host policies, retaining
frequency, pressure and child scheduling observations. Large generational
Heap/context constructor ratios are 1.0389 [1.0306, 1.0524] / 1.0204
[1.0069, 1.0299], warm major 0.9984 [0.9978, 0.9999]. Selection preserves
compact native layouts and fully owned backing; it does not establish a
performance win in every case, application SLOs, or RSS/service budgets.
Evidence: `target/gc-validation/2026-10-19-provenance/abi-storage-inline-trial/`
and `abi-storage-validation/`.

## Layout-table ownership — applied and verified, 2026-10-20

A normal-library regression finds 764 retained bytes of descriptor-vector and
owned interior-pointer-map capacity missing from the native registry ledger.
An owned-map append into spare descriptor capacity adds another 22 untracked
bytes. The original assertion fails against preserved Mac and Linux libraries.

The selected, separate 553-input candidate validates descriptors and inventories
owned capacity in one pass before Heap backing is created. It preserves std Vec
allocation/reallocation, adopts backing without copying, and charges actual
capacity. Borrowed static maps add no charge. Private shared-slice access keeps
ownership fixed; exclusive append transfers a pending charge only after insertion.
The descriptor graph physically drops before its charge owner releases. Public
TypeInfo is unchanged and no production unsafe code is added.

Mac gates pass 1,297 verified/poisoned runtime checks, 12 strict Miri runs,
154 integrations, 72 mutation cases / 18,432 cycles and 24 ordinary-library
controls. The controls preserve pointer identity, physical allocation/realloc
traffic, stable IDs, and complete physical release. Heap grows from 4,768 to
4,784 bytes. Native Linux passes 2,616 checks and Rust/std TSan passes 1,298 without reports.
All 24 Linux normal-library ownership controls pass; native Heap grows from
3,328 to 3,344 bytes. Linux compiler/workspace/JIT passes 2,344 checks and selected emulated s390x
passes 1,020. The exact 553-input candidate is applied. Fresh main passes another
154 integrations and 24 ordinary controls matching the candidate. Canonical
debug/release artifacts are rebuilt; 144 ordinary caller/array/metadata/layout
controls and strict same-pin Rust metadata, code/data, relocation and
normalized-symbol comparisons against a fresh reference pass. Totals overlap
candidate coverage.

Seven rounds per descriptor count compare three normal release builds in 105
processes. At 65,536 descriptors, selected constructor ratio is 1.0057
[0.9971, 1.0085], warm major collection 1.0000 [0.9872, 1.0046], and streaming
registration 1.2321 [1.2158, 1.2845]. Registration has a material measured cost.
The original native-vector attempt's extra overlapping growth peak and larger
constructor cost are preserved in its evidence; the selected std Vec design
preserves baseline physical growth. Empty-registration ratios are undefined
when clock samples are zero; absolute observations are retained. These fixtures
do not establish application SLOs, RSS limits, or resource budgets.
Linux paired timing adds a material large-constructor cost: ratio 1.2005
[1.1226, 1.2240] at 65,536 descriptors, with streaming registration 1.3351
[1.1966, 1.5300]. This remains performance follow-up work; the Mac constructor
result does not establish cross-platform neutrality. Seventy Linux processes
ran after the known project gates, with host load retained and affinity
uncontrolled.
Evidence: `target/gc-validation/2026-10-19-provenance/layout-table-hot-trial/`.

## Installed reflection tables — applied and verified, 2026-10-20

Installed type/value reflection metadata and allocation-site tables retain
ordinary vectors, strings, variant/field arrays and projected-reference maps.
Original-source regressions miss 6,636 retained bytes and fail to include a
retained 2 MiB name in native pressure. A staged immutable owner charges actual
capacities without copying backing or changing public descriptor types. Only
shared slices escape; the charge releases after complete physical destruction.
Rejected replacement and concurrent publication retain the winner and free the
loser, preserving existing borrowed descriptors.

The frozen 550-input candidate passes 1,281 verified/poisoned Mac runtime checks,
nine strict Miri cases, 154 generated/reflection/allocation-profile integrations,
72 mutation cases / 18,432 cycles and 24 ordinary release-library controls.
At 1,024 fixtures it includes 6,795,264 previously missed bytes; physical
construction, adoption, rejected replacement and release traffic matches the
baseline. Native Heap size grows from 4,704 to 4,752 bytes.

Seven alternating paired process samples measure the installation traversal
separately from construction and destruction: median installation is about
58 ns for empty tables, 80 ns for one fixture, 823 ns for 32 and 29.5 us for
1,024, versus about 50/49/53/162 ns before accounting. Large-case destruction
ratio is 0.9952 [0.9741, 1.0172]; immutable lookup ratio is 1.0127
[0.9986, 1.0632]. These are finite shared-host fixtures, not application startup,
RSS or latency budgets. Linux native validation passes 2,584 checks after a
preserved disk-full interruption and bounded cleanup of completed build artifacts.
Rust/std ThreadSanitizer passes 1,282 checks without reports; full Linux
compiler/workspace/JIT passes 2,328 checks and selected emulated s390x passes
988. The exact source is applied. Fresh main builds pass another 154 generated
integrations and 24 physical allocation/retention controls matching the candidate.
Standard debug/release artifacts are rebuilt; 72 caller controls, 24 metadata
ownership controls and strict same-toolchain metadata, code/data, relocation
and normalized-symbol comparisons against a fresh reference pass. These totals
overlap candidate coverage.

Native search application byte-identical index and complete UTF-8 query oracles
pass with both runtime archives across default and pressure corpora (129/513
documents). The pressure requests retain two/six minor collections for
index/query. Candidate accounting includes 19,315 retained descriptor bytes.
Thirty alternating pairs per operation/corpus total 240 paired process requests;
raw timing/RSS observations remain separate from service guarantees. Application
binary size grows by 21,504 bytes.
Evidence: `target/gc-validation/2026-10-19-provenance/installed-metadata-trial/`.

## Collector access-lock array — applied and verified, 2026-10-20

The actual lock-array constructor requests 4,096 bytes on macOS ARM64, outside
its Heap ledger. Preparing the locks separately requests 16,384 bytes of opaque
platform mutex controls; owner drop releases both domains. A separate 551-input
candidate gives the boxed array its owning metadata allocator and uses address
bits for stripe hashing without exposing pointer provenance. Platform controls
remain separately open.

The selected candidate passes 644 debug and 643 release verified/poisoned Mac
runtime checks, nine strict Miri cases, 154 generated integrations, 72 mutation
cases / 18,432 cycles, and 24 ordinary release-library controls. Actual constructor,
three concurrent major cycles, and final release have matching allocation,
reallocation and deallocation counts and requested/released bytes. Live and peak
ledger gain exactly the array layout; native Heap size grows 4,752 to 4,768 bytes.
A new ownership assertion linked against the original runtime fails at zero
versus 4,096 bytes.

The initial broader run exposed ten exact retained-total expectations missing
the new array and one subsequent poisoned-fixture failure. The selected tests
include the actual owned array extent; zero-after-final-release checks remain.
The original failing log is preserved. Seven paired timing samples per case
retain all uncertainty: every displayed confidence interval includes one, with
32-object warm major point ratio 1.0655 [0.9343, 1.1085]. These finite samples
establish no application budget or general neutrality. Linux validation passes
2,596 native checks, 1,288 Rust/std ThreadSanitizer checks, 2,334 compiler/workspace
checks and 1,000 selected emulated s390x checks. The exact source is applied.
Fresh main builds pass another 154 integrations and 24 ordinary ownership
controls. Standard debug/release artifacts are rebuilt; 120 ordinary controls
and strict same-toolchain metadata, code/data, relocation and normalized-symbol
comparisons against a fresh reference pass. Counts overlap candidate coverage.
Evidence: `target/gc-validation/2026-10-19-provenance/object-lock-storage-accounted-trial/`.

## FIFO collector admission and thread scratch — applied and verified, 2026-10-20

The 548-input candidate fixes coordinator admission starvation and accounts
OS-thread-owned collector scratch. Captured Linux stacks show both allocating
writers waiting for nursery collection while repeated major cycles and reader
snapshots continued. FIFO tickets prevent later requests overtaking queued
allocators. The unchanged generated workload passes all 32 modes across a
serial run and three concurrent copies, retaining its 60-second child deadline.

Scratch vectors charge actual requested backing to a static process-wide ledger,
including nested and TLS-teardown fallback. They survive heap drop and release
at thread exit. Metrics distinguish process scratch from per-heap native demand.
An ordering fault control reports a Miri data race when the coordinator's
release/acquire is removed; the tightened test excludes alternate wait-mutex and
thread-completion publication paths.

The stronger source passes 1,271 Mac runtime checks with verification/poisoning,
21 strict Miri checks, 102 generated integrations, and 72 mutation cases /
18,432 cycles. Thirty-two ordinary thread-lifetime controls track 24,576 retained
scratch bytes, warm reuse and exact thread-exit release. Seventy-two collection
caller controls match baseline allocation counts, bytes, requested peak and
retention. Heap size grows from 4,672 to 4,704 bytes; alignment stays 16 and arena
size stays 128. This is a native Heap layout change, with fresh artifacts required.

Seven paired steady samples retain a 1.06068 shared-owner collection ratio
[1.05764, 1.07126] and 1.01672 batch ratio [1.01467, 1.02202]. The distinct-owner
1,024-channel collection ratio is 0.95770 [0.94781, 0.96384]. Other fixtures and
first-use timing are retained separately. Host load and affinity are uncontrolled;
these do not establish application latency/RSS budgets or general neutrality.

Linux validation passes 2,564 native runtime/detector/doc checks, 1,272 Rust/std
ThreadSanitizer checks and 2,318 compiler/workspace checks. Selected emulated
s390x big-endian execution passes 968 checks under QEMU on the existing Linux
host. This establishes compiled big-endian behavior, not native s390x hardware
or performance evidence. The exact 548-input repair is applied. Fresh main
builds pass 102 integrations, 32 thread-lifetime controls and 72 allocation/peak
controls. Standard debug/release artifacts are rebuilt; another 72 controls and
strict same-toolchain metadata, code/data, relocation and normalized-symbol
comparisons against a fresh reference pass. Other native control/ownership domains and the full access/embedding,
release/platform and resource-budget audits remain open. Evidence:
`target/gc-validation/2026-10-19-provenance/coordinator-fair-handoff-trial/`.

## Channel tracing scratch — applied and verified, 2026-10-19

Channel-owner snapshots, grouped channel references, type flags and owner lookup
tables charge their actual backing through the heap's cached metadata allocator.
Borrowed allocator ownership avoids new counter-owner clones; empty plans allocate
nothing. Snapshot backing is released before the source lookup is allocated.
Managed-header classification and channel liveness rules are preserved.

The original-source negative control misses 116 retained bytes for one source
owner. The candidate passes 38 focused channel checks, 628 debug and 627 release
runtime checks, and 12 strict Miri cases covering real storage, grouping, release
ordering and constructor unwind. Ordinary controls at 1,024 distinct owners
recover 67,600 bytes of missing peak accounting with unchanged physical requested
peak. A separate 40-process small-request audit reproduces the one-channel
variation in both builds: non-caller-thread 21-byte allocations and 24-byte
releases exactly explain it. Caller small-request histograms match; the exact
background thread and call stack remain unidentified. A separate 72-process
caller-only control exactly matches allocation/reallocation counts, requested
bytes, peak and retained bytes in every baseline/candidate case. Original
whole-process measurements remain preserved with their background variation.

Seven steady paired measurements show collection ratios of 0.9962, 0.9781,
1.0021, 0.9766 and 0.8825 for zero, one, 32 distinct, 1,024 distinct and 1,024
shared-owner channels. The first three confidence intervals include one; the
last two lie below one. These are finite shared-host fixtures, not application
latency guarantees. Wider generated-program, mutation and Linux native/sanitizer
validation has passed locally (102 generated checks) and on native Linux
(2,532 runtime/detector/doc checks and 1,256 Rust/std ThreadSanitizer checks
without reports). The 72-case / 18,432-cycle mutation soak has passed. Linux
compiler/JIT validation passed 2,302 checks. The exact 546-input candidate is
applied to main. Fresh main artifacts pass another 102 generated integrations
and 72 caller-only normal-library controls; these totals overlap candidate
coverage. Default debug/release artifacts are rebuilt; 72 normal controls and
strict same-toolchain Rust metadata, code/data, relocation and normalized-symbol
comparisons against a fresh reference pass. Evidence: `target/gc-validation/2026-10-19-provenance/channel-scratch-validation/verification.json`.

## TLAB reservation records — applied and verified, 2026-10-19

Heap-owned TLAB records now allocate their actual control/value storage through
MetadataAllocator. A private strong shared owner stores its allocator in the
allocation; the last owner destroys the value and releases backing, including
on destructor unwind. The final release uses an acquire load in every build,
which is also modeled by ThreadSanitizer. Public standalone arenas retain their
untracked allocator defaults.

Records still survive arena reset until their TLAB owner validates the native
epoch. Test inventories deduplicate arena and registered-thread owners, including
stale records. Exact ledger equality and zero after the final metadata owner are
required. Earlier incomplete inventories and their cascading failures are saved.
The external allocator-api2 boxed-destructor unwind leak is separately reproduced
and recorded in the project pad; this shared owner's deallocation guard handles
unwind independently.

Ordinary clients validate every object without GC at 0–64 independent TLABs.
The record is 40 bytes on the tested ARM64/x86-64 hosts: 64 records add 2,560
previously unaccounted bytes. Physical requested bytes, request counts, peak and
arena/Heap sizes (128/4,672 bytes) remain unchanged in these controls. The common
64-byte Mac first-lock control remains outside this repair; other Arc/control,
scratch and auxiliary ownership domains remain open.

The frozen and applied 545-input source passes 624 debug and 623 release runtime
checks, 102 generated integrations, 24 strict Miri checks, 72 mutation cases /
18,432 concurrent cycles, 2,516 native Linux checks, 1,248 Linux Rust/std sanitizer
checks without reports, and 2,294 Linux workspace/compiler/JIT checks. Relaxing
last-owner synchronization makes the exact Miri regression report a data race.
Fresh main-source artifacts independently pass another 102 generated integrations
and all normal ownership/peak/layout controls; these totals overlap. Canonical
Mac debug/release artifacts are rebuilt, with ordinary controls and strict
same-toolchain code/data, relocation, metadata and normalized-symbol comparisons.

Seven paired Mac allocation samples retain a first-allocation ratio of 1.24995
[1.15361, 1.29228], roughly a few tens of nanoseconds in this fixture. The 64-TLAB
cold ratio is 1.03861 [1.01590, 1.07610]. Sustained allocation ratios are
0.97030/0.97070. These finite measurements do not establish general neutrality.

Real optimized search CLI workloads compare complete serialized snapshots and
all UTF-8 query rows against independent references. Each operation uses 30
alternating request pairs after warming. At 129 documents / 453,888 bytes,
median indexing/query time is about 20/10 ms; those default runs perform no GC.
At 513 documents / 1,815,552 bytes with a 1 MiB nursery and 16 MiB tenured space,
index/query execute two/six minor collections and take about 30/27 ms in both
variants. Managed-heap peaks match. Separate maximum RSS differs by a few pages;
binary size grows 21,296 bytes. Empirical p95 and paired intervals are retained,
without a service guarantee, persistent-process plateau or production budget.

Evidence: `target/gc-validation/2026-10-19-provenance/tlab-control-validation/`,
`tlab-control-acquire-load-trial/`, and `search-app-record-control-summary.json`.
Earlier primitive, fence-based, inventory and deliberately relaxed fault snapshots
remain available. Overall production readiness remains open.

## TLAB registry ownership and fast allocation — applied and verified, 2026-10-19

Heap arenas now own the allocator for their TLAB registry vector backing. Empty
registries allocate no backing; reset retains and charges reusable capacity until
arena destruction. The public standalone arena constructor keeps its untracked
default. Reservation record/control allocations remain outside this repair.
Normal 64-mutator controls charge 512 previously unaccounted bytes. At 33
reservations, allocate/copy/free growth raises requested peak from 1,832 to 2,088
bytes while final retained storage stays 1,832. Every payload is validated without
GC in this probe.

The initial direct-vector candidate regressed sustained allocation. ARM64
inspection showed refill machinery expanding the allocation function and its
stack frame. Reservation is now cold and out of line; the two small fast helpers
permit normal inlining. Checks, initialization and epoch/publication rules remain.
Applied-library Mac sustained-allocation ratios are 0.90898 and 0.93109. Its
first-allocation ratio is 1.11855 [1.01551, 1.53792]; retain that noisy cold cost.
Other Mac cold intervals include parity. Linux-specific sustained ratios are
1.01306 and 1.02116, retaining small costs versus the unaccounted baseline, down
from roughly 9%. The Linux first-allocation cost of about 5.8% remains. These
finite fixtures do not establish application budgets or general neutrality.

Retirement inventories now include actual Heap-owned arena vector capacity;
exact ledger equality and zero after Heap destruction remain required. Failed
intermediate inventories and their cascading fixture failure are preserved.
The frozen 543-input source passes 1,229 local runtime checks, 102 generated
integrations, nine strict Miri checks, 72 mutation cases / 18,432 concurrent cycles,
2,480 native Linux checks and 1,230 Linux Rust/std sanitizer checks without reports.
The full local core retry also passes: 1,142 workspace debug checks, 1,141 workspace
release checks, the armed detector and Python tooling. These counts overlap the
targeted gates. Its initial 30-second timeout on an 8,190-field record is preserved;
the unchanged test passed alone and in both complete retry profiles. Contention
remains a hypothesis for the first timeout.

Canonical Mac runtime Rust metadata, every code/data section and relocation table,
and ordered symbols match the fresh build after normalizing only numeric LLVM
module suffixes. Ordinary backing/peak controls pass. Raw archive hashes differ;
the raw-hash assertion failure is preserved separately from the earlier genuine
stale-Cargo-artifact failure.

Evidence: `target/gc-validation/2026-10-19-provenance/tlab-registry-validation/`
and `tlab-registry-final-trial/`. Direct, cold, reviewed and inline intermediate
snapshots are retained. Development, ordinary tests and Miri run locally;
`computer.jimmyhmiller.com` supplies Linux-specific port validation.

## Promotion preflight scratch — applied and verified, 2026-10-19

Read-only promotion-capacity traversal and combined-pause nursery marking use
standard hash sets and pending vectors outside the native ledger. An isolated
repair gives the actual backing the Heap's cached borrowed metadata allocator.
The nursery-live set stays charged through its last major-collection consumer.
Conservative old-to-young preflight roots, cycles and deduplication are preserved.
Applied source matches the frozen 544-input candidate exactly.

The frozen 544-input candidate passes 619 debug and 618 release runtime checks,
102 generated integrations and nine strict Miri checks across three seeds.
An actual Heap regression verifies returned-set ownership against physical
allocation requests and checks all capacity exits. That identical regression
fails against the original implementation: zero charge for 4,616 retained bytes.

Ordinary release clients validate every payload and edge after one major and one
minor at 3, 17, 65, 257 and 1,024 young objects. All five keep physical requested
traffic, final bytes and requested peak unchanged. At 1,024 objects, the ledger
peak includes 18,440 previously unaccounted bytes: requested extra peak of 88,208
versus registry peak growth of 88,080 leaves 128 bytes in other control domains.
Final scratch charge returns to baseline. Vector growth changes reallocations
to allocate/copy/free; total request counts stay equal. Complete native accounting
remains open.

Seven uninstrumented alternating pairs give collection ratios of 0.98491, 0.96558
and 0.96569 for 3, 65 and 1,024 live-node fixtures. The largest interval includes
parity. Every fixture validates one major plus one minor and its surviving graph.
Timing began after the full core and candidate compiler checks finished; host
background load remains uncontrolled. These are finite fixtures, not application
budgets. The mutation soak passes 72 cases / 18,432 concurrent cycles. Linux gates pass
2,496 native runtime checks, 1,238 Rust/std sanitizer checks without reports, and
2,284 workspace/compiler/JIT checks. Fresh applied Mac artifacts pass another
102 generated integrations and the same ordinary ownership/peak control. The
complete source manifest binds candidate runtime, Miri, soak and platform results
to the applied source. Default Mac artifacts are rebuilt; ordinary controls and strict code/data,
relocation, Rust metadata and normalized-symbol comparisons pass against a fresh
reference using the same pinned toolchain. The earlier cross-sysroot comparison
failed on standard-library source-path metadata and is preserved separately.

Evidence: `target/gc-validation/2026-10-19-provenance/promotion-scratch-trial/`
and `promotion-scratch-validation/verification.json`.

## Retained object/card indexes — applied and verified, 2026-10-19

Each generational Heap owns two incremental card-overlap indexes. Their range
history and per-card starts previously used standard vectors outside the native
ledger. The isolated repair gives both reusable buffers owned registry allocators;
clear/reset retains the physical charge and destruction releases it. The indexing
algorithm and lazy clean-card path stay unchanged.

The final candidate passes 725 Linux GC checks, 725 macOS GC checks and nine
strict Miri checks (three metadata owner/backing/reuse cases across three seeds).
Existing arena/TLAB models cover incremental geometry and reset natively.
An identical first-dirty normal client validates 4096 old parents and their young
leaf after one minor/no major. Linux enrollment extra 75344 now equals its ledger
delta; the retained 1600-byte index remains charged after the temporary remembered
work drops. Before the repair, the same physical requests left 1600 unaccounted.

Paired macOS clients also close exactly 1600 bytes: ledger delta 73736→75336,
retained charge 0→1600, while observed extra 75464 stays unchanged. A separate
128-byte dirty-only retained residual exists in both builds. The initial assertion
that all macOS bytes would match was overbroad and is preserved with its logs.
A valid quiescent walk before collection makes 128 bytes of registry first-use
control requests and reduces the ensuing dirty residual 128→64. One 64-byte
component is therefore isolated to initialized-buffer registry first use; the
remaining 64-byte source is open. These platform control allocations are not
included in the index repair claim.

Warmed minor request counts/bytes/requested peak remain unchanged. Seven
uninstrumented paired collection ratios are 0.99369/1.00333/1.00181/0.98758
for small dirty/large clean/large dirty one/four-worker cases. Retain the roughly
0.3% large-clean median cost (its paired interval lies above one); other intervals
include one. These finite shared-host fixtures do not establish application budgets.

A separate ordinary incremental-growth client builds 1→4096 old parents across
nine minors, validates every payload and requires no major. The allocator-owned
vector replaces one reallocation with an allocate/copy/free at six growing stages;
requested collection peak remains unchanged at all nine stages. Retained charge
ends at 2112 bytes, matching the existing geometric spare capacity rather than
the first-dirty fixture's 1600. This growth allocation cost is documented; a
separate uninstrumented growing-fixture comparison is queued after the final
warmed measurements.

The exact candidate passes fresh 2468 Linux runtime checks,1224 Rust/std
ThreadSanitizer checks,1223 macOS runtime checks,102 integrations per platform,
nine strict Miri checks and72 mutation cases/18432 concurrent cycles. An ordinary
Mac control caught a stale canonical Cargo release artifact after old source
mtimes were copied. The stale failure is preserved; fresh source-specific Mac
build/integrations/control passed, and canonical debug/release outputs were then
explicitly rebuilt. Their normal control passes and the canonical runtime hash
matches the fresh target. Source bytes/modes remain unchanged.

Final rebuilt-library warmed ratios are 0.99010/1.00108/1.00022/0.99945; all but
the small dirty interval include parity. The earlier isolated large-clean cost
remains preserved. The separate growing fixture gives collection ratio 1.00356
[1.00241,1.00504] and total 0.99688[0.99188,1.01106]: retain the roughly 0.36%
growing-collection cost. No general neutrality/application-budget claim is made.
Evidence: `target/gc-validation/2026-10-19-provenance/object-index-validation/`,
`object-index-trial/`, `object-index-audit/`, `macos-index-control-audit/`, and
`object-index-growth-{audit,benchmark}/`. Artifact freshness/recovery and final
measurement scopes are recorded in `verification.json`.

## Thread census snapshots — applied and verified, 2026-10-19

Temporary collection/inspection thread owners and concurrent safepoint epochs
now use four inline slots. Larger buffers borrow the cached Heap allocator;
owner references remain retained through the collection or WorldPause guard.
The final isolated 543-input source passes 719 Linux GC checks, 719 macOS GC
checks and 12 strict Miri checks (four metadata cases across three seeds).
The identical normal single-thread probe's clean extra 8 bytes disappears;
dirty extra 73744 matches the native ledger delta. A normal six-sibling pause
probe holds 64 charged bytes until guard release and returns registry storage
to zero after retirement. No new unsafe memory operations are introduced.

Normal requested allocations fall 12→11 (clean) and 24→23 (dirty); requested
bytes and peak fall 8 in both cases. Seven alternating uninstrumented minor
pairs give collection ratios 1.00298/1.00537/1.00597/0.94554 for small dirty,
large clean, large dirty one/four-worker cases. Retain the small-case ≈0.3% cost
(interval above one); larger intervals include one and the four-worker result
is noisy. These finite shared-host results do not establish application budgets.

The exact candidate is applied. Fresh gates pass 2456 Linux runtime checks,
1218 Rust/std ThreadSanitizer checks, 1217 macOS runtime checks, 102 generated
integrations per platform, 12 strict Miri checks and 72 mutation cases / 18432
concurrent cycles. Both platforms' normal allocation controls pass. Source bytes
and modes remain unchanged across the gates and final measurements.

Final rebuilt-library controls preserve the allocation/peak reductions and the
64-byte six-sibling guard charge through release. Final collection ratios are
0.97425/1.00296/0.99797/0.99778; all four paired intervals include one, with
substantial variation in the small case. The earlier isolated small-case cost
remains documented; neither run establishes application budgets or general
neutrality. Evidence: `target/gc-validation/2026-10-19-provenance/thread-snapshot-validation/`
(`verification.json`, `peak.json`, `benchmark-summary.json`, guard control), and
isolated final/preserved initial evidence in `thread-snapshot-trial/`.

## Initialized-range snapshots — applied and verified, 2026-10-19

Collector and Heap walker range snapshots now store up to three initialized
regions inline. Larger snapshots use one allocator-owned buffer for sorted
TLAB tails and their in-place complement; small coalesced results release that
buffer before return. Both global standalone walking and Heap-accounted walking
share one traversal. Buffer registry locks are released before callbacks.
Existing validity/quiescence requirements are explicit. No new unsafe memory
operations are introduced.

The exact isolated 542-input candidate passes 711 Linux GC checks, 711 macOS GC
checks and 15 strict Miri checks. Native tests include four active TLABs with
nested snapshots and a registry-lock availability assertion from callbacks.
Miri covers five metadata model/classification/backing/owner/unwind cases across
three seeds, not the arena walker. Identical normal Linux/macOS dirty/clean
controls isolate the disappearing unaccounted 16-byte small snapshot.

Separate instrumented normal consumers confirm zero allocations for empty,
one-tail and two-tail walks. Four tails use one 80-byte request instead of two
requests totaling 144 bytes. With 64 tails, one 1040-byte request replaces two
allocations and four reallocations totaling 3024 requested bytes; measured peak
live requested storage falls from 2064 to 1040 bytes and the retained walk
backing is fully charged. Counts and timing use separately compiled consumers;
the timing binaries have no observing global allocator.

Seven paired heap-walk timings give median candidate/baseline ratios 0.48658,
0.57869, 0.60619, 1.00344 and 1.01330 for 0/1/2/4/64 tails with one object per
TLAB. The sparse 64-tail interval lies above one and remains a measured cost.
With eight objects per TLAB (512 objects), the ratio is 0.96650. Minor-call
medians for small dirty/large clean/large dirty-one/large dirty-four workloads
are 0.99410, 0.99016, 1.00489 and 1.00930; dirty intervals include one. These
finite shared-host results do not establish application budgets or general
neutrality.

The exact applied source passes 2440 Linux runtime checks, 1210 Rust/std
ThreadSanitizer checks, 1209 macOS runtime checks, 102 generated integrations
on each platform, 15 strict Miri checks, and 72 mutation cases (18432 concurrent
cycles). Source bytes and modes remain unchanged across those gates.

Final rebuilt-library measurements preserve the allocation reductions above.
Dirty minor requested allocations fall 25→24 and bytes 426037→426021;
requested peak remains 278584 bytes. Final walk ratios for 0/1/2/4/64 tails
are 0.48472/0.58658/0.60683/1.00204/0.98388; the 512-object ratio is
0.96100. Minor collection ratios are 0.99263/0.99176/1.00417/1.00934
for small dirty/large clean/large dirty-one/large dirty-four. The final large
dirty intervals lie above one: retain the roughly 0.4%/0.9% costs, along with
the earlier sparse-walk cost. These are finite shared-host measurements.
Evidence: `target/gc-validation/2026-10-19-provenance/range-snapshot-validation/`
(including paired summary files), isolated `range-snapshot-trial/`, and initial
control `range-snapshot-audit/`.

## Remembered-work staging — applied and verified, 2026-10-19

Minor remembered-object deduplication now uses the Heap accounting allocator.
An exact-sized owning iterator streams object tasks into FIFO construction,
removing the intermediate task vector. Empty major/clean-minor paths construct
neither a hasher nor an allocator owner. Hash backing remains charged until
its iterator drops; overlapping-card deduplication and scan estimates remain
covered. No new unsafe code is introduced.

The exact isolated candidate passes 108 native Linux parallel checks, 108
macOS parallel checks and nine strict Miri checks (three iterator/ownership
cases across three seeds with leak checking). Normal Linux/macOS controls
verify the table's actual request: 73744 bytes on Linux, 73736 on macOS for
this 4096-parent fixture. The separate common thread snapshot remains outside
this repair. All managed values validate and backing returns to its retained
baseline after collection.

A normal Linux allocator probe observes one fewer allocation and 65536 fewer
requested bytes per dirty collection, but peak live requested storage rises
8208 bytes (270376 to 278584) because hash backing lives longer. Clean-card
allocation traffic is unchanged. Seven paired minor workloads on fixed
preflight-selected cores give collection-time median stream/baseline ratios
0.98200 for 15 dirty parents, 1.00063 for 4096 clean parents, 0.95349 for
4096 dirty parents with one worker, and 0.95057 with four. Dirty large-workload
intervals lie below one; shared-host variation remains. These finite fixture
results are not process RSS or application budgets.

The exact 541-input applied source passes 2416 native Linux runtime checks,
1198 Rust/std ThreadSanitizer checks, 1197 macOS runtime checks, 102 generated
integrations per platform, nine strict Miri checks and 72 mutation cases /
18432 cycles. Fresh normal Linux/macOS controls verify the applied accounting.
Exact rebuilt-library peak controls reproduce one fewer request, 65536 fewer
requested bytes and 8208 more peak live requested bytes per dirty cycle.
Rebuilt-library collection-time medians are 1.00476, 1.01100, 0.96380 and
0.92134 for the four workloads above. Large dirty intervals remain below one;
the 4096-parent clean interval is above one and its approximately 1.1% median
cost is retained as a limitation. Shared-host variation is visible; these
measurements do not establish application budgets. Raw/summary evidence is
`benchmark.json`, `benchmark-summary.json`, `peak.json` and `verification.json`
in the applied checkpoint directory. Evidence: `target/gc-validation/2026-10-19-provenance/remembered-work-trial/`
and sibling `remembered-work-validation/`; negative controls in `minor-work-audit/`.

## Copy-claim spill accounting — applied and verified, 2026-10-19

Job-local copy-claim spill now borrows the Heap's cached accounting allocator.
The eight-claim inline path remains allocation-free and takes no Arc owner;
spill retains its actual backing charge after draining until the job-local
container drops. Claim ordering, partial-drain rollback, unwind and original
header restoration remain covered. No new unsafe code is introduced.

The exact isolated candidate passes 102 Linux parallel checks, 102 macOS
parallel checks and 21 strict Miri checks (seven cases across three seeds with
leak checking). Normal Linux probes isolate a 28672-byte gap: inline dispatch
requests/accounts 65536 bytes; spill dispatch now requests/accounts 94208 bytes;
both return to zero. Normal macOS before/after pairs close the same spill gap;
a separate common 64-byte allocation remains outside the backing ledger and
matches a standalone mutex first-lock control. The initial overbroad macOS
assertion and its raw output are preserved.

Seven longer paired samples show median candidate/baseline time ratios of
1.00536 for 16 roots, 0.99854 for 4096 roots, 1.01033 for the single-worker wide
graph and 1.00380 with four workers. The four-worker interval includes one.
The finite small/single-worker cost is recorded; no general neutrality or
application-budget claim is made.

The exact 540-input applied source passes 2404 native Linux runtime checks,
1192 Rust/std ThreadSanitizer checks, 1191 macOS runtime checks, 102 generated
integrations per platform, 21 strict Miri checks and 72 mutation cases / 18432
cycles. Fresh normal Linux/macOS control pairs verify the applied spill delta.
The exact rebuilt-library run has larger variation than the isolated trial:
median applied/baseline ratios are 1.00749, 1.00013, 1.04519 and 1.01760.
The wide single-worker paired interval remains above one; this cost requires
follow-up. An unrelated three-process CPU workload is present on the shared
host. A completed twelve-triple baseline/isolated/applied comparison uses fixed
preflight-selected cores 8/9/10/12, avoiding the unrelated narrow-affinity jobs
on cores 0–7 and their siblings. Applied/baseline wide ratios are 1.00024 with
one worker and 0.99801 with four; both paired intervals include one. Applied
and isolated build intervals also include one in all workloads. The larger
wide regression does not reproduce in this comparison; original raw pairs
remain retained. Shared-host background and application budgets remain open.
Evidence: `three-builds.json` and `three-builds-summary.json` in the applied
checkpoint directory.
Evidence: `target/gc-validation/2026-10-19-provenance/pending-copy-trial/` and
sibling `pending-copy-validation/`; negative/control pair in `pending-copy-audit/`.

## Collector FIFO performance follow-up — correctness verified, 2026-10-19

Cycle-local FIFO storage now borrows a cached NativeMemory accounting allocator.
Queue growth clones the reference; persistent buffers keep their independent
allocator owners. The capacity guard is separated from slow growth and empty
publication avoids redundant slice writes. No new unsafe code is introduced.

The exact isolated candidate passes 100 Linux parallel checks, 100 macOS
parallel checks and 18 strict Miri checks (six queue cases, three seeds, leak
checking). A longer three-way normal-library comparison uses twelve triples per
workload, six balanced execution orders repeated twice, and pinned cores. Against
the first accounted FIFO, median ratios are 0.96147 for 16 roots, 0.99478 for
4096 roots, 0.97222 for the single-worker wide graph and 0.99562 with four workers.
Against the original unaccounted standard queue, medians remain 1.01219, 0.99890,
1.03151 and 0.98420 respectively. This reduces the measured regressions without
establishing universal neutrality or an application performance budget. An
unrelated compiler build was active on the shared Linux host and is recorded.

The exact 540-input applied source now passes 2400 native Linux runtime checks,
1190 Rust/std ThreadSanitizer checks, 1189 macOS runtime checks, 102 generated
integrations per platform, 18 strict Miri checks and 72 concurrent mutation
cases / 18432 cycles. The normal release client accounts all 65536 queue bytes
during dispatch and returns to zero afterward. The fresh rebuilt normal libraries also completed twelve balanced triples per
workload after validation finished. Median applied/first-accounted ratios are
0.96487, 0.99150, 0.97280 and 1.00477 for the workloads listed above; the
four-worker paired interval includes one. Against original standard FIFO,
medians are 1.01729, 0.99795, 1.03369 and 0.97873. Residual single-worker costs
remain; these finite shared-host measurements do not establish application
budgets. Exact raw/summary evidence is in `work-ring-hot-validation/` as
`three-way-benchmark.json` and `three-way-summary.json`. Evidence: `target/gc-validation/2026-10-19-provenance/work-ring-hot-trial/`
(`verification.json`, `macos-verification.json`, `three-way-benchmark.json`,
`three-way-summary.json`) and sibling `work-ring-hot-validation/`.

## Collector FIFO accounting — correctness verified, performance follow-up open, 2026-10-19

The cycle-local evacuation FIFO now uses allocator-owned initialized arena
storage. Its actual backing remains charged while logically empty and is
released when collection storage drops. Wrapped growth preserves FIFO order;
both old and new allocations count during their overlap. No private standard
collection layout or estimated capacity charge is used.

The normal release-client reproduction now reports the same **65536 requested
bytes** in the Heap registry during dispatch, returning to zero afterward.
Exact-source validation covers 2396 native Linux runtime checks, 1188 Rust/std
ThreadSanitizer checks, 1187 macOS runtime checks, 102 generated integration
checks per platform, 15 strict-provenance Miri cases with leak checking, and 72
concurrent mutation cases (18432 cycles). Miri covers the five FIFO tests across
three seeds, not the complete collector.

Seven alternating paired performance samples show median changes of +4.6% for
16-root single-worker collections, approximately parity for 4096 roots, +6.4%
for the single-worker 4096-leaf graph, and -1.6% with four workers. These finite
workloads do not establish an application performance budget. The subsequent applied
performance follow-up and its current validation state are recorded above.

Evidence: `target/gc-validation/2026-10-19-provenance/work-ring-validation/verification.json`,
`observation.json`, `benchmark.json`, and `benchmark-summary.json`. Other native
control/auxiliary storage and the broader readiness requirements remain open.

## Root-address table accounting — verified correctness checkpoint, 2026-10-19

Per-collection root-address deduplication now charges actual hash bucket/control
backing to the Heap registry allocator. Inline enrollment borrows the counter
without acquiring an allocator owner. Spill/reservation acquires independent
ownership; consuming iteration retains backing charges through physical release.
The inline path allocates nothing and known-capacity/duplicate enrollment causes
no extra allocator traffic. An iterator can outlive its initial counter borrow.

The exact applied **539-input** source passes **2376 native Linux runtime checks**,
**1178 Rust/std ThreadSanitizer checks**, **1177 macOS runtime tests**, **102 generated
integrations per platform**, **15 strict Miri checks** and **72 mutation cases /
18432 concurrent cycles**. Miri covers the formatted applied source, including
nine root/borrowed-counter cases and six dependency-owner cases; it is not whole
collector strict provenance. A normal release client observes all **73744 bytes**
charged during 4096-slot enrollment and zero afterward; the original client
reported zero while backing was live. Evidence and hashes:
`target/gc-validation/2026-10-19-provenance/root-dedup-validation/verification.json`.

The exact applied release library was measured after this task's validation
jobs finished: seven pinned paired samples per root count. The 16-root median
ratio is 1.00001; paired log-ratio geomean 95% bootstrap interval is
[0.99463, 1.00734]. The 4096-root median ratio is 0.89681, with a large spike in
one pair; raw pairs are retained. This finite one-leaf root-enrollment workload
does not establish an application latency/RSS budget or a general speedup.
The warmed next-phase audit observes **65536 requested work-queue bytes** during
dispatch with a zero registry ledger, separately from this repaired root table.
That queue and other storage owners remain open. Overall readiness remains open.


## Empty hash-table allocator ownership — verified checkpoint, 2026-10-19

Pinned hashbrown 0.16.1 leaked allocator owners when consuming unallocated empty
HashMap/HashSet tables. The root-accounting trial's Miri leak led to a standalone
normal-library reproduction: allocator Arc strong counts remained 2/3 instead
of 1. New runtime regressions fail on that original source.

A checksum-verified local copy of 0.16.1 now carries a documented backport of the
upstream allocator-drop correction. Only its raw allocator-conversion file differs
from the original package; licenses/source are retained. The workspace patch keeps
the existing API and dependency version, and only that package's lockfile source
changes. Empty conversion drops the allocator locally; allocated iterators retain
their original ownership. The standalone client now reports 1/1/1 owners after
empty-set, empty-map and allocated-empty consumption. No leak checks are disabled.

The exact **539-input** snapshot (including vendored source) passes **2364 native
Linux runtime checks**, **1172 Rust/std ThreadSanitizer checks**, **1171 macOS runtime
tests**, **102 generated integrations per platform**, **six strict Miri checks**,
and **72 mutation cases / 18432 concurrent cycles**. The initial omitted-import
compile failure is preserved. Original crate checksum, exact vendor delta,
lockfile scope, raw soak logs and hashes are checked by
`target/gc-validation/2026-10-19-provenance/hashbrown-backport/verification.json`.
See `crates/vendor/hashbrown/GC_PATCHES.md` for the upstream revision and patch.

The root-table accounting change remains an isolated trial. Its refined version
borrows the counter until spill, passes 88 native + nine strict Miri checks and
charges all 73744 observed table bytes before releasing them after collection.
Applied-source validation is still required. Overall readiness remains open.


## Pointer-preserving worker jobs — verified checkpoint, 2026-10-19

Synchronous worker dispatch now keeps the borrowed closure context as a pointer
through type erasure. A private documented Send contract shares `F: Sync` until
all participants complete; it does not require movable or static closures.
A new MutexGuard-owning closure regression checks borrowed non-Send state across
changing worker counts. Existing panic propagation/reuse and shutdown cases also
pass strict-provenance Miri, where the old integer-context control stopped.

The exact applied **489-input** source passes **2356 native Linux runtime checks**,
**1168 Rust/std ThreadSanitizer checks**, **1167 macOS runtime tests**, **102 generated
integrations per platform**, **15 strict Miri checks**, and **72 mutation cases /
18432 concurrent cycles**. Strict Miri's trial and applied manifests match every
input byte/mode; copied logs/hashes and required named cases are checked by the
final verifier. The normal release client still observes 96 worker-handle bytes.
Evidence: `target/gc-validation/2026-10-19-provenance/worker-context-validation/verification.json`.
This is worker-job provenance coverage, not whole-collector strict provenance.

The subsequent isolated root-deduplication accounting trial reproduces 73744
untracked temporary bytes, then fixes the normal-runtime charge, but its inline
Miri case finds a real allocator-owner leak in pinned hashbrown 0.16.1's empty
consuming iteration. That trial is not applied. Dependency-level reproduction
and original failure are retained in `root-dedup-trial/`; the project pad records
the upstream issue. Overall production readiness remains open.


## Heap worker-handle backing — verified checkpoint, 2026-10-19

Parallel worker-pool helper handles now retain the Heap registry allocator.
Backing stays charged through changing participation, native helper exit and TLS
shutdown, then releases with the vector. No per-job accounting is added. Shared
control/OS resources and reusable thread-local scratch remain separate owners.

The exact **489-input** snapshot passes **2352 native Linux runtime checks**,
**1166 Rust/std ThreadSanitizer checks**, **1165 macOS runtime tests**, **102 generated
integrations per platform**, three exposed-provenance Miri lifetime checks, and
**72 seeded mutation cases / 18432 concurrent cycles**. A normal release client
reports 96 worker-handle bytes where the previous runtime reported zero. Six
initial four-worker fixture failures omitted this owner; updated assertions
include it separately and preserve zero after Heap destruction. Sources, patch,
matrices, raw soak logs and artifact/client hashes:
`target/gc-validation/2026-10-19-provenance/worker-handles-validation/verification.json`.

A separate one-file trial preserves borrowed job context as a pointer instead
of reconstructing it from an integer. It passes 15 strict Miri and 82 native
parallel checks; its subsequent applied-source validation is separate from this
accounting checkpoint. Overall readiness remains open.


## Global SATB queue ownership — verified checkpoint, 2026-10-19

Heap-created queues now charge bitmap, chunk-container and internally created
payload backing. Public standard vectors transfer their allocation without copying
and become charged at adoption; existing accounted mutator chunks retain their
original allocator without double charging. Drained batches and payload iterators
keep ownership until physical release. Public drain_all returns an independent
standard snapshot. Bitmap backing remains charged across cycle boundaries.

The exact applied **489-input** snapshot passes **2348 native Linux runtime checks**,
**1164 Rust/std ThreadSanitizer checks**, **1163 macOS runtime tests**, **102 generated
integrations per platform**, three strict-provenance public-vector lifetime checks
and three exposed-provenance bounded-cycle checks. Strict bounded-cycle Miri hits
the existing integer-to-pointer policy decoder; its unsupported run is retained.
The **72-case** seeded mutation soak passes **18432 concurrent cycles**; all raw
logs match parsed report counters and the exact source manifest.

A normal release client now reports the full 8320 bytes (8192 payload + 128
container), drops the ledger to zero after draining, and retains its independent
snapshot after Heap drop. The initial registry-compaction fixture omitted the
newly charged retained bitmap; updated assertions include it and still check
zero after final Heap release. Initial failure/source evidence is retained.

Pinned single-thread public queue push/drain measurements show about 20 ns extra
for one-value publications (median paired ratio 1.267); 256-value measurements
are noisy and establish no improvement. This is scoped overhead, not overall
collector throughput. Exact sources, patch, matrix, client/artifact hashes and
soak evidence: `target/gc-validation/2026-10-19-provenance/satb-queue-validation/verification.json`.

Other controls/collector auxiliary storage remain outside this ledger. A normal
four-worker collection retains additional requested bytes while the ledger stays
zero; ownership includes Heap worker records, OS-thread scratch and transient
cycle storage, which require distinct lifetime treatment. That observation is
retained in `worker-storage-audit/observation.json`. Overall readiness remains open.


## Bounded statemap tracing — verified checkpoint, 2026-10-19

Tracing now retains a bounded history: defaults are 65536 events and 1 MiB of
custom entity text, with explicit eviction/rejection counts in the exported title.
Common GC/thread recording allocates nothing, including ring rollover. Export
escapes arbitrary entity text, uses the constructor's wall-clock origin, and
snapshots before calling external writers so reentrant recording remains possible.

The exact applied **489-input** snapshot passes **2340 native Linux runtime checks**,
**1160 Rust/std ThreadSanitizer checks**, **1159 macOS runtime tests**, **102 generated
integrations on each platform**, and **18 focused strict-provenance Miri checks**
with host-clock access. A normal-runtime Linux client measures zero allocations
for 4096 warmed thread events and validates every JSON record. A release-runtime
2.11-million-event soak retains exactly 65536 numeric events, then 4096 custom
entities within the 1 MiB text budget. RSS observations are retained without a
whole-process memory guarantee.

The pinned upstream statemap consumer renders ordinary GC/thread entities and
preserves the loss title. Its renderer emits malformed SVG for quoted entity
names despite valid input JSON; the input, output and upstream revision are
recorded in the project pad. Snapshot buffers and shared tracer ownership remain
separate from the Heap native ledger. Full source, patch, matrices and client/
artifact hashes: `target/gc-validation/2026-10-19-provenance/statemap-validation/verification.json`.
Overall production readiness remains open.


## Heap-retired profile backing — verified checkpoint, 2026-10-19

Retired allocation-site totals now use the heap's MetadataAllocator in both
semispace and generational constructors. Their high-water backing stays charged
through final state-observer release until Heap drop. Report snapshots remain
standalone owned vectors and survive their source heap. Retirement growth is a
cold accounting event; profiling increments are unchanged.

Native debug/release reproduce the missing table charge for both constructors.
New lifetime tests separate state/census backing, the retained heap table, and
independent reports. Updated foreign-scope/registry-compaction fixtures include
the remaining heap-owned 64-byte table and assert zero after final Heap release;
the first candidate's stale-total failures are retained.

The exact applied **488-input** snapshot passes **2316 native Linux runtime checks**,
**1148 Rust/std ThreadSanitizer checks**, **1147 macOS runtime tests**, **102 generated
allocation/telemetry/channel/thread/FFI integrations on each platform**, and **nine
focused strict-provenance Miri checks** across seeds 0/1/17. A plain normal-runtime
client reports 133120 bytes after retirement while its state observer lives,
then 65536 bytes owned by Heap after that observer drops; its report remains valid
after Heap drop. Exact source, patch, matrices and compiled-client/artifact hashes:
`target/gc-validation/2026-10-19-provenance/retired-profile-validation/verification.json`.

Controls, temporary snapshots and other collector auxiliary allocations remain
outside this ledger, which is not RSS. The subsequent statemap checkpoint above repairs the JSON, clock-origin,
allocation and retention defects reproduced in `statemap-audit/observation.json`.
Overall production readiness remains open.


## Registration buffer backing — verified checkpoint, 2026-10-19

Heap registration now gives each state's SATB buffer and allocation-site counter
vector MetadataAllocator ownership. Requested backing remains charged through
retirement and Heap drop until the final state observer releases it. A SATB
fallback handoff carries its allocator through queue and iterator ownership;
standalone queue inputs retain their zero-copy vectors. Normal bounded collector
flushes reuse local storage. Warm profile writes allocate/recharge nothing, and
no per-write accounting was added to the local barrier push. Chunk-metadata
statistics reflect the actual owner representation. Public SATB drain returns a
standard snapshot and resets the configured reservation, including oversized input.

Native debug/release independently reproduce missing SATB/profile accounting.
Ledger fixtures include state backing while checking census ownership separately.
The first full-run failure and its poisoned-census cleanup abort are retained;
updated handoff assertions release guards before comparing complete owned totals.

The exact applied **487-input** snapshot passes **2308 native Linux runtime checks**,
**1144 Rust/std ThreadSanitizer checks**, **1143 macOS runtime tests**, **102 generated
allocation/telemetry/channel/thread/FFI integrations on each platform**, and **21
focused strict-provenance Miri checks** (seven cases × seeds 0/1/17). Miri covers
raw counter-window writes across growth and warm owner updates, native queue/
iterator transfers and public snapshots; it is not whole-collector strict provenance.
A plain client linked to the normal cfg(test)-free runtime observes registry bytes
0 → 2080 registered → 67616 profiled → 67584 retired/observer-held → 0 released,
while its 4096-entry standard snapshot stays valid. Source/rlib/client and compiler/
static-runtime hashes are retained with the archived source and repair patch:
`target/gc-validation/2026-10-19-provenance/registration-buffers-validation/verification.json`.

Record/control allocations (including embedded scratch roots), temporary snapshots,
heap-retired profile totals and other collector auxiliary storage remain outside
this ledger. It is not total native RSS. The remaining inventory and production
performance/resource/embedding requirements remain open.


## Channel condition-variable sibling scopes — verified checkpoint, 2026-10-19

Full-queue send and empty-queue receive now park all current-OS-thread RUNNING
registrations for the heap during their condition-variable waits. The shared
blocking scope preserves preblocked siblings, wait-counter/state-mutex ownership,
and frame publication through complete restoration. Native debug/release
reproduce both missing-sibling cases before repair. Repaired bounded fixtures
perform moving collection during the actual wait, preserve rooted payload 42,
and release/join before assertions.

The exact applied **484-input** snapshot passes **2280 native Linux runtime
checks**, **1130 Rust/std ThreadSanitizer checks**, **1129 macOS runtime tests**,
**64 generated channel/concurrency/thread integrations on each platform**, and
**six exposed-provenance Miri checks** across seeds 0/1/17. Exact source archive,
repair patch, matrix/compiled-artifact hashes and performance scope are retained:
`target/gc-validation/2026-10-19-provenance/channel-siblings-validation/verification.json`.

Paired native AOT handoffs use one compiler, hashed prior/repaired runtime archives,
ordered messages with checked sum, zero collections, and explicit per-thread
pinning. Six pairs of 200,000 messages show elapsed ratios 1.0068/1.0052 for
same-core capacities 1/64 and 0.9960 for separate-core capacity 64. Separate-core
capacity 1 is variable; an eight-pair 1,000,000-message confirmation observes
1.0269 with a paired bootstrap 95% interval 0.9944–1.0587. These finite samples
do not establish general throughput or latency/RSS budgets. Raw timings, CPU/RSS,
load, topology, sources and binaries are retained; see GC_PERFORMANCE.md.
Overall readiness and the remaining native-memory inventory remain open.


## Owned foreign-call sibling scopes — verified checkpoint, 2026-10-19

Foreign entry now owns a token suspending every RUNNING registration on the
current OS thread for the heap. Callback entry consumes that token and restores
the complete group before managed execution; callback exit republishes roots
and parks a new group. Nested foreign calls use independent scope ownership.
Already-BLOCKED registrations retain their state and frame publication.
Tokens own their heap with Arc handles, and scope backing uses MetadataAllocator
accounting. Warm calls reuse backing without allocator traffic; runtime retirement
releases it. Retirement with an active foreign scope aborts before deregistration.
Existing generated Thread field offsets remain checked and unchanged.

Native debug/release reproduced the missing embedding sibling suspension. The
nested-callback regression performs moving collections in both native and managed
phases, verifies rooted payload 42, and checks warm reuse and final backing release.
The exact applied 483-input snapshot passes **2272 native Linux runtime checks**,
**1126 Rust/std ThreadSanitizer checks**, **1125 macOS runtime tests**, and **110
FFI/thread/telemetry/lifetime integrations on each of native Linux and applied
macOS**. Rebuilt compiler/static-runtime artifacts are hashed. Six exposed-provenance
Miri checks cover nested moving collections and ABI storage across three seeds;
three separate ABI-storage checks pass strict-provenance Miri. Whole-collector
strict mode remains unsupported by integer root-slot transport, with its original
failure retained; these results do not establish whole-collector strict provenance.

Exact source archive, patch, matrices, artifact hashes and Miri scope:
`target/gc-validation/2026-10-19-provenance/ffi-siblings-validation/verification.json`.
The newer channel-wait checkpoint extends the sibling-coordination coverage.
Overall production readiness and performance/resource budgets remain open.


## Combined sibling coordination — verified checkpoint, 2026-10-19

High-level and generated ABI polling park same-OS-thread RUNNING registrations
for their heap and restore the complete group under one census lock after the
pause ends. Concurrent copying can continue between pause phases. The shared
native blocking helper likewise parks siblings, preserves preblocked scopes,
and restores before clearing the caller's published frame.

Automatic native-wait restoration performs one census transition after pause
and copy windows close. This excludes new collection censuses without recursively
acquiring a coordinator already held by supported retirement paths. Explicit
collector-side restoration retains its existing coordinator exclusion.

Native debug/release reproduce missing sibling parking in high-level polling,
generated polling and runtime sleep. Rejected whole-cycle polling waits and
coordinator-reentrant native waits are retained with their SATB/retirement failures.
The nested blocking/unwind regression verifies state and published-frame lifetime.

The exact applied 483-input snapshot passes **2264 native Linux runtime checks**,
**1122 Rust/std ThreadSanitizer checks**, **1121 macOS runtime tests**, **58 focused
language integrations**, and **518 complete compiler/language tests in each
macOS debug/release profile**. Named regressions execute in all native profiles
and worker configurations, with source bytes/modes unchanged. Exact evidence,
source archive, matrices and combined repair patch:
`target/gc-validation/2026-10-19-provenance/blocking-siblings-validation/verification.json`.
Separate foreign-call enter/leave and callback ownership is under validation in
an isolated successor; this checkpoint does not claim overall readiness.

## Sibling native waits and group restoration — verified checkpoint, 2026-10-19

Contended `MutatorThread::lock_safe` now parks every RUNNING registration on the
current OS thread for its heap, preserving already-BLOCKED siblings. Native
mutex ownership is released before restoration, including poison paths.
Automatic group restoration acquires collector exclusion while all members are
still BLOCKED and retains it until the whole group is restored. This also closes
the partial-restoration race in shared telemetry parking guards.

Both missing sibling parking and missing restoration exclusion were reproduced
on native Linux in debug/release before their repairs. The applied 483-input
snapshot passes 2244 native Linux runtime checks, 1112 Rust/std ThreadSanitizer
checks, 28 focused macOS checks and 58 macOS language integration checks.
The warmed uncontended lock path allocates nothing and leaves safepoint
generations unchanged. Cold platform mutex initialization is outside that claim.
Evidence and exact source hashes/modes: `target/gc-validation/2026-10-19-provenance/lock-safe-siblings-validation/verification.json`.
Ordinary safepoint sibling coordination and broader embedding/reentry remain
under investigation; this checkpoint does not establish overall readiness.


## Thread census native backing — verified checkpoint, 2026-10-19

The heap's thread-census pointer backing now uses the registry MetadataAllocator.
Requested bytes remain charged until physical deallocation, including staged
replacement and old storage waiting for disposal. Sparse census backing is
planned outside its mutex, revalidated against concurrent growth, then installed
without allocation. Only Arc entries move; state addresses and diagnostic IDs
stay stable. Final retirement detaches empty backing and frees it after registry
and collector locks are released. Existing native-pressure policy consumes the
newly accounted bytes; no per-object accounting work is added.

Native Linux debug/release reproduced both missing accounting and capacity 128
retained after shrinking to three registrations. Tests now verify exact ledger
ownership, sparse reclamation, final release despite external state observers,
concurrent growth during staging and access during old-buffer disposal. Exact
root/handoff assertions include census ownership. Pressure tests seed real
unserviced demand before arming their observers; production admission is unchanged.

The applied 482-input snapshot passes 2228 full native Linux runtime checks,
1104 Rust/std ThreadSanitizer checks without reports, 1,103 macOS runtime checks
across debug/release, and 58 macOS language thread/completion/telemetry/lifetime
checks. A separate 2 MiB first-channel-allocation program preserves live caller
references through native-pressure collection in normal/stress JIT and AOT on
both macOS and Linux (eight runs). Source bytes/modes match the frozen candidate.

Evidence: `target/gc-validation/2026-10-19-provenance/thread-census-accounting-validation/verification.json`
and `candidate.patch`. Native failures and earlier candidate/fixture failures
remain in sibling `thread-census-accounting-experiment/`. ThreadState record/control
allocations, per-state buffers, temporary snapshots, profile totals and optional
statemap history remain outside this repair's scope and are inventoried in the
project pad. Full native accounting and overall production readiness remain open.

## Stable thread diagnostic identities — verified checkpoint, 2026-10-19

The registry audit reproduced three failures in native Linux debug/release:
retirement followed by registration reused a surviving diagnostic ID, heaps
sharing one tracer reused IDs, and collector trace labels changed after census
compaction. Each ThreadState now owns a checked process-wide ID; registration,
mutator tracing and both collector pauses use that stable identity. Exhaustion
panics without wrapping before constructing a state. The ID is allocated only
at state construction and read immutably during tracing.

The 481-input snapshot passes 68 macOS debug/release identity and language
thread/completion/telemetry/metadata-lifetime checks, 2216 full native Linux
runtime checks and 1098 native Linux Rust/std ThreadSanitizer checks without
reports. Runtime coverage includes debug/release, one/four workers, armed
detectors and doctests. Source hashes and modes remain exact. Evidence:
`target/gc-validation/2026-10-19-provenance/thread-identity-validation/verification.json`;
authentic failures: sibling `thread-identity-reproduction/verification.json`.
This repairs diagnostic identity; census/control/counter native allocation
accounting remains open and is inventoried in the project pad.

## Custom headers with tagged graph slots — verified checkpoint, 2026-10-19

A combined matrix now crosses three custom header geometries with identity,
low-tag and high-tag pointer encodings, major/minor/concurrent collection and
one/two/four/eight workers. It checks fixed references, inline interior slots,
variable-length reference elements, encoded nulls, shared aliases, complete
cycle IDs, exact live bytes and byte-for-byte header preservation. Generational
cases also publish a young object into old fixed/interior/varlen slots, check
all three relocated destinations after a remembered-set minor, then run major
collections. Concurrent cases use a parked owner while copier workers run.

The matrix passed 648 layout cases and 2,160 collections: macOS and native Linux
debug/release, native Linux ThreadSanitizer with Rust and std instrumented, and
s390x big-endian debug/release under QEMU (one/four workers). The surrounding
custom-header checks and direct big-endian cases total 61 passing tests.
A missing-interior-descriptor control fails at the intended stale-slot oracle;
its isolated source is restored and hash-verified afterward. Every spawned
positive child must execute the named regression as well as exit successfully.

The 480-input snapshot differs from the preceding liveness checkpoint only in
test support: one new matrix file, its cfg(test) module and a test-header
visibility adjustment. Runtime implementation code is unchanged. All source
bytes/modes match the archive; native and sanitizer builds use stage-owned
output directories, and big-endian outputs use a fresh directory.
Evidence: `target/gc-validation/2026-10-19-provenance/custom-header-policy-validation/verification.json`
and `repair.patch`. Native Linux runs use `computer.jimmyhmiller.com`; the
existing local s390x Docker/QEMU image supplies the distinct big-endian check.
Further descriptor rejection cases, other header/policy geometries, active
mutation, embedding lifetime/reentry and native accounting remain open.

## Reference-only root liveness — verified checkpoint, 2026-10-19

Wide scalar records previously inflated copied root-liveness sets even though
scalar locals have no traced mirror to clear. The analysis now tracks direct
reference locals. Dead-root emission visits the dead set instead of scanning
all locals after every statement. Indirect aggregate and full-debug lifetimes
retain their existing behavior. A differential test compares reference results
against full liveness across mixed scalar/reference control flow.

Bounded regressions compile an 8,190-field scalar record and verify a managed
reference inside a 4,096-field record survives a subsequent stress collection.
The 479-input snapshot passes 36 native Linux and 34 macOS checks across debug
and release: liveness units, dead-root snapshots, operand/safepoint integrations,
wide values, and Linux concurrent JIT probes validating eight modes per profile.
All source bytes and modes match the archived snapshot.

The final delta changes two implementation files and adds one regression file
after the preceding full-workspace-tested nested-offset checkpoint. That prior
workspace result is scoped to its 478-input snapshot, not this final delta.
Evidence: `target/gc-validation/2026-10-19-provenance/root-liveness-validation/verification.json`
and `repair.patch`. Diagnostic timing pairs are retained in sibling
`wide-struct-investigation/paired-diagnostic.json`; they are workload-specific
macOS observations, not a general compiler-performance guarantee. Overall
production-readiness requirements remain open.

## Nested value reference offsets — verified checkpoint, 2026-10-19

Composing a valid 8,191-reference value enum after two scalar fields previously
panicked in debug code generation and silently wrapped interior-root offsets in
release. Inline struct and tuple construction now validates composed references.
Code generation and descriptor construction share the checked offset visitor.
Intermediate offsets remain wide so scalar-only nested leaves beyond offset
65,535 are accepted; only actual reference locations consume descriptor offsets.
Both debug failure modes were reproduced on native Linux before the repair.

Named, tuple and recursively nested overflow cases return layout diagnostics
before codegen. Accepted boundaries, scalar-only LLVM emission, heap snapshots
and reflection pass 64 focused Linux and 22 macOS checks across both profiles.
The full Linux workspace passes 1063 debug / 1062 release checks, both concurrent
JIT probes validate eight modes, and the armed detector passes. Source hashes
and modes match the archived 478-input snapshot throughout.

Evidence: `target/gc-validation/2026-10-19-provenance/nested-value-offset-validation/verification.json`;
original failures: sibling `nested-value-offset-reproduction/`; repair delta:
`nested-value-offset-validation/repair.patch`. Wide-record compilation performance
was recorded separately and remains under investigation. Overall readiness
requirements remain open.

## Value enum reference counts — verified checkpoint, 2026-10-19

A native Linux reproduction with 65,536 reference payloads panicked in debug
and silently emitted an 8-byte layout in release. Recursive reference counting
now uses checked addition, and layout construction rejects canonical GC reference
offsets beyond the descriptor range. Tests preserve the accepted layout boundary
at 8,192 references and reject 8,193 references and count overflow. Direct and
nested count overflow are covered independently.

The 478-input source snapshot passed 48 native Linux checks across debug/release
(count units, layout limits, heap snapshots and reflection) and eight macOS
checks across both profiles. Fresh Linux stage-owned builds executed the named
regressions in both profiles; source bytes and modes remained unchanged.
Evidence: `target/gc-validation/2026-10-19-provenance/value-enum-count-validation/verification.json`;
original failure: sibling `value-enum-count-reproduction/native/`.
This establishes count/leading-slot bounds; nested aggregate offset composition
remains under audit.

## Inline enum metadata and rendering — verified checkpoint, 2026-10-19

Compiler metadata describes canonical value enums and projected nested values
whose references live in enclosing pointer slots. Child/enclosing reference maps
compose through nested views and survive AOT encoding/decoding. Named payloads
retain source names. Boxed enum aggregate payloads participate in source-order
reflection with kind 5; scalar access rejects them without reading their storage.
Metadata offset/count errors propagate through JIT, AOT and reflection emission.

The language regression covers named/direct-reference/unit variants, nested
structs, two projected enum levels and boxed payloads in normal/stress JIT and
native execution. Linux workspace passed 1057 debug / 1056 release checks plus
the armed detector. Separately ordered debug/release concurrent probes each
validated all eight required modes. macOS metadata/dump/integration passed 56
checks. Controls breaking nested composition or AOT-map decoding fail at their
intended language assertions; restored source passes all four execution modes.

Generated AOT programs now require `gcr_runtime_main_v4`, rejecting archives that
cannot decode projected metadata at link time. v3/v2/legacy entries remain for old
programs. This final ABI-only delta changes three inputs from the workspace-tested
snapshot and passed 20 Linux debug/release plus 10 macOS archive/snapshot/lifetime
checks. Both 477-input source snapshots and the delta are retained and verified.

Evidence: `target/gc-validation/2026-10-19-provenance/inline-enum-metadata-validation/verification.json`
and sibling `inline-enum-metadata-abi-validation/verification.json`. Cumulative
patch: sibling `inline-enum-metadata-final.patch`. Earlier cross-checkout cache
results were discarded; fresh stage-owned builds and explicit regression checks
provide current evidence. Original stderr ordering prevented section attribution
of concurrent counters, so those probes were rerun with streams merged before SSH.
Other GC readiness requirements remain open.

## Public telemetry wait coordination repair — 2026-10-19

A native Linux reproduction showed a registered RUNNING caller blocked in trace
sink flushing prevents independent world-pause progress. The test releases the
sink and joins both threads before reporting the failure. Public telemetry
readers/configuration/flush now suspend all current registrations before native
lock or I/O waits and release the telemetry guard before restoring RUNNING.
Collector event recording keeps its paused path; no allocation hot-path change
or asynchronous-sink guarantee is claimed. The frame publication/reload contract
is documented in the APIs and [gc.md](gc.md).

Seven focused tests pass in debug/release on Linux and macOS. They cover blocked
sink progress, sibling restoration, all seven entry points, lock release after an
explicit completion checkpoint during a world pause, and panic/poison restoration.
A deliberately incorrect restoration-order control fails through its intended
assertion. The full Linux runtime matrix passed 2180 checks with verifier/poisoning,
including debug/release, one/four workers, detectors and doctests. The full native Linux runtime/standard-library ThreadSanitizer matrix also passes
1080 checks without reports. The applied 477-input worktree exactly matches the
validated archive. All 15 applied-worktree language telemetry, generational logging,
allocation-profile and snapshot-progress integration checks pass.

Evidence: `target/gc-validation/2026-10-19-provenance/telemetry-blocking-validation/`;
original failure: sibling `telemetry-blocking-baseline/`. Remaining transition,
embedding and production-readiness requirements remain open.

## Policy-aware heap inspection repair — 2026-10-19

Added explicit pointer-policy selection to root visitation and text/JSON heap
inspection. Root slots, fixed/interior/varlen graph edges and reflected reference
fields now use the selected collector decoder, including recursive inline value
fields and encoded null. Existing convenience APIs select IdentityPtrPolicy.
No policy state was added to allocation or collection paths; snapshot schema is
unchanged. The embedding contract is documented in [gc.md](gc.md).

A 12-case matrix covers Compact/Full, semispace/generational heaps and identity,
low-tag and high-tag encodings, checking roots, edges, rendering and reachability
before and after relocation. Three controls independently restoring raw decoding
for roots, edges or rendering fail through regression assertions. Native Linux
focused debug/release and the full 2168-check runtime matrix pass with verification
and poisoning; macOS debug/release dump suites pass six checks each. Applied source
bytes/modes exactly match the archived 477-input candidate. All 26 applied-worktree language reflection, heap command/diff, mid-execution
snapshot and snapshot-progress integration checks also pass.

Evidence: `target/gc-validation/2026-10-19-provenance/inspection-policy-validation/`.
This repairs inspection coverage; the remaining operation/embedding audits and
broader production requirements remain open.

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

## Sustained channel lifecycle evidence — 2026-10-19

Two 300-second native Linux phases passed 694,641 waves: 360,936 semispace and
333,705 generational. Each wave constructs 512 bound channels whose queued
messages refer to their owners, collects live queues, closes/drains and validates
every message, drops owner roots, collects again and checks the fixed live tree.
All 355,656,192 channel lifetimes returned to 163,824 managed live bytes and zero
native queue-buffer bytes after each wave. A checked/freed 64 KiB FFI buffer adds
native allocation churn. Four GC workers were requested.

RSS high-water observations were 3940/4000 KiB; 30-second medians rose in steps
and ended at 3940/3976 KiB. These observations do not establish a long-term plateau.
Tracked registry maxima were 84 bytes and teardown reported 32 bytes in each mode;
these counters exclude Arc/control allocations and allocator overhead.

The raw datasets, exact quantiles and RSS samples passed full verification,
including ordered waves, monotonic timestamps/counters, live-byte checks and
matching archived harness hashes. Evidence:
`target/gc-validation/2026-10-19-provenance/channel-operation-soak-linux/sustained300/`.
The runtime matches the 476-input verifier-fixed snapshot. This workload explicitly
closes and drains queues; queued-cycle abandonment is being validated separately.
It does not establish application latency/resource budgets or overall readiness.

## Continuous single-runtime operating evidence — 2026-10-19

Two native Linux runs each kept one runtime alive for 300 seconds, preserving a
4095-node/163,800-byte tree while each wave created/validated 512 bound native
atomic cells, allocated/validated/freed a 64 KiB FFI buffer, called GC and checked
every live node and edge. All 4,128,755 waves passed: 2,179,281 semispace and
1,949,474 generational, including 365,615 recorded minor collections in the latter.
That is 2,113,922,560 native cell constructions across the two runs.

Observed process RSS/VmHWM plateaued at 3708/4208 KiB. Thirty-second RSS medians
were stable from the 90-second window onward; tracked registry maxima remained
32/167,968 bytes in every window, and both reported 32 bytes after context
teardown while the heap/root remained held for inspection. These tracked bytes
exclude Arc/control allocations and allocator overhead; RSS is separate evidence.

Exact observed GC-call P99 durations were 127.750/128.601 microseconds, with maxima
3.734/3.791 milliseconds. Calls include coordination/native maintenance and may
coalesce; these are not isolated pauses or application-request latency budgets.
The standalone embedding workload, four workers and recorded Linux host conditions
bound the claim. Other workloads, platforms, longer service lifetimes and remaining
operation/embedding/accounting requirements remain open.

Evidence: `target/gc-validation/2026-10-19-provenance/operation-soak-linux/sustained300/`
contains verified compressed raw wave logs, RSS samples, exact quantiles/windows,
binary/source hashes and an archive of the exact harness/observer/lock sources.
All 476 runtime inputs match the preceding complete release checkpoint. Overall
production readiness is unfinished; continue beyond these observed results.


## Bounded local work sharing — 2026-10-19

Applied the tested two-file queue/scratch change. Active workers retain at most
one extra preallocated 128-entry batch of published object tasks. A parked-helper
hint guides overflow donation under the queue predicate mutex; retained tasks
remain covered by the active producer. BUSY retries and large-copy boundaries
return retained work before retiring that producer. Scratch leases clear the
added buffer on normal return, unwinding, nesting and TLS teardown. Idle helpers
still prepare only their original batch buffer.

Full processing batches precede donation. The bounded subprocess model checks
exact visitation/quiescence plus local bounds, parked-helper donation and narrow
frontiers. Restoring eager sharing fails the narrow-frontier assertion; retiring
a producer with retained work fails the active-work assertion. Complete candidate
runtime suites pass on macOS and Linux; focused ThreadSanitizer passes 39 tests
on macOS and 39 at each Linux worker setting 1/4, without reports. All 12 standard
Miri probes and nine strict-provenance queue probes pass. The latter establish
only their focused models, not whole-collector strict provenance.

The macOS checked tree comparison shows median paired candidate/baseline ratios
0.757/0.609 at four/eight workers; cyclic and sustained payload comparisons are
near baseline. Linux timings vary widely, including same-binary controls; no
Linux throughput improvement or regression-free guarantee is established. See
GC_PERFORMANCE.md and retained evidence under
`target/gc-validation/2026-10-19-provenance/`. All 476 applied source bytes/modes
match the tested candidate. The full applied-source matrix passed:
1054 debug/1053 release macOS workspace executions; both native Linux workspace
profiles and JIT matrices; both platform sanitizers; 760 big-endian executions;
12 Miri probes; and all 72 seeded soak cases (18,432 concurrent cycles).
`work-sharing-full-verification.json` verifies source/archive identity and counts.
Overall readiness, application latency/resource budgets and remaining
operation/embedding/accounting work remain unfinished. A separate continuous
single-runtime resource harness is entering native Linux smoke validation.


## Exported allocation storage repair — 2026-10-19

A further Miri probe reproduces invalidated exported aliases when moving the
allocator's boxed atomic cursor. The boxed heap allocation window has the same
problem independently. An isolated candidate introduces private AbiStorage<T>
sole ownership through Box::into_raw and one final Box::from_raw. It preserves
allocation alignment, payload destruction and existing synchronization without
extra allocations or reference counting, and also shares the runtime thread's
existing raw-owner implementation.

The candidate passes 536 debug and 535 release runtime tests, all 25 tooling
tests, and 12 Linux Miri case/seed interpretations. The new heap regression
exports the window before moving its owner, then accesses the window/cursor
and performs an allocation. Independent controls restoring either Box owner
fail with the expected Unique-retag violation. Primitive tests also cover
256-byte alignment and exactly-once destruction.

Evidence and the checked patch are retained under
`target/gc-validation/2026-10-19-provenance/`. The candidate source matches its
successful Miri manifest. Applied all nine prepared files with exact before/after
bytes and modes checked after the preceding gate became terminal. That gate
failed in Linux release because a batched-scan fixture assumed 1024 consecutive
24-byte leaves would include a specific lock stripe. The fixture now selects
an exact collision with a valid variable-length padding allocation over a range
containing a complete aligned stripe block. The existing lock-release, partial
batch, old-slot preservation and zero-native-scratch assertions remain intact.
The new applied-source full matrix passes: 1054 debug/1053 release macOS
workspace executions, both native Linux workspace profiles and explicit JIT
matrices, both platform sanitizer suites, 760 big-endian executions, all 12 Miri
interpretations and all 72 seeded soak cases (18,432 concurrent cycles). Verified
all 476 source/archive bytes and modes. Evidence: `abi-all-gates/report.json` and
`abi-full-verification.json` under the directory above. The failed predecessor
is retained separately. Matched allocation measurement passed all 48 processes, with median paired
candidate/baseline ratios 0.951–0.999; see GC_PERFORMANCE.md for scope. Application
latency/RSS budgets, hosted CI and remaining operation/embedding/accounting work
still keep overall production readiness unfinished.

## Runtime ABI and indirect-root provenance — 2026-10-19

Miri exposed two runtime pointer-provenance defects beyond the native/sanitizer
checkpoint. Returning/moving RuntimeThread's Box owner invalidated its escaped
poll-field pointer under Stacked Borrows. RuntimeThreadStorage now keeps sole
ownership through Box::into_raw and NonNull, reconstructing Box only for final
destruction after registration retires. The Thread ABI and allocation size are
unchanged; no reference counting was added.

The frame walker also read indirect address cells as integers before casting
back to pointers. It now loads pointers directly. A Rust fixture additionally
needed to derive its published frame pointer from the whole ValueFrame rather
than a header-only borrow; the walker contract documents full-allocation
provenance and valid writable root targets.

The isolated candidate passed the complete native runtime debug/release suites
(533/532 passes) and three focused Miri cases. Independent controls reproduce
both production defects when either original behavior is restored. The applied
source now passes the new linux-miri suite: all nine case/seed interpretations,
with 25 tooling tests, shell syntax and workflow lint. Source-identified evidence:
`target/gc-validation/2026-10-19-provenance/miri/`; original failures and controls
remain under `2026-10-19-public-lock/`.

The subsequent full applied-source matrix passed; see the allocator repair checkpoint above.
Default interpreter checks remain enabled; managed integer-pointer paths emit
provenance warnings, so these checks do not establish whole-collector soundness
or strict provenance. Hosted CI, remaining operation/embedding/native-accounting
work, long-duration resource/latency evidence and numerical performance budgets
remain open. Overall production readiness is unfinished.

## Public native-lock resumption — 2026-10-19

Applied a fix for an embedding deadlock in MutatorThread::lock_safe. It formerly
held a native mutex while BLOCKED and waiting for a world pause to end; an
embedding root scanner needing that mutex could not finish the pause. The
helper now waits for availability without retaining ownership, restores RUNNING
through an unwind guard, then retries acquisition. Poisoned guards are also
released before resumption. Unsafe callers must drop returned guards before
parking, polling or making a collecting call.

Three bounded regressions cover ordinary contention/root scanning, poison
arriving during contention, and immediate poison with subsequent clean-lock
acquisition. Restoring the original method fails both contended cases; exact
source restoration passes debug/release. Big-endian selection now includes
these cases and the aggregate multi-waiter broadcast regression, using the
existing outer timeout instead of emulator self-reexec.

Eight alternating process pairs with identical retained harnesses validate two
million increments per process after warmup. The isolated candidate's median
paired cost ratio is 0.203; medians are 6.87 versus 33.72 ns per operation. This
measures one mutator and an uncontended native mutex without GC; it establishes
no application, contention, pause or latency budget. Archived sources, binaries,
hashes and raw samples are in `lock-safe-bench-artifacts/` under the evidence
root above. The applied method differs only in formatting and caller-contract
comments. The subsequent public-lock checkpoint passed every named suite:
1050 debug/1049 release macOS workspace executions, both native Linux workspace
profiles and explicit JIT matrices, both platform sanitizer suites, big-endian
checks and all 72 soak runs. Source identity covers 474 inputs. Evidence is
`target/gc-validation/2026-10-19-public-lock/all-gates/report.json` and its
companion verification record. This checkpoint predates the provenance repairs
above; it does not establish correctness of those later changes.

## Aggregate contention during concurrent copying — 2026-10-19

Fixed a slow-path ownership problem: an aggregate waiter acquired its stripe
inside a blocking region, then retained it while waiting for GC completion.
The waiter now waits only for availability, resumes RUNNING, reloads its rooted
receiver and tries the current stripe. Release wakes all availability waiters;
relocation may move subsequent acquisitions to another stripe, so a remaining
waiter cannot rely on a later release at the old address.

Five focused runtime checks pass, including a waiter awakened during an open
world pause, moved receiver/indirect RHS reloads, and four parked availability
waiters released together. Independent negative controls reject premature stripe
ownership and a single notification; source restoration and passing checks are
retained under `target/gc-validation/2026-10-19-resumed/`.

Generated integration now counts copying accesses only when no pause is
requested. Completion uses actual native AtomicI64 loads/stores from a RUNNING
owner, avoiding foreign polling that parks for the remainder of copying. The
shared holder is published through Atom. Temporary collector forcing and hold
hooks were removed. Twenty JIT repetitions (ten debug, ten release) passed all
160 required coverage rows. Every row recorded at least eight cycles and 6088
active writes; semispace rows recorded at least two old-alias writes and shared
rows at least one active reader snapshot. All 23 tooling tests pass. Broad
validation passed: 1047 debug/1046 release workspace executions on macOS,
1042 debug/1041 release on Linux, both explicit Linux JIT matrices and the
armed detectors. All 50 generated sanitizer runs passed on each platform;
414 macOS runtime sanitizer executions passed with no reports. All 473
source/archive entries and modes match, including remote before/after checks.
The private-copy ABI negative control is tied to matching retained source
archives; production codegen bytes are unchanged. Evidence:
`target/gc-validation/2026-10-19-resumed/aggregate-ffi-verification.json`.
Earlier failed gates remain retained. This checkpoint precedes the public
lock_safe fix above and does not close overall production readiness.

## Generated-code Linux sanitizer integration — 2026-10-18

AOT linking now honors `CC` as a single compiler executable path, defaults to
`cc`, and names the selected executable in errors. Missing compiler paths fail
as requested; paths containing spaces link and run. The regression passes in
macOS debug/release and Linux debug. This enables explicitly selecting installed
Clang 21 for generated-code sanitizer validation on `computer.jimmyhmiller.com`.
The previous hard-coded GCC 13 driver linked a sanitizer that failed startup with
an unexpected-memory-mapping diagnostic; this external toolchain observation is
retained in the project pad. No host configuration changed.

The new `linux-generated-tsan` release suite builds the compiler with LLVM 21,
rebuilds Rust runtime/std with instrumentation and an external Clang sanitizer,
and validates a shared corpus: 22 fixtures and three model-checked applications,
each with GC stress off/on. **All 50 Linux runs passed, with zero reports**.
The same shared harness passed all **50 macOS generated runs**. Its fixture list
and application expectations match the preceding archived harness exactly.
Missing/repeated program/stress combinations fail report validation. Linux uses
an isolated SSH snapshot; source bytes/modes match before and after execution.
The hosted macOS job uses the shared harness; hosted Linux generated-code CI
still needs the compiler dependency setup and is not claimed here.

Full local validation passed **1045 debug and 1044 release workspace tests**,
21 tooling tests, one armed detector, and 413 runtime sanitizer executions plus
the 50 generated macOS runs; zero failures. Filter totals overlap; ten/eleven
workspace ignores remain. All 472 source/archive entries and modes match in the
local and remote evidence. Runtime code is unchanged; diff check passes.
Evidence: `target/gc-validation/2026-10-18-linux-generated-tsan/verification.json`,
`remote/report.json`, `local-gates/report.json`, `linux-linker-test.log`.

Remaining operation/embedding/native-accounting and concurrent-publication audits,
long-duration resource/latency budgets, broader platform compiler validation and
hosted CI evidence remain open. Overall production readiness is unfinished.

## Native Linux ThreadSanitizer gate — 2026-10-18

`--suite linux-tsan` now instruments the complete Rust runtime and rebuilt
standard library on native x86-64 Linux using pinned `nightly-2026-06-24`.
From macOS it uses the verified SSH snapshot transport to Jimmy's existing
`computer.jimmyhmiller.com`; Linux CI runs directly. The release runner's `all`
suite and an independent CI job include it. Verification, evacuated-space poison,
frame pointers and `TSAN_OPTIONS=halt_on_error=1` are enabled. No suppressions or
host configuration changes were made.

Both worker settings (1/4) passed **527 runtime tests each**, with six intentional
ignores per run. Both separately selected corruption detectors passed exactly
one test: **1056 successful instrumented executions, zero failures and no
sanitizer reports**. The report requires all four matrix sections and rejects
missing/empty sections and unarmed detectors. All 20 tooling tests, actionlint and
shell syntax checks passed. All 470 source/archive entries and modes were verified
locally and remotely before/after execution. Only gate/CI tooling changed from the
native Linux checkpoint; compiler, runtime and their tests are unchanged.
Evidence: `target/gc-validation/2026-10-18-linux-tsan/verification.json`,
`remote/report.json`, `tooling/report.json`.

This covers Rust runtime execution and its instrumented standard library.
Generated compiler/JIT/native-code sanitizer integration remains unverified on
Linux and separately covered on macOS. Hosted CI, remaining operation/embedding
and native-accounting audits, concurrent publication, long-duration resource and
numeric latency/performance budgets remain open. Overall production readiness
is unfinished.

## Native Linux runtime gate — 2026-10-18

`--suite linux` now runs the full runtime on native Linux. From this macOS host it
uses Jimmy's existing `computer.jimmyhmiller.com` via SSH, transfers an isolated
source archive and retains its staging directory. Linux CI hosts run the same
native script directly. The unvalidated Docker Linux draft was replaced; the
existing s390x dependency preparer remains unchanged. Rust is pinned to installed
1.96.0; no existing remote checkout or host configuration is modified.

On x86-64 Linux, both worker settings (1/4) passed **528 debug and 527 release
runtime tests per setting**, with verification and evacuated-space poisoning.
All four explicit corruption-detector runs passed exactly one test, and each
profile passed nine doctests: **2132 successful executions, zero failures**.
Totals overlap; runtime ignores are five/debug and six/release, plus five ignored
documentation examples per profile. The report validates all ten matrix sections
and rejects missing/empty sections and unarmed detectors. All 19 tooling tests,
actionlint and shell syntax checks passed. All 469 source/archive entries and
modes were verified locally and on the remote before/after execution.

Poisoning exposed an invalid test assumption: the actual-collection barrier
regression read old aliases after their arena was reclaimed. It now reloads the
registered root across three collections, checking current-space membership,
both barriers and payload. Pre-reclamation acquire/publication coverage remains
separate. Production runtime code is unchanged; source comparison verifies this.
The first failed run and the intermediate gate's empty debug-only detector
selection are retained. The final gate uses `--include-ignored --exact` and checks
one detector execution per configuration.

Evidence: `target/gc-validation/2026-10-18-native-linux/verification.json`,
`remote-verified/report.json`, `tooling/report.json`. This is runtime-only Linux
evidence; compiler/JIT, Linux sanitizer, hosted CI, remaining operation/embedding
and native-accounting audits, concurrent publication and numeric resource/latency
budgets remain open. Overall production readiness is unfinished.

## Repeatable concurrent mutation soak — 2026-10-18

The release runner now provides `--suite soak`; `all` includes it. An independent
CI job retains the same evidence. The matrix has three trials of three nonzero
seeds, four worker settings (1/2/4/8) and both heap modes: 72 isolated runs of the
existing independent mutation ledger. Each run requires one exact passing test,
matching seed/mode, 256 concurrent cycles, at least 8192 allocations, mutation
during actual scanning, final graph/live-byte validation and generational minors.
Missing/duplicate summaries and incomplete coverage fail even after exit zero.
Each process group has at most 180 seconds, or a lower user-selected timeout.
Worker settings are policy limits, not proof of parallel participation per cycle.

Local execution passed **72 runs, 18,432 concurrent cycles, 816,749 allocations
and 12,459 minor collections**. All runs overlapped mutation with scanning
(minimum 32 recorded active iterations). All 18 tooling tests and actionlint
passed. Both source archives and current bytes/modes for 467 inputs were verified.
Only release-runner/tests/CI inputs changed from the reusable-SATB checkpoint;
runtime/compiler/test code is unchanged. The initial parser rejection of Rust's
same-line test-name prefix is retained; the corrected parser accepts the exact
known prefix and is regression-tested. Evidence:
`target/gc-validation/2026-10-18-repeatable-soak/verification.json`,
`soak-final/report.json`, `tooling/report.json`.

This establishes repeatable bounded randomized correctness coverage. Hosted CI
has not run; long-duration memory/RSS/latency budgets, wider platforms,
operation/embedding audits, remaining native accounting and concurrent publication
coverage remain open. Overall production readiness is unfinished.

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

## High-tag graph oracle and big-endian matrix — 2026-10-18

Four new graph-oracle cases use upper-byte `0xa5` tagged references, including
bit 63, across Compact/Full headers and major/promotion paths. Every case runs
16 seeds and six cycles with raw payload, exact live bytes, all edges, nulls,
immediates, cycles and alias identity checked independently of collector traversal.
Major paths alternate STW/concurrent collection; promotion paths verify minor
survivors before major reclamation. These are quiescent graph tests, not concurrent
publication evidence.

All **12 graph cases passed in debug, release and rebuilt ThreadSanitizer**.
The full sanitizer gate before the final emulation-only harness adjustment passed
50 generated native and 410 runtime executions. The expanded big-endian gate
passed **716 executions**, including all 12 graph cases in each of four
profile/worker combinations. Source/archive bytes and modes for 467 inputs and
per-test coverage were verified; diff check passed. Production runtime behavior
is unchanged from the preceding fully validated forwarding checkpoint.

The initial emulated run exposed a harness re-execution failure (child exit 127,
no graph execution). Emulated invocations now explicitly use in-process graph
execution under the gate's 180-second timeout. Native/sanitizer defaults retain
per-test 30-second child deadlines. Failed evidence remains retained.
Evidence: `target/gc-validation/2026-10-18-high-tag-graph/verification.json`,
`focused-final.json`, `tsan/report.json`, `big-endian-fixed/report.json`.

Concurrent publication, complete operation/embedding audits, remaining native
accounting, hosted/platform/soak evidence and numeric performance budgets remain
open. Overall production readiness is unfinished.

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


## Generated nursery/concurrent-major integration — 2026-10-18

The generated-access regression now runs both semispace and generational heaps,
with a 1 MiB nursery and 32 MiB tenured space in generational modes. Both single
and shared workloads run optimized and unoptimized. Generational coverage requires
a successful nonparking aggregate replacement during concurrent copying on an
old owner with nursery references, followed by a strictly later minor collection.
The observer reads reference offsets/values without retaining managed pointers or
adding roots, delegates to production try-write, and records the minor count
before any safepoint. Existing exact snapshot/scalar/live-graph checks and shared
participant coverage remain. Semispace single-mutator published-alias coverage
remains mandatory; no generational published-alias claim is added.

An initial generational draft waited indefinitely for an alias condition that
this source did not reliably produce, despite many successful old-to-young writes
and later minors. Those failed runs and a native sample are preserved. The
coordinator now expresses the distinct generational requirements and stops
issuing majors once major-copy coverage is satisfied while allocation pressure
triggers the required later minor. The 60-second whole-workload bound remains.
This is integration coverage, not an isolated proof of a particular card/SATB
barrier: final major rescanning can also preserve edges; deterministic runtime
barrier tests remain separate evidence.

Fresh validation: **241 focused compiler/concurrency/root/thread-completion
passes in debug and 241 in release**, zero failures or ignores. Ten release
repetitions cover eighty code/heap modes. All forty generational modes completed
active old-to-young writes and 1–4 subsequent minors; their observed major-cycle
counts were 8–206 and young-edge writes 1–94,751. These are coverage observations,
not throughput, fairness, pause or RSS budgets. All 466 source/archive entries
and modes match; diff check passes. Source comparison confirms only the existing
regression body changed. Production compiler/runtime inputs are unchanged;
sanitizer/big-endian gates were not repeated and do not instrument this JIT test.

Final evidence: `target/gc-validation/2026-10-18-generated-nursery-major/`
(`report.json`, `scope.json`, `verification.json`, repetition/profile logs).
Development diagnostics: `target/gc-validation/2026-10-18-generational-development/`.

The aggregate nursery/major integration gap is now exercised. Other operations,
forced claim/publication transitions, embedding, total native accounting,
platform/soak coverage and numerical performance budgets remain open. Overall
production readiness is unfinished.


## Multiple generated mutators during concurrent copying — 2026-10-18

The bounded generated-code regression now includes two writers and a dedicated
reader sharing reference-bearing aggregate snapshots and a scalar slot. A live
120,000-reference graph is published read-only, checked during reading and fully
validated after all joins. Test-only ABI observers delegate to production access
operations, distinguish participant Thread addresses without dereferencing foreign
Thread state, and require successful active-copy writes from both writers plus
active-copy snapshots from the dedicated reader. The single-mutator forwarded-
alias case remains and now uses the same graph size. Runtime child completion is
drained before retiring the JIT engine.

The test coordinator waits for initialized managed work and all shared
participants, runs at least eight cycles until required coverage is observed,
and permits managed-operation progress between cycles. Completion is polled
between 1024-operation chunks because the foreign polling ABI parks. Data checks
remain per iteration, and the whole child process retains its 60-second bound.
Earlier small/fixed-cycle drafts missed participant scheduling. Full-release
runs also exposed missing single-mutator alias coverage and a timeout with the
mutator repeatedly waiting in ai_ffi_leave while collections kept running. The
failed reports and a native stack sample remain archived; final coverage-driven
coordination and chunked completion polling pass.

Fresh validation: **241 compiler/concurrency/root/thread-completion passes in each
of debug and release**, zero failures or ignores, plus ten release repetitions
covering forty generated modes. All twenty shared modes proved both writers and
the reader active during copying; they needed 8–37 cycles. Work varies with
scheduling: shared modes completed 2–41,463 active writes and 1–39,900 dedicated
reader snapshots. These are coverage counts, not throughput or fairness budgets.
All 466 source/archive entries and modes match; diff check passes. Comparison
with the prior archive confirms only this test body and its fixture changed;
production compiler/runtime inputs are unchanged. Sanitizer and big-endian gates
were not repeated and do not instrument this new JIT workload.

Final evidence: `target/gc-validation/2026-10-18-generated-shared-chunks/`
(`report.json`, `scope.json`, `verification.json`, repetition/profile logs).
Earlier failed runs remain under `generated-shared`, `generated-shared-final`,
and `generated-shared-verified`; the latter includes `stalled-child.sample`.

This is semispace concurrent integration with explicit test Sync assertions.
It does not close nursery/major interaction, every runtime operation, forced
claim/publication transitions, embedding, total native accounting, platform/soak
coverage or numerical performance budgets. Production readiness remains unfinished.


## Generated access through published old aliases — 2026-10-18

The active-copy compiler fixture now reloads its holder through a heap edge.
Concurrent discovery defers healing that edge until final STW, so generated
operations receive old bases whose forwarding destinations are already published.
The test-only observer requires successful production try-writes through such
aliases in both optimized and unoptimized code. Exact snapshot/scalar checks and
the live 12,000-reference array remain. Ten release repetitions (twenty modes)
completed eight cycles and 1,200–1,568 old-alias writes per mode.

Production fault controls independently store through the incoming old base or
read the snapshot from it, bypassing canonical storage. Both now fail with model
result 81 instead of 42. Each source mutation was restored; repeated release and
focused debug/release checks pass. This strengthens the previous fixture, whose
stale-alias fault was not detected. It proves published-alias use and correct
canonical reads/writes, not a forced generated-code rendezvous during the
claim-to-publication transition or complete multi-mutator coverage.

Fresh validation: **213 compiler/managed-memory/operand-root passes in each of
debug and release**, zero failures or ignores. All 465 source/archive entries and
modes were verified. Comparison with the preceding full-validation source archive
confirms the only source change is inside the existing regression body; production
compiler and runtime code are unchanged. The preceding full suite remains the
baseline evidence, not a newly repeated sanitizer/big-endian result. Diff check
passes. Evidence: `target/gc-validation/2026-10-18-generated-old-alias/`, including
`report.json`, `scope.json`, and `verification.json`; fault/repetition logs are in
`target/gc-validation/2026-10-18-generated-old-alias-fault/`.

No performance budget is established. Forced transition rendezvous, multi-mutator
active-copy integration, remaining embedding/native accounting, platform/soak
coverage and numerical budgets remain open. Production readiness is unfinished.


## Generated aggregate operations during concurrent copying — 2026-10-18

A bounded compiler regression now executes generated scalar and reference-bearing
aggregate accesses alongside a separate concurrent collector. A live array of
12,000 references checks preservation across relocation; exact snapshot values
and scalar writes are checked against the iteration model. Both optimized and
unoptimized code must span eight concurrent cycles. A test-only ABI observer
calls the production nonparking try-write and requires successful replacements
while copying is active. Ten repeated release runs (twenty generated-code modes)
observed 21,542–26,275 active-copy writes per mode. Production/test JIT contexts
now share the runtime-symbol mapping helper.

A fixed-workload version passed debug but finished after seven cycles in the
first complete release run. An explicit native completion handshake now keeps
work/checks running until all eight cycles finish, instead of assuming scheduling
from a loop count. An initial sum-only oracle was strengthened to exact values.
A fault falsely acknowledging active-copy stores without writing fails the final
regression; restored source passes. An earlier stale-alias control was **not**
detected and remains archived: deterministic old/new alias transitions are not
proved by this fixture. It covers one generated mutator and a concurrent collector,
not the complete multi-mutator or embedding audit. The new JIT regression itself
is not run under the separate ThreadSanitizer harness.

Final local validation: **1033 debug and 1032 release workspace passes**, one
armed detector, 15 tooling tests, 50 generated native and 402 runtime sanitizer
executions, and 652 big-endian executions; zero failures. Filter/suite totals
overlap. Debug/release retain ten/eleven ignored tests, with the detector separately
armed. All 465 source/archive entries and modes were verified; diff check passes.
Final evidence: `target/gc-validation/2026-10-18-generated-concurrent-final/`.
Fault/repetition evidence: `target/gc-validation/2026-10-18-generated-concurrent-fault/`.
The earlier failed full run remains at `target/gc-validation/2026-10-18-generated-concurrent/`.

No throughput, pause, tail-latency or RSS budget is established by this correctness
workload. Remaining alias-transition/multi-mutator/embedding audits, total native
accounting, hosted CI/platform/soak coverage and numerical budgets remain open.
Production readiness remains unfinished.


## Native buffer retirement-node accounting — 2026-10-18

Deferred buffer payloads were charged to native pressure, but their separately
allocated intrusive retirement nodes were omitted. Nodes now use allocator-api2
boxes with the buffer owner's metadata allocator. The allocator retains the
independent counter through physical deallocation, including native-worker
ownership after queue detachment. Buffer-byte metrics retain their existing
scope. Nodes are still reserved in native construction; publication remains
allocation-free. This adds accurate metadata demand rather than a memory cap.

A paused-consumer regression checks exact requested node bytes while reserved
and detached, then zero after destruction; synchronous explicit release is also
covered. Temporarily replacing the node allocator with an unaccounted allocator
makes this regression fail. Source restoration and the passing regression were
verified afterward. Existing allocation-free publication checks also pass.

Full local validation: **1032 debug and 1031 release workspace passes**, one
armed detector, 15 tooling tests, 50 generated native and 402 runtime sanitizer
executions, and 652 big-endian executions. Zero failures; filters/suites overlap.
Ten/eleven debug/release ignores remain, with the detector run separately.
All 465 source entries and archived bytes/modes match after the fault control.
Evidence: `target/gc-validation/2026-10-18-retirement-node-accounting/`, including
`report.json`, `verification.json`, and `fault-control.json`. Diff check passes.

No fresh throughput/RSS benchmark or universal performance claim accompanies
this accounting correction. Shared Arc/control blocks, other native accounting,
remaining concurrent/embedding audits, wider platforms, soaks and numerical
budgets remain open; overall production readiness remains unfinished.


## Repeatable release validation and CI — 2026-10-18

Added `scripts/check_gc_release.py` and `.github/workflows/gc-validation.yml`.
One command runs the complete debug/release workspace, separately armed detector,
generated/runtime ThreadSanitizer checks, and selected s390x big-endian checks.
The runner retains command logs, tool/environment details, a source archive and
hashes; rejects empty results; excludes concurrent runner instances per checkout;
and terminates timed-out command groups. Fifteen Python tooling tests pass,
including archive integrity, source changes, lock contention, failure retention,
and termination of a descendant that ignores SIGTERM. Application sources are
included in the archive. See [usage and scope](release-validation.md).

The local `all` suite passed: **1031 debug workspace passes, 1030 release workspace
passes, one armed detector, 50 generated native sanitizer executions, 401 runtime
sanitizer executions, and 648 big-endian executions**, with zero failures. Counts
across filters/suites overlap. Debug/release retain ten/eleven ignored tests;
the release-only ignored detector is run separately with verification armed.
All 465 source entries and archive bytes/modes were reverified after completion.
Workflow lint and diff checks pass. Evidence:
`target/gc-validation/2026-10-18-release-gate/report.json` and `verification.json`.

Hosted CI has not run. The local sanitizer toolchain used the installed `nightly`
alias whose manifest date is 2026-03-12; CI selects that dated nightly explicitly.
This checkpoint adds repeatable validation, not a new performance result or a
claim of overall production readiness. Remaining audits, platform coverage,
long-running workloads and numerical performance budgets remain open.


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

## Immutable reference-enum access audit — 2026-10-18

Reference-enum destructuring reads immutable payload bytes, including leading
reference slots and references restored into nested value payloads. Inspection
of `Heap::process_slot_concurrent` confirms that concurrent discovery only reads
parent slots and copies children; parent-slot rewrites are deferred to the final
stopped-world root and to-space passes. Source payload storage remains valid
until that pause. These immutable reads therefore do not establish a concurrent
payload-write race, provided the owning mutator follows rooting/safepoint rules.
This exception does not apply to mutable struct, array, Atom or aggregate slots.

A draft routing enum references through phase-checked scoped loads was removed:
its regression enforced an emission strategy without reproducing wrong behavior.
The added phase/lock overhead was not justified by the audited protocol. Compiler
source was restored only after an exact byte comparison with the preceding
archive. All 119 runtime/gate and 161 application source pins and build hashes
from the preceding passing checkpoint match again. Draft tests, fault controls,
sanitizer logs and timing observations remain in
`target/gc-validation/2026-10-18-enum-access/`, explicitly marked as superseded.
They are not claimed as validation of a production change.

Concurrent integration still requires the remaining mutable access, identity,
publication and embedding inventory. This audit closes only the proposed
immutable enum-read issue, not overall concurrent or production readiness.

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

## Zeroed metadata allocation path — 2026-10-18

The metadata allocator now preserves the base allocator's zeroed-allocation
implementation instead of falling back to allocate-and-clear. Both allocation
paths share checked requested-byte accounting and cleanup on counter overflow.
Grow/shrink behavior is unchanged. No application speedup is claimed.

Eight focused allocator tests pass. The new regression checks base delegation,
alignment, zero contents, exact charges, overflow cleanup and allocation failure;
removing delegation makes it fail. Optimized runtime verification passes 508 unit
tests and nine doctests. The full workspace passes **1022 tests, zero failures**.
Source pins match; evidence is in
`target/gc-validation/2026-10-18-zeroed-metadata/report.json`.
Earlier platform/race/application measurements predate this change. Native record
accounting, within-execution pressure adaptation, concurrent-mode integration and
broader production-readiness work remain open.

## Global and handoff root storage accounting — 2026-10-18

Heap-owned global root words and handoff vacancy indices now use the requested-byte
registry ledger. Staging preserves accounting ownership; incompatible replacement
storage is rejected before any move. Charges include staged/obsolete buffers until
physical free. Global and handoff backing survives execution shutdown, then releases
with the heap. Teardown tests compare the exact retained-root total rather than
mistaking legitimate heap-lifetime backing for leaked execution tables.

Three new references cover full/generational handoff allocation traffic, retained
root backing, pressure participation, independent heap lifetime and foreign stages.
The strict probe initializes its cfg(test) hook mutex before measuring the two
production allocations. Four deliberate faults fail.

Final verification: **1021 workspace passes, zero failures, ten ignored**;
507 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, 616 big-endian executions, 50 generated native and 393 runtime
ThreadSanitizer executions. Platform/race filters overlap. All 119 runtime/gate
and 161 application source pins match, along with measured binaries, compiler,
runtime library, baseline report and fixture hashes. Diff check passes. No new
whole-runtime Miri claim is made.
All 480 original application runs and 64 follow-up runs are correct. Original
cohort wall ratios span 0.904–1.089; peak-RSS ratios span 0.980–1.009. The slowest
cohort's isolated 31-pair follow-up has a median paired wall ratio of 1.0215 and
peak-RSS ratio 0.9994. Both observations are retained; neither establishes zero
cost or numerical budgets. See GC_PERFORMANCE.md for scope.
Evidence: `target/gc-validation/2026-10-18-root-storage/report.json`.

Record/control and other metadata accounting, remaining embedding/concurrent
audit, pressure coverage, broader platforms/soaks and numerical budgets remain
open. Overall production readiness remains unfinished.

## Permanent root publication and accounting — 2026-10-18

Permanent-source vector growth now publishes a pinned temporary source before
parking all originally RUNNING registrations. Replacement backing is allocated
outside collector/registry locks. An allocation-free commit transfers the source
to permanent publication under collector exclusion, avoiding duplicate enumeration.
Obsolete/unused backing is freed while callers remain BLOCKED, then registrations
restore together under exclusion. Spare-capacity publication stays RUNNING and
allocates nothing. Permanent-source backing now participates in registry accounting
through physical free at heap release, including staged/obsolete storage.

Four references cover commit/fast-path allocation behavior, stale/foreign stages,
exact root enumeration, moving roots during allocation/disposal, nested caller
restoration, backing accounting and snapshot history. Seven deliberate faults
fail. Two older tests now declare permanent-source storage before the heap to
honor the API lifetime contract. Frame publication and pointer reload requirements
are documented.

Final verification: **1018 workspace passes, zero failures, ten ignored**;
504 optimized runtime units plus nine docs, armed detector, 28 optimized
integrations including reflection, 604 big-endian executions, 50 generated native
and 390 runtime ThreadSanitizer executions. Platform/race filters overlap.
All 120 runtime/gate/changed-test and 161 application source pins match, along
with measured binaries, compiler, runtime library and baseline report hashes.
Diff check passes. No new whole-runtime Miri claim is made.
All 480 application runs are correct. Observed cohort wall ratios span
0.958–1.044; peak-RSS ratios span 0.992–1.012. These do not establish numerical
budgets or universal improvements. See GC_PERFORMANCE.md for scope.
Evidence: `target/gc-validation/2026-10-18-permanent-roots/report.json`.

Full record/control and other metadata accounting, remaining embedding/concurrent
audit, pressure-policy coverage, broader platforms/soaks and numerical budgets
remain open. Overall production readiness remains unfinished.

## Native pressure policy across executions — 2026-10-18

A reused runtime could retain a pressure threshold sized for a previous
execution's released native buffers. A real FFI-spike/re-entry regression fails
before the fix. Shutdown now lowers obsolete headroom after native retirement
drains, using current buffer/registry demand under the pressure-service mutex.
It never raises the trigger without a collection, preserving outstanding pressure
and old-epoch drain behavior. The minimum-threshold path avoids the service lock.
Collection counts and peak metrics retain their original meaning.

Three references cover full/generational re-entry after a released 8 MiB spike,
retained demand and unchanged metrics, and delayed physical release with a
competing collection. Five deliberate faults fail, including early retuning and
forgiving unserviced demand. The latter was caught by the existing closed-epoch
pressure regression; the policy was corrected rather than weakening that test.
Final verification: **1014 workspace passes, zero failures, ten ignored**;
500 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, 588 big-endian executions, 50 generated native and 386 runtime
ThreadSanitizer executions. Platform/race filters overlap. All 117 runtime/gate
and 159 application source pins match, along with measured binaries, compiler,
runtime library and baseline report hashes. Diff check passes. No new whole-runtime
Miri claim is made.
All 480 paired application runs are correct. Observed cohort wall ratios span
0.919–1.042; peak-RSS ratios span 0.983–1.018. These do not establish numerical
budgets or universal speedups. See GC_PERFORMANCE.md for scope.
Evidence: `target/gc-validation/2026-10-18-pressure-epochs/report.json`.

Complete native accounting, remaining embedding/concurrent audit, pressure-policy
coverage within long executions, broader platforms/soaks and numerical budgets
remain open. Overall production readiness remains unfinished.

## Off-lock native registry teardown — 2026-10-18

Execution shutdown now prepares empty atomic/channel/completion registries with
the heap's existing accounting owner, detaches contents with allocation-free
swaps under brief collector exclusion, waiting for every late forwarding write.
Record retirement and backing release follow outside registry/coordinator locks.
Caller registrations stay BLOCKED through disposal and the native retirement
drain. Repeated finish and later runtime entry preserve accounting and prepare
fresh retirement epochs.

Three references cover detachment without allocation or freeing, waiting through
collector exclusion, and a paused disposal interval that permits another
collection while all registries are unlocked.
They verify retained backing charges, moving global roots, record reclamation,
repeated finish and re-entry in full/generational heaps. Four deliberate faults
fail: RUNNING disposal, locked disposal, lost replacement accounting ownership,
and missing collector exclusion.
Final validation: **1011 workspace passes, zero failures, ten ignored**;
497 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, 576 big-endian executions, 50 generated native and 383 runtime
ThreadSanitizer executions. Platform/race filters overlap. All 117 runtime/gate
and 159 application source pins match, along with measured binaries, compiler,
runtime library and baseline report hashes. Diff check passes.
All 480 corrected-implementation application runs are correct; observed cohort
wall ratios span 0.933–1.039 and peak-RSS ratios 0.977–1.014. These do not establish
numerical budgets or universal speedups. See GC_PERFORMANCE.md for scope.
The superseded draft lacked collector exclusion and is separately archived with
relocation notes; its green narrow checks are not final lifetime evidence.
Final evidence: `target/gc-validation/2026-10-18-registry-teardown/report.json`.

Record/control accounting, other native metadata, remaining embedding/concurrent
coordination audit, wider platforms/soaks and numerical performance budgets
remain unfinished.

## Allocation-free scoped extra roots — 2026-10-18

Scoped root registrations now borrow pinned stack descriptors through an
intrusive list instead of allocating descriptors and vector backing. Source
lifetimes remain tied to registration guards. The list mutex excludes enumeration
and unlink; collector exclusion still protects borrowed slot addresses through
late parallel forwarding writes. Sources stay published during native maintenance.
Mutator removal retains its RUNNING protection. No metadata allocation or disposal
is needed during registration/removal, including under collector exclusion.

Four registry-core references cover arbitrary unlink/reuse, empty/unwind cleanup,
forwarded slot updates and cross-thread publication. An allocation probe verifies
1024 Heap registration/scan/removal cycles with zero allocations/frees. Four core
references pass both isolated Miri modes; this is not whole-runtime Miri.
Final validation: **1008 workspace passes, zero failures, ten ignored**;
494 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, 564 big-endian executions, 50 generated native and 380 runtime
ThreadSanitizer executions. Platform/race filters overlap. Four isolated registry
references pass both Miri modes. Three deliberate faults fail. All 115 runtime/gate
and 157 application source pins match, along with measured binaries, compiler,
runtime library and baseline report hashes. Diff check passes.
All 480 paired application runs are correct. Observed cohort wall ratios span
0.920–1.034; peak-RSS ratios span 0.984–1.016. These do not establish performance
budgets or a universal speedup. See GC_PERFORMANCE.md for scope.
Evidence: `target/gc-validation/2026-10-18-scoped-roots/report.json`.

Native record/control accounting, other native metadata, teardown storage
coordination, the remaining concurrent audit, broader platforms/soaks and numerical
performance budgets remain unfinished.

## Registry-driven native pressure — 2026-10-18

Native pressure decisions and post-collection thresholds now combine buffer and
registry backing bytes. Buffer/registry metrics retain separate scopes. Atomic
construction and thread spawn service accumulated pressure; spawn publishes its
environment handoff root before collection. Channel and FFI allocations use the
same combined pressure calculation. Registry backing is charged at allocation;
the next native constructor services accumulated demand. This is a growth trigger,
not a hard memory limit.

Three new regressions cover registry-only atomic churn in full/generational heaps,
closure handoff across pressure GC, and combined-ledger threshold adaptation.
A compaction fixture now prepares accounted backing without invoking constructor
service before arming its maintenance hook. Record/control allocation accounting
and other native metadata remain open.

Final validation: **1003 workspace passes, zero failures, ten ignored**;
489 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, 544 big-endian executions, 50 generated native and 375 runtime
ThreadSanitizer executions. Platform/race filters overlap. Three deliberate
faults fail. All 113 runtime/gate and 155 application source pins match, as do
measured binaries, compiler, library and baseline report hashes. Diff check passes.
All 480 paired application runs are correct; observed cohort wall ratios span
0.964–1.043 and peak-RSS ratios 0.994–1.006. See GC_PERFORMANCE.md for scope.
Evidence: `target/gc-validation/2026-10-18-registry-pressure/report.json`.

## Registry backing accounting — 2026-10-18

Atomic, channel and completion table/vector backing is now accounted at the
allocator boundary, through physical freeing and across staged growth/compaction.
Commits reject storage prepared for another heap's counter. Allocation failure,
overflow, retained backing, concurrent updates and heap-independent lifetime have
regression coverage. New registry metrics remain separate from buffer metrics.
Record/control allocations, other metadata and native-pressure integration remain
unfinished.

Final verification: **1000 workspace passes, zero failures, ten ignored**;
486 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, 532 big-endian test executions, 50 generated native plus 372 runtime
ThreadSanitizer executions. Platform/race filters overlap. Seven isolated
allocator references pass both Miri modes; this is not whole-runtime Miri.
Four deliberate fault controls fail. All 112 runtime/gate and 155 application
source pins match; diff check passes. All 480 paired application runs are correct;
observed wall ratios span 0.956–1.055, peak-RSS ratios 0.996–1.037. These do not
establish production budgets. See GC_PERFORMANCE.md for scope and costs.
Evidence: `target/gc-validation/2026-10-18-registry-accounting/report.json`.

## Component-specific native admission — 2026-10-18

Atomic, channel and completion publication now requests backing only for exhausted
components. Commit checks per-component liveness budgets and preflights every
required component before moving ownership. It replaces only components still
needing growth, preserving other storage, native record identity and pending
order/positions. Owner reservation follows total records even when no raw handle
has bound yet. Allocation and old/unused storage disposal remain off-lock/BLOCKED.

Seventeen references cover individual components, untouched storage addresses,
allocation-free commits/publication/future binding, raw histories, incomplete
stages, independent stale budgets and pending order. Six deliberate controls fail.
Final verification: **986 workspace passes, zero failures, ten ignored**;
472 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, worker-one/four reference rechecks, 476 big-endian executions and
50 generated native plus 357 runtime ThreadSanitizer executions. Filters overlap;
the dedicated profile is ignored during ordinary tests. No new whole-runtime
Miri claim is made. All 106 source/gate pins, 150 benchmark source pins and final
executable/library hashes match; diff check passes.

The shared native profile measures production publication with prepared inputs,
excluding constructors/OS-thread creation/setup/teardown. It completes 126 paired
process executions and 1008 measured publications. At 32768 existing records,
raw-channel membership admission changes from about 1.69 ms to 0.69 µs, and
completion pending admission from about 0.83 ms to 0.27 µs. Atomic owner growth
allocates one backing vector rather than two tables. GC_PERFORMANCE.md retains
all cases, allocation bytes, scope and limitations. These are isolated admission
improvements, not whole-program speedups. All 480 application runs are correct;
median paired cohort wall ratios span 0.983–1.045. Evidence:
`target/gc-validation/2026-10-18-component-admission/report.json`.

Still required: full native metadata accounting/admission, remaining embedding
and native coordination (including scoped-root descriptors and teardown table
disposal), concurrent-mode audit, broader platform/soak coverage and numerical
startup/throughput/pause/tail/RSS budgets. Overall production readiness remains
unfinished.

## Off-pause registry compaction and terminal joins — 2026-10-18

Completion, atomic and channel weak sweeps now remove ownership without shrinking
native backing. Sparse replacement storage is allocated outside collector
exclusion and registry locks. Commits recheck current liveness, stale-request
budgets and future binding capacity, move entries without allocation, and release
obsolete/unused storage after unlock. Mutator callers remain BLOCKED through
maintenance and restore together under the coordinator. Commits still briefly
serialize with GC on registry mutexes.

Explicit extra-root sources remain published while maintenance allows another
collection; unregistering borrowed storage waits for collector exclusion and all
late root writes. Regressions pause allocation/disposal and run another GC across
major/minor, STW, concurrent, native-pressure and extra-root paths, checking roots
and nested registration states. Five deliberate faults fail.

Optimized tests exposed transient retention of already-joined completion records
when a native observer held their state mutex. A monotone release/acquire flag now
publishes full OS/TLS join completion separately from body-exit notices. Terminal
GC/reaper probes need no state mutex; active joins still defer. The deterministic
held-state regression is red before the fix, and the original churn case passes
20 optimized repetitions. Native observers retain their own record ownership.

Final verification: **969 workspace passes, zero failures, nine ignored**;
455 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, worker-one/four rechecks, 408 big-endian executions and 50 generated
native plus 335 runtime ThreadSanitizer executions. Filters overlap. All 102
source/gate pins and 146 benchmark source pins, libraries and measured binaries
match. A manifest correction excludes a growing log from executable hashes;
original metadata and all raw measurements remain retained. Diff check passes.

All 480 final paired application runs are correct; cohort wall ratios span
0.888–1.025. Identical-source native profiles separate stopped-world telemetry,
GC-call latency and full teardown. They retain small costs as well as gains;
GC_PERFORMANCE.md gives observations, scope and archived intermediate versions.
Measured heap/completion sizes remain 4496/128 bytes on this target. Evidence:
`target/gc-validation/2026-10-18-registry-compaction/report.json`.

Still required: full native metadata accounting/admission, independent component
admission work, remaining embedding/native coordination (including scoped-extra
descriptor allocation/retirement and teardown table disposal), concurrent-mode audit, broader platform/soak
coverage and numerical performance budgets. Overall production readiness is
unfinished.

## Deferred completion-record destruction — 2026-10-18

Completed native thread records now transfer through prepared typed links to the
existing retirement service. GC immediately removes registry/pending/catalogue
ownership, then the native worker destroys record and registration metadata
outside collector and registry locks. Shutdown drains completion jobs through the
same global activity used by buffers, cells and channel controls. Admission
prepares ownership while BLOCKED, before publication; test fixtures use their
owning heap's service so teardown verifies the real drain contract.

Two regressions cover zero allocation/deallocation during non-compacting transfer,
deferred physical destruction, retained scalar observation and another collection
while the native destructor is paused. The real collection check runs in
semi-space and generational subprocesses. Restoring inline destruction fails the
reference. Tests distinguish immediate registry removal from native destruction.

Final correctness verification: 963 workspace passes, zero failures, nine ignored;
449 optimized runtime units plus nine docs, armed detector and 12 optimized
integrations. Broad gates pass 388 big-endian executions and 50 generated native
plus 329 runtime ThreadSanitizer executions. After fixture-only corrections, final
completion coverage passes 20 big-endian references and 40 ThreadSanitizer tests.
Overlapping filters repeat cases. All 480 paired application runs are correct;
median paired cohort wall ratios span 0.954–1.085. All source and benchmark
artifact hashes match. Costs and measurement scope are in GC_PERFORMANCE.md.
Evidence is retained in
`target/gc-validation/2026-10-18-completion-retirement/`.

Still open: off-pause registry compaction, full native metadata accounting and
admission, remaining embedding coordination, concurrent-mode audit, broader
platform/soak coverage and numerical performance budgets. Completion-specific
performance costs are tracked separately. Overall production readiness remains
unfinished.

## Cross-epoch native pressure correction — 2026-10-18

Confirmed and fixed: service drain used only the current worker, ignoring old
published work retained after shutdown. Shared pending activity now covers all
worker epochs and nested/foreign publication. Pressure drains old garbage before
collection decisions, then checks global idle again after reacquiring coordinator
exclusion. Threshold retuning excludes publication; non-idle retries wait without
pinning the collector. Preparation remains BLOCKED and outside collector locks.

The old regression is red before the fix. Heap tests cover major/minor modes,
continued collection during an old-worker wait and avoiding a redundant native
pressure collection. References cover nested publication, first-epoch identity,
idle snapshot exclusion and zero prepared allocator traffic. Four deliberate
faults fail. Exact activity code passes three isolated Miri references under
default and explicit tree-borrows; this is not whole-runtime Miri. Optimized
runtime checks pass. Final verification: 961 workspace tests with zero failures
and nine ignored; 447 optimized runtime units plus nine docs, armed detector,
12 optimized integrations, 384 big-endian executions and 50 generated native
plus 327 runtime ThreadSanitizer executions. All final source/artifact hashes
match. Measurements and costs are retained in GC_PERFORMANCE.md. Evidence:
`target/gc-validation/2026-10-18-retirement-epochs/report.json`.

Completion-record destruction, registry compaction, full metadata accounting,
remaining embedding coordination, concurrent-mode audit, platform/soak coverage
and numerical budgets remain open. Overall production readiness is unfinished.

## Deferred channel control destruction — 2026-10-18

Channel metadata and inline backing now retire through the same typed native
ownership core as atomic cells. GC removal/publication allocates no retirement
nodes. The consumer unlinks records before destruction, releases same-worker
backing under the existing pending jobs, and preserves foreign-epoch publication.
Retained native observers remain valid until their final release.

References check allocator traffic, mixed typed mailboxes, pending-job coverage
and foreign epochs. A paused metadata destructor leaves the registry/coordinator
available for another GC. Collector snapshots drop before weak sweeping. Four
deliberate faults fail, and the exact shared core passes isolated Miri references
under default/explicit tree-borrows, including cross-thread late publication.
This is not whole-runtime Miri. Final verification passes 954 workspace tests,
440 optimized runtime units plus nine docs, armed detector, 12 optimized
integrations, 360 big-endian executions, and 50 generated native plus 318 runtime
ThreadSanitizer executions. All final source/artifact hashes match. Measurements
and costs are in GC_PERFORMANCE.md; evidence is
`target/gc-validation/2026-10-18-channel-retirement/report.json`.

Completion-record destruction, off-pause registry compaction, full metadata
accounting/admission, remaining embedding coordination, concurrent-mode audit,
platform/soak coverage and numerical budgets remain open. Overall production
readiness remains unfinished.

## Deferred atomic record destruction — 2026-10-18

GC removes dead atomic cells from their owner index immediately and publishes
native-only ownership batches without allocating retirement nodes. The existing
retirement worker consumes them off-lock and outside collector execution. A
shared producer source covers each worker epoch; cancellation, old/new epoch
handoffs, observers and buffer/cell draining have regression coverage. Tests
pause native destruction and prove another collection still completes. Internal
Weak assertions now distinguish registry removal from asynchronous destruction.

The isolated ownership core passes Miri under default and tree-borrows modes;
this is not whole-runtime Miri. Optimized runtime, detector, big-endian and
ThreadSanitizer checks pass. A test-only drain-contract adjustment is covered by
full optimized runtime and targeted ThreadSanitizer rechecks; broad gates cover
unchanged production code. Workspace verification passes 949 tests with zero
failures and nine ignored. Evidence is in
`target/gc-validation/2026-10-18-atomic-retirement/report.json`.

Bulk collection pauses improve in the measured atomic churn profiles, with
higher constructor, storage and teardown costs; see GC_PERFORMANCE.md for all
ratios, including small workloads and the application outlier. Completion and
channel control destruction, registry shrinking, full native metadata accounting,
concurrent-mode integration audit, platform/soak coverage and numerical budgets
remain open. Overall production readiness is unfinished.

## Allocation-free collection caller parking — 2026-10-18

Collection, inspection, registration and retirement entries suspend current
RUNNING registrations without allocating temporary vectors. Scoped intrusive
ownership preserves prior BLOCKED states, handles nested admission and unwind,
and restores iteratively. Tests check allocator traffic, Arc retention and all
17 sole scratch roots through a pending major/minor collection with one/four
workers. This removes native allocator work before coordinator acquisition;
GC-time record destruction and registry shrinking remain open, along with full
native metadata accounting and quantitative performance budgets.

## Cooperative handoff storage — 2026-10-18

Handoff admission reuses prepared slots without allocating. When more storage is
needed, an inline shadow frame roots the environment while native root/pool
buffers are allocated BLOCKED outside their locks. RUNNING commit moves atomic
root words and vacant indices into reserved storage, preserving values and
indices. Obsolete buffers are disposed BLOCKED before claiming a vacancy or
publishing a new handoff. Capacity is rechecked after collection or competing
admission, including insufficient and no-longer-needed stages.

The pool tracks all allocated handoff slots separately from current vacancies.
Every new slot reserves its future return, so successful scratch transfer and
failed-admission cleanup return indices without growing the pool. Pool and
global backing mutex backends are initialized before publication. Scratch roots
remain fixed storage; the slow-path inline frame consumes no scratch-stack slot.

References check allocation-free transfer/return/reuse, value/index preservation,
reservation and prepared mutexes. Native phase tests make the temporary frame
the environment's only root, force major/minor collection during allocation and
real backing disposal, and verify address relocation, value, counters and slot
return. Competing admission makes the first planned stage too small. Fault
controls and final measurements/platform gates are recorded in the checkpoint
report: `target/gc-validation/2026-10-18-handoff-storage/`.

Remaining native work includes off-pause record/registry cleanup, complete native
metadata accounting/admission, broader platform/soak coverage and numerical
performance budgets. General embedding/root API callers still own their native
allocation and collection coordination contracts.

## Handoff root retirement excludes evacuation — 2026-10-18

A failed or uncalled child can drop its handoff before registering a mutator.
The previous cleanup cleared its global slot under AtomicRootSet's backing
mutex and immediately advertised the vacant index. Parallel evacuation enrolls
root addresses under that mutex but retains them after enumeration; a later
forwarding store could overwrite zero and resurrect the retired root.

Failure cleanup now waits for the heap collection coordinator before clearing
its exclusively owned slot. Any active runtime entry parks before that wait;
unregistered cleanup accesses no thread-registry membership, preserving failed
admission cleanup even when that registry is poisoned. Vacancy is published only
after the final clear. Successful transfer already occurs RUNNING with the
child's scratch root installed, so it retains its existing transfer protocol.

The reference reproduces the ordering bug by retaining a root address under an
active coordinator interval and issuing the last forwarding store. It fails
before repair. A real collection test requests major/minor GC while the handoff
owner is RUNNING, then drops the handoff and checks progress, exact collection
counts, moved witness data, final zero and vacancy. Three controls detect missing
exclusion, exclusion acquired too late, and a RUNNING coordinator wait. Existing
failed-admission, failed-setup and slot-reuse tests remain covered.

Handoff/global-root storage growth while RUNNING remains open, along with
off-pause metadata destruction and full native metadata accounting/admission.
Evidence and final gates: `target/gc-validation/2026-10-18-handoff-root/`.

## Prepared completion catalogue — 2026-10-18

Replaced the reaper B-tree with one prepared native node per completion. The
node is allocated while BLOCKED before publication; its immutable completion
reference is weak. Enqueue and removal update FIFO links in constant time under
the existing monitor mutex, without container growth, allocation or node
destruction. Duplicate enqueue and repeated removal are harmless. Record
retirement dispatches through its registration's owning monitor, including older
reaper epochs during late-descendant shutdown.

The private link operations require monitor-exclusive access. Batches borrow
strong chain links while locked and return at most 64 weak completion references.
Removing the cursor node advances the cursor. A pass snapshots its starting
queue length as a visit budget, so continuous appends cannot postpone rewind
indefinitely even when the original tail is removed. Adaptive backoff remains. OS joins and native payload destruction remain outside
monitor/registry locks. Shutdown closes and detaches the catalogue under the
monitor, then clears links iteratively off-lock. See [the link protocol](SAFETY.md).

References cover boundary sizes, cursor removal, duplicate/repeated operations,
remove/reinsert order, zero allocator traffic, record-release unlink and a
4096-node teardown on a small native stack. Six fault controls detect allocating
updates/batches, lost cursor repair, retained strong ownership, an unbounded
probe cycle and the incorrect
current-monitor ownership assumption. Native major/minor phase tests include
catalogue preparation. Isolated Miri checks use the actual catalogue/reaper
sources with unrelated joining stubbed; real worker/GC paths run in the runtime
gates. Measurements and full gate results are recorded in the checkpoint report.

Native metadata is prepaid for active bodies as well as queued completions.
On the measured macOS build a node requests one 48-byte allocation and the
record grows 72→96 bytes. Full accounting/admission, handoff/global-root growth,
off-pause record/registry cleanup and broader platform/soak/performance budgets
remain open. Evidence: `target/gc-validation/2026-10-18-reaper-catalogue/`.

## Allocation-free reaper probe batches — 2026-10-18

The native completion worker previously collected each ordered probe batch into
a Vec while holding its monitor mutex. A collector retiring a completion can
need the same mutex. Batches now use fixed stack storage for at most 64 weak
records. The monitor lock is released before weak upgrades, completion probing
and OS joins; cursor ordering, backoff and catalogue ownership are unchanged.
No unsafe storage or shared scratch cache is introduced.

The allocation-probed reference fails with the previous Vec implementation and
passes for empty, 1/63/64/65/257-entry catalogues, exclusive cursors through
usize::MAX, complete multi-batch scans and expired weak records. It checks
record identity, absence of retained strong owners and zero allocator traffic.
Four deliberate faults are detected: heap batches, inclusive cursors, incorrect
weak records and increasing the 64-record work limit. Existing worker race,
backlog, orphan and TLS-exit tests exercise the actual reaper loop.

Catalogue B-tree insertion/removal and shutdown destruction can still allocate
or free under the monitor mutex. Their coordinated replacement/retirement,
handoff/global-root growth and complete native metadata accounting remain open.
Evidence: `target/gc-validation/2026-10-18-reaper-batches/`.

## Cooperative thread completion admission — 2026-10-18

Thread startup now prepares the reaper, completion notice, OS thread and native
completion record in BLOCKED regions outside the completion-registry mutex.
Table staging, unused-resource disposal and notice publication also cooperate
with collection. The environment remains in the existing registered handoff
root until the child transfers it into scratch roots; native preparation never
uses a stale managed pointer. Native mutex backends are prepared before
publication. No generated ABI changes are required.

Reserved record, pending-key and owner tables migrate without allocating or
cloning records. Pending keys use dense storage and a per-record position;
swap-removal repairs the moved key's position, while shutdown removes a pending
key before joining. Migration preserves positions and the execution's reaper.
Late owner binding reserves capacity for every unbound record and stays
allocation-free after GC maintenance. Owner tracing visits dense bound records.
The shared live-record staging budget rejects oversized historical stages after
GC and retries stages made insufficient by competing registrations.

Five new tests cover allocation-free transfer/binding, pending-position models,
stale-stage rejection, prepared mutexes and major/minor progress through seven
native phases. Nine deliberate faults are detected, including RUNNING native
setup, lock-held staging, corrupted positions, missing reservation, historical
capacity acceptance and lost reaper identity. Tests retain real timeouts.

Remaining work includes native handoff/global-root storage growth, reaper
catalogue allocation under its monitor mutex, coordinated off-pause shrinking
and destruction, complete native metadata accounting/admission, and broader
platform/soak coverage. This checkpoint does not establish full production
readiness. Evidence: `target/gc-validation/2026-10-18-completion-registry/`.

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

Active objective: make the GC fully fixed and production ready with high
performance. This ledger covers the allocator, all collector entry points,
embedding/runtime ownership and generated code that publishes or accesses managed
storage. Passing one regression or workload does not close the objective.
The language-wide roadmap remains in [ROADMAP.md](ROADMAP.md); contracts and
historical verification are in [SAFETY.md](SAFETY.md) and [STATUS.md](STATUS.md).

## Remembered references in owned channels

Channel remembered-reference checkpoint (2026-10-17): a committed send marks its
native queue if the reloaded message is young. A completed minor clears only
surviving owned queues, during the existing weak-owner sweep after all copying
workers finish. Clean queues are omitted from minor conditional snapshots.
Counting included records first permits an entirely clean snapshot to allocate
nothing. Major and read-only graph walks retain all edges and their single-pass
registry traversal; raw unbound queues remain conservative.

Three poisoned/verified references run at 1/4 workers. They cover young sends
after cleaning, old sends, wrapped FIFO slots, old-message/young-child heap cards,
clean major/read-only tracing, major retention of a nursery edge, raw-to-bound
transition, clean young-owner relocation and dead clean-owner retirement. Ten
restored controls fail as intended, including a missing heap card barrier and
premature or major-GC marker clearing. Big-endian runs execute these references
directly under the existing supervised runner and worker matrix.

Final gates: 886 workspace passes, zero failures, nine ignores; optimized runtime
377 units + one armed detector + nine doctests; twelve optimized integrations;
136 s390x checks; 50 native and 250 runtime ThreadSanitizer executions without
reports (runtime filters overlap). Relevant source hashes match all gates.

252 final profiling processes compare the unchanged owner/queue example with the
previous verified binary. Minor paired ratios span 0.638–1.050; major ratios span
0.942–1.101. Improvements depend on workload/workers; some cases regress. The
initial two-pass major traversal and its samples are retained; final major and
read-only paths avoid that extra pass. These are local samples, not certified
application, startup, pause or RSS budgets. Other collector allocation, registry
shrinking and metadata accounting/admission remain open.
Evidence: `target/gc-validation/2026-10-17-channel-remembered/`.

## Channel owner metadata during collection

Channel metadata checkpoint (2026-10-17): a sorted temporary owner snapshot feeds
one contiguous channel vector. Group ends replace individually allocated queue
vectors; one lookup map contains only copy-activated source owners. Initial
groups remain disjoint from source groups and need no lookup entries. An
exact-size iterator supplies initial work without a separate index vector.
Multiple queues sharing an owner remain one group. Sorting is O(n log n), and
temporary/retained metadata still allocates during the pause.

At 1024 distinct owners, conditional construction uses five Rust allocation
calls instead of 1055; read-only edges use three instead of 1035. Requested bytes
fall from 221120 to 92176 and from 192460 to 83976 respectively. All-old minor
owners use three allocations instead of 1053, retaining 32768 bytes instead of
40960. The earlier iteration unnecessarily retained 67592 bytes in that case;
the new regression rejects it. Shared-owner storage stays near one Arc per
queue. These are scoped Rust allocator requests, not allocator overhead or RSS.

Four references check unique/shared/mixed owners, exact edges, Arc lifetimes,
generation partitions, combined-pause filtering, initial iterator cardinality,
allocation bounds and old-owner workspace. Moving-GC coverage adds 33 queues
sharing an owner across headers, pointer policies, five collection modes and
1/4 workers. Eleven restored controls fail as intended; the original allocation
bound and earlier all-owner-map workspace regressions also fail their new bounds.

Final gates: 883 workspace passes, zero failures, nine ignores; optimized runtime
374 units + one armed detector + nine doctests; twelve selected optimized
integrations; 124 s390x checks; 50 native and 244 runtime ThreadSanitizer executions
without reports (runtime filters overlap). Relevant source hashes match all gates
and the final profiling binary.

378 final profiling processes compare original, linked prototype and selected
contiguous storage on identical example code. Major candidate/baseline paired
ratios span 0.657–1.015; minor ratios span 0.613–0.822. Minor cases tenure owners
and queue messages first, then admit and verify one young leaf per collection.
The linked prototype was rejected for excess shared-owner metadata. Local
samples do not certify application throughput or pause/RSS/startup budgets.

An earlier iteration's compact-enum JIT/stress/four-worker case exceeded its
60-second limit during overlapping gates. The saved executable completed a
replay in 56.403 seconds; sampling showed active work. That unchanged iteration
later passed the full matrix in 33.54 seconds; final source passes in 31.59.
No assertion, worker count or deadline changed. Contention causality and timing
margins remain unproven. The initial profiling example also exceeded scratch-root
capacity; it was corrected to global roots before rebuilding all three variants.

Evidence, rejected iterations, initial failures, exact samples, sources and logs:
`target/gc-validation/2026-10-17-channel-root-groups/`. Other collector allocation,
registry shrinking, native metadata accounting/admission and broader production
budgets remain open; the full objective is active.

## Demand-driven channel registry staging

Demand staging checkpoint (2026-10-17): construction no longer takes a
speculative registry-planning lock. Publication attempts the existing-capacity
path directly; actual shortage triggers native BLOCKED staging off-lock.
Capacity is still rechecked before the allocation-free commit, and obsolete
tables are still disposed BLOCKED off-lock. Growth takes an extra transition;
ordinary construction needs only the publication lock.

Coverage correction: the preceding race test could leave capacity in the shared
tables and publish without retrying. It now exhausts that capacity and completes
another GC during retry-stage allocation, checking the exact collection count
and preserving queued managed data and witness roots. Nine restored controls
include the original spare-capacity gap and omission of the retry collection.

Final gates: 879 workspace passes, zero failures, nine ignores; optimized runtime
370 units + nine doctests; eight selected optimized integrations; 108 s390x
checks; 23 selected ThreadSanitizer tests without reports. Source hashes match.
The compact-enum test exceeded 60 seconds cumulatively but passed with every
individual case's existing 60-second deadline unchanged.

63 local profiling processes compare the unchanged construction/empty-close
example across original, planning and demand variants. Demand/planning paired
ratios are 0.969 / 1.017 / 1.084 for 1 / 4097 / 131072 slots; demand/original
ratios are 1.078 / 1.032 / 1.186. The small case improves modestly over planning
but remains slower than the original allocator-in-RUNNING path. Larger cases
were slower in this sample; no general performance win is established. First
use is included, teardown excluded. Migration, collector shrinking, other
registries, native metadata accounting and production budgets remain open.
Evidence: `target/gc-validation/2026-10-17-channel-registry-demand/`.

## Staged channel registry growth

Registry growth checkpoint (2026-10-17): channel creation stages native tables
while BLOCKED without holding the registry lock. After returning RUNNING it
rechecks capacity, moves existing records/indexes without cloning controls or
allocating, then inserts the new channel. Empty obsolete tables and unused or
stale stages are freed off-lock while BLOCKED. Concurrent registration can
invalidate a stage; the caller retries with newly observed capacity. The heap
prepares the registry mutex backend before mutator publication. Synthetic test
setup has an explicitly separate allocating helper; production insertion asserts
that its capacity was prepared.

Allocation probes cover the cold prepared mutex, rejected stages and successful
migration/insertion. Forced collection covers allocation and disposal, queued
managed values and witness roots, and competing registration. The demand staging
checkpoint above corrects the original race test's incomplete retry coverage.
Seven restored fault controls detect cold mutex allocation, lost owner/unbound
indexes, a missing capacity check, RUNNING allocation/disposal and locked disposal.

Final validation: 879 workspace tests passed, zero failed, nine ignored;
optimized runtime: 370 units and nine doctests; eight selected optimized
integrations; 108 s390x checks; 23 selected ThreadSanitizer tests without reports.
Matching source hashes, logs, controls and 42 profiling processes are retained in
`target/gc-validation/2026-10-17-channel-registry-growth/`.

Local construction/close paired ratios are 1.092, 0.990 and 0.968 for 1, 4097 and
131072 slots. The small case regresses about 9%; larger samples are noisy.
This moves allocator work out of RUNNING registration without proving a general
speedup. Migration remains O(live registry size) under the lock; staging adds
transient metadata capacity. Collector-side shrinking, other registries, metadata
accounting and application/pause/RSS budgets remain open.

## Channel owner binding reservation

Channel binding checkpoint (2026-10-17): registration reserves owner-index
capacity for every registered channel, including unbound raw handles. Collection
preserves that reservation while shrinking historical burst capacity. Binding
can therefore transfer an existing registration into the owner index without
allocating or adding a collection boundary. This retains extra index capacity
for unbound channels until they retire; registration allocation, collector-side
shrinking and broader native metadata accounting remain open.

A current-thread Rust allocator probe checks fresh binding and binding after a
sparse sweep at several registry sizes. The public ABI is also measured in both
heap modes. Removing registration's reservation or discarding it during sweep
makes the regression fail. Final validation: 876 workspace tests passed, zero
failed, nine ignored; optimized runtime: 367 unit tests and nine doctests passed.
No application performance or production-readiness budget is established here.
Logs, source hashes and restored controls are retained under
`target/gc-validation/2026-10-17-channel-binding/`.

## Channel control synchronization preparation

Channel synchronization checkpoint (2026-10-17): every capacity now constructs
native control/backing in a root-published BLOCKED region. A native-only helper
takes scalar type IDs and reserved backing charge, prepares the state mutex,
backing lock modes and both notification backends, then returns a complete control
for registration. No managed pointer enters that helper. The shared test-only
Rust allocator probe observes zero allocations/deallocations on first primitive
access before registry insertion and on uncontended send/receive/sender/diagnostic
paths afterward. Close allocates nothing while freeing backing. This does not
cover registry/index growth, exceptional diagnostics, OS internals or arbitrary
contention/GC paths.

Forced collection completes during both one-slot and large unpublished
construction, with reservations charged and witness roots published. Seven
restored controls detect cold not_empty/not_full/state initialization, missing
preparation, cold public operations and RUNNING small/large construction.

The new small-constructor transition exposed an invalid atom-test schedule:
concurrent scanning waited for a constructor that could wait for that same phase.
The repaired schedule keeps alias/SATB/closure/witness checks in the held scan,
releases it before construction, reloads managed arguments and acknowledges
RUNNING readiness before the next cycle. The original 20-second watchdog, eight
cycles and payload/byte-count assertions remain. Ten debug and ten optimized
runs pass; two restored dependency/acknowledgement controls hit the watchdog.
An initial compact-enum 60-second timeout passed an isolated saved-binary replay
in 5.859 seconds and the final unchanged integration; its deadline is unchanged.

Final gates: 875 workspace tests passed, zero failed, nine intentional ignores;
367 runtime units and nine doctests are included. Optimized: 366 default runtime
units + one armed detector + nine doctests; 63 selected integrations. Actual
s390x: 100 checks, including both channel-sync references. ThreadSanitizer:
50 native runs and 228 runtime test executions without reports (filters overlap).

112 profiling processes retain noisy first-use/construction and operation samples.
Paired construction/close ratios span 0.423–1.199; send/receive ratios span
0.798–1.155. No speedup, numerical pause/throughput/RSS or startup budget follows
from these samples. Registry/index allocation, other native synchronization and
metadata domains, hard admission and broader production budgets remain open.

Evidence, initial failures, replay, restored controls, exact samples and final
logs/hashes: `target/gc-validation/2026-10-17-channel-sync/`.

## Batched native retirement and notification initialization

Batched native-retirement checkpoint (2026-10-17): registry sweep and execution
shutdown group already reserved backing nodes into one O(1) mailbox publication
per same-monitor group. Separate item/batch links permit iterative destruction
without allocation during group publication. Exclusive controls are unwrapped;
retained native observers keep storage until their final individual handoff.
Weak-only observers do not prevent grouping. Charges remain until actual free.
Tests observe 64 retired buffers in one sweep/shutdown publication, retained
observer fallback, mixed monitors and three collections behind a stalled release.

Local std source inspection found lazy pthread Condvar allocation even from
notification. Worker construction now prepares notification before publication,
inside its native BLOCKED body. A no-worker current-thread Rust allocator probe
checks group building/publication: zero allocations and zero deallocations.
Removing initialization makes that test fail. This does not cover registry/control
metadata destruction or other native synchronization paths. Shutdown observation
now completes GC before inspecting locks, avoiding the short publication race.

Final gates: 872 workspace tests passed, zero failed, nine intentional ignores;
364 runtime units and nine doctests are included. Optimized: 363 default runtime
units + one armed detector + nine doctests; 63 selected integrations. Actual
s390x: 92 checks, including all three native-retirement references. ThreadSanitizer:
50 native runs and 222 runtime test executions without reports (filters overlap).
Eleven restored fault controls detect skipped sweep/shutdown grouping, lost item
or batch links, wrong counts, early observer detachment, inline free, weak-only
fallback, missing flush, mixed monitors and cold notification allocation.

Final local profiling compares the verified individual-handoff worker with the
current implementation: 98 processes, seven alternating pairs per case. Paired
collection ratios are 0.904 for 256 × 32776-byte buffers and 1.066 for 32 × 1 MiB;
complete-cleanup ratios are 1.148 and 1.104. Steady construction/close ratios span
0.964–1.004. Fewer handoffs do not prove a general speedup: publication happens
later, more bytes may remain queued at GC return, and total cleanup can cost more.
Application, startup, tail/pause, RSS, hard admission and broader native metadata
budgets remain open. Bounded grouping/overlap tuning is further performance work.

Evidence, baseline provenance, source/binary hashes, exact samples, fault controls
and final logs: `target/gc-validation/2026-10-17-retirement-batches/`.

## Deferred native backing retirement checkpoint

Native channel-retirement checkpoint (2026-10-17): implicit destruction of
large queue backing transfers a preallocated native node to a lazy native-only
worker. Enqueue allocates nothing and frees no payload. The worker detaches
batches before destruction, outside mailbox/registry/collector locks. Explicit
close/drain release stays synchronous and BLOCKED. Charges follow backing through
queued/in-flight release and native observers, ending after deallocation.
Execution shutdown drains and joins its worker while BLOCKED; native observers
can retain a native-only monitor after Heap destruction, with worker termination
once the last producer and pending work end. A reused execution starts a fresh
worker. Retained native backing does not extend managed object lifetimes.

Validation caught and repaired a growth-policy regression: adapting while dead
backing was queued inflated future demand. A separate pressure-service mutex now
serializes collect/drain/adapt decisions. Callers publish their roots and become
BLOCKED before any pressure lock wait. Collector exclusion is released during
native drain, then reacquired to adapt from actual remaining backing and restore
same-thread registrations together. The unchanged serial retention assertion is
1,310,720 bytes. Saved failing optimized/debug binaries peak at 4,980,768 and
5,505,056 bytes; identical fixture source with the final native/JIT implementation
peaks at 786,464 normally and 524,320 under stress, ending at zero.

Final gates: 867 workspace tests passed, zero failed, nine intentional ignores;
359 runtime units and nine doctests are included. Optimized: 358 default runtime
units + one separately armed detector + nine doctests; 63 selected integrations.
Actual s390x: 84 checks. ThreadSanitizer: 50 native runs and 217 runtime test
executions without reports (selected filters overlap). Ten restored fault controls
detect inline implicit free, early uncharge, mailbox-held free, RUNNING shutdown,
worker leak, joining a retained observer, RUNNING pressure drain/lock wait, stale
growth tuning and collector exclusion retained through deferred drain.

Final performance samples contain 98 processes and separate first use, steady
construction, collection and complete cleanup. They show overhead and substantial
variation, not a broad speedup or latency certification; nearby host load is
roughly 17–20 on 12 active CPUs. Batched retirement handoffs are the next measured
optimization. Registry/control/mailbox/stack accounting and allocation policy,
hard admission, other native domains and application/pause/RSS/platform budgets
remain open. Earlier large-backing destruction backlog is addressed; the overall
production objective remains open.

Evidence, initial failed gates, saved regression binaries, final logs, profiles,
controls and source hashes: `target/gc-validation/2026-10-17-native-retirement/`.

## Occupancy-proportional channel edges checkpoint

Sparse channel-root checkpoint (2026-10-17): collector, preflight and inspection
walks now visit the occupied ring range, using at most two slices rather than
visiting every reserved slot. Send/receive publish head/count with slot changes
in a safepoint-free RUNNING transaction under queue state. The collector reads
only during a world pause; this is not a concurrent queue snapshot. Scanning
never takes the state mutex that a BLOCKED waiter may hold. Native capacity and
its charge are unchanged; range metadata is part of unmetered control storage.

Final gates: 859 workspace tests passed, zero failed, nine intentional ignores;
351 runtime units and nine doctests are included. Optimized: 350 default runtime
units + one armed detector + nine doctests; 40 selected integrations. Actual
s390x: 80 checks. ThreadSanitizer: 50 native runs and 209 runtime test executions
without reports (selected filters overlap). Five restored fault controls detect
missing send/receive publication, omitted wrapped segments, capacity scanning
and acquisition of the waiter-held state mutex.

126 profiling processes provide seven alternating baseline/current pairs for
nine capacity/occupancy combinations. For 131072 slots and one queued message,
local single-worker collection medians are 5998.323 µs before and 1.068 µs after.
Paired send/receive ratios span 0.971–1.033 across the cases. This is quiescent
collection of a fixed live graph and a separate empty-queue send/receive loop;
it does not establish dense large-queue throughput or application tail budgets.

Evidence: `target/gc-validation/2026-10-17-channel-scan/`. Collector-sweep,
shutdown and transient-observer final destruction of large backing, registry
metadata allocation, hard native admission and broader runtime budgets remain
open. Earlier capacity-scan backlog entries are addressed by this checkpoint.

## Channel construction and mutator retirement checkpoint

Channel native-buffer checkpoint (2026-10-17): large queue construction now
reserves its native charge before allocation, captures scalar type metadata while
RUNNING, and builds zeroed backing while BLOCKED before registry publication.
Empty close, final-sender close and closed-queue drain detach backing under queue
locks, then release large backing after unlocking while BLOCKED. The last received
message transfers to a scratch root and reloads after collecting cleanup.

Final gates: 857 workspace tests passed, zero failed, nine intentional ignores;
349 runtime units and nine doctests are included. Optimized runtime: 348 default
units + one separately armed detector + nine doctests; 53 selected optimized
integrations. Actual s390x: 76 selected checks. ThreadSanitizer: 50 native runs
and 205 runtime test executions without reports (selected filters overlap).
Six restored fault controls detect RUNNING construction/release, late reservation,
stale/unrooted popped values and a retained queue-state lock.

Seven alternating baseline/current pairs across seven capacities check 98
processes. Median paired construction/empty-close ratios range from 1.014 to
1.088; the 1 MiB backing case is 1.053. These include registry insertion and
exclude managed wrappers/final execution teardown. They establish neither an
application latency budget nor RSS or admission limits.

Evidence: `target/gc-validation/2026-10-17-channel-buffers/`, including gated
source hashes, logs, restored controls and local profiles. Collector-sweep,
shutdown and transient-observer final destruction can still release large native
backing outside this mutator protocol. Capacity-proportional queue scans,
registry metadata, hard admission and broader production budgets remain open.

## Native FFI ownership/accounting checkpoint

Current generated buffers use opaque ownership tokens and separate aligned
payloads. Large allocation/release bodies run BLOCKED with no managed borrows;
pressure collection uses the existing coalesced native-buffer trigger. Managed
sources are reloaded after allocation. The heap owns one shared queue/FFI ledger;
metric queries and leaf FFI allocation do not lock the channel registry. Charges
include pending reservations, payload backing and token storage, and end after
actual release. Other metadata/allocator overhead and RSS are excluded.

A real early-return leak is closed: the saved baseline retains 96 native bytes,
while the identical source with the final compiler retains zero. Language return,
break and continue clean up escaping call scopes, preserve surrounding scopes,
and refresh traced return values after cleanup that may collect. Direct argument
termination no longer emits calls with missing arguments or middle terminators.

Final verification: 851 workspace tests with nine intentional ignores; optimized
runtime/detector/docs plus 49 integrations; 72 s390x checks; 48 native and 195
runtime ThreadSanitizer executions without reports. Six restored fault controls
cover native phase state, charge lifetime, pressure, source reload and cleanup.
The allocation/release profile contains 98 processes/seven alternating pairs per
size. Small owned/charged operations cost more than legacy unmetered operations;
the 8-byte medians are roughly 15 ns versus 38 ns in this local loop.

This closes FFI ownership, accounting and large native-phase cooperation for
current generated code. Hard admission, other native allocation domains,
small-allocation OS latency, full application/RSS/latency budgets and the wider
production roadmap remain open. Evidence and replay:
`target/gc-validation/2026-10-17-ffi-buffers/`.

## Cooperative FFI copy checkpoint

Generated foreign argument copies use versioned runtime entry points. Large
scalar copies service collection between 32 KiB pieces, with canonical access
scoped inside each piece. Immutable String copies use 256 KiB pieces. Both pin
and reload their owners; read-only progress tests deliberately omit caller roots.
The 32 KiB leaf path and legacy noncollecting ABI remain intact. Width/canary,
UTF-8, mid-copy poison, caller-root and owner-identity checks cover the change.
Six restored fault controls detect the intended failure modes.

The final workspace suite passes 843 tests with nine intentional ignores;
optimized runtime/detector/docs and 46 selected integrations pass. Actual s390x
runs pass 68 selected checks, and ThreadSanitizer reports none across 44 native
runs and 189 runtime executions. The completion-churn test distinguishes
immediate registry unlink from last-native-borrow destruction; the collector
still never waits for the reaper or native observers.

Copy-only profiling retains a measurable cost: the 1 MiB String median paired
time ratio is 1.061 versus the legacy noncollecting helper. Scalar ratios at
1 MiB are within about 1% of legacy. Complete native-call throughput, native
allocation/free latency, accounting/admission and production resource budgets
remain open. Evidence: `target/gc-validation/2026-10-17-ffi-copy/`.

## Native orphan thread reaping checkpoint

Owner-dead safe threads enter an execution-owned weak orphan index. A native
worker probes up to 64 entries per batch, retains unfinished/contended entries
for retry, and joins through actual OS/TLS exit outside collector and registry
locks. Main-function completion alone does not prove OS exit. Native panic-payload
destruction runs outside collection; cleanup failure aborts. Raw unbound handles
retain execution lifetime. GC can retire completed records without waiting for
transient native borrows to finish releasing afterward.

A fixed admission queue was rejected after a bounded regression proved its
overflow waited for another GC. The indexed backlog drains independently of GC
frequency; its size is bounded by known orphan records. Adaptive 1–100 ms polling
handles unfinished/contended probes. One worker belongs to each execution using
owned threads. Shutdown stops/joins it while the caller is BLOCKED, outside locks,
then checks for late descendants admitted by worker TLS before code unloading.
Independent heaps do not share a stalled cleanup worker. The worker owns no heap
or managed roots; a TLS-held Heap Arc cycle is released by execution shutdown.

Final source passes **771 debug workspace checks** and **771 optimized workspace
checks including the separately armed detector** (770 default passes), with nine
remaining intentional ignores. Runtime coverage includes 284 unit checks and
nine doctests. Eighteen poisoned/verified ownership checks and twelve fault
controls pass. ThreadSanitizer passes 36 native runs / 133 runtime executions with
no reports. The twelve new regressions cover header/pointer-policy/copy-path
matrices at 1/4 workers, 257-entry backlogs without another GC, bounded scan
batches, observer contention, 128 actual unobserved spawns, child/worker TLS
blocking with live GC, native panic reentry/fatal cleanup and late shutdown
admissions. JIT/AOT and native sanitizer fixtures abandon managed thread results.

All 365 final source/test hashes remained unchanged across gates. The Thread ABI
source and native spawn/join/bind entry bodies match the previous checkpoint;
ordinary joins gain no registry lookup. Matching source, logs, fault controls,
initial rejected-queue evidence and ABI fingerprints:
`target/gc-validation/2026-10-16-orphan-reaper/`.
No measured speed, pause or RSS improvement is claimed. Worker startup,
backlog/polling/native allocation costs, generation-dependent retention,
resource-pressure scheduling and application budgets remain to be measured.
The full GC production-readiness/high-performance objective remains active.

## Conditional channel ownership checkpoint

Safe channels bind stable native controls to their managed Channel wrapper. Queue
slots are conditional owner edges, allowing abandoned self/cross-channel cycles
to reclaim both managed messages and native storage. The mutable type witness
is not the owner. Raw unbound queues retain unconditional execution roots.
Native operations pin the owner before any blocking transition. A single atomic
weak-owner word supplies pinning and relocation without a per-call registry lock.

Collector copy claims activate conditional groups through the existing bounded
parallel worklist. Concurrent copying defers native slot scanning to the final
pause, when slot storage cannot retire. Full-graph and nursery survivor preflights
include these edges; the combined-pause nursery filter suppresses dead-owner
queues. Read-only snapshots report queue messages as owner edges rather than
permanent roots. All four copy hooks relocate/retire weak controls before reset.

Final gates pass **758 debug workspace checks** and **758 optimized workspace
checks including the separately armed detector** (757 default passes), with nine
remaining intentional ignores. Optimized runtime coverage includes 272 unit
checks and nine doctests. Seven poisoned/verified ownership checks pass, along
with 34 native sanitizer runs / 121 runtime executions and no sanitizer reports.
Twelve fault controls detect omitted binding, tracing, owner pins, relocation,
retirement, generation filters, preflights and snapshot edges. The matrix includes
6,000-channel cycles, both header/pointer policies, 1/4 workers, blocked operations
and concurrent late binding. Final hashes cover 362 source/test inputs; the only
workspace fixture adjustment retains a raw borrow's owner through its intended
type-mismatch check. Runtime/compiler inputs did not change during the gates.
Matching sources, controls and logs:
`target/gc-validation/2026-10-16-channel-ownership/`.

There is no channel throughput, RSS or latency improvement claim. Native metadata,
snapshots and worklists incur owner/queue enumeration and temporary storage;
blocking operations add a scoped root. The full production objective remains open.

## Atomic native ownership checkpoint

Safe AtomicI64 now uses a GC reference wrapper rather than an inline RawPtr
value. Its stable native SC cell weakly follows wrapper identity, so all safe
aliases keep it alive and collected owners can release cells before shutdown.
Raw unbound handles retain execution lifetime. Wrapper construction allocates;
load/store/CAS/fetch-add still neither allocate nor safepoint and take no registry
lock. Codegen and effect analysis share that nonrelocating contract.

Four collector hooks relocate weak native owners before source reset, preserving
uncollected-generation owners. Atomic/completion indices shrink after sparse
bursts, and shutdown releases empty table storage. The tests cover 20
header/pointer-policy/path cases, old/nursery liveness, 128 cell lifetimes,
10,000 raw records with one owner callback, late binding during actual concurrent
copying, and 10,000-owner bursts with three surviving raw-history sizes. JIT/AOT
and sanitizer fixtures exercise moving aliases and concurrent updates. Nine fault
controls reproduce omitted binding/relocation/retirement/shrinking, every collector
hook and the atomic effect classification.

Final source passes **749 optimized workspace checks** including the separately
armed detector (748 default passes; nine remaining intentional ignores), 266
debug runtime units plus nine doctests, five poisoned/verified ownership checks,
and the expanded sanitizer gate: 32 native runs / 108 runtime executions, no
reports. All 359 source/test hashes stayed unchanged across final gates. Thread
ABI and native atomic operation source bodies match the prior checkpoint.
Evidence: `target/gc-validation/2026-10-16-atomic-ownership/`.

This representation adds a wrapper allocation and native owner bookkeeping;
wrapper reads still follow GC barriers. Table compaction can rehash survivors.
No throughput, tail-pause or RSS improvement is claimed without measurement.
Channel ownership/queued-reference cycles, never-joined completions, raw handle
retention, physical/native budgets and remaining admission/outcome/platform/
generated-mutation/soak/CI audits stay open. The prior full debug workspace (741)
predates this follow-up; a current full debug workspace has not been replayed.

## Earlier failed-admission handoff checkpoint

A child that failed registration before its runtime owner existed previously
abandoned the global closure env root and its reusable slot. ThreadHandoff now
owns both the heap and that root until transfer to active scratch storage.
Failure clears and returns the slot even on an unregistered child; successful
transfer disarms ownership before publishing vacancy. AtomicRootSet serializes
unregistered clearing with collector visits. Failed cleanup is fatal.

The bounded test passes 128 failed admissions across semi-space/generational
heaps, one reusable slot, zero retained envs after collection and no dead child
registrations. Four fatal cleanup subcases reject incidental double-panics.
Fault controls detect missing ownership, root clear, slot return, cleanup
boundary and successful-transfer disarming. The test restores only its
purposefully poisoned, unchanged registry; production retains its poison policy.

Final gates pass **741 full debug workspace checks** (nine intentional ignores),
262 optimized runtime units including the armed detector plus nine doctests,
three poisoned/verified retirement tests and the full sanitizer gate: 30 native
runs / 104 runtime executions, no reports. All 357 source/test input hashes
remained unchanged across gates. Thread ABI and allocation entry-point source
bodies match the prior checkpoint. A temporary heap Arc clone bridges handoff
to registered ownership per spawn; no throughput/RSS improvement is claimed.
Evidence: `target/gc-validation/2026-10-16-admission-handoff/`.

Other kernel admission, cancellation/foreign failure semantics, typed outcomes,
never-joined/native resource lifetimes, native/RSS/latency budgets and remaining
platform/generated-mutation/long-soak/CI work remain open. The previous 740-check
full optimized workspace predates this follow-up; no new full optimized
workspace run is claimed.

## Earlier owning runtime registration checkpoint

A native child setup panic previously bypassed manual deregistration: join
returned zero while the dead child remained registered with freed poll storage.
The bounded regression inspects membership before any poll/collection, then
verifies corrected retirement, env reclamation and an empty final registry.
Main contexts and children now share an owner whose registration guard is armed
before further setup and drops before its boxed Thread. Scratch and parked roots
retire with it. Failed deregistration aborts inside coordinator exclusion;
failed draining aborts for both explicit finish_threads and Drop, before callers
can unload still-executing code. Fatal regressions reject destructor double-panics.

Final source passes **740 optimized workspace checks**, including the separately
armed detector (739 default passes; nine remaining intentional ignores), 261
debug runtime units plus nine doctests, two poisoned/verified retirement tests
and the full sanitizer gate: 30 native runs / 103 runtime executions, no reports.
The cleanup test has three bounded fatal subcases. All 357 source/test input
hashes stayed unchanged across final gates. Thread ABI and allocation entry-point
bodies match the prior checkpoint; no throughput/RSS improvement is claimed.
Source, fingerprints, fault controls and logs:
`target/gc-validation/2026-10-16-runtime-retirement/`.

At that checkpoint, full debug workspace replay and admission failures before
registration ownership, never-joined/native resource lifetimes, typed outcomes, native/RSS
and latency budgets and remaining platform/soak/embedding audits remain open.

## Earlier inspection-unwind retirement checkpoint

Safe inspection unwinding previously poisoned the coordinator mutex, so later
runtime retirement could panic before detaching its poll target. A live external
atomic reproduces the stale write without accessing freed storage. Coordinator
exclusion now has an explicit non-poisoning policy; collection failures and
failed pause setup/restoration still abort while holding exclusion. Other locks
retain their poison policies. See SAFETY.md for the invariant.

That targeted checkpoint passed 259 debug runtime units and nine doctests;
259 optimized runtime unit checks including the armed detector and nine doctests;
17 integrations and eight poisoned/verified unwind checks. The ten-case
Full/Compact × collector-entry matrix repeats inspection unwinds and verifies
payloads, restored state, registration and retirement. Full-workspace counts
below predated that follow-up. The owning-registration repair above continues
the child/runtime cleanup audit. Evidence: `target/gc-validation/2026-10-16-coordinator-unwind/`. The final sanitizer gate passes 30 native runs and
101 runtime executions, with no reports; all 356 source/test inputs remained
unchanged across final gates.

## Joined-completion ownership checkpoint

Safe Thread completions weakly follow their traced Thread wrappers. All evacuation
paths update owners before source reset; only joined, owner-dead metadata retires.
GC never waits on a join observer or performs OS joins. Hash-indexed owner
tracking excludes legacy raw history from per-GC tracing. A weak shutdown
worklist accepts descendants while draining and removes bookkeeping at retirement.

The joined-completion checkpoint passed **735 full debug workspace checks** (nine intentional
ignores), 256 optimized runtime unit checks including the armed detector plus
nine runtime doctests, 17 targeted integrations and six poisoning/verification
regressions. The full macOS ARM64 sanitizer gate passes 30 native runs and 99
runtime test executions. All 355 non-Markdown source/test input hashes stayed
unchanged across final gates. Source and evidence:
`target/gc-validation/2026-10-16-completion-ownership/`.

The owner matrix covers 20 Compact/Full × raw/tagged cases across standalone
major, mutator major, minor, concurrent and nursery-major→minor behavior. Tests
also cover late binding during actual concurrent copying in two heap modes,
128 real spawn/join cycles with 256 collections and empty native/admission/owner
indices afterward, nonblocking cleanup, deterministic descendant draining and
one owner callback despite 10,000 legacy records. A typed owner assertion and
JIT/AOT field-replacement regression ensure a replaceable result cell cannot
anchor native handle lifetime; restoring the cell binding fails the assertion. No measured RSS or throughput
improvement is claimed. The previous full optimized workspace checkpoint (727)
predates this change; no new full optimized workspace run is claimed here.

Never-joined/unfinished completion retention, native AtomicI64/channel lifetimes,
remaining retirement-unwind cleanup, RSS/latency budgets and the remaining
ownership/generated-mutation/platform/soak/performance audits stay open.

## Bounded telemetry checkpoint before completion ownership

Per-heap event history is bounded at 1,024 recent events; lifetime totals remain
independent. Exact short-run quantiles transition to fixed histograms with an
explicit relative error bound. Full traces stream through bounded buffering.
JIT/AOT sink errors are reported, both telemetry outputs are attempted, and
failed buffers are discarded without a teardown write retry.

That source checkpoint passed **727 optimized workspace checks**: 726 default passes
plus the separately armed detector (nine other intentional ignores remain).
Targeted debug validation passes 250 runtime unit checks, nine runtime doctests
and 14 integration checks. The complete macOS ARM64 sanitizer gate passes 30
instrumented native runs and 93 runtime test executions. All 354 non-Markdown
validation-input hashes remained unchanged across the final gates. Source,
negative controls, teardown regression and logs:
`target/gc-validation/2026-10-16-bounded-telemetry/`.

The older 716-check full debug checkpoint predates this change; no new full debug
workspace run is claimed. This closes event-history growth with collection count,
not native/RSS or application-latency budgets. The remaining ownership, generated
mutation, platform, soak and performance audits stay open. Quantile and embedding
contracts are in [GC_PERFORMANCE.md](GC_PERFORMANCE.md).

## Allocation-profile checkpoint before bounded telemetry

Native and generated allocation-site counters, live/retired merges and final
report totals now saturate at u64::MAX (documented lower bounds). That
source checkpoint passed 716 full-workspace debug checks with nine intentional ignores;
242 optimized runtime unit checks including the armed detector, nine optimized
runtime doctests, and the optimized generated-counter boundary test; and the
full sanitizer gate (30 native runs, 85 runtime test executions). The safepoint
fixture now creates required live promotion pressure to exercise major/minor
coordination. Evidence: `target/gc-validation/2026-10-16-allocation-profile-saturation/`.
That checkpoint did not include a full optimized-workspace gate. The subsequent
bounded-telemetry checkpoint above does. The remaining platform, ownership,
generated-mutation, soak and performance audits remain open.

## Required evidence

| Requirement | Current evidence | Remaining work |
| --- | --- | --- |
| Checked allocation/layouts | Descriptor/length/alignment/overflow/ID regressions; initialized-range/TLAB tests; custom forwarding/ID locations crossed with identity/low/high policies and fixed/interior/varlen slots in 648 layout cases and 2,160 collections, including native TSan and big-endian | Extend descriptor rejection/failure inventory and untested header/policy geometries |
| Complete root visibility | Shared roots cover globals/raw queues, host/JIT frames and scoped sources; safe channel queues use conditional owner edges across copy paths, preflights and inspection; scoped contention matrix | Check every entry point and embedding lifetime against this inventory |
| Safe collection coordination | Deterministic poll, registration, retirement, nested execution and same-OS-thread sibling tests; fatal collection boundaries and bounded failure tests | Audit all blocking/collection transitions, diagnostics, unwind and lock ordering |
| Moving major/minor correctness | Parallel shared-cycle, generational/card, promotion, root reload and native stress tests; seeded major/minor graph oracle for Compact/Full, raw/low-tag references, embedded references and wide arrays | Wide-array streaming and producer accounting repaired/tested; extend randomized matrix to other pointer policies, publication during copying and long-run evidence |
| Concurrent collector correctness | Both pause requests/root inventory repaired; scoped host access, exclusive copy claims and deterministic copy/publication tests | Integrate all generated/runtime accesses and identity operations; finish matching-width, aggregate and reference/SATB/card protocols and full mutation matrix |
| Generated/runtime accesses | Selected SC/private-access structural checks and native ThreadSanitizer fixtures | Exhaustive loads/stores, managed-reference identity/CAS, inline/value/closure allocation, barriers and callback handoff inventory |
| Embedding ownership | Unsafe raw root insertion, scoped TLS and collection registration; root token provenance/generations | Finish safe API ownership, metadata, source/code lifetime and reentry audit |
| Reproducible performance | Correctness-checked copy/TLAB microbenchmarks; runner captures raw samples, logs and source/build identity | Baseline and allocation improvement recorded in GC_PERFORMANCE.md; queue contention profiled and adaptive/streaming batching measured under recorded load; obtain quiet paired confirmation, resolve small-object eight-worker scaling and add workload latency/RSS budgets |
| Repeatable release validation | Bounded-work-sharing checkpoint: 1054 debug/1053 release workspace passes, armed detector, 421 macOS runtime sanitizer checks plus generated corpus, 760 big-endian checks, 2164 native Linux runtime checks, 1072 Linux sanitizer checks, 50 generated Linux sanitizer runs, 2100 Linux compiler/JIT checks, 12 Miri rows and 72 seeded concurrent-ledger runs (18,432 cycles); 476 archived inputs verified. Subsequent Python transport-verifier repair passed 26 tooling tests, 12 native SSH cases and the 12-row Miri gate; runtime Rust inputs are unchanged | Execute hosted CI, extend platform evidence, workload/resource coverage and ongoing retained release checks; application latency/resource budgets remain open |

Completion requires evidence for every row. An open audit is not a verified
invariant. Public collector capabilities must be repaired rather than removed to
make the checklist easier to satisfy. The concurrent collector has not been
validated for arbitrary generated-program mutation, and its current tests must
not be used to imply that guarantee.

The full 2026-10-16 workspace checkpoint is retained under
`target/gc-validation/2026-10-16-workspace-checkpoint/`: source archive,
`report.json`, debug/release logs and the separately armed release detector log.
Optimized validation checked source hashes before/after. Debug passed 687 checks
with nine ignores; release passed 686 with ten ignores, then the detector added
one explicit pass. The remaining ignores are four opt-in benchmarks and five
documentation examples. These are full-suite local results; they do not supply
the missing platform, concurrent-publication, long-soak or performance-budget
evidence.

Thread-spawn environment handoffs now reuse cleared, per-heap global root slots.
The parent publishes the environment before spawning; the RUNNING child transfers
the relocated value into scratch roots, clears the global and only then returns
its index to the vacant pool. No safepoint occurs in that transfer. Root-table
occupancy from handoffs therefore follows peak simultaneous handoffs rather than
the total number of spawns. A bounded regression forces moving collection in
each of 64 sequential children and checks the transferred payload, one extra
global slot and a cleared returned slot. The later weak-owner repair above also
reclaims joined safe completion records. Unbound raw handles and never-joined
records still retain execution lifetime.

## Current concurrent-copy audit

Heap peak accounting now samples combined active old/nursery occupancy under one
world pause and retains its maximum independently from event history. Samples
precede reclamation, include both concurrent pauses and run during inspections;
metrics emission covers programs without GC using its existing profiling pause.
`Heap::sampled_heap_peak_bytes()` and the benchmark label make sampling explicit.
It includes reserved TLAB slack, excluding inactive copy space/native memory/RSS.
Independent concurrent allocation observation and no-GC/minor/major regressions
pass; negative controls catch omitted nursery usage and late-phase sampling.
Passed 239 debug and 239 optimized runtime checks including detector, 11 bench/log
integrations and the complete sanitizer gate (30 native/77 runtime executions),
no reports. Source/log checkpoint:
`target/gc-validation/2026-10-16-combined-heap-peak/`. The later bounded-telemetry
repair addresses event retention; continuous resource/latency budgets remain open.

Telemetry now separates concurrent world-pause duration from complete cycle
duration (`pause_ns` / additive `elapsed_ns`). `gc_elapsed_total_ms` is available
in runtime metrics and the benchmark catalog. Nearest-rank p50/p99 are consistent
between text and JSON; the old 100-sample p99 incorrectly selected the maximum.
A delayed-copy regression and rank negative controls reproduce both errors.
Passed 236 debug and 236 optimized runtime checks including detector, 11 bench/log
integrations and the full sanitizer gate (30 native/74 runtime executions), no
reports. Retained source and logs: `target/gc-validation/2026-10-16-gc-telemetry/`.
The later bounded-telemetry repair addresses event-history retention. These
metrics do not close native-memory or application-latency budgets.
Definitions: [GC_PERFORMANCE.md](GC_PERFORMANCE.md).

The independent allocation/mutation ledger now runs both semi-space and
generational heaps. The generational case uses a 1 KiB nursery and 4 KiB old
arena, requires automatic minor collection during the worker's allocation loop,
and combines it with 256 external concurrent majors. After worker retirement,
a registered coordinator promotes remaining young roots before the final exact
old-space census. The first actual scan/mutation overlap remains deterministic;
later cycles are free-running. A nonzero seed controls the bounded mutation trace.

Three seeds at 1/2/4/8 workers, five trials per setting, passed 120 isolated heap
mode runs: 30,720 concurrent cycles, 1,076,731 allocations and 12,284 minors.
Each iteration checks prior slot values, successful/failed logical CAS and
duplicate-edge publication against the native ledger; final payloads, alias
identity and exact survivor bytes are checked after reclamation. The full debug
runtime suite passed 233 tests and both allocation-pressure sanitizer test names
passed. Only test coverage changed; production build inputs match the preceding
full source checkpoint. Delta sources, hashed logs and per-mode counters:
`target/gc-validation/2026-10-16-generational-pressure/`. This is a bounded seeded
correctness matrix; long-running memory/latency and platform budgets remain open.

Minor promotion capacity now uses a read-only reachable-young preflight when
total nursery occupancy exceeds old-space headroom. It includes roots and edges
from retained old objects, deduplicates aliases/cycles and stops once surviving
bytes exceed available space. Garbage occupancy alone no longer forces a major.

If reclamation is required, whole-graph marking computes the live nursery root
sources for the preceding major. Unreachable nursery wrappers must not retain
old garbage and cause false promotion exhaustion. This filtered major is private
to the combined fallback: all mutators remain stopped until the following minor
resets nursery storage, including unreachable wrappers with obsolete old bits.
Standalone major collection keeps its existing conservative nursery treatment.
The legacy verifier accepts legitimate nursery edges from copied old objects
while still rejecting evacuated old-space references. Minor pause accounting
includes preflight/marking and excludes the separately recorded major duration.

Regressions cover zero survivors at near-full old capacity, exact-fit remembered
cycles, required majors and a live old/young cycle beside an unreachable young
wrapper retaining a large old buffer. The original fallback aborts with promotion
space exhaustion; whole-graph nursery liveness reclaims it correctly. Compact/Full
and raw/tagged policies pass, with exact reclamation/promotion event bytes.
Verifier and evacuated-space poisoning checks also pass. Final checks: 233 debug
and 233 optimized runtime checks including detector, 18 integrations and the full
sanitizer gate (30 native/71 runtime executions), no reports. Retained source,
negative controls and passing logs:
`target/gc-validation/2026-10-16-promotion-liveness/`. Long soaks, larger randomized
cross-generation pressure and pause/scratch-memory budgets remain open.

Allocation boundary coverage now checks payload lengths 0, 999, 1000 and 1001
against a 1 KiB nursery: header/count overhead and padding yield extents 24,
1024, 1024 and 1032. Exact-fit requests remain young; the next rounded extent
tenures directly, with no collections when capacity exists. An independent
constructor case has zero leading reference slots and only a raw-region reference
at byte 2048. Its nursery child survives a minor and three majors through initial
owner remembering. Omitting the embedded-pointer condition makes that case fail.
Passed 229 full debug runtime tests and six targeted allocation cases in optimized
and ThreadSanitizer builds. Production runtime/heap hashes match the preceding
full sanitizer checkpoint; only regression coverage changed. Delta sources/logs
and the parent checkpoint reference:
`target/gc-validation/2026-10-16-allocation-boundaries/`.

Oversized runtime objects now try tenured storage directly when their validated
full extent exceeds nursery capacity. Available tenured capacity requires no
minor/major attempt. On exhaustion, nursery scavenging first removes dead young
owners whose back edges would otherwise conservatively retain old objects in a
major scan. The runtime retries after scavenging, then uses the capacity-aware
major path if needed. Explicit collect-per-allocation stress remains active.

Fresh reference-bearing tenured objects are remembered before returning to
generated/private constructor stores. Direct tenure can preserve nursery operands
that the previous minor-first route promoted; plain constructor initialization
must therefore be visible to the next minor. Tests cover independent constructor
and later-card witnesses, fixed/byte/reference-array layouts, exact live bytes,
tenured exhaustion, dead nursery back edges and stress. Omitting owner remembering
loses a constructor edge; omitting exhaustion scavenging reproduces OOM; restoring
minor-first oversized routing fails collection-count checks.

Passed 227 debug and 227 optimized runtime checks including detector, 18 selected
integrations and the full sanitizer script (30 native/65 runtime executions),
without reports. Full source/log evidence:
`target/gc-validation/2026-10-16-oversized-allocation/`. Collection counts establish
removed work, not an elapsed-time speedup or production operating budget.

Allocation-driven major requests now coalesce a competing collection only when
the active arena has room for the validated object extent. A concurrent cycle
can retain SATB floating garbage and fill its destination even when a fresh
stopped-world collection could reclaim enough space. Previously the allocation
helper accepted the changed collection counter unconditionally, retried against
the full arena and aborted with a false OOM. Explicit non-allocation requests
retain their original coalescing behavior.

A deterministic regression fills a 4 KiB source with a 128-element array and
127 leaves, removes all edges during copying, then blocks an allocation behind
the coordinator. SATB retains a full destination; the waiting mutator must run
a fresh collection and allocate successfully. Restoring unconditional coalescing
reproduces OOM. An 8 KiB companion fills the source with additional unreachable
garbage and requires coalescing when the destination has enough room. A final
collection removes retired TLAB reservation slack before exact live-byte checks.

The independent pressure ledger also passed 40 optimized runs at 1/2/4/8 workers:
10,240 concurrent cycles, 328,923 allocations and 12,332 iteration intervals
overlapping actual scanning, on 4 KiB heaps. Initial overlap uses one guaranteed
fitting publication; later cycles remain free-running so allocation exhaustion
can wait for the coordinator to finish. These runs exposed the OOM above.
Final checks passed: 223 debug and 223 optimized runtime checks including detector,
21 selected integrations, full sanitizer gate (30 native and 61 runtime executions)
and two further sanitizer tests covering the expanded capacity cases. Retained
sources, failure logs and passing evidence:
`target/gc-validation/2026-10-16-allocation-capacity/`. Long soaks, broader
concurrent publication and operating budgets remain open.

The final pause now consumes owned SATB chunks directly, avoiding a second
flattened payload vector, and drains local residual values in place without
replacing their reserved storage. The public owned `drain_all` API remains
available, with its queue lock released before flattening. Allocation-identity
tests check chunk transfer and local-storage reuse. An actual-copy regression
now installs and removes a private object, forces its SATB entry into a full
chunk and requires that extra 24-byte survivor; skipping queued consumption
produces 496 rather than 520 live bytes. This strengthens retention evidence
beyond checking that a queue was nonempty.

Passed 221 debug and 221 optimized runtime checks including the detector,
eight selected integrations and all 44 collector-filter ThreadSanitizer tests,
without reports. Full source/log checkpoint:
`target/gc-validation/2026-10-16-satb-consumption/`. The earlier full native
sanitizer/workspace runs predate this drain optimization. No pause-speedup or
whole-cycle memory bound is inferred from removing these allocations.

Internal SATB logging now shares `ThreadState::log_satb`: ordinary reference
stores, logical CAS and MutatorThread barriers publish full threshold-sized
chunks to the queue without a managed safepoint. Previously runtime stores/CAS
appended directly and bypassed threshold flushing. Nonempty drains reserve a
replacement local buffer; empty drains preserve its allocation. This bounds
internal local chunk occupancy, not total queued cycle memory. Queue-lock scope
does not acquire access stripes or wait for mutator progress.

The active-copy ABI regression now performs over 2,000 store/CAS logs per cycle,
asserting local occupancy below 256 and queued publication. A separate regression
checks drained values and retained reserved capacity. Disabling flushing and
restoring capacity-losing drains each fail the corresponding negative control.
Passed 219 debug and 219 optimized runtime checks including the separately armed
detector, nine selected integrations and the full sanitizer script (30 native,
57 runtime executions), without reports. Source archive and hashed logs:
`target/gc-validation/2026-10-16-satb-chunks/`. Whole-cycle memory bounds and
barrier throughput remain open.

The concurrent collector has a distinct copying protocol. Its two root passes
now share the registered-root enumerator with the stop-the-world collector and
snapshots, and both pauses raise mutator poll flags. Tests cover permanent/JIT
roots, temporary roots waiting behind it and allocation-free poll-only workers.
Those tests deliberately isolate coordination and root coverage.

Further source issues to resolve with mutation regressions:

- `atomic_copy_words` operates at 64-bit granularity while managed fields can be
  8/16/32 bits and inline aggregates require coherent snapshots.
- Generated `ai_gc_write_barrier` performs generational card marking; concurrent
  read/SATB/replication protocols must be traced through every actual mutation.
- Copy claims/publication, mutator changes to forwarded objects, both root
  transitions, allocation during copying and final remembered-set rebuilding
  require an end-to-end model. Comments describing from-space discovery must
  agree with the implementation's to-space scan.

These are outstanding protocol obligations, not a claim that the new root and
poll fixes establish concurrent-copy readiness.

### Seeded graph oracle

`gc::graph_model_tests` checks 16 deterministic seeds per Compact/Full header,
six successive graph rewrites/collections per seed, alternating stop-the-world
and concurrent major collection. A separate logical-ID graph computes reachability
and exact live bytes. Traversal after each collection checks every expected edge,
null, payload ID, descriptor, array length and physical alias; dead nodes must be
reclaimed. Layouts include fixed reference slots, an embedded reference in raw
storage, empty/small traced tails and a 600-element traced tail. Duplicate roots
and edges exercise identity preservation. Each header runs in a subprocess with
a 30-second deadline so termination regressions fail rather than stall the gate.

Generational cases allocate each new batch in the nursery, publish field-address
cards and check exact promotion bytes using a second closure rooted in all old
objects plus global roots. This accounts for nursery objects reachable only from
dead tenured objects, which remain allocated during a minor. The graph is checked
immediately after promotion, then again after a major reclaims unreachable old
objects and excess promotion. Eligibility rotates each cycle, permitting new
edges and dropping old reachability rather than permanently excluding new nodes.
Across all four cases, one invocation covers 576 collections: 192 minors and
384 majors.

The oracle passed optimized runs at 1/2/4/8 workers with verification enabled,
and ThreadSanitizer. A negative control that omitted the embedded-reference
descriptor failed for both headers on a stale source-space edge; the correct
descriptor was restored before verification. Omitting dirty-card publication
also makes both promotion cases fail at seed 1, cycle 1. Both negative controls
were restored before final verification. This is bounded major/minor graph
evidence with mutation **between** collections. It does not establish concurrent
mutation/publication safety, all pointer policies, custom headers or a long-running
soak.

Tagged-policy extension: the same oracle now runs with both raw identity and
low-bit-tagged references for both headers and collection configurations. Every
relocated reference must retain its tag and decode to the expected unique object;
additional immediate (3) and null (0) roots must remain byte-for-byte unchanged.
Eight bounded cases cover 1,152 collection calls per invocation. Optimized
verification at 1/2/4/8 workers and ThreadSanitizer passed. Deliberately replacing
the evacuation publisher's policy encoding with raw pointer bits makes all four
tagged cases fail. The collector was restored to its prior full-workspace hash
before verification; the full debug runtime suite passed 218 tests, four ignores.
Retained source/log evidence: `target/gc-validation/2026-10-16-tagged-graph/`.
This extends the policy matrix; it does not validate every possible pointer
encoding or concurrent mutation/publication.

## Performance evidence

Run the benchmark runner only after other builds, tests and benchmarks finish:

```sh
python3 scripts/benchmark_gc.py --output target/gc-bench/baseline.json \
  --workers 1 2 4 8 --trials 3
```

It runs separate small-object copy, large-payload copy and allocation experiments.
Every Rust workload checks exact object/live-byte counts. Copy timings retain all
nine GC pause samples (first two warmup); allocation retains five samples and
includes barrier release through worker joins/retirement. The outer trials rotate
worker order. JSON records source hashes, revision/worktree state, machine,
compiler/build settings, exact commands and raw log paths. Source drift or failed
validation makes the artifact fail rather than silently mixing measurements.

The allocation workload is fixed at 250,000 objects per mutator in a 64 MiB heap,
so the runner accepts up to eight mutators. This is a workload limit, not a runtime
thread limit. These microbenchmarks establish comparative local evidence; they do
not measure application startup, latency distributions or peak RSS. Those budgets
remain part of readiness and cannot be inferred from copy throughput alone.

Current measured baseline and before/after results: [GC_PERFORMANCE.md](GC_PERFORMANCE.md).

Read-barrier marker/claim publication and custom-header forwarding locations are repaired. Collection panic escape/restoration is repaired; the broader failure inventory and concurrent mutation remain open; see docs/SAFETY.md and the project status pad.

## Scoped concurrent object access checkpoint

String joining now loads each mutable array slot through canonical scoped
64-bit SC access, ending that scope before reading immutable String bytes and
allocating the result. It is a sequence of element reads, not an atomic whole-array
snapshot. Direct source-array reads could return replaced entries after forwarding.
The bounded runtime ABI fixture now schedules its concurrent-scan rendezvous on
a String edge from an already-published join array. A mutator changes both slots
through an old array alias and joins through that alias over eight actual cycles.
Restoring the direct load reproduces `leftleftleft` where `rightleftright` is
required. Optimized and sanitizer regression runs pass with canonical loading.

`Heap::object_access` and its affine MutatorThread wrapper provide a complete
non-safepointing access scope. Old/new aliases share ordered source/destination
stripes; duplicate stripes are acquired once. Concurrent copying claims the
source forwarding word before reserving storage, then holds the same pair through
payload copying and forwarding publication. Waiters release stripes before
retrying marker-only claims. Only the claim owner allocates a destination.
Concurrent discovery releases an owner's stripe before copying a child.

A bounded actual-cycle regression schedules a mutator write after the payload
snapshot and before forwarding publication, for 8/16/32/64-bit payloads with
Compact, Full and offset-two custom headers. Both alias forms read the retained
write after publication. Wrong old-alias resolution makes the test fail. Separate
tests cover competing copiers' exact destination size and opposite/colliding
stripe pairs. The mutator borrow has a compile-fail no-safepoint check.

This establishes the scoped host path, not arbitrary language-program mutation.
Generated scalar/private/aggregate accesses and runtime/FFI/reflection/container
accesses do not yet all enter the scope. The after-store replication hook alone
is not a full write protocol. Managed-reference Atom loads and CAS now use scoped runtime entry points.
Scalar Atom operations now use matching-width scoped runtime calls too. ScalarRepr::Ptr
is a non-GC RawPtr and must retain its physical-address comparison semantics. All of that integration
remains required before closing concurrent correctness. Existing public collector
capabilities are retained; no stop-the-world fallback replaces concurrent copying.

The scoped cycle test now reloads parent/child references after the first pause
before retaining the old field alias in a non-safepointing interval. It also
stores a nursery reference into a forwarded parent using SATB/card protocols,
clears the temporary nursery root, and verifies promotion by the next minor GC.
This exposed and repaired destination-space card selection and below-base card
marking overflow. Heap::reference_eq provides a non-waiting managed-identity
primitive for published references and is used by generated managed-reference Atom CAS.

Generated aggregate field reads now use `ai_managed_read`: acquire the aggregate
stripe first, then retain canonical ObjectAccess through the native snapshot
copy. Uncontended reads complete during copying; contended acquisition may park,
with roots published and revalidated. Generated aggregate replacements try `ai_managed_try_write` without parking,
using constant per-value reference offsets for SATB and canonical card barriers.
SC events stay inside canonical exclusion for both reads and writes. Failed
attempts use the rooted legacy lock fallback and reload the RHS. Legacy locking
uses `ai_managed_lock`, which tries a stripe while the collector is idle. This path stays
RUNNING without a safepoint until unlock. During copying or contention it uses
a rooted blocking region and waits for relocation before returning. Do not nest that
managed blocking transition inside ObjectAccess. Design the combined acquisition
so no held aggregate/access lock prevents a collector or a resumed waiter from
finishing. Complete all actual load/store/header/identity paths rather than
turning concurrent collection into a whole-cycle stop-the-world fallback.

### Identity-aware host CAS

`Heap::compare_exchange_reference` and the exclusive MutatorThread wrapper now
perform an untagged managed-reference SeqCst CAS inside the canonical access
scope. Logical identity is checked before each physical CAS attempt; an atomic
failure retries from the observed value. SATB logs the replaced candidate before
CAS (conservatively on retries); success marks a nursery edge using the actual
canonical slot's physical card. These APIs require a RUNNING owning registration,
live published bases and a valid aligned reference field. They do not support
tagged payloads or arbitrary raw-pointer fields.

A deterministic post-publication fixture keeps source bits in the field and
uses the destination as expected; a raw-bit comparison fails this regression.
Failed comparison preserves the field and does not add a SATB entry; successful
replacement logs the actual previous bits. The real concurrent-cycle regression
also uses this CAS for field replacement and retains the following minor-GC edge
check. Generated managed-reference AtomCas now routes through `ai_atom_ref_cas`;
managed-reference AtomLoad uses `ai_atom_ref_load`. Scalar/RawPtr atom operations
now use matching-width scoped SeqCst runtime calls; RawPtr retains physical-address equality.

### Generated managed-reference Atom integration

Codegen selects the reference ABI only for Repr::Ref; RawPtr and scalar semantics
are unchanged. Both entry points are non-safepointing, and the codegen relocation
effect list records this property. JIT symbol mappings and native declarations
use pointer results for loads and an explicit u8 CAS result. The runtime load
holds canonical access through its SeqCst slot load; CAS includes the complete
identity/SATB/card protocol described above.

A bounded runtime ABI regression alternates reference replacement over eight
actual concurrent cycles, reloads global roots after each poll, checks successful
and failed CAS, and verifies exact final live size. Native/JIT normal/stress
programs distinguish same-valued objects by identity and replace references
across allocating expressions. The managed-memory sanitizer fixture now includes
two contending reference writers and verifies their exact final counter. An IR
regression requires calls to both scoped reference entry points.

Scalar Atom accesses now have the scoped protocol described below. This does
not establish concurrent safety of general field reads, construction, reflection
or aggregate operations. Those paths remain
required before enabling arbitrary language programs to mutate during copying.

### Matching-width scalar Atom integration

All scalar Atom loads and CAS now enter scoped runtime operations specialized
for 8/16/32/64 bits. Bool uses eight-bit storage with one-bit encode/decode;
floats use exact integer bit patterns, preserving NaN and signed-zero CAS
semantics. RawPtr round-trips through pointer-sized bits and retains address
comparison, without becoming a managed reference or invoking reference barriers.
The specialization avoids runtime width dispatch and preserves matching-width
SeqCst atomics among mutators. No field interior pointer crosses a safepoint.

The concurrent ABI regression covers all widths over eight actual cycles,
checks successful and failed CAS, and verifies neighboring bytes survive narrow
operations and copying. Native/JIT normal/stress tests cover bool publication,
narrow extremes, char, floats, managed identity and RawPtr address CAS. The RawPtr
fixture explicitly constructs its Atom inside unsafe, as required by the current
RawPtr-record capability rules. IR checks require all four width-specialized
load/CAS call paths. The native sanitizer managed-memory fixture now exercises
8/16/32/64-bit scalar Atom paths as well as contending reference writers.

General field/array access, closure/header reads, constructors, reflection and
aggregate synchronization remain open. Performance comparisons of these new
entry points remain required; historical benchmark artifacts predate them.

The remaining field/array integration must retain semantic slot information:
`managed_load` currently receives an already-derived address; FieldLoc::Ptr and
reference/value-array pointer loads pass ScalarRepr::Ptr as their storage type.
That is a storage convention, not proof these slots hold native RawPtr values.
Carry base+offset and actual trace/reference classification into the scoped API;
do not infer reference-barrier behavior from the LLVM storage representation.
Value-array loads also read a separate managed value box after loading its base,
so end the array scope before acquiring the box scope rather than nesting guards.

### Phase-checked ordinary scalar/reference slots

Generated mutable scalar/reference field and array-slot accesses now retain the
object base and byte offset through a phase check. Thread appends a heap-owned
atomic phase pointer at offset 80; root/child runtime initialization and layout
assertions cover it. A RUNNING mutator cannot cross either world pause without a
poll. Therefore an idle-phase access may use the existing direct SC operation,
or its existing proven-private plain operation with alias metadata, until that
non-safepointing access finishes. The active path calls width-specialized scoped
runtime loads/stores. No precomputed field interior pointer enters the helper.

Pointer loads share the 64-bit storage helper. Stores carry an explicit traced
classification from FieldLoc/array element layout, including boxed value-array
slots, rather than guessing from LLVM pointer representation. Active reference
replacement logs a candidate old value before a physical CAS and retries on
contention, then marks the canonical slot's physical card. Native RawPtr scalar
stores remain untraced. Idle reference stores retain the existing generational
barrier. The collector's overlap metadata supports marks on later field cards;
new CAS/store regressions preserve a nursery edge in a later card of a large
owner through minor collection. A suspected start-card restriction was disproved
by both tests and inspection of the overlap-aware scanner; no marking behavior
was changed for that hypothesis.

The concurrent ABI regression now covers nonzero offsets at all four widths,
reference stores, successful/failed Atom CAS, sentinel preservation and exact
live size across eight actual cycles. IR checks require the phase check, all
width-specialized fallbacks, traced store routing and private idle metadata.
Full workspace, optimized and sanitizer gates passed on unchanged source;
the dated status checkpoint records exact counts, source hash and log paths.

Aggregate field operations retain their separate existing managed-lock protocol;
do not nest an ObjectAccess around that managed blocking transition. Immutable
value-box payload, length, closure and header reads need an explicit immutability/
publication audit, as do construction, runtime/reflection/FFI and bulk operations.
These changes do not establish arbitrary generated-program concurrent safety.
Current performance overhead and broader performance budgets remain unmeasured.

### Scoped reflection access

The four in-language reflection entry points now resolve supplied aliases before
reading the type/enum tag. `field_i64` retains one canonical access through both
metadata selection and its matching-width SeqCst field load. Bounds/value-kind
failure paths drop access before formatting and reporting diagnostics, which
may park for GC. `field_name` retains only off-heap metadata after inspection;
it allocates the returned String after the access scope has ended. Enum tags
remain immutable after publication under the language object contract.

The bounded runtime ABI test now uses a scheduled identity pointer policy to
rendezvous at concurrent-scan pointer decoding after the owner stripe is released.
Each of eight cycles must remain active until the RUNNING worker completes an
iteration. It exercises a standard Full-header enum and forwarded source type word, alias-based payload
mutation/reflection, reference operations, all scalar widths at nonzero offsets,
metadata kinds/counts, neighboring-byte sentinels and exact live bytes. Scalar
objects have distinct width-correct reflection descriptors. Tiny heaps can no
longer pass this test by executing all mutator operations between cycles.

Using the original alias for metadata yields zero fields for the forwarded enum;
using it for the payload returns the stale value. Both negative variants fail
with captured assertions (`/tmp/gcr-reflection-{metadata,payload}-negative.log`),
and correct source was restored before final verification. The subprocess bound
also contains the ensuing collector wait when an intentionally failing worker
cannot reply to the rendezvous.

This closes these reflection read entry points' canonical-access gap. Heap dump
rendering, runtime/container/FFI operations, construction/publication proofs,
generated full concurrent workloads, embedding/platform/soak gates and measured
performance remain part of the broader audit.

Reflection follow-up verification passed on unchanged changed-file hashes:
208 debug runtime unit tests, 208 optimized runtime unit tests including explicit
detector, nine compile-fail doctests, 23 selected optimized integrations and
31 collector ThreadSanitizer tests with no reports. See the dated STATUS entry
for exact logs and hash scope. The earlier 680-test full workspace checkpoint
predates this follow-up; it was not rerun for this targeted runtime change.

### Scoped scalar buffer handoff

`ai_buf_copy_in/out` now resolve the supplied managed base and retain canonical
object access through the entire non-safepointing copy. Interior data addresses
are derived only after resolution and never escape. The native buffer remains
private/disjoint, may be unaligned, and crosses the foreign call independently
of managed storage. Per-element matching-width SeqCst operations and bool-byte
normalization are unchanged; this is not a whole-array atomic snapshot.
Reference arrays are excluded from scalar copy-out, so no SATB/card protocol is
omitted. Thread must be the owning RUNNING mutator and no outer object guard may
be held. The guard ends before foreign calls, callbacks or managed blocking.

The scheduled eight-cycle ABI regression retains a buffer source alias in an
untouched traced probe field while the collector also updates a global buffer
root. Each active iteration changes its input pattern, exercises 1/8/16/32/64-bit
copy-out/in and zero-length null native buffers, checks canonical data separately,
and preserves tail sentinels, length and exact live bytes. A repeated pattern
initially hid stale copy-in and was replaced with per-iteration/per-width data.
Deliberately using the original source for either direction now fails with
captured byte-array mismatches; correct code was restored before final gates.

This repairs the mutable scalar-buffer bridge. Immutable String copying has a
separate immutability/publication obligation; bulk reference/container operations,
constructors and other runtime paths remain in the audit. Final targeted and
full sanitizer evidence passed and is recorded in the dated status checkpoint.

Buffer follow-up: 208 debug runtime unit tests, 208 optimized runtime unit tests
including detector, nine compile-fail doctests, 40 selected optimized integrations
and full sanitizer 30 native/46 runtime executions passed, with no reports. The
changed-file hash and logs are recorded in STATUS; the earlier full-workspace
checkpoint remains separate.

### Host response construction and publication

The host I/O builder's entries array is scratch-rooted while subsequent entry
allocations can park or collect. Its entries now use the complete scoped SC
reference-store protocol, including SATB/card handling. Before each entry
allocation the builder services pending collection requests with the normal JIT
frame-publication safepoint helper. At that point only native owned values and
scratch-rooted managed objects are live. It reloads the array after allocation;
the new String cannot safepoint between return, byte initialization and pointer
publication. The final response reloads both scratch roots after its allocation,
then initializes and barriers its unpublished fields without another safepoint.

Redundant count stores were removed from String and entries-array construction:
the typed allocator already initializes each immutable varlen count. Counts and
headers are valid before the object enters scratch roots. The String bytes finish
before any managed publication or later safepoint.

A bounded runtime regression uses a test-only rendezvous after the entries
array is scratch-rooted and before its first entry. The hook waits for the
collection request but does not poll or park; the production loop must service it. A precise pointer policy inspects the stopped-world
scratch root and requires an array whose last entry is still null. The test then
checks every one of 32,768 UTF-8 entries after collection and response completion.
Removing the loop poll fails that partial-array observation. Reinstating plain
stores with the poll in place produces a ThreadSanitizer race between ai_host_call
and process_slot_concurrent. Correct code was restored before final gates.
Negative logs: `/tmp/gcr-io-{poll,store}-negative.log`.

This establishes this specific partial-construction path; it does not close all
generated constructors, runtime publication or performance requirements. Final
full sanitizer/workspace/optimized evidence is in the dated status checkpoint.

The initial reservation-watching test passed the full workspace but failed its
partial-array assertion in release mode when construction outran the collector.
That timing assumption was replaced with the scratch-root rendezvous. Both the
missing-poll negative case and the plain-store sanitizer race reproduce with
the deterministic schedule. No production-only delay or test timeout change was
introduced. The 681-test workspace pass predates this test-only scheduling change;
final runtime and sanitizer checks cover the updated harness separately.

Host response follow-up final checks passed: 209 debug runtime unit tests,
209 optimized runtime unit tests including detector, nine compile-fail doctests,
37 selected optimized integrations, full sanitizer 30 native/47 runtime executions,
no reports. The 681-test workspace passed before the test-only rendezvous fix;
STATUS distinguishes that run from current targeted checks and records hash scope.

### Canonical header and closure-code reads

`ai_object_type_id` reads the physical type ID while retaining canonical object
access. Generated TypeIdOf uses its original direct load only in the idle phase;
copying uses this non-safepointing helper. Physical compact-enum IDs are still
mapped to their nominal LayoutId after the branch merge. Native declarations,
JIT mappings and nonrelocating call effects are connected.

Closure-code lookup now resolves the environment once and holds access through
type lookup, capture-count lookup and the code-address read. Type-name lookup
releases type inspection before allocating its result. Channel witness creation,
witness validation and message validation use the same scoped type-ID helper;
no guard crosses diagnostics or native queue blocking. Runtime's remaining direct
obj_type_id calls are inside canonical scopes or the scoped helper.

The scheduled eight-cycle ABI regression retains old closure/witness aliases in
untouched traced probe fields, checks a two-capture closure's executable marker
address and physical ID, validates an existing channel, creates one from an old
witness during copying, and allocates the correct type name before releasing the
collector rendezvous. All managed aliases used before that allocation are dead
before the next poll. Exact live-byte accounting excludes unrooted name results.
The IR regression requires phase-checked resolution and nominal enum mapping.

Wrong-source type inspection returns 152 instead of fixture ID 7; wrong-source
closure lookup uses that corrupted ID as a layout-table index and aborts. Logs:
`/tmp/gcr-header-{type-id,closure}-negative.log`. Correct code is restored before
final gates. This repairs these header readers; code/metadata lifetime, other
publication/runtime/bulk paths, generated full concurrent workloads and measured
performance remain open.

Header follow-up final verification passed on unchanged changed-file hashes:
189 compiler + 209 runtime library tests; 209 optimized runtime unit tests
including detector; nine compile-fail doctests; 46 selected optimized integrations;
full sanitizer 30 native + 47 runtime executions, no reports. STATUS records hash
scope and logs. The earlier full-workspace checkpoint predates this follow-up.

### Immutable generated enum/payload audit

For normal generated Full-header reference enums, the tag occupies
Full::ENUM_TAG_OFFSET (the first word), while heap forwarding occupies the
separate word at offset eight. Published enum tags and payloads are immutable;
all pointer/raw payload addresses used by EnumPayload and match binding are at
or beyond the 16-byte header. Concurrent copying reads those source bytes and
changes only the forwarding word. Direct immutable reads therefore need no
canonical write-exclusion scope within a RUNNING/no-safepoint interval. Pointer
payloads may yield published old aliases, whose subsequent mutable/header/identity
operations must follow their own scoped protocols. Paused root fixups and the
next relocation-epoch reload delimit that interval.

This argument also distinguishes immutable boxed-value array payloads (array
mutation replaces the box reference rather than mutating its bytes) from mutable
array slots. No safepoint occurs between loading the box pointer and copying its
payload. It is conditional on completed construction/publication and immutable
language semantics. It does not establish arbitrary custom-header, unsafe host
mutation, debugger edits or code/metadata lifetime safety; those audits remain.

### Bounded local parallel continuation

Stopped-world workers can keep up to 128 published-object tasks locally when
copied work is at most 32 KiB. Their active-producer registration remains live
through the continuation, so idle workers cannot declare quiescence while local
work exists. Excess work is published, and large-copy batches and BUSY retries
return to the shared queue. All owned claims finish publication before local
work starts. Streaming scans retain the existing bounded mid-scan publication
protocol. No copying-space tail reservation or worker-count cap was added.

The production transition is exercised by chain/tree/star models at 1/2/4/8
workers, full runtime tests and full parallel-module/collector sanitizer filters.
The current source baseline, rejected finish/take experiment and three retained
candidate/control pairs are archived in GC_PERFORMANCE. Small-object pairs favor
the candidate at four/eight workers under heavy recorded background load; quiet
confirmation and broader budgets are still required. Generated concurrent-access
overhead is not measured by these stopped-world collector microbenchmarks.
