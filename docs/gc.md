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
| GCR_STRESS_HEAP_MB | 8 | Per-space MiB for every-allocation stress execution, shared by JIT/native |
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

## Embedding allocation and header contracts — 2026-10-06

`ObjHeader` is an unsafe trait: its physical size, initialized type-id layout and
reserved first forwarding word must satisfy the documented representation
contract. Arenas retain the validated physical layout. Allocation rejects header
size/alignment mismatches, malformed interior-reference offsets and invalid
extents before reserving space or writing a header. Atomic arenas remain limited
to eight-byte alignment; the legacy arena retains greater alignment support.
Zero-sized arenas are rejected before calling the native allocator.

Heap construction validates layout ids against their table indices. Heap and
mutator allocation reject unregistered or altered layouts, and initialized
allocation also checks the header's physical size, alignment and type-id offset.
Invalid mutator requests return null before triggering stress GC or reserving a
TLAB. `dynamic_add_type` now requires `&mut Heap`, assigns the new id and rejects
id exhaustion; shared-reference vector mutation is removed. Live shared-runtime
type extension is not provided by this API. A world pause cannot invalidate
outstanding Rust borrows of existing layouts.

Direct Heap allocation is unsafe. Callers must either be registered RUNNING
mutators or have exclusive access without collector/mutator activity. They finish
header/length initialization and root publication before a safepoint, heap walk
or collection. Taking a world pause does not authorize another thread to
allocate. JIT frame-walker installation and allocation-window remapping are
also unsafe, with explicit quiescence and storage-lifetime contracts.

The legacy Cheney collector scans exact allocation starts instead of treating
alignment padding as objects. Its forwarding entry uses the reserved first
header word, matching the heap collector and `follow_forwarding`, including Full
headers whose type id is in the second word. Regressions retain mixed-alignment
cyclic graphs through repeated collections and verify Full-header forwarding.

Dynamic root frames store real `Cell<u64>` slots. Escaping registration guards
require unsafe lifetime management; `DynRootFrame::with_pushed` keeps its guard
private, rejects duplicate registration, and unlinks on normal return or unwind.
The managed references placed in slots remain subject to the collector's pointer
policy and root-enumeration contracts.


## Allocation and root fast paths — 2026-10-06

The release compiler uses LLVM O3 and reports optimization failures. Fixed Full
header objects of at most 8192 bytes can allocate directly in the owning
mutator's nursery TLAB. The generated path checks capacity, unsigned overflow,
the arena reset epoch and stress mode. It zeroes the object, initializes its
header, advances the shared native/generated cursor and publishes the initialized
extent before any safepoint. Refill, unsupported layouts, stress allocation and
collection retain the runtime path. The runtime validates canonical registered
layouts; general embedding allocation keeps its full descriptor checks.

The inline window is a stable member of ThreadState. Its initialized-extent
pointer remains owned by the TLAB until refill closes the old window. Epoch,
stress and extent publication are atomic; cursor, limit and allocation-site
counters belong exclusively to the owning mutator. Generated allocations update
the same exact site counts and byte totals as runtime allocations. Counter-vector
growth uses a noncollecting runtime helper. Non-generational allocation windows
stay disabled.

Optimized reference locals use private working slots and escaped traced mirrors.
Every assignment updates both. Functions with no reference parameters, closure
captures or indirect roots defer frame registration until their first reference
assignment. Until then only private slots are initialized; frame metadata and
mirrors are initialized when registration occurs, before the new reference is
published and before any subsequent safepoint. Inactive frames skip relocation
checks, mirror reads and unlinking. Full-debug functions register on entry.
Before returning mutators to RUNNING, every world
pause publishes a relocation epoch after collector root updates. Potentially
collecting or parking calls compare acquire-loaded epochs and refresh working
references if the epoch changed. Loop-poll fast paths leave private references
available to LLVM register promotion. Unknown calls are treated conservatively;
only explicitly known noncollecting runtime routines, LLVM intrinsics and
managed callees proven nonrelocating by a conservative call-graph fixed point
skip this check. The analysis admits straight-line arithmetic, locals, branches, immutable enum reads
and direct calls; loops, allocation, foreign/indirect calls and unsupported IR
operations remain effectful. Pure recursive components are safe only when all
their operations and outgoing calls are safe. Proven nonrelocating optimized
functions also omit root frames: collectors cannot move objects until the
executing mutator reaches a safepoint, and these functions contain none. Callers
retain their traced roots across the call. Full-debug functions retain frames. Managed heap field loads and stores retain their existing sequentially
consistent semantics.

A foreign call leaves the mutator BLOCKED. Generated code never reloads its roots
in that state. After ai_ffi_leave restores RUNNING, it refreshes registered mirrors
unconditionally, including roots moved during callbacks and before managed-array
copy-out. Full-debug builds continue using root slots directly to preserve
editable debugger variables.

When the remembered set is empty, minor collection omits tenured indexing and
starts directly from traced roots. A later dirty collection indexes every new
initialized range accumulated since the previous update, including arena resets.
Tenured object/card overlap metadata is extended only over newly initialized
arena ranges and discarded on arena reset. This includes TLAB prefixes filled in
address order different from reservation order. Cards use atomic bitmap words;
concurrent marks of different cards in a word compose with fetch_or. Collection
still uses initialized-range metadata to avoid interpreting unused TLAB tails as
objects. Neither optimization changes which references are traced.

## Private arrays and bounded safepoints

The compiler performs a conservative whole-program inclusion analysis of scalar
array allocation origins. Origins flow through local aliases, assignments, direct
call parameters, returns, branches and loop exits. Heap storage, captures,
foreign buffers and unknown uses escape their reference operands. Address-taken
parameters, closure captures and incoming execution arguments have unknown
origins. Unsupported operations prevent specialization. A scalar-array access
uses ordinary memory operations only if every possible allocation origin is
known and unescaped. All other managed accesses retain their SC behavior.

Disjoint proven origin sets receive access-scoped LLVM alias metadata. Overlapping
sets remain potentially aliased; this is not a noalias claim on Thread pointers
or function arguments. Runtime calls and collector/root memory carry no such
metadata. Immutable array lengths are propagated only when every possible origin
has the same proven constant length and none is null or unknown. Integer
constants likewise require complete compatible dataflow. Optimized whole-program
helpers have internal linkage; the execution entry remains externally callable,
and foreign declarations keep their C ABI. Full-debug code disables these proofs
and retains editable generic roots.

Array-length loads no longer use invariant.load metadata. A managed object's
length is immutable, but a collector can recycle the physical memory location
for a different object. Object-origin facts preserve the former property without
claiming global invariance of the latter.

Small loops may coalesce header polls into a function-entry poll. The proof
requires an i64 induction variable with a known nonnegative initial range, a
constant upper bound of at most 32, exactly one final increment by one, no
counter reset or continue path, and bounded body work of at most 8192 IR steps
(including nested loops). Unknown calls, allocation and unsupported effects
reject the proof. Other loops keep atomic acquire/volatile header polls.
Functions with coalesced loops always poll on entry, including recursive calls,
so repeated short invocations still cooperate with pending collection.

When a coalesced function's entire remaining body is also proven nonrelocating,
only its incoming references need entry roots. The fast entry path runs without
registering a frame. The slow path initializes and links incoming roots before
parking, reloads their relocated values on return and unlinks the frame before
executing the bounded body. This specialization excludes value locals containing
interior references and full-debug code. Potentially collecting callers continue
to refresh their own working references after the call.
