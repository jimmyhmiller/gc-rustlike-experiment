# Garbage collector and runtime

The runtime lives in `crates/gcrust-rt`. `gc/heap.rs`, `gc/semi_space.rs`,
`gc/scan.rs`, `gc/roots.rs`, `gc/thread.rs`, and `runtime.rs` implement the
collector, root protocol, mutator coordination, and generated-code entry points.

Normal execution uses a nursery plus copying tenured space. Minor collections
promote survivors. Major collection copies tenured objects. A card-table write
barrier records old-to-young stores, including references inside flattened value
fields. Objects carry a type ID; TypeInfo describes the traced locations.

When promotion capacity is tight, the read-only survivor preflight uses the
same dirty-card object enumeration as minor evacuation, plus native channel
owner edges. It leaves cards intact for evacuation and avoids rescanning the
entire tenured heap. Direct raw old-to-young writes must mark their cards,
including initialization of objects allocated directly into tenured space.

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
Objects without traced payload slots need no scan task; native channel-owner
edges are enrolled independently during claiming, including for opaque owners.
There are no losing duplicate copies or unused copying-buffer tails. Linear heap
walks wait until all reservations are initialized; copying does not zero-fill first.
The same protocol promotes nursery survivors into tenured space during minor GC.

A shared queue transfers batches of up to 128 tasks under one mutex. Workers
reuse OS-thread task, discovery, layout and destination-address scratch buffers
across cycles; heap-borrowing claim guards remain job-local. Each active producer
can retain up to 128 published objects locally, with a 32 KiB copied-work bound,
and donates excess work or work requested by sleeping helpers. Workers publish
pending copies before publishing discovered work. Termination requires both an
empty queue and zero active batches; a worker publishes discoveries before
retiring its active batch. Idle workers sleep on a condition variable. Root/field
rewrites use atomic accesses, and destination cards are rebuilt during major
object scanning. Every participant completes before swapping spaces or resetting
the nursery. The worker pool dispatches borrowed jobs synchronously and waits for
all helpers even when a coordinator or helper panics. Helpers do not retain a heap
reference between jobs, and their handles are joined during heap teardown.
Moving-collection panics abort the process at the collection boundary. A failed
partially relocated cycle has no complete rollback and must not resume managed
execution. The worker pool's generic borrowed-job panic propagation does not
make collector failures recoverable.

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

`Heap::register_permanent_extra` is unsafe: the source must keep a stable address
and outlive the heap, and its slots must remain valid, nonoverlapping and quiescent
through collection. Growth may park and relocate managed objects. Publish frame
chains and every other live root before calling, then reload derived pointers.
A pinned temporary source keeps the new roots visible while backing is allocated
outside collector/registry locks. Collector exclusion transfers publication to
the permanent registry without enumerating the same slots twice. Old backing is
freed while callers remain BLOCKED. Spare-capacity publication neither allocates
nor parks. This internal bridge does not mark later snapshots incomplete as
external per-call root histories can do.


## Allocation and root fast paths — 2026-10-06

The release compiler uses LLVM O3 and reports optimization failures. Fixed Full
header objects of at most 8192 bytes can allocate directly in the owning
mutator's nursery TLAB. The generated path checks capacity,
the published reservation limit and stress mode. Native allocation validates
the arena reset epoch. It initializes the object, writes its
header, advances the shared native/generated cursor and publishes the initialized
extent before any safepoint. Refill, unsupported layouts, stress allocation and
collection retain the runtime path. The runtime validates canonical registered
layouts; general embedding allocation keeps its full descriptor checks.

The inline window is a stable member of ThreadState. Its initialized-extent
pointer remains owned by the TLAB until refill closes the old window. Epoch,
stress and extent publication are atomic; cursor, limit and allocation-site
counters belong exclusively to the owning mutator. Generated allocations update
the same exact site counts and byte totals as runtime allocations. Counter-vector
growth occurs on the native first-use allocation path. Non-generational allocation windows
stay disabled.

Optimized reference locals use private working slots and escaped traced mirrors.
Every assignment updates both. Functions with no reference parameters, closure
captures or indirect roots defer frame registration until their first reference
assignment. Until then only private slots are initialized; frame metadata and
mirrors are initialized when registration occurs, before the new reference is
published and before any subsequent safepoint. Inactive frames skip relocation
checks, mirror reads and unlinking. Full-debug functions register on entry.
Before returning mutators to RUNNING, every world
pause advances each census member's owner-local relocation epoch after collector
root updates and before resuming that owner. Potentially collecting or parking
calls compare ordinary epoch loads and refresh working
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

## Private heap graphs and bounded safepoints

The compiler performs a conservative whole-program inclusion analysis of managed
allocation origins. Origins flow through local aliases, assignments, direct
call parameters, returns, branches and loop exits. Reference arrays and object
fields also propagate their contained references. Memory-flow edges and origin
sets are solved together to a fixed point, including cyclic graphs and stores
that follow loads in source order. Escaping containers transitively escape their
contents. Captures, foreign buffers, unknown owners and unsupported aggregates
remain conservative; opaque option payloads cannot hide private references.
Address-taken parameters, closure captures and incoming execution arguments have
unknown origins. Scalar/reference array slots and scalar/reference object fields
use ordinary memory operations only when every possible owner allocation is
known and unescaped. Other managed accesses retain their SC behavior. Pointer
stores retain their generational write barriers, including private old-to-young
edges. Mutable aggregate field snapshots still use the managed lock protocol.

Disjoint proven origin sets receive access-scoped LLVM alias metadata. Overlapping
sets remain potentially aliased; this is not a noalias claim on Thread pointers
or function arguments. Runtime calls and collector/root memory carry no such
metadata. Immutable array lengths are propagated only when every possible origin
has the same proven constant length and none is null or unknown. Integer
constants likewise require complete compatible dataflow. Native code is compiled
in partitions, so helpers are linked across JIT units or AOT objects. Their only
callers are the program's own code: AOT symbols carry a `gcr.` prefix that no C
identifier can name, and the executable exports only `main`. Address-taken
functions and the execution entry have unknown origins, and foreign declarations
keep their C ABI. Full-debug code disables these proofs and retains editable
generic roots.

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

### Reference-enum shapes

Reference enums store their `u32` tag in the low 32 bits of the Full header's
first word. Its forwarding bit remains clear in live objects. The parallel
collector claims the second header word and copies the first word unchanged;
the legacy copying collector copies the original header before forwarding the
source. Neither collector changes a destination's tag. Pointer payload slots
start at byte 16; scalar and inline-value payloads follow the pointer region.

Payload-free constructions can allocate a separate header-only physical shape.
The compiler appends these descriptors after the nominal Core layouts using one
deterministic expansion shared by generated allocation code, JIT tables and AOT
metadata. Physical IDs describe allocation extent and tracing; `type_id_of`
maps the extra shape back to its nominal source layout. Both shapes retain the
same type name and variant metadata. Allocation-site labels remain nominal,
while counters record the actual allocated bytes. Exhaustion of the physical
`u16` namespace retains the union representation.

An empty reflection field list alone does not prove an empty payload: some
inline value payloads are intentionally omitted from scalar reflection. Codegen
also requires an empty construction argument list before selecting the smaller
shape. Every construction still allocates a distinct object. Debug mode uses
the same layout, so full-debug, optimized native and JIT executions agree on
physical sizes and nominal reflection.

Before a collection resumes its census, it closes every generated TLAB window,
including the triggering collector and BLOCKED workers. This happens under the
registration/retirement lock before clearing the collection request. The
compiled capacity check therefore rejects a stale reservation without loading
an arena epoch on every allocation. The owning native allocation path still
checks arena identity and epoch before reusing or reopening a reservation.
Initialized-prefix publication and the acquire stress-mode check remain intact.

Fast constructors write the complete Full header as one native-layout aggregate.
Scalar/reference stores cover known payload byte ranges; only uncovered bytes
are cleared. Inline values retain conservative full payload clearing. These
stores precede initialized-prefix publication. Collecting slow paths reload
relocatable operands from roots before constructing the payload. A zero tag
already present in a freshly cleared native allocation needs no extra store.

Backward liveness clears a direct traced mirror once when its local value dies.
Branch successor sets are united, loops iterate to a fixed point, break and
continue use their corresponding loop successors, and returns end continuation
liveness. New definitions are included when clearing unused assigned values.
Private working slots remain untouched; indirect value roots retain their prior
lifetimes. Full-debug keeps lexical roots for inspection. A snapshot regression
checks both optimized reachability and full-debug preservation.

Native objects using only allocation-window invalidation require at least the
`gcr_runtime_main_v2` startup contract. Current generated objects require v3. An older
custom archive therefore fails to link rather than silently violating generated
allocation-window assumptions. The current runtime keeps the legacy startup
entry for programs that use the older per-allocation epoch check.


### Owner-local epoch and recursive branch outlining

The v3 AOT startup contract stores each relocation epoch in its ThreadState.
The coordinator advances it only while that owner is quiescent (including
BLOCKED threads), or on the collecting owner's own call stack. Resume's
release/acquire handshake orders those writes before generated reads. A
nonmoving world pause that excludes its requesting owner does not write that
owner's epoch. Generated calls can therefore compare ordinary loads without a
shared atomic read. The epoch remains atomic storage for legacy generated
callers. `gcr_runtime_main_v3` prevents linking this code against older archives;
the v1 and v2 entry symbols remain compatibility wrappers in the new runtime.
FFI leave still reloads traced mirrors unconditionally.

Release code generation can split an allocating recursive branch from a small
base case. The helper is noinline, but the original wrapper can inline at its
callers, exposing base-case allocations without recursive stack setup. The
current eligibility proof permits straight-line blocks and requires every
non-parameter read to refer to a definition within the extracted branch.
Assignment, control transfer, loops, closures, and unsupported expressions
retain their original bodies. The ordinary root protocol is generated for the
helper; allocation-site labels and source spans retain the source function.
Full debug code generation leaves source functions intact.

### GC-aware self tail calls

Direct self tail calls are normalized into Core loops before all of these
analyses, including adjacent ANF call results returned immediately. Arguments
are evaluated into fresh temporaries before parallel parameter assignment.
Nonrecursive exits retain their return values. Closure functions and tail
returns inside existing loops are left as ordinary calls. LLVM tail-call
elimination is disabled for managed functions: the backend must not create
backedges after the compiler has placed its safepoints. This constraint applies
to full-debug too; recognized direct self recursion keeps one logical frame
with the original parameter and local names.

Inline array reads, including immutable value-box loads, scalar/reference array
writes and scalar/reference fields cannot collect or park. Call-effect analysis
recognizes those operations and inline aggregate
construction. Mutable aggregate field snapshots and boxed value-array writes
retain their relocation checks and roots.

### Versioned polling string scans

Large String equality and FNV hashing can service a pending collection in
`ai_str_eq_v2` / `ai_str_hash_v2`. Inputs are immutable, scratch-rooted before
polling, and reloaded for each scan chunk. No managed slice survives a poll.
Hashes preserve the original FNV-1a constants and nonnegative 63-bit result.

The original `ai_str_eq` / `ai_str_hash` symbols remain noncollecting: older
compiled callers omit root-cache invalidation around them. Changing those
symbols to collect would silently invalidate the old calling convention.
Current code generation guards the immutable first input's byte count against
`STRING_SCAN_LEAF_BYTES` (32 KiB). Small inputs call the original leaf symbols;
larger inputs call the versioned symbols with relocation-epoch checks and root
reloads. Equality with unequal lengths remains constant work on the leaf path.
The versioned symbols make incompatible custom archives fail at link time.
Thread/AotLayout offsets and the v3 startup record remain unchanged.

This bounds work between scan polls, not total GC pause or OS scheduling time.
Legacy callers retain their old unbounded scan behavior until recompiled.

## Pointer policies in heap inspection

Embedders using tagged or boxed references must select the same `PtrPolicy` for
inspection as for collection. `Heap::visit_roots_with_policy::<P>` decodes root
slots; `dump_heap_text_with_policy::<P>` and `dump_heap_json_with_policy::<P>`
decode graph edges and reflected references, including inline value fields.
Inline value enums render their active variant and payload; their metadata tag
and payload offsets are interpreted relative to the embedded value base.
Non-pointer values and decoded null pointers are omitted from roots and edges.
The existing convenience entry points use `IdentityPtrPolicy`.

These remain unsafe inspection APIs: objects and metadata must be initialized,
and the selected policy must match all traced references. Root visitation requires
an existing world pause; text/JSON dumping acquires its own pause and must not be
called while already holding collector exclusion. JSON schema and object IDs are
unchanged. Use the policy-aware APIs for correct root attribution, reachability
and retained-size analysis of custom-encoded heaps.

## Telemetry waits and mutator coordination

Public event/summary/metrics readers, trace configuration and trace flushing may
wait for native locks or I/O. They suspend every RUNNING registration on the
calling OS thread before waiting, then release the telemetry lock before
restoring those registrations. Already BLOCKED entries stay blocked. This also
applies on native unwind; trace sink panics still propagate and poison remains
observable. JIT embedders must publish current frames before these calls and
reload derived managed pointers afterward, as at other safepoints.

Collector event recording retains its paused path. Optional trace writes can
still delay a collection, and embedding sinks must not reenter the heap or
managed runtime. Parking a flushing caller permits an independent world-pause
inspection; it does not make a blocked trace sink asynchronous.

## Inline enum reflection metadata

Value enums describe their tag and variant payloads in `ValueMeta`. Nested value
payloads can store raw bytes in the payload region while their references occupy
the enclosing enum's leading pointer slots. `FieldTy::ProjectedValue` records
child-relative/enclosing-relative reference-offset pairs; rendering composes these
maps while reading scalar/tag bytes from the raw value base. Named variant fields
retain their source names. Boxed enum payload values use the same representation.

`field_count`, `field_kind` and `field_name` include aggregate enum payload fields
in source order. Their kind is 5; `field_i64` rejects aggregate storage. Metadata
builders and reflection emission return errors for unrepresentable offsets/counts.
The metadata encoding retains existing tags and adds field tag 3 for projected
values, with a checked u16 mapping count and u16 offset pairs.

Generated native programs require `gcr_runtime_main_v4` so incompatible custom
archives fail to link. The runtime keeps v3/v2/legacy startup exports for older
generated programs and their existing metadata. `gc::reflect::encode` and the
compiler's metadata builders supply the current format.


### Native thread-census storage

`native_registry_bytes` includes the allocator-requested backing of the registered
thread census. Sparse replacement is staged outside the census mutex and checked
against current membership before installation; state allocations and identities
stay stable. Old backing remains charged until freed outside registry/coordinator
locks. Final retirement releases an empty census even while host observers retain
retired states. Native pressure combines these bytes with the existing registry
and buffer ledgers.

Registered SATB and allocation-site counter backing is also allocator-accounted.
Retired state observers keep those bytes charged until their final Arc releases
the actual allocation, even after Heap drop. SATB queue/iterator handoffs preserve
the buffer's allocator; bounded collector flushes reuse it. Standalone snapshots
use standard vectors outside this ledger, while public SATB draining resets its
configured local reservation. Warm profile writes add no allocation/accounting.

Heap-retired allocation-site totals also retain their allocator charge until
Heap release, independently of retired state observers. Profile reports copy
into standalone vectors rather than retaining the heap allocator.

Heap-created global SATB queues charge their deduplication bitmap, chunk-container
backing and bounded-cycle payload buffers. Public standard-vector inputs transfer
their backing without copying and become charged at adoption. Already-accounted
mutator chunks preserve their original allocator without a second charge.
Drained chunk containers and payload iterators retain allocator ownership until
physical release. The bitmap remains charged across cycle boundaries until the
queue drops. Public `drain_all` produces a caller-owned standard snapshot outside
this ledger and releases the source batch after copying.

This metric excludes ThreadState record/control allocations (including embedded
scratch roots), temporary census/profile snapshots and other unaccounted collector
auxiliary storage. Independently constructed standalone queues have no Heap
ledger. The metric is allocator-requested ownership, not total native RSS.


## Optional statemap history — 2026-10-19

Attaching a `StatemapTracer` enables event history. `TraceLimits` bounds committed
history by event count and custom entity UTF-8 bytes (defaults: 65536 events and
1 MiB). Oldest events are evicted as needed; an individually oversized entity or
zero event capacity rejects recording. `stats()` reports retained events, text
bytes, total attempts, evictions and rejections. Export includes loss counts in
its title. GC and canonical numeric thread entities require no recording-time
allocation, including rollover. Custom names retain their original text.

Export uses the tracer creation wall-clock origin and monotonic event offsets,
escapes JSON names, and takes a bounded snapshot before invoking the writer.
Concurrent and reentrant recording therefore continues independently of output.
Each export has its own snapshot allocation; custom strings awaiting publication
and caller storage are separate from the committed-history limits. These limits
are not a process RSS quota or a charge to one Heap's metadata ledger. The pinned
upstream renderer currently mishandles quoted names when producing SVG; our JSON
preserves those names correctly (see the project pad's upstream renderer issue).


## Parallel worker storage and job lifetime — 2026-10-19

Heap-created worker pools charge the requested capacity of their native helper
handle vector to `native_registry_bytes`. Changing participation can leave idle
helpers and retained capacity; both belong to the pool until Heap destruction.
Pool destruction shuts down and joins helpers, including native TLS teardown,
before releasing handle-vector backing. Shared control allocations, OS resources
and reusable thread-local scratch have separate lifetimes and remain outside this
scoped metric.

Synchronous job dispatch preserves the borrowed closure pointer through type
erasure. Helpers receive shared access to `F: Sync`; the closure need not be Send
or static. Dispatch holds its exclusive ownership until all participants finish,
then clears the published job before returning or propagating a panic. This
pointer-preserving job representation does not change managed-value encoding or
establish whole-collector strict provenance.


## Temporary root-address metadata — 2026-10-19

The native registry metric also includes the collector's temporary root-address
hash backing, including bucket/control storage. The inline prefix borrows the
Heap counter and takes no allocator owner. Spill storage acquires independent
allocator ownership; its consuming iterator keeps backing charged until physical
release, even after the original borrow ends. This table deduplicates writable
slot addresses, not their managed values. Caller root storage remains separate.

The charge is transient and is usually gone before parallel dispatch, so an
end-of-collection sample alone does not describe peak metadata ownership. The
metric still excludes other collector auxiliary storage, record/control overhead
and OS-thread scratch caches; it is not total native RSS. These ownership scopes do not change root-source
quiescence or managed-address lifetime requirements.


## Temporary evacuation FIFO metadata — 2026-10-19

Heap collections charge the actual backing of their cycle-local FIFO to the
native registry ledger. The allocator-aware initialized arena reuses inactive
Copy task slots, exposes only the logical head/length interval, and retains its
charge until physical release. Growth accounts both allocations during overlap.
The normal 4096-root release-client reproduction now observes 65536 bytes during
dispatch and zero afterward. This extends storage accounting; it does not make
the ledger a complete process RSS measurement. Initial paired measurements show
small-collection and single-worker wide-graph regressions under investigation;
see the current checkpoint in `GC_READINESS.md` for scope and evidence.


## Temporary remembered-object metadata — 2026-10-19

Minor collection charges actual remembered-object hash backing to the Heap
registry allocator. Its exact-sized owning iterator supplies tasks directly to
FIFO construction, avoiding a separate materialized task vector. The table
remains charged through enrollment and iterator exhaustion until physical
release. Empty major/clean-minor paths allocate neither a table nor an allocator
owner. Managed object addresses remain valid under the same scoped pause.

The 4096-parent normal fixture reports actual table requests of 73744 bytes on
Linux and 73736 on macOS; accounting uses allocator requests rather than an
assumed cross-platform layout. Removing the vector saves one 65536-byte request
in the Linux fixture, while retaining the table longer increases its measured
peak live requested storage by 8208 bytes. Isolated dirty-minor timing improves;
fresh applied-source gates are described in `GC_READINESS.md`. Initialized-range
and thread snapshots, other controls and OS-thread scratch remain separately
scoped; this metric is not complete process RSS.


## Initialized allocation-region snapshots — 2026-10-19

Heap-owned collector/inspection walks and remembered-card enumeration use a
snapshot of initialized regions that excludes active and retired TLAB tails.
Up to three regions are inline; larger snapshots borrow the Heap accounting
allocator and use one backing buffer. Sorted holes are converted to initialized
prefixes in place; small coalesced results release temporary backing immediately.
Standalone arena walking retains its global-allocator entry point. Actual backing
is charged through physical release, including while visitors run. No buffer
registry lock is held across a visitor callback; nested metadata inspection is
covered by native tests.

Walkers require stopped mutators or exclusive arena/heap access with valid,
stable allocation extents. Visitors must not collect or invalidate visited
objects. The backing ledger still excludes thread census snapshots, persistent
object-index storage, TLAB controls and other separately scoped owners. Current
validation and measured sparse/dense tradeoffs are in `GC_READINESS.md`.
