#[path = "thread_handle.rs"]
mod handle;
pub use handle::{ThreadStateHandle, ThreadStateWeak};

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::gc::header::ObjHeader;
use crate::gc::roots::{FrameChain, RootSource};
use crate::gc::type_info::TypeInfo;

use crate::gc::barrier::SATBBuffer;
use crate::gc::heap::Heap;
use crate::gc::semi_space::PtrPolicy;
use crate::gc::statemap::TraceState;

// Diagnostic identities belong to states, not to a heap's mutable census.
// A process-wide namespace also permits several heaps to share one tracer.
static NEXT_THREAD_TRACE_ID: AtomicUsize = AtomicUsize::new(0);

fn next_trace_id(counter: &AtomicUsize) -> usize {
    counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("thread diagnostic identity space exhausted")
}

// ─── Allocation-site counters ───────────────────────────────────────

/// Per-(allocation-site) cumulative count + byte total. One entry per
/// compile-time-assigned site id (see `Heap::set_alloc_sites`). Updated by
/// the owning thread on compiled and runtime allocation paths; summed across threads at
/// dump time. Plain integers — NOT atomics — because only the owning thread
/// ever writes them (between safepoints) and they are read only when the
/// world is quiescent (program end / a safepoint). Keeping them non-atomic
/// is the whole point: the allocation path must not contend on a shared
/// counter.
#[derive(Clone, Copy, Default, Debug)]
#[repr(C)]
pub struct SiteCounter {
    /// Number of objects allocated at this site, saturating at `u64::MAX`.
    pub count: u64,
    /// Total bytes allocated at this site (header + payload, as the allocator
    /// sized it via `TypeInfo::allocation_size`), saturating at `u64::MAX`.
    pub bytes: u64,
}

const _: () = {
    assert!(core::mem::size_of::<SiteCounter>() == 16);
    assert!(core::mem::offset_of!(SiteCounter, count) == 0);
    assert!(core::mem::offset_of!(SiteCounter, bytes) == 8);
};

// ─── Thread states ──────────────────────────────────────────────────

pub const STATE_RUNNING: u8 = 0;
pub const STATE_AT_SAFEPOINT: u8 = 1;
/// Thread is parked on an external blocking call (mutex, condvar, IO).
/// The GC treats this state as "safepoint-equivalent": it scans the
/// thread's roots without waiting for the thread to poll. The thread
/// must transition back to RUNNING via `exit_blocked` before resuming
/// any mutator work.
pub const STATE_BLOCKED: u8 = 2;

// ─── ThreadState ────────────────────────────────────────────────────

/// Per-thread GC coordination state.
///
/// Each mutator thread owns a `ThreadState` that contains:
/// - A `FrameChain` for shadow frame roots (per-thread, `Cell`-based)
/// - Safepoint synchronization primitives
///
/// The GC coordinator signals threads to reach safepoints, then scans
/// their roots while they're suspended.
pub struct ThreadState {
    trace_id: usize,
    /// Shadow frame chain for this thread's stack roots.
    frame_chain: FrameChain,

    /// Current state: STATE_RUNNING or STATE_AT_SAFEPOINT.
    state: AtomicU8,

    /// Intrusive ownership for a scoped collection caller guard. Only the
    /// owning OS thread touches this link; collectors never inspect it. A
    /// linked registration is BLOCKED, so nested guards skip it. The guard
    /// owns the chain head and unlinks iteratively before restoring RUNNING.
    collection_caller_next: UnsafeCell<Option<ThreadStateHandle>>,

    /// Incremented each time the thread enters a safepoint.
    /// Used by the GC to distinguish old safepoints from new ones.
    safepoint_gen: AtomicU64,

    /// Mutex + Condvar for safepoint suspension.
    safepoint_lock: Mutex<bool>, // bool = "gc_done"
    safepoint_cond: Condvar,

    /// SATB write barrier buffer. Only accessed by the owning thread
    /// during mutation, or by the GC thread after safepoint is reached.
    /// UnsafeCell because it needs interior mutability without Sync.
    satb_buffer: UnsafeCell<SATBBuffer>,

    /// Frame pointer of the JIT call this thread is currently parked
    /// at. Set by the JIT safepoint handler **before** parking, so a
    /// concurrent GC running on another thread can walk this thread's
    /// JIT frames as roots. Cleared (set null) on resume.
    ///
    /// Without this, a concurrent GC scans only the parked thread's
    /// `frame_chain` (host-side roots), missing the JIT-frame spill
    /// slots — and so doesn't update them when relocating objects.
    /// The parked thread then resumes and reads stale `from`-space
    /// pointers (SIGBUS).
    ///
    /// `AtomicPtr` because both the owning thread (set/clear) and
    /// the GC coordinator (read) touch it; the value is a raw FP
    /// that points into the owning thread's stack — valid only while
    /// the owning thread is parked.
    parked_jit_fp: AtomicPtr<u8>,

    /// Pointer to the owning runtime `Thread`'s `state` byte — the flag
    /// the JIT inline safepoint poll reads. The GC coordinator stores a
    /// non-zero value here (via [`request_poll`](Self::request_poll))
    /// when it wants this thread to park, so a thread spinning in JIT'd
    /// code (e.g. an allocation-free self-tail loop) reaches a safepoint.
    /// Null for threads with no runtime `Thread` (e.g. pure-Rust
    /// `MutatorThread`s that poll `gc_requested` directly).
    ///
    /// Set once via [`set_poll_flag`](Self::set_poll_flag) right after
    /// the owning `Thread` is constructed; the address is stable for the
    /// `Thread`'s lifetime (it lives in a `Box`).
    poll_flag: AtomicPtr<AtomicU8>,
    /// Serializes flag installation with stop requests, retaining a request
    /// raised after registration but before the runtime Thread is constructed.
    poll_registration: Mutex<bool>,

    /// Updated only by a world-pause coordinator while this owner is
    /// quiescent, or by the owner acting as collector. Generated code reads
    /// it ordinarily while RUNNING; resume establishes the happens-before.
    relocation_epoch: std::sync::atomic::AtomicUsize,

    /// The OS thread that owns (and registered) this state. Recorded at
    /// construction — registration always happens on the owning thread.
    /// Used by [`Heap::pause_world`](crate::gc::Heap::pause_world) to
    /// exclude the requesting thread from the set it parks.
    os_thread: std::thread::ThreadId,

    /// Scratch GC-root STACK for runtime functions that hold heap pointers
    /// across an allocation (which may collect + relocate). A fn marks the
    /// current depth, pushes its live heap-pointer args, allocates, re-reads
    /// the (relocated) values, then resets to its mark. A stack (not fixed
    /// slots) is required because these calls NEST — e.g. `atom_swap`'s
    /// updater closure calls `string_concat`, which also needs scratch — so
    /// the inner use pushes above the outer's roots and resets back without
    /// clobbering them. Slots `[0..depth)` are scanned by `scan_roots`;
    /// values above `depth` are ignored. Touched only by the owning thread
    /// (between safepoints) or the collector (while it's parked).
    gc_scratch: [std::sync::atomic::AtomicU64; GC_SCRATCH_DEPTH],
    scratch_depth: std::sync::atomic::AtomicUsize,

    /// Per-allocation-site counters (Target-1b allocation-site profiling).
    /// Indexed by the compile-time site id passed to `ai_gc_alloc_*`. Grown
    /// lazily on first use to fit the highest site id seen. `UnsafeCell` (not
    /// atomics) because writes happen ONLY on the owning thread's allocation
    /// path — making this contention-free is the design's whole point — and
    /// reads happen only at a quiescent merge (program end / safepoint), same
    /// access discipline as `satb_buffer`.
    alloc_sites: UnsafeCell<allocator_api2::vec::Vec<SiteCounter, crate::runtime::metadata_allocator::MetadataAllocator>>,
    tlab: UnsafeCell<crate::gc::alloc::Tlab>,
}

/// Maximum nesting of runtime alloc fns parking scratch roots at once.
pub const GC_SCRATCH_DEPTH: usize = 32;

// Safety: ThreadState is designed to be shared between the owning
// mutator thread and the GC coordinator thread.
// - `frame_chain` uses Cell (not Sync), but it's only accessed by:
//   (a) the owning thread during normal execution, or
//   (b) the GC thread after the owning thread has reached a safepoint
//       (happens-before established by the atomic state transition)
// - collection_caller_next is exclusively accessed by the owning OS thread;
//   its private guard retains every linked registration until unlinking.
unsafe impl Sync for ThreadState {}

impl ThreadState {
    pub fn new() -> Self {
        Self::with_allocator(crate::runtime::metadata_allocator::MetadataAllocator::default())
    }

    pub(crate) fn with_allocator(allocator: crate::runtime::metadata_allocator::MetadataAllocator) -> Self {
        ThreadState {
            trace_id: next_trace_id(&NEXT_THREAD_TRACE_ID),
            frame_chain: FrameChain::new(),
            state: AtomicU8::new(STATE_RUNNING),
            collection_caller_next: UnsafeCell::new(None),
            safepoint_gen: AtomicU64::new(0),
            safepoint_lock: Mutex::new(false),
            safepoint_cond: Condvar::new(),
            satb_buffer: UnsafeCell::new(SATBBuffer::with_allocator(256, allocator.clone())),
            parked_jit_fp: AtomicPtr::new(std::ptr::null_mut()),
            poll_flag: AtomicPtr::new(std::ptr::null_mut()),
            poll_registration: Mutex::new(false),
            relocation_epoch: std::sync::atomic::AtomicUsize::new(0),
            os_thread: std::thread::current().id(),
            gc_scratch: Default::default(),
            scratch_depth: std::sync::atomic::AtomicUsize::new(0),
            alloc_sites: UnsafeCell::new(allocator_api2::vec::Vec::new_in(allocator)),
            tlab: UnsafeCell::new(crate::gc::alloc::Tlab::new()),
        }
    }

    /// Stable across retirement, census reordering and shared-tracer heaps.
    pub(crate) fn trace_id(&self) -> usize { self.trace_id }

    fn assert_owner_running(&self) {
        assert_eq!(self.os_thread, std::thread::current().id(), "roots belong to the registering thread");
        assert_eq!(self.state.load(Ordering::Acquire), STATE_RUNNING, "cannot mutate parked roots");
    }

    /// Clear runtime transient roots on final registration retirement.
    /// # Safety
    /// The registering OS thread holds this heap's collector exclusion, no
    /// collection/root visit overlaps, and no active runtime/scratch scope may
    /// resume after retirement. Unlike ordinary root mutation, an idle state
    /// can already be BLOCKED here.
    pub(crate) unsafe fn retire_runtime_roots(&self) {
        assert_eq!(self.os_thread, std::thread::current().id(), "retirement belongs to the registering thread");
        unsafe { self.clear_parked_jit_fp(); }
        self.scratch_depth.store(0, Ordering::Release);
    }

    /// Access host frame storage only on the registering RUNNING mutator.
    /// Collector access goes through root enumeration under a world pause.
    pub fn frame_chain(&self) -> &FrameChain {
        self.assert_owner_running();
        &self.frame_chain
    }

    /// Layout and size have been validated by this thread's heap. Owning
    /// RUNNING mutator only; no safepoint may overlap TLAB initialization.
    #[inline]
    pub(crate) unsafe fn alloc_local_sized(&self, space: &crate::gc::AtomicBumpAllocator, size: usize) -> *mut u8 {
        debug_assert_eq!(self.os_thread, std::thread::current().id());
        unsafe { (&mut *self.tlab.get()).alloc_sized(space, size) }
    }

    /// Obtain the stable owner-only window address. The ThreadState must stay
    /// alive and allocation may only be performed by this state's mutator.
    pub(crate) unsafe fn inline_tlab_ptr(&self) -> *mut crate::gc::InlineTlab {
        unsafe { core::ptr::addr_of_mut!((*self.tlab.get()).window) }
    }

    /// Stable address for the owning generated mutator.
    pub(crate) fn relocation_epoch_ptr(&self) -> *const std::sync::atomic::AtomicUsize {
        &self.relocation_epoch
    }

    /// # Safety
    /// The owner is quiescent under the world-pause protocol, or is the
    /// calling collector itself. No generated read may execute concurrently.
    pub(crate) unsafe fn advance_relocation_epoch(&self) {
        self.relocation_epoch.fetch_add(1, Ordering::Relaxed);
    }

    /// Close a generated allocation reservation before the world resumes.
    /// The caller owns the collection/pause census: this mutator is quiescent
    /// or is the triggering collector, and registration/retirement are locked.
    /// Keep the extent alive until the owner validates its native arena epoch.
    pub(crate) unsafe fn invalidate_inline_tlab(&self) {
        unsafe { (*self.tlab.get()).window.limit = 0; }
    }

    /// Record one allocation at `site_id` of `bytes` bytes (Target-1b
    /// allocation-site profiling). Called by `ai_gc_alloc_*` on the owning
    /// thread's slow path. Non-atomic, owning-thread-only; grows the counter
    /// vector on demand to fit `site_id` (a cold branch taken at most once per
    /// distinct site). No allocation profiling table installed is fine — the
    /// counters still accumulate and are simply unlabelled at merge time.
    ///
    /// # Safety
    /// Must be called only by the thread that owns this `ThreadState`, never
    /// concurrently with a quiescent read (`alloc_site_counters`).
    #[inline]
    pub unsafe fn record_alloc(&self, site_id: u32, bytes: u64) {
        let v = unsafe { &mut *self.alloc_sites.get() };
        let idx = site_id as usize;
        if idx >= v.len() {
            v.resize(idx + 1, SiteCounter::default());
            // Compiled allocations update these same owner-only counters.
            // Refresh the window after every possible vector relocation.
            let window = unsafe { &mut (*self.tlab.get()).window };
            window.counters = v.as_mut_ptr();
            window.counters_len = v.len();
        }
        let c = &mut v[idx];
        c.count = c.count.saturating_add(1);
        c.bytes = c.bytes.saturating_add(bytes);
    }

    /// Snapshot this thread's per-site counters for a cross-thread merge.
    /// Call only when the world is quiescent (program end / at a safepoint),
    /// when the owning thread is not mutating the counters.
    ///
    /// # Safety
    /// No concurrent `record_alloc` on the owning thread.
    pub unsafe fn alloc_site_counters(&self) -> Vec<SiteCounter> {
        unsafe { &*self.alloc_sites.get() }.iter().copied().collect()
    }

    /// Record storage can survive arena reset until the owner validates its
    /// epoch. Test inventories deduplicate this owner with the arena registry.
    /// # Safety
    /// Owning OS thread or a quiescent observation without TLAB writes.
    #[cfg(test)]
    pub(crate) unsafe fn tlab_metadata_record(&self) -> Option<(usize, usize)> {
        unsafe { &*self.tlab.get() }.metadata_record()
    }

    /// Requested backing still owned by this state, excluding embedded/control
    /// storage and SATB chunks already transferred to the global queue.
    /// # Safety
    /// Owning OS thread, or a quiescent observation with no buffer/counter writes.
    #[cfg(test)]
    pub(crate) unsafe fn native_backing_bytes(&self) -> usize {
        unsafe { &*self.satb_buffer.get() }.backing_bytes()
            + unsafe { &*self.alloc_sites.get() }.capacity() * std::mem::size_of::<SiteCounter>()
    }

    /// Remember the current scratch-stack depth so a nested run of
    /// allocations can be unwound back to here with [`scratch_reset`]. Call
    /// at the top of a runtime fn that parks roots; pass the result to
    /// `scratch_reset` before returning.
    pub fn scratch_mark(&self) -> usize {
        self.assert_owner_running();
        self.scratch_depth.load(Ordering::Acquire)
    }

    /// Push a heap pointer onto the scratch stack so it survives (and is
    /// relocated by) a collection during a subsequent allocation. Returns
    /// the slot index; re-read the (possibly relocated) pointer with
    /// [`scratch_at`](Self::scratch_at) after each alloc. Panics if the
    /// stack overflows `GC_SCRATCH_DEPTH` (runtime-fn nesting is shallow).
    /// # Safety
    /// Owning RUNNING mutator; ptr is a valid managed pointer or null and is
    /// reloaded after every collection. Preserve all live slots until use ends.
    pub unsafe fn push_scratch(&self, ptr: *const u8) -> usize {
        self.assert_owner_running();
        let i = self.scratch_depth.load(Ordering::Acquire);
        assert!(
            i < GC_SCRATCH_DEPTH,
            "GC scratch stack overflow (depth {GC_SCRATCH_DEPTH}); runtime-fn nesting too deep"
        );
        self.gc_scratch[i].store(ptr as u64, Ordering::Release);
        self.scratch_depth.store(i + 1, Ordering::Release);
        i
    }

    /// Read scratch slot `i` (the post-relocation pointer).
    pub fn scratch_at(&self, i: usize) -> *mut u8 {
        self.assert_owner_running();
        assert!(i < self.scratch_depth.load(Ordering::Acquire), "scratch root is not live");
        self.gc_scratch[i].load(Ordering::Acquire) as *mut u8
    }

    /// Pop the scratch stack back to a depth previously returned by
    /// [`scratch_mark`](Self::scratch_mark), so a later collection doesn't
    /// pin stale pointers.
    /// # Safety
    /// Owning RUNNING mutator, restoring a valid LIFO mark; no live inner roots
    /// or callers may depend on the removed slots.
    pub unsafe fn scratch_reset(&self, mark: usize) {
        self.assert_owner_running();
        assert!(mark <= self.scratch_depth.load(Ordering::Acquire), "invalid scratch mark");
        self.scratch_depth.store(mark, Ordering::Release);
    }

    /// Open a scoped scratch region that auto-resets on drop. Preferred over
    /// manual `scratch_mark`/`scratch_reset` in Rust code with `?`
    /// early-returns (wire decode, arg builders): the `Drop` runs on every
    /// exit path, so a decode error can't leak stale scratch roots that a
    /// later collection would wrongly pin/scan.
    /// # Safety
    /// Scope stays on the RUNNING owner and is destroyed in LIFO order before
    /// any outer scratch roots are reset. Do not transfer or forget live roots.
    pub unsafe fn scratch_scope(&self) -> ScratchScope<'_> {
        self.assert_owner_running();
        ScratchScope {
            ts: self,
            mark: self.scratch_mark(),
            _owner: core::marker::PhantomData,
        }
    }

    /// The OS thread that owns this state.
    pub fn os_thread(&self) -> std::thread::ThreadId {
        self.os_thread
    }

    /// Point this thread's safepoint poll flag at the owning runtime
    /// `Thread`'s `state` byte. Called once, right after the `Thread` is
    /// built, before the thread starts running JIT'd code.
    /// # Safety
    /// Non-null storage is a valid pinned AtomicU8 until detached. Detaching
    /// serializes with every coordinator access before the storage may be freed.
    pub unsafe fn set_poll_flag(&self, state_byte: *mut AtomicU8) {
        let requested = self.poll_registration.lock().unwrap();
        if *requested && !state_byte.is_null() {
            unsafe { (*state_byte).store(1, Ordering::Release); }
        }
        self.poll_flag.store(state_byte, Ordering::Release);
    }

    /// Ask this thread to reach a safepoint by raising its JIT poll flag.
    /// No-op if the thread has no runtime `Thread` (null flag). Generated
    /// polls read this request hint atomically and volatilely. Root publication
    /// is ordered by the parked-state release/acquire pair; resumption uses
    /// the safepoint mutex/condvar. The hint does not publish heap data.
    /// Release stores also support older callers using acquire hint loads.
    pub fn request_poll(&self) {
        let mut requested = self.poll_registration.lock().unwrap();
        *requested = true;
        let p = self.poll_flag.load(Ordering::Acquire);
        if !p.is_null() {
            unsafe { (*p).store(1, Ordering::Release) };
        }
    }

    /// Lower this thread's JIT poll flag before the coordinator resumes it.
    /// The resume mutex handshake orders this clear before the owner's next
    /// hint read, preventing an old request from causing another park.
    pub fn clear_poll(&self) {
        let mut requested = self.poll_registration.lock().unwrap();
        *requested = false;
        let p = self.poll_flag.load(Ordering::Acquire);
        if !p.is_null() {
            unsafe { (*p).store(0, Ordering::Release) };
        }
    }

    /// Set the JIT frame pointer that should be exposed to GC root
    /// scans while this thread is parked at a safepoint. Cleared
    /// (with `clear_parked_jit_fp`) on resume.
    /// # Safety
    /// Owning RUNNING mutator; fp is null or a valid published JIT frame chain until cleared after resumption.
    pub unsafe fn set_parked_jit_fp(&self, fp: *const u8) {
        self.parked_jit_fp.store(fp as *mut u8, Ordering::Release);
    }

    /// Clear the parked JIT FP pointer.
    /// # Safety
    /// Owning RUNNING mutator after resumption; no collector is scanning the retired JIT frames.
    pub unsafe fn clear_parked_jit_fp(&self) {
        self.parked_jit_fp
            .store(std::ptr::null_mut(), Ordering::Release);
    }

    /// Read the parked JIT frame pointer. Returns null if this
    /// thread isn't currently parked at a JIT safepoint.
    pub fn parked_jit_fp(&self) -> *const u8 {
        self.parked_jit_fp.load(Ordering::Acquire) as *const u8
    }

    /// Access the SATB buffer.
    ///
    /// # Safety
    /// Must only be called by the owning thread during mutation,
    /// or by the GC thread after the owning thread is at a safepoint.
    pub unsafe fn satb_buffer(&self) -> &mut SATBBuffer {
        unsafe { &mut *self.satb_buffer.get() }
    }

    /// Record a pre-write reference and publish full local chunks without a
    /// managed safepoint. May run inside canonical object access: the queue
    /// lock never waits for a mutator or acquires object-access stripes.
    ///
    /// # Safety
    /// Called only by this state's owning RUNNING mutator in `heap`. The old
    /// bits must be valid for its pointer policy. Caller checks copying phase.
    pub(crate) unsafe fn log_satb(&self, heap: &Heap, old: u64) {
        let buffer = unsafe { self.satb_buffer() };
        buffer.log(old);
        if buffer.should_flush() {
            heap.satb_queue.flush_buffer(buffer);
        }
    }

    /// Enter safepoint: mark this thread as suspended and wait
    /// for the GC to finish.
    /// # Safety
    /// Owning registered mutator; all live references are published and no heap/root mutation occurs until resumption.
    pub unsafe fn enter_safepoint(&self) {
        self.safepoint_gen.fetch_add(1, Ordering::AcqRel);
        self.state.store(STATE_AT_SAFEPOINT, Ordering::Release);

        let mut gc_done = self.safepoint_lock.lock().unwrap();
        while !*gc_done {
            gc_done = self.safepoint_cond.wait(gc_done).unwrap();
        }

        *gc_done = false;
        self.state.store(STATE_RUNNING, Ordering::Release);
    }

    /// Resume this thread after GC completes.
    /// # Safety
    /// Coordinator holds the registry release protocol; evacuation and all root updates for this state are complete.
    pub unsafe fn resume(&self) {
        let mut gc_done = self.safepoint_lock.lock().unwrap();
        *gc_done = true;
        self.safepoint_cond.notify_one();
        drop(gc_done);
    }

    /// Check if this thread is at a safepoint (atomic state only).
    pub fn is_at_safepoint(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_AT_SAFEPOINT
    }

    /// Check if this thread is truly blocked at a safepoint with no
    /// stale resume pending. This acquires the safepoint lock to verify
    /// `gc_done` is false, meaning the thread is genuinely waiting on the
    /// condvar and won't spontaneously wake up.
    ///
    /// Use this instead of `is_at_safepoint()` when you need to ensure
    /// the thread is safe to scan and won't leave its safepoint until
    /// explicitly resumed.
    ///
    /// `STATE_BLOCKED` threads are also considered safely at safepoint:
    /// they cannot transition back to RUNNING without first observing
    /// that no GC is in progress, so it is sound to scan their roots.
    pub fn is_safely_at_safepoint(&self) -> bool {
        match self.state.load(Ordering::Acquire) {
            STATE_BLOCKED => true,
            STATE_AT_SAFEPOINT => {
                let gc_done = self.safepoint_lock.lock().unwrap();
                // Re-read the state under the lock: enter_safepoint
                // consumes gc_done and stores RUNNING while holding it,
                // so the pre-lock state load above can be stale — seeing
                // (AT_SAFEPOINT, gc_done=false) for a thread that has
                // already resumed mutating.
                !*gc_done && self.state.load(Ordering::Acquire) == STATE_AT_SAFEPOINT
            }
            _ => false,
        }
    }

    /// Used by inspection pauses to suspend only RUNNING registrations owned
    /// by the calling OS thread, preserving outer blocked scopes.
    pub(crate) fn is_running(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_RUNNING
    }

    /// # Safety
    /// Called only by the owning OS thread, while RUNNING, when adding this
    /// registration to its private scoped caller guard. No link may already
    /// exist. The guard must retain this registration until it takes the link.
    pub(super) unsafe fn link_collection_caller(&self, next: Option<ThreadStateHandle>) {
        debug_assert_eq!(self.os_thread, std::thread::current().id());
        debug_assert!(self.is_running());
        let link = unsafe { &mut *self.collection_caller_next.get() };
        assert!(link.is_none(), "registration already belongs to a caller guard");
        *link = next;
    }

    /// # Safety
    /// The owning OS thread is removing this BLOCKED registration from the
    /// same private guard that installed the link, before restoring RUNNING.
    pub(super) unsafe fn take_collection_caller(&self) -> Option<ThreadStateHandle> {
        debug_assert_eq!(self.os_thread, std::thread::current().id());
        debug_assert!(self.is_blocked());
        unsafe { (&mut *self.collection_caller_next.get()).take() }
    }

    /// True if this thread is currently in the BLOCKED state.
    pub fn is_blocked(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_BLOCKED
    }

    /// Current safepoint generation (incremented each enter_safepoint call).
    pub fn safepoint_gen(&self) -> u64 {
        self.safepoint_gen.load(Ordering::Acquire)
    }

    /// Enter a blocking region. The thread is treated as if it had
    /// reached a safepoint, so the GC can scan its roots and proceed
    /// without waiting for the thread to poll.
    ///
    /// MUST be paired with `exit_blocked`. Nesting is not supported —
    /// callers must be in `STATE_RUNNING` when calling.
    ///
    /// # Preconditions
    /// - All currently-live GC pointers must already be rooted in
    ///   `frame_chain`. The GC may scan immediately after this returns.
    /// - The caller must not mutate the heap or root set while in the
    ///   blocked region. Allocations and root-set changes are only
    ///   allowed after `exit_blocked` returns.
    ///
    /// # Safety
    /// Violating these preconditions can expose missed roots or dangling pointers.
    /// Owning RUNNING mutator; all live roots are published and heap/root mutation stops until matching exit_blocked.
    pub unsafe fn enter_blocked(&self) {
        self.safepoint_gen.fetch_add(1, Ordering::AcqRel);
        let prev = self.state.swap(STATE_BLOCKED, Ordering::AcqRel);
        debug_assert_eq!(
            prev, STATE_RUNNING,
            "enter_blocked: thread was not in STATE_RUNNING (was {prev})"
        );
    }

    /// Exit a blocking region. Waits for any in-progress GC to finish
    /// (so the thread does not start running with stale references after
    /// a moving collector compacted), then transitions back to RUNNING.
    ///
    /// Also clears any stale `gc_done` set by `resume()` calls that
    /// fired during the blocked window — without this, the next
    /// `enter_safepoint` would observe the residual flag and exit early
    /// without participating in the next collection.
    /// # Safety
    /// Owning BLOCKED mutator, paired with enter_blocked and the exact heap it registered with.
    pub unsafe fn exit_blocked(&self, heap: &Heap) {
        loop {
            // Wait for any active STW window to fully close before
            // resuming. Concurrent GC's `barriers_active` is also a
            // window during which we shouldn't blindly transition: the
            // collector may still need our state stable.
            while heap.gc_requested() || heap.barriers_active() {
                std::thread::yield_now();
            }
            // The check above and the swap below must be atomic w.r.t.
            // a collector's safepoint census: a collector that raises
            // gc_requested between them samples us as BLOCKED ("safely
            // parked, scan in place") while we resume mutating — its
            // relocations then race our reads/writes and leave stale
            // from-space pointers behind (garbage type_id one or two
            // collections later). `transition_outside_gc` revalidates
            // under the threads lock, which every collector holds while
            // raising the flag.
            let transitioned = heap.transition_outside_gc(|| {
                unsafe { self.resume_blocked_after_pause(); }
            });
            if transitioned {
                return;
            }
        }
    }

    /// Restore one BLOCKED registration while the heap census lock excludes a
    /// new pause. Concurrent copying may remain active; its barriers protect
    /// resumed mutator operations just as after an ordinary safepoint.
    /// # Safety
    /// Owning OS thread, all root updates complete, heap threads lock held and
    /// gc_requested false. Restore every sibling before releasing that lock.
    pub(super) unsafe fn resume_blocked_after_pause(&self) {
        // Drain any stale resume flag. resume() during our BLOCKED
        // window left gc_done = true; if we don't consume it, the
        // next enter_safepoint short-circuits and the next GC
        // busy-waits for us until we poll a fresh safepoint.
        {
            let mut gc_done = self.safepoint_lock.lock().unwrap();
            *gc_done = false;
    }
    let prev = self.state.swap(STATE_RUNNING, Ordering::AcqRel);
    debug_assert_eq!(
        prev, STATE_BLOCKED,
        "exit_blocked: thread was not in STATE_BLOCKED (was {prev})"
    );
    }

    /// Permanently park this state on behalf of an owner thread that has
    /// handed its runtime off to another OS thread and will never mutate
    /// the heap through it again.
    ///
    /// Without this, a handed-off runtime's home `ThreadState` stays
    /// `STATE_RUNNING` forever while its OS thread runs unrelated code —
    /// it never polls this heap's safepoints, so every stop-the-world
    /// collection spins in `is_safely_at_safepoint` waiting for it
    /// (livelock). Parking it as `STATE_BLOCKED` lets the GC scan its
    /// (empty) frame chain in place, exactly like a thread blocked in a
    /// syscall.
    ///
    /// CAS `RUNNING → BLOCKED`; idempotent (no-op unless currently
    /// RUNNING, so concurrent/repeated calls and an in-progress safepoint
    /// are all safe). May be called from any thread.
    ///
    /// # Preconditions (logical, like `enter_blocked`)
    /// The owning thread must have no live un-rooted heap pointers and
    /// must never run mutator code on this state again. Violating this
    /// is a GC-level soundness bug.
    /// # Safety
    /// Owning mutator participates in the collector handoff protocol with all live roots published.
    pub unsafe fn park_handed_off(&self) {
        if self
            .state
            .compare_exchange(
                STATE_RUNNING,
                STATE_BLOCKED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.safepoint_gen.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// RAII scratch-stack region. Push heap pointers with [`push`](Self::push)
/// to root them across a subsequent allocation; re-read the relocated
/// pointer with [`get`](Self::get) after each alloc. The region pops back to
/// its opening depth on drop, so every exit path (including `?` errors)
/// cleans up. The token cannot be transferred to another thread:
/// ```compile_fail
/// use gcrust_rt::gc::thread::ScratchScope;
/// fn requires_send<T: Send>() {}
/// requires_send::<ScratchScope<'static>>();
/// ```
pub struct ScratchScope<'a> {
    ts: &'a ThreadState,
    mark: usize,
    _owner: core::marker::PhantomData<std::rc::Rc<()>>,
}

impl<'a> ScratchScope<'a> {
    /// Push a heap pointer; returns its slot index for later [`get`](Self::get).
    /// # Safety
    /// ptr is a valid live managed reference or null on this RUNNING owner.
    pub unsafe fn push(&self, ptr: *const u8) -> usize {
        unsafe { self.ts.push_scratch(ptr) }
    }

    /// Read the (possibly relocated) pointer at slot index `i`.
    pub fn get(&self, i: usize) -> *mut u8 {
        self.ts.scratch_at(i)
    }
}

impl Drop for ScratchScope<'_> {
    fn drop(&mut self) {
        unsafe { self.ts.scratch_reset(self.mark); }
    }
}

unsafe impl RootSource for ThreadState {
    unsafe fn scan_roots(&self, visitor: &mut dyn FnMut(*mut u64)) {
        unsafe { self.frame_chain.scan_roots(visitor); }
        // Scratch roots: heap pointers a runtime fn parked here across an
        // allocation. Only the live prefix `[0..depth)` is scanned; slots
        // above `depth` hold stale pointers a popped frame left behind.
        let depth = self.scratch_depth.load(Ordering::Acquire);
        for s in &self.gc_scratch[..depth] {
            visitor(s.as_ptr() as *mut u64);
        }
    }
}

// ─── MutatorThread ──────────────────────────────────────────────────

/// RAII handle for a mutator thread registered with a `Heap`.
///
/// This is the main API for mutator code:
/// - `alloc()` / `alloc_obj()` — allocate objects, auto-trigger GC if needed
/// - `safepoint()` — poll for GC requests (call at loop backedges, etc.)
/// - `frame_chain()` — access the shadow frame chain for rooting
///
/// On drop, deregisters from the heap.
///
/// # Example
/// ```rust,ignore
/// let heap = Arc::new(Heap::new::<Compact>(65536));
/// let mut mt = MutatorThread::register(heap.clone());
///
/// let frame = RootFrame::<2>::new();
/// let _guard = unsafe { mt.frame_chain().push(&frame) };
///
/// let obj = mt.alloc_obj::<Compact>(&PAIR_INFO, 0);
/// frame.slots[0].set(obj as u64);
///
/// mt.safepoint(); // check for GC
/// ```
/// Mutator state and shadow roots belong to the registering OS thread.
/// ```compile_fail
/// use gcrust_rt::gc::{MutatorThread, IdentityPtrPolicy};
/// fn requires_send<T: Send>() {}
/// requires_send::<MutatorThread<IdentityPtrPolicy>>();
/// ```
pub struct MutatorThread<P: PtrPolicy> {
    heap: Arc<Heap>,
    state: ThreadStateHandle,
    thread_id: usize,
    _marker: core::marker::PhantomData<(P, std::rc::Rc<()>)>,
}

impl<P: PtrPolicy> MutatorThread<P> {
    /// Register a new mutator thread with the heap.
    pub fn register(heap: Arc<Heap>) -> Self {
        let (state, thread_id) = heap.register_thread();
        MutatorThread {
            heap,
            state,
            thread_id,
            _marker: core::marker::PhantomData,
        }
    }

    /// Access the thread's shadow frame chain for rooting.
    pub fn frame_chain(&self) -> &FrameChain {
        self.state.frame_chain()
    }

    /// Access the thread state (for advanced usage).
    pub fn state(&self) -> &ThreadStateHandle {
        &self.state
    }

    /// Access the heap.
    pub fn heap(&self) -> &Arc<Heap> {
        &self.heap
    }

    /// Poll for GC. If collection has been requested, this thread
    /// enters a safepoint (suspends) until the GC completes.
    ///
    /// Also flushes the SATB buffer if it's full.
    ///
    /// Call this at loop backedges and other natural safepoints.
    /// Fast path: single relaxed atomic load + branch.
    #[inline(always)]
    /// # Safety
    /// All live managed references in every registration on this OS thread for
    /// this heap must be published and remain valid through collection. Reload
    /// derived pointers afterward. Run on the owning mutator thread.
    pub unsafe fn safepoint(&self) {
        // Flush SATB buffer if full (before entering safepoint)
        let buf = unsafe { self.state.satb_buffer() };
        if buf.should_flush() {
            self.heap.satb_queue.flush_buffer(buf);
        }
        if self.heap.gc_requested() {
            if let Some(ref tracer) = self.heap.tracer {
                tracer.record_thread(self.thread_id, TraceState::AtSafepoint);
            }
            unsafe { self.heap.safepoint(&self.state) };
            if let Some(ref tracer) = self.heap.tracer {
                tracer.record_thread(self.thread_id, TraceState::Running);
            }
        }
    }

    /// Protect one object for a complete non-safepointing access.
    ///
    /// The exclusive borrow prevents allocation or safepoint methods through
    /// this mutator while the guard remains live.
    ///
    /// ```compile_fail
    /// use gcrust_rt::gc::{MutatorThread, IdentityPtrPolicy};
    /// fn invalid(thread: &mut MutatorThread<IdentityPtrPolicy>, object: *mut u8) {
    ///     let access = unsafe { thread.object_access(object) };
    ///     unsafe { thread.safepoint(); }
    ///     let _ = access.as_ptr();
    /// }
    /// ```
    ///
    /// # Safety
    /// As Heap::object_access. Fields still obey their usual atomic/aggregate
    /// synchronization, and pointer writes require their normal GC barriers.
    pub unsafe fn object_access(&mut self, object: *mut u8) -> crate::gc::ObjectAccess<'_> {
        unsafe { self.heap.object_access(object) }
    }

    /// Identity-aware SeqCst CAS for an untagged managed-reference field.
    ///
    /// # Safety
    /// As Heap::compare_exchange_reference. This is for raw managed bases,
    /// not fields encoded with a tagged pointer policy.
    pub unsafe fn compare_exchange_reference(
        &mut self,
        object: *mut u8,
        offset: usize,
        expected: *mut u8,
        new: *mut u8,
    ) -> Result<*mut u8, *mut u8> {
        unsafe { self.heap.compare_exchange_reference(&self.state, object, offset, expected, new) }
    }

    /// Write barrier: call BEFORE overwriting a pointer field.
    ///
    /// Logs the old value in the thread-local SATB buffer so the
    /// concurrent GC can discover objects disconnected by the mutator.
    ///
    /// Fast path when no GC is active: single atomic load + branch.
    /// When GC is active: append locally and publish threshold-sized chunks
    /// to the global queue. This cannot enter a managed safepoint.
    #[inline(always)]
    /// # Safety
    /// Run on the owning mutator thread. `old_value` must be null or a valid
    /// managed reference under this heap's pointer policy.
    pub unsafe fn write_barrier(&self, old_value: u64) {
        if !self.heap.barriers_active() {
            return;
        }
        unsafe { self.state.log_satb(&self.heap, old_value); }
    }

    /// Compatibility after-store hook for a forwarded eight-byte field.
    /// It does not synchronize the original operation. Use object_access for
    /// the complete access and the normal language/reference barriers too.
    ///
    /// # Safety
    /// As Heap::replication_barrier: valid aligned eight-byte field, complete
    /// copying exclusion, rooting and ordinary field/reference synchronization.
    #[inline(always)]
    pub unsafe fn replication_barrier(&self, obj: *mut u8, field_offset: usize, new_bits: u64) {
        unsafe { self.heap.replication_barrier(obj, field_offset, new_bits) };
    }

    /// Generational write barrier: if a tenured object stores a nursery
    /// pointer, mark the corresponding card dirty.
    ///
    /// Call this AFTER writing to a field. The card table ensures minor
    /// GC can find old→young pointers without scanning all of tenured space.
    ///
    /// Fast path when no nursery: single branch on Option.
    ///
    /// # Safety
    /// - `obj` must point to a valid heap object.
    #[inline(always)]
    pub unsafe fn generational_write_barrier(&self, obj: *mut u8, new_bits: u64) {
        if !self.heap.has_nursery() {
            return;
        }
        if let Some(new_ptr) = P::try_decode_ptr(new_bits) {
            if self.heap.is_nursery(new_ptr) {
                self.heap.mark_card_dirty(obj as *const u8);
            }
        }
    }

    /// Acquire a `Mutex` while keeping the GC unblocked.
    ///
    /// Naively calling `mutex.lock()` parks the OS thread without
    /// transitioning to a safepoint state. If another thread triggers
    /// GC while this one is parked, the GC busy-waits forever for our
    /// thread to reach a safepoint — deadlock.
    ///
    /// An uncontended acquisition stays RUNNING. On contention, wait while
    /// BLOCKED, release native ownership, then resume and retry acquisition.
    /// No guard is retained while waiting for GC to finish: embedding root
    /// scanners may need this same mutex during a collection pause.
    /// Poison and unwind paths restore the original RUNNING state.
    /// Contention suspends all RUNNING registrations on this OS thread for
    /// this heap; already-BLOCKED siblings keep their enclosing state. Publish
    /// every affected JIT frame before calling and reload derived managed
    /// pointers afterward, as at any safepoint.
    ///
    /// # Preconditions
    /// - All currently-live GC pointers must be rooted in `frame_chain`
    ///   before calling. The GC may scan immediately after we park.
    /// - The caller must currently be in `STATE_RUNNING` (no nested
    ///   `lock_safe` calls inside another blocking region).
    ///
    /// After this returns the thread is RUNNING again. Drop the guard before
    /// polling or making a collecting call: a collector may need the same mutex
    /// to visit embedding roots. Poll reasonably soon after releasing it; a
    /// fresh GC may have started while we were exiting the blocked window.
    /// # Safety
    /// All live managed references must remain registered and valid through any
    /// collection. Do not park, safepoint or allocate managed objects while
    /// holding the returned guard.
    pub unsafe fn lock_safe<'a, T>(&self, mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
        loop {
            match mutex.try_lock() {
                Ok(guard) => return guard,
                Err(std::sync::TryLockError::Poisoned(error)) => {
                    panic!("lock_safe: mutex poisoned: {error}");
                }
                Err(std::sync::TryLockError::WouldBlock) => {}
            }
            let resume = self.heap.suspend_current_registrations();
            // Wait for availability without carrying ownership through GC
            // resumption. Root scanning may need this same native mutex.
            let poisoned = match mutex.lock() {
                Ok(guard) => {
                    drop(guard);
                    false
                }
                Err(error) => {
                    drop(error);
                    true
                }
            };
            drop(resume);
            if poisoned {
                panic!("lock_safe: mutex poisoned");
            }
            // Another caller may win after wakeup. Retry only while RUNNING;
            // never block or restore RUNNING while retaining the native guard.
        }
    }

    /// Read barrier: follow forwarding pointer if object was relocated.
    ///
    /// During concurrent GC, objects may have been copied to to-space.
    /// This returns the up-to-date address.
    ///
    /// Fast path when no GC is active: returns ptr unchanged.
    ///
    /// # Safety
    /// - `ptr` must point to a valid heap object or be null.
    #[inline(always)]
    pub unsafe fn read_barrier(&self, ptr: *mut u8) -> *mut u8 {
        unsafe { self.heap.read_barrier(ptr) }
    }

    /// Allocate an object from the heap. For generational heaps, allocates
    /// from the nursery first. If space is exhausted, triggers minor GC
    /// (generational) or major GC (non-generational) and retries.
    ///
    /// Returns null only if allocation still fails after GC.
    /// # Safety
    /// All live managed references must remain registered and valid through any
    /// collection; allocation descriptors must match this heap and header.
    pub unsafe fn alloc(&self, info: &TypeInfo, varlen_len: usize) -> *mut u8 {
        let Some(request) = self.heap.allocation_request(info, varlen_len) else {
            return core::ptr::null_mut();
        };
        if self.heap.gc_every_alloc() {
            self.trigger_gc();
        }
        let ptr = unsafe { request.allocate(&self.state) };
        if !ptr.is_null() {
            return ptr;
        }
        self.alloc_slow_path(info, varlen_len)
    }

    /// Allocate and initialize header + varlen count.
    /// Triggers GC if from-space is full.
    /// # Safety
    /// All live managed references must remain registered and valid through any
    /// collection; allocation descriptors must match this heap and header.
    pub unsafe fn alloc_obj<H: ObjHeader>(&self, info: &TypeInfo, varlen_len: usize) -> *mut u8 {
        let Some(request) = self.heap.object_request::<H>(info, varlen_len) else {
            return core::ptr::null_mut();
        };
        if self.heap.gc_every_alloc() {
            self.trigger_gc();
        }
        let ptr = unsafe { request.allocate(&self.state) };
        if !ptr.is_null() {
            return ptr;
        }
        self.alloc_obj_slow_path::<H>(info, varlen_len)
    }

    #[cold]
    fn alloc_slow_path(&self, info: &TypeInfo, varlen_len: usize) -> *mut u8 {
        if self.heap.has_nursery() {
            // Try minor GC first → retry nursery
            self.trigger_minor_gc();
            let ptr = unsafe { self.heap.alloc_mutator(&self.state, info, varlen_len) };
            if !ptr.is_null() {
                return ptr;
            }
            // Nursery still full (shouldn't happen after reset) → try major GC
            self.trigger_gc();
            let ptr = unsafe { self.heap.alloc_mutator(&self.state, info, varlen_len) };
            if !ptr.is_null() {
                return ptr;
            }
            // Last resort: allocate directly in tenured
            unsafe { self.heap.alloc_tenured(info, varlen_len) }
        } else {
            self.trigger_gc();
            unsafe { self.heap.alloc(info, varlen_len) }
        }
    }

    #[cold]
    fn alloc_obj_slow_path<H: ObjHeader>(&self, info: &TypeInfo, varlen_len: usize) -> *mut u8 {
        if self.heap.has_nursery() {
            // Try minor GC first → retry nursery
            self.trigger_minor_gc();
            let ptr = unsafe {
                self.heap
                    .alloc_mutator_obj::<H>(&self.state, info, varlen_len)
            };
            if !ptr.is_null() {
                return ptr;
            }
            // Major GC
            self.trigger_gc();
            let ptr = unsafe {
                self.heap
                    .alloc_mutator_obj::<H>(&self.state, info, varlen_len)
            };
            if !ptr.is_null() {
                return ptr;
            }
            // Last resort: tenured
            unsafe { self.heap.alloc_obj::<H>(info, varlen_len) }
        } else {
            self.trigger_gc();
            unsafe { self.heap.alloc_obj::<H>(info, varlen_len) }
        }
    }

    /// Trigger a minor (nursery) GC from this mutator thread.
    ///
    /// If a concurrent major GC is in progress, waits for it to finish
    /// instead. If tenured space doesn't have enough room for worst-case
    /// promotion, triggers a major GC first to reclaim tenured space.
    fn trigger_minor_gc(&self) {
        if self.heap.barriers_active() {
            // Concurrent major GC in progress — wait for it
            while self.heap.barriers_active() {
                if self.heap.gc_requested() {
                    unsafe { self.heap.safepoint(&self.state) };
                } else {
                    std::thread::yield_now();
                }
            }
            return;
        }
        // Check if tenured has enough room for worst-case promotion.
        // If not, run a major GC first to reclaim tenured space.
        if self.heap.nursery_used() > self.heap.from_remaining() {
            unsafe { self.heap.mutator_triggered_gc::<P>(&self.state) };
        }
        unsafe { self.heap.mutator_triggered_minor_gc::<P>(&self.state) };
    }

    /// Trigger a GC from this mutator thread.
    ///
    /// If a concurrent GC is already in progress, yields and waits for
    /// it to finish (participating in safepoints as needed). Otherwise,
    /// runs a full STW collection via `mutator_triggered_gc`.
    fn trigger_gc(&self) {
        if self.heap.barriers_active() {
            if let Some(ref tracer) = self.heap.tracer {
                tracer.record_thread(self.thread_id, TraceState::WaitingForGc);
            }
            while self.heap.barriers_active() {
                if self.heap.gc_requested() {
                    if let Some(ref tracer) = self.heap.tracer {
                        tracer.record_thread(self.thread_id, TraceState::AtSafepoint);
                    }
                    unsafe { self.heap.safepoint(&self.state) };
                } else {
                    std::thread::yield_now();
                }
            }
            if let Some(ref tracer) = self.heap.tracer {
                tracer.record_thread(self.thread_id, TraceState::Running);
            }
            return;
        }
        unsafe { (*self.heap).mutator_triggered_gc::<P>(&self.state) };
    }
}

impl<P: PtrPolicy> Drop for MutatorThread<P> {
    fn drop(&mut self) {
        unsafe { self.heap.safe_deregister_thread(&self.state) };
    }
}

#[cfg(test)]
mod poll_registration_tests {
    use super::*;

    #[test]
    fn host_frame_access_rejects_a_different_owner_thread() {
        let state = ThreadState::new();
        std::thread::scope(|scope| {
            let result = scope.spawn(|| state.frame_chain().depth()).join();
            assert!(result.is_err());
        });
        assert_eq!(state.frame_chain().depth(), 0);
    }

    #[test]
    fn poll_hint_publication_and_resume_order_plain_slots() {
        use std::cell::UnsafeCell;
        use std::sync::atomic::AtomicUsize;
        struct Slots(UnsafeCell<[usize; 2]>);
        impl Slots {
            unsafe fn read(&self) -> [usize; 2] {
                unsafe { *self.0.get() }
            }
            unsafe fn write(&self, value: [usize; 2]) {
                unsafe {
                    *self.0.get() = value;
                }
            }
        }
        // The owner writes while RUNNING; the coordinator accesses slots only
        // after the parked-state acquire, before the resume mutex release.
        unsafe impl Sync for Slots {}
        let slots = Slots(UnsafeCell::new([0, 0]));
        let ready = AtomicUsize::new(0);
        let state = ThreadState::new();
        let flag = AtomicU8::new(0);
        unsafe {
            state.set_poll_flag(&flag as *const AtomicU8 as *mut AtomicU8);
        }
        std::thread::scope(|scope| {
            // Borrow the owner's state; never transfer its thread-affine roots.
            let coordinator = scope.spawn(|| {
                let mut errors = 0;
                for round in 1..=256usize {
                    while ready.load(Ordering::Relaxed) < round {
                        std::thread::yield_now();
                    }
                    state.request_poll();
                    while !state.is_safely_at_safepoint() {
                        std::thread::yield_now();
                    }
                    if unsafe { slots.read() } != [round, !round] {
                        errors += 1;
                    }
                    unsafe {
                        slots.write([round + 1000, !(round + 1000)]);
                    }
                    // Clearing before resume is ordered by the resume mutex
                    // handshake, so an old hint cannot cause another park.
                    state.clear_poll();
                    unsafe {
                        state.resume();
                    }
                }
                errors
            });
            let mut errors = 0;
            for round in 1..=256usize {
                unsafe {
                    slots.write([round, !round]);
                }
                // Scheduling only: this deliberately does not publish slots.
                ready.store(round, Ordering::Relaxed);
                while flag.load(Ordering::Relaxed) == 0 {
                    std::thread::yield_now();
                }
                unsafe {
                    state.enter_safepoint();
                }
                if unsafe { slots.read() } != [round + 1000, !(round + 1000)] {
                    errors += 1;
                }
            }
            unsafe {
                state.set_poll_flag(std::ptr::null_mut());
            }
            assert_eq!(errors + coordinator.join().unwrap(), 0);
        });
    }

    #[test]
    fn request_before_flag_installation_is_not_lost() {
        let state = ThreadState::new();
        let flag = AtomicU8::new(0);
        state.request_poll();
        unsafe { state.set_poll_flag(&flag as *const AtomicU8 as *mut AtomicU8); }
        assert_eq!(flag.load(Ordering::Acquire), 1);
        state.clear_poll();
        assert_eq!(flag.load(Ordering::Acquire), 0);
    }

    #[test]
    fn cleared_request_is_not_replayed_on_late_installation() {
        let state = ThreadState::new();
        let flag = AtomicU8::new(0);
        state.request_poll();
        state.clear_poll();
        unsafe { state.set_poll_flag(&flag as *const AtomicU8 as *mut AtomicU8); }
        assert_eq!(flag.load(Ordering::Acquire), 0);
    }

    #[test]
    fn concurrent_registration_and_stop_request_always_raise_flag() {
        for _ in 0..64 {
            let state = ThreadState::new();
            let start = std::sync::Barrier::new(2);
            let flag = AtomicU8::new(0);
            std::thread::scope(|scope| {
                let requester = scope.spawn(|| { start.wait(); state.request_poll(); });
                start.wait();
                unsafe { state.set_poll_flag(&flag as *const AtomicU8 as *mut AtomicU8); }
                requester.join().unwrap();
            });
            assert_eq!(flag.load(Ordering::Acquire), 1);
        }
    }
}

#[cfg(test)]
mod trace_identity_counter_tests {
    use super::*;
    #[test]
    fn collector_thread_identity_exhaustion_does_not_wrap_or_mutate_counter() {
        let counter = AtomicUsize::new(usize::MAX - 1);
        assert_eq!(next_trace_id(&counter), usize::MAX - 1);
        assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);
        assert!(std::panic::catch_unwind(|| next_trace_id(&counter)).is_err());
        assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);
    }
    #[test]
    fn collector_thread_identity_concurrent_construction_is_unique() {
        let workers: Vec<_> = (0..8).map(|_| std::thread::spawn(|| {
            (0..128).map(|_| ThreadState::new().trace_id()).collect::<Vec<_>>()
        })).collect();
        let ids: std::collections::BTreeSet<_> = workers.into_iter().flat_map(|worker| worker.join().unwrap()).collect();
        assert_eq!(ids.len(), 1024);
    }
}

#[cfg(test)]
#[path = "thread_native_backing_tests.rs"]
mod native_backing_tests;
