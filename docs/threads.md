# Threads and shared state

`src/prelude.gcr`, `src/lower.rs`, and `crates/gcrust-rt/src/runtime.rs` implement
Thread<T>, Atom<T>, AtomicI64, and bounded channels. Spawn runs a closure on an
OS thread registered with the shared GC heap. A GC-managed handle retains the
worker result for all live aliases, including repeated joins. Runtime blocking operations publish frames and park
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

The target semantics are specified in [concurrency.md](concurrency.md): shared
mutable managed objects, sequentially consistent access, structured execution and
cooperative async cancellation. Current Sync checks are transitional. `Thread<T>.join()` and the scalar `thread_join` intrinsic now support repeated
and concurrent observation. Native completion records belong to the root execution;
managed aliases borrow stable addresses, and the first observer consumes the OS
join handle under a mutex. Every observer acquires that mutex while GC-blocked,
then reads the same completed result. The managed result cell remains traced while
any Thread alias retains it. Reference results preserve identity; value results
are copied by ordinary value loads.

On main return, JIT and AOT drain all children, including descendants created
while draining, before heap diagnostics or code retirement. Abandoned handles do
not detach children. Native completion metadata is reclaimed at root shutdown,
even if an embedder retains the heap. Metadata grows with total spawns during a
root execution; nested scopes and earlier reclamation remain future work. A blocked
or nonterminating child can delay root shutdown indefinitely: cancellation and
typed outcomes are not implemented. Native panic still maps to scalar zero.

`tests/thread_completion.rs` verifies aliased/concurrent joins, embedded result
references, retirement under moving GC, abandoned descendants, and inferred
closure early returns in JIT and optimized AOT with subprocess deadlines. The
contract ledger distinguishes existing behavior from required implementation work.

## Atomic cell ownership

AtomicI64 cells are native, stable-address SC integers. The root execution owns
all cells allocated by itself or its descendants. Aliases borrow those addresses;
shutdown first joins all children and descendants, then releases cells, including
when an embedder retains the GC heap. No explicit free may invalidate an alias.
Cells accumulate until root shutdown; earlier reclamation remains a requirement
for applications that continually create cells over a long lifetime.

The log analyzer in `apps/gcr-logstats` uses fetch-add for exclusive work claiming
and immutable Atom snapshots for aggregate results. This is a supported
synchronized workload, not evidence for arbitrary shared field mutation.

## Atom element contract

Atom operations support integer widths, bool, char, floats, RawPtr and managed
references. CAS compares scalar bit patterns and reference identity. Thus float
positive/negative zero are distinct, and an unchanged NaN bit pattern can match
its expected value. Bool uses a canonical 0/1 byte for atomic memory operations.
Inline value aggregates and unit receive frontend diagnostics: aggregate CAS
has no specified equality contract. An Atom pointing to an immutable reference
summary, as in gcr-logstats, atomically replaces the entire referenced snapshot.
Mutating that referent separately still depends on ordinary managed-access safety.

The representation lowering follows [LLVM atomic load and cmpxchg operand
requirements](https://llvm.org/docs/LangRef.html#cmpxchg-instruction).

## Channel API and ownership — 2026-10-05

`Channel<T>::new(cap)` requires positive capacity. Each successful send commits
one initialized message box to a bounded native FIFO; scalar, reference and inline
value payloads are supported. Native queue slots are roots in every major/minor
collection and heap-root enumeration. The managed `buf` field is now a zero-length
array witness for the generic message type; changing its contents/length cannot
replace queue storage. Mixing different generic channel controls fails a runtime
type check before any message access.

- `send(value) -> Result<(),T>` blocks while full. Ok means committed; Err returns
  the original unsent value when closed, preserving reference identity.
- `recv() -> Option<T>` blocks while empty. Some is the next committed message;
  None means closed and drained. `recv_value()` unwraps a protocol-required
  message and reports a panic on closed/drained queues.
- `close()` is idempotent, wakes every sender/receiver and allows committed values
  to drain. It does not depend on GC finalization or sender-count subtraction.
- `waiting_senders()` / `waiting_receivers()` expose synchronized waiter counts
  for diagnostics and deterministic tests.

FIFO order is successful-send linearization under the queue mutex. Concurrent
close/send either commits once or returns the value; closed/drained slots retire
immediately. Stable native handle metadata remains execution-owned until root
children drain, so aliases cannot dangle. Metadata still grows with channel
creation until root exit; earlier handle reclamation remains work. Cancellation,
async channel methods and cancellation-aware waiter removal are not implemented.

The former send/recv signatures (i64/T) have changed; fixed-message-count examples
use recv_value, while shutdown-aware code handles Option/Result explicitly.
`tests/channel_contract.rs` runs real blocked waiters, FIFO pipelines, repeated
close, close/send races, scalar/value/reference payloads, witness reassignment
and GC-root retention through JIT/AOT and stress. Native teardown tests verify
reclamation with a retained heap. The sanitizer script covers this fixture as
well as managed-memory and GC handoff regressions.
