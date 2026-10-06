# Implementation plan

Updated 2026-10-06. Completed checkpoints are identified below; the remaining
sections describe proposed work.
The target semantics are in [concurrency.md](concurrency.md); verified behavior
and open defects are in [STATUS.md](STATUS.md). Release gates are in
[PRODUCTION.md](PRODUCTION.md).

## 1. Execution ownership and compiler inference — implemented 2026-10-04

Root-owned native completion records replace consuming raw-handle ownership.
`Thread<T>.join()` and scalar `thread_join` can observe completion repeatedly and
concurrently. The OS join handle is consumed once; managed results stay traced by
live Thread aliases. Root exit drains children and descendants before diagnostics
or JIT-code retirement, then releases completion metadata. Native panic still maps
to zero. Nested scopes, fallible spawn, typed outcomes and cancellation remain in
later steps. Completion metadata currently grows with total spawns until root exit.

Inferred closure return types now reconcile explicit returns with the body result,
including early returns, return-only branches and nested closures; incompatible
return types are rejected. The original publication reproduction is executable.

Regression coverage: `tests/thread_completion.rs` runs aliased and concurrent
joins, dropped aliases, GC-relocated reference/value results, abandoned descendants
and the original inference reproduction in bounded JIT/AOT subprocesses. Runtime
unit tests cover completion before first join and reclamation with an externally
retained heap. Current verification commands/results are in STATUS.md.

## 2. Make managed sharing safe — in progress

The 2026-10-04 collector audit repaired atomic poll/card races, allocation-free
minor-GC coordination, and preservation of nursery edges across major collection.
The selected collector target is parallel stop-the-world moving collection with
concurrent application mutators. Major and minor evacuation now use atomic copy
ownership, batched worker tasks and quiescence detection. The heap-wide
managed-access mutex proposal was withdrawn before implementation. Persistent
collector workers, exact batched copy reservation and mutator TLABs with
initialized-range descriptors are implemented. Collector copying buffers
with local cursors and the remaining sharing/API audit remain work; details
and measured limits are in [gc.md](gc.md).

Generated mutable scalar/reference fields and array slots now use SC atomics.
Inline aggregate field reads/replacements use relocation-aware address stripes
and an SC linearization event; critical regions contain no safepoint or
allocation. Native channel/string-array accesses and FFI scalar buffer copies
use matching atomics. Mixed store buffering, enum/value replacement and live
embedded references pass JIT/AOT, normal/stress GC. Optimized LLVM structural
checks and the native ThreadSanitizer gate pass. Startup poll registration and
pause-release/retirement handoffs have focused regressions.

This is a partial checkpoint, not the section's exit criterion. Ordinary Vec/Map
remain unsynchronized, following the JVM model: concurrent mutation requires
external synchronization, including for backing-array aliases. Their operations
need memory-safe boundaries, not automatic transactions or snapshot guarantees.
Separate concurrent versions will have explicitly specified atomic operations and
iteration semantics after the synchronization foundations exist. Root registration/enumeration and GC transitions now have
explicit unsafe contracts; scratch storage is owner-checked and poll retirement
detaches native storage before freeing it. Header/layout validation, exclusive type registration, raw heap allocation
contracts and legacy collector alignment/forwarding repairs are implemented
2026-10-06. Keep auditing caller obligations and generated allocation paths.
Backing-array bounds and unwritten reference/value slots now
produce defined diagnostic failures; array allocation arithmetic is checked.
Transitional Sync capture checks remain in place. See STATUS.md for exact
coverage and sanitizer limits.

Inventory every heap access, aggregate copy, reference publication and write
barrier. Implement the proposed SC managed memory model and coherent aggregate
snapshots, coordinating publication and relocation with the collector. Audit
collection boundaries for managed memory safety under unsynchronized mutation;
do not add implicit locks or atomic operation guarantees to ordinary Vec/Map.
Preserve reference identity and initialize every published representation.

Run store-buffering, racing scalar/reference accesses, enum replacement and
old-to-young mutation during collection. Instrument native runtime races and
state explicitly what generated code is covered. Verify optimization does not
introduce undefined LLVM behavior. Remove transitional Sync restrictions only
once arbitrary managed sharing is safe, consistently across literals, aliases,
generics and higher-order calls.

Exit criteria: the memory-model conformance cases pass in JIT/AOT under GC stress;
no reliance on capture rejection to preserve managed memory safety.

## 3. Add synchronization, outcomes and structured lifetimes

Channel ownership/close checkpoint implemented 2026-10-05: native queue slots
are traced in all collector/root enumeration paths; witnesses cannot replace
queue storage. Positive capacity is validated. Scalar, reference and inline-value
messages use initialized boxes. `send` returns Result<(),T> with the unsent value
on close; `recv` returns Option<T>, with None only when closed and drained.
Explicit close is idempotent and wakes all sender/receiver waiters. Closed,
drained slot storage retires immediately; stable handle metadata retires after
root children drain. Earlier handle reclamation and cancellation remain work.
FIFO, wakeups, close/send races, value preservation and JIT/AOT moving-GC behavior
have bounded regressions and native sanitizer coverage. This does not finish the
lock, typed execution-outcome or nested scope requirements below.

Implement reentrant locks and condition waits, with lexical cleanup. Build
separate concurrent Vec/Map APIs with defined atomic operations and iteration
semantics; ordinary collections retain external synchronization. Replace
channel close with an explicit idempotent state transition, waking all waiters
and returning unsent values. Define positive-capacity validation and FIFO
linearization. Add typed success/failure/cancelled completion, fallible spawn,
child registration, scope join and deterministic failure aggregation.

Exit criteria: blocked senders/receivers wake on close; concurrent close/send
never loses a value; failed and abandoned children remain scope-owned; repeated
completion observation is safe.

## 4. Add cancellation and shutdown

Add cooperative checkpoints, cancellation-aware blocking operations and waiter
removal. Specify cleanup masking and cleanup failure reporting. Root shutdown
stops admission, cancels and drains children. Native operations must declare
whether cancellation can interrupt them or waits for completion.

Exit criteria: cancellation races complete once, release waiters/resources and
preserve committed effects; shutdown leaves no silently detached executions.

## 5. Implement async tasks

Lower cold futures to resumable rooted frames. Build task scheduling, task-local
state, explicit pinned execution and a bounded blocking executor. Reject lock
guards across suspension. Share outcomes, cancellation and channel state with
blocking APIs. Await must free the worker to run another task.

Exit criteria: continuation references survive moving GC while suspended; task
migration preserves task locals; JIT and AOT produce the same observable outcomes.

Development native archive production/link ownership is implemented: dedicated
profile/target Cargo caches, a lock held through linking, fatal producer errors,
concurrent-build regressions, and error-preserving benchmark output. Packaged ABI
versioning and clean-machine delivery remain separate release requirements.

## 6. Extend the application and release evidence

Keep gcr-search as the standard-library workload. Extend it to a long-running
index/query service after scope, shutdown and async support exist. Add bounded
request queues, filesystem refresh, cancellation, diagnostic metrics and explicit
resource limits. Grow filesystem, strings, collections and task APIs through
real application requirements, with reusable library implementations.

Automate clean-checkout tests, native packaging and bounded stress on declared
platforms. Retain longer soaks with seeds and failure artifacts. Measure RSS,
retained heap, GC pauses, throughput, startup and compilation costs; choose numeric
release budgets before declaring production readiness. Finish or explicitly scope
the outstanding full binary-tree GC-stress workload.

Exit criteria: application correctness survives refresh/shutdown/load; declared
resource budgets and reproducible delivery gates pass. macOS ARM64 evidence alone
does not establish support for other targets.

## Evidence at this checkpoint

Before the concurrency documentation/test addition: 427 workspace tests, 58
selected release checks and 23 normal examples passed. The new synchronized
publication test passes in debug and release, each executing JIT and native AOT
under moving-GC stress. Full-suite totals were not rerun after that addition.
Exact commands and limitations are recorded in STATUS.md. None of the future
concurrency features above is certified by those counts.

## Application-driven acceptance workloads — 2026-10-04

Use real applications alongside focused memory-model litmus tests. An application
passing through a synchronized subset does not certify arbitrary managed races.

1. **gcr-search**: preserve persisted-index/ordered-query correctness. Extend to
   incremental refresh and concurrent queries when scope/cancellation semantics
   exist. Publish immutable index generations through Atom; exercise bounded
   request queues, refresh cancellation, shutdown and eventually networking/async.
2. **gcr-logstats**: implemented bounded parallel UTF-8 file summaries. Fixed
   workers claim files using AtomicI64 and publish immutable summaries using Atom
   CAS. Serial differential checks cover JIT/native, normal/stress GC and I/O
   failures. Extend to streaming live monitoring after resource lifetimes and
   cancellation are defined; exercise blocked readers, queue close and shutdown.
3. **gcr-csvreport / gcr-buildplan / gcr-routes**: implemented bounded CSV
   grouping, dependency scheduling and directed cheapest routes. The reporter
   uses fixed workers, local ordinary maps and a bounded completion channel;
   schedulers/routes use owner-local minimum heaps. Independent grouping,
   scan-scheduler and Bellman-Ford models exercise native/JIT and moving GC.
   Continue growing these through streaming and cancellation once resource
   lifetimes are defined; preserve explicit synchronization for concurrent APIs.
4. **Service acceptance**: sustained requests, refresh/monitor activity, client
   disconnects and shutdown during GC. Add networking through real service needs,
   then bounded async request handling; require correctness and numeric memory,
   latency and shutdown budgets before declaring the service gate complete.

AtomicI64 cells now have stable root-execution ownership and are reclaimed after
children drain. Cells remain retained until that boundary; earlier reclamation
and bounded long-running resource lifetimes remain work. Never free a native
atomic based on one alias disappearing.
