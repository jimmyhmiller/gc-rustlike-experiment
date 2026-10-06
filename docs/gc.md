# Garbage collector and runtime

The runtime lives in `crates/gcrust-rt`. `gc/heap.rs`, `gc/semi_space.rs`,
`gc/scan.rs`, `gc/roots.rs`, `gc/thread.rs`, and `runtime.rs` implement the
collector, root protocol, mutator coordination, and generated-code entry points.

Normal execution uses a nursery plus copying tenured space. Minor collections
promote survivors. Major collection copies tenured objects. A card-table write
barrier records old-to-young stores, including references inside flattened value
fields. Objects carry a type ID; TypeInfo describes the traced locations.

Generated functions pass a runtime Thread pointer. Reference locals use shadow-
stack root slots; value locals containing references use indirect roots. ANF
normalization evaluates eager operands into locals, including scalar expressions
that can allocate, and snapshots local operands before later side effects.
The enclosing operation reloads the resulting rooted heap addresses. The collector rewrites
roots when it moves objects. Registered mutators cooperate through safepoints;
blocking/native transitions publish frames for collection.

`configured_heap_sizes()` in `runtime.rs` supplies both JIT and AOT settings:

| Variable | Default | Interpretation |
| --- | --- | --- |
| GCR_NURSERY_MB | 16 | Nursery setting in MiB |
| GCR_TENURED_MB | 256 | Tenured setting in MiB |

Positive integer values override defaults; invalid/zero/overflowing inputs fall back to the
default. These are startup settings, not adaptive heap sizing or a total RSS cap.
Copying space requires reserve memory. Allocation failure currently aborts.

`gcr run --gc-stress` uses collect-on-allocation semi-space execution in both
bare-file JIT and project-native paths. Standalone native executables select the
same mode with `GCR_GC_STRESS=1`. The precise-layout
verifier is available with `GCR_GC_VERIFY=1`; it checks traced pointers against
object headers. A detector and passing unit tests do not establish soundness.

The supported collector uses parallel stop-the-world evacuation for both major
and minor collections. Application mutators run concurrently between pauses.
`GCR_GC_WORKERS=N` selects the total collector worker count, including the
coordinator. Without an override, the count is available CPU parallelism capped
at eight, and cycles with less than 256 KiB in the collected space run on the
coordinator alone. Each heap lazily starts persistent helpers and reuses them
across collections; idle helpers sleep and heap teardown shuts them down and joins
them. A smaller configured worker count leaves excess helpers idle.

Mutators now allocate small objects through thread-local buffers. Shared atomic
cursor updates reserve buffers, allocate large objects and reserve collector copy
batches. Copying buffers with independent collector-local cursors remain work. Concurrent-copy machinery exists, but is not a supported, verified
execution mode. Arbitrary racing managed accesses remain unimplemented; see
[concurrency.md](concurrency.md).

## Parallel evacuation protocol — 2026-10-04

Root sources are enumerated by the coordinator while the world is stopped;
they need not implement Sync. Root slot addresses are deduplicated and remain
valid until every participating persistent worker acknowledges completion. Minor collection builds distinct object tasks
from dirty cards, including objects spanning several cards. Major collection also
treats nursery reference slots as roots into the collected tenured generation.

Workers claim an old object's type-info word with an atomic compare-exchange.
The reserved high-bit-only value denotes a copy in progress. Each worker collects
its claims locally, reserves their exact destination extents in one cursor CAS,
and initializes each copy without reading the claimed old header word. It restores
the original type-info word in each copy and release-publishes the destination.
Other workers acquire-load forwarding words. References to BUSY copies become
retry tasks rather than blocking: waiting while holding other unpublished claims
could create a cycle between workers. Only owners queue copied objects for scan.
There are no losing duplicate copies or unused copying-buffer tails. Linear heap
walks wait until all reservations are initialized; copying does not zero-fill first.
The same protocol promotes nursery survivors into tenured space during minor GC.

A shared queue transfers batches of up to 32 tasks under one mutex. Workers scan
batches and reuse task, discovery, claim, layout and destination-address buffers
within each cycle. They publish pending copies before publishing discovered work. Termination requires both an
empty queue and zero active batches; a worker publishes discoveries before
retiring its active batch. Idle workers sleep on a condition variable. Root/field
rewrites use atomic accesses, and destination cards are rebuilt during major
object scanning. Every participant completes before swapping spaces or resetting
the nursery. The worker pool dispatches borrowed jobs synchronously and waits for
all helpers even when a coordinator or helper panics. Helpers do not retain a heap
reference between jobs, and their handles are joined during heap teardown.
Collector panics cancel queue waiters, release uncompleted header claims and
propagate; a failed partially relocated cycle must not resume managed execution.

This is a correctness-oriented parallel backend with measured copy throughput,
not a completed mutator-scaling effort. Root enumeration, dirty-card indexing,
queue contention, remaining batch reservation contention and cold pool startup
are explicit remaining costs. Worker-local allocation requires accurate extents and unused-tail
handling before replacing the shared cursor.

## Thread-local mutator allocation — 2026-10-04

Runtime JIT/AOT allocation calls and the Rust MutatorThread API use owning-thread
local buffers for objects up to 8 KiB. Buffer targets start at 2 KiB and double
on refills up to 32 KiB, accommodating the current object when larger than the
initial target. Refills reserve space from the active
nursery (generational) or from-space (semi-space), shrinking to remaining capacity
at the end of a region. Objects over 8 KiB use exact shared reservations. Collect-
on-allocation stress mode bypasses buffering while preserving its per-allocation
collection behavior. Public Heap allocation remains an exact shared reservation.

Each reserved buffer has an immutable extent and an atomic initialized-prefix
boundary. The owner zeroes an allocation and advances that boundary without a
shared cursor RMW. It initializes the header/count before reaching a safepoint,
as required by the existing allocation contract. Heap walks run after the world
stops, derive initialized ranges from descriptors and skip all unused tails.
Descriptors remain in the allocator after their mutator exits, so abandoned tails
are still excluded. Snapshot walks, nursery-as-major-roots scanning, prewalk
validation and remembered-card object indexing use these boundaries.

Allocator reset increments a generation and clears current descriptors. Local
state checks allocator identity and generation before reuse, invalidating buffers
across nursery reset and space flips. A major collection leaves nursery buffers
valid, while updating pointers in their initialized objects. Descriptors retained
by local state own metadata, not heap storage. Physical `used`/`remaining` and GC
occupancy include reserved tails; initialized object walks and allocation-site
byte counts exclude unused tails. Buffer refill registration is synchronized; the
hot local bump cursor belongs exclusively to its mutator.

The Rust mutator handle is neither Send nor Sync: shadow roots, local buffers and
profiling belong to the registering OS thread. Register a separate handle in each
worker. A compile-fail check protects this ownership requirement.

## Coordination repairs — 2026-10-04

The JIT safepoint flag is an AtomicU8 at the same ABI offset. Generated polls use
atomic acquire loads (also volatile to preserve checkpoints); the coordinator
uses release stores. Minor collection requests all mutator polls before waiting,
including allocation-free workers. Promotion capacity is checked after the world
stops. A required major and the following minor share that same pause/census.

Card-table bytes are atomic. Dirtying uses relaxed stores; the safepoint handshake
orders mutator stores and card marks before collection reads. Major evacuation
rebuilds destination cards while scanning copied objects, retaining nursery edges
for the subsequent minor. No extra heap pass or global mutator-access lock is
introduced by these repairs.

Tests cover concurrent card marking, atomic polls before/after LLVM optimization,
allocation-free JIT/AOT workers during minor and major-before-minor collection,
and preservation of fixed/interior/varlen nursery references across major/minor.
Exact commands, negative reproductions and totals are in [STATUS.md](STATUS.md).

## Scalable collector design constraints

The earlier proposal to put all managed accesses behind a heap-wide mutex was
withdrawn before implementation. Independent mutators need independent hot paths:
thread-local allocation buffers, atomic scalar/reference publication and card
marking, and coherent aggregate snapshots. TLAB initialized extents and unused
tails are now explicit metadata consumed by heap walkers. Scalar/reference and
aggregate managed memory semantics remain implementation work.

Parallel stop-the-world evacuation is the selected target. Its worker queue,
forwarding ownership/publication and termination protocol are implemented above.
Exact batched copy reservation is implemented; copying buffers with local cursors
remain future work; mutator TLABs and initialized-range metadata are implemented.
Concurrent
moving collection would additionally require a specified read/write-barrier and
evacuation protocol preserving mutations and roots during application execution.
Measure mutator scaling, allocation throughput, pause distributions, retained
heap and barrier cost before claiming efficient collector behavior generally.

Allocation stress mode: use `gcr run --gc-stress ...` or set
`GCR_GC_STRESS=1` for JIT/native execution (including a standalone native
binary). Every managed allocation through the runtime or `MutatorThread`
requests a collection before allocation, with published roots. Allocation
buffers are bypassed. Contending requests retry after servicing the current
pause rather than coalescing; an empty nursery still receives a stress cycle.
Raw heap allocation primitives do not collect by themselves: embedding code
must use its registered mutator allocation API. Stress mode deliberately
sacrifices throughput to expose missing roots and relocation errors.
