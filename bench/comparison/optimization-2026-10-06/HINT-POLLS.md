# Atomic request hints and native synchronization

Discarded experiment: generated GC polls used atomic volatile monotonic loads
at every existing checkpoint. Production retains its acquire loads and ABI v3. The byte only asks the mutator to enter the native slow
path; it does not publish relocated heap data.

Before parking, ai_gc_pollcheck_slow publishes the shadow frame and
ThreadState::enter_safepoint publishes STATE_AT_SAFEPOINT with release ordering.
The collector observes that state with acquire ordering and verifies it under
the safepoint mutex, including the existing state recheck. That orders all prior
mutator heap/root writes before collection. On resume, the collector clears the
request hint before releasing the safepoint mutex/condition variable; the owner
acquires the mutex before consuming resumed heap/root updates. A zero fast hint
therefore does not need to acquire separately. Shared language data accesses
retain their SC ordering; the stress-mode guard is unchanged.

The experimental runtime startup ABI v4 required the slow path to establish resumed-data ordering
independently of the incoming hint. v1/v2/v3 startup wrappers retain compatibility
for older generated callers. An archive exporting only the previous symbols
cannot silently satisfy the new generated entry.

The poll_hint_publication_and_resume_order_plain_slots regression uses plain
UnsafeCell slots, a relaxed scheduling counter and relaxed atomic request reads.
Only the native park/resume handshake orders the data. It checks both directions
for 256 rounds without transferring the owner-affine ThreadState. Existing native
moving-GC tests cover actual rooted pointers and concurrent generated spinners.

Ordering semantics: [Rust atomic orderings](https://doc.rust-lang.org/std/sync/atomic/enum.Ordering.html)
and [LLVM atomic operations](https://llvm.org/docs/Atomics.html). The protocol
argument above comes from this runtime's release/acquire and mutex operations;
relaxed atomicity alone does not order other data.

The one-fork pilot (20 warmups, 10 samples) showed no meaningful performance
improvement: fannkuch 19.6045 ms versus Java 16.77694 ms, binary trees
13.176 ms versus 10.9487 ms, and array 4.961 ms versus 6.5964 ms.
Raw results are in hint-polls-pilot.json. This pilot is not a parity claim.
The production poll change and ABI bump were discarded. The regression test
and corrected protocol comments are retained; final validation is recorded in
final-validation.json. The existing state recheck was already correct; this
work does not claim to repair a new race.
