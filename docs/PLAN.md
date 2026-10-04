# Implementation plan

Updated 2026-10-03. This plan records remaining work, not completed features.
The target semantics are in [concurrency.md](concurrency.md); verified behavior
and open defects are in [STATUS.md](STATUS.md). Release gates are in
[PRODUCTION.md](PRODUCTION.md).

## 1. Repair execution ownership and compiler inference

Replace the raw consuming native thread handle with owned completion state.
Specify which current Thread APIs transition to repeatable join and typed outcomes.
Root the result until every observer has finished; reclaim native handles exactly
once. Add regressions for repeated/concurrent joins, dropped aliases, completion
before join, and collection during retirement in JIT and optimized AOT.

Reproduce and repair integer early-return inference inside a spawn closure.
The existing publication fixture uses an if/else expression; it does not fix the
inference defect. Add a regression containing the original early return.

Exit criteria: no raw-handle reuse/double free; stable completion observations;
original inference reproduction accepted with the correct result.

## 2. Make managed sharing safe

Inventory every heap access, aggregate copy, reference publication and write
barrier. Implement the proposed SC managed memory model and coherent aggregate
snapshots, coordinating publication and relocation with the collector. Audit
collection operations and define their behavior during structural mutation.
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

Implement reentrant locks and condition waits, with lexical cleanup. Replace
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
