# Threads and shared state

`src/prelude.gcr`, `src/lower.rs`, and `crates/gcrust-rt/src/runtime.rs` implement
Thread<T>, Atom<T>, AtomicI64, and bounded channels. Spawn runs a closure on an
OS thread registered with the shared GC heap. A GC-managed handle retains the
worker result until join. Runtime blocking operations publish frames and park
the mutator so another thread can collect.

Lowering checks captured types at spawn sites and rejects mutable shared reference
captures that fail its Sync check. Tests cover rejection, accepted atom captures,
atomic counters, channels, and generic thread results. These checks do not
constitute a completed audit of all aliasing and native-code interactions.

The full 50-iteration comprehensive stress test passes in debug and release.
Shared-memory (15), threads (13), and randomized thread-stress (3) integration
tests pass. Fixed-size codegen temporaries now allocate at function entry, scalar
allocating operands are normalized before heap accesses, and channel mutex
acquisition parks the mutator. These fixes address the observed overflow, stale
store, and GC/channel lock deadlock. See STATUS.md for commands and scope.

Production work must define worker failure, cancellation/shutdown, channel close,
abandoned handles, and supported shared types, then verify GC coordination under
pressure. Passing these tests does not establish deadlock freedom for all schedules.
