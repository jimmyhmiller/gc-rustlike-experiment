# Threads and shared state

## Native embedding entries

A `RuntimeContext` is registered but parked between synchronous entries. Use
`context.enter(|thread| ...)` for generated code or native runtime calls.
`thread_ptr()` only borrows the ABI address; it does not activate the mutator.
The lower-level unsafe `with_current_thread` requires stable, live thread
storage owned by the calling OS thread for the entire scope.

Entering a different context suspends an active outer entry and publishes its
roots. Returning or unwinding parks the inner context before restoring the
outer entry. A surrounding foreign blocking scope stays blocked until its
matching leave. Do not access suspended outer roots or derived pointers from
the inner entry; reload pointers after returning because GC may have moved them.
Managed values retained between entries need independent roots.

Shutdown activates its context while suspending any outer entry, then parks
while waiting for native children. Idle contexts therefore do not prevent
another context's collection or shutdown from progressing. Root retirement and
poll-target detachment occur under collector exclusion before ABI storage is
freed. These rules cover synchronous embedding; asynchronous callbacks and
cross-thread context transfer still require separate lifetime contracts.

## Native registration handles

`Heap::register_thread()` returns `(ThreadStateHandle, usize)`, and
`MutatorThread::state()` borrows a `ThreadStateHandle`. The diagnostic identity
is stable and never reused. Prefer `MutatorThread` when its lifetime matches the
native mutator: its destructor retires the registration. Lower-level owners must
follow `Heap::safe_deregister_thread`'s safety contract.

`ThreadStateHandle` replaces `Arc<ThreadState>` in the Rust embedding API.
Clone the handle with `.clone()`. Replace `Arc::as_ptr`, `Arc::ptr_eq`,
`Arc::strong_count` and `Arc::downgrade` on registration states with the
corresponding `ThreadStateHandle` associated functions. Other heap and runtime
`Arc` APIs are unchanged. `ThreadStateWeak::upgrade()` returns `None` after the
last strong owner is released. The strong count is a changing diagnostic
snapshot; it does not establish exclusive access or synchronization.

Clones allocate no storage and retain the same stable payload address. The
record and ownership control are charged to the originating native registry
ledger. Strong owners retain the record's buffers even after registration
retirement and heap destruction. After the last strong owner, the buffers are
destroyed; weak observers retain the record allocation and its charge until the
last weak observer is dropped. Retained requested bytes do not include allocator
overhead or opaque platform synchronization backing.

An observer does not keep a retired registration active, preserve generated
code, own the runtime ABI thread storage, or extend managed root lifetimes.
Never resume managed execution through a retired state. A pointer from
`ThreadStateHandle::as_ptr` owns no reference and requires a live strong owner
for any dereference; ordinary GC and thread-access rules still apply.

```rust
use gcrust_rt::gc::{Full, Heap, IdentityPtrPolicy, MutatorThread, ThreadStateHandle};
use std::sync::Arc;

fn main() {
    let heap = Arc::new(Heap::new::<Full>(4096, vec![]));
    let mutator = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
    let observer = mutator.state().clone();
    let weak = ThreadStateHandle::downgrade(&observer);
    assert!(ThreadStateHandle::ptr_eq(&observer, mutator.state()));

    // Dropping the mutator retires the registration before its owner disappears.
    drop(mutator);
    drop(heap);
    assert!(weak.upgrade().is_some());
    drop(observer);
    assert!(weak.upgrade().is_none());
}
```

## Managed threads

`src/prelude.gcr`, `src/lower.rs`, and `crates/gcrust-rt/src/runtime.rs` implement
Thread<T>, Atom<T>, AtomicI64, and bounded channels. Spawn runs a closure on an
OS thread registered with the shared GC heap. A GC-managed handle retains the
worker result for all live aliases, including repeated joins. Runtime blocking operations publish frames and park
the mutator so another thread can collect.

Lowering checks captured types at spawn sites and rejects mutable shared reference
captures that fail its Sync check. Tests cover rejection, accepted atom captures,
atomic counters, channels, and generic thread results. These checks do not
constitute a completed audit of all aliasing and native-code interactions.

Historical debug/release 50-iteration comprehensive stress results are recorded
in STATUS.md; current checkpoint scopes are listed there separately.
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
cooperative async cancellation. Current Sync checks are transitional.
`Thread<T>.join()` and the scalar `thread_join` intrinsic support repeated and
concurrent observation. The first observer or native reaper consumes the OS join handle under a
mutex; every observer acquires that mutex while GC-blocked. Managed results live
in a traced ThreadResult cell: reference results preserve identity, and inline
results use ordinary value loads.

Safe `Thread<T>` binds its native completion weakly to the Thread wrapper itself. GC
updates the weak owner after copying, before source reset, and may reclaim native
metadata once the owner is collected **and native join has completed**. The
native record does not keep its owner alive. The owner is the wrapper identity,
not its replaceable result field; replacing that field cannot invalidate a
live wrapper's native handle. A minor cannot decide old-owner
liveness; an old-space major cannot decide nursery-owner liveness. Cleanup uses a
nonblocking state-lock check and never joins a thread or runs a user finalizer
inside a collection pause. All aliases preserve the wrapper identity, even when its result field is replaced.

The low-level `thread_spawn` API retains its unbound raw handle records for the
root execution. Native OS join can happen earlier; the record and cached scalar
remain valid for repeated observers. The unsafe `thread_bind_owner(handle, managed_reference)` primitive
can narrow a raw handle's lifetime; subsequent raw uses must keep that managed
owner strongly rooted across every call and safepoint. Reading `Thread.handle`
through unsafe code does not extend its owner's lifetime. Native u64 results are
scalar bits, not traced/relocated GC references; managed results require traced
storage. All native execution-owned handles expire at the root execution boundary,
even if an embedder retains managed wrappers afterward.

On main return, JIT and AOT drain every admitted child, including descendants
created while draining, before diagnostics or code retirement. A weak admission
worklist replaces an index into a mutable registry, so GC pruning cannot skip a
descendant. Native records are hash-indexed; GC visits only managed-owner records,
not the execution-owned raw history. Retirement removes admission bookkeeping too.

An owning handoff token keeps the closure env globally rooted before child
registration. Failed admission clears that root and returns its reusable slot;
successful admission moves the env into scratch before clearing and disarming
the token, then advertises vacancy. Root cleanup on an unregistered child is
serialized with collector visits by AtomicRootSet. A cleanup failure is fatal.

Both main and child mutators now own their registration with their boxed ABI
Thread. The registration guard is armed before further initialization and drops
before poll storage, including after a child setup unwind. Scratch roots and
parked frame pointers retire before deregistration. Failed cleanup aborts while
storage is still alive; explicit `finish_threads` calls also cannot unwind to a
caller that could unload still-executing code. Deregistration failures abort
inside coordinator exclusion, before caller-restoration guards can unwind.

Runtime spawn installs a completion publisher and a unique exit guard before
native launch. The guard is captured by the closure, so setup unwind or dropping
an uncalled closure also notifies cleanup. Publication and exit can occur in
either order without waiting for each other. Body exit weakly indexes the native
record for reaping, even when its managed owner remains live or it is an unbound
raw handle. Active notified bodies stay outside polling, including after GC finds
their owners dead. Legacy native records without a notice use GC's orphan-index
fallback. These notifications do not root managed owners or hold a Heap reference.

An execution-owned worker probes weak entries in batches of at most 64. It retains
unfinished or contended records for retry, with adaptive 1–100 ms polling, and
drains ready work without another GC. `is_finished` permits the join attempt;
the worker still joins through actual OS/TLS exit before caching the scalar.
GC never performs that join or destroys native panic payloads. All aliases and
repeated/concurrent joins observe the same cached result. Metadata can retire
when its managed owner is dead and native join has completed.

The worker starts on first actual native spawn (or a legacy owned-record
admission). Shutdown drains child handles, then stops/joins the worker outside
registry/coordinator locks while the caller is BLOCKED. It checks again for
descendants admitted by worker TLS cleanup before returning and unloading code.
Different executions own independent workers. A blocking native TLS cleanup can
delay the reaper and subsequent records as well as shutdown; no finite cleanup
latency is promised for arbitrary native TLS code. The weak backlog scales with
pending body-exit notices and legacy orphans; 64 bounds a scan batch, not total
admission. Native metadata/stack accounting, resource budgets and throughput
measurements remain open.

Unbound raw thread/atomic/channel handles retain execution lifetime. Safe channel
controls follow their managed wrapper lifetime, including pins while blocked.
A blocked or nonterminating child/TLS cleanup can delay shutdown indefinitely:
cancellation, nested execution scopes and typed outcomes remain future work.
Native panic still maps to scalar zero. Remaining admission, embedding and outcome
policies still require audit.

`tests/thread_completion.rs` verifies aliased/concurrent joins, embedded result
references, retirement under moving GC, abandoned descendants, and inferred
closure early returns in JIT and optimized AOT with subprocess deadlines. The
contract ledger distinguishes existing behavior from required implementation work.

## Atomic cell ownership

AtomicI64 cells remain native, stable-address SC integers. Safe AtomicI64 is now
a GC reference wrapper, rather than an inline value record, and its native cell
weakly follows that wrapper identity. All safe aliases retain the same wrapper;
GC can release a cell only after its owner is proven dead in the collected
space. Minor GC preserves old owners, and old-space major GC preserves nursery
owners. There is no explicit free that can invalidate another safe alias.
Programs and layout/reflection users must rebuild for this representation change.

The unsafe atomic_i64_new API still returns an execution-owned raw handle.
atomic_i64_bind_owner can narrow that lifetime; subsequent raw uses must keep the
owner alive across every safepoint and finish before root teardown. Load, store,
CAS and fetch-add never allocate, park or collect; the compiler uses that shared
nonrelocating contract. Wrapper field access still follows normal GC barriers.
Construction now allocates a managed wrapper and registers a weak owner.

Only managed-owner keys are weakly traced. Sparse atomic/completion hash tables
shrink after bursts, and root shutdown releases empty table storage even if an
embedder retains the heap. Compaction may rehash survivors as amortized retirement
work. These are lifetime/storage guarantees, not measured throughput, pause or
RSS budgets. The owner matrix, live/dead churn, raw-history exclusion, concurrent
binding and JIT/AOT alias fixture cover the current behavior.

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

## Channel API and ownership — 2026-10-16

`Channel<T>::new(cap)` requires positive capacity. Each successful send commits
one initialized message box to a bounded native FIFO; scalar, reference and inline
value payloads are supported. Safe native queues bind to the managed Channel
wrapper identity. Their slots are conditional edges of that wrapper in every
major/minor collection, promotion preflight and heap inspection. Unreachable
self/cross-channel cycles can therefore release native controls and messages.
Unbound raw queues keep unconditional execution roots. The managed `buf` field is a zero-length
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
immediately. Native operations pin their managed owner in scoped runtime roots
before parking; blocking send/receive keep both native state and queued values
alive even when no other managed alias is rooted. All copy paths relocate weak
owners before resetting source storage. Minor collection conservatively retains
old owners, while combined major/minor preflight uses exact nursery reachability.
Unsafe binding must precede publication; raw handles must be valid at entry and
retain an owner across earlier safepoints. Cancellation,
async channel methods and cancellation-aware waiter removal are not implemented.

The former send/recv signatures (i64/T) have changed; fixed-message-count examples
use recv_value, while shutdown-aware code handles Option/Result explicitly.
`tests/channel_contract.rs` runs real blocked waiters, FIFO pipelines, repeated
close, close/send races, scalar/value/reference payloads, witness reassignment
and GC-root retention through JIT/AOT and stress. Native teardown tests verify
reclamation with a retained heap. The sanitizer script covers this fixture as
well as managed-memory and GC handoff regressions. The channel ownership fixture
adds abandoned cycles, mutable witness replacement, aliases and blocked waiters.

## Native queue-buffer pressure

Managed occupancy alone does not schedule collection soon enough for small
Channel wrappers backed by large native buffers. The heap-owned native ledger therefore
accounts for queue slot-buffer reservations, generated FFI buffers, and native
registry table/vector backing. Buffer and registry metrics have separate scopes.
A buffer charge follows its storage through
registry retirement and transient native Arc borrows, and ends after storage is
freed. Closing an empty queue releases its buffer; a closed nonempty queue keeps
storage until the last message is drained. Accounting holds an independent
counter, not a Heap reference.

Native queue/FFI allocation and atomic/spawn construction check whether combined
accounted demand plus the pending buffer request exceeds a growth threshold.
Registry backing is charged when allocated and serviced by the next native
constructor. The initial threshold is 1 MiB.
Pressure-service decisions serialize separately from collector exclusion. The
caller publishes roots and becomes BLOCKED before waiting for that service. It
drains already queued backing, rechecks pressure, collects if still needed,
releases collector exclusion while deferred backing drains, then adapts under
exclusion. The new threshold is the larger of 1 MiB and twice the sum of actual
remaining accounted bytes and the pending request. This accommodates live
or execution-owned buffers rather than collecting on every allocation. A zero
accounted footprint does not trigger an empty collection. Concurrent requests
recheck the threshold under collector exclusion.

During allocation checks, the runtime also lowers obsolete headroom to the
larger of 1 MiB and twice the sum of current accounted demand and the pending request.
This lets a long execution recover from a released spike without waiting for
shutdown. Large pending replacements retain their serviced headroom. These
checks only lower policy; demand already above its budget still requires
collection. Concurrent checks may conservatively remove headroom, but cannot
grant unserviced growth.

At an execution boundary, after native retirement drains, the runtime lowers
obsolete headroom based on current retained buffer and registry demand. It never
raises the trigger without collection. Collection counts and peak metrics stay
unchanged. A later entry therefore does not inherit headroom for buffers already
freed. A lower existing trigger is preserved, so retained demand that already
exceeds its budget still enters pressure service.

For generational heaps, pressure service marks the complete root closure and
performs major collection and nursery evacuation in one pause. Merely collecting
the old generation would let unreachable young objects retain old native owners.
Constructor witnesses and caller roots remain published and are reloaded after
movement. Native atomic load/store/CAS/fetch-add remain noncollecting.

`native_buffer_bytes`, `native_buffer_peak_bytes` and
`native_pressure_collections` are available in metrics JSON; Heap exposes the
same counters. `gcr bench` includes them alongside managed-heap metrics.
The byte counts cover queue slot-buffer reservations and current generated FFI
payload/ownership allocations, including buffers kept by transient native borrows. They exclude allocator overhead,
registry/control metadata, thread stacks and other native allocations. The peak
is an accounting high-water mark, not sampled RSS. The threshold is a collection
growth policy, not a hard allocation limit. Large individual requests and
concurrent allocation can exceed it. Broader native-resource budgets, configurable
release limits, admission outcomes and process-memory measurements remain open.

`native_registry_bytes` and `native_registry_peak_bytes` report requested table
and vector backing layouts, including permanent-root sources, heap-owned global
root words, handoff vacancy indices, and staged/obsolete replacement storage.
Root backing stays charged for the heap's lifetime, across execution shutdown. The metrics exclude record/control
allocations, allocator overhead and RSS. Complete
native metadata accounting and general within-execution shrink policy remain open.

### Cooperative queue backing operations

All construction captures scalar type metadata and reserves backing charge while
RUNNING, then builds zeroed native backing and synchronization while BLOCKED,
even for a one-slot queue. State/backing locks and both notification paths are
prepared before the complete native control enters the registry.
Empty close, final sender release and draining a closed queue detach backing
under queue locks, release those locks, then free large backing while BLOCKED.
The last received managed value stays in a scratch root during this cleanup and
is reloaded afterward. Smaller backing releases use the bounded leaf path.

The `channel_buffers` fixture exercises concurrent allocation, nested message
graphs, repeated close and draining through JIT/AOT with moving-GC stress. Runtime
tests force collection during construction and each retirement path, checking
reservation lifetime, unlocked queue state and refreshed message addresses.
Implicit large-backing destruction uses the native worker described below;
registry/control metadata and numerical pause/resource budgets remain open.

### Occupied queue roots

During a world pause, root scans visit only the `count` queued messages starting
at `head`, using at most two slices for ring wraparound. Empty reserved backing
adds no slot visits. Send/receive publish this range together with slot changes
before any safepoint or blocking transition, under the state mutex. The published
atomics are stable collector metadata while paused, not a consistent concurrent
snapshot. Scans take the backing read lock but never the state mutex, which a
BLOCKED waiter can own. Reserved capacity and native byte accounting are unchanged.

The `collector_channel_scan_` tests check ring shapes/FIFO order, real wrapping
and draining, moving-GC message survival in both heap modes, and scanning while
state is held. Restored fault controls cover missing publication, omitted wrapped
edges, scanning capacity and taking the waiter mutex.

### Implicit backing retirement

Large backing (more than 32 KiB) reserves a native retirement node during BLOCKED
construction. A final implicit drop transfers its node and byte charge to a
lazily started native-only worker without allocating or freeing payloads. The
worker detaches batches and deallocates outside mailbox, registry and collector
locks. Explicit close/drain stays synchronous and BLOCKED. Pending and in-flight
backing remains charged; an ordinary GC can return before this release finishes.
Counter queries remain noncollecting and do not wait for the worker.

Execution shutdown clears the registries, then drains and joins its worker while
BLOCKED without holding registry/collector locks. Reusing a context can start a
fresh worker. Native observers may retain backing after execution or Heap ends;
the native-only monitor/worker survives until their last producer and work end,
without retaining Heap or managed objects. Such storage retention does not make
expired managed values or post-shutdown raw handles valid.

Eight native-retirement tests cover batch accounting, worker shutdown/reuse,
retained observers after Heap destruction, collection progress with release
stalled, pressure lock/root publication and adaptation after deferred release.
Ten restored fault controls exercise the corresponding failure paths. Mailbox
handoff contention, control/map/worker metadata and stack budgets, hard admission
and application latency/RSS budgets remain open.

### Grouped registry retirement

Sweep/shutdown unwrap exclusively owned controls and group their preallocated
retirement nodes. A same-monitor group publishes through one O(1) mailbox
operation; different monitors flush separately. Item and batch links are consumed
iteratively, so neither chain is recursively destroyed. A retained native observer
keeps its control and backing, using the individual final-drop path later. Weak
observers alone do not prevent grouping. Logical byte accounting is unchanged.

The worker's notification primitive is prepared during BLOCKED construction:
pthread-backed std Condvar can allocate lazily on notification. A no-worker Rust
allocator probe covers group construction/publication. Other channel notification
and metadata paths remain in the broader native-allocation/latency audit.

Tests check exact sweep/shutdown handoff counts, observer fallback and unchanged
storage, mixed-monitor groups, multiple collection waves while release is stalled,
full drain and notification allocation. Grouping delays publication until the
retirement boundary; collection time and complete cleanup must be evaluated
separately. Group-size/overlap tuning and numerical resource budgets remain open.

### Cold synchronization and constructor safepoints

Channel construction may collect or block at every capacity. Callers publish roots
before calling and reload managed arguments afterward. Native preparation takes
only scalar type IDs, charge and native retirement state; it never dereferences
managed storage. Counter reservation stays charged before registry publication.

Reference tests access synchronization before registration can warm it, then
exercise uncontended public send/receive/sender/diagnostic paths with a shared
current-thread Rust allocator probe. Close frees backing but allocates nothing.
Both heap modes and s390x are checked; forced-GC tests cover small/large construction.
These tests do not bound registry/index, diagnostics, OS or contended-path costs.

Concurrent test schedulers must release held scans before waiting for collecting
or blocking operations, and coordinate RUNNING readiness for any next cycle that
requires mutator progress. The atom ABI test retains its alias/SATB assertions
inside the held phase and performs construction afterward; its deadline is unchanged.
