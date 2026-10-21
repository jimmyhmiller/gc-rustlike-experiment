# C interoperability

The parser accepts `extern "C"` declarations. Lowering checks their parameter
and return representations; managed reference types do not cross as ordinary
extern arguments. Codegen calls C symbols without the hidden managed Thread
parameter and emits native/managed transition calls.

Every foreign call requires an explicit `unsafe { ... }` block. Raw pointer
reads/native handle primitives and RawPtr record representation access require
the same capability; ordinary typed Thread/Channel/Atomic APIs discharge it
internally. Unsafe permission does not leak into closures or called functions.
See [SAFETY.md](SAFETY.md) for caller obligations and the audit ledger.

Inspected/covered features:

- Scalar arguments and returns.
- Blittable value-struct arguments by value; mutable value-struct parameters by
  pointer to stack storage. Aggregate coercion uses AAPCS64 on ARM64 and SysV
  INTEGER/SSE eightbyte classification on x86-64 Unix. SysV aggregates that do
  not fit the remaining argument registers pass as typed byval stack copies.
  Indirect non-mutable arguments use private snapshots: callee writes and later
  sibling-argument evaluation do not mutate an earlier by-value argument.
- Blittable aggregate returns up to 16 bytes; larger returns are rejected.
- `as_c_bytes` copies String/scalar-array contents to eight-byte-aligned native heap storage for a direct
  extern argument. Mutable buffer parameters support copy-back.
- Named callback functions use synthesized C-ABI trampolines and runtime reentry.
  Already-foreign C callback targets use their native function pointer directly.
  Callback parameters/returns are restricted to scalars/unit; aggregate callback
  ABIs receive a diagnostic. Managed callbacks must run synchronously on the
  registered calling mutator; retained/foreign-thread callbacks are unsupported.

The FFI integration suite includes an independent C-compiler oracle for aggregate
arguments and returns, mixed INTEGER/SSE register pressure, and private by-value
snapshots. All 26 FFI integration tests pass in debug and release on macOS ARM64
and native Linux x86-64, including the independent aggregate oracle. This does
not establish all callback uses or other target ABIs.
The implementation rejects non-C ABI declarations. Native link configuration
comes from CLI link arguments and/or the manifest's `[link]` section.

Native calls publish roots and suspend all RUNNING registrations on the calling
OS thread for the heap, preserving already-blocked scopes. Embedders must publish
all such registrations' roots before entry and reload derived pointers after
resumption. Each foreign scope owns its suspension token: callback entry consumes
it and restores the whole group, while callback exit republishes and parks a new
group. Nested calls keep separate tokens. Returning from native code or entering
a managed callback waits for active collection before resuming. Native scope
backing is accounted and reused after warmup, then released with runtime storage;
retiring a context with an active foreign scope aborts before deregistration.
Buffer copy-back reloads the destination from its root after native return.
Temporary buffers use checked allocation geometry, eight-byte-aligned initialized
payloads and an opaque ownership token. Zero-length arrays still receive a
non-null payload. The v2 allocation/data/release entry points keep payload
geometry separate from ownership metadata. Both allocations count in native
buffer backing/reservations; the charge ends after both deallocations. The
heap-owned ledger is shared with queue buffers and does not require the channel
registry lock for FFI allocation or metric queries. Legacy v1 helpers retain
their noncollecting, unmetered contract.

Large native allocation and release run BLOCKED; native pressure can collect
before even a small allocation. Generated code reloads the captured source
before copy-in. Cleanup runs after normal calls and on return/break/continue
escaping argument evaluation, preserving any surrounding call scope. Traced
return values are refreshed after collecting cleanup. Directly terminating
arguments stop further argument, coercion and call emission.

Buffers remain stable during the C call and copy-back and are freed before the
scope ends, including inside loops. Tests cover sibling allocation pressure,
allocating callbacks, concurrent collection, nested scopes, early exits, managed
return values and original mutable-owner identity after field replacement.

Review targets include native pointer lifetimes/retention, foreign-thread callbacks,
callback failure/unwinding, native admission and other allocation domains, and ABI classification on
supported targets. Existing `RawPtr` operations require an explicit safety
contract. See PRODUCTION.md. The raylib example has not been launched as part
of this assessment.

Current generated code uses versioned copy helpers. Large scalar copies root
and reload the exact owner and copy at most 32 KiB under canonical object access
before releasing access and polling. Large immutable String copies poll between
pieces of at most 256 KiB; no managed address spans a poll. Inputs up to 32 KiB
retain noncollecting leaf paths. The legacy copy symbols remain noncollecting
for older compiled callers. Scalar copies preserve matching-width per-element SC
access and boolean normalization; all access scopes end before foreign code or
callbacks. Native buffer storage is
private and disjoint from the managed heap; it remains valid while managed roots
move during the foreign call. Copy-out reloads the rooted destination after
foreign return. These operations do not make the entire array an atomic snapshot
and do not support reference arrays. General concurrent runtime/publication
readiness remains tracked in `GC_READINESS.md`.
