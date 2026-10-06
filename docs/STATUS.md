# Verified state

Assessment and application work: 2026-10-03. Repository HEAD: `5c29ee7`;
the application and its supporting changes are in the working tree. Host: Darwin ARM64; Rust/Cargo 1.96.0. The committed baseline includes the prior widget,
`.gitignore`, and benchmark-artifact changes.

The reproduced defects below have fixes and regression coverage. Production
readiness remains unproven; remaining gates are in [PRODUCTION.md](PRODUCTION.md).

## Tests observed after fixes

`cargo test --workspace`: 427 passed, zero failures, five ignored runtime doc
examples. Application/module/concurrency release and CLI release verification: 58 tests passed with
`GCR_GC_VERIFY=1`. The preceding bug-fix baseline passed 41 release checks.

| Suite | Observed result |
| --- | --- |
| Compiler/frontend unit tests | 164 passed |
| Runtime unit tests | 110 passed; 5 runtime doc examples ignored |
| AOT executables | 4 passed |
| FFI | 19 passed |
| Operand rooting / stack / native transitions | 9 passed |
| Arithmetic boundaries / diagnostics | 5 passed |
| Project CLI / native stress selection | 6 passed |
| LLDB debugger / DWARF | 5 / 3 passed |
| Allocation profiling / benchmark command | 6 / 4 passed |
| Parser fuzz / GC ABI / generational | 5 / 1 / 4 passed |
| Heap command / diff / snapshot | 5 / 2 / 1 passed |
| Modules / reflection / stdlib integration | 8 / 14 / 9 passed |
| Checked I/O / parallel library / JIT and native search app | 7 / 1 / 3 passed |
| Comprehensive concurrency stress | Full default 50 iterations pass in debug and release |
| Shared-memory / threads / randomized thread stress | 15 / 13 / 3 passed |
| Example runner, bundled macOS Bash | 23 expected results pass in normal mode |
| Separate bounded stress example runs | 22 pass; full binary-tree stress run incomplete |

Validation commands:

```sh
cargo test --workspace
GCR_GC_VERIFY=1 cargo test --release --test search_app --test stdlib_parallel --test stdlib_io --test modules --test concurrency_stress --test project_cli --test aot --test ffi --test operand_roots
GCR_GC_VERIFY=1 cargo test --release --test concurrency_stress --test operand_roots --test arithmetic --test aot --test project_cli --test ffi
/bin/bash scripts/run_examples.sh
```

The release command uses the default 50 concurrency iterations, not a reduced
workload. Arithmetic tests execute optimized JIT output and check native AOT
behavior. Project tests observe zero major collections in normal execution and
at least ten with the stress flag, including a standalone native executable.

`/bin/bash scripts/run_examples.sh --gc-stress` passed atom and channel, then
spent several minutes in the full depth-16, 40-tree benchmark without finishing.
That process was stopped; no pass is claimed for it. All 22 other runner examples
passed separate runs with a 60-second per-process deadline. The unchanged full
binary-tree workload passes normal JIT and AOT execution.

## Defects repaired

### Stack growth in generated loops

LLDB showed an `alloca` subtracting from the stack pointer on every join-loop
iteration. Fixed-size match, variant, and FFI temporaries now allocate once at
function entry. Dynamic foreign buffers use stack save/restore around the call
and copy-back, so repeated calls release their storage.

A regression checks that fixed allocas belong to the entry block and runs
200,000 matches; another runs 200,000 native-buffer calls. Temporarily restoring
expression-site allocation makes the structural regression fail.

### Stale addresses across allocating scalar operands

The old normalization only hoisted GC-valued results. A scalar-returning call
could collect after codegen loaded a sibling object's address, so stores wrote
to stale space. This left join results unset and caused infinite polling.

Eager operands now finish and are rooted/snapshotted before the enclosing
operation loads them. Local snapshots also preserve left-to-right values when
a later operand reassigns the original local. Short-circuit right operands stay
conditional. Temporarily disabling scalar hoisting makes the regression return
10 instead of 52; the fixed code returns 52.

### Channel mutex / collection deadlock

A receiver could wait for the channel mutex while marked RUNNING, while a sender
held that mutex waiting to resume after GC. Collection waited for the receiver.
Mutex acquisition now publishes roots and marks the mutator BLOCKED, including
sender clone/drop paths. The unreduced comprehensive and shared-memory/channel
stress suites pass.

### Native-call state and moved copy-back destinations

Native entry now marks the mutator blocked after publishing roots. Native return
and callback reentry wait for collection before resuming managed execution.
Mutable-buffer copy-back reloads the array from its root after the call instead
of retaining a raw pre-call heap address. Foreign argument normalization keeps
buffer conversion and later argument side effects in source order.

Tests cover callback-triggered relocation, a sleeping C call while another
mutator collects, argument order, and repeated buffer calls.

### Project stress selection and example runner

Manifest-driven `run --gc-stress` propagates stress selection to the native
runtime. Standalone executables honor `GCR_GC_STRESS=1`; bare-file JIT runs honor
that variable too. The Bash runner uses indexed arrays supported by macOS Bash
3.2 and checks compiler/program exit status.

### Manifest validation

Typed TOML deserialization replaces the lenient handwritten parser. Malformed
syntax, wrong types, duplicates, unknown keys, invalid package names, and empty
entries are rejected. Quoted hashes, escapes, and multiline arrays parse
correctly. Discovery reports invalid parent manifests instead of suppressing
errors and running without their link configuration. Relative file arguments also
search ancestors above the working directory.

### Integer arithmetic and heap-setting overflow

Division/remainder by zero abort with a diagnostic. Signed minimum divided by
minus one wraps; its remainder is zero. Shift counts are masked by width minus
one. Checked multiplication rejects minimum times minus one in both orders.
See [overflow.md](overflow.md) for the tested contract.

Heap-setting conversion uses checked MiB-to-byte multiplication. Values that
would overflow fall back to the default instead of wrapping to a small heap.

### AOT test archive replacement race

A final parallel validation run caught an AOT fixture observing a missing runtime
archive immediately after a successful Cargo build. Compiler/dependency builds
could replace the same top-level archive. AOT fixtures now use a dedicated target
directory and share one runtime build per integration-test process. Release
fixtures build and link the release runtime rather than the debug runtime.
The full workspace and release AOT/FFI suites pass with the isolated artifacts.

## Application milestone

[gcr-search](../apps/gcr-search/README.md) indexes a directory into a persisted
text snapshot and answers case-sensitive literal queries with JSON Lines output.
The application is written in gc-rust. Checked host I/O returns typed results,
closes native resources inside calls, and supports real argv. Project execution
forwards arguments after `--` without treating them as compiler flags. String joining now uses linear assembly.

This workload exposed and repaired two compiler defects: statement-ending
returns lost their diverging type in blocks/conditional branches, and layout
conversion discarded canonical module names in nested fields. Regression tests
cover returning match/if branches and module-defined struct/enum/container fields.

Native reference comparisons cover Unicode, newline-containing names, CRLF,
empty trees, binary/excluded files, symlink cycles, deterministic replacement,
source changes after indexing, malformed inputs, and truncated snapshots. These
run under collect-on-every-allocation GC with subprocess deadlines. Full-repository
index/query execution is observed in normal mode through JIT and AOT with identical
query output. Search uses bounded parallel batches and ordered output. Persistent
worker pools, watch mode,
posting indexes, and HTTP/UI remain future work.

## Further defects exposed by parallel library work

Value arrays previously allocated/indexed using inconsistent strides and omitted
embedded GC references. They now store value elements in individually traced
boxes; this preserves value semantics at the cost of an allocation per store.
Nested value-enum references now occupy shared leading pointer slots rather than
untraced union bytes. Their raw payloads are 8-aligned and sized from the actual
nested layouts. Closure environments include embedded captured-value references
in their GC metadata and align scalar capture storage consistently with codegen.

`Sync` generic bounds now accept the structural sharing rules used by spawn;
qualified nominal types remain canonical in structural checks. Named functions
can be passed as managed callbacks, including generics with contextual types.
Argument lowering preserves already-inferred generic hints, including partial
function signatures with a concrete input and inferred output.

Parser condition/scrutinee restrictions no longer leak into delimited call
arguments, groups, or closure blocks. Build options are parsed completely,
independent of output/debug flag order, and unknown or duplicate flags are
rejected. Native/JIT argv preserves non-UTF-8 program arguments so the checked
library reports errors; invalid compiler arguments receive a diagnostic.

Execution-local JIT arguments are inherited immutably by child threads. Explicit
manifest `run --jit` forwards program stdout and exit status like native runs;
native link configuration is rejected in that mode. Tests cover independent
embedding contexts, invalid UTF-8 arguments, nested enum-valued worker results,
worker-count bounds, and ordered Unicode output under stress in both backends.

The proposed target is shared mutable managed objects, SC managed accesses,
structured threads/tasks and cooperative async cancellation; see
[concurrency.md](concurrency.md). Current alias/higher-order capture checks are
inconsistent and ordinary codegen does not meet the race-safety contract. Sync
restrictions remain transitional. Root-owned completion state now supports
repeated/concurrent joins; see the 2026-10-04 checkpoint below.

## Implemented, with limits

The implementation contains monomorphized generics, value aggregates, heap
structs/enums, traits, closures, Option/Result, strings, Vec/HashMap, modules,
Thread<T>, atoms/atomics/channels, generational GC, and source/heap tooling.
Passing component and stress tests support those particular behaviors.

Lowering rejects nested, tuple, and struct match patterns. FFI aggregate handling
uses AAPCS64 classification and rejects aggregate returns larger than 16 bytes.
The manifest handles metadata and native linking, not managed dependency
resolution. These are unsupported capabilities, not closed bug fixes.

Heap sizes are startup-configurable: 16 MiB nursery and 256 MiB tenured setting
by default. Allocation exhaustion aborts. Unchecked array/Vec access remains
ordinary callable functions with undefined behavior outside bounds; the public
safety boundary requires a design decision and audit. Floating casts and general
allocation size/offset arithmetic need further boundary review.

## Evidence still missing

No fresh comparative benchmark, visual widget validation, sanitizer run,
clean-machine install test, cross-platform ABI matrix, or sustained service soak
forms part of this assessment. No `.github` CI directory exists. Concurrent
collector scaffolding exists; these runs establish no supported concurrent-major
collection guarantee. Stress harness deadlines and longer automated soaks still
need a project-wide policy.

For new results, record date, checkout, host, command, environment, completed
coverage, failures, and skipped coverage. A code change alone does not close a
defect. Keep proposed capabilities in PRODUCTION.md until implementation and
verification support them.

## Concurrency contract work (2026-10-03)

[concurrency.md](concurrency.md) proposes version 1 semantics and records runtime
implementation gaps. `tests/concurrency_contract.rs` adds synchronized mutable
payload publication via start/join, channels and atomic signaling, checked in
JIT and AOT with moving-GC stress. It does not certify arbitrary managed races.

An additional lowering issue surfaced while constructing that fixture: an
inferred spawn closure with `if condition { return 81; }` followed by mutations
and a final `0` reports `expected (), found i64` at the early return. Rewriting
the body as an integer-valued if/else compiles. This inference issue was repaired at the 2026-10-04 checkpoint below;
it is separate from the concurrency memory-model implementation.

The new publication test passes in debug and release (`GCR_GC_VERIFY=1`), each
executing both JIT and native AOT with GC stress. Commands:
`cargo test --test concurrency_contract` and
`GCR_GC_VERIFY=1 cargo test --release --test concurrency_contract`.
The previously recorded 427/58 totals precede this added test.

## Execution ownership and closure inference — 2026-10-04

Checkout: `3a3d579` plus local changes. Host: Darwin ARM64, rustc 1.96.0.

`Thread<T>.join()` and scalar `thread_join` now support repeated and concurrent
observations through aliases. Root-owned native completion records keep borrowed
handle addresses stable. The first observer consumes the OS handle exactly once;
later observers acquire the completion mutex and obtain its published outcome.
All managed observers enter the GC blocking protocol before acquiring that mutex.
Managed result cells remain traced through surviving Thread aliases, preserving
reference identity and value-result copies.

JIT and AOT drain root children and newly spawned descendants before diagnostics
and code retirement. Root exit clears retired main frames and parks its mutator,
so child collection can proceed while draining. Completion metadata is released
at that boundary, even if an embedder keeps the heap. Metadata grows with total
spawns until root exit; nested scopes and earlier reclamation remain future work.
A nonterminating child can delay shutdown indefinitely. Fallible spawn, typed
failure/cancellation and the broader managed race-safety contract remain pending.
Native panic retains its previous scalar-zero behavior.

Closure lowering now infers a return constraint from explicit returns and checks
it against subsequent returns and the body result. Nested closures save/restore
that constraint. The original spawn-closure early return executes correctly;
return-only branches and incompatible value/unit returns have regression coverage.

Observed verification:

- `cargo test --workspace`: 434 passed, zero failures, five ignored runtime doc
  examples. Two runtime completion tests were added afterward and pass separately
  in debug and release; no new aggregate workspace total is claimed.
- `GCR_GC_VERIFY=1 cargo test --release --test thread_completion --test
  concurrency_contract --test concurrency_stress --test threads --test shared_mem
  --test threads_stress --test operand_roots --test search_app --test
  stdlib_parallel`: 52 passed. Comprehensive stress used its default 50 iterations.
- Final `cargo test --test thread_completion` and its release equivalent: six
  passed in each profile (four bounded JIT/AOT fixtures, incompatible-return checks,
  and the subprocess helper). The final repeat fixture retires an original alias
  before joining a surviving one.
- `cargo test -p gcrust-rt completion_tests` and
  `GCR_GC_VERIFY=1 cargo test --release -p gcrust-rt completion_tests`: two passed
  each, covering completion before first join and native-record reclamation with
  an externally retained heap.
- Negative controls: restoring the original runtime and matching JIT entry path
  makes the repeat-join subprocess abort (`failed to join thread: No such process`).
  Disabling closure return inference reproduces `expected (), found i64`.
  Restored implementations pass the completion suite again.
- `git diff --check`: clean.

Existing cross-platform, sanitizer, service-soak and full binary-tree GC-stress
coverage gaps remain. The next implementation step is managed SC access,
aggregate coherence, collection semantics and GC/barrier coordination.

## Collector coordination audit and repairs — 2026-10-04

Checkout `3a3d579` plus local changes, preserving the preceding ownership/inference
work. Darwin ARM64, rustc 1.96.0. The proposed heap-wide managed-access mutex was
withdrawn before implementation; it is not part of these changes.

Repaired issues:

- Card dirtying used plain racing byte stores and an incorrect benign-race claim.
  Cards now use AtomicU8 relaxed loads/stores. Safepoint publication supplies the
  ordering before STW scanning. A test concurrently marks shared/distinct cards.
- The coordinator wrote the poll flag atomically, but JIT polls were plain volatile
  loads. Runtime flag storage is now AtomicU8 with unchanged ABI offsets; generated
  loads are atomic acquire and volatile. IR checks cover optimized/unoptimized code.
- Minor GC never raised other mutators' JIT polls, including the major-before-minor
  branch. The new allocation-free worker reproduction timed out after ten seconds
  before repair. Minor collection now requests every worker before waiting and
  clears polls before resumption. Promotion capacity is checked after the world
  stops; required major/minor phases share a pause instead of resuming/reparking.
- Major collection cleared destination cards even though copied tenured objects
  still referenced nursery objects. A native unit reproduction failed before repair:
  the next minor left the child's pointer in the reset nursery. Destination cards
  are now rebuilt while scanning copied objects, with no additional heap pass.
  Tests cover fixed, interior and multi-card varlen references in both major paths.

Verification observed:

- `cargo test --workspace`: 440 passed, zero failures, five ignored doc examples.
- `GCR_GC_VERIFY=1 cargo test --release -p gcrust-rt`: 113 passed, one detector
  test ignored by its release annotation, plus five ignored doc examples. Running
  that detector explicitly with `--ignored --exact` and GC verification passes.
- `GCR_GC_VERIFY=1 cargo test --release --test gc_safepoints --test gc_abi_smoke
  --test generational --test concurrency_stress --test threads --test threads_stress
  --test thread_completion --test shared_mem --test operand_roots --test
  concurrency_contract --test search_app --test stdlib_parallel`: 59 passed.
  Comprehensive stress retains its default 50 iterations. The safepoint fixture
  uses 1 MiB nursery/tenured settings, verifies both minor and major collections,
  and enforces deadlines for JIT/native subprocesses.
- `git diff --check`: clean.

These totals describe the coordination-repair checkpoint. The following parallel
collector checkpoint supersedes its collector implementation and target choice.

## Parallel collector checkpoint — 2026-10-04

User selected parallel stop-the-world moving collection with concurrent application
mutators. Major and minor evacuation now use the same parallel engine. Collector
workers claim objects before allocating a destination copy, preserve the original
header without non-atomic access to its claimed word, and release-publish forwarding
addresses. Only the copy owner queues scanning. Batched queue transfers amortize
locking; termination requires an empty queue and no active producer batches.
Idle workers sleep. Dirty-card objects are deduplicated before minor scans, and
major scans preserve destination nursery cards. All worker joins precede space
swap, nursery reset and mutator resumption. Failed cycles cancel queue waiters and
propagate their panic; partially relocated heaps must not resume execution.

Destination reservations omit zero-fill before copying. The shared allocator's
cursor rounding/addition now checks overflow; a regression confirms an exhausted
cursor cannot wrap back into live objects. No external-project bug encountered.

`GCR_GC_WORKERS=N` sets total workers including the coordinator. Default: available
CPU parallelism capped at eight, with coordinator-only collection below 256 KiB
in the collected generation. Helpers are scoped OS threads per parallel cycle.
Details and explicit remaining costs are in [gc.md](gc.md).

Verification on Darwin ARM64, rustc 1.96.0:

- `cargo test --workspace`: **445 passed**, zero failures; one explicit throughput
  benchmark and five documentation examples ignored.
- `GCR_GC_WORKERS=4 GCR_GC_VERIFY=1 cargo test --release -p gcrust-rt --lib`:
  **118 passed**, zero failures, two ignored (throughput benchmark and annotated
  detector). The detector passes when explicitly selected with `--ignored --exact`.
  Runtime suites also passed with one and two workers during implementation.
- `GCR_GC_WORKERS=4 GCR_GC_VERIFY=1 cargo test --release --test gc_safepoints
  --test gc_abi_smoke --test generational --test concurrency_stress --test threads
  --test threads_stress --test thread_completion --test shared_mem --test operand_roots
  --test concurrency_contract --test search_app --test stdlib_parallel`:
  **59 passed**, zero failures, including default comprehensive stress iterations.
- New checks cover eight-way copy contention with exactly one destination,
  Full/Compact headers and byte payloads, cyclic/shared graphs with exact live
  occupancy through repeated major collections, duplicate root registrations,
  outstanding-work termination, and tagged cyclic nursery promotion from a
  multicard old object followed by major collection.
- `git diff --check`: clean.

Copy-heavy measurement:
`GCR_GC_WORKERS=N cargo test --release -p gcrust-rt parallel_copy_throughput --
--ignored --nocapture`, separately for N=1,2,4. The fixture has a single root into
4,095 binary-tree objects with 16 KiB byte payloads (67,256,280 live bytes).
Nine collections per run; discard two warmups and report the median of seven.

| Workers | Median collection pause |
| --- | --- |
| 1 | 2.316 ms |
| 2 | 2.086 ms |
| 4 | 1.674 ms |

Four workers improve this fixture's median by approximately 28% (1.38×). This is
one warmed copy-heavy microbenchmark, not evidence of general mutator scaling,
small-object throughput, or workload tail latency. Logs: `/tmp/gcr-parallel-copy-1.log`,
`/tmp/gcr-parallel-copy-2.log`, `/tmp/gcr-parallel-copy-4.log`.
Verification logs: `/tmp/gcr-parallel-final-workspace.log`,
`/tmp/gcr-parallel-final-runtime.log`, `/tmp/gcr-parallel-final-release.log`,
`/tmp/gcr-parallel-final-detector.log`.

Remaining work: persistent collector workers; worker-local copying allocation
and mutator TLABs with accurate extents/tails; broader scaling measurements;
generated SC managed accesses and coherent aggregate snapshots. The experimental
concurrent moving machinery remains unsupported. No sanitizer certification or
arbitrary managed-race safety is claimed by this checkpoint.

## Persistent workers and batched copying — 2026-10-04

Supersedes the scoped-worker/per-object-reservation implementation above.
Each heap lazily owns a persistent collector worker pool. Dispatch lends a job to
helpers synchronously and does not return or propagate a job panic until every
participant acknowledges completion. Worker counts may change between cycles;
excess helpers remain idle. Heap teardown shuts down and joins all helper handles.
No helper holds an owning heap reference. Tests cover worker reuse, changing
participation, coordinator/helper panic handling, and actual thread teardown.
Startup failures occur before job publication and release dispatch locks so
already-created helpers can shut down normally.

Workers reuse their batch/discovery/claim/layout/address buffers within a cycle.
They gather exclusive copy claims, reserve exact destination extents in one cursor
CAS per batch, initialize/publish copies, then publish discovered work. References
to BUSY copies are deferred as tasks rather than blocking with unpublished claims.
A deterministic two-worker test forces crossing claims; neither worker waits on
the other before publishing. Reservation failure leaves the destination cursor
unchanged. This avoids unused copying-buffer tails and preserves exact live sizes.

The new small-object benchmark exposed a genuine allocation-contention regression
that the earlier byte-copy benchmark had hidden. Persistent workers with per-object
reservation measured 4.755/11.249/30.387 ms at one/two/four workers on a single-root
262,143-node tree (10,485,720 live bytes). Batching changed this to
5.654/4.489/3.760 ms: an approximately 88% reduction at four workers, with about
19% additional single-worker overhead. Both measurements are recorded in the pad
cell `collector-allocation-regression-2026-10-04`.

Release microbenchmarks with persistent workers and batch reservation:

| Workers | 64 MiB payload graph | 10 MiB small-object graph |
| --- | --- | --- |
| 1 | 2.231 ms | 5.654 ms |
| 2 | 1.391 ms | 4.489 ms |
| 4 | 1.320 ms | 3.760 ms |

The four-worker speedups relative to the current one-worker backend are 1.69×
and 1.50× respectively. These are warmed fixture medians, not application pause
percentiles or general allocation-scaling claims. Command:
`GCR_GC_WORKERS=N cargo test --release -p gcrust-rt throughput -- --ignored
--nocapture --test-threads=1`, separately for N=1,2,4; nine collections per fixture,
two warmups discarded, median of seven. Logs: `/tmp/gcr-pool-bench-{1,2,4}.log`
(before batching), `/tmp/gcr-batch-bench-{1,2,4}.log` (after batching).

Verification:

- `cargo test --workspace`: **450 passed**, zero failures; two explicit throughput
  fixtures and five documentation examples ignored.
- `GCR_GC_WORKERS=4 GCR_GC_VERIFY=1 cargo test --release -p gcrust-rt --lib`:
  **123 passed**, zero failures, three ignored (two benchmarks and the annotated
  detector). The prior explicit detector run remains applicable; its implementation
  is unchanged.
- The same 59 selected release JIT/AOT checks listed above, with four workers and
  GC verification: **59 passed**, including default comprehensive stress iterations.
- One/two-worker release runtime suites pass as well. Pool-specific tests pass
  after the final startup-failure cleanup change.
- `git diff --check`: clean.

Logs: `/tmp/gcr-persistent-final-workspace.log`,
`/tmp/gcr-persistent-final-runtime.log`, `/tmp/gcr-persistent-final-release.log`,
`/tmp/gcr-persistent-runtime-{1,2}.log`, `/tmp/gcr-persistent-pool-final.log`.

Remaining costs/work: shared queue synchronization, remaining batched cursor
contention, root/card enumeration, cold pool startup, copying buffers with local
cursors, mutator TLABs with initialized extents/tail accounting, generated SC
managed accesses and coherent aggregate snapshots. No external-project bug was
encountered, and no sanitizer certification is claimed.

## Mutator TLABs and initialized allocation ranges — 2026-10-04

Runtime JIT/AOT allocation calls and Rust MutatorThread allocations now use
thread-local buffers for objects up to 8 KiB. Buffer targets start at 2 KiB,
grow on refill to a 32 KiB cap, accommodate larger current objects, and shrink
to remaining capacity. Large objects and public Heap allocation retain exact
shared reservations. Collect-on-allocation stress bypasses buffering.

Each buffer records an initialized prefix; active and retired tails are excluded
from heap walks. Descriptors survive mutator teardown until allocator reset.
Reset generations and allocator identity invalidate stale local buffers across
nursery reset and space flips. Major collection preserves nursery buffers and
updates their initialized references. Snapshot walks, nursery-as-major-roots
scanning, prewalk validation and remembered-card indexing honor these boundaries.
Physical occupancy includes reserved tails; initialized walks and allocation-site
bytes exclude them. No generated-code ABI changed.

Rust MutatorThread is now neither Send nor Sync, enforced by its type and a
compile-fail test. Its roots, profiling and local cursor belong to the registering
OS thread. A remaining low-level ThreadState ownership API issue is recorded in
the project pad (`thread-state-owner-api-audit-2026-10-04`): public ownership-sensitive
operations still need a complete API audit/enforcement pass. Runtime call sites
use the owning thread; the handle restriction alone is not certification of all
low-level embedding APIs or arbitrary managed races.

Two allocation/GC bugs found and repaired:

- AllocWindow exported a cursor address stored inline in a movable allocator.
  The cursor now has boxed stable storage; a move regression verifies its exported
  address and reads it after allocation. Window ABI is unchanged.
- Dirty-card indexing could skip an object crossing a leading card boundary when
  another object began later in that card. A native reproduction failed before
  repair and passes afterward. Index entries now identify the first initialized
  overlapping object in each card. Evidence: `/tmp/gcr-card-boundary-negative.log`,
  `/tmp/gcr-card-boundary-fixed.log`.

Final validation:

- `cargo test --workspace`: **457 passed**, zero failures. Three explicit benchmarks
  and five documentation examples ignored; the ownership compile-fail test passes.
- `GCR_GC_WORKERS=4 GCR_GC_VERIFY=1 cargo test --release -p gcrust-rt --lib`:
  **129 passed**, zero failures, four ignored (three benchmarks and the annotated
  detector, whose unchanged implementation passed explicitly earlier).
- Expanded selected release checks with four workers and GC verification:
  **73 passed**. The prior 59-check selection plus `heap_cmd`, `heap_snapshot`,
  `heap_diff`, and `alloc_profile` covers snapshots, CLI heap walks and profiling.
- Explicit rooted-chain regression passes with `GCR_GC_VERIFY=1 GCRUST_PREWALK=1`,
  exercising external major/prewalk entry as well as mutator major/minor paths.
- Tests poison active/retired tails, verify new fields are zeroed, invalidate
  partially used buffers after reset, preserve rooted chains across repeated
  major/minor cycles, check adaptive growth and stable cursor addresses, and
  reproduce the crossing-card reference loss.
- `git diff --check`: clean.

Logs: `/tmp/gcr-tlab-verified-workspace.log`, `/tmp/gcr-tlab-verified-runtime.log`,
`/tmp/gcr-tlab-verified-release.log`, `/tmp/gcr-tlab-prewalk.log`.

Allocation fixture after adaptive buffering and stable cursor storage:

| Workers | Objects allocated | Shared cursor | TLAB |
| --- | --- | --- | --- |
| 1 | 250,000 | 1.384 ms | 2.019 ms |
| 2 | 500,000 | 17.591 ms | 2.119 ms |
| 4 | 1,000,000 | 114.859 ms | 2.235 ms |

Each worker allocates 250,000 32-byte objects into a 64 MiB semi-space heap; no
collection occurs during timing. Both paths initialize objects. Five fresh-heap
runs per mode, median elapsed time; timing covers the synchronized allocation
phase and worker joins. Post-run heap walks verify the exact object count.
Command: `GCR_ALLOC_BENCH_WORKERS=N cargo test --release -p gcrust-rt
mutator_tlab_allocation_throughput -- --ignored --nocapture --test-threads=1`,
separately for N=1,2,4. Logs: `/tmp/gcr-tlab-adaptive-allocation-{1,2,4}.log`.
Four-worker TLAB time is about 51× lower than this contended shared-cursor baseline;
single-worker time is about 46% higher. This isolates allocation contention and
does not establish application speedups, GC tail latency or retained-heap scaling.

Remaining work: low-level owner/collector API enforcement, generated SC managed
accesses and coherent aggregate snapshots, collector-local copying cursors and
broader scheduling/workload measurements. No external-project bug encountered.

### Allocation stress audit (2026-10-04)

`--gc-stress` / `GCR_GC_STRESS=1` collect before every managed runtime
allocation and bypass TLABs. Fixed request coalescing under contention and
empty-nursery skips: each stress request now acquires its own collection pause,
servicing safepoints while waiting. The four-mutator regression asserts exactly
400 collections for 400 allocations in both semispace and nursery paths.
Restoring the old behavior fails the regression (188 versus 400 collections).
Raw heap allocation primitives remain noncollecting; embedding callers use
registered mutators. Release runtime validation with four collector workers
and heap verification: 130 passed, zero failures. Release JIT/native integration
checks cover project CLI stress, thread completion, threads, parallel library,
and GC ABI paths; all pass. Logs: `/tmp/gcr-every-allocation-runtime.log`,
`/tmp/gcr-every-allocation-integration.log`,
`/tmp/gcr-every-allocation-negative.log`.

Prebuilt final-source workspace run: **458 passed**, zero failures. Log:
`/tmp/gcr-every-allocation-workspace-prebuilt.log`. A preceding run exposed
a separate native-runtime archive rebuild race in concurrent CLI tests
(missing `libgcrust_rt.a` after a stale rebuild); recorded as open in status
pad cell `native-runtime-rebuild-race-2026-10-04`.

## Application and atomic contracts checkpoint — 2026-10-04

`apps/gcr-logstats` adds a real parallel UTF-8 log analyzer. Fixed workers claim
input occurrences using AtomicI64 fetch-add and publish immutable summary
references through Atom CAS. Counts include files, bytes, lines, ERROR lines and
WARN lines. Worker/file limits and errors are documented in its README. All
workers are joined; read failure produces no partial success summary.

Native AtomicI64 cells previously leaked via Box::into_raw. They are now stable
root-execution-owned Arc allocations, released after all children/descendants
drain, including when an embedder retains the heap. Aliases remain valid through
registry growth. Cells still accumulate until root exit; earlier reclamation is
pending for long-lived roots.

Atom<bool> previously emitted invalid atomic i1 load/CAS operations (LLVM verifier
reproduction: /tmp/gcr-atom-bool-before.log). Bool construction now initializes a
canonical full byte; operations use byte atomics and convert to/from language
bool. Float atoms use integer-bit atomic operations; CAS distinguishes signed
zero and matches unchanged NaN bits. Inline aggregates/unit get frontend errors,
and atom intrinsic arity is checked before indexing arguments. The scalar/type
regressions run JIT and native, normal and every-allocation GC, with bounded child
processes. These repairs do not implement general managed race semantics.

Current low-level ThreadState record_alloc and alloc_site_counters already have
unsafe signatures and documented owner/quiescence requirements. The earlier pad
finding about safe record_alloc no longer describes its current signature;
public frame_chain and the broader owner/collector API enforcement remain open.

Verification completed across midnight, 2026-10-04/05, on Darwin ARM64:

- `cargo test --workspace`: **462 passed**, zero failures; three explicit runtime
  benchmarks and five documentation examples ignored. The ownership compile-fail
  test passes. Log: `/tmp/gcr-app-final-source-workspace.log`.
- `GCR_GC_WORKERS=4 GCR_GC_VERIFY=1 cargo test --release --test atom_types --test
  logstats_app --test shared_mem --test thread_completion --test search_app --test
  concurrency_contract --test concurrency_stress`: **29 passed**, zero failures;
  comprehensive stress uses its default 50 iterations. Log:
  `/tmp/gcr-app-final-source-release.log`.
- Atomic native-cell lifetime regression passes separately in debug and release;
  log `/tmp/gcr-atomic-lifetime-release.log`.
- A native gcr-logstats run over three actual compiler/test logs agrees with a
  serial byte/line reference (3 files, 11,249 bytes, 247 lines). Evidence:
  `/tmp/gcr-logstats-real-logs.json`. This is correctness evidence, not a benchmark.
- `git diff --check`: clean. After the test builds, an ineffective Inkwell
  cmpxchg alignment setter was removed; it always returned an ignored error, so
  generated alignment remains LLVM's natural operand alignment. The external
  accessor limitation is recorded in the project status pad.

Application milestones are in PLAN.md. General managed SC accesses/aggregate
coherence, low-level ownership enforcement, channel-close semantics, structured
outcomes/scopes, cancellation, async, the native archive rebuild race, and release
platform/soak gates remain open. No sanitizer or production certification claimed.

## Managed memory and GC handoff checkpoint — 2026-10-05

Generated mutable scalar/reference fields and array slots use naturally aligned
SC loads/stores, including byte storage for bool. Inline value-aggregate fields
use 64 address stripes per heap. Acquisition roots the object, parks while
waiting, reloads its relocated address and revalidates the stripe before copying.
The critical region performs only the complete aggregate copy and embedded
reference barriers. Unlock performs an SC RMW as the aggregate linearization
event. There is no allocation, safepoint or foreign call while a stripe is held.
Array value elements remain immutable traced boxes, with atomic slot publication.
Native channel slots and string-array reads use SC atomics; FFI scalar buffer
copies perform SC accesses per element (not a whole-array snapshot).

Plain accesses remain for privately initialized constructors, immutable
headers/counts/string bytes/captures/boxed value elements, and collector-only
relocation during a world pause. Arbitrary managed sharing is not yet certified:
collection metadata/structural mutation, unpublished or null reference slots,
unchecked library accesses and the low-level owner/collector root API remain
open. Transitional Sync capture checks have not been removed.

Two GC handoff defects are repaired. Stop requests arriving between ThreadState
registration and poll-flag installation are retained under a registration mutex.
Pause release clears polls, releases the collection request and resumes states
under the registry lock, before blocked mutators can resume or retire poll
storage. Three startup-order tests and a 32-round/16-worker retirement test cover
these interleavings. An earlier selected release stress run hit a native allocator
SIGTRAP; the retirement audit found a concrete defect, but the original trap's
cause is not independently established. The final full default stress passes.

Verification on Darwin ARM64:

- `cargo test --workspace`: **468 passed**, zero failures, eight ignored
  (three explicit benchmarks and five documentation examples).
  Log: `/tmp/gcr-managed-workspace.log`.
- `GCR_GC_WORKERS=4 GCR_GC_VERIFY=1 cargo test --release --test managed_memory
  --test concurrency_stress --test concurrency_contract --test shared_mem
  --test thread_completion --test search_app --test logstats_app --test ffi
  --test operand_roots`: **56 passed**, zero failures. Stress uses its unchanged
  default 50 iterations. Log: `/tmp/gcr-managed-final-release.log`.
- An additional optimized/unoptimized LLVM structural check verifies atomic
  scalar loads/stores and aggregate regions without collecting calls; passed.
  Log: `/tmp/gcr-managed-ir-check.log`. It was added after the two suite builds.
- `scripts/check_thread_sanitizer.sh` instruments generated native code, the
  Rust runtime and rebuilt Rust std. Managed-memory normal/stress fixtures and
  four native poll/retirement regressions pass using Apple's external sanitizer
  runtime. JIT is covered by behavioral tests, not sanitizer instrumentation.
  This gate currently supports macOS ARM64 only.

Rust nightly's bundled sanitizer crashed during dyld initialization before any
native unit test ran. The external-runtime gate explicitly links Apple's runtime
because Rust supplies `-nodefaultlibs`; it retains full instrumentation and uses
no suppressions. Startup evidence and the ownership/collection backlog are in
the project status pad. The full implementation plan remains in progress.

## Collection boundary and initialization checkpoint — 2026-10-05

Legacy direct-return array reads now validate the actual immutable backing-array
length before emitting an inbounds element address. This protects prelude
Vec/Map reads when separately observed mutable metadata no longer describes the
array. Invalid reads abort with the existing bounds diagnostic. The legacy
`unchecked` names are retained but no longer bypass this check.

Array constructors reject negative lengths and multiplication overflow before
computing the allocator count. Variable-length allocation checks the complete
TypeInfo size, count/header overhead, alignment and isize pointer-offset limit.
A native unit test checks agreement with ordinary layout sizing across headers,
fields, byte/value tails and alignments, and rejects overflowing tails.

Unwritten reference and value-array slots are internal null storage, with no
universal source-language default. All generated read paths now fail with an
uninitialized-element diagnostic before a null reference or fabricated inline
aggregate can escape. Native string-array join uses the same rule. Scalar arrays
retain zero/false initialization. See language.md for the observable contract.
These are currently diagnostic aborts; typed failure propagation belongs to the
outcomes/structured lifetime work. Collection operations still are not atomic
transactions, and native channel storage ownership remains open.

Validation:

- `cargo test --workspace`: **470 passed**, zero failures, eight existing ignores.
  Log: `/tmp/gcr-collection-workspace.log`.
- Release selection with four collector workers and heap verification:
  collection_boundaries, managed_memory, concurrency_stress, ffi, search_app,
  logstats_app: **27 passed**, zero failures. Full default concurrency workload.
  Log: `/tmp/gcr-collection-release.log`.
- Allocation-size unit test passed separately; log:
  `/tmp/gcr-allocation-size-unit.log`.
- `scripts/check_thread_sanitizer.sh`: native generated managed-memory fixture
  passes normal/stress GC and all four poll/retirement regressions pass.
  Log: `/tmp/gcr-collection-tsan.log`. No JIT sanitizer coverage is claimed.
- `git diff --check`: clean.

The audit confirmed Channel::new(0) allocates no slots while native metadata
clamps capacity to one. Mutable buffer/control fields can also mix queue state
with unrelated storage. Canonical rooted native queue ownership, positive
capacity, resource retirement and proper close/outcome semantics remain required
in PLAN sections 2/3. Details are recorded in project status pad cell
`channel-backing-buffer-contract-2026-10-05`. The full goal remains active.

## Channel ownership and close checkpoint — 2026-10-05

Native fixed-address queue slots replace the managed-buffer/control split.
All major/minor collector paths and heap root enumeration scan these slots.
The managed zero-length witness array carries only the generic type; modifying
it cannot change queue storage. Runtime array/message type checks prevent valid
handles from different generic channels from causing representation confusion.
Capacity must be positive and representable. Public Channel<T> boxes initialized
scalar, reference and inline-value messages; the previous scalar-channel codegen
pointer cast failure is removed.

The public signatures change to `send(T) -> Result<(),T>` and `recv() -> Option<T>`.
A closed send returns the original value, preserving reference identity. Receive
reports None only after close and draining. `recv_value()` is a convenience for
protocol-required messages and fails explicitly on closed/drained queues. Close
is an idempotent state transition with notifications to every sender/receiver.
FIFO order and close/send commitment linearize under the queue state mutex.
Synchronized waiter counts let tests prove participants actually parked before
closing. No GC finalization is needed for close.

Closed drained slot storage is reclaimed immediately. Slot-storage locking occurs
only in RUNNING, noncollecting regions, so root enumeration under a world pause
can inspect it without waiting for a parked owner. Stable handle metadata stays
root-execution-owned until children drain; a weak-ownership regression verifies
actual reclamation with an externally retained heap. Earlier metadata reclamation,
async/cancellation-aware channels, execution outcomes/scopes and locks remain open.

The unchanged selected release workload passes **29 tests**, with four collector
workers and heap verification: channel_contract, collection_boundaries,
concurrency_contract, concurrency_stress (default full 50 iterations), shared_mem,
search_app, logstats_app and thread_completion. Native ThreadSanitizer passes the
managed-memory and complete channel fixtures in normal/every-allocation GC, all
four poll regressions and the native channel retirement test. The channel fixture
covers two actually blocked senders/receivers, repeated close, a 100-message FIFO
pipeline, 64 close/send races, reference identity, inline values/floats/bools,
witness replacement and message roots surviving moving GC. The example channel
program still reports 109900.

Logs: `/tmp/gcr-channel-final-release.log`, `/tmp/gcr-channel-final-tsan.log`,
`/tmp/gcr-channel-example.log`. The first workspace attempt failed five CLI tests
with ENOENT during concurrent replacement of target/debug/gcr by the sanitizer
compiler build; it is not counted as a pass. The sanitizer gate now builds its
compiler in target/tsan-compiler, isolated from in-flight workspace executables.
Final workspace validation is recorded below once complete. The full PLAN goal
remains active.

Final channel workspace run: **472 passed**, zero failures, eight existing ignores.
Log: `/tmp/gcr-channel-final-workspace.log`. A subsequent frontend-only hardening
requires RawPtr control arguments for close/clone/drop and adds malformed-call
regressions; the focused channel suite passes separately, including the unchanged
JIT/AOT normal/stress fixture. Log: `/tmp/gcr-channel-frontend-final.log`.

## Root registration and runtime retirement checkpoint — 2026-10-05

Raw root enumeration is now an unsafe trait operation with explicit storage,
quiescence and pointer-policy requirements. Raw stack-frame/TLS registration is
unsafe because forgetting its guard can outlive stack storage; callback APIs keep
the guards private and clean up on unwind. Owned scopes unlink their exact frame,
including when scopes are dropped out of registration order.

ThreadState root storage is private and owner-checked. Scratch tokens cannot move
between threads; publication/reset, GC state transitions, allocation and polling
entry points now state their unsafe caller obligations. Classic Mutator allocation
and SATB reference publication likewise require valid layouts/references.
Deregistration detaches the poll byte before native Thread storage is freed, even
when an external Arc retains the ThreadState. Regressions exercise wrong-owner
access, unwind cleanup, scope unlinking and retained-state poll operations.

This does not complete the low-level allocation/collector API audit or collection
operation consistency. Sync capture restrictions remain. Final validation for this
checkpoint is recorded in the project status pad after the running gates finish.

Validation after the root API changes: `cargo test --workspace` passes **480
 tests**, zero failures, eight ignored (three benchmarks and five documentation
 examples). The optimized channel/boundary/concurrency/shared-memory/search/logstats/
 completion suites pass **30 tests**. `scripts/check_thread_sanitizer.sh` passes
 managed-memory and channel fixtures in normal/stress GC plus six poll regressions
 and the channel metadata retirement regression. Native AOT is instrumented;
 JIT sanitizer instrumentation remains unavailable. `git diff --check` is clean.

## Raw allocator extent checkpoint — 2026-10-05

Legacy bump, shared atomic bump and TLAB reservations validate descriptor/length
arithmetic before reserving memory. Bump cursor padding and end calculation use
checked additions. Invalid sizes/alignment shifts return null without advancing
cursors or publishing buffers. Two native regressions check rejection, unchanged
initialized prefixes and successful subsequent allocations. The wider allocator
alignment/header and collector ownership API audit remains open.

## Alignment and reset checkpoint — 2026-10-05

Legacy bump allocation now aligns absolute addresses and records object starts so
walking skips mixed-alignment padding. Existing over-alignment support is preserved;
a mixed ordinary/64-byte-aligned externally backed arena has a traversal/reset
regression. Atomic managed arenas and TLABs accept eight-byte alignment and reject
other descriptors before reservation. Batch validation rejects unsupported alignment,
zero sizes and non-word extents. Checked TypeInfo sizes reject sub-word alignment.
Arena reset now has an unsafe exclusive-access/retired-allocation contract.
The broader collector API and collection consistency audit remains open.

Validation: **147 runtime unit tests and three compile-fail documentation tests
pass**, zero failures; three benchmarks/five examples ignored. Five optimized
allocator/layout regressions and eight optimized application/concurrency checks
pass. Application checks include JIT/AOT serial-model comparisons under moving-GC
stress. `git diff --check` is clean. Exact commands/logs are in the status pad.
