# Production language roadmap — 2026-10-13

Implementation started with item 1. Track completed repairs, verification and
remaining safety work in [SAFETY.md](SAFETY.md) and [STATUS.md](STATUS.md).

Assessment of checkout `40321f8`, branch `inline-closure-alloc`; working tree clean when inspected. Source, tests, repository documentation and existing project pads reviewed. No new full test run or benchmark was performed for this assessment. This is a comprehensive engineering backlog, not a fresh certification or a promise that every conceivable language feature is required.

## What already exists

LLVM JIT/AOT; precise moving generational GC with parallel stop-the-world evacuation, TLAB allocation and inline closure allocation; structs/enums/inline values; generics, methods, traits and operator traits; closures; nested patterns and exhaustiveness; later-use inference; array literals, ranges and collection loops; Option/Result and `?`; built-in formatting/assertion macros; method-based String/Vec/HashMap APIs; modules and manifests; checked application I/O; threads, atomics and closeable bounded channels; source debugging, heap inspection and compiler IR inspection. Five real CLI apps exercise independent reference models and GC stress.

The October 12 pad records 586 passing tests and eight ignores for the prelude migration. October 6 records 520 workspace tests, 46 selected release tests and native TSan gates. These are historical, differently scoped checkpoints. Current source is newer than the older production docs. References (`&T`, `&mut T`, dereferencing) are deliberately excluded from this language.

## 1. Finish the language and safety contract — release blocker

- Specify evaluation order, identity vs structural equality, assignment/copy/clone behavior, alias-visible mutation, closure capture and value aggregate semantics.
- Specify initialization, constructor publication and unwritten-array behavior, including racing reads.
- Finish arithmetic/cast semantics: float-to-int NaN/infinity/out-of-range, integer narrowing/sign changes, float exceptional values, Unicode char validity and checked size/offset arithmetic.
- Establish the public safety boundary for RawPtr, extern calls, callbacks and callable unchecked intrinsics: explicit unsafe capability or inaccessible internal primitives with checked public APIs.
- Managed slots follow the plain-field model in `docs/gc.md` (unordered scalars, release/acquire reference publication); still distinguish indivisible access from compound operations. Ordinary Vec/Map stay externally synchronized.
- Audit every generated/runtime heap access, root/reload, aggregate snapshot, allocation publication and write barrier. Cover generic specializations, embedded references, closures, callbacks, thread handoff, diagnostics and optimized paths.
- Audit Rust embedding APIs for thread ownership, lifetimes and collection coordination; document which APIs can safely execute concurrently.
- Remove transitional Sync capture restrictions only after arbitrary managed sharing is actually safe, including aliases and higher-order calls.

Done when boundary behavior is specified and tested in optimized JIT/AOT, adversarial sharing preserves memory safety, and safe embedding APIs cannot violate mutator ownership.

## 2. Deterministic native resource ownership — release blocker

Current GC follow-up: joined safe Thread completion records can retire when their
wrapper dies, and main/child mutator registration now has owning cleanup before
poll storage is freed, including failed setup. Owning closure handoff also clears
and reuses env roots when admission fails before registration. Cleanup failure is fatal before
code/storage retirement. Safe AtomicI64 cells now follow a GC wrapper and retire
after its last alias dies; raw handles keep execution lifetime. Never-joined
completions, channel lifetimes,
lexical resource cleanup, admission failure and typed outcomes remain open.

- Choose lexical cleanup semantics for files, sockets, native objects and lock guards, including early return, `?`, failure and cancellation.
- Define alias behavior, double-close, use-after-close, ownership transfer and thread affinity for resource wrappers.
- Add cleanup scopes/guards and explicit close APIs; do not rely on GC timing for timely cleanup.
- Finish native bookkeeping reclamation: joined safe Thread and safe AtomicI64 owners now retire at GC; never-joined completions, channels and unbound raw handles still retain execution lifetime.
- Define panic/failure propagation and cleanup. Replace native child panic's scalar-zero behavior with an observable typed outcome.
- Define failure during cleanup and foreign unwinding boundaries.

Done when repeated resource cycles and abandoned aliases remain bounded, and every supported exit path performs the documented cleanup.

## 3. Synchronization and structured concurrency — blocker for shared mutable applications

- Add reentrant locks, lexical guards and condition waits integrated with GC blocking transitions.
- Specify lock execution identity, recursive depth restoration, waiter registration and spurious wakeups.
- Add nested scopes, reliable child registration and scope-owned completion; retain safe repeated/concurrent joins.
- Add fallible spawn, typed success/failure/cancelled outcomes and deterministic failure aggregation.
- Add timeouts/deadlines to waits, joins and channels where practical; specify commitment races.
- Add separate concurrent map/vector APIs with defined operation and iteration/snapshot semantics. Ordinary collections do not gain implicit locks.
- Bound worker/thread creation and pending work; prevent completion bookkeeping from growing with total historical spawns.

Done when failed/abandoned children stay owned, waits cooperate with GC, and synchronization/publication conformance passes under contention and stress.

## 4. Cancellation and process shutdown — blocker for services

- Add cooperative cancellation tokens/checkpoints, including allocation-free loops and recursion.
- Make blocking waits cancellation-aware; unregister cancelled waiters without losing committed sends/receives.
- Define cleanup masking and reporting of cleanup failure.
- Stop new work admission, cancel and drain descendants on shutdown; handle signals through safe runtime coordination.
- Specify native-operation interruption vs waiting for completion; expose operations that may delay shutdown.
- Define shutdown deadlines and an explicit escalation policy for noncooperative foreign work.

Done when shutdown during blocked I/O, channel contention and GC completes within an agreed budget and leaves no silently detached execution.

## 5. Async execution — planned capability, required for the proposed async service target

- Implement async syntax and cold single-execution futures, await and scoped task spawning.
- Lower resumable frames with precise GC roots for suspended locals, values and captures.
- Implement scheduling, wake registration, duplicate-wake/complete races, fairness and queue bounds.
- Preserve task-local state through migration; support explicitly pinned/thread-affine work.
- Build a bounded blocking executor and timers; integrate readiness-based I/O.
- Reject lock guards across suspension; share cancellation, outcomes and channel state with blocking APIs.

Done when suspended references survive relocation, awaiting frees workers, and JIT/AOT agree under cancellation and migration. Async is not required to ship a sound CLI language.

## 6. Runtime resilience and GC operating limits — release blocker

- Specify initial/maximum heap, large allocations, live-set exhaustion, OOM behavior and whether heap growth/shrink is supported.
- Check all collection capacity, object size, alignment and offset calculations, not only array allocation.
- Define stack/recursion limits. Foreign String/scalar-array arguments now use scoped native heap storage and rooted bounded-copy safepoints; current FFI buffers now have accounted ownership and BLOCKED large allocation/free. Hard native admission and remaining allocation domains are open.
- Keep GC progress possible through allocation-free execution and blocking native calls. Large channel construction and mutator-side backing release now run BLOCKED with rooted receive cleanup; queue scans follow occupancy without waiter-state locking. Large implicit backing release now uses a native-only worker, with BLOCKED pressure/shutdown service and tested charge lifetimes; same-monitor handoffs are now grouped with allocation-probed notification initialization. All-size channel construction now prepares control synchronization while BLOCKED, with first-use allocation probes and corrected concurrent-test scheduling. Group-size/overlap tuning, remaining native metadata paths, accounting/admission, off-pause metadata cleanup and numerical resource/performance budgets remain open. [Current GC evidence](GC_READINESS.md) covers cooperative atomic/completion registration and the prepared reaper catalogue with finite probe cycles.
- Validate cycles, sparse survivors, large live graphs, old-to-young mutation, promotion and parallel evacuation under pressure.
- State that parallel stop-the-world collection is the supported target; isolate experimental concurrent-major machinery unless separately verified.
- Add operational counters for heap/native memory, roots, threads, queues, allocation and pause distributions.
- Evaluate adaptive GC-worker selection using live work and coordination costs. Existing tree evidence records automatic workers slower than one worker; do not tune a global default to one benchmark.

Done when memory/resource/latency budgets hold under pressure and long soaks, and every limit failure has defined behavior.

## 7. General-purpose standard library — needed for everyday development

- Add binary and streaming I/O: File/reader/writer abstractions, buffered reads/writes, partial operations, seek, flush and close.
- Add portable paths, metadata, directory operations, temporary files and non-UTF-8 platform path handling or an explicit supported restriction.
- Add process spawning, pipes, environment, working directories, exit status, signals and subprocess cleanup.
- Add monotonic time, durations, wall-clock time, timers and secure randomness.
- Add TCP/UDP, DNS, timeouts and TLS through maintainable libraries; add HTTP client/server packages for the service workload.
- Expand collections according to workloads: sets, deque/queue, byte buffers, sorting/searching and lazy iteration or another efficient traversal abstraction.
- Specify String byte/character boundaries and Unicode behavior; avoid invalid UTF-8 substring construction.
- Supply checked parsing/formatting for supported numeric widths, useful format specifications and debugging representations.
- Add real JSON parsing/serialization and schema/error support; CSV and JSON string escaping alone do not provide a serialization ecosystem.
- Provide structured error types/context, error chaining, logging and application metrics.

Done when useful applications can avoid RawPtr and private host intrinsics, with consistent Result-based errors and resource bounds. Databases, regex, compression, crypto and other domains may be packages rather than built-ins.

## 8. Language abstraction and completeness — select deliberately

- Associated types are parsed but explicitly rejected by resolution: implement them or state their absence in the stable language.
- Define generic bounds, supertraits, impl overlap/coherence, inference ambiguity and recursive specialization limits completely.
- Consider dynamic dispatch/type erasure for heterogeneous collections, plugin-like interfaces and dependency injection. `dyn Trait` is absent; it is not intrinsically a safety blocker.
- Decide on an extensible iteration protocol; current combinators are eager and collection `for` relies on len/index.
- Decide on user-defined macros/derive or another boilerplate mechanism. Built-in macros exist; a macro ecosystem is optional.
- Decide whether nested items and more constant evaluation are useful; current lowering rejects nested items.
- Complete reflection for inline reference-enum payloads whose references occupy split slots; update metadata and renderers together.
- Establish module/re-export/import ergonomics and access-control behavior for libraries.

Done when chosen abstractions compose across separately authored libraries and unsupported forms reliably diagnose. No Rust references or borrow checker should be added to satisfy this roadmap.

## 9. Packages, workspaces and application builds — practical development blocker

- Add managed dependencies: local/path sources first, versioned distribution next; define package identity and resolution.
- Add lockfiles, offline caches, deterministic transitive graphs, cycles/conflict diagnostics and integrity verification.
- Define workspaces, multiple binaries, library packages, dev/test dependencies and target-specific dependencies.
- Define native dependency/build configuration, cross-compilation, runtime archive ABI versioning and stale/incompatible artifact rejection.
- Support library artifacts or reproducible source dependency compilation with useful caching.
- Decide feature/configuration semantics and constrain executable build hooks if introduced.
- Add publish/install workflows and a package distribution/support policy; a public registry can follow a local-package MVP.

Done when a second project can consume a versioned first project reproducibly on a clean machine without copying sources or modifying the compiler prelude.

## 10. Everyday developer tooling — practical development blocker

- Add `gcr test`: discover language tests, assertions, filtering, expected failure and bounded subprocess execution; Rust integration tests are not a user-facing test framework.
- Add formatter and check mode; preserve comments and guarantee stable formatting.
- Add LSP diagnostics, completion, hover, go-to-definition, references, rename and formatting across modules/dependencies.
- Improve diagnostics with source identity, secondary spans, generic-instantiation context, several recoverable errors and machine-readable output.
- Add API documentation generation with examples/doctests and searchable library documentation.
- Add useful lints with stable categories and warning controls.
- Harden the existing debugger: values/closures/enums, optimized frames, threads, native transitions and documented platform support.
- Finish the heap/reflection tooling; support long-running profiling, not only program-end snapshots.

Done when an ordinary developer can navigate, test, format and debug a multi-package program without editing compiler internals.

## 11. Compiler robustness and scale — release blocker

- Fuzz the full lex/parse/resolve/type/layout/lower/codegen pipeline, including valid generated programs and malformed input.
- Add differential/property tests for semantics and GC object graphs; verify optimized IR and JIT/AOT equivalence.
- Diagnose unsupported syntax instead of panicking or producing invalid LLVM.
- Bound parser depth, type expansion, monomorphization, diagnostics and compiler memory/time for pathological inputs.
- Establish compile-time and binary-size budgets on substantial projects; profile cold and incremental builds.
- Design dependency-aware caching/incremental compilation if measured edit/build time warrants it.
- Stabilize internal ownership/invariants and test harness failure artifacts so future compiler changes remain reviewable.

Done when adversarial inputs receive bounded diagnostics and large representative applications compile within published budgets.

## 12. FFI and embedding — blocker for declared interoperability support

- Implement C ABI classification for each advertised target. Current aggregate support is AAPCS64-based and returns above 16 bytes are rejected.
- Specify foreign pointer retention, buffer length/mutability, callback registration/lifetime, foreign-thread callbacks and reentry.
- Specify failure/unwinding and thread affinity at the native boundary.
- Provide safe wrappers for native resource ownership and retained callbacks. Large argument copies now have rooted polling, width/boundary tests and original-owner/callback regressions; scoped FFI buffers now have native accounting and early-exit cleanup. Retained-pointer contracts, hard admission and broader native budgets remain open.
- Document an embedding lifecycle, multiple execution contexts, heap/type registration, shutdown and runtime compatibility.
- Test shared/static libraries, symbol visibility and loader/linker behavior on supported platforms.

Done when the declared scalar/aggregate/buffer/callback matrix passes per platform and application wrappers can enforce the contract.

## 13. Automation, portability and delivery — release blocker

- Add CI: clean workspace checks, user-language examples, optimized AOT/JIT, bounded GC/concurrency stress, sanitizers and artifact capture.
- Schedule longer seeded soaks; resolve the full binary-tree GC-stress gate or document separate bounded/long gates without quietly dropping coverage.
- Explain every existing ignored test and tool-dependent skip; do not count unavailable checks as passes.
- Pin/document compiler build inputs: Rust, LLVM, Inkwell revision, linker and runtime.
- Declare macOS/Linux/Windows and architecture support explicitly; current evidence mainly establishes macOS ARM64 cases.
- Ship compiler/runtime/stdlib bundles, install/update/uninstall paths and clean-machine smoke tests.
- Verify deployed native programs run without a compiler/LLVM installation and document native library requirements.
- Add reproducible release artifacts, checksums/signing policy, release notes and upgrade/downgrade tests.

Done when fresh users install and build supported programs, and shipped artifacts pass the same declared gates without development-checkout assumptions.

## 14. Compatibility, documentation and maintenance — release blocker

- Publish a versioned language/library/runtime contract, stability policy and migration rules.
- Write a tutorial grounded in current method-based APIs, library authoring guide, concurrency/resource guide and deployment guide.
- Reconcile stale STATUS/PRODUCTION/pad summaries with newer patterns, checked indexing, tests and closed archive/mutable-pattern defects.
- Establish bug triage, release branches, supported versions and security reporting/patch policy.
- Audit licenses and redistribution terms for compiler/runtime/native dependencies; record dependency updates and integrity policy.
- Keep benchmark claims attached to exact versions, configurations and raw evidence.

Done when users know what is supported, how upgrades affect programs and where to report defects.

## 15. Production acceptance through real applications — final proof

- Retain the five CLI apps with independent reference models, malformed input and native/JIT moving-GC checks.
- Add a substantial multi-package tool exercising dependency consumption, binary/streaming I/O and native resources.
- Extend search/logstats into long-running refresh/query or monitoring workloads after ownership/cancellation foundations exist.
- Add a network service with bounded queues, disconnects, deadlines, refresh, overload and graceful shutdown.
- Define numerical budgets before the gate: correctness, startup/build time, throughput, p95/p99 latency, maximum GC pause, RSS/native handles and shutdown time.
- Exercise low memory, disk-full/permissions, malformed requests, worker failure and native library failure; retain seeds and logs.
- Publish sustained-run evidence and limitations for each supported platform.

Done when someone can maintain and operate these applications through upgrades, load and failures within the agreed budgets.

## Recommended implementation sequence

1. Safety/embedding audit + deterministic cleanup + GC-aware locks.
2. Nested scopes, typed outcomes, fallible spawn and earlier metadata reclamation.
3. Cancellation, deadlines and shutdown.
4. Local packages/lockfiles, `gcr test`, formatter and useful editor support, alongside streaming/process APIs.
5. CI, clean installation, versioned runtime packaging and one declared platform release.
6. Async/networking and long-running acceptance apps; expand platform support after per-target gates.
7. Improve abstraction/performance based on real application constraints.

A first production CLI release can explicitly exclude async, dyn dispatch and user macros. It cannot leave its advertised safety, failure, installation or compatibility behavior ambiguous. The next best concrete milestone is a safely scoped worker application using a reusable package, language tests, streaming resources and bounded shutdown, shipped through clean-machine CI.

