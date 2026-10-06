# Production readiness proposal

This document proposes work. None of the gates below has been signed off.
Current evidence and failures live in [STATUS.md](STATUS.md).

## Choose a release target

Start with one application and one declared platform. The locally assessed host
is macOS ARM64. That evidence alone does not establish a supported platform.
Choose the application's allocation profile, concurrency needs, native libraries,
maximum memory, acceptable pauses, startup time, and failure behavior before
promising production readiness. Record the selected workload and numeric budgets
here when agreed; these are currently open decisions.

A native application needs resource ownership and packaging. A long-running
service also needs cancellation/shutdown, bounded queues, memory pressure controls,
and sustained concurrency evidence. A portable language release adds target ABIs,
installation, compatibility, and dependency distribution. These scopes differ.

## Gate 1: restore trustworthy correctness checks

The observed stack overflow, stale scalar-operand store, channel lock deadlock,
and native-call root/buffer defects have causal fixes and regression coverage.
The full workspace and unreduced 50-iteration debug/release concurrency checks
pass locally. Preserve these checks and extend them into automated, bounded gates.

Acceptance:

- The current failing debug and release reproductions pass after a causal fix.
- Add a small regression that fails on the old implementation and exercises the
  repaired behavior. Run the unreduced existing workloads as well.
- `cargo test --workspace` passes from a clean checkout, with tool-dependent
  skips reported. Give stress tests bounded deadlines so CI reports hangs.
- The example runner is repaired, and project/JIT/AOT stress selection is
  consistent. Normal examples pass. Finish full binary-tree stress validation or
  define an explicit bounded per-change workload and retain a longer stress gate.
- Separate bounded per-change stress from a longer scheduled soak. Persist seeds,
  generated source, compiler/runtime versions, and logs on failure. Do not silently
  replace the default suite with a reduced one.

## Gate 2: define and verify the safety contract

Write a language/runtime contract before claiming memory safety. Classify checked
operations, unchecked operations, foreign pointers, native callbacks, and
thread-sharing rules. Decide whether unchecked access requires an explicit unsafe
capability or becomes an internal primitive with checked public APIs.

Audit exception cases in arithmetic: divide by zero, signed minimum divided by
minus one, oversized/negative shifts, casts, and size/offset overflow. Specify
failure or wrapping semantics and verify the emitted LLVM behavior, including
optimized builds. Addition/subtraction/multiplication wrapping does not settle
these other cases.

Audit GC roots and reloads across allocating operands, closure captures, flattened
values/enums, mutable old-to-young stores, callback reentry, spawn handoff, thread
retirement, and diagnostics/snapshots. Check every path that can delay a stop-the-
world pause, including allocation-free recursion and native blocking calls.
Audit aliasing/capture rules for supported shared types and channels.

The target sharing and execution contract is [concurrency.md](concurrency.md).
Current capture checks can be bypassed through aliases and higher-order calls,
and the managed-access audit is still incomplete. Mutable fields/array slots now
use SC accesses and aggregate snapshots; finish GC/barrier and memory-safety
boundary audits before removing Sync restrictions. Ordinary Vec/Map require
external synchronization for shared mutation; concurrent versions are separate APIs.
Root-owned completion records now support repeated/concurrent joins without
freeing aliased native handles. Root exit drains descendants and releases records;
metadata grows with total spawns until then. Nested scopes, earlier reclamation,
structured outcomes, cancellation and async remain required.

Acceptance:

- Each supported operation has defined behavior on boundary inputs, with tests
  that distinguish the chosen contract from undefined LLVM behavior.
- Differential/property tests exercise object graphs, cycles, sharing, variants,
  and value-embedded references under frequent minor and major collections.
- Suitable sanitizer builds pass, with the instrumentation scope documented.
  Rust-runtime ASan coverage alone does not instrument all JIT-generated code.
- Compiler fuzzing includes resolution, lowering, layouts, and codegen, beyond
  parser-only fuzzing. Malformed/unsupported input gets a diagnostic.
- Publish the supported GC mode. Verify concurrent-major machinery or remove it
  from the supported path and document its experimental status.

## Gate 3: survive resource pressure and shutdown

Specify initial/max heap, allocation limits, large-object behavior, and the OOM
policy. Add checked size arithmetic and diagnostics; evaluate heap growth/shrink
against the target workload. Specify stack/recursion limits and how native buffer
copies consume stack space. Define how users close file handles, sockets, native
objects, channels, and worker threads. Choose explicit resource cleanup semantics;
finalizers are optional unless the chosen contract requires them.

Acceptance:

- Pressure tests force repeated major collections and approach/exceed limits
  with bounded memory use and the specified failure behavior.
- Worker failure, blocked send/receive, shutdown, and abandoned handles have
  defined outcomes and do not leave the runtime waiting forever.
- The target application passes a soak long enough to expose its resource cycles.
  Track RSS, live/retained heap, allocation rate, GC pauses, and thread/handle
  counts. Set and record numeric thresholds before declaring this gate passed.

## Gate 4: reproducible delivery and platform support

Pin/document the Rust, LLVM, Inkwell, native linker, and runtime build inputs.
Test installation and native executable distribution on a clean machine. Typed,
validated manifest parsing is implemented; keep malformed-config regressions.
Define supported targets before enabling an ABI path on them; implement the C ABI
for each supported target, including large aggregate returns where required.

Acceptance:

- CI runs compiler/runtime tests, native builds, debugger checks where supported,
  bounded stress, and sanitizer gates. Longer soaks run on a declared schedule.
- A platform matrix records pass/fail/unsupported for scalar and aggregate FFI,
  buffers, callbacks, threads, debugger, and linking.
- Native artifacts run without LLVM on the deployment machine, with external
  library requirements documented. Release builds use the intended runtime.
- A release has a versioned language/runtime contract, install instructions,
  reproducible build inputs, migration notes, and a support policy.
- Decide how application dependencies are distributed. Build a resolver/lockfile
  if the chosen release requires it; local modules alone are not a package system.

## Gate 5: measure the chosen application

Measure correctness, throughput, startup, compilation time, artifact size, peak
RSS, and GC pause distribution on the agreed workload. Benchmark against an
appropriate alternative with equivalent observable work and build settings.
Keep machine, inputs, environment, compiler revisions, raw timings, and output
checks with the result. Profile before choosing inline allocation/barriers, TLABs,
parallel collection, root liveness, or collector changes.

Acceptance: satisfy the budgets agreed for the release target and publish enough
raw evidence to reproduce the result. No fixed Rust-performance ratio constitutes
production readiness across workloads.

## Next work

Follow the ordered [implementation plan](PLAN.md): preserve the repaired execution
ownership and closure inference, implement managed race safety, then add structured
outcomes, cancellation and async. The search application exercises the evolving
standard library. Numeric release budgets and declared platform support remain
open; the production gates above are not signed off.
