# Concurrency contract: proposed version 1

This is the target language contract, proposed on 2026-10-03. It is not a claim
that the current compiler implements it. The implementation ledger below is part
of this document. API spellings for scopes, tasks, locks and async are illustrative
until their syntax and types are implemented. The ledger records implemented
changes, including typed channel send/receive outcomes.

## Shared heap and memory model

Threads and tasks share one managed heap. Any ordinary managed object may be
shared, including mutable objects, closures and collections. Sharing does not
require a Rust-style Send/Sync proof. References preserve identity across threads;
GC relocation is invisible. A mutable local captured by a closure is copied into
the closure at creation, as today; mutation of a referenced object is shared.
A future shared lexical-cell feature must be explicit, rather than silently
changing capture semantics. Async suspension preserves that activation's locals.

The proposed first version is sequentially consistent (SC) for all managed
memory accesses. There must exist one total order of access events consistent
with each execution's program order. Each read sees the latest preceding write
to that location in that order, or its initialized value. No invented values,
torn scalar values, invalid object references or undefined behavior arise from
managed data races. This is deliberately stronger than Java's ordinary-field
model. It gives us a precise initial contract; weakening it later is a language
compatibility change, not a compiler optimization.

Scalar fields, reference fields and array-element slots are indivisible access
units. Reading or assigning a value aggregate is one indivisible snapshot of its
entire representation, including its tag and embedded references. Implementations
may use immutable boxes and atomic slot replacement or locks. Several separate
field operations are not a transaction. Consequently `counter.n = counter.n + 1`
can lose updates. Atomics or a lock are required for a compound invariant. A
collection's individual fields being safe does not make push/remove/iteration
linearizable: unsynchronized structural mutation must return a defined error or
operate on a valid snapshot, never produce an invalid index, pointer or variant.
Each collection API must choose which behavior before becoming shareable.

All objects are initialized before publication. Constructors may not expose a
partially initialized reference. Allocation, collection, safepoints, sleep and
yield do not add application-level synchronization edges. An allocation-free
loop must still permit GC coordination and cancellation checkpoints.

These choices adopt Java's shared-object programming model, not the complete
Java Memory Model. See the [Java memory-model specification](https://docs.oracle.com/javase/specs/jls/se25/html/jls-17.html).
Ordinary LLVM loads/stores cannot implement this contract for racing memory;
backend lowering must respect [LLVM's concurrency rules](https://llvm.org/docs/Atomics.html).

## Synchronization and publication

Happens-before is the transitive closure of program order and these edges:

- Parent actions before successful start happen before the child's first action.
- All child actions happen before a successful join/await observes completion.
- Unlock happens before a subsequent successful acquisition of that same lock.
- Successful channel send happens before receipt of that particular message.
- Atomic operations participate in the same SC order as managed accesses.
  Reading a published flag after its store therefore observes preceding writes.

SC does not guarantee that a thread runs, or make deadlocks impossible. Scheduling
order, CPU affinity and exact wakeup timing are unspecified. Sleep/yield cannot
be used as publication. Locks are reentrant, owned by an execution identity and
released on lexical exit, including failure and cancellation. Condition waits
release all recursion depth, atomically register the waiter, then reacquire the
same depth; callers test their predicate in a loop. Waits may wake spuriously.
No lock guard may survive async suspension: the compiler rejects that case.
Non-suspending calls inside a guard remain legal; blocking while holding locks
can still cause an application deadlock.

AtomicI64 load/store/fetch-add/CAS are SC. CAS succeeds exactly when the current
integer equals the expected integer; it has no spurious failure. Atom<T> uses
reference identity for reference values and bit-pattern equality for
supported scalar values (NaNs compare by their bits, and positive/negative zero
are distinct); aggregate CAS requires a separately specified equality
contract before support. swap's callback may execute repeatedly, so it must not
perform irreversible effects; only one successful replacement is committed.
ABA is possible without versioned state. reset is an atomic replacement.

Channels are bounded FIFO queues, ordered by successful-send linearization,
with multiple producers and consumers. Capacity must be positive; zero/negative
capacity is an error (rendezvous is a separate future API). Close is explicit,
idempotent, wakes all senders and receivers, rejects subsequent sends with the
unsent value, and allows queued messages to drain before recv reports Closed.
Concurrent send/close either commits a message or returns it, never silently
loses it. Async and blocking channel methods use the same queue and close state.
A cancelled operation that has not committed removes its waiter; a committed
send/receive reports success even if cancellation arrives concurrently. Close
must not depend on nondeterministic GC finalization of sender objects.

## Threads, tasks and async

Thread is a dedicated OS thread for blocking work, native interoperability and
explicit thread affinity. Task is a scheduled execution, potentially migrating
between worker threads. An async function call creates a cold, single-execution
Future: its body starts when awaited or explicitly spawned into a scope. Await
runs that future within the current task; spawning creates a child task. A Future
cannot be consumed twice. A completed Task handle, by contrast, can be awaited
or joined repeatedly and by multiple observers, returning the same outcome.
Reference results retain identity and value results are copied.

Await suspends the task, not its worker thread. Suspended frames, pending results
and callback environments are GC roots. Resumption has the same publication
semantics as completion observation. Blocking native work runs on dedicated
threads or a bounded blocking executor; ordinary async workers must not enter
unbounded blocking native calls. Coroutine lowering, task scheduling and GC
registration use the same implementation in JIT and AOT.

Task-local state follows task identity; OS thread-local state follows the OS
thread and cannot be assumed stable across await. Native thread-affine handles
require an explicit pinned executor and cannot migrate. Shared arbitrary managed
captures are legal; affinity and resource ownership are separate constraints.
There is no user-visible promise that one task occupies one thread.

The root program is a scope. Child threads and tasks belong to a scope. Scope
exit waits for all children and their cleanup; a handle being garbage collected
does not detach its child. Detached background work requires an explicit
supervisor lifetime and error observer. Returning from main closes the root
scope before process exit. Runtime shutdown stops admission, requests cancellation
and drains children/cleanup; it does not kill threads or discard reachable roots.

Each execution completes exactly once with Success(value), Failure(error) or
Cancelled. Failure in a child requests cancellation of its siblings. Scope exit
waits for all of them and reports failures in child-creation order, preserving
all failures rather than choosing the fastest. An explicitly handled failure
may be consumed by its owner; merely dropping a handle never counts as handling.
Spawn/resource exhaustion returns an error before registering a child. Fatal
runtime corruption and unrecoverable allocation failure may still abort the
process; ordinary language panic becomes Failure, not process termination.

Cancellation is a cooperative request, not asynchronous exception injection.
Checks occur at await, scheduler/GC checkpoints and cancellation-aware blocking
operations. A request does not undo committed effects. An execution reports
Cancelled only when it observes the request before completing; completion may
win a race with an unobserved request. Cleanup runs exactly once on the owning
execution. Cancellation is masked during cleanup; cleanup can await, and failures
are retained with the original outcome. Non-cooperating native work can delay
shutdown indefinitely. Timeouts request cancellation and wait for cleanup; an
explicit deadline result does not imply work has stopped. Forced OS termination
is outside structured cancellation.

## Native boundary and GC obligations

Unsafe raw-pointer/FFI access is outside managed race guarantees and must obey its
native ABI and synchronization contract. A native call cannot retain movable
managed pointers after return or while parked unless registered roots and an
approved pin/copy mechanism preserve them. Publishing a reference, replacing an
aggregate and updating the generational barrier must be coordinated with the
collector. Atomic pointer replacement alone is insufficient. Blocked threads
publish frames before parking and reload relocated references on resumption.
Task frames must remain rooted even when no worker is executing them. Retiring
a thread/task unregisters roots only after its result and failure are published.

## Implementation ledger

| Area | Observed today | Required before version 1 conformance |
|---|---|---|
| Sharing | Direct spawn captures checked against Sync; aliases/higher-order callbacks bypass the check | Race-safe backend and collections, then remove ordinary managed Sync restrictions consistently |
| Memory | Mutable scalar/reference fields and array slots use SC atomics; inline aggregate fields use relocation-aware striped snapshots | Complete collection/initialization and native root API audit; extend conformance coverage |
| Thread start/join | Shared GC heap, rooted environment/result, root-owned completion state; repeated/concurrent join | Fallible spawn and typed outcomes |
| Join lifetime | Stable root-owned completion records; OS handle consumed once; JIT/AOT drain abandoned descendants at root exit | Nested scopes and earlier metadata reclamation; cancellable drain |
| Atomics | AtomicI64 and scalar/reference Atom operations use SC; bool uses byte storage, float CAS compares bits; native cells retire after root children drain | Audit mixed ordinary managed accesses; earlier native-cell reclamation for long-lived roots; aggregate CAS equality design |
| Channels | Positive-capacity native FIFO with traced message boxes; idempotent close wakes all; send returns Result<(),T>, recv returns Option<T>; drained closed storage retires | Cancellation-aware waiters, async methods and earlier handle-metadata reclamation |
| Failure | Several failures abort; native join maps native panic to zero | Typed completion outcomes and propagation after cleanup |
| Tasks/async | No async syntax, continuation frames or scheduler | Cold futures, task scopes, resumable GC roots and blocking executor |
| Cancellation/shutdown | No structured cancellation or child registry | Scope ownership, checkpoints, waiter removal, deterministic cleanup and root drain |
| JIT/AOT | Existing thread/atomic/channel tests and parallel app run through both | Same contract suite for every new feature, optimized builds and GC stress |

The capture-check bypass is still a current safety exposure because collection
and low-level runtime safety audits are incomplete. It is not a reason to make shared mutable objects illegal in the
target language. Adding closure provenance restrictions is not the next step.

## Acceptance and implementation order

1. Thread-handle ownership is repaired for root-execution lifetimes (2026-10-04).
   Preserve repeat/concurrent join and abandoned-descendant regressions; extend
   reclamation to nested scopes when those are introduced.
2. Implement managed SC access, aggregate snapshots and collector/barrier
   coordination. Audit collections and all optimizations, then remove Sync gates.
3. Add reentrant locks, typed outcomes, explicit channel close and scope ownership.
4. Add cooperative cancellation and shutdown, including blocked/native behavior.
5. Lower async into rooted continuations; implement tasks and the blocking executor.
6. Run the same conformance suite in JIT and optimized AOT, under GC stress and
   sustained contention. Add supported-platform CI and native race instrumentation.

`tests/concurrency_contract.rs` exercises existing synchronized publication through
start/join, channel send/receive and atomic signaling, including moving-GC pressure.
Its explicit Sync declaration is a bridge for a synchronized fixture, not proof
that arbitrary shared races already work. These are observational regressions.

Pending conformance cases (must become executable with their implementation):

| Case | Required outcome |
|---|---|
| Store buffering: each thread writes its flag then reads the other | Both reads zero forbidden under SC |
| Concurrent scalar/reference stores and loads | Only complete initialized stored values; no invalid reference |
| Concurrent enum/value replacement during collection | A complete valid variant snapshot; embedded references survive |
| Concurrent ordinary increments | Lost updates allowed; atomic/locked increments exact |
| Start, completion, channel and flag publication | Published payload always visible after corresponding observation |
| Repeated/concurrent join and abandoned handle | Same outcome, no double free, child still owned by scope |
| Close full channel with waiting senders | All waiters wake; uncommitted messages returned; queue drains |
| Cancel await/send/recv while GC runs | Exactly one completion; no stranded waiter or unrooted continuation |
| Child failure and cleanup failure | Siblings cancelled, all cleanup joined, every failure retained |
| Task migrates between workers | Task locals stable, thread locals allowed to differ |
| Root exit with sleeping/blocked child | Explicit cancellation/drain policy, no silent detached execution |

Remaining design work is concrete API syntax, error types, collection-specific
snapshot/error choices, pinned-native resource APIs and scheduler tuning. Worker
counts, queue limits and fairness instrumentation are runtime configuration; none
may weaken memory safety, completion ownership or cleanup guarantees.
