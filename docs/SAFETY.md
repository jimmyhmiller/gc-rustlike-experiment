# Language and safety contract

## Component-specific native admission — 2026-10-18

Atomic, channel and completion admission now snapshot growth requests per backing
component. Zero requests allocate no backing for that component. Atomic plans
have two typed entries; channel/completion plans have three. Owner-index requests
use total record count, not current owner count, preserving future allocation-free
binding of all raw handles. Raw-channel and pending-list requests use their own
live counts.

Installation first checks each component's requested-size budget against current
liveness and preflights every exhausted component. A missing or insufficient
component rejects the entire stage before any move. Only components still needing
growth are transferred/swapped under the registry mutex. Other storage remains
untouched; native object addresses and pending order/positions remain valid.
Allocation and disposal retain the existing off-lock BLOCKED protocol. GC or
competing admission can invalidate a plan; the producer discards it off-lock and
replans without publishing partial state.

References cover each component, untouched storage addresses, zero allocator
traffic at commit/publication/binding, raw-history future reservation, incomplete
stages, per-component stale requests and pending order. Six deliberate controls
reject unnecessary planning/allocation, lost raw-owner reservation, global-only
budgeting, missing preflight and reversed pending order. Full native metadata
accounting/admission, teardown/scoped-root coordination and broader concurrent
coverage remain separate requirements.

## Off-pause registry backing and joined publication — 2026-10-18

Weak sweeping no longer shrinks completion, atomic or channel registry tables.
It removes dead ownership, publishes native destruction and schedules backing
maintenance. After resuming other mutators and releasing collector exclusion,
maintenance allocates replacement storage without a registry lock. A short
registry-locked commit rechecks liveness, available capacity and stale-request
budgets, moves entries without allocation, and retains old/unused backing until
unlock. Record addresses do not move. Pending order/positions are unchanged.
Owner-index reservation covers all records that may bind later; raw-channel
membership is compacted against its own live count.

Mutator GC retains its existing BLOCKED caller chain through maintenance and
reacquires coordinator exclusion before restoring registrations together.
Unregistered/BLOCKED collection entrypoints retain that contract throughout.
`collect(extra_roots)` publishes its borrowed sources across maintenance and
waits for collector exclusion before unregistering them: parallel forwarding
stores can outlive root enumeration. Reference subprocesses pause allocation and
disposal, run another GC, and verify roots and nested registration states across
major/minor, STW, concurrent, native-pressure and explicit-extra entry paths.
The extra-root call cannot return through another active coordinator owner.

Completion records publish a monotone terminal flag with release ordering only
after full OS/TLS join and scalar outcome caching. Acquire readers can recognize
terminal records without the state mutex, even while a native observer holds it.
A false flag still permits the existing nonblocking state probe; active joins
continue to defer reclamation. Observers retain native ownership independently
of registry removal. The already-joined/held-state regression fails before this
fix; the previously intermittent churn case passes repeated optimized checks.

These guarantees do not eliminate allocation-free commit lock waits. Native
metadata admission/accounting and remaining embedding coordination are separate
work. The later allocation-free scoped-root repair replaced descriptor allocation
and disposal with pinned borrowed nodes; see GC_READINESS.md, "Allocation-free
scoped extra roots". Root publication must still extend through return and all
late parallel forwarding writes. Removal from an unregistered/BLOCKED caller
therefore retains collector exclusion; freeing borrowed storage early is invalid.

## Completion metadata retirement — 2026-10-18

Completion admission prepares a private typed retirement link while BLOCKED,
before registry publication. Registry insertion requires that preparation.
Sweeping first unlinks the reaper catalogue and dense pending entry, removes the
registry's strong owner, then transfers the link exactly once into the completion
mailbox. Clearing transfers all remaining records through the same path. No
retirement node is allocated at either transfer.

The native worker detaches the mailbox under its monitor mutex and unlinks each
record before destruction, outside monitor, registry and collector locks.
Completion state and reaper registration contain native ownership only; managed
results remain separately traced. Native observers retain their scalar state
until their own release. Reaper probes can also retain a temporary native borrow.
The existing shared activity accounts for completion jobs across worker epochs,
and execution teardown drains them after joining/detaching completion reapers.

A non-compacting sweep reference checks zero allocator traffic and deferred
physical destruction. Another reference pauses native destruction, checks that
registry removal is immediate and scalar observation remains valid, then runs
another real collection in semi-space and generational subprocesses. Restoring
inline destruction makes the allocation/destruction reference fail. Registry
shrinking still allocates/frees during collection and is tracked separately.

## Cross-epoch native activity and pressure snapshots

Every native worker epoch of a retirement service retains the same lazy activity
object. Publication increments activity under the monitor before queue visibility;
completion decrements it only after native destruction. A consumer's nested or
foreign-epoch publication therefore enters activity before that consumer finishes.
Global drain observes all published work, including monitors detached at an older
execution boundary. Reserved observer storage is not pending work. Monitors retain
activity without retaining Heap or the service, so observers may still outlive it.

Lock order is monitor → activity. Activity callbacks never acquire monitor locks.
Draining waits without coordinator/registry exclusion. Native pressure prepares
activity while BLOCKED before taking the coordinator; it does not lazily allocate
at the idle snapshot. After drain, pressure reacquires the coordinator and tests
idle again. If publication intervened, it releases the coordinator before waiting.
Threshold retuning runs under activity publication exclusion and performs only
native atomic accounting. Caller registrations restore while coordinator exclusion
is still held. Work published after the snapshot is later work, not part of the
completed drain guarantee.

## Typed native record retirement

Atomic cells and channel controls use private typed links. Their Record
implementations return the unique link and corresponding typed mailbox.
Observers never access links. After once-only registry removal, ownership passes
through a private batch, publication under the native monitor, and a detached
consumer. Each source remains retained in queued records until consumption,
including after a worker epoch closes. Unlinking precedes native destruction and
is iterative. No per-retirement node allocation is needed.

Collector channel snapshots live inside parallel evacuation; that call returns
and drops its temporary control Arcs before the heap sweeps weak native owners.
This ordering prevents a collector snapshot from becoming the last control owner
after the native worker has released registry ownership. A real-GC pause test
checks only the worker and test observer retain a dead control after GC returns.

The native consumer destroys control metadata outside all registry/coordinator
locks. Same-worker backing releases consume reserved producers in one transaction
and free backing under the existing pending controller jobs. Those jobs complete
only after backing is gone. A retained monitor identifies this native mode;
foreign-epoch backing must still publish to its own mailbox. Native observers
can retain a controller/backing independently; teardown drains registry handoff,
while late observer release follows the original native lifetime protocol.

## Deferred native atomic records

Native atomic pointers still address a stable leading AtomicI64 field. No
managed pointer is stored in the cell or dereferenced by its retirement worker.
The private link is initialized before publication while BLOCKED. Only the
owning registry may transfer it, once, into a producer batch. Publication links
whole batches under the native monitor; the worker detaches queues before
consuming links or destroying records. Iterative unlinking avoids recursive
native destruction. No observer accesses links; observers only access the scalar.

Cells share a producer source prepared once per worker epoch. Queued links
retain it until native consumption, so an old epoch can close before a late
handoff without exiting prematurely or freeing producer metadata in GC. The
worker drops its cached source before deciding whether to join or detach.
Cancellation of an unpublished cell stays native BLOCKED. Root teardown drains
both scalar records and channel backing before ending the owned service.
Native observers may retain records after registry removal; their final Arc
release determines final destruction. Internal Weak tests must check registry
removal at GC and wait for native drain before asserting destruction.

## Collection caller ownership chain

`PauseCallers` uses links inside the existing native `ThreadState` records.
Only the registration's owning OS thread accesses its `UnsafeCell` link; GC
threads never inspect it. The private guard retains the chain head and each
link retains the next record with an Arc. Under the thread registry lock it
selects only same-owner RUNNING registrations, links each before publishing
BLOCKED, and performs no native allocation. Nested guards skip existing BLOCKED
registrations. Restoration takes each link before returning that registration
to RUNNING, so subsequent guards may reuse it. Iterative unlinking avoids
recursive chain destruction. Existing outer blocked scopes remain unchanged.
All existing root-publication and coordinator-exclusion requirements still apply.

## Handoff storage preparation

Before native preparation can park the parent, a private guard registers an
inline RootFrame containing the environment. Rooted storage stays at a stable
stack address throughout the callback and unwind. Every post-park attempt
reloads that frame's current value; the original raw argument is not reused.
No vacancy is claimed and no handoff root is published until staged native
buffers have been installed or discarded and disposed while BLOCKED.

Commit runs on the owning RUNNING parent, excluding relocation. Pool/root
backing locks serialize native access; guards end before parking. Transfer moves
initialized atomic words without cloning/reinterpreting pointer bits, preserves
indices, and returns empty obsolete backing for off-lock disposal. Safe external
root reads continue to use the backing lock; unsafe mutation callers retain
their collection-exclusion obligations.

Pool return capacity covers every allocated handoff slot, including slots still
owned by children. A new root is appended only when both global and return
capacity are available. Slot ownership stays one-shot through the handoff guard;
return neither reallocates nor publishes a new managed reference. Failed cleanup
still performs the coordinator-protected clear before making the index vacant.

## Handoff root retirement

The handoff owns its global root until transfer into a RUNNING child's scratch
scope, or failed-admission cleanup. An unregistered destructor cannot use the
root backing mutex alone to retire that root: parallel evacuation keeps slot
addresses after enumeration and can still publish a forwarding value.

Failure cleanup parks a current entry, then acquires the heap collection
coordinator before clearing its exclusively owned slot. That guard excludes the
entire evacuation, including its final root stores. No new managed reference is
published. The vacant index is made available only after the clear completes.
Cleanup must not wait for this coordinator while RUNNING, nor hold the backing
mutex or vacancy-pool mutex while acquiring it. The coordinator follows its
existing poison policy and cleanup does not inspect the thread registry, which
may be poisoned by the very admission failure being unwound.

## Prepared completion catalogue links

A completion's `OnceLock<Registration>` publishes one native node and its owning
monitor before the record can be enqueued. The node holds only a weak completion
reference. Its private `UnsafeCell<Links>` is accessed by internal unsafe
catalogue operations under that exact monitor's state mutex. Enqueue validates
monitor identity; removal dispatches through the record's registration rather
than the current execution reaper, because shutdown TLS can create a new epoch.
The node's Send/Sync implementation relies on this exclusion contract.

Link operations retain the target node through unlock. Neighbours remain in the
chain, so unlinking drops no last node owner under the monitor. Batch traversal
borrows strong chain links while locked and returns only a fixed array of weak
completion references. No borrowed link escapes the guard. Removal advances a
cursor that names the removed node; new work is appended to the FIFO chain.
A pass snapshots its starting queue length as a visit budget. New appends cannot
extend that pass indefinitely, even when removals destroy its original tail.
Native state probing and OS join occur after the monitor guard is released.

Shutdown marks the monitor closed and detaches the entire catalogue under its
mutex. Future registration removal observes closed and does not touch links;
enqueue rejects closed. The detached owner can then clear links iteratively
outside the lock without competing access or recursive chain destruction.
Record retirement can still destroy native metadata during GC; moving that
remaining cleanup off-pause is separate unfinished work.

Native admission allocates the node while BLOCKED. Reference tests check zero
allocator traffic in prepared operations, weak ownership, cursor mutation,
duplicate/repeated operations and bounded teardown. Isolated Miri references
check the actual link code with native joining stubbed; runtime ThreadSanitizer
coverage checks the real concurrent worker and GC paths. These checks support
the protocol above and do not replace its ownership/exclusion requirements.

This is the working contract and audit ledger for roadmap item 1. It does not
certify the entire implementation as memory-safe. Requirements and completed
checks are distinguished below; an unreviewed path remains open even when an
application test passes. The language deliberately has no Rust references.

## Variable-length runtime allocation fast path

Heap validates header/alignment/interior-pointer metadata when installing or
extending its immutable canonical type table. Allocation requests with canonical
identity reuse that validation; noncanonical descriptors still require structural
equality before binding to the canonical entry. Length multiplication, padding,
alignment arithmetic and the isize pointer-offset limit remain checked per
request. The standalone/public descriptor size check still validates the full
metadata. The internal geometry-only helper does not grant permission to allocate
or scan an unvalidated descriptor.

`ai_gc_alloc_varlen` first tries a validated ObjectRequest in the owning mutator's
local buffer, limited to 8192 bytes. Success cannot collect or enter a managed
safepoint and initializes zeroed storage, header and native count word before
return. The compiled frame is published before the collecting fallback on stress,
large requests or exhausted allocation. Stress still collects once per allocation;
large/direct-tenure fallback retains its owner-card protocol. Allocation-site
counts/bytes are updated once for either path. Caller root spilling and native
entry signatures remain unchanged; this matches the existing fixed-size fast
path's publication discipline. These guarantees do not extend to arbitrary
foreign callbacks or an asynchronous embedding lifetime.

## Concurrent SATB retention bound

Before enabling concurrent-copy barriers, Heap configures the global SATB queue
for the source arena and pointer policy. A bitmap retains at most one encoded
reference per aligned source address, independent of overwrite count and tag
variants. Non-source/null/immediate values need no global retention; nursery and
destination references remain covered by the concurrent collector's existing
root/nursery/final destination scans. Seen bits survive draining and reset for
the next cycle. A logged referent stays alive for this cycle and may become
floating garbage, reclaimed in a later collection.

Retained values pack into 256-value chunks; sparse publications cannot retain a
full local-buffer allocation per unique object. There are at most
`ceil(source_bytes / 8)` unique values, plus rounding for the final chunk and
chunk-vector allocation. Bitmap storage is approximately source capacity / 64
bytes, with Vec minimum capacity/rounding, allocated on first concurrent use and
reused until Heap destruction. Each mutator still owns its bounded 256-value
local buffer; publication uses fixed 256-address stack scratch (2 KiB on the
validated 64-bit targets). Local buffer replacement is transient native storage.
Pointer-policy decoding occurs outside the queue mutex; callbacks must support
concurrent calls without managed safepoints or managed reentry. Queue guards do
not escape into tracing, and publication does not acquire object-access stripes
or enter a managed safepoint.

`heap.satb_queue.stats()` reports pending/peak/unique values, source slots, bitmap
storage, pending chunk capacity and chunk metadata. These describe the current
or last bounded cycle, not lifetime GC totals or a process RSS bound. Standalone
`SATBQueue::new()` retains its unfiltered chunk-transfer API until Heap enables a
cycle. Producer count, native thread stacks, total metadata, hard admission and
application throughput/pause budgets remain separate open obligations.

## Runtime layout metadata lifetime

`TypeInfo` owns dynamically generated interior-pointer offsets through
`Cow<'static, [u16]>`; static layouts may borrow constant offsets. Both the JIT
layout bridge and native metadata decoder transfer ownership into the type table.
Native decoding moves its vectors rather than retaining duplicate offset buffers.
Offsets remain valid while the heap's canonical descriptors exist and are freed
when those descriptors retire. Repeated embedded executions must not accumulate
per-execution offset allocations. Collector and allocation paths borrow entries;
no descriptor cloning is required per object or scan.

The Rust embedding API's descriptor is Clone rather than Copy. Explicit cloning
of owned offsets creates an independent buffer. `with_interior_ptrs` borrows a
static slice at runtime; const static layouts use
`TypeInfo::for_header_with_interior_ptrs(...).with_raw_bytes(...)`. This changes
the Rust descriptor representation/API; serialized `AotLayout`, metadata encoding
and the exported native entry signature do not change. The allocation-lifetime
regression observes actual frees across repeated JIT-style and native entries,
along with clone independence and equivalence to static descriptors. This does
not settle all metadata/resource accounting or application RSS budgets.

## Values, aliases and evaluation

Managed structs, reference enums, arrays and collections carry object identity.
Assignment and argument passing copy their managed references, not the objects.
Mutations through a permitted access path are visible through aliases. `mut`
controls the path used to mutate; it does not establish exclusive ownership or
deep immutability. `clone` has type-specific behavior; collection clones copy
storage but do not promise a deep copy of referenced elements.

Inline value aggregates copy their representation, including any managed
references embedded within it. Those embedded references retain identity.
Closures capture the values present at creation: later rebinding an outer local
does not rebind the captured local, while referenced-object mutation is shared.

Eager binary operands and positional call arguments evaluate left to right,
preserving earlier values across later side effects and collection. Named record
and named enum-variant construction evaluate initializers in written order into
rooted temporaries, then assemble fields in declaration order. Duplicate, unknown and missing fields
are diagnosed before construction. The full operand matrix remains under audit.
Short-circuit boolean operands stay conditional. Rooting/ANF regressions cover
selected cases; a complete expression-path audit remains open. Equality for
user-defined types dispatches through Eq; it must not be assumed to test identity
or imply deep equality. Atomic reference CAS uses identity. A complete equality
matrix, including closures and aggregate atomics, remains open.

## Casts and arithmetic

Integer addition, subtraction and multiplication wrap at the operand width.
Division/remainder and shifts follow [overflow.md](overflow.md).

The following cast rules are implemented:

- Integer narrowing retains low bits; widening sign-extends a signed source and
  zero-extends an unsigned source. Same-width signedness changes retain bits.
- Float-to-integer casts truncate toward zero and saturate to the destination
  bounds. NaN becomes zero; infinities saturate; negative-to-unsigned becomes
  zero. Both f32/f64 and signed/unsigned 8/16/32/64-bit destinations are covered.
- Integer literals must fit their inferred/annotated/suffixed type. Negated
  signed literals admit the signed minimum. Explicit narrowing casts retain
  their wrapping behavior. Floating literals must be finite at their declared
  width; overflow via arithmetic or explicit float-width conversion yields IEEE
  infinity. Incompatible numeric suffixes are rejected by the lexer.
- Float arithmetic/comparisons follow IEEE behavior without fast-math: NaN is
  unequal to every value including itself, ordered comparisons involving NaN
  are false, infinities order normally, and positive/negative zero compare equal.
  FloatBits and atomic CAS observe the representation bits. Arithmetic NaN
  payloads are unspecified; conversion/rounding/platform coverage remains open.
- Integer-to-float and float-width conversions use LLVM numeric conversions.
- Bool and char can convert to integer types. Bool identity casts are accepted;
  numeric-to-bool, bool/char-to-float and float-to-char are rejected.
- Integer-to-char validates the original integer before narrowing. Only Unicode
  scalar values (0 through 0x10FFFF, excluding 0xD800 through 0xDFFF) are valid.
  Invalid input prints a diagnostic and aborts. Char identity casts are accepted.
- Casts involving RawPtr, managed references or inline aggregates are rejected.
  They are not representation-punning or pointer-manufacturing operations.

Codegen uses LLVM saturating conversion intrinsics. Plain fptosi/fptoui produce
poison on exceptional inputs; runtime checks alone cannot establish correctness,
since optimization may eliminate checks derived from poison. Arithmetic tests
therefore verify emitted IR as well as optimized JIT/native results.

General size/offset arithmetic, literal ranges and all integer-cast combinations
still need a complete boundary matrix beyond the verified literal endpoints.
These repairs do not close the layout/size audit. Unary and compound operators
cannot manufacture invalid chars; bitwise/shift operations require integers.

## Initialization and sharing

Deferred local declarations remain supported. Their storage is zero-filled for
GC, but that is not a readable language value. A typed-IR definite-assignment
pass requires assignment on every reachable path before reads, compound writes
or closure capture. Branches merge reachable paths, loops merge break exits and
short-circuit right operands cannot initialize their skipped path. Closure
checking has an independent lexical context even when inference fails/retries.
Break/continue require the current function's loop; loops with breaks yield unit.
Value-carrying break is unsupported and receives a diagnostic.

Scalar array slots start at zero/false. Reference/value slots require explicit
initialization; checked reads of unwritten slots abort. Array/Vec indexing rejects
out-of-bounds access. The legacy array_get_unchecked spelling currently receives
backend bounds and initialization checks; its name does not establish an unsafe
capability. Every internal caller still needs review.

The target is sequentially consistent managed scalar/reference access and
indivisible value-aggregate snapshots. Multiple accesses are not a transaction.
Ordinary Vec/HashMap require external synchronization when shared storage mutates,
including backing-array aliases. Unsynchronized use may lose logical invariants
or trigger defined bounds/initialization failure, but must not produce native
memory corruption, fabricated managed references or torn aggregates.

See [concurrency.md](concurrency.md) for the proposed model and implementation
ledger. Existing SC lowering and sanitizer checkpoints cover selected operations;
they do not complete the arbitrary-sharing audit. Transitional Sync restrictions
remain until that audit passes. Constructor publication, safepoint coverage and
all optimized allocation paths must preserve initialized traced representations.

## Foreign and privileged operations

RawPtr is untraced native storage, not a managed reference. Foreign calls,
ptr_read_i64, raw thread/channel/atomic primitives and Atom representation
primitives require `unsafe { ... }`. Reading/writing RawPtr record fields or
constructing records with RawPtr fields also requires that capability, including
through destructuring and generic specialization. This protects transparent
native wrapper representations from safe-code handle forgery.

The capability is lexical: it does not flow into separately checked functions
or closures, nor erase an extern function's calling obligations through a safe
function-value conversion. Named functions genuinely shadowing native primitive
names remain ordinary functions. Typed library wrappers contain explicit unsafe
blocks that discharge handle provenance and publication obligations.

Unsafe callers must supply live correctly typed native handles, valid aligned
foreign memory, accurate buffer lengths and the declared C ABI. Temporary
as_c_bytes storage and managed callback addresses must not be retained after
their call/execution lifetime. Managed callbacks require the calling execution's
registered mutator thread; foreign-thread callbacks are not supported. Direct C
callback targets retain their C ABI; managed targets use a transition trampoline.
Aggregate callbacks are diagnosed as unsupported before LLVM emission.

Passing the boundary tests does not certify all unsafe implementations. Callback
retirement/unwinding and the remaining native caller/ABI audit remain open; see
[ffi.md](ffi.md). Owned retained callbacks are a future resource API.

Initial source inventory for the next boundary pass:

| Entry points | Source | Obligation still requiring enforcement |
| --- | --- | --- |
| ptr_read_i64 | src/lower.rs; src/codegen.rs | Foreign address must be live, aligned and readable |
| atomic_i64_new/bind_owner/load/store/fetch_add/cas | src/lower.rs; runtime.rs | Atomic handle kind, owner execution and lifetime |
| thread_spawn/join | src/lower.rs; runtime.rs | Completion handle kind/lifetime and code/environment ownership |
| chan_new/send/recv/close/sender operations | src/lower.rs; runtime.rs | Channel handle kind/lifetime and matching message type |
| extern calls and callback trampolines | src/lower.rs; src/codegen.rs | Foreign safety, retained callbacks, reentry and ABI |
| as_c_bytes | src/lower.rs; src/codegen.rs | Temporary buffer cannot be retained past its call |
| host_io | src/lower.rs; runtime/io.rs | Maintain validated response layout and checked host boundary |
| frame/root registration and mutation | gc/roots.rs; gc/thread.rs | Valid traced references, owner thread and registered storage lifetime |

Runtime paths in this table are under crates/gcrust-rt/src. This inventory is
initial, not exhaustive. Rejecting integer-to-RawPtr casts does not distinguish
an atomic handle from a thread or channel handle: each still has the same source
type. Direct access/construction is now privileged; unsafe implementations must
still establish provenance, rather than infer handle kind from RawPtr alone.

## Strings and blocking host operations

Strings are UTF-8 text. Byte-indexed ranges require character-boundary endpoints;
partial code points abort with a diagnostic. The legacy substring primitive
clamps both endpoints before ordering them, including extreme signed indices.
Endpoint checks include empty ranges inside a code point. Range copying checks
only endpoints and the selected UTF-8 output, with work proportional to copied
bytes rather than the surrounding input. The selected bytes become an owned
native Rust String before allocating managed output, so relocation/poisoning
cannot invalidate the source copy. No unchecked Rust text is constructed.

Strings are immutable valid UTF-8 after publication. Ordinary internal text
construction accepts `&str`. Concat/join use a private native-buffer constructor
with an explicit valid-UTF-8 precondition: concatenating complete valid String
sequences and separators preserves UTF-8, so these already-established outputs
need no rescan. Their buffers are fully copied out before any managed allocation.
No arbitrary unvalidated byte ingress uses that path; foreign raw String pointers
must meet the existing valid-object/UTF-8 contract. Text I/O and byte decoding
validate ingress rather than revalidating an entire source for each small range.
The safe range-copy helper validates its copied result even for malformed input
bytes; it does not validate unrelated source bytes beyond endpoint checks.
Legacy read_file returns empty on I/O/UTF-8 decoding failure; checked text I/O
preserves the error. All generated runtime symbol lookups now have JIT mappings,
including read_file, which previously crashed on execution in JIT.

File reads and output primitives copy their managed inputs to host-owned storage
before entering BLOCKED state. Published roots remain visible while pipe/file I/O
blocks; native return waits for collection and codegen reloads moved roots. A
bounded output-pipe regression observes output before admitting the worker, leaves
the pipe undrained while that worker collects, then checks the complete output.
Legacy print primitives ignore output errors and return zero; checked stdout_line
reports them. The rest of the host-operation/safepoint inventory remains open.

## Rust root-storage boundary

Borrowed RootFrame/DynRootFrame registration callbacks require unsafe because
their raw slots/header can otherwise be changed through safe Rust operations.
Lifetime cleanup remains automatic, including unwind. RootScope creates zeroed
stable storage safely; inserting raw bits or changing a Rooted value requires
unsafe, with valid tracing policy, heap lifetime and owning-mutator obligations.
AtomicRootSet raw insertion/replacement has the same obligations.

AtomicRootSet visitation now holds the backing lock through the collector's plain
word reads/writes. Registered mutators pause for GC, while unregistered Rust
observers synchronize through that same lock. Scanning cannot race safe get or
vector growth. Raw registration, insertion and mutation have compile-fail fences.
The broader embedding execution/TLS/metadata API audit remains open.

## Reflection and zero-size values

Reflection scalar/reference reads use matching-width sequentially consistent
atomics, including float representation bits. `field_i64` rejects inline
aggregates (kind 5) with a diagnostic instead of reading their first eight bytes.
This includes aggregates smaller than a word and aggregates with no payload.
Unit fields have no storage and are omitted from the reflectable field list;
reflection indexes address that list, not the original declaration indexes.
Reference bits are only an observation, not a rooted reference or a stable ID.

Heap unit fields have an explicit no-storage location. Inline field access maps
source indexes to LLVM's stored fields, which omit unit. Assignment expressions
return unit without manufacturing a scalar result. Unit arrays preserve logical
length, operand evaluation and bounds checks without reading/writing element
payloads; in-range optional reads return Some(()). Their byte tails reserve one
byte per logical element for length bookkeeping. Zero-size user value aggregates
remain boxed array elements with the ordinary explicit-initialization rule.

Native reflection encoding checks every length/count against the wire format and
returns an error instead of asserting or narrowing. Names are limited to 65,535
UTF-8 bytes; field/variant/interior lists to 65,535 entries; table counts use u32.
Unrepresentable metadata produces a build error and preserves existing output.
The valid encoding and native runtime ABI are unchanged.

## Snapshot coordination

Language snapshots publish roots and enter BLOCKED before acquiring the GC lock,
then keep the caller blocked through host output. A collector already waiting for
the caller can finish. Generic Rust world pauses also suspend every RUNNING
registration on the calling OS thread before lock acquisition, preserving outer
BLOCKED registrations. Guard cleanup restores those registrations while holding
the GC lock, avoiding a collection between restoring two current registrations.
JIT embedders must publish their frame chains before such a safepoint operation.

JSON snapshots copy rendered fields, graph edges and roots under a world pause.
Graph analysis and JSON serialization then use pointer-free owned graph data
outside that pause. `snapshot_pause_ns` covers acquiring the pause and capture,
including copying render data, instead of omitting the final field-render phase.
Transient-root completeness is reported in `summary.reachability_complete`, with
no I/O inside the heap walker. Snapshot output is best-effort; a failed output
write does not panic across the native ABI.

Regressions cover a collector already holding the GC lock, direct Rust profiling
with two current registrations, an undrained multi-megabyte snapshot pipe while a
worker collects, post-snapshot root reloads, output descriptor failure and the
transient-root completeness flag.

## Audit ledger

| Area | Existing foundation / this checkpoint | Remaining acceptance |
| --- | --- | --- |
| Numeric casts | Saturating casts, literal endpoints/suffixes, char validity and IEEE exceptional comparisons | Complete integer conversion matrix, size/layout arithmetic and rounding coverage |
| Operand evaluation | ANF/root reload, definite assignment, written-order record/variant construction | Full expression/foreign conversion matrix |
| Heap allocation | Validated descriptors/headers, TLAB initialized ranges, closure fast allocation | All constructor/escape/optimized publication paths |
| Mutable fields/arrays | SC scalar/reference operations, guarded aggregate snapshots | Complete access inventory, adversarial races and barriers |
| Collections | Bounds/unwritten-slot checks; documented external synchronization | Every metadata/capacity race and backing-array alias path |
| Closures/values/enums | Embedded-reference tracing and payload regressions | Capture/identity/copy matrix and every generic representation |
| Threads/channels/atomics | Root-owned handles, repeatable joins, traced queues and lexical representation capability | Arbitrary capture/sharing audit and verification before removing Sync gates |
| FFI | Explicit unsafe calls, correct C targets, scalar managed trampolines and buffer copy-back | Complete supported ABI/callback caller audit, lifetime/affinity/failure contract |
| Rust embedding | Raw-root contracts, serialized root scanning, explicit native execution contract and scoped TLS restoration | Metadata APIs and complete owner/lifetime inventory |
| Strings/host operations | UTF-8 ranges, text-only reads, complete used-symbol mapping and GC-aware output/read blocking | Remaining host-operation and allocation-boundary inventory |
| Diagnostics/heap tooling | SC reflected slots, aggregate-read diagnostics, checked metadata encoding, snapshots/safepoints | Complete concurrent inspection inventory and split inline-enum projection paths |

Each audit closure must record exact source paths, invariants, a distinguishing
regression, debug/release JIT/AOT coverage, GC stress where relevant, and sanitizer
scope where applicable. Do not remove Sync gates or declare blanket safety based
on the cast checkpoint.

## Nominal identity, layouts and entry points

Nominal layout caches use semantic types, including qualified names and concrete
arguments. Generic function caches use source identity, substitution and receiver
type. Display spellings do not determine either identity. Array element boxes
use value-layout IDs. Qualified user Array records are ordinary records; only
the built-in Array receives variable-length runtime operations. Declared Vec,
Option and Result layouts follow their declarations.

Managed LLVM functions receive distinct symbols if their display names collide
with another function, a C target or a runtime function. Execution resolves the
actual emitted entry symbol. The program entry is the root-level, non-generic
`fn main() -> i64`, with no arguments or foreign declaration.

Reference-field offsets, embedded reference offsets, closure capture sizes and
value aggregate size arithmetic are checked. Unrepresentable layouts produce a
compiler diagnostic before emission. Inline layout construction rejects cyclic
storage and permits nested generic wrappers that consume a smaller type argument.
Reference/array descriptors are reserved, then completed after inline sizes are
known; recursive reference/value types and boxed recursive array elements remain
supported. Failed registration restores its descriptor and worklist checkpoint.

Qualified names cannot fall back to an unrelated last segment. Unqualified type
lookup requires one candidate or an explicit import. Type IDs and reflected field
offsets are checked against their runtime representations. Expanding reference
specializations and the complete limit/metadata inventory still require audit.

Rust JIT execution functions are unsafe: the caller must supply valid typed
Core and uphold contracts of any source-level unsafe operations or foreign
functions. Native execution cannot be presented as a safe Rust operation on
arbitrary, publicly constructible Core. Internal CLI/tests invoke these APIs
explicitly. Current-thread TLS is bound through a synchronous scope that restores
its previous value on return/unwinding. No public unscoped setter remains. Nested
bindings suspend a different outer mutator, publish its roots and allow its
heap to collect. Existing BLOCKED scopes retain their state. Regressions cover
actual outer-heap collection during inner execution, normal/unwinding restoration,
same-mutator reentry, an already-blocked outer scope and OS-thread isolation.
Retained callbacks and
foreign-thread managed callbacks remain outside the supported contract.

## Compound assignment locations

Lowering captures a compound field receiver, or an indexed receiver and index,
into typed Core locals before reading the old value. The old indexed value is
also captured before evaluating the RHS. These locals participate in ordinary
root placement and reload across GC. Trait calls accept typed Core operands
through the same method inference/checking path as source operands; lowering
never synthesizes user-visible variable names for places. The final store uses
the captured location, including when the RHS changes an array element through
which a field was reached. `tests/compound_places.rs` covers builtin arrays,
Vec, custom Index/IndexSet ordering, operator traits, RHS mutation and moving GC
in JIT and native execution.

## Terminal diagnostics and observability output

Language panic, invalid reflection indexes/kinds, arithmetic, array bounds and
initialization, allocation overflow/OOM, channel representation failures and
invalid string-range diagnostics use owned host text before parking for stderr.
They abort inside the BLOCKED region, so no managed access resumes after output.
Output errors are ignored on these fatal paths; they cannot invoke a Rust panic
through formatting macros across the C ABI. Runtime failure entry points that
accept a mutator pointer are unsafe Rust APIs with owning-thread preconditions.
Valid array-length validation still returns without a safepoint.

JIT/native heap-dump, GC-stat and allocation-profile stderr output also uses a
GC-aware error-tolerant writer. Failure to write optional observability output
does not replace the program's result. File-dump failure diagnostics follow the
same rule. This does not establish recovery from Rust allocation failure,
poisoned runtime locks, malformed privileged metadata or arbitrary foreign panic.

`tests/diagnostic_progress.rs` holds a multi-megabyte panic in an undrained pipe
until a worker completes allocation and collection, then checks the complete
message and expected abort. It also checks failed teardown stderr/file output in
JIT/native execution. Reflection index regressions reject negative/past-end
indexes without Rust unwinding. The runtime diagnostic subprocess installs a
panic hook and verifies that EBADF output failure aborts without invoking it.

## Registration during collection

Registering a mutator on an OS thread that already owns a RUNNING registration
is a safepoint. Registration suspends all of that OS thread's RUNNING states in
the same heap before waiting for the collector lock. Once the collection window
closes, it registers the new state and restores previous states while retaining
the collector lock, preventing a new collector from waiting on a partially
restored set. Already-BLOCKED states retain their surrounding scopes. World-pause
acquisition uses the same suspension helper. JIT embedders publish frame chains
before these operations and reload derived pointers afterward.

The snapshot coordination subprocess also forces a collector to own gc_lock and
wait for the caller before that caller registers an additional state. It checks
that registration finishes, collection occurs, and all current states resume.

## Retirement and mutator collection with sibling registrations

Retirement and mutator-triggered major/minor collections suspend every RUNNING
registration owned by the calling OS thread before waiting for gc_lock. A guard
restores those states while still holding the collector lock, including early
returns when a prior collection serviced the request. Already-BLOCKED states
retain their surrounding scopes. Retirement detaches poll storage and folds
allocation counters only after the active collection census has finished.

Collection requests compare actual counters for the requested collection kind.
Waiting for a world pause or another non-collection lock holder does not satisfy
a collection request. Non-stress requests can reuse a completed collection of
the requested kind; GC stress still performs a collection for each request.
JIT frame publication/reload requirements apply to every same-thread state.

The bounded retirement subprocess forces a collector already waiting for two
registrations on the caller's OS thread. A separate 12-case subprocess matrix
covers major/minor, normal/stress, immediate collection, existing collector and
non-collecting world-pause contention. A root registered only in a sibling's
Rust frame chain survives relocation; a third pre-blocked state stays blocked.

## Legacy embedding root tokens

The standalone Mutator root container validates instance provenance and slot
identity on every get/set. Root handles remain Copy but cannot alias a newly
allocated slot after restore, or a slot in another Mutator. Scope tokens identify
a surviving prefix: restoration is reusable while that prefix remains, but a
removed/replaced boundary or another instance's token is rejected before any
mutation. Moving Mutator preserves identity; GC slot-value updates preserve
handle validity. Instance/generation counters never wrap into reused identities.
These checks run in optimized builds too.

Token regressions cover stale/reused and cross-instance handles, stale scopes,
reusable surviving scopes, moves, collector updates and generation exhaustion.
The standalone container's safepoint is still a no-op; registered execution uses
ThreadState/FrameChain. This repair does not declare the legacy container a
complete production execution or ownership API.

## Per-call collection roots

Mutator-triggered collection publishes nonempty extra-root sources in a private
scoped registry before parking or waiting for the collector lock. Major/minor
collectors and world-pause root enumeration scan those sources too: a competing
collector can relocate a root even when the requesting collection returns early
after that collector finishes. Registry entries borrow the source slice binding
and its sources; the private guard removes its entry under the registry lock
before that borrow expires. Root slots remain valid until all collector work
has finished and the owner has resumed. No scoped registration is allocated for
empty lists. Heap-lifetime sources still require permanent registration.

Callers must own a registered RUNNING mutator in the same heap, keep extra-root
storage alive and quiescent while parked, and use the heap's pointer policy.
Registration ends on return; subsequent collections need their own registration.
The low-level collect API still requires exclusive quiescent access and does not
establish general concurrent embedding ownership. Snapshot completeness flags
remain conservative for histories involving per-call roots.

The ten-case subprocess matrix covers own collection, competing major/minor and
concurrent collection, world-pause enumeration, normal/stress execution, relocation and
payload preservation, guard removal on early/normal return and collection after
the source is destroyed. Restoring the old per-call-only visibility makes the
competing-major regression fail; restoring scoped publication passes.

## Shared root inventory and concurrent pauses

Stop-the-world collection, both concurrent-collector pauses and snapshots now use
one registered-root enumerator: persistent globals/native queues, registered
host frames, parked JIT frames, scoped sources and permanent sources. The
concurrent path previously omitted JIT/permanent sources. Both pauses request
poll flags before waiting; allocation-free mutators need not allocate to stop.
The temporary-root matrix also covers a competing concurrent collector.
Permanent/JIT-only root preservation and an allocation-free two-pause worker have
bounded regressions. Concurrent mutation/copy/barrier correctness remains open;
these tests establish root coverage and pause coordination only.

## Validated allocation requests

Heap-bound requests validate caller metadata against the immutable canonical type
table and check length/size once. Typed object requests also establish the physical
header match. Private fields prevent bypassing construction. Arena identity is
resolved at each allocation, including reuse after a collection; a request never
caches a TLAB reservation or from-space address. Small requests use the validated
sized TLAB path; large/stress requests retain shared allocator checks. Public
allocation paths still reject invalid input before triggering stress collection.
Tests cover descriptor/header/overflow rejection, space flips, zeroing, header and
count initialization and large allocation. Measured evidence and remaining
performance work are in [GC_PERFORMANCE.md](GC_PERFORMANCE.md).

## Read barrier encoding and publication

Both read barriers use the collector's shared FORWARDING_BIT (bit 63), not the
obsolete bit-0 format that conflicts with odd type IDs. The atomic barrier waits
through a marker-only copy claim and reloads with Acquire until a destination is
published or the original header is restored. Non-atomic reads require a
quiescent writer; the supplied offset must locate an aligned eight-byte forwarding
word. Source storage remains alive through resolution.

Tests use odd IDs, actual Compact/Full heap relocation, both old/new object
addresses, null, deterministic claim/publication/rollback sequences and bounded
cross-thread payload publication. Restoring bit-0 decoding makes the odd-ID test
fail. These repairs do not close concurrent mutation/replication correctness.

Heap forwarding uses `ObjHeader::HEAP_FORWARDING_OFFSET`, independently of
`TYPE_ID_OFFSET`. The default is the first eight-byte word; Full explicitly
retains offset eight and Compact retains offset zero. HeaderLayout validates an
aligned, wholly in-header forwarding word. When the ID overlaps that word,
collectors decode it from captured native-order bytes; otherwise they read the
immutable separate ID. Parallel copying excludes the claimed word from memcpy
and restores its original value into the destination, preserving all metadata.
Legacy SemiSpace continues to reserve the first word. Custom headers must keep
bit 63 clear in both reserved words and initialize every byte read by collectors.
Built-in Compact and Full headers store type IDs in the numeric low 16 bits of
an eight-byte word. Their native u16 slots are at offsets 0/8 on little-endian
and 6/14 on big-endian targets. A leading u16 field on big-endian targets would
put ID 32768's top bit on the forwarding marker. Full's first word retains the
enum tag in its numeric low 32 bits, at byte offset 0 or 4 respectively.
Generated inline initialization stores two numeric i64 words to match Full's
runtime representation. Little-endian object bytes/sizes are unchanged; Compact's
Rust alignment is now eight on the validated 64-bit targets, matching the
existing managed-allocation minimum.

Bounded custom-header tests cover IDs at offsets two and ten, forwarding at zero
and eight, complete metadata preservation, shared cycles, exact live size, minor
promotion then major moves, and concurrent collections without mutator writes.
Collection subprocesses exercise 1/2/4/8 workers. These tests do not close the
separate concurrent mutation protocol audit.

## Collection failure policy

A moving collection does not have a complete relocation rollback log. A panic
that escapes its collection work is therefore fatal: the collection boundary
aborts before unwinding any mutator-restoration guard. This covers direct Heap
collection, dedicated STW/concurrent cycles (including both concurrent pauses),
mutator major/minor requests and scoped-extra requests, and legacy SemiSpace.
The boundary runs inside the collection lock and outside relocation work; for
mutator requests, the registration-restoration guard stays outside the catch.

Evacuation jobs also contain panics locally, before the reusable helper pool
can retain or drop their arbitrary payloads. Failure aborts the entire process;
no failed borrowed job can return to a coordinator or mutator. The boundary performs no secondary
formatting/locking and never drops an arbitrary panic payload. Existing Rust
panic hooks execute before unwinding reaches the boundary; the policy does not
replace caller-installed hooks. A successful collection invoked during an
unrelated caller unwind preserves that caller's panic. Construction/descriptor
validation before collection retains its existing rejection/panic semantics.

Bounded subprocess tests verify SIGABRT and the injected diagnostic, rather
than accepting any nonzero exit: all public entry points, first/final concurrent
root passes, 1/4 workers, real helper-thread failures (including simultaneous payloads whose destructors
must not run), promotion exhaustion and
a panic payload whose destructor must not run. The existing precise-layout
detector test now verifies its diagnostic plus fatal termination in a subprocess.
Removing the boundary reproduces the mutator-restoration hang within two seconds:
`/tmp/gcr-failure-boundary-negative.log`. Prior stack evidence remains at
`/tmp/gcr-custom-header-negative-sample.txt`.

This fixes collection panic escape/restoration; it does not establish rollback,
recovery from OOM, arbitrary panic-hook progress or the remaining allocation and
embedding failure inventory in GC_READINESS.md.

An additional regression exposed the reusable pool dropping an earlier helper
panic payload when another helper failed. Three synchronized helper panics with
destructors that exit 88 demonstrated the error before the local job boundary:
`/tmp/gcr-worker-payload-negative.log`. Collection jobs now abort locally, leaving
the general worker pool's separately tested reusable-job semantics intact.

## Adaptive parallel work and bounded large scans

Queue batches leave scarce work for other configured participants and take up to
128 tasks from a wide frontier. Publishing work during an object scan does not
decrement its active-producer count. Quiescence still requires an empty shared
queue and no active producers; each batch completes only after its claims have
been published and its discovered tasks have been transferred.

Per-worker task buffers hold at most 128 items. Small fixed scans have at most
four traced slots per task, so a complete task batch produces at most 512 claims
and retry/discovery items combined. Larger fixed scans and variable value arrays
publish before/after scanning and flush during traversal at a combined 256-item
threshold. Their publication retains the active producer while other workers
consume the frontier. This keeps each local claim, discovery, layout and address
buffer within 512 entries even for arbitrarily wide valid objects. Debug checks
assert those bounds. Shared queues and root inventories still scale with the
live graph; this is not a bound on the collector's total RSS.

Separate scan specializations remove streaming checks from small fixed-field
callbacks. Functional tests cover publication before producer completion,
chain/tree/star work graphs, and 65,536-element value arrays referencing 8,192
shared back-linked children. The heap tests verify actual relocation, aliases,
payloads and exact live sizes through major collections and minor promotion
followed by major collections, with 1/2/4/8 workers in bounded subprocesses.
Native-byte extraction tests also cover overlapping captured words and separate
ID slots across every aligned u16 offset of a two-word header.

## Scoped accesses during concurrent relocation

Concurrent copying now reserves a marker-only claim under the source stripe
before destination allocation. Only its owner allocates; contenders wait without
retaining a stripe. The owner acquires source/destination stripes in sorted,
deduplicated order, copies/restores the captured header word, then publishes a
forwarding destination with Release. A failure restores the source word before
the enclosing fatal collection boundary terminates the process.

`ObjectAccess` retains exclusion for the complete memory operation. Unforwarded
aliases retain their source stripe; forwarded aliases acquire both source and
destination in sorted order; direct destination aliases acquire its stripe.
All forms therefore exclude writes/copying of canonical storage. Header claims
are retried only after releasing the acquired stripe. No identity side table or
extra object-header bits are needed. Concurrent pointer discovery locks its
canonical parent only for the load and releases it before claiming a child,
avoiding nested-lock cycles from graph edges and stripe collisions.

The guard may not cross a safepoint, managed allocation, managed blocking call,
managed callback or nested object-access scope. Physical length/type-ID and
forwarding invariants remain valid; other header payload follows the scope.
Raw derived addresses/references may not be used after guard drop; base aliases
still obey normal root/reload rules at later safepoints. The MutatorThread helper
borrows the owner exclusively while the scope is live; its compile-fail example
rejects a safepoint through that owner. Low-level Heap access remains unsafe and
requires caller-established registration/liveness. Language atomic/aggregate and
reference/SATB/card synchronization remain necessary, including when GC is idle
and the guard takes its fast path without locking.

The stripe mutexes contain no poisonable Rust state. Host access must preserve
valid heap/traceable-slot invariants even on unwind; mutex poison alone does not
prevent subsequent exclusion. Internal copying failures still use fatal collection
boundaries. Atomic word copying is retained for compatibility with existing
eight-byte atomic accesses; differently sized scoped operations are excluded
from copying by the shared mutex protocol. Unguarded narrow accesses remain an
open compiler/runtime integration issue.

Bounded tests exercise an actual concurrent cycle with writes after the snapshot
and before forwarding, 8/16/32/64-bit payloads, Compact/Full/custom headers,
metadata preservation, both alias forms and exact live size. Contended copies
reserve exactly one destination. Opposite and duplicate stripe pairs terminate.
Returning the obsolete source alias fails the actual-cycle regression:
`/tmp/gcr-object-alias-negative.log`. These tests establish the guarded host path;
they do not certify uninstrumented generated/runtime mutation. Integration scope
is explicit in GC_READINESS.md, including managed-reference identity comparisons.

The actual-cycle test reloads registered roots after the first pause; the old
child alias is then read from the canonical parent's still-unrewritten field.
No such alias is used after the final pause. It also stores a nursery reference
into the forwarded parent, applies SATB/card protocols, removes its temporary
root after the major and checks that the next minor promotes it through the
remembered parent edge. Destination card marking selects the physical arena
containing the supplied address, not merely the current from-space index.
Below-base/out-of-range CardTable marks are ignored without integer underflow.
Negative evidence: `/tmp/gcr-card-destination-negative.log` and
`/tmp/gcr-card-range-negative.log`.

`Heap::reference_eq` compares live published managed bases, including old/new
aliases, without waiting on marker-only claims. Distinct published references
cannot alias a destination that has not yet been published. Publication/acquire
ordering must make a destination's source-forwarding entry visible before use.
Nulls are supported; raw/interior/function pointers are excluded. Reserved
forwarding words have atomic observers outside the stripe: any mutation of
other bits during copying must use matching-width AtomicU64 accesses within the
scope, while preserving type/forwarding invariants. Other header payload uses
ordinary guarded access. Generated managed-reference Atom CAS now uses this
primitive through the scoped host CAS. RawPtr equality retains raw-address semantics.

Replacing canonical resolution with the obsolete source, or replacing managed
identity with raw pointer equality, fails the rooted actual-cycle test. Logs:
`/tmp/gcr-object-alias-negative-final.log` and
`/tmp/gcr-object-identity-negative-final.log`.

The identity-aware untagged-reference CAS holds ObjectAccess across slot load,
identity comparison, SATB log, physical SeqCst CAS and successful nursery card
marking. It retries on physical contention, so an alias representation change
cannot produce a false logical mismatch. Conservative SATB entries on failed
physical attempts are permitted. A logical mismatch leaves both slot and SATB
buffer untouched. Returned bits may retain an old alias and follow the ordinary
root/reload rules. The operation cannot be nested inside another access scope.
Generated managed-reference Atom CAS now uses this host primitive through
`ai_atom_ref_cas`. Managed-reference loads use `ai_atom_ref_load`; both are
non-safepointing ABI operations. Scalar/RawPtr Atom accesses now use matching-width scoped runtime atomics too.
Their bit/address equality is distinct from managed-reference identity.

Scalar Atom runtime entry points specialize at 8/16/32/64 bits and hold canonical
access through the matching-width SeqCst load/CAS. No mixed-width scalar copying
race is permitted while the scope is held. Bool stores a byte; floats compare
bits including NaNs and signed zeros; RawPtr compares physical addresses and is
untraced. These operations never safepoint or acquire a second object guard.
Successful scalar replacement needs no SATB/card barrier because its slot is
untraced. This does not cover direct ordinary field access to Atom storage or
other general field/array operations, which remain part of the integration audit.

Ordinary mutable scalar/reference generated fields and array slots now branch
on the heap-owned atomic copying phase while RUNNING. Before the next poll, an
idle access cannot overlap the first pause that starts copying. Its direct
SC/private operation and ordinary reference-card barrier remain valid. Active
access passes a base+offset into a complete canonical scope. Shared and private
active operations use matching-width SC atomics; traced stores use pre-CAS SATB
logging with retry and canonical physical-card marking. Private idle metadata
is not applied to runtime fallback accesses. Tracedness comes from layout,
including value-box array slots; RawPtr storage alone is not a tracedness test.

The phase pointer is appended to Thread at offset 80 and borrows stable heap
storage for the registration lifetime. Both root and spawned Thread initializers
install it; compile-time offset assertions cover it. This adds no safepoint or
managed blocking transition to a slot access. Aggregate operations and the
remaining immutable/publication/runtime access audit are separate obligations.

Reflection metadata selection now resolves canonical storage under ObjectAccess.
`field_i64` selects its descriptor and reads the value in one scope, using the
same matching-width SC scalar/reference operations as before. No managed interior
address escapes. Bounds/aggregate-kind errors release the guard before terminal
diagnostics can park. `field_name` allocates after guard release; its copied name
comes from immutable off-heap metadata. Published enum tags are immutable in the
language model, so separate field-count/kind/name calls need no transaction.
The scheduled actual-cycle regression and deliberately wrong metadata/payload
alias variants establish that canonical resolution is required for both paths.

Scalar native-buffer handoff now retains ObjectAccess while deriving and using
the managed data pointer. Calls are non-safepointing and RUNNING; object scope
must end before foreign entry, callbacks or managed blocking. Private native
storage is disjoint and may be unaligned; managed elements retain matching-width
SeqCst accesses and bool normalization. Scalar arrays are untraced and reference
arrays are excluded. Zero length touches no native storage. Concurrent copying
cannot separate the operation from canonical storage; ordinary mutators still
observe per-element SC operations rather than a whole-array transaction.

Host I/O response construction now polls only at an entry-loop boundary where
all live managed state is in scratch roots. The normal poll helper publishes
JIT frames; the entries array is reloaded after the following String allocation.
New entry bytes finish before the non-safepointing scoped reference store
publishes the pointer. The array may already be visible to the collector even
though the response has not returned to user code. It therefore requires the
same SC/canonical/SATB/card protocol as other published reference storage.
The final response is initialized from reloaded scratch roots without further
safepoints before publication. Typed allocation owns varlen-count initialization;
constructors no longer rewrite those immutable counts.

Physical type-ID reads now resolve through ObjectAccess during copying. The
idle generated load is bounded by the RUNNING/no-safepoint phase invariant;
nominal enum-ID mapping follows either path. Closure-code lookup retains one
canonical scope across the header, capture-count metadata and immutable code
pointer read. Returned code addresses still depend on the embedding's executable
code lifetime. Type-name allocation and channel diagnostics/queue blocking occur
after the type-ID helper has released its guard. Channel creation and message/
witness checks must inspect canonical headers even when payloads are immutable:
forwarding may overwrite the source's type word.

## Inspection unwinding and coordinator exclusion

`Heap::pause_world` permits user inspection to unwind. Its destructor restores
parked mutators and current-thread registrations before releasing the coordinator
lock. The data-free `Mutex<()>` used for coordinator exclusion deliberately does
not reject subsequent callers because of that unwind: registration, retirement,
inspection and collection acquire it through `lock_coordinator`.

This policy does not recover an interrupted collection. Collection/relocation
panics abort; pause setup and restoration failures also abort. Pause setup keeps
the coordinator guard and current-registration guard **outside** the fatal panic
boundary so a failure cannot release exclusion before aborting. Other runtime
mutexes retain their poisoning behavior. This distinction prevents a completed
inspection unwind from making retirement panic before poll-target detachment.

Regressions use a still-live external poll target to demonstrate stale writes on
the old path without touching freed storage. A bounded Full/Compact matrix
checks five collector entry points, live payloads, restored mutator state,
registration and retirement after repeated inspection unwinds. The ownership
repair below continues the native-child cleanup audit.

## Owning runtime registration and fatal cleanup

RuntimeThread owns a RuntimeRegistration guard and a pinned boxed ABI Thread.
Rust field-drop order places registration retirement before freeing that box.
The guard is armed immediately after registration, before further initialization;
main contexts and child setup share the constructor. Its owner-thread marker
prevents transfer, and it clears parked/scratch roots before safe deregistration.
The child env uses ScratchScope for both return and unwind paths.

A setup panic after poll installation previously skipped manual deregistration.
Join returned zero while the dead child remained in the registry. The regression
checks membership before attempting any GC, then proves env reclamation and an
empty registry after corrected cleanup. It never requests a poll through the
broken path's freed pointer.

Failed deregistration aborts inside coordinator exclusion, before caller guards
can restore through a partially modified or poisoned registry. Failed root-child
draining aborts from both Drop and explicit finish_threads, so callers cannot
unwind into unloading code still executing in children. Bounded fatal regressions
reject incidental destructor double-panics as well as successful/unwinding exits.
This does not add typed panic/cancellation outcomes or establish every admission,
foreign callback and nested-execution policy.

## Closure handoff before mutator admission

ThreadHandoff owns the heap and globally rooted closure env until transfer to the
child's active scratch scope. Before that point, its destructor clears the root
and returns the slot even if registration failed before RuntimeThread exists.
AtomicRootSet serializes that unregistered clear with collector visits. Vacancy
is advertised only after the clear. Successful transfer disarms the token before
publishing vacancy, so a later destructor cannot clear a reused slot.

The bounded test injects registry poison without changing membership, then checks
128 failed native admissions over semi-space/generational heaps. It restores only
the deliberately unchanged test registry; production retains the poison policy.
Every env is collected afterward, with one reusable slot and no dead child
registrations. Unbound raw completion records still have execution lifetime.
Failed handoff cleanup during the admission unwind is fatal; regressions reject
incidental double-panic aborts. Other admission/cancellation/foreign semantics
remain separate audits. The token adds one temporary heap Arc clone per spawn,
which bridges unregistered ownership to registered ownership; allocation paths
are unchanged and no throughput improvement is claimed.

## Native atomic cells and stable managed owners

Safe AtomicI64 is a reference wrapper with a RawPtr field, whose construction and
mutation require unsafe capability. Its factory weakly binds the native cell to
the wrapper identity. A replaceable inline witness cannot supply that lifetime;
the wrapper itself remains stable across aliases and relocation. Raw unbound
cells retain execution lifetime, and bound raw uses require owner liveness.

All four evacuation paths process weak native owners after copy work and before
source reset. They update only owners in the collected source and preserve
owners in uncollected generations. Native atomic values are scalar bits, never
GC roots. Cell destruction runs no user finalizer. Registry mutexes serialize
binding and sweeping; ordinary native SC operations use no registry lock and
never safepoint. Codegen and effect analysis share that nonrelocating contract;
wrapper construction remains allocating.

Native tests cover 20 header/pointer-policy/path cases, old/nursery liveness,
128 actual cell lifetimes, raw history, late binding during concurrent copying,
metadata bursts and retained-heap shutdown. Typed Core/LLVM tests require factory
binding to an AtomicI64 reference. JIT/AOT and sanitizer fixtures exercise aliases
and concurrent updates through moving GC. Sparse-table maintenance and wrapper
allocation add costs whose application throughput/latency/RSS budgets remain open.


## Big-endian built-in header verification

`scripts/check_big_endian_runtime.sh` builds a pinned public Rust image and
cross-compiles the standalone runtime crate for s390x Linux. QEMU executes the
actual big-endian binaries in debug/release at one/four GC workers, with verifier
and source poisoning enabled. It exhausts all 65,536 IDs for both built-in
headers and verifies high-ID shared cycles, tagged slots, metadata preservation,
minor/major/concurrent paths and standalone SemiSpace copying. It also executes
variable-length runtime initialization/stress/root/counter checks, the checked
geometry oracle and canonical-descriptor rejection checks. The current matrix
has 48 selected executions, including string-range Unicode/relocation checks. Source is mounted read-only; the script needs no
privileged container or global binfmt handler.

This selected runtime gate does not claim a supported s390x language platform,
a big-endian JIT/AOT compiler or FFI ABI, a complete cross-target runtime suite,
or physical-hardware performance. The selected tests avoid subprocess re-exec,
which would need additional emulator integration. Native host workspace and
sanitizer checks remain separate gates.

## Bounded string-copy checkpoint (2026-10-17)

Concat/join native staging now checks each result extent against the largest
managed arena before growing its buffer. This rejects impossible repeated-alias
outputs with a controlled diagnostic. It does not guarantee allocation under a
live-set constraint or impose an aggregate native-memory budget.

Copies poll in chunks of at most 32 KiB. Array/separator roots survive polls;
each selected array element is independently rooted before a possible park,
preserving the observed value if another writer replaces its slot. No managed
source slice spans a poll. Large native Vec growth runs in a blocking region
with only native storage borrowed. Large managed output copying roots its
private, initialized partial result and reloads it after each poll, acquiring
object access for each write. Partial UTF-8 sequences remain private until
construction finishes; the collector treats this payload as opaque bytes.

Focused verifier/poison regressions and four negative controls cover preparation
progress before managed allocation, moved source/output pointers, slot
replacement, and output extent arithmetic. Broad checkpoint validation passed (811 debug/optimized including detector,
52 s390x checks, sanitizer 38 native/170 runtime); evidence is in target/gc-validation/2026-10-17-bounded-string-copy/.
Managed allocation zeroing and OS allocation are not bounded by the copy chunk,
so this is no hard real-time guarantee. Other native loops, total producer
budgets, metadata/stacks, embedding and platform audits remain open.

## Polling equality/hash ABI

Current generated String scans split at the runtime-owned 32-KiB leaf bound.
Only the large branch can call the versioned collecting symbols; that branch
checks relocation and reloads caller roots. Runtime large scans root every
argument before the first poll and reload each chunk, preserving immutable
bytes across collection. Existing unversioned symbols retain noncollecting
behavior for already compiled callers; they cannot silently acquire safepoints.
The scan tests include pre-entry and mid-scan collections, unequal late bytes,
chunk edges, both heap modes, legacy effects, generated callers and missing
versioned symbols. Broad checkpoint validation passed: 819 debug/optimized
checks including detector; 56 s390x checks; sanitizer 40 native/175 runtime.

## Native String input ownership (verified checkpoint)

Substring, raw/newline printing, legacy read-file paths and checked host-I/O
inputs now use shared native-copy preparation. Checked substring endpoints are
computed before any poll; the selected range is copied in chunks of at most
32 KiB, pinning and reloading its immutable owner across native growth and
collection. No managed slice survives a poll. Small copies retain a leaf path.

Large UTF-8 validation runs BLOCKED after the copy is fully native-owned. A
deterministic test waits for the collector call to return inside that phase.
Malformed selected bytes remain an explicit substring error, malformed checked
host input reports its existing invalid-data outcome, and legacy read-file
failure still yields empty text. No unchecked Rust str is constructed.

Checked host calls root both path/text before either preparation can park and
reload the second root after the first preparation. Both roots drop once native
ownership is complete, before potentially long host work. These entry points
already permit collection in their ABI; Thread/startup layouts and legacy
noncollecting scan symbols are unchanged. This is not an aggregate allocation,
OS latency or whole-runtime pause guarantee. Broad gates passed: 825 debug and
optimized checks including detector; 60 s390x checks; sanitizer 40 native and
181 runtime executions without reports. All 385 input hashes match; measured
runtime code and 425 archived current files are verified.
