//! Runtime support for compiled gc-rust code.
//!
//! This is the ABI boundary between LLVM-compiled gc-rust code and the
//! [`crate::gc`] collector. It is a deliberately *trimmed* descendant of
//! ai-lang's runtime: we keep only the pieces the GC protocol requires and
//! drop everything ai-lang-specific (content-addressed code tables, the
//! uniform-boxed-Int machinery, atom/prim-array shapes, closure code-hash
//! dispatch). gc-rust monomorphizes, so there is no uniform-boxed
//! representation to support here.
//!
//! ## What compiled code sees
//!
//! - [`Thread`] — passed as the first parameter of every compiled function.
//!   Holds the safepoint `state` byte, the head of the per-thread shadow-stack
//!   chain ([`Thread::top_frame`]), a pointer to the GC [`Heap`], and a pointer
//!   to the heap's inline-allocation [`AllocWindow`]. Compiled IR reads these
//!   at the fixed byte offsets in [`thread_offsets`].
//! - [`Frame`] / [`FrameOrigin`] — per-call shadow-stack frames. Compiled code
//!   alloca's a `{ Frame, [*mut u8; N] }` on the native stack, links it into
//!   the chain on entry, zeroes its root slots, and unlinks on return. The GC
//!   walks the chain to find live heap pointers — this is what makes our roots
//!   *precise* (no stack scanning, no false retention).
//!
//! ## How the GC finds our roots
//!
//! The collector can run only at a safepoint. Before a mutator does anything
//! that can trigger a collection (any `ai_gc_alloc_*`, any poll trap), it
//! publishes its current [`Thread::top_frame`] into the owning
//! [`ThreadState::set_parked_jit_fp`]. At collection time the GC reads each
//! parked thread's fp and invokes [`walk_gc_frames`] (registered via
//! [`Heap::set_jit_frame_walker`]) to scan the `Frame` chain. The
//! `state`/`top_frame`/`heap`/`alloc_window` layout is load-bearing ABI; the
//! `const _: ()` asserts below fail the build if it drifts.

mod completion_reaper;
mod completion_notice;
#[path = "runtime/handoff_storage.rs"]
pub(crate) mod handoff_storage;
pub(crate) mod native_memory;
#[cfg(test)]
#[path = "runtime/allocation_probe.rs"]
pub(crate) mod allocation_probe;
#[cfg(test)]
#[path = "runtime/channel_root_probe.rs"]
pub(crate) mod channel_root_probe;
#[path = "runtime/channel_sync.rs"]
mod channel_sync;
#[path = "runtime/channel_registry_growth.rs"]
mod channel_registry_growth;
#[path = "runtime/atomic_registry_growth.rs"]
mod atomic_registry_growth;
#[path = "runtime/registry_stage.rs"]
mod registry_stage;
#[path = "runtime/allocation_counter.rs"]
pub(crate) mod allocation_counter;
#[path = "runtime/native_metadata.rs"]
pub(crate) mod native_metadata;
#[path = "runtime/metadata_allocator.rs"]
pub(crate) mod metadata_allocator;
#[path = "runtime/registry_compaction.rs"]
pub(crate) mod registry_compaction;
#[path = "runtime/registry_teardown.rs"]
mod registry_teardown;
#[path = "runtime/completion_registry_growth.rs"]
mod completion_registry_growth;
#[cfg(test)]
#[path = "runtime/channel_remembered_tests.rs"]
mod channel_remembered_tests;
#[path = "runtime/native_retirement.rs"]
pub(crate) mod native_retirement;
#[path = "runtime/array_copy.rs"]
mod array_copy;
pub use array_copy::ai_array_copy;
mod slots;
pub use slots::{ai_slot_bits8_load, ai_slot_bits8_store, ai_slot_bits16_load, ai_slot_bits16_store,
    ai_slot_bits32_load, ai_slot_bits32_store, ai_slot_bits64_load, ai_slot_bits64_store, ai_slot_ref_store};
mod atom_scalar;
pub use atom_scalar::{
    ai_atom_bits8_load, ai_atom_bits8_cas, ai_atom_bits16_load, ai_atom_bits16_cas,
    ai_atom_bits32_load, ai_atom_bits32_cas, ai_atom_bits64_load, ai_atom_bits64_cas,
};
pub mod io;
pub(crate) mod managed;
pub use managed::{ai_managed_lock, ai_managed_read, ai_managed_try_write, ai_managed_unlock};

use crate::gc::{AllocWindow, Full, Heap, IdentityPtrPolicy, ThreadState, ThreadStateHandle, TypeInfo};
use std::cell::Cell;
use std::sync::Arc;

thread_local! {
    /// THIS thread's current mutator `Thread*`. Set when a thread's gc-rust entry
    /// starts (the main thread at program entry; a spawned thread in
    /// `ai_thread_spawn`). Read by FFI **callback trampolines**, which are plain C
    /// functions invoked by foreign code and therefore receive no `Thread*`
    /// argument — they recover it from here. Thread-local so each mutator gets its
    /// own (crucial once multiple threads run). See `docs/ffi.md`, `docs/threads.md`.
    static CURRENT_THREAD: Cell<*mut Thread> = const { Cell::new(core::ptr::null_mut()) };
}

/// Activate one synchronous managed execution and restore its previous binding.
/// Runtime-owned contexts are parked between entries. An active outer context
/// is suspended before an inner one runs, including while waiting for inner GC.
///
/// # Safety
/// `thread` is a non-null, live registration owned by this OS thread, either
/// RUNNING in an enclosing entry or BLOCKED/idle. Its Thread/heap storage stays
/// stable and registered through `execute`. Callbacks finish within this scope.
/// Do not access a suspended outer heap or mutate its roots; reload derived
/// pointers after returning. Raw ABI access outside an entry is not permitted.
pub unsafe fn with_current_thread<T>(thread: *mut Thread, execute: impl FnOnce() -> T) -> T {
    struct Restore {
        previous: *mut Thread,
        thread: *mut Thread,
        park: bool,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            abort_on_cleanup_panic(|| unsafe {
                if self.park {
                    let thread = &*self.thread;
                    let state = &*thread.dyna_thread;
                    state.set_parked_jit_fp(thread.top_frame as *const u8);
                    state.enter_blocked();
                }
                CURRENT_THREAD.with(|current| current.set(self.previous));
            });
        }
    }
    assert!(!thread.is_null(), "cannot bind a null mutator");
    let previous = CURRENT_THREAD.with(|current| current.get());
    let bind = || unsafe {
        let current = &*thread;
        let state = &*current.dyna_thread;
        let blocked = state.is_blocked();
        assert!(
            blocked || state.is_running(),
            "entry requires an idle or running mutator"
        );
        if blocked {
            state.exit_blocked(&*current.heap);
            state.clear_parked_jit_fp();
        }
        CURRENT_THREAD.with(|current| current.set(thread));
        let _restore = Restore {
            previous,
            thread,
            park: blocked || previous != thread,
        };
        execute()
    };
    if !previous.is_null()
        && previous != thread
        && !unsafe { (*(*previous).dyna_thread).is_blocked() }
    {
        unsafe { blocking_region(previous, bind) }
    } else {
        bind()
    }
}

/// This thread's current mutator `Thread*`. Used by callback trampolines. Returns
/// null if no gc-rust entry is on this thread's stack.
///
/// # Safety
/// The returned pointer is valid only while a gc-rust entry is on this thread's
/// stack.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_current_thread() -> *mut Thread {
    CURRENT_THREAD.with(|c| c.get())
}

// =============================================================================
// Thread
// =============================================================================

/// Per-thread state visible to compiled code. Passed as the first argument of
/// every compiled gc-rust function.
///
/// Accessed from LLVM IR by fixed byte offset — see [`thread_offsets`]. The
/// asserts below enforce the ABI.
#[repr(C)]
pub struct Thread {
    /// Safepoint flag. `0` = running normally. Non-zero means the GC has
    /// requested a safepoint; the next safepoint poll traps into
    /// [`ai_gc_pollcheck_slow`].
    pub state: std::sync::atomic::AtomicU8,
    _pad: [u8; 7],

    /// Head of this thread's shadow-stack chain. Each compiled function
    /// alloca's its own [`Frame`] on entry, links it here, and unlinks on
    /// return. The GC walks this to find live roots.
    pub top_frame: *mut Frame,

    /// Pointer to the GC heap. Compiled code calls `ai_gc_alloc_*`, which
    /// dereference this to allocate.
    pub heap: *mut Heap,

    /// Pointer to the [`ThreadState`] in the `gc` module that this `Thread`
    /// shadows. Used to publish/clear `parked_jit_fp` around allocations so the
    /// GC walks our chain, and to enter the safepoint on a poll trap.
    pub dyna_thread: *const ThreadState,

    /// Legacy shared-arena allocation window, retained in the runtime ABI.
    /// It is remapped under stop-the-world arena flips. Compiled fixed-object
    /// allocation uses the owning `tlab_window` below instead.
    pub alloc_window: *const AllocWindow,
    /// Execution-local argv, shared immutably with child mutators.
    pub arguments: Arc<Vec<std::ffi::OsString>>,
    /// Owning-mutator aggregate critical section; never live at a safepoint.
    managed_lock: Option<usize>,
    pub tlab_window: *mut crate::gc::InlineTlab,
    /// Owner-local epoch, advanced only under the world-pause handshake.
    /// Older generated callers may read it atomically; v3 callers read it
    /// ordinarily while RUNNING, when coordinator writes are excluded.
    pub relocation_epoch: *const std::sync::atomic::AtomicUsize,
    /// Heap-owned flag; generated access paths read it atomically while RUNNING.
    pub copying_phase: *const std::sync::atomic::AtomicU8,
    /// Heap-owned flag: nonzero once a second mutator registered.
    pub multi_mutator: *const std::sync::atomic::AtomicU8,
    /// The nursery's fixed address range (size 0 without generations):
    /// generated write barriers call the runtime only for a young value
    /// stored into an old object.
    pub nursery_base: usize,
    pub nursery_size: usize,
    // Owner-local ABI scope stack. Some = native/parked, None = callback is
    // managed. Never retain a mutable borrow across group restoration.
    foreign_scopes: std::cell::UnsafeCell<allocator_api2::vec::Vec<
        Option<crate::gc::OwnedPauseCallers>, metadata_allocator::MetadataAllocator,
    >>,
}

pub mod thread_offsets {
    //! Byte offsets within [`super::Thread`]. Mirrored in LLVM codegen.
    pub const STATE: usize = 0;
    pub const TOP_FRAME: usize = 8;
    pub const HEAP: usize = 16;
    pub const DYNA_THREAD: usize = 24;
    pub const ALLOC_WINDOW: usize = 32;
    pub const TLAB_WINDOW: usize = 64;
    pub const RELOCATION_EPOCH: usize = 72;
    pub const COPYING_PHASE: usize = 80;
    pub const MULTI_MUTATOR: usize = 88;
    pub const NURSERY_BASE: usize = 96;
    pub const NURSERY_SIZE: usize = 104;
}

const _: () = {
    assert!(core::mem::offset_of!(Thread, state) == thread_offsets::STATE);
    assert!(core::mem::offset_of!(Thread, top_frame) == thread_offsets::TOP_FRAME);
    assert!(core::mem::offset_of!(Thread, heap) == thread_offsets::HEAP);
    assert!(core::mem::offset_of!(Thread, dyna_thread) == thread_offsets::DYNA_THREAD);
    assert!(core::mem::offset_of!(Thread, alloc_window) == thread_offsets::ALLOC_WINDOW);
    assert!(core::mem::offset_of!(Thread, tlab_window) == thread_offsets::TLAB_WINDOW);
    assert!(core::mem::offset_of!(Thread, relocation_epoch) == thread_offsets::RELOCATION_EPOCH);
    assert!(core::mem::offset_of!(Thread, copying_phase) == thread_offsets::COPYING_PHASE);
    assert!(core::mem::offset_of!(Thread, multi_mutator) == thread_offsets::MULTI_MUTATOR);
    assert!(core::mem::offset_of!(Thread, nursery_base) == thread_offsets::NURSERY_BASE);
    assert!(core::mem::offset_of!(Thread, nursery_size) == thread_offsets::NURSERY_SIZE);
};

// =============================================================================
// Frame + FrameOrigin
// =============================================================================

/// Per-call shadow-stack frame header.
///
/// Compiled code allocates a `{ Frame, [*mut u8; N] }` on the native stack
/// (N = the function's GC-typed local/temporary count), links it into the
/// thread chain on entry, and unlinks on return.
///
/// Layout (load-bearing — matches LLVM IR `{ ptr, ptr, [N x ptr] }`):
///
/// ```text
/// offset 0  : parent  : *mut Frame
/// offset 8  : origin  : *const FrameOrigin
/// offset 16 : roots   : [*mut u8; N]   (variable, walked via origin.num_roots)
/// ```
#[repr(C)]
pub struct Frame {
    pub parent: *mut Frame,
    pub origin: *const FrameOrigin,
    // followed by [*mut u8; num_roots] starting at offset 16
}

pub mod frame_offsets {
    pub const PARENT: usize = 0;
    pub const ORIGIN: usize = 8;
    /// First root slot. `num_roots` slots of 8 bytes each follow.
    pub const ROOTS: usize = 16;
}

const _: () = {
    assert!(core::mem::offset_of!(Frame, parent) == frame_offsets::PARENT);
    assert!(core::mem::offset_of!(Frame, origin) == frame_offsets::ORIGIN);
    // The trailing root array is outside the struct (variable-length), so the
    // declared size is exactly the header.
    assert!(core::mem::size_of::<Frame>() == frame_offsets::ROOTS);
};

/// Static per-function descriptor. One emitted per compiled function as a
/// private constant global; each frame's `origin` field points at it. The GC
/// reads `num_roots` to know how many root slots follow the frame header, then
/// `num_indirect` *indirect* slots after them.
///
/// A **direct** root slot holds a GC pointer the collector reads and updates in
/// place. An **indirect** root slot holds the *address* of a GC pointer that
/// lives elsewhere on the stack — namely a reference embedded in a flattened
/// `#[value]` local's alloca (the alloca's interior). The collector dereferences
/// the indirect slot once to reach the real pointer and updates it in place.
/// This is how value-with-ref locals are kept precise across collections without
/// moving the value out of its alloca.
#[repr(C)]
pub struct FrameOrigin {
    pub num_roots: u32,
    pub num_indirect: u32,
    /// Function name, NUL-terminated, for debugging / GC tracing. May be null.
    pub name: *const u8,
}

impl FrameOrigin {
    pub const fn new(num_roots: u32, name: *const u8) -> Self {
        FrameOrigin {
            num_roots,
            num_indirect: 0,
            name,
        }
    }
}

// =============================================================================
// GC frame walker
// =============================================================================

/// Walk one parked thread's shadow-stack chain, visiting every live root slot.
///
/// Registered with the heap via [`Heap::set_jit_frame_walker`]. `jit_fp` is the
/// value the mutator published into its `ThreadState::parked_jit_fp` — i.e. the
/// top [`Frame`] of its chain. We follow `parent` links to the root of the
/// chain, and for each frame scan `origin.num_roots` pointer slots.
///
/// # Safety
/// Every non-null frame pointer must carry provenance for its whole allocation:
/// its header, direct root words and indirect pointer cells. Deriving a pointer
/// solely from a Rust reference to the header subfield is insufficient for the
/// trailing cells. Root words and indirect targets must be initialized, aligned,
/// writable and alive. The complete chain and descriptors must remain stable
/// for the duration of the call. The safepoint protocol guarantees
/// this: the owning mutator is parked with its chain published and is not
/// mutating it.
pub unsafe fn walk_gc_frames(jit_fp: *const u8, visitor: &mut dyn FnMut(*mut u64)) {
    let mut frame = jit_fp as *const Frame;
    while !frame.is_null() {
        unsafe {
            let origin = (*frame).origin;
            let (num_roots, num_indirect) = if origin.is_null() {
                (0, 0)
            } else {
                ((*origin).num_roots as usize, (*origin).num_indirect as usize)
            };
            // Direct root slots start at byte offset ROOTS past the frame header.
            let slots = (frame as *const u8).add(frame_offsets::ROOTS) as *mut u64;
            for i in 0..num_roots {
                visitor(slots.add(i));
            }
            // Indirect slots follow the direct ones: each holds the address of a
            // GC pointer inside a value-with-ref local's alloca. Deref once, then
            // visit that pointer's slot so the collector updates it in place.
            for j in 0..num_indirect {
                // These cells contain addresses, not encoded managed words.
                // Load a pointer so its provenance survives the frame handoff.
                let cell = slots.add(num_roots + j).cast::<*mut u64>();
                let target = cell.read();
                if !target.is_null() {
                    visitor(target);
                }
            }
            frame = (*frame).parent;
        }
    }
}

// =============================================================================
// RuntimeContext — owns the heap + the main mutator Thread
// =============================================================================

/// Owns a [`Heap`] and one registered mutator [`Thread`], wired together so
/// compiled code can run against it: the GC frame walker is installed, the
/// thread's safepoint poll flag points at the `Thread::state` byte, and the
/// `alloc_window` is pointed at the heap's inline-allocation window.
///
/// The `Heap` always uses the [`Full`] (16-byte) header — it carries the GC
/// word the copying collector parks forwarding pointers in — and the
/// [`IdentityPtrPolicy`] (root/field slot bits are raw pointers; `0` is the
/// null non-pointer). Codegen targets exactly this configuration.
pub struct RuntimeContext {
    thread: RuntimeThread,
}

/// Registration drops before the owned ABI storage. This also covers a failed
/// constructor or child setup, where an explicit trailing cleanup is skipped.
struct RuntimeRegistration {
    heap: Arc<Heap>,
    dyna: ThreadStateHandle,
    _owner: core::marker::PhantomData<std::rc::Rc<()>>,
}

fn abort_on_cleanup_panic<T>(cleanup: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(cleanup)) {
        Ok(value) => value,
        Err(payload) => {
            // Never free poll/frame storage or unload code after failed cleanup.
            std::mem::forget(payload);
            std::process::abort();
        }
    }
}

impl Drop for RuntimeRegistration {
    fn drop(&mut self) {
        abort_on_cleanup_panic(|| unsafe {
            self.heap.safe_deregister_runtime_thread(&self.dyna);
        });
    }
}

// Registration retires published poll/frame aliases before this allocation drops.
struct RuntimeThreadStorage {
    allocation: crate::abi_storage::AbiStorage<std::cell::UnsafeCell<Thread>>,
}
impl RuntimeThreadStorage {
    fn new(value: Thread, allocator: &metadata_allocator::MetadataAllocator) -> Self {
        Self {
            allocation: crate::abi_storage::AbiStorage::new_in(std::cell::UnsafeCell::new(value), allocator),
        }
    }
    fn get(&self) -> *mut Thread {
        std::cell::UnsafeCell::raw_get(self.allocation.as_raw())
    }
}

struct RuntimeThread {
    // Rust drops fields in declaration order. Keep registration first: an Arc
    // to ThreadState alone cannot keep the poll target allocation alive.
    registration: RuntimeRegistration,
    // Native/JIT code mutates ABI fields through its stable raw address even
    // when host code holds shared references to the owning runtime.
    thread: RuntimeThreadStorage,
}

impl Drop for RuntimeThread {
    fn drop(&mut self) {
        // Active ABI scopes retain published native stack frames. Retiring
        // their registration first would make later token restoration unsound.
        if !unsafe { &*(*self.thread.get()).foreign_scopes.get() }.is_empty() {
            std::process::abort();
        }
    }
}

impl core::ops::Deref for RuntimeThread {
    type Target = Thread;
    fn deref(&self) -> &Thread { unsafe { &*self.thread.get() } }
}
impl RuntimeThread {
    fn new(heap: Arc<Heap>, arguments: Arc<Vec<std::ffi::OsString>>) -> Self {
        let (dyna, _) = heap.register_thread();
        // Arm cleanup before any further initialization can unwind.
        let registration = RuntimeRegistration { heap, dyna, _owner: core::marker::PhantomData };
        let heap = &registration.heap;
        let dyna = &registration.dyna;
        let thread = RuntimeThreadStorage::new(Thread {
            state: std::sync::atomic::AtomicU8::new(0),
            _pad: [0; 7],
            top_frame: core::ptr::null_mut(),
            heap: Arc::as_ptr(heap) as *mut Heap,
            dyna_thread: ThreadStateHandle::as_ptr(dyna),
            alloc_window: heap.alloc_window_ptr(),
            arguments,
            managed_lock: None,
            tlab_window: unsafe { heap.configure_inline_tlab(dyna) },
            relocation_epoch: dyna.relocation_epoch_ptr(),
            copying_phase: heap.copying_phase_ptr(),
            multi_mutator: heap.multi_mutator_ptr(),
            nursery_base: heap.nursery_range().0,
            nursery_size: heap.nursery_range().1,
            foreign_scopes: std::cell::UnsafeCell::new(allocator_api2::vec::Vec::new_in(
                metadata_allocator::MetadataAllocator::new(&heap.native_memory.registry))),
        }, heap.native_memory.registry_allocator());
        let owner = Self { registration, thread };
        unsafe {
            owner.registration.dyna.set_poll_flag(&mut (*owner.thread.get()).state);
            // Construction finishes while RUNNING; only then may collectors
            // inspect this idle registration and its initialized TLAB storage.
            owner.registration.dyna.enter_blocked();
        }
        owner
    }
}

/// Owns the global env root even before a child has registered a mutator.
/// Once transferred, the child's scratch scope owns the reference instead.
struct ThreadHandoff {
    heap: Arc<Heap>,
    slot: Option<usize>,
}

impl ThreadHandoff {
    /// Parent is RUNNING; env is a live managed closure object on this heap.
    unsafe fn new(parent: *mut Thread, heap: Arc<Heap>, env: *mut u8) -> Self {
        let slot = unsafe { handoff_storage::admit(parent, &heap, env) };
        Self { heap, slot: Some(slot) }
    }

    /// Caller owns this RUNNING state and has opened a scratch scope that will
    /// outlive the managed call. No safepoint may occur during the transfer.
    unsafe fn transfer_to_scratch(&mut self, state: &ThreadState) -> usize {
        let index = self.slot.expect("handoff already transferred");
        let slot = unsafe { state.push_scratch(self.heap.globals.get(index) as *const u8) };
        unsafe { self.heap.globals.set(index, 0); }
        // Transfer ownership before publishing vacancy. If pool publication
        // fails, scratch still owns the env and unwinds before the mutator.
        self.slot = None;
        self.heap.thread_handoff_slots.lock().unwrap().release(index);
        slot
    }
}

impl Drop for ThreadHandoff {
    fn drop(&mut self) {
        if let Some(index) = self.slot.take() {
            // This can run on an unregistered child or during entry unwind.
            // Root stores outlive enumeration, so retire under coordinator
            // exclusion. Park any current entry before waiting; publish the
            // vacancy only after the clear is final.
            abort_on_cleanup_panic(|| {
                let current = CURRENT_THREAD.try_with(|slot| slot.get())
                    .unwrap_or(core::ptr::null_mut());
                unsafe {
                    blocking_region(current, || self.heap.clear_thread_handoff_root(index));
                }
                self.heap.thread_handoff_slots.lock().unwrap().release(index);
            });
        }
    }
}

impl RuntimeContext {
    /// Build a runtime over a single-generation semi-space heap of `space_size`
    /// bytes per space, with `type_table` describing every heap shape compiled
    /// code can allocate (`type_id` is the index into this table).
    pub fn new(space_size: usize, type_table: Vec<TypeInfo>) -> Self {
        Self::from_heap(Heap::new::<Full>(space_size, type_table))
    }

    /// Build a runtime over a GENERATIONAL heap: a `nursery_size`-byte young
    /// generation plus a `tenured_size`-byte (per-space) old generation.
    /// New allocations land in the nursery; a cheap minor GC scavenges it and
    /// promotes survivors to tenured; a major GC collects tenured when it fills.
    pub fn new_generational(nursery_size: usize, tenured_size: usize, type_table: Vec<TypeInfo>) -> Self {
        Self::from_heap(Heap::new_generational::<Full>(nursery_size, tenured_size, type_table))
    }

    /// Shared wiring: register the mutator thread, install the JIT frame walker,
    /// and point the safepoint poll flag. Used by both `new` and
    /// `new_generational`.
    pub(crate) fn from_heap(heap: Heap) -> Self {
        let heap = Arc::new(heap);
        // The GC walks our shadow-stack chain through this walker whenever a
        // parked thread has published its top frame.
        unsafe {
            heap.set_jit_frame_walker(walk_gc_frames);
        }

        Self { thread: RuntimeThread::new(heap, Arc::new(std::env::args_os().collect())) }
    }

    /// Set execution-local arguments before entering compiled code.
    pub fn set_arguments(&mut self, args: Vec<std::ffi::OsString>) {
        // Borrow only the owner-local field: collectors may write the poll
        // flag even while this context is idle.
        unsafe { (*self.thread.thread.get()).arguments = Arc::new(args); }
    }

    /// Borrow the ABI address. Native calls require an active `enter` or
    /// `with_current_thread` scope; obtaining the address does not activate it.
    pub fn thread_ptr(&mut self) -> *mut Thread {
        self.thread.thread.get()
    }

    /// Enter managed/native runtime work synchronously. The context is idle
    /// again on return or unwind. Root retained managed values separately.
    pub fn enter<R>(&mut self, execute: impl FnOnce(*mut Thread) -> R) -> R {
        let thread = self.thread_ptr();
        unsafe { with_current_thread(thread, || execute(thread)) }
    }

    pub fn heap(&self) -> &Arc<Heap> {
        &self.thread.registration.heap
    }

    /// Force a collection driven *by this mutator* — the model real compiled
    /// code uses when it hits allocation exhaustion. The calling thread becomes
    /// the GC thread (it parks every *other* registered mutator, excludes
    /// itself, and scans every parked thread's published frame chain). The
    /// caller MUST have published its own top frame into `parked_jit_fp` first
    /// (so its roots are scanned), via `thread`.
    ///
    /// # Safety
    /// `thread` must be this context's live mutator thread, with its current
    /// frame chain published. All live roots must already be in frame slots.
    pub unsafe fn force_collect(&self, thread: &Thread) {
        unsafe {
            with_current_thread(thread as *const Thread as *mut Thread, || {
                let dyna = &*thread.dyna_thread;
                dyna.set_parked_jit_fp(thread.top_frame as *const u8);
                self.heap().mutator_triggered_gc::<IdentityPtrPolicy>(dyna);
                dyna.clear_parked_jit_fp();
            });
        }
    }
}

impl RuntimeContext {
    /// Drain root-execution children, including descendants admitted by them.
    /// Unbound metadata is retained until this boundary; joined bound records
    /// may retire earlier once their managed owner is dead. Native OS handles
    /// are consumed once by the first observer. A cleanup failure aborts before
    /// returning to a caller that could unload still-executing code.
    ///
    /// # Safety
    /// No managed entry may be active on this context's main mutator. Its frame
    /// chain is retired here; callers must root any managed values they retain
    /// separately. Borrowed native handles from this execution (Thread,
    /// Channel, AtomicI64) must not be used after this boundary even if their
    /// managed wrappers remain rooted. The JIT code must stay loaded until this
    /// method returns.
    pub unsafe fn finish_threads(&mut self) {
        // JIT/AOT callers also invoke this explicitly before unloading code.
        // The fatal boundary must cover that call as well as automatic Drop.
        let thread = self.thread_ptr();
        abort_on_cleanup_panic(|| unsafe {
            // Shutdown can run inside another context's entry. Suspend that
            // entry before waiting for children that may collect its heap.
            with_current_thread(thread, || self.drain_threads());
        });
    }

    unsafe fn drain_threads(&mut self) {
        // main has returned: its generated frames no longer exist. Keep this
        // mutator registered but BLOCKED while draining children, so their GC
        // can proceed without scanning stale main frame addresses.
        let thread = self.thread_ptr();
        unsafe { (*thread).top_frame = core::ptr::null_mut(); }
        unsafe {
            blocking_region(thread, || {
                loop {
                    loop {
                        // Pop before joining: descendants can admit more work and
                        // GC can prune records without shifting a vector cursor.
                        let completion = self
                            .heap()
                            .thread_completions
                            .lock()
                            .unwrap()
                            .next_pending();
                        let Some(completion) = completion else {
                            break;
                        };
                        completion.join();
                    }
                    let reaper = self.heap().thread_completions.lock().unwrap().reaper.take();
                    if let Some(reaper) = reaper {
                        // Native worker TLS cleanup may admit more descendants.
                        // No registry or coordinator lock is held through this join.
                        drop(reaper);
                    } else {
                        break;
                    }
                }
            });
        }
        // Detach registry ownership without disposing backing under its locks.
        // Keep this caller and nested registrations BLOCKED through retirement,
        // physical table disposal, and the final native drain.
        unsafe {
            blocking_region(thread, || {
                registry_teardown::clear(self.heap());
                self.heap().native_retirement.shutdown();
                self.heap().native_memory.finish_execution();
            });
        }
    }
}


impl Drop for RuntimeContext {
    fn drop(&mut self) {
        unsafe { self.finish_threads(); }
        // RuntimeThread then retires registration before freeing ABI storage.
    }
}

// =============================================================================
// Threads: spawn / join
// =============================================================================

/// The ABI of a compiled no-arg gc-rust closure body: `(thread, env) -> u64`.
/// The u64 is a scalar ABI result, never traced or relocated. Managed results
/// must be published through traced storage such as ThreadResult<T>.
type ClosureFn = unsafe extern "C" fn(*mut Thread, *mut u8) -> u64;

/// Native records and their one-shot shutdown worklist. Pending keys carry no
/// additional strong reference and can be removed without scanning history.
pub(crate) struct CompletionRegistry {
    records: native_metadata::Map<Arc<JoinHandle>>,
    pending: native_metadata::Vec<usize>,
    owned: native_metadata::Vec<usize>,
    reaper: Option<completion_reaper::Reaper>,
}

impl CompletionRegistry {
    #[cfg(test)]
    fn ensure_reaper(&mut self) {
        if self.reaper.is_none() {
            self.reaper = Some(completion_reaper::Reaper::new());
        }
    }

    #[cfg(test)]
    fn register(&mut self, completion: Arc<JoinHandle>, retirement: &native_retirement::Retirement) {
        self.ensure_reaper();
        completion.retirement.get_or_init(|| native_retirement::RecordLink::new(retirement));
        self.reaper.as_ref().unwrap().monitor().prepare(&completion);
        self.records.reserve(1);
        self.pending.reserve(1);
        self.owned
            .reserve(self.records.len() + 1 - self.owned.len());
        self.register_reserved(completion);
    }

    fn register_reserved(&mut self, completion: Arc<JoinHandle>) {
        assert!(completion.retirement.get().is_some(), "completion retirement not prepared");
        assert!(self.can_register(), "completion capacity not prepared");
        let key = Arc::as_ptr(&completion) as usize;
        assert_eq!(
            completion.pending_index.load(Ordering::Relaxed),
            usize::MAX,
            "completion already pending"
        );
        completion
            .pending_index
            .store(self.pending.len(), Ordering::Relaxed);
        if completion.owner.load(std::sync::atomic::Ordering::Relaxed) != 1 {
            assert!(self.reaper.is_some(), "completion reaper not prepared");
            self.owned.push(key);
        }
        self.pending.push(key);
        assert!(
            self.records.insert(key, completion).is_none(),
            "completion registered twice"
        );
    }

    fn bind(&mut self, handle: *mut JoinHandle, owner: *mut u8) {
        assert!(self.reaper.is_some(), "completion reaper not prepared");
        assert!(
            self.owned.capacity() >= self.records.len(),
            "completion binding capacity not prepared"
        );
        use std::sync::atomic::Ordering;
        let key = handle as usize;
        let completion = self
            .records
            .get(&key)
            .expect("completion does not belong to this execution");
        completion
            .owner
            .compare_exchange(1, owner as usize, Ordering::Relaxed, Ordering::Relaxed)
            .expect("completion owner already bound");
        self.owned.push(key);
    }

    fn next_pending(&mut self) -> Option<Arc<JoinHandle>> {
        let key = self.pending.pop()?;
        let completion = self.records.get(&key).expect("pending completion missing");
        assert_eq!(
            completion.pending_index.swap(usize::MAX, Ordering::Relaxed),
            self.pending.len()
        );
        Some(completion.clone())
    }

    pub(crate) fn sweep(&mut self, mut keep: impl FnMut(&Arc<JoinHandle>) -> bool) {
        // Only managed-owner records need tracing. Legacy raw handles have no
        // managed edges; their retained history must not add GC pause work.
        let mut retired = native_retirement::CompletionBatch::default();
        let Self { records, pending, owned, reaper, .. } = self;
        owned.retain(|key| {
            let record = records.get(key).expect("owned completion missing");
            let retain = keep(record);
            if retain
                && !record.completion_notice
                && record.owner.load(std::sync::atomic::Ordering::Relaxed) == 0
            {
                reaper
                    .as_ref()
                    .expect("owned completion without a reaper")
                    .enqueue(record);
            }
            if !retain {
                if let Some(reaper) = reaper {
                    reaper.forget(record);
                }
                completion_registry_growth::remove_pending(records, pending, *key);
                let record = records.remove(key).expect("retired completion missing");
                // Exclusive registry removal transfers the prepared link once.
                unsafe { native_retirement::RecordLink::retire(record, &mut retired); }
            }
            retain
        });
    }

    fn clear(&mut self) {
        // Stopping a worker can run TLS cleanup. RuntimeContext detaches it and
        // joins outside this mutex first; clearing metadata never stops it.
        for key in &self.pending {
            self.records[key]
                .pending_index
                .store(usize::MAX, Ordering::Relaxed);
        }
        let mut retired = native_retirement::CompletionBatch::default();
        for (_, record) in self.records.drain() {
            unsafe { native_retirement::RecordLink::retire(record, &mut retired); }
        }
        drop(retired);
        self.pending.clear();
        self.owned.clear();
        self.records.shrink_to_fit();
        self.pending.shrink_to_fit();
        self.owned.shrink_to_fit();
    }
}

/// Native completion record. Raw spawn handles have execution lifetime;
/// bound handles can retire after owner death and observed or reaped native exit.
/// The OS handle is consumed exactly once, and
/// the scalar outcome is retained for every subsequent observer. Managed Thread
/// results remain in the traced ThreadResult cell, not in this native record.
pub struct JoinHandle {
    // Runtime admission guarantees a body-exit notification. Legacy records
    // without that capability use GC's orphan-index fallback instead.
    completion_notice: bool,
    // Monotone publication after full OS/TLS join, not merely body exit.
    // GC can observe completed state even while a native observer holds state.
    native_joined: std::sync::atomic::AtomicBool,
    // Registry mutex protects the dense pending-list position. Atomic storage
    // keeps the otherwise shared native completion Sync; readers outside the
    // registry never use this index. usize::MAX means already removed.
    pending_index: std::sync::atomic::AtomicUsize,
    reaper_registration: std::sync::OnceLock<completion_reaper::Registration>,
    retirement: std::sync::OnceLock<native_retirement::RecordLink<Self>>,
    state: Mutex<JoinState>,
    // 1 = legacy execution-owned; 0 = managed owner was collected; otherwise
    // the raw base of a weak managed owner, relocated only at GC pauses.
    owner: std::sync::atomic::AtomicUsize,
}

// SAFETY: completion destruction touches only native state. The private link
// transfers exactly once when the owning registry removes the record.
unsafe impl native_retirement::Record for JoinHandle {
    fn retirement(&self) -> &native_retirement::RecordLink<Self> {
        self.retirement.get().expect("completion retirement not prepared")
    }
    fn mailbox(state: &mut native_retirement::State) -> &mut native_retirement::RecordQueue<Self> {
        &mut state.completions
    }
}

enum JoinState {
    Running(std::thread::JoinHandle<u64>),
    Complete(u64),
    /// Only present while the observer holds the state mutex through OS join.
    Joining,
}

impl JoinHandle {
    fn is_complete(&self) -> bool {
        if self.native_joined.load(Ordering::Acquire) { return true; }
        let complete = self.state.try_lock()
            .is_ok_and(|state| matches!(*state, JoinState::Complete(_)));
        if complete { self.publish_joined(); }
        complete
    }

    fn publish_joined(&self) {
        if !self.native_joined.load(Ordering::Relaxed) {
            self.native_joined.store(true, Ordering::Release);
        }
    }

    /// Called with all mutators stopped, after copying and before source reset.
    /// Never wait for an observer that holds the state lock through OS join.
    pub(crate) fn relocate_owner_and_reclaimable(
        &self,
        mut relocate: impl FnMut(*mut u8) -> Option<*mut u8>,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let owner = self.owner.load(Ordering::Relaxed);
        if owner == 1 {
            return false;
        }
        let relocated = if owner == 0 {
            0
        } else {
            relocate(owner as *mut u8).map_or(0, |pointer| pointer as usize)
        };
        self.owner.store(relocated, Ordering::Relaxed);
        relocated == 0 && self.is_complete()
    }

    /// An unfinished native join defers cleanup if an observer holds state.
    /// Published terminal joins need no mutex. Only the execution's native
    /// worker calls this; OS/TLS exit can still block.
    fn try_join_finished(&self, claimed: impl FnOnce()) -> bool {
        if self.native_joined.load(Ordering::Acquire) { return true; }
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::WouldBlock) => return false,
            Err(error) => panic!("poisoned native completion: {error}"),
        };
        match &*state {
            JoinState::Complete(_) => { self.publish_joined(); true },
            JoinState::Running(handle) if handle.is_finished() => {
                claimed();
                self.finish_join(&mut state);
                true
            }
            JoinState::Running(_) => false,
            JoinState::Joining => unreachable!("unowned native join transition"),
        }
    }

    fn finish_join(&self, state: &mut JoinState) -> u64 {
        if let JoinState::Complete(result) = *state {
            self.publish_joined();
            return result;
        }
        let JoinState::Running(handle) = core::mem::replace(state, JoinState::Joining) else {
            unreachable!()
        };
        let result = handle.join().unwrap_or(0);
        *state = JoinState::Complete(result);
        self.publish_joined();
        result
    }

    pub(crate) fn join(&self) -> u64 {
        // Managed observers block before taking the state lock. The native
        // reaper uses the same one-shot transition, outside collection.
        self.finish_join(&mut self.state.lock().unwrap())
    }
}

/// Spawn a new OS thread that runs gc-rust closure code against the SAME heap as
/// `parent`. Codegen passes the closure's `env` object (a GC pointer) and its
/// `code` pointer (read from the env by the caller, which knows the layout). The
/// child registers its own mutator `Thread` (so stop-the-world can pause it and
/// the collector scans its roots), publishes its thread-local current thread,
/// invokes `code(child_thread, env)`, captures the `u64` result, and deregisters.
/// Body exit notifies the execution-owned reaper. Native OS/TLS exit is joined
/// outside collector/registry locks and its scalar result is cached for later
/// observers, including live managed owners and raw handles without another GC.
/// Returns an opaque `*mut JoinHandle`.
///
/// # Safety
/// `parent` must be the caller's live mutator thread; `env` a valid closure env
/// object; `code` its body with ABI `(thread, env) -> u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_thread_spawn(
    parent: *mut Thread,
    env: *mut u8,
    code: *const u8,
) -> *mut JoinHandle {
    debug_assert!(
        unsafe { &*(*parent).dyna_thread }.is_running(),
        "spawn requires an active runtime entry"
    );
    let heap = unsafe { clone_thread_heap(&*parent) };
    // Hand off the env object via a GLOBAL ROOT slot. This runs on the PARENT
    // thread while `env` is still live in the parent's frame, so registering it
    // as a global root keeps it alive and relocated across any GC during the
    // handoff — closing the race where the child reads a stale (moved) pointer.
    // The child reads the (relocated) env via `get`, then clears the slot.
    let handoff = unsafe { ThreadHandoff::new(parent, heap.clone(), env) };
    // The handoff root, not the original env address, survives pressure GC.
    unsafe { service_native_pressure(parent, 0) };
    let code_bits = code as usize;
    // The active main entry or admitted parent covers child publication and
    // native TLS exit. Shutdown cannot close this monitor mid-admission; worker
    // TLS admissions obtain the next execution-owned monitor after detachment.
    let arguments = unsafe { (*parent).arguments.clone() };
    let _heap_id = Arc::as_ptr(&heap) as usize;
    let (notice, completion) = unsafe {
        completion_registry_growth::create(parent, &heap, handoff, arguments, code_bits)
    };
    let address = Arc::as_ptr(&completion) as *mut JoinHandle;
    unsafe {
        completion_registry_growth::publish(parent, &heap, completion.clone());
    }
    unsafe {
        blocking_region(parent, || {
            #[cfg(test)]
            completion_registry_growth::checkpoint(
                _heap_id,
                completion_registry_growth::Phase::NoticePublish,
            );
            notice.publish(&completion)
        });
    }
    address
}

/// Bind a newly spawned native completion to a weak managed lifetime owner.
/// Safe Thread<T> uses its traced wrapper identity. Binding does not make it a
/// root. After owner death and observed or reaped native exit, GC may reclaim it.
///
/// # Safety
/// Caller is RUNNING on this heap. `handle` is an unbound handle issued by
/// ai_thread_spawn in this execution; `owner` is a live managed object base in
/// the same heap. Binding is single-use. Every subsequent raw use of the handle
/// must keep this owner strongly rooted across the call and all safepoints.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_thread_bind_owner(
    thread: *mut Thread, handle: *mut JoinHandle, owner: *mut u8,
) {
    assert!(!owner.is_null() && owner as usize != 1, "invalid completion owner");
    // Both lookup and weak-owner indexing are constant-time. The caller stays
    // RUNNING through this critical section, so GC cannot see a partial bind.
    let heap = unsafe { &*(*thread).heap };
    heap.thread_completions.lock().unwrap().bind(handle, owner);
}

/// Run `body` while this mutator is transitioned to `STATE_BLOCKED`, so a
/// stop-the-world collection on another thread can proceed *without* this thread
/// reaching a safepoint (a blocked thread counts as already-parked, and is
/// scanned in place). Without this, any blocking call (join, sleep) that outlasts
/// another thread's GC trigger DEADLOCKS: the collector spins waiting for a
/// safepoint this thread — parked in libc — will never poll.
///
/// We PUBLISH the caller's current JIT frame chain (`top_frame`) before blocking,
/// so the collector scans the joining/sleeping thread's live GC roots (its
/// caller's spill slots — e.g. `main`'s `Vec` locals across `t.join()`). Compiled
/// code spills live roots into frame slots before any call, so `top_frame` is a
/// complete root set. Without this, a GC during the block would miss those roots
/// and the thread would read relocated-away pointers on resume.
///
/// # Safety
/// `thread` must be this thread's live RUNNING mutator with a valid heap/dyna.
/// The body must not mutate managed storage or publish managed references while
/// BLOCKED. Retiring an exclusively owned handoff root is allowed only under
/// collection-coordinator exclusion, after all earlier forwarding stores finish.
/// Diagnostic reads require a WorldPause guard; callbacks must explicitly reenter
/// managed state. Derived managed pointers must be reloaded after this call.
unsafe fn blocking_region<R>(thread: *mut Thread, body: impl FnOnce() -> R) -> R {
    unsafe {
        if thread.is_null() {
            return body();
        }
        let t = &*thread;
        let dyna = &*t.dyna_thread;
        let heap = &*t.heap;
        // Preserve an enclosing blocked scope's published frame. Other
        // RUNNING registrations on this OS thread must still be suspended.
        let publish = !dyna.is_blocked();
        if publish { dyna.set_parked_jit_fp(t.top_frame as *const u8); }
        struct ClearFrame<'a> { state: &'a ThreadState, published: bool }
        impl Drop for ClearFrame<'_> {
            fn drop(&mut self) {
                if self.published { unsafe { self.state.clear_parked_jit_fp(); } }
            }
        }
        // Group restoration (including unwind) finishes before the frame is
        // cleared. The heap helper retains ownership of preblocked scopes.
        let _frame = ClearFrame { state: dyna, published: publish };
        heap.with_current_registrations_blocked(body)
    }
}

/// Observe a spawned thread, returning the same `u64` result on every join.
/// Transitions the joining thread to BLOCKED so a collection triggered by the
/// child (or any other thread) can proceed while we wait.
///
/// # Safety
/// `thread` must be the caller's live mutator thread; `handle` a `*mut JoinHandle`
/// from `ai_thread_spawn` in the same active root execution (before
/// `RuntimeContext::finish_threads`). If bound with `ai_thread_bind_owner`, its
/// managed owner must remain strongly rooted across this call. Aliases may join
/// concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_thread_join(thread: *mut Thread, handle: *mut JoinHandle) -> u64 {
    unsafe { blocking_region(thread, || (&*handle).join()) }
}

/// `Thread.sleep(ms)`. Transitions to BLOCKED so a sleeping mutator doesn't stall
/// every other thread's GC for the sleep duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_thread_sleep(thread: *mut Thread, millis: i64) {
    if millis > 0 {
        unsafe {
            blocking_region(thread, || {
                std::thread::sleep(std::time::Duration::from_millis(millis as u64));
            });
        }
    }
}

/// `Thread.yield_now()`.
#[unsafe(no_mangle)]
pub extern "C" fn ai_thread_yield(_thread: *mut Thread) {
    std::thread::yield_now();
}

// =============================================================================
use std::sync::{Condvar, Mutex, RwLock};

// Channels — stable native metadata and conditional traced queue slots.
// =============================================================================

struct ChanState {
    head: usize,
    tail: usize,
    count: usize,
    closed: bool,
    senders: usize,
    waiting_senders: usize,
    waiting_receivers: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct ChannelOwner {
    pub address: usize,
    pub type_id: u16,
}

pub(crate) struct ChannelRegistry {
    records: native_metadata::Map<(Arc<ChanCtrl>, Option<u16>)>,
    unbound: native_metadata::Set,
    owned: native_metadata::Set,
}
impl ChannelRegistry {
    // Synthetic test registries have no mutator transition. Production uses
    // staged native growth followed by register_reserved instead.
    #[cfg(test)]
    fn register(&mut self, channel: Arc<ChanCtrl>, memory: &Arc<native_memory::NativeMemory>) -> *mut ChanCtrl {
        self.records.reserve(1);
        self.unbound.reserve(1);
        self.owned.reserve(self.records.len() + 1 - self.owned.len());
        self.register_reserved(channel, memory)
    }
    fn register_reserved(&mut self, channel: Arc<ChanCtrl>, memory: &Arc<native_memory::NativeMemory>) -> *mut ChanCtrl {
        assert!(self.can_register(), "channel registration capacity not prepared");
        if let Some(slots) = channel.slots.write().unwrap().as_mut() {
            slots.register(memory);
        }
        let pointer = Arc::as_ptr(&channel).cast_mut();
        assert!(
            self.records
                .insert(pointer as usize, (channel, None))
                .is_none()
        );
        self.unbound.insert(pointer as usize);
        pointer
    }
    fn bind(&mut self, channel: *const ChanCtrl, owner: ChannelOwner) {
        let (control, slot) = self
            .records
            .get_mut(&(channel as usize))
            .expect("channel from another execution");
        assert!(slot.is_none(), "channel owner already bound");
        *slot = Some(owner.type_id);
        control.owner.store(owner.address, Ordering::Release);
        self.unbound.remove(&(channel as usize));
        self.owned.insert(channel as usize);
    }
    pub(crate) fn has_owned(&self) -> bool {
        !self.owned.is_empty()
    }
    pub(crate) fn owned_iter(&self) -> impl ExactSizeIterator<Item = (ChannelOwner, &Arc<ChanCtrl>)> {
        self.owned
            .iter()
            .map(|key| {
                let (channel, owner) = &self.records[key];
                (
                    ChannelOwner {
                        address: channel.owner.load(Ordering::Acquire),
                        type_id: owner.unwrap(),
                    },
                    channel,
                )
            })
    }
    pub(crate) unsafe fn scan_unbound_roots(&self, visitor: &mut dyn FnMut(*mut u64), minor: bool) {
        for key in &self.unbound {
            let channel = &self.records[key].0;
            if !minor || channel.needs_minor_scan() {
                unsafe { channel.scan_roots(visitor); }
            }
        }
    }
    fn retire_control(control: Arc<ChanCtrl>, batch: &mut native_retirement::ChannelBatch) {
        unsafe { native_retirement::RecordLink::retire(control, batch); }
    }
    // Called only by the native worker, without registry/coordinator locks.
    fn destroy_retired_control(control: Arc<ChanCtrl>, batch: &mut native_retirement::Batch) {
        match Arc::try_unwrap(control) {
            Ok(mut control) => {
                if let Some(slots) = control.slots.get_mut().unwrap().take() {
                    slots.retire_into(batch);
                }
            }
            // Retained observers own storage until their final native drop.
            // A racing last drop still uses NativeSlots' individual handoff.
            Err(control) => drop(control),
        }
    }
    pub(crate) fn sweep(&mut self, relocate: impl FnMut(*mut u8) -> Option<*mut u8>) {
        // No remembered state is cleared by a major or synthetic sweep.
        unsafe { self.sweep_inner(relocate, false) }
    }
    /// # Safety
    /// World stopped and all minor-copy jobs completed. Relocation must retire
    /// dead nursery owners and retain only queues whose direct values are old.
    pub(crate) unsafe fn sweep_minor(&mut self, relocate: impl FnMut(*mut u8) -> Option<*mut u8>) {
        unsafe { self.sweep_inner(relocate, true) }
    }
    /// # Safety
    /// A true minor flag requires the sweep_minor completion contract.
    unsafe fn sweep_inner(&mut self, mut relocate: impl FnMut(*mut u8) -> Option<*mut u8>, minor: bool) {
        let Self {
            records,
            owned,
            unbound,
        } = self;
        let mut batch = native_retirement::ChannelBatch::default();
        owned.retain(|key| {
            let (control, _) = records.get_mut(key).expect("owned channel missing");
            if let Some(moved) = relocate(control.owner.load(Ordering::Acquire) as *mut u8) {
                control.owner.store(moved as usize, Ordering::Release);
                if minor { unsafe { control.scan.minor_completed(); } }
                true
            } else {
                control.owner.store(0, Ordering::Release);
                let (control, _) = records.remove(key).unwrap();
                Self::retire_control(control, &mut batch);
                false
            }
        });
        // Raw queues are unconditional execution roots. Their slots have also
        // completed minor evacuation, but their records must never retire here.
        if minor {
            for key in unbound.iter() {
                unsafe { records[key].0.scan.minor_completed(); }
            }
        }
        // Unbound channels may bind after this collection without allocating.
        // Retain their reservation while releasing historical burst capacity.
    }
    fn clear(&mut self) {
        let mut batch = native_retirement::ChannelBatch::default();
        for (_, (control, _)) in self.records.drain() {
            Self::retire_control(control, &mut batch);
        }
        self.owned.clear();
        self.unbound.clear();
        self.records.shrink_to_fit();
        self.owned.shrink_to_fit();
        self.unbound.shrink_to_fit();
    }
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

pub struct ChanCtrl {
    retirement: native_retirement::RecordLink<Self>,
    // Weak managed address; 1 denotes an unbound execution-owned queue.
    owner: std::sync::atomic::AtomicUsize,
    state: Mutex<ChanState>,
    scan: channel_scan::QueueRange,
    not_empty: Condvar,
    not_full: Condvar,
    // Fixed addresses traced while stopped, conditional on a managed owner
    // when bound; unbound raw channels retain unconditional execution roots.
    slots: RwLock<Option<native_memory::NativeSlots>>,
    cap: usize,
    array_type: u16,
    message_type: u16,
}

impl ChanCtrl {
    pub(crate) fn needs_minor_scan(&self) -> bool {
        self.scan.needs_minor_scan()
    }
    /// Caller holds a world pause; mutators cannot publish or retire slots.
    pub(crate) unsafe fn scan_roots(&self, visitor: &mut dyn FnMut(*mut u64)) {
        // Slot publication and range updates are a single safepoint-free
        // RUNNING transaction under state. A blocked waiter may hold state,
        // so collector access must never acquire that mutex.
        let slots = self.slots.read().unwrap();
        if let Some(slots) = slots.as_ref() {
            unsafe { self.scan.visit(slots, visitor) };
        }
    }

    unsafe fn validate(&self, thread: *mut Thread, witness: *const u8) {
        if unsafe { ai_object_type_id(thread, witness) } != self.array_type {
            unsafe {
                abort_with_diagnostic(thread, b"gc-rust: channel handle/message type mismatch")
            }
        }
    }
}

/// Allocate a positive-capacity native queue, registered before publication.
/// Witness arrays establish the generic type only; their contents/capacity do
/// not own queue storage. Native slots are roots while unbound; binding makes
/// them conditional owner edges, independent of managed array storage.
/// # Safety
/// Running owning mutator; witness is a live array of the supplied message type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_chan_new(
    thread: *mut Thread,
    cap: i64,
    witness: *const u8,
    message_type: u32,
) -> *mut ChanCtrl {
    if cap <= 0 || (cap as u64) > (isize::MAX as u64 / 8) {
        let message =
            format!("gc-rust: channel capacity must be positive and representable: {cap}");
        unsafe { abort_with_diagnostic(thread, message.as_bytes()) }
    }
    let heap = unsafe { &*(*thread).heap };
    let dyna = unsafe { &*(*thread).dyna_thread };
    let roots = unsafe { dyna.scratch_scope() };
    let witness_root = unsafe { roots.push(witness as *mut u8) };
    unsafe {
        dyna.set_parked_jit_fp((*thread).top_frame as *const u8);
        heap.mutator_gc_for_native_allocations::<IdentityPtrPolicy>(dyna, cap as usize * 8);
        dyna.clear_parked_jit_fp();
    }
    // Only scalar type metadata enters native allocation. The witness remains
    // rooted through construction, but is never dereferenced while BLOCKED.
    let array_type = unsafe { ai_object_type_id(thread, roots.get(witness_root)) };
    let charge = heap.native_memory.charge(cap as usize * 8);
    let retirement = &heap.native_retirement;
    #[cfg(test)]
    let heap_id = heap as *const crate::gc::Heap as usize;
    let construct = move || {
        #[cfg(test)]
        channel_buffer::tests::checkpoint(heap_id, false);
        channel_sync::construct(cap as usize, array_type, message_type as u16, charge, retirement)
    };
    // Native synchronization can lazily allocate even for a one-slot queue.
    // Complete all control/backing initialization before RUNNING publication.
    let channel = unsafe { blocking_region(thread, construct) };
    unsafe { channel_registry_growth::publish(thread, heap, channel) }
}

/// Bind a channel to a weak managed owner. Queue edges are conditional on that
/// owner, so queued self/cross-channel cycles do not become permanent roots.
/// # Safety
/// RUNNING owning mutator; fresh unbound channel and live owner on this heap.
/// Bind before publishing the handle or beginning concurrent operations.
/// Every subsequent raw use must keep the owner rooted across all safepoints.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_chan_bind_owner(
    thread: *mut Thread,
    channel: *const ChanCtrl,
    owner: *mut u8,
) {
    assert!(!owner.is_null(), "invalid channel owner");
    let heap = unsafe { &*(*thread).heap };
    let type_id = unsafe { ai_object_type_id(thread, owner) };
    heap.channels.lock().unwrap().bind(
        channel,
        ChannelOwner {
            address: owner as usize,
            type_id,
        },
    );
}

/// A valid borrowed handle enters while RUNNING, excluding weak retirement.
/// Pin its owner before any safepoint; this keeps native state and conditional
/// queue edges alive through blocking operations without a registry lookup.
unsafe fn with_channel<R>(
    thread: *mut Thread,
    ctrl: *const ChanCtrl,
    body: impl FnOnce(&ChanCtrl) -> R,
) -> R {
    let t = unsafe { &*thread };
    let dyna = unsafe { &*t.dyna_thread };
    let roots = unsafe { dyna.scratch_scope() };
    let control = unsafe { &*ctrl };
    let owner = control.owner.load(Ordering::Acquire);
    if owner == 0 {
        unsafe { abort_with_diagnostic(thread, b"gc-rust: expired channel handle") }
    }
    if owner != 1 {
        unsafe {
            roots.push(owner as *const u8);
        }
    }
    body(control)
}

/// # Safety
/// Owning live mutator and live channel handle. A bound owner must be live
/// at entry; this call roots it before any blocking transition.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_chan_sender_clone(thread: *mut Thread, ctrl: *const ChanCtrl) {
    unsafe {
        with_channel(thread, ctrl, |c| {
            let mut state = blocking_region(thread, || c.state.lock().unwrap());
            state.senders = state
                .senders
                .checked_add(1)
                .expect("channel sender count overflow");
        })
    }
}

/// Idempotent explicit close: wake both kinds of waiter. Queued values drain.
/// # Safety
/// Owning live mutator and live channel handle. A bound owner must be live
/// at entry; this call roots it before any blocking transition.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_chan_close(thread: *mut Thread, ctrl: *const ChanCtrl) {
    unsafe {
        with_channel(thread, ctrl, |c| {
            let mut state = blocking_region(thread, || c.state.lock().unwrap());
            state.closed = true;
            let retired = if state.count == 0 { c.slots.write().unwrap().take() } else { None };
            c.not_empty.notify_all();
            c.not_full.notify_all();
            drop(state);
            channel_buffer::retire(thread, retired);
        })
    }
}

/// Queue diagnostics used to observe blocked participants, under the same lock.
/// # Safety
/// Live owning mutator and live channel handle; a bound owner is live at entry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_chan_waiting_senders(
    thread: *mut Thread,
    ctrl: *const ChanCtrl,
) -> i64 {
    unsafe {
        with_channel(thread, ctrl, |c| {
            blocking_region(thread, || c.state.lock().unwrap()).waiting_senders as i64
        })
    }
}
/// # Safety
/// As ai_chan_waiting_senders.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_chan_waiting_receivers(
    thread: *mut Thread,
    ctrl: *const ChanCtrl,
) -> i64 {
    unsafe {
        with_channel(thread, ctrl, |c| {
            blocking_region(thread, || c.state.lock().unwrap()).waiting_receivers as i64
        })
    }
}

/// # Safety
/// As ai_chan_close; each sender registration may be retired once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_chan_sender_drop(thread: *mut Thread, ctrl: *const ChanCtrl) {
    unsafe {
        with_channel(thread, ctrl, |c| {
            let mut state = blocking_region(thread, || c.state.lock().unwrap());
            if state.senders > 0 {
                state.senders -= 1;
            }
            let mut retired = None;
            if state.senders == 0 {
                state.closed = true;
                if state.count == 0 {
                    retired = c.slots.write().unwrap().take();
                }
                c.not_empty.notify_all();
                c.not_full.notify_all();
            }
            drop(state);
            channel_buffer::retire(thread, retired);
        })
    }
}

/// Return 0 on commitment, 1 on closed (the caller retains the unsent value).
/// FIFO linearization occurs under the state lock when the slot is installed.
/// # Safety
/// Live owning mutator, witness array, handle and initialized message box.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_chan_send(
    thread: *mut Thread,
    witness: *mut u8,
    ctrl: *const ChanCtrl,
    value: *mut u8,
) -> i64 {
    unsafe {
        with_channel(thread, ctrl, |c| {
            c.validate(thread, witness);
            let t = &*thread;
            let heap = &*t.heap;
            if value.is_null() || ai_object_type_id(thread, value) != c.message_type {
                abort_with_diagnostic(thread, b"gc-rust: channel message type mismatch");
            }
            let dyna = &*t.dyna_thread;
            let roots = dyna.scratch_scope();
            let vslot = roots.push(value);
            let mut state = blocking_region(thread, || c.state.lock().unwrap());
            while state.count == c.cap && !state.closed {
                state = blocking_region(thread, || {
                    state.waiting_senders += 1;
                    let mut state = c.not_full.wait(state).unwrap();
                    state.waiting_senders -= 1;
                    state
                });
            }
            if state.closed {
                return 1;
            }
            {
                let slots = c.slots.read().unwrap();
                if heap.is_nursery(roots.get(vslot)) {
                    c.scan.remember_young();
                }
                slots.as_ref().expect("open channel has storage")[state.tail]
                    .store(roots.get(vslot) as u64, Ordering::SeqCst);
            }
            state.tail = (state.tail + 1) % c.cap;
            state.count += 1;
            c.scan.publish(state.head, state.count);
            c.not_empty.notify_one();
            0
        })
    }
}

/// Return null only for closed+drained. Popped values transfer from the native
/// slot root to a scratch root during collecting native cleanup, then to the
/// running caller. No popped value is exposed unrooted across a safepoint.
/// # Safety
/// Live owning mutator, witness array and channel handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_chan_recv(
    thread: *mut Thread,
    witness: *mut u8,
    ctrl: *const ChanCtrl,
) -> *mut u8 {
    unsafe {
        with_channel(thread, ctrl, |c| {
            c.validate(thread, witness);
            let t = &*thread;
            let dyna = &*t.dyna_thread;
            let mut state = blocking_region(thread, || c.state.lock().unwrap());
            while state.count == 0 && !state.closed {
                state = blocking_region(thread, || {
                    state.waiting_receivers += 1;
                    let mut state = c.not_empty.wait(state).unwrap();
                    state.waiting_receivers -= 1;
                    state
                });
            }
            if state.count == 0 {
                return core::ptr::null_mut();
            }
            let value = {
                let slots = c.slots.read().unwrap();
                slots.as_ref().expect("queued channel has storage")[state.head]
                    .swap(0, Ordering::SeqCst) as *mut u8
            };
            assert!(!value.is_null(), "committed channel slot is empty");
            state.head = (state.head + 1) % c.cap;
            state.count -= 1;
            c.scan.publish(state.head, state.count);
            let retired = if state.closed && state.count == 0 { c.slots.write().unwrap().take() } else { None };
            c.not_full.notify_one();
            drop(state);
            if retired.as_ref().is_some_and(|slots| slots.len() * 8 > string_copy::COPY_BYTES) {
                // The popped value has left the queue's root set. Keep it
                // reachable and refresh it after large native deallocation.
                let roots = dyna.scratch_scope();
                let received = roots.push(value);
                channel_buffer::retire(thread, retired);
                roots.get(received)
            } else {
                drop(retired);
                value
            }
        })
    }
}

// =============================================================================
// AtomicI64 — a lock-free shared integer cell
// =============================================================================
//
// The cell lives OFF the GC heap in an Arc, so its address is stable: the
// relocating collector never moves it, and concurrent CAS/fetch-add operate on a
// fixed location. The gc-rust handle is a `RawPtr` to this `AtomicI64`. (Scalars
// aren't GC pointers, so the cell needs no root registration.) Bound safe cells
// weakly follow their GC wrapper; raw unbound cells live until root shutdown.
// Load/store/CAS/fetch-add neither allocate, park nor safepoint. The compiler's
// native_atomic_is_nonrelocating classification relies on this contract.

use std::sync::atomic::{AtomicI64, Ordering};

/// Native cells have stable addresses. Only bound managed-owner keys need GC
/// work; legacy raw handles remain execution-owned without per-GC traversal.
pub(crate) struct AtomicCellRegistry {
    records: native_metadata::Map<(Arc<native_retirement::AtomicCell>, Option<usize>)>,
    // Binding is one-shot; sweeping is the only removal. Dense iteration keeps
    // scan work proportional to owners even when raw cells reserve spare space.
    owned: native_metadata::Vec<usize>,
}
impl AtomicCellRegistry {
    #[cfg(test)]
    fn register(&mut self, cell: Arc<native_retirement::AtomicCell>) -> *mut AtomicI64 {
        self.records.reserve(1);
        self.owned
            .reserve(self.records.len() + 1 - self.owned.len());
        self.register_reserved(cell)
    }
    fn register_reserved(&mut self, cell: Arc<native_retirement::AtomicCell>) -> *mut AtomicI64 {
        assert!(self.can_register(), "atomic registry capacity not prepared");
        let pointer = cell.pointer();
        assert!(
            self.records
                .insert(pointer as usize, (cell, None))
                .is_none()
        );
        pointer
    }
    fn bind(&mut self, pointer: *mut AtomicI64, owner: *mut u8) {
        assert!(
            self.owned.capacity() >= self.records.len(),
            "atomic binding capacity not prepared"
        );
        let (_, slot) = self
            .records
            .get_mut(&(pointer as usize))
            .expect("atomic cell from another execution");
        assert!(slot.is_none(), "atomic owner already bound");
        *slot = Some(owner as usize);
        self.owned.push(pointer as usize);
    }
    pub(crate) fn sweep(&mut self, mut relocate: impl FnMut(*mut u8) -> Option<*mut u8>) {
        let Self { records, owned } = self;
        let mut retired = native_retirement::AtomicBatch::default();
        owned.retain(|key| {
            let (_, owner) = records.get_mut(key).expect("owned atomic missing");
            if let Some(moved) = relocate(owner.unwrap() as *mut u8) {
                *owner = Some(moved as usize);
                true
            } else {
                let (cell, _) = records.remove(key).unwrap();
                unsafe { cell.retire(&mut retired); }
                false
            }
        });
        // Every unbound record can bind later without allocating or parking.
        // Dense iteration never scans this unused reservation during GC.
    }
    fn clear(&mut self) {
        let mut retired = native_retirement::AtomicBatch::default();
        for (_, (cell, _)) in self.records.drain() {
            unsafe { cell.retire(&mut retired); }
        }
        self.owned.clear();
        self.records.shrink_to_fit();
        self.owned.shrink_to_fit();
    }
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// Native constructors must enter with all managed roots published. Registry
/// growth is charged at allocation; the next constructor services accumulated
/// demand, including requests that need no further backing growth. This is a
/// growth trigger rather than a memory cap.
unsafe fn service_native_pressure(thread: *mut Thread, additional: usize) {
    unsafe {
        let heap = &*(*thread).heap;
        let state = &*(*thread).dyna_thread;
        state.set_parked_jit_fp((*thread).top_frame as *const u8);
        heap.mutator_gc_for_native_allocations::<IdentityPtrPolicy>(state, additional);
        state.clear_parked_jit_fp();
    }
}

/// `AtomicI64::new(v)` → a borrowed pointer to a stable execution-owned cell.
/// All aliases remain valid until root shutdown has drained every child.
///
/// # Safety
/// `thread` must be a live mutator in the owning root execution.
/// This constructor may park and relocate managed roots. Reload derived
/// managed pointers after it returns; native cell addresses remain stable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_atomic_i64_new(thread: *mut Thread, v: i64) -> *mut AtomicI64 {
    let heap = unsafe { &*(*thread).heap };
    unsafe { service_native_pressure(thread, 0) };
    let cell = unsafe {
        blocking_region(thread, || {
            atomic_registry_growth::new_cell(v, heap)
        })
    };
    unsafe { atomic_registry_growth::publish(thread, heap, cell) }
}

/// Bind a fresh native cell to a weak managed wrapper identity.
/// # Safety
/// Caller is RUNNING on the owning heap. Cell is unbound and from this execution;
/// owner is a live managed object base. All subsequent raw cell uses must keep
/// that owner live through every safepoint and must finish before root teardown.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_atomic_i64_bind_owner(thread: *mut Thread, cell: *mut AtomicI64, owner: *mut u8) {
    assert!(!owner.is_null() && owner as usize != 1, "invalid atomic owner");
    let heap = unsafe { &*(*thread).heap };
    heap.atomic_cells.lock().unwrap().bind(cell, owner);
}

/// `a.load()` (SeqCst).
///
/// # Safety
/// `a` must be a cell from `ai_atomic_i64_new` in an active root execution.
/// If bound, its managed owner must stay live across every safepoint/use.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_atomic_i64_load(_thread: *mut Thread, a: *const AtomicI64) -> i64 {
    unsafe { (*a).load(Ordering::SeqCst) }
}

/// `a.store(v)` (SeqCst). Yields 0.
///
/// # Safety
/// As [`ai_atomic_i64_load`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_atomic_i64_store(_thread: *mut Thread, a: *const AtomicI64, v: i64) -> i64 {
    unsafe { (*a).store(v, Ordering::SeqCst) };
    0
}

/// `a.fetch_add(delta)` → the PREVIOUS value (SeqCst).
///
/// # Safety
/// As [`ai_atomic_i64_load`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_atomic_i64_fetch_add(_thread: *mut Thread, a: *const AtomicI64, delta: i64) -> i64 {
    unsafe { (*a).fetch_add(delta, Ordering::SeqCst) }
}

/// `a.compare_and_set(expected, new)` → 1 if the swap happened, else 0 (SeqCst).
///
/// # Safety
/// As [`ai_atomic_i64_load`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_atomic_i64_compare_and_set(
    _thread: *mut Thread,
    a: *const AtomicI64,
    expected: i64,
    new: i64,
) -> i64 {
    unsafe {
        match (*a).compare_exchange(expected, new, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => 1,
            Err(_) => 0,
        }
    }
}

/// Install the program's table of `static` cells (an array of references) as
/// a heap-wide root. Generated `main` calls this before running initializers.
/// A second install (re-entering `main`) replaces the table.
///
/// # Safety
/// `thread` is this OS thread's live RUNNING mutator; `table` is a live object
/// in its heap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_statics_install(thread: *mut Thread, table: *mut u8) {
    unsafe {
        let heap = &*(*thread).heap;
        let slot = heap.statics_slot.load(Ordering::Acquire);
        if slot != usize::MAX {
            heap.globals.set(slot, table as u64);
            return;
        }
        let slot = handoff_storage::admit(thread, heap, table);
        heap.statics_slot.store(slot, Ordering::Release);
    }
}

/// The table installed by [`ai_statics_install`]. Every thread of the program
/// shares it.
///
/// # Safety
/// `thread` is this OS thread's live RUNNING mutator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_statics_table(thread: *mut Thread) -> *mut u8 {
    unsafe {
        let heap = &*(*thread).heap;
        let slot = heap.statics_slot.load(Ordering::Acquire);
        if slot == usize::MAX {
            abort_with_diagnostic(thread, b"gc-rust: a static was used before main initialized it")
        }
        heap.globals.get(slot) as *mut u8
    }
}

/// A stable per-thread id (`Thread.current_id()`), derived from the OS thread.
#[unsafe(no_mangle)]
pub extern "C" fn ai_thread_current_id(_thread: *mut Thread) -> i64 {
    // ThreadId isn't directly an integer; hash it to a stable-per-thread value.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    (h.finish() & 0x7fff_ffff_ffff_ffff) as i64
}

// =============================================================================
// AOT entry point
// =============================================================================

/// One heap-shape descriptor as emitted into an AOT object file by codegen.
///
/// This is the *source* data for a `gc::TypeInfo` (the same fields
/// `codegen::layouts_to_type_infos` derives a `TypeInfo` from), serialized as a
/// fixed `#[repr(C)]` record so an AOT-compiled binary can hand its per-program
/// layout table to the runtime at startup. We deliberately do NOT serialize a
/// `gc::TypeInfo` directly: that type is not `#[repr(C)]`, so its in-memory
/// field order is unspecified. Instead the runtime rebuilds each `TypeInfo`
/// here with the exact same logic the JIT path uses, guaranteeing the GC
/// scanner sees identical shapes regardless of compilation mode.
///
/// `varlen`: 0 = None, 1 = Values, 2 = Bytes, 3 = Bytes of inline records
/// whose stride is `_pad[0..2]` (little-endian); their reference offsets
/// arrive as the layout's interior offsets in the metadata blob.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AotLayout {
    pub ptr_fields: u16,
    pub raw_bytes: u16,
    pub varlen: u8,
    pub _pad: [u8; 3],
}

/// Default + env-overridable generational heap sizes `(nursery_bytes,
/// tenured_bytes)`. BOTH the AOT runtime ([`gcr_runtime_main`]) and the JIT
/// driver use this so a program gets the SAME GC behavior regardless of run mode
/// (they previously disagreed — AOT 1 MB vs JIT 16 MB nursery — so the same
/// program had different perf/GC by run mode). Defaults: 64 MB nursery / 256 MB
/// tenured (per space). Override with `GCR_NURSERY_MB` / `GCR_TENURED_MB`
/// (binary MiB).
///
/// The nursery is sized so ordinary workloads DO collect (not sized past the
/// workload to dodge GC), while being large enough that a moderately-sized object
/// graph can live and die in the nursery — reclaimed by a cheap minor GC —
/// instead of being promoted wholesale to tenured. A too-small nursery (e.g. 1 MB
/// vs a 5 MB object graph) forces wholesale promotion with no reclaim until a
/// major GC fires, which is the AOT large-object cliff this fixes.
pub fn configured_heap_sizes() -> (usize, usize) {
    fn mb_env(key: &str, default_mb: usize) -> usize {
        heap_setting_bytes(std::env::var(key).ok().as_deref(), default_mb)
    }
    (mb_env("GCR_NURSERY_MB", 64), mb_env("GCR_TENURED_MB", 256))
}

fn heap_setting_bytes(value: Option<&str>, default_mb: usize) -> usize {
    value.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&mb| mb > 0)
        .and_then(|mb| mb.checked_mul(1 << 20))
        .unwrap_or(default_mb * (1 << 20))
}

#[cfg(test)]
mod configuration_tests {
    use super::heap_setting_bytes;

    #[test]
    fn heap_size_settings_reject_overflow_instead_of_wrapping() {
        for value in [None, Some("0"), Some("invalid"), Some("-1"), Some("18446744073709551615")] {
            assert_eq!(heap_setting_bytes(value, 16), 16 << 20);
        }
        assert_eq!(heap_setting_bytes(Some(" 32 "), 16), 32 << 20);
        let overflow = (usize::MAX / (1 << 20) + 1).to_string();
        assert_eq!(heap_setting_bytes(Some(&overflow), 16), 16 << 20);
    }
}

/// Per-space capacity for collect-on-every-allocation stress execution.
/// JIT and native drivers share this setting so valid larger applications can
/// exercise relocation without being restricted to the default eight MiB.
pub fn configured_stress_heap_size() -> usize {
    heap_setting_bytes(std::env::var("GCR_STRESS_HEAP_MB").ok().as_deref(), 8)
}

/// The native driver and JIT use the same opt-in stress selection.
pub fn configured_gc_stress() -> bool {
    std::env::var_os("GCR_GC_STRESS")
        .map(|value| !value.is_empty() && value != "0")
        .unwrap_or(false)
}

/// AOT program entry, called from the native `main` emitted into the object
/// file. Builds a [`RuntimeContext`] over the program's layout table (passed as
/// a `[AotLayout; ti_count]` blob in the binary), then invokes the compiled
/// program entry (`gcrust_entry`) with a live `Thread*` and returns its `i64`.
///
/// The `RuntimeContext` setup is identical to the JIT driver's: the GC frame
/// walker is installed, the safepoint poll flag is wired to `Thread::state`, and
/// the alloc window is pointed at the heap. This MUST match the JIT path or the
/// GC ABI breaks.
///
/// `meta`/`meta_len` carry the reflection metadata blob (encoded by
/// `gc::reflect::encode`); it is decoded into the heap's `TypeMeta` table so
/// heap-exploration tooling and in-language reflection can recover type/field
/// names. An empty blob (`meta_len == 0`) simply installs no metadata.
///
/// # Safety
/// `layouts` must point at `ti_count` valid `AotLayout` records, `meta` at
/// `meta_len` bytes produced by `gc::reflect::encode`, and `entry` must be the
/// compiled program entry with signature `extern "C" fn(*mut Thread) -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gcr_runtime_main_v4(
    layouts: *const AotLayout,
    ti_count: usize,
    meta: *const u8,
    meta_len: usize,
    entry: extern "C" fn(*mut Thread) -> i64,
) -> i64 {
    use crate::gc::{Full, ObjHeader, TypeInfo};
    let slice = if ti_count == 0 {
        &[][..]
    } else {
        assert!(!layouts.is_null(), "gcr_runtime_main: null layout table");
        unsafe { std::slice::from_raw_parts(layouts, ti_count) }
    };
    // Decode the metadata blob FIRST: besides the reflection tables it carries
    // the per-type interior-pointer offsets, which must be baked into the
    // `TypeInfo`s as we build the table (so the GC traces refs embedded in
    // flattened value fields). Each descriptor owns its decoded offsets; they
    // are released with the execution, matching the JIT path.
    let (meta_types, meta_values, interior, alloc_sites) = if meta_len > 0 {
        assert!(!meta.is_null(), "gcr_runtime_main: null metadata blob");
        let bytes = unsafe { std::slice::from_raw_parts(meta, meta_len) };
        crate::gc::reflect::decode(bytes)
    } else {
        (Vec::new(), Vec::new(), Vec::new(), Vec::new())
    };

    // Rebuild the type table exactly as `codegen::layouts_to_type_infos` does:
    // pointer fields first (traced), then raw bytes, then any varlen tail, plus
    // interior pointers for flattened value-with-ref fields.
    let type_table: Vec<TypeInfo> = slice
        .iter()
        .zip(interior.into_iter().chain(std::iter::repeat_with(Vec::new)))
        .enumerate()
        .map(|(i, (l, offsets))| {
            let mut ti = TypeInfo::for_header(Full::SIZE)
                .with_type_id(i as u16)
                .with_fields(l.ptr_fields)
                .with_raw_bytes(l.raw_bytes);
            ti = match l.varlen {
                0 => ti,
                1 => ti.with_varlen_values(l.ptr_fields),
                2 => ti.with_varlen_bytes(l.ptr_fields),
                3 => {
                    let stride = u16::from_le_bytes([l._pad[0], l._pad[1]]);
                    return ti.with_varlen_records(l.ptr_fields, stride, offsets);
                }
                other => panic!("gcr_runtime_main: invalid varlen kind {}", other),
            };
            if !offsets.is_empty() {
                ti = ti.with_owned_interior_ptrs(offsets);
            }
            ti
        })
        .collect();

    // Generational heap: a nursery (collected cheaply and often) over a large
    // tenured generation. Most objects die young, so minor GCs reclaim the bulk
    // of garbage without touching tenured space. Sizes come from
    // `configured_heap_sizes()` so the AOT runtime and the JIT driver agree (they
    // previously disagreed — AOT 1 MB vs JIT 16 MB — giving the same program
    // different GC behavior/perf by run mode). The moving path is precise (every
    // scalar in the untraced raw region; unused enum ptr-slots zeroed), so a
    // traced slot only ever holds a pointer-or-null; the collector arms a
    // precise-layout detector under debug / --gc-stress / GCR_GC_VERIFY=1.
    let (nursery, tenured) = configured_heap_sizes();
    let mut rt = if configured_gc_stress() {
        let rt = RuntimeContext::new(configured_stress_heap_size(), type_table);
        rt.heap().set_gc_every_alloc(true);
        rt
    } else {
        RuntimeContext::new_generational(nursery, tenured, type_table)
    };
    // Install the reflection metadata decoded above (type/field names + types)
    // so heap-exploration tooling and in-language reflection have nominal info.
    if !meta_types.is_empty() || !meta_values.is_empty() {
        rt.heap().set_type_meta(meta_types);
        rt.heap().set_value_meta(meta_values);
    }
    // Install the allocation-site table (Target-1b) so a profile can name each
    // site's (function, type). Installed even if empty is a no-op.
    if !alloc_sites.is_empty() {
        rt.heap().set_alloc_sites(alloc_sites);
    }
    let thread = rt.thread_ptr();
    // Configure streaming before user code can collect; no per-run event Vec.
    if let Some(path) = std::env::var_os("GCR_GC_LOG") {
        if let Err(error) = rt.heap().start_gc_log(&path) {
            let message = format!("gc-rust: cannot create GC log {:?}: {error}\n", path);
            unsafe { write_diagnostic(thread, message.as_bytes()); }
            return 1;
        }
    }
    // Publish the current thread so FFI callback trampolines can recover it.
    let result = rt.enter(|thread| entry(thread));
    unsafe {
        rt.finish_threads();
    }
    // Opt-in heap dump: render the live object graph with reflection metadata,
    // after the program returns (heap quiescent). `GCR_HEAP_DUMP=json` emits a
    // structured snapshot for tooling; any other non-empty value emits text.
    if let Some(mode) = std::env::var_os("GCR_HEAP_DUMP") {
        let dump = if mode == "json" {
            unsafe { crate::gc::dump::dump_heap_json(rt.heap()) }
        } else {
            unsafe { crate::gc::dump::dump_heap_text(rt.heap()) }
        };
        // `GCR_HEAP_DUMP_FILE` redirects the dump to a file (un-interleaved with
        // program output) — the data source for `gcr heap` + the explorer widget.
        if let Some(path) = std::env::var_os("GCR_HEAP_DUMP_FILE") {
            if let Err(e) = std::fs::write(&path, &dump) {
                let message = format!("gc-rust: cannot write heap dump to {:?}: {e}\n", path);
                unsafe { write_diagnostic(thread, message.as_bytes()); }
            }
        } else {
            unsafe { write_diagnostic(thread, dump.as_bytes()); }
        }
    }
    // Opt-in GC observability, matching the JIT path so AOT binaries report the
    // same numbers. `GCR_GC_STATS=1` → collection counts + pause-time summary;
    // `GCR_GC_LOG=<path>` → one JSON object per collection for offline analysis.
    if std::env::var_os("GCR_GC_STATS").is_some() {
        let summary = format!("gc-rust: {} minor + {} major collections\n{}", rt.heap().minor_collections(), rt.heap().collections(), rt.heap().gc_stats_summary());
        unsafe { write_diagnostic(thread, summary.as_bytes()); }
    }
    // Opt-in allocation-site profile (Target-1b): GCR_ALLOC_PROFILE=1 prints the
    // per-site count+bytes table at program end (heap quiescent).
    if std::env::var_os("GCR_ALLOC_PROFILE").is_some() {
        let report = rt.heap().alloc_site_profile_report();
        unsafe { write_diagnostic(thread, report.as_bytes()); }
    }
    // `gcr bench` metrics sink: GCR_METRICS_FILE=<path> writes the machine-readable
    // per-run metrics (GC cycles/pauses, allocation churn, peak heap) as JSON.
    let metrics_path = std::env::var_os("GCR_METRICS_FILE");
    if let Err(error) = finish_gc_telemetry(rt.heap(), metrics_path.as_deref().map(std::path::Path::new)) {
        let message = format!("gc-rust: {error}\n");
        unsafe { write_diagnostic(thread, message.as_bytes()); }
        return 1;
    }
    result
}

/// Finish optional GC observability after managed workers have retired.
/// Both sinks are attempted: a metrics failure must not strand buffered trace
/// events, and a trace failure must not suppress otherwise usable metrics.
/// The returned diagnostic includes every sink failure.
pub fn finish_gc_telemetry(heap: &Heap, metrics_path: Option<&std::path::Path>) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Some(path) = metrics_path {
        if let Err(error) = std::fs::write(path, heap.metrics_json()) {
            errors.push(format!("cannot write GC metrics {:?}: {error}", path));
        }
    }
    if let Err(error) = heap.flush_gc_log() {
        errors.push(format!("cannot write GC log: {error}"));
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
}

/// Startup compatibility for generated code predating projected value metadata.
/// New generated programs require v4 so older archives fail at link time.
/// # Safety
/// Same requirements as `gcr_runtime_main_v4`; legacy metadata remains supported.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gcr_runtime_main_v3(
    layouts: *const AotLayout,
    ti_count: usize,
    meta: *const u8,
    meta_len: usize,
    entry: extern "C" fn(*mut Thread) -> i64,
) -> i64 {
    unsafe { gcr_runtime_main_v4(layouts, ti_count, meta, meta_len, entry) }
}

/// Startup compatibility for code using atomic relocation-epoch loads.
/// # Safety
/// Same requirements as `gcr_runtime_main_v3`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gcr_runtime_main_v2(
    layouts: *const AotLayout,
    ti_count: usize,
    meta: *const u8,
    meta_len: usize,
    entry: extern "C" fn(*mut Thread) -> i64,
) -> i64 {
    unsafe { gcr_runtime_main_v3(layouts, ti_count, meta, meta_len, entry) }
}

/// Legacy AOT startup symbol. Older generated code retains its per-allocation
/// arena-epoch checks and works with the current runtime. Version 2 requires
/// closing generated allocation windows before resumption; version 3 also
/// permits ordinary loads from owner-local relocation epochs.
///
/// # Safety
/// Same layout, metadata and entry-pointer requirements as `gcr_runtime_main_v2`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gcr_runtime_main(
    layouts: *const AotLayout, ti_count: usize, meta: *const u8, meta_len: usize,
    entry: extern "C" fn(*mut Thread) -> i64,
) -> i64 {
    unsafe { gcr_runtime_main_v2(layouts, ti_count, meta, meta_len, entry) }
}

// =============================================================================
// In-language structural reflection (field iteration)
// =============================================================================

/// The reflection fields *active* for `obj` right now: a struct's fields, or the
/// payload fields of an enum's currently-tagged variant. Empty for opaque
/// builtins or when no metadata is installed. Borrows the heap's (immortal)
/// metadata table.
unsafe fn active_fields<'h>(heap: &'h Heap, obj: *const u8) -> &'h [crate::gc::FieldMeta] {
    // Only metadata escapes this scope, never an interior managed address.
    let access = unsafe { heap.object_access(obj as *mut u8) };
    unsafe { active_fields_canonical(heap, access.as_ptr()) }
}

/// Inspect metadata while the caller retains canonical access. Only immutable
/// off-heap metadata may escape; enum tags remain immutable after publication.
unsafe fn active_fields_canonical<'h>(heap: &'h Heap, obj: *const u8) -> &'h [crate::gc::FieldMeta] {
    use crate::gc::TypeKind;
    let tid = unsafe { heap.obj_type_id(obj) };
    match heap.type_meta_by_id(tid) {
        Some(m) => match &m.kind {
            TypeKind::Struct { fields } => fields,
            TypeKind::Enum { tag_offset, variants } => {
                let tag = unsafe { (obj.add(*tag_offset as usize) as *const u32).read_unaligned() };
                variants
                    .iter()
                    .find(|v| v.tag == tag)
                    .map(|v| v.fields.as_slice())
                    .unwrap_or(&[])
            }
            TypeKind::Opaque => &[],
        },
        None => &[],
    }
}

/// `field_count(obj)` — number of reflectable fields of `obj` (a struct's fields,
/// or the active enum variant's payload). 0 for opaque/builtin objects.
///
/// # Safety
/// `thread` is this OS thread's live RUNNING mutator; `obj` is a live object
/// in its heap. Installed reflection metadata must match the object's layout,
/// including field kinds, bounds and natural scalar/reference alignment.
/// Concurrent accesses must follow the managed slot model. No object-access
/// guard may already be held; these operations acquire their own scopes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_reflect_field_count(thread: *mut Thread, obj: *const u8) -> i64 {
    unsafe {
        let heap = &*(*thread).heap;
        active_fields(heap, obj).len() as i64
    }
}

/// Bounds-check a source-visible reflection index with a terminal diagnostic.
/// # Safety
/// `thread` is the current live RUNNING mutator.
unsafe fn field_at<'a>(thread: *mut Thread, fields: &'a [crate::gc::FieldMeta], idx: i64, op: &str) -> &'a crate::gc::FieldMeta {
    if idx < 0 || idx as usize >= fields.len() {
        unsafe { reflection_index_failure(thread, idx, fields.len(), op) }
    }
    &fields[idx as usize]
}

/// Terminal diagnostics can park; callers must release object access first.
unsafe fn reflection_index_failure(thread: *mut Thread, idx: i64, count: usize, op: &str) -> ! {
    let message = format!("reflect::{op}: field index {idx} out of range (0..{count})");
    unsafe { abort_with_diagnostic(thread, message.as_bytes()) }
}

/// `field_kind(obj, i)` — the kind of field `i`: 0=ref, 1=int, 2=float, 3=bool,
/// 4=char, 5=value-aggregate. Lets in-language code decide how to read it.
///
/// # Safety
/// As [`ai_reflect_field_count`]. Indices are checked before accessing storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_reflect_field_kind(thread: *mut Thread, obj: *const u8, i: i64) -> i64 {
    use crate::gc::{FieldTy, ScalarKind};
    unsafe {
        let heap = &*(*thread).heap;
        let f = field_at(thread, active_fields(heap, obj), i, "field_kind");
        match &f.ty {
            FieldTy::Ref(_) => 0,
            FieldTy::Scalar(ScalarKind::F32 | ScalarKind::F64) => 2,
            FieldTy::Scalar(ScalarKind::Bool) => 3,
            FieldTy::Scalar(ScalarKind::Char) => 4,
            FieldTy::Scalar(_) => 1,
            FieldTy::Value(_) | FieldTy::ProjectedValue { .. } => 5,
        }
    }
}

/// `field_i64(obj, i)` — field `i`'s value widened to i64: a scalar decoded by
/// its kind (sign-extended for signed ints, float bits for floats), or a ref's
/// pointer bits. Pairs with `field_kind` so the caller interprets it. Value
/// aggregates (kind 5) are rejected without reading their representation.
///
/// # Safety
/// As [`ai_reflect_field_count`]. Indices are checked before accessing storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_reflect_field_i64(thread: *mut Thread, obj: *const u8, i: i64) -> i64 {
    use crate::gc::{FieldTy, ScalarKind};
    unsafe {
        let heap = &*(*thread).heap;
        let access = heap.object_access(obj as *mut u8);
        let fields = active_fields_canonical(heap, access.as_ptr());
        let f = match usize::try_from(i).ok().and_then(|index| fields.get(index)) {
            Some(field) => field,
            None => {
                drop(access);
                reflection_index_failure(thread, i, fields.len(), "field_i64");
            }
        };
        // Diagnostics may park for GC. No access stripe is held on either
        // bounds or unsupported-kind failure.
        if matches!(&f.ty, FieldTy::Value(_) | FieldTy::ProjectedValue { .. }) {
            drop(access);
            let message = format!("reflect::field_i64 requires a scalar or reference field; value aggregate at index {i}");
            abort_with_diagnostic(thread, message.as_bytes());
        }
        let at = access.as_ptr().add(f.offset as usize);
        // Retain canonical storage through the matching-width SC load.
        macro_rules! load {
            ($atomic:ty) => { (&*at.cast::<$atomic>()).load(Ordering::SeqCst) };
        }
        use std::sync::atomic::{AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicU8, AtomicU16, AtomicU32, AtomicU64};
        match &f.ty {
            FieldTy::Ref(_) => load!(AtomicU64) as i64,
            FieldTy::Value(_) | FieldTy::ProjectedValue { .. } => unreachable!("aggregate rejected before scoped load"),
            FieldTy::Scalar(k) => match k {
                ScalarKind::I8 => load!(AtomicI8) as i64,
                ScalarKind::I16 => load!(AtomicI16) as i64,
                ScalarKind::I32 => load!(AtomicI32) as i64,
                ScalarKind::I64 => load!(AtomicI64),
                ScalarKind::U8 | ScalarKind::Bool => load!(AtomicU8) as i64,
                ScalarKind::U16 => load!(AtomicU16) as i64,
                ScalarKind::U32 | ScalarKind::Char | ScalarKind::F32 => load!(AtomicU32) as i64,
                ScalarKind::U64 | ScalarKind::Ptr | ScalarKind::F64 => load!(AtomicU64) as i64,
                // The low half, as an `as i64` cast keeps.
                ScalarKind::I128 | ScalarKind::U128 => load!(AtomicU64) as i64,
            },
        }
    }
}

/// `field_name(obj, i)` — a fresh `String` holding field `i`'s source name.
/// `str_type_id` is the `String` layout id used to allocate the result.
///
/// # Safety
/// As [`ai_reflect_field_count`]. Indices are checked before accessing storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_reflect_field_name(
    thread: *mut Thread,
    str_type_id: i64,
    obj: *const u8,
    i: i64,
) -> *mut u8 {
    unsafe {
        let heap = &*(*thread).heap;
        // Clone the name out before allocating (the alloc may move the heap, but
        // the metadata table is off-heap and immortal, so the &str stays valid;
        // we copy to be explicit and avoid borrowing across the alloc).
        let name = field_at(thread, active_fields(heap, obj), i, "field_name").name.clone();
        alloc_string_from_text(thread, str_type_id as u32, &name)
    }
}

// =============================================================================
// Runtime extern functions called by compiled code
// =============================================================================

/// Allocate a fixed-size (non-varlen) heap object of the shape described by the
/// `TypeInfo` at index `type_id` in the heap's type table. Returns a pointer to
/// the object (past nothing — the header is at offset 0). The object's header is
/// initialized; value-field slots are zeroed.
///
/// Compiled code spills all live roots into its frame before calling this,
/// because a collection triggered here can relocate every live object.
///
/// # Safety
/// `thread` must be a valid `*mut Thread` whose `heap` points at a live `Heap`,
/// and `type_id` must index a registered `TypeInfo`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_gc_alloc_fixed(
    thread: *mut Thread,
    type_id: u32,
    site_id: u32,
) -> *mut u8 {
    unsafe {
        let t = &*thread;
        let heap = &*t.heap;
        let info: &TypeInfo = heap.type_info_by_id(type_id as u16);
        // A successful TLAB allocation cannot collect. Other collectors wait
        // for our safepoint before reading roots; publish the frame only when
        // entering the path that may collect or run allocation stress.
        let fast = heap.try_alloc_runtime_fixed(&*t.dyna_thread, type_id as u16);
        let p = if fast.is_null() { alloc_with_published_frame(t, heap, info, 0) } else { fast };
        // Allocation-site profiling (Target-1b): record this site's count+bytes
        // on the OWNING thread's non-atomic per-site counter. `dyna_thread` is
        // the same `ThreadState` `alloc_with_published_frame` already
        // dereferenced, so it is non-null here. Only on a successful alloc.
        if !p.is_null() {
            (*t.dyna_thread).record_alloc(site_id, info.allocation_size(0) as u64);
        }
        p
    }
}

/// Record a compiled inline allocation. Owning RUNNING mutator only; this
/// operation cannot collect or reach a managed safepoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_gc_record_alloc(thread: *mut Thread, site_id: u32, bytes: u64) {
    unsafe { (*(*thread).dyna_thread).record_alloc(site_id, bytes); }
}

/// Read a live managed object's physical type ID through canonical storage.
///
/// # Safety
/// Thread is the owning RUNNING registration; object is a live published base
/// in its heap. No nested access guard, safepoint or managed callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_object_type_id(thread: *mut Thread, object: *const u8) -> u16 {
    unsafe {
        let heap = &*(*thread).heap;
        let access = heap.object_access(object as *mut u8);
        heap.obj_type_id(access.as_ptr())
    }
}

/// Read a closure env object's CODE POINTER. The code pointer is stored at
/// `HEADER + ptr_fields*8` (right after the captured GC-pointer slots), but
/// `ptr_fields` varies per closure and is NOT known at a polymorphic call/spawn
/// site (where the closure is typed as a bare `fn()` and lowers to a placeholder
/// layout with ptr_fields=0). So we recover it at runtime from the env object's
/// own header `type_id` → `TypeInfo.value_field_count`. This is the single source
/// of truth for the code-pointer offset; codegen's `gen_call_closure` and
/// `ThreadSpawn` both use it instead of a static (possibly-wrong) offset. See
/// `docs/threads.md`.
///
/// # Safety
/// `thread` is the owning RUNNING mutator; `env` is a live closure environment
/// allocated by gen_make_closure. No outer access guard may be held. The code
/// address remains executable under the embedding's code-lifetime contract.
/// This call does not safepoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_closure_code_ptr(thread: *mut Thread, env: *const u8) -> *const u8 {
    unsafe {
        let heap = &*(*thread).heap;
        let access = heap.object_access(env as *mut u8);
        let env = access.as_ptr();
        let type_id = heap.obj_type_id(env);
        let info: &TypeInfo = heap.type_info_by_id(type_id);
        let ptr_fields = info.value_field_count as usize;
        let code_off = 16 + ptr_fields * 8; // 16 = Full header size
        *(env.add(code_off) as *const *const u8)
    }
}

/// On-demand mid-execution heap snapshot (heap-explorer P2). The in-language
/// `heap_snapshot()` builtin lowers to this. Called from COMPILED CODE, so the
/// calling thread is at a safepoint by construction — this is the signal-SAFE
/// trigger (no `pause_world` from arbitrary signal context). Writes a JSON
/// snapshot (same schema as `GCR_HEAP_DUMP=json`) to
/// `GCR_HEAP_SNAPSHOT_DIR/snapshot-NNNN.json` (a numbered series for
/// `gcr heap-diff` leak-hunting), or to stderr if the dir is unset.
///
/// We publish this thread's own frame (`parked_jit_fp`) first so the snapshot's
/// real-roots scan (`visit_roots`) sees this thread's LIVE roots — `dump_heap_json`
/// pauses every *other* mutator but excludes the requester (us), so without this
/// our own live objects would be mis-reported as unreachable. This is the whole
/// point of a mid-execution snapshot: rich live roots, unlike the program-end dump.
///
/// # Safety
/// `thread` valid (a live compiled-code thread with `heap`/`dyna_thread` set).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_heap_snapshot(thread: *mut Thread) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::io::Write;
    static SNAPSHOT_SEQ: AtomicUsize = AtomicUsize::new(0);
    unsafe {
        let t = &*thread;
        let heap = &*t.heap;
        // Publish roots and suspend this mutator before waiting for gc_lock.
        // A collector holding that lock may itself be waiting for us to stop.
        // The diagnostic walker reads managed bytes only under its WorldPause
        // guard; the returned JSON and subsequent I/O use host-owned storage.
        blocking_region(thread, || {
            let json = crate::gc::dump::dump_heap_json(heap);
            let seq = SNAPSHOT_SEQ.fetch_add(1, Ordering::Relaxed);
            if let Some(dir) = std::env::var_os("GCR_HEAP_SNAPSHOT_DIR") {
                let path = std::path::Path::new(&dir).join(format!("snapshot-{seq:04}.json"));
                if let Err(e) = std::fs::write(&path, &json) {
                    let _ = writeln!(std::io::stderr().lock(), "gc-rust: heap_snapshot: cannot write {}: {e}", path.display());
                }
            } else {
                let _ = std::io::stderr().lock().write_all(json.as_bytes());
            }
        });
    }
}

/// Allocate a variable-length heap object (array / string / bytes shape) with
/// `varlen_len` trailing elements, per the `TypeInfo` at `type_id`.
///
/// # Safety
/// As [`ai_gc_alloc_fixed`]; additionally the `TypeInfo` must be a varlen shape.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_gc_alloc_varlen(
    thread: *mut Thread,
    type_id: u32,
    varlen_len: u64,
    site_id: u32,
) -> *mut u8 {
    unsafe {
        let t = &*thread;
        let heap = &*t.heap;
        let info: &TypeInfo = heap.type_info_by_id(type_id as u16);
        let Some(request) = heap.object_request::<Full>(info, varlen_len as usize) else {
            abort_with_diagnostic(thread, b"gc-rust: variable-length allocation size overflow");
        };
        // A local success initializes header/count before publication and cannot
        // collect. Publish the compiled frame only before the collecting path.
        let fast = request.try_allocate_local(&*t.dyna_thread);
        let p = if fast.is_null() {
            alloc_with_published_frame(t, heap, info, varlen_len as usize)
        } else { fast };
        // Allocation-site profiling (Target-1b) — see `ai_gc_alloc_fixed`.
        if !p.is_null() {
            (*t.dyna_thread).record_alloc(site_id, info.allocation_size(varlen_len as usize) as u64);
        }
        p
    }
}

/// Load the managed-reference payload of an Atom with SeqCst ordering.
///
/// # Safety
/// `thread` is the owning RUNNING mutator; `atom` is a live published Full-header
/// Atom with an aligned untagged reference payload. No nested access scope.
/// Returned references obey the normal root/reload rules.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_atom_ref_load(thread: *mut Thread, atom: *mut u8) -> *mut u8 {
    unsafe {
        let heap = &*(*thread).heap;
        let access = heap.object_access(atom);
        let slot = &*access.as_ptr().add(<Full as crate::gc::ObjHeader>::SIZE).cast::<std::sync::atomic::AtomicU64>();
        slot.load(Ordering::SeqCst) as *mut u8
    }
}

/// Compare managed identity and replace an Atom reference, with GC barriers.
///
/// # Safety
/// As `ai_atom_ref_load`; expected/new are null or live published managed bases
/// kept reachable throughout the call. This operation cannot safepoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_atom_ref_cas(
    thread: *mut Thread,
    atom: *mut u8,
    expected: *mut u8,
    new: *mut u8,
) -> u8 {
    unsafe {
        let heap = &*(*thread).heap;
        heap.compare_exchange_reference(&*(*thread).dyna_thread, atom, <Full as crate::gc::ObjHeader>::SIZE, expected, new).is_ok() as u8
    }
}

/// Generational write barrier. Compiled code calls this AFTER storing the
/// pointer `new_val` into `slot`, a field or element of a tenured object
/// (compiled code filters out young objects). If `new_val` points into the
/// nursery, the card covering `slot` is marked dirty so the next minor GC
/// visits the old→young pointer without scanning all of tenured space. A
/// large array is scanned only in its dirty cards, so the card is the slot's,
/// not the object's.
///
/// Fast path: for a non-generational heap (no nursery) this is a single load +
/// branch and returns immediately. Null/non-nursery writes also return cheaply.
///
/// # Safety
/// `thread` valid; `slot` inside a valid heap object; `new_val` a heap pointer
/// or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_gc_write_barrier(thread: *mut Thread, slot: *mut u8, new_val: *mut u8) {
    unsafe {
        let heap = &*(*thread).heap;
        // Cheapest possible bail-out for the common (non-generational) case.
        if !heap.has_nursery() {
            return;
        }
        if !new_val.is_null() && heap.is_nursery(new_val) {
            heap.mark_card_dirty(slot);
        }
    }
}

/// Allocate stable, eight-byte-aligned native storage for a foreign argument.
/// This noncollecting ABI keeps managed arguments valid during copy-in. The
/// generated caller frees every allocation after the foreign call and copy-out.
#[unsafe(no_mangle)]
pub extern "C" fn ai_ffi_buffer_alloc_v1(bytes: i64) -> *mut u8 {
    let size = usize::try_from(bytes).expect("invalid foreign buffer size").max(1);
    let layout = std::alloc::Layout::from_size_align(size, 8)
        .expect("foreign buffer size exceeds native allocation limit");
    // Initialize even output buffers: native readers may inspect the complete
    // scalar representation, including bytes the foreign callee leaves alone.
    let buffer = unsafe { std::alloc::alloc_zeroed(layout) };
    if buffer.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    buffer
}

/// Release storage returned by ai_ffi_buffer_alloc_v1.
///
/// # Safety
/// `buffer` must be a live allocation returned by that function with exactly
/// `bytes`, and no native code may retain or access it after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_ffi_buffer_free_v1(buffer: *mut u8, bytes: i64) {
    let size = usize::try_from(bytes).expect("invalid foreign buffer size").max(1);
    let layout = std::alloc::Layout::from_size_align(size, 8)
        .expect("foreign buffer size exceeds native allocation limit");
    unsafe { std::alloc::dealloc(buffer, layout) };
}

/// Allocate an opaque ownership token for accounted native argument storage.
/// Obtain its aligned payload with ai_ffi_buffer_data_v2.
/// Large native allocation runs BLOCKED. Native pressure may collect even for
/// a small request; callers must reload every managed pointer used afterward.
///
/// # Safety
/// This OS thread owns the RUNNING mutator. All live managed values are rooted;
/// no object-access scope may span the call. Pair with ai_ffi_buffer_free_v2.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_ffi_buffer_alloc_v2(thread: *mut Thread, bytes: i64) -> *mut u8 {
    unsafe { ffi_buffer::allocate(thread, bytes) }
}

/// Release accounted native storage after the foreign call and copy-back.
/// Large release runs BLOCKED, and may allow managed objects to move.
///
/// # Safety
/// As ai_ffi_buffer_alloc_v2. Pass its live ownership token, not the payload
/// address returned by ai_ffi_buffer_data_v2. Release it exactly once, with no
/// retained payload access. The token owns its exact allocation geometry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_ffi_buffer_free_v2(thread: *mut Thread, token: *mut u8) {
    unsafe { ffi_buffer::release(thread, token) }
}

/// Return the stable payload address of a live native ownership token.
/// This operation neither blocks nor collects.
///
/// # Safety
/// The token was returned by ai_ffi_buffer_alloc_v2 and has not been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_ffi_buffer_data_v2(token: *const u8) -> *mut u8 {
    unsafe { ffi_buffer::data(token) }
}

/// FFI `as_c_bytes`: copy a `String`'s UTF-8 bytes followed by a NUL terminator
/// into the caller-provided buffer `dst`, which must have room for
/// `str_len(s) + 1` bytes. The caller (compiled code) allocates `dst` as a
/// scoped native allocation, so the resulting C pointer is stable for the
/// duration of the enclosing extern call and needs no pinning. Returns the byte
/// length (excluding the NUL). See `docs/ffi.md`.
///
/// # Safety
/// `s` must be a valid `String` object; `dst` must point to at least
/// `str_len(s) + 1` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_copy_to_buf(_thread: *mut Thread, s: *const u8, dst: *mut u8) -> i64 {
    unsafe {
        let bytes = str_bytes(s);
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        *dst.add(bytes.len()) = 0; // NUL terminator for C string consumers
        bytes.len() as i64
    }
}

/// FFI `as_c_bytes` (generic): copy `byte_len` bytes from a varlen object's data
/// region (a `String` or scalar `Array`, both laid out as
/// `[header][count][data…]`) into the caller-provided native buffer `dst`. Used
/// for both strings and arrays; the caller passes the exact byte length (for a
/// String this is its length, for an `Array<T>` it is `len * sizeof(T)`).
///
/// # Safety
/// `obj` must be a valid varlen object with at least `byte_len` data bytes; `dst`
/// must point to at least `byte_len` writable bytes. Thread is the owning
/// RUNNING mutator. Width is 1/8/16/32/64; length is nonnegative and divisible
/// by the element width in bytes. Native storage is disjoint and unshared;
/// managed elements have natural alignment and compatible atomic accesses.
/// No nested object-access scope may be held. This call cannot safepoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_buf_copy_in(thread: *mut Thread, obj: *const u8, dst: *mut u8, byte_len: i64, bits: i64) {
    unsafe {
        let access = (&*(*thread).heap).object_access(obj as *mut u8);
        managed::copy_scalar_buffer(access.as_ptr().add(STR_DATA_OFF), dst, byte_len, bits, false);
    }
}

/// FFI copy-out: copy `byte_len` bytes from the native buffer `src` BACK into a
/// varlen object's data region. Pairs with [`ai_buf_copy_in`] for a `mut` array
/// argument that the C function filled in place (e.g. `read(fd, buf, n)`).
///
/// # Safety
/// `obj` must be a valid varlen object with room for `byte_len` data bytes; `src`
/// must point to at least `byte_len` readable bytes. Thread, alignment, width,
/// disjointness and scope requirements are as ai_buf_copy_in. The managed
/// buffer is untraced scalar storage; reference arrays are excluded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_buf_copy_out(thread: *mut Thread, obj: *mut u8, src: *const u8, byte_len: i64, bits: i64) {
    unsafe {
        let access = (&*(*thread).heap).object_access(obj);
        managed::copy_scalar_buffer(access.as_ptr().add(STR_DATA_OFF), src.cast_mut(), byte_len, bits, true);
    }
}

/// Cooperatively copy String bytes and a trailing NUL to native storage.
///
/// # Safety
/// As ai_str_copy_to_buf, with this OS thread's RUNNING mutator. May collect:
/// callers must root any managed values used afterward. No object access may
/// remain active across this call. The destination is disjoint native storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_copy_to_buf_v2(thread: *mut Thread, source: *const u8, native: *mut u8) -> i64 {
    unsafe { ffi_copy::string(thread, source, native) }
}

/// Cooperatively copy scalar-array elements into native storage.
///
/// # Safety
/// As ai_buf_copy_in, except this version may collect. Caller roots all values
/// needed afterward; no object-access scope may be held across this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_buf_copy_in_v2(thread: *mut Thread, owner: *const u8, native: *mut u8, bytes: i64, bits: i64) {
    unsafe { ffi_copy::scalar(thread, owner, native, bytes, bits, false) }
}

/// Cooperatively copy native scalar elements back into their managed owner.
///
/// # Safety
/// As ai_buf_copy_out, except this version may collect. Caller roots all values
/// needed afterward; no object-access scope may be held across this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_buf_copy_out_v2(thread: *mut Thread, owner: *mut u8, native: *const u8, bytes: i64, bits: i64) {
    unsafe { ffi_copy::scalar(thread, owner, native.cast_mut(), bytes, bits, true) }
}

/// FFI boundary: enter a foreign (`extern "C"`) call. Publishes this thread's
/// current frame chain so that a garbage collection occurring while we are
/// inside native code (triggered by another mutator thread) can still find this
/// thread's live roots — i.e. the thread is treated as parked at a safepoint for
/// the duration of the call. This is the "managed → native" transition.
///
/// Compiled code calls this immediately BEFORE every `extern "C"` call and pairs
/// it with [`ai_ffi_leave`] immediately after. Because roots are published, a GC
/// may safely run during the call: the FFI is *safe by default*, not dependent
/// on the foreign function promising not to allocate. Per the FFI design, only
/// non-moving data (scalars, pointers to stack-resident value structs) crosses
/// the boundary, so the collector never needs to update anything native code
/// holds. See `docs/ffi.md`.
///
/// # Safety
/// `thread` is this OS thread's live RUNNING mutator. Every current registration
/// for its heap must publish roots, retain their frames through the foreign
/// scope, and reload derived pointers after resumption. Pair with ai_ffi_leave;
/// callbacks must use ai_ffi_reenter/ai_ffi_exit before accessing managed state.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_ffi_enter(thread: *mut Thread) {
    unsafe {
        let t = &*thread;
        let dyna = &*t.dyna_thread;
        assert!(dyna.is_running(), "foreign entry requires a running mutator");
        dyna.set_parked_jit_fp(t.top_frame as *const u8);
        let heap = clone_thread_heap(t);
        let group = heap.suspend_owned();
        (&mut *t.foreign_scopes.get()).push(Some(group));
    }
}

// Runtime registrations own an Arc to this exact heap allocation.
unsafe fn clone_thread_heap(thread: &Thread) -> Arc<Heap> {
    let pointer = thread.heap as *const Heap;
    unsafe { Arc::increment_strong_count(pointer); Arc::from_raw(pointer) }
}

/// FFI boundary: leave a foreign call (the "native → managed" transition).
/// Waits for any active collection before marking the thread as running and
/// clearing the published frame pointer set by [`ai_ffi_enter`].
///
/// # Safety
/// As [`ai_ffi_enter`]; must be paired with a preceding `ai_ffi_enter`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_ffi_leave(thread: *mut Thread) {
    unsafe {
        let t = &*thread;
        let group = (&mut *t.foreign_scopes.get()).pop()
            .expect("foreign leave without entry").expect("foreign leave during callback");
        drop(group);
        (&*t.dyna_thread).clear_parked_jit_fp();
    }
}

/// Callback boundary: the inverse transition, for a gc-rust function invoked as a
/// C **callback** while the thread is parked "in native" (inside an outer
/// `extern` call). The callback re-enters managed code and may allocate, so it
/// must re-acquire managed state and clear the published frame pointer on entry
/// ([`ai_ffi_reenter`]) and re-publish it on exit ([`ai_ffi_exit`]) since control
/// returns to the foreign caller. A callback trampoline brackets the real
/// gc-rust call with this pair. See `docs/ffi.md`.
///
/// # Safety
/// `thread` must be the live current mutator thread (from `ai_current_thread`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_ffi_reenter(thread: *mut Thread) {
    unsafe {
        let t = &*thread;
        let group = (&mut *t.foreign_scopes.get()).last_mut()
            .expect("callback without foreign scope").take().expect("callback already managed");
        drop(group);
        (&*t.dyna_thread).clear_parked_jit_fp();
    }
}

/// Callback boundary: re-publish the frame pointer before returning from a
/// callback into foreign code. Pairs with [`ai_ffi_reenter`].
///
/// # Safety
/// As [`ai_ffi_reenter`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_ffi_exit(thread: *mut Thread) {
    unsafe {
        let t = &*thread;
        assert!((&*t.foreign_scopes.get()).last().is_some_and(Option::is_none),
            "callback exit without managed scope");
        (&*t.dyna_thread).set_parked_jit_fp(t.top_frame as *const u8);
        let heap = clone_thread_heap(t);
        let group = heap.suspend_owned();
        *(&mut *t.foreign_scopes.get()).last_mut().unwrap() = Some(group);
    }
}

/// Shared allocation helper: publish the current frame chain so a GC triggered
/// inside `heap.alloc` can find our roots, allocate, then clear the published
/// fp. Allocation may move objects, so callers must have spilled live roots to
/// frame slots already.
#[inline]
unsafe fn alloc_with_published_frame(
    t: &Thread,
    heap: &Heap,
    info: &TypeInfo,
    varlen_len: usize,
) -> *mut u8 {
    unsafe {
        let dyna = &*t.dyna_thread;
    debug_assert!(dyna.is_running(), "allocation requires an active runtime entry");
        dyna.set_parked_jit_fp(t.top_frame as *const u8);

        // Stress mode: collect on EVERY allocation, maximally exercising precise
        // rooting and relocation. The frame is published above, so all live roots
        // are scannable (ANF guarantees no GC value is stranded in a register, and
        // construction sites reload GC operands after the alloc). Sound for both
        // single- and multi-threaded programs. Minor GC for generational heaps; a
        // full collection for the semi-space heap the `--gc-stress` driver uses.
        if heap.gc_every_alloc() {
            if heap.has_nursery() {
                heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(dyna);
            } else {
                heap.mutator_triggered_gc::<IdentityPtrPolicy>(dyna);
            }
        }

        // `alloc_obj::<Full>` (not bare `alloc`) stamps the object header with
        // `info.type_id`; bare `alloc` only zeroes, leaving type_id 0, which
        // breaks the GC scanner.
        let size = info.allocation_size(varlen_len);
        let p = if heap.has_nursery() && !heap.allocation_fits_nursery(size) {
            // An oversized object cannot benefit from nursery scavenging.
            // Try tenured storage directly and collect only on exhaustion.
            let mut p = heap.alloc_obj::<Full>(info, varlen_len);
            if p.is_null() {
                // Major scanning treats nursery contents as roots. Discard
                // dead nursery owners before reclaiming their tenured targets.
                heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(dyna);
                p = heap.alloc_obj::<Full>(info, varlen_len);
            }
            if p.is_null() {
                heap.mutator_gc_for_allocation::<IdentityPtrPolicy>(dyna, size);
                p = heap.alloc_obj::<Full>(info, varlen_len);
            }
            p
        } else if heap.has_nursery() {
            // Generational: allocate in the nursery; on full, a MINOR GC (cheap,
            // scavenges only the young generation), retry, then fall back to a
            // major GC, and finally to tenured space directly.
            let mut p = heap.alloc_mutator_obj::<Full>(dyna, info, varlen_len);
            if p.is_null() {
                heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(dyna);
                p = heap.alloc_mutator_obj::<Full>(dyna, info, varlen_len);
            }
            if p.is_null() {
                heap.mutator_gc_for_allocation::<IdentityPtrPolicy>(dyna, info.allocation_size(varlen_len));
                p = heap.alloc_mutator_obj::<Full>(dyna, info, varlen_len);
            }
            if p.is_null() {
                // Another mutator can refill the nursery between collection
                // and retry. Use available tenured storage as the fallback.
                p = heap.alloc_obj::<Full>(info, varlen_len);
            }
            p
        } else {
            // Non-generational semi-space: from-space, then a major GC + retry.
            let mut p = heap.alloc_mutator_obj::<Full>(dyna, info, varlen_len);
            if p.is_null() {
                heap.mutator_gc_for_allocation::<IdentityPtrPolicy>(dyna, info.allocation_size(varlen_len));
                p = heap.alloc_mutator_obj::<Full>(dyna, info, varlen_len);
            }
            p
        };

        if p.is_null() {
            dyna.clear_parked_jit_fp();
            let message = format!("gc-rust: out of memory — live set exceeds the heap (object type_id {}, {} varlen elems)", info.type_id, varlen_len);
            abort_with_diagnostic(t as *const Thread as *mut Thread, message.as_bytes());
        }
        // Constructors initialize private objects with plain stores after this
        // call returns. Direct tenure no longer implies that their operands
        // were promoted by a preceding minor collection. Remember the whole
        // fresh owner before publication so those nursery edges are scanned.
        if heap.has_nursery() && heap.is_tenured(p)
            && (info.value_field_count != 0 || !info.interior_ptrs.is_empty()
                || info.varlen == crate::gc::VarLenKind::Values || !info.record_ptrs.is_empty())
        {
            heap.mark_card_dirty(p);
        }
        dyna.clear_parked_jit_fp();
        p
    }
}

/// Print a signed 64-bit integer followed by a newline. A minimal IO primitive
/// so compiled programs can emit output. Returns 0.
/// # Safety
/// `thread` is null or the current live mutator with published managed roots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_print_int(thread: *mut Thread, v: i64) -> i64 {
    unsafe { print_host_bytes(thread, v.to_string().as_bytes(), true, false) }
}

/// Print a 64-bit float followed by a newline. Returns 0.
/// # Safety
/// As `ai_print_int`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_print_float(thread: *mut Thread, v: f64) -> i64 {
    unsafe { print_host_bytes(thread, v.to_string().as_bytes(), true, false) }
}

/// Host-owned bytes remain valid while the managed mutator parks for output.
/// Legacy print primitives ignore I/O failure; checked stdout_line reports it.
unsafe fn print_host_bytes(thread: *mut Thread, bytes: &[u8], newline: bool, flush: bool) -> i64 {
    use std::io::Write;
    unsafe {
        blocking_region(thread, || {
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            let result = output.write_all(bytes);
            if result.is_ok() && newline { let _ = output.write_all(b"\n"); }
            if flush { let _ = output.flush(); }
        });
    }
    0
}

/// Write host-owned diagnostic bytes without unwinding on output errors.
/// # Safety
/// `thread` is null or this OS thread's live RUNNING mutator.
pub unsafe fn write_diagnostic(thread: *mut Thread, bytes: &[u8]) {
    use std::io::Write;
    unsafe { blocking_region(thread, || { let _ = std::io::stderr().lock().write_all(bytes); }); }
}

/// Terminal diagnostics own their text before publishing roots and parking.
/// They never resume managed execution, even when stderr is closed or fails.
/// # Safety
/// `thread` is null or this OS thread's live RUNNING mutator.
unsafe fn abort_with_diagnostic(thread: *mut Thread, message: &[u8]) -> ! {
    use std::io::Write;
    unsafe {
        blocking_region(thread, || {
            let stderr = std::io::stderr();
            let mut output = stderr.lock();
            let _ = output.write_all(message);
            let _ = output.write_all(b"\n");
            std::process::abort()
        })
    }
}

/// Abort with a clear message on an out-of-bounds array access. Called from the
/// inlined bounds check codegen emits around every `array_get`/`array_set`; it
/// never returns (the compiled fail path ends in `unreachable`). A legible
/// diagnostic on stderr then `abort()`, instead of a silent memory-safety bug.
/// # Safety
/// `thread` is null or this OS thread's live RUNNING mutator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_bounds_fail(thread: *mut Thread, index: i64, len: i64) -> ! {
    let message = format!("gc-rust: array index out of bounds: the index is {index} but the length is {len}");
    unsafe { abort_with_diagnostic(thread, message.as_bytes()) }
}

/// Reference/value array slots have no universal default inhabitant. Null is
/// internal unpublished storage, never a valid source-language element.
/// # Safety
/// `thread` is null or this OS thread's live RUNNING mutator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_uninitialized_array_fail(thread: *mut Thread, index: i64) -> ! {
    let message = format!("gc-rust: uninitialized array element at index {index}");
    unsafe { abort_with_diagnostic(thread, message.as_bytes()) }
}

/// Validate array lengths before computing the allocator's trailing count.
/// Valid lengths return without suspending; invalid lengths report and abort.
/// # Safety
/// `thread` is null or this OS thread's live RUNNING mutator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_array_allocation_len(
    thread: *mut Thread, count: i64, stride: u64, traced: i32,
) -> u64 {
    let bytes = (count as u64).checked_mul(stride);
    if count < 0 || stride == 0 || bytes.is_none_or(|n| n > isize::MAX as u64) {
        let message = format!("gc-rust: invalid array length: {count} (element size {stride})");
        unsafe { abort_with_diagnostic(thread, message.as_bytes()) }
    }
    if traced != 0 { count as u64 } else { bytes.unwrap() }
}

/// Report a terminal arithmetic failure.
/// # Safety
/// `thread` is null or this OS thread's live RUNNING mutator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_arithmetic_fail(thread: *mut Thread, reason: i32) -> ! {
    // Compiler/runtime ABI: 0 = division, 1 = remainder, 2 = char cast.
    let message = match reason {
        0 => "integer division by zero",
        1 => "integer remainder by zero",
        2 => "invalid Unicode scalar value in char cast",
        _ => "invalid arithmetic failure reason",
    };
    let message = format!("gc-rust: {message}");
    unsafe { abort_with_diagnostic(thread, message.as_bytes()) }
}

// ---- String primitives ----------------------------------------------------
//
// A gc-rust `String` is a varlen `Bytes` object: [ObjHeader (Full::SIZE)] then a
// u64 byte-length count word, then the UTF-8 bytes. This matches the layout
// codegen emits for string literals and what `String`'s registered `TypeInfo`
// (varlen bytes) describes. The constants below mirror `codegen`'s `HEADER`.

/// Byte offset from an object pointer to its varlen count word.
const STR_COUNT_OFF: usize = 16; // Full::SIZE
/// Byte offset from an object pointer to the first UTF-8 byte.
const STR_DATA_OFF: usize = 24; // Full::SIZE + 8 (count word)

/// Borrow a `String` object's bytes as a slice. The pointer must be live (not
/// mid-relocation) for the duration of the borrow.
///
/// # Safety
/// `s` must point to a valid gc-rust `String` object.
unsafe fn str_bytes<'a>(s: *const u8) -> &'a [u8] {
    unsafe {
        let len = *(s.add(STR_COUNT_OFF) as *const u64) as usize;
        core::slice::from_raw_parts(s.add(STR_DATA_OFF), len)
    }
}

/// `print_str`: write a `String`'s bytes followed by a newline. Returns 0.
///
/// # Safety
/// `s` is a live String; `thread` is the current mutator with published roots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_print_str(thread: *mut Thread, s: *const u8) -> i64 {
    unsafe {
        let bytes = string_owned::copy(thread, s);
        print_host_bytes(thread, &bytes, true, false)
    }
}

/// `print`: write a `String`'s bytes with NO trailing newline, then flush so
/// partial-line output (prompts, progress) appears immediately. Returns 0.
///
/// # Safety
/// As `ai_print_str`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_print_str_raw(thread: *mut Thread, s: *const u8) -> i64 {
    unsafe {
        let bytes = string_owned::copy(thread, s);
        print_host_bytes(thread, &bytes, false, true)
    }
}

/// `str_len`: the byte length of a `String`.
///
/// # Safety
/// `s` must point to a valid gc-rust `String` object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_len(_thread: *mut Thread, s: *const u8) -> i64 {
    unsafe { *(s.add(STR_COUNT_OFF) as *const u64) as i64 }
}

/// `str_eq`: byte-wise equality of two `String`s. Returns 1 if equal, else 0.
///
/// # Safety
/// `a` and `b` must point to valid gc-rust `String` objects; `thread` is their
/// owning RUNNING mutator. Large scans can collect and relocate caller roots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_eq_v2(thread: *mut Thread, a: *const u8, b: *const u8) -> i64 {
    unsafe { string_scan::equal(thread, a, b) }
}

/// Does String `s` hold exactly the `len` bytes at `bytes` (a literal)?
/// The scan is bounded by the literal, so it never parks.
/// # Safety
/// `s` is a valid immutable String; `bytes..bytes+len` is readable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_eq_bytes(_thread: *mut Thread, s: *const u8, bytes: *const u8, len: i64) -> i64 {
    unsafe { (str_bytes(s) == std::slice::from_raw_parts(bytes, len as usize)) as i64 }
}

/// Legacy noncollecting equality ABI. New generated code calls this only for
/// bounded small inputs and uses `ai_str_eq_v2` for potentially large scans.
/// # Safety
/// Both arguments are valid immutable Strings. This function never parks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_eq(_thread: *mut Thread, a: *const u8, b: *const u8) -> i64 {
    unsafe { (str_bytes(a) == str_bytes(b)) as i64 }
}

/// `str_concat`: allocate a fresh `String` (whose varlen `Bytes` shape is the
/// `TypeInfo` at `type_id`) holding `a` followed by `b`.
///
/// The source bytes are copied into a Rust-owned buffer *before* allocating, so
/// a collection triggered by the allocation (which may relocate `a`/`b`) cannot
/// invalidate the copy. Codegen passes the String layout's `type_id` (the same
/// one it uses for string literals), mirroring `ai_gc_alloc_varlen`.
///
/// # Safety
/// `a` and `b` must point to valid gc-rust `String` objects; `thread` valid;
/// `type_id` must index the registered `String` `TypeInfo`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_concat(
    thread: *mut Thread,
    type_id: u32,
    a: *const u8,
    b: *const u8,
) -> *mut u8 {
    unsafe {
        let heap = &*(*thread).heap;
        let limit = string_copy::output_limit(heap).unwrap_or_else(|| {
            abort_with_diagnostic(thread, b"gc-rust: string result exceeds maximum object size")
        });
        let mut buffer = string_copy::Buffer::new(limit);
        let total = str_bytes(a).len().checked_add(str_bytes(b).len()).unwrap_or_else(|| {
            abort_with_diagnostic(thread, b"gc-rust: string result exceeds maximum object size")
        });
        buffer.check(thread, total);
        if total <= string_copy::COPY_BYTES {
            buffer.reserve(thread, total);
            buffer.append(thread, a);
            buffer.append(thread, b);
        } else {
            let roots = (&*(*thread).dyna_thread).scratch_scope();
            let a_root = roots.push(a);
            let b_root = roots.push(b);
            buffer.reserve(thread, total);
            buffer.append(thread, roots.get(a_root));
            buffer.append(thread, roots.get(b_root));
        }
        // Complete UTF-8 sequences preserve validity under concatenation.
        alloc_string_from_utf8_bytes(thread, type_id, &buffer.bytes)
    }
}

/// Allocate a fresh `String` (varlen `Bytes`, `TypeInfo` at `type_id`) holding
/// validated `text`. The caller must hold this UTF-8 text in nonmoving native
/// memory, never borrow it from a GC object across the allocating call.
///
/// # Safety
/// `thread` valid; `type_id` indexes the registered `String` `TypeInfo`.
unsafe fn alloc_string_from_text(thread: *mut Thread, type_id: u32, text: &str) -> *mut u8 {
    unsafe { alloc_string_from_utf8_bytes(thread, type_id, text.as_bytes()) }
}

/// Copy a native buffer whose UTF-8 validity is already established.
/// # Safety
/// `bytes` is valid UTF-8 in nonmoving native memory for this call. The only
/// byte-buffer callers are concat/join: complete valid String sequences and
/// separators preserve UTF-8 under concatenation. Other callers use &str above.
/// `thread` is the owning RUNNING mutator; type_id is its String layout.
unsafe fn alloc_string_from_utf8_bytes(thread: *mut Thread, type_id: u32, bytes: &[u8]) -> *mut u8 {
    unsafe {
        let t = &*thread;
        let heap = &*t.heap;
        let info: &TypeInfo = heap.type_info_by_id(type_id as u16);
        // The thread-local buffer first, as `ai_gc_alloc_varlen` does.
        let fast = match heap.object_request::<Full>(info, bytes.len()) {
            Some(request) => request.try_allocate_local(&*t.dyna_thread),
            None => core::ptr::null_mut(),
        };
        let obj = if fast.is_null() { alloc_with_published_frame(t, heap, info, bytes.len()) } else { fast };
        // Count/header and zero-filled storage are complete before a poll.
        // A partially copied String remains private; GC treats bytes as opaque.
        if bytes.len() <= string_copy::COPY_BYTES {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), obj.add(STR_DATA_OFF), bytes.len());
            return obj;
        }
        let roots = (&*t.dyna_thread).scratch_scope();
        let result_root = roots.push(obj);
        for (index, chunk) in bytes.chunks(string_copy::COPY_BYTES).enumerate() {
            #[cfg(test)]
            if index == 1 { string_copy::tests::output_checkpoint(thread); }
            string_copy::poll(thread);
            let access = heap.object_access(roots.get(result_root));
            core::ptr::copy_nonoverlapping(chunk.as_ptr(), access.as_ptr().add(STR_DATA_OFF + index*string_copy::COPY_BYTES), chunk.len());
        }
        roots.get(result_root)
    }
}

/// Abort after reporting an explicit language panic.
/// # Safety
/// `message` is a live String; `thread` is this OS thread's live RUNNING
/// mutator with published managed roots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_panic(thread: *mut Thread, message: *const u8) -> ! {
    unsafe {
        let text = format!("gc-rust: panic: {}", String::from_utf8_lossy(str_bytes(message)));
        abort_with_diagnostic(thread, text.as_bytes())
    }
}

/// Join a prefix of Array<String> in linear time, with one managed allocation.
/// Stage source bytes in bounded chunks, rooting and reloading each observed
/// String across collection polls. Allocate the managed output after staging.
/// # Safety
/// `array` and `separator` are live managed objects with the stated types.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_join(thread: *mut Thread, type_id: u32, array: *const u8, count: i64, separator: *const u8) -> *mut u8 {
    unsafe {
        let len = *(array.add(16) as *const u64);
        if count < 0 || count as u64 > len { ai_bounds_fail(thread, count, len as i64); }
        let heap = &*(*thread).heap;
        let limit = string_copy::output_limit(heap).unwrap_or_else(|| {
            abort_with_diagnostic(thread, b"gc-rust: string result exceeds maximum object size")
        });
        let mut buffer = string_copy::Buffer::new(limit);
        {
            let roots = (&*(*thread).dyna_thread).scratch_scope();
            let array_root = roots.push(array);
            let separator_root = roots.push(separator);
            for index in 0..count as usize {
                #[cfg(test)]
                if index == 1 { string_copy::tests::input_checkpoint(thread); }
                string_copy::poll(thread);
                if index > 0 { buffer.append(thread, roots.get(separator_root)); }
                let string = slots::ai_slot_bits64_load(
                    thread, roots.get(array_root), STR_DATA_OFF + index * 8,
                ) as *const u8;
                if string.is_null() { ai_uninitialized_array_fail(thread, index as i64); }
                // append pins this exact observed value before any park; never
                // re-read a concurrently mutable array slot after collection.
                buffer.append(thread, string);
            }
            #[cfg(test)]
            string_copy::tests::input_finished(thread);
        }
        alloc_string_from_utf8_bytes(thread, type_id, &buffer.bytes)
    }
}

/// `str_get`: the byte at index `i` (as an i64), or -1 if out of range.
///
/// # Safety
/// `s` must point to a valid gc-rust `String` object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_get(_thread: *mut Thread, s: *const u8, i: i64) -> i64 {
    unsafe {
        let bytes = str_bytes(s);
        if i < 0 || i as usize >= bytes.len() { -1 } else { bytes[i as usize] as i64 }
    }
}

/// `str_substring`: a fresh `String` with bytes `[start, end)` of `s`. Indices
/// are clamped to `[0, len]`; reversed ranges become empty. Endpoints must be
/// UTF-8 character boundaries, so a String never contains a partial code point.
///
/// # Safety
/// `s` valid `String`; `thread`/`type_id` as `ai_str_concat`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_substring(
    thread: *mut Thread,
    type_id: u32,
    s: *const u8,
    start: i64,
    end: i64,
) -> *mut u8 {
    unsafe {
        let text = string_owned::range(thread, s, start, end).unwrap_or_else(|error| {
            let message: &[u8] = match error {
                string_range::RangeError::Boundary => b"gc-rust: string byte range is not on UTF-8 character boundaries",
                string_range::RangeError::InvalidUtf8 => b"gc-rust: invalid UTF-8 String",
            };
            abort_with_diagnostic(thread, message)
        });
        alloc_string_from_text(thread, type_id, &text)
    }
}

/// `str_from_bytes(array, len)`: a fresh `String` holding the first `len`
/// bytes of a `u8` array. Invalid UTF-8 aborts; `String::from_utf8` checks
/// the bytes first.
///
/// # Safety
/// `array` is a live `Array<u8>` with at least `len` elements; `thread` and
/// `type_id` as `ai_str_concat`. May collect.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_from_bytes(thread: *mut Thread, type_id: i64, array: *const u8, len: i64) -> *mut u8 {
    unsafe {
        with_array_bytes(thread, array, len, |bytes| match std::str::from_utf8(bytes) {
            Ok(text) => alloc_string_from_text(thread, type_id as u32, text),
            Err(_) => abort_with_diagnostic(thread, b"gc-rust: invalid UTF-8 bytes for a String"),
        })
    }
}

/// `str_from_utf8_prefix(array, len)`: a fresh `String` holding the longest
/// valid UTF-8 prefix of the first `len` bytes of a `u8` array
/// (`String::from_utf8` compares its length with `len`).
///
/// # Safety
/// As `ai_str_from_bytes`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_from_utf8_prefix(thread: *mut Thread, type_id: i64, array: *const u8, len: i64) -> *mut u8 {
    unsafe {
        with_array_bytes(thread, array, len, |bytes| {
            let valid = if is_ascii(bytes) { bytes.len() } else {
                match std::str::from_utf8(bytes) { Ok(_) => bytes.len(), Err(e) => e.valid_up_to() }
            };
            alloc_string_from_utf8_bytes(thread, type_id as u32, &bytes[..valid])
        })
    }
}

/// Are all of `bytes` ASCII? Eight at a time; short identifiers are common.
fn is_ascii(bytes: &[u8]) -> bool {
    let mut words = bytes.chunks_exact(8);
    let mut high = 0u64;
    for word in &mut words { high |= u64::from_ne_bytes(word.try_into().unwrap()); }
    for byte in words.remainder() { high |= *byte as u64; }
    high & 0x8080_8080_8080_8080 == 0
}

/// Run `body` on a native copy of the first `len` bytes of a `u8` array,
/// which stays valid while `body` allocates. Short arrays use the stack.
unsafe fn with_array_bytes<R>(thread: *mut Thread, array: *const u8, len: i64, body: impl FnOnce(&[u8]) -> R) -> R {
    const STACK: usize = 256;
    let len = len.max(0) as usize;
    if len <= STACK {
        // A short copy needs no polls: one access, then the bytes.
        let mut buffer = core::mem::MaybeUninit::<[u8; STACK]>::uninit();
        let start = buffer.as_mut_ptr().cast::<u8>();
        if len > 0 { unsafe { ai_buf_copy_in(thread, array, start, len as i64, 8); } }
        return body(unsafe { core::slice::from_raw_parts(start, len) });
    }
    let mut bytes = vec![0u8; len];
    unsafe { ffi_copy::scalar(thread, array, bytes.as_mut_ptr(), len as i64, 8, false); }
    body(&bytes)
}

/// `str_copy_bytes(s, array)`: copy the bytes of `s` into the first
/// `s.len()` elements of a `u8` array.
///
/// # Safety
/// `s` is a live `String` and `array` a live `Array<u8>` with at least
/// `s.len()` elements; `thread` is the RUNNING mutator. May collect.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_copy_bytes(thread: *mut Thread, s: *const u8, array: *mut u8) {
    unsafe {
        let text = string_owned::text(thread, s).unwrap_or_default();
        if !text.is_empty() {
            ffi_copy::scalar(thread, array, text.as_ptr().cast_mut(), text.len() as i64, 8, true);
        }
    }
}

/// `read_file`: UTF-8 text from the file at `path` (empty on I/O or decoding
/// failure). Binary files cannot manufacture invalid String representations.
///
/// # Safety
/// `path` valid `String`; `thread`/`type_id` as `ai_str_concat`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_read_file(thread: *mut Thread, type_id: u32, path: *const u8) -> *mut u8 {
    unsafe {
        let path_str = string_owned::text(thread, path).unwrap_or_default();
        let content = blocking_region(thread, || std::fs::read_to_string(&path_str).unwrap_or_default());
        alloc_string_from_text(thread, type_id, &content)
    }
}

/// `str_from_int`: a fresh `String` holding the decimal rendering of `v`.
///
/// # Safety
/// `thread`/`type_id` as `ai_str_concat`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_from_int(thread: *mut Thread, type_id: u32, v: i64) -> *mut u8 {
    unsafe {
        let s = v.to_string();
        alloc_string_from_text(thread, type_id, &s)
    }
}

/// `type_name_of(obj)`: a fresh `String` holding the source type name of `obj`,
/// looked up in the heap's reflection metadata table by the object's header
/// `type_id`. `str_type_id` is the `String` layout id used to allocate the
/// result. Falls back to `<type N>` when no metadata is installed for the type.
///
/// # Safety
/// `thread` valid with a live heap; `obj` a valid heap object with an
/// initialized header; `str_type_id` the registered `String` layout.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_type_name(thread: *mut Thread, str_type_id: u32, obj: *const u8) -> *mut u8 {
    unsafe {
        let heap = &*(*thread).heap;
        let obj_tid = ai_object_type_id(thread, obj);
        let name = heap
            .type_meta_by_id(obj_tid)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| format!("<type {}>", obj_tid));
        alloc_string_from_text(thread, str_type_id, &name)
    }
}

/// `char_to_str`: a fresh `String` holding the UTF-8 encoding of the Unicode
/// scalar value `cp` (1–4 bytes). Invalid scalars — negative, above U+10FFFF, or
/// a surrogate (U+D800..U+DFFF) — become U+FFFD, so this is total and always
/// produces valid UTF-8.
///
/// # Safety
/// `thread`/`type_id` as `ai_str_concat`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_char_to_str(thread: *mut Thread, type_id: u32, cp: i64) -> *mut u8 {
    let ch = u32::try_from(cp).ok().and_then(char::from_u32).unwrap_or('\u{FFFD}');
    let mut buf = [0u8; 4];
    let s = ch.encode_utf8(&mut buf);
    unsafe { alloc_string_from_text(thread, type_id, s) }
}

/// `str_from_float`: a fresh `String` holding the rendering of `v`.
///
/// # Safety
/// `thread`/`type_id` as `ai_str_concat`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_from_float(thread: *mut Thread, type_id: u32, v: f64) -> *mut u8 {
    unsafe {
        let s = v.to_string();
        alloc_string_from_text(thread, type_id, &s)
    }
}

/// `fmt_f64`: format a float the way Rust's `format!` does. `mode` 0 is
/// Display (`{}`), 1 is Debug (`{:?}`), 2 is `{:e}` and 3 is `{:E}`; a
/// negative `prec` means no precision was written.
///
/// # Safety
/// As [`ai_str_from_float`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_fmt_f64(thread: *mut Thread, type_id: i64, v: f64, prec: i64, mode: i64) -> *mut u8 {
    let p = prec.clamp(0, 1000) as usize;
    let s = match (mode, prec >= 0) {
        (1, false) => format!("{v:?}"),
        (1, true) => format!("{v:.p$?}"),
        (2, false) => format!("{v:e}"),
        (2, true) => format!("{v:.p$e}"),
        (3, false) => format!("{v:E}"),
        (3, true) => format!("{v:.p$E}"),
        (_, false) => format!("{v}"),
        (_, true) => format!("{v:.p$}"),
    };
    unsafe { alloc_string_from_text(thread, type_id as u32, &s) }
}

/// `str_hash`: a stable 63-bit FNV-1a hash of a `String`'s bytes (non-negative
/// so it can be used as an i64 array index after a modulo). Does not allocate.
///
/// # Safety
/// `s` must point to a valid gc-rust `String` object; `thread` is its owning
/// RUNNING mutator. Large scans can collect and relocate caller roots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_hash_v2(thread: *mut Thread, s: *const u8) -> i64 {
    unsafe { string_scan::hash(thread, s) }
}

/// Legacy noncollecting FNV ABI. Current generated code calls this only for
/// bounded small inputs; larger scans use `ai_str_hash_v2`.
/// # Safety
/// `s` is a valid immutable String. This function never parks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_hash(_thread: *mut Thread, s: *const u8) -> i64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &byte in unsafe { str_bytes(s) } {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    (hash & 0x7fff_ffff_ffff_ffff) as i64
}

/// `str_to_float`: parse a `String` as f64, returning 0.0 on a malformed input.
/// Does not allocate.
///
/// # Safety
/// `s` must point to a valid gc-rust `String` object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_to_float(_thread: *mut Thread, s: *const u8) -> f64 {
    unsafe {
        match core::str::from_utf8(str_bytes(s)) {
            Ok(text) => text.trim().parse::<f64>().unwrap_or(0.0),
            Err(_) => 0.0,
        }
    }
}

/// Collecting float-parse ABI for generated large String inputs. Existing
/// unversioned callers retain their noncollecting contract.
/// # Safety
/// `thread` is this OS thread's RUNNING mutator; `s` is its valid immutable String.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_str_to_float_v2(thread: *mut Thread, s: *const u8) -> f64 {
    unsafe { float_parse::parse(thread, s) }
}

/// Safepoint poll slow path. Compiled code inlines a load of `thread.state` at
/// loop back-edges; when it is non-zero, it traps here. We publish our frame
/// chain and park at a safepoint until the GC that requested the stop completes,
/// then return so the mutator resumes.
///
/// # Safety
/// `thread` must be a valid `*mut Thread` with a live `dyna_thread`.
/// Additional registrations on this OS thread for the heap must publish their
/// roots before this call and reload derived pointers afterward.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_gc_pollcheck_slow(thread: *mut Thread) {
    unsafe {
        let t = &*thread;
        let dyna = &*t.dyna_thread;
        dyna.set_parked_jit_fp(t.top_frame as *const u8);
        (&*t.heap).safepoint(dyna);
        dyna.clear_parked_jit_fp();
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;

    #[test]
    fn completed_native_thread_can_be_observed_repeatedly() {
        let native = std::thread::spawn(|| 81);
        // Exercise completion before the first observer, without timing sleeps.
        while !native.is_finished() {
            std::thread::yield_now();
        }
        let completion = JoinHandle {
            completion_notice: false,
            native_joined: std::sync::atomic::AtomicBool::new(false),
            pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
            reaper_registration: std::sync::OnceLock::new(),
            retirement: std::sync::OnceLock::new(),
            state: Mutex::new(JoinState::Running(native)),
            owner: std::sync::atomic::AtomicUsize::new(1),
        };
        assert_eq!(completion.join(), 81);
        assert_eq!(completion.join(), 81);
        assert!(matches!(
            *completion.state.lock().unwrap(),
            JoinState::Complete(81)
        ));
    }

    #[test]
    fn atomic_cells_keep_stable_aliases_and_retire_with_root_execution() {
        let mut context = RuntimeContext::new(1 << 20, vec![]);
        let heap = context.heap().clone();
        let weak = context.enter(|thread| {
        let first = unsafe { ai_atomic_i64_new(thread, 7) };
        let weak = Arc::downgrade(&heap.atomic_cells.lock().unwrap().records[&(first as usize)].0);
        // Force registry growth; previously returned addresses must stay valid.
        for n in 0..1024 {
            unsafe { ai_atomic_i64_new(thread, n) };
        }
        let alias = first;
        unsafe {
            assert_eq!(ai_atomic_i64_fetch_add(thread, first, 3), 7);
            assert_eq!(ai_atomic_i64_load(thread, alias), 10);
        }
        weak
        });
        drop(context);
        assert!(heap.atomic_cells.lock().unwrap().is_empty());
        assert!(heap.atomic_cells.lock().unwrap().records.capacity() <= 8);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn retired_runtime_detaches_poll_storage_with_retained_state() {
        let context = RuntimeContext::new(1 << 20, vec![]);
        let state = context.thread.registration.dyna.clone();
        drop(context);
        // Safe coordinator operations must not write a freed Thread::state.
        state.request_poll();
        state.clear_poll();
    }

    #[test]
    fn runtime_retirement_after_inspection_unwind_detaches_poll_storage() {
        let context = RuntimeContext::new(1 << 20, vec![]);
        let state = context.thread.registration.dyna.clone();
        let heap = context.heap().clone();
        // Keep the target alive even on the broken path: this demonstrates the
        // dangling registration without ever accessing a freed Thread box.
        let mut target = std::sync::atomic::AtomicU8::new(0);
        unsafe { state.set_poll_flag(&mut target); }
        let inspection = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _pause = heap.pause_world();
            panic!("injected inspection unwind");
        }));
        assert!(inspection.is_err());
        let retirement = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(context)));
        state.request_poll();
        let stale_target_written = target.load(Ordering::Acquire) != 0;
        unsafe { state.set_poll_flag(std::ptr::null_mut()); }
        assert!(!stale_target_written, "retired state still writes its former target");
        assert!(retirement.is_ok(), "inspection unwind poisoned runtime retirement");
    }

    #[test]
    fn runtime_retirement_during_outer_unwind_preserves_coordinator() {
        let context = RuntimeContext::new(1 << 20, vec![]);
        let heap = context.heap().clone();
        let state = context.thread.registration.dyna.clone();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _context = context;
            panic!("injected outer unwind");
        }));
        assert!(unwind.is_err());
        state.request_poll();
        state.clear_poll();
        // Successful cleanup during an outer unwind must leave a retained heap
        // ready for later registrations and inspection.
        let (next, _) = heap.register_thread();
        unsafe { heap.safe_deregister_thread(&next); }
        let _pause = heap.pause_world();
    }

    #[test]
    fn channel_native_metadata_retires_after_execution_with_retained_heap() {
        let array = TypeInfo::for_header(16).with_varlen_values(0).with_type_id(0);
        let message = TypeInfo::for_header(16).with_raw_bytes(8).with_type_id(1);
        let mut context = RuntimeContext::new(1 << 20, vec![array, message]);
        let heap = context.heap().clone();
        let weak = context.enter(|thread| {
        let witness = unsafe { ai_gc_alloc_varlen(thread, 0, 0, 0) };
        let handle = unsafe { ai_chan_new(thread, 1, witness, 1) };
        let weak = Arc::downgrade(&heap.channels.lock().unwrap().records[&(handle as usize)].0);
        for _ in 0..128 { unsafe { ai_chan_new(thread, 2, witness, 1); } }
        // Registry growth cannot invalidate a borrowed handle or its slots.
        unsafe { ai_chan_close(thread, handle); ai_chan_close(thread, handle); }
        assert!(unsafe { &*handle }.state.lock().unwrap().closed);
        assert!(unsafe { &*handle }.slots.read().unwrap().is_none());
        weak
        });
        drop(context);
        assert!(heap.channels.lock().unwrap().is_empty());
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn teardown_releases_completion_records_even_if_embedder_keeps_heap() {
        let context = RuntimeContext::new(1 << 20, vec![]);
        let heap = context.heap().clone();
        let completion = Arc::new(JoinHandle {
            completion_notice: false,
            native_joined: std::sync::atomic::AtomicBool::new(false),
            pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
            reaper_registration: std::sync::OnceLock::new(),
            retirement: std::sync::OnceLock::new(),
            state: Mutex::new(JoinState::Running(std::thread::spawn(|| 42))),
            owner: std::sync::atomic::AtomicUsize::new(1),
        });
        let weak = Arc::downgrade(&completion);
        heap.thread_completions.lock().unwrap().register(completion, &heap.native_retirement);
        drop(context);
        assert!(heap.thread_completions.lock().unwrap().records.is_empty());
        assert!(weak.upgrade().is_none());
    }
}

#[cfg(test)]
mod current_thread_tests {
    use super::*;

    #[test]
    fn current_thread_binding_restores_nested_execution_and_unwinding() {
        let mut outer = RuntimeContext::new(1 << 16, vec![]);
        let mut inner = RuntimeContext::new(1 << 16, vec![]);
        let outer_thread = outer.thread_ptr();
        let inner_thread = inner.thread_ptr();
        assert!(unsafe { ai_current_thread() }.is_null());
        unsafe {
            with_current_thread(outer_thread, || {
                assert_eq!(ai_current_thread(), outer_thread);
                with_current_thread(inner_thread, || {
                    assert_eq!(ai_current_thread(), inner_thread);
                    assert!(outer.thread.registration.dyna.is_blocked());
                });
                assert!(!outer.thread.registration.dyna.is_blocked());
                assert_eq!(ai_current_thread(), outer_thread);
                with_current_thread(outer_thread, || assert!(!outer.thread.registration.dyna.is_blocked()));
                blocking_region(outer_thread, || {
                    with_current_thread(inner_thread, || assert!(outer.thread.registration.dyna.is_blocked()));
                    assert!(outer.thread.registration.dyna.is_blocked());
                });
                assert!(!outer.thread.registration.dyna.is_blocked());
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    with_current_thread(inner_thread, || panic!("test unwind"));
                }));
                assert!(result.is_err());
                assert_eq!(ai_current_thread(), outer_thread);
            });
            assert!(ai_current_thread().is_null());
        }
    }

    #[test]
    fn current_thread_binding_allows_outer_heap_collection_during_nested_execution() {
        let mut outer = RuntimeContext::new(1 << 16, vec![]);
        let mut inner = RuntimeContext::new(1 << 16, vec![]);
        let outer_thread = outer.thread_ptr();
        let inner_thread = inner.thread_ptr();
        unsafe {
            with_current_thread(outer_thread, || {
                with_current_thread(inner_thread, || {
                    assert!(outer.thread.registration.dyna.is_blocked());
                    let heap = outer.heap().clone();
                    std::thread::spawn(move || heap.stw_collect::<crate::gc::IdentityPtrPolicy>()).join().unwrap();
                    assert_eq!(outer.heap().collections(), 1);
                    assert_eq!(ai_current_thread(), inner_thread);
                });
                assert!(!outer.thread.registration.dyna.is_blocked());
                assert_eq!(ai_current_thread(), outer_thread);
            });
        }
    }

    #[test]
    fn current_thread_binding_is_local_to_the_os_thread() {
        let mut context = RuntimeContext::new(1 << 16, vec![]);
        let thread = context.thread_ptr();
        unsafe {
            with_current_thread(thread, || {
                std::thread::spawn(|| assert!(ai_current_thread().is_null())).join().unwrap();
                assert_eq!(ai_current_thread(), thread);
            });
        }
    }
}

#[cfg(test)]
mod snapshot_coordination_tests {
    use super::*;

    #[test]
    fn snapshot_waiting_for_collector_publishes_roots_and_parks() {
        const HELPER: &str = "GCR_SNAPSHOT_COORDINATION_HELPER";
        if let Some(operation) = std::env::var_os(HELPER) {
            let mut runtime = RuntimeContext::new(1 << 16, vec![]);
            let thread = runtime.thread_ptr();
            let execute = || {
            let extra = if operation == "heap" || operation == "deregister" { Some(runtime.heap().register_thread().0) } else { None };
            let heap = runtime.heap().clone();
            let collector = std::thread::spawn(move || unsafe { heap.stw_collect::<crate::gc::IdentityPtrPolicy>() });
            // The collector owns gc_lock and is waiting for this mutator.
            while !runtime.heap().gc_requested() { std::thread::yield_now(); }
            if operation == "deregister" {
                unsafe { runtime.heap().safe_deregister_thread(extra.as_ref().unwrap()); }
            } else if operation == "register" {
                let (new, _) = runtime.heap().register_thread();
                assert!(!new.is_blocked());
                unsafe { runtime.heap().safe_deregister_thread(&new); }
            } else if operation == "heap" {
                // Profiling calls pause_world directly through the safe Rust API.
                assert!(runtime.heap().alloc_site_profile().is_empty());
            } else {
                unsafe { ai_heap_snapshot(thread); }
            }
            collector.join().unwrap();
            assert_eq!(runtime.heap().collections(), 1);
            assert!(!runtime.thread.registration.dyna.is_blocked());
            if let Some(extra) = extra {
                assert!(!extra.is_blocked());
                unsafe { runtime.heap().safe_deregister_thread(&extra); }
            }
            };
            return unsafe { with_current_thread(thread, execute) };
        }
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        for operation in ["runtime", "heap", "register", "deregister"] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .arg("--exact").arg("runtime::snapshot_coordination_tests::snapshot_waiting_for_collector_publishes_roots_and_parks")
                .env(HELPER, operation).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if child.try_wait().unwrap().is_some() { break; }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!("snapshot waited for gc_lock without parking: {output:?}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
        }
    }
}

#[cfg(all(test, unix))]
mod diagnostic_output_tests {
    #[test]
    fn diagnostic_write_failure_aborts_without_invoking_the_rust_panic_hook() {
        use std::process::{Command, Stdio};
        use std::os::unix::process::ExitStatusExt;
        use std::time::{Duration, Instant};
        const HELPER: &str = "GCR_DIAGNOSTIC_FAILURE_HELPER";
        if let Some(marker) = std::env::var_os(HELPER) {
            std::panic::set_hook(Box::new(move |_| { let _ = std::fs::write(&marker, "Rust panic"); }));
            unsafe { super::ai_arithmetic_fail(std::ptr::null_mut(), 0) }
        }
        let marker = std::env::temp_dir().join(format!("gcr-diagnostic-panic-hook-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runtime::diagnostic_output_tests::diagnostic_write_failure_aborts_without_invoking_the_rust_panic_hook"])
            .env(HELPER, &marker).stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::open("/dev/null").unwrap())).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() { break status; }
            if Instant::now() >= deadline {
                child.kill().unwrap(); child.wait().unwrap(); panic!("diagnostic output failure hung");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.signal(), Some(6));
        assert!(!marker.exists(), "output error invoked a Rust panic across the C boundary");
    }
}

#[cfg(test)]
mod mutator_collection_coordination_tests {
    use super::*;
    use crate::gc::ObjHeader;

    #[test]
    fn collector_requests_park_same_os_thread_siblings_and_preserve_roots() {
        const HELPER: &str = "GCR_MUTATOR_COLLECTION_HELPER";
        if let Ok(operation) = std::env::var(HELPER) {
            let minor = operation.starts_with("minor");
            let contended = operation.contains("contended");
            let paused = operation.contains("paused");
            let stress = operation.ends_with("stress");
            let info = TypeInfo::for_header(Full::SIZE).with_type_id(0).with_raw_bytes(8);
            let runtime = RuntimeContext::new_generational(1 << 16, 1 << 16, vec![info.clone()]);
            runtime.heap().set_gc_every_alloc(stress);
            let (sibling, _) = runtime.heap().register_thread();
            let (blocked, _) = runtime.heap().register_thread();
            unsafe { blocked.enter_blocked(); }
            let object = unsafe {
                if minor { runtime.heap().alloc_nursery_obj::<Full>(&info, 0) }
                else { runtime.heap().alloc_obj::<Full>(&info, 0) }
            };
            assert!(!object.is_null());
            unsafe { object.add(16).cast::<u64>().write(42); }
            let scope = crate::gc::roots::RootScope::new(sibling.frame_chain(), 1);
            let root = unsafe { scope.root::<u8>(object as u64) };
            let collector = if contended || paused {
                let heap = runtime.heap().clone();
                let collector = std::thread::spawn(move || {
                    if paused { drop(heap.pause_world()); }
                    else { unsafe { heap.stw_collect::<IdentityPtrPolicy>(); } }
                });
                while !runtime.heap().gc_requested() { std::thread::yield_now(); }
                Some(collector)
            } else { None };
            unsafe {
                if minor { runtime.heap().mutator_triggered_minor_gc::<IdentityPtrPolicy>(&runtime.thread.registration.dyna); }
                else { runtime.heap().mutator_triggered_gc::<IdentityPtrPolicy>(&runtime.thread.registration.dyna); }
            }
            if let Some(collector) = collector { collector.join().unwrap(); }
            assert!(runtime.thread.registration.dyna.is_blocked());
            assert!(!sibling.is_blocked());
            assert!(blocked.is_blocked());
            let moved = root.get() as *const u8;
            assert_eq!(unsafe { moved.add(16).cast::<u64>().read() }, 42);
            if minor { assert_eq!(runtime.heap().minor_collections(), 1); }
            else { assert_eq!(runtime.heap().collections(), if contended && stress { 2 } else { 1 }); }
            drop(scope);
            unsafe {
                runtime.heap().safe_deregister_thread(&sibling);
                runtime.heap().safe_deregister_thread(&blocked);
            }
            return;
        }
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        for operation in ["major", "minor", "major-contended", "minor-contended",
            "major-stress", "minor-stress", "major-contended-stress", "minor-contended-stress",
            "major-paused", "minor-paused", "major-paused-stress", "minor-paused-stress"] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "runtime::mutator_collection_coordination_tests::collector_requests_park_same_os_thread_siblings_and_preserve_roots"])
                .env(HELPER, operation).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!("{operation}: same-thread collection deadlock: {output:?}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{operation}: {output:?}");
        }
    }
}

#[cfg(test)]
mod atom_tests;
#[cfg(test)]
mod concurrent_allocation_tests;
#[cfg(test)]
mod large_allocation_tests;
#[cfg(test)]
mod promotion_capacity_tests;

#[cfg(test)]
mod thread_handoff_tests {
    use super::*;
    use crate::gc::{Full, IdentityPtrPolicy, ObjHeader};

    #[test]
    fn collector_thread_handoff_drop_allows_pending_major_and_minor_collection() {
        if !channel_buffer::tests::child(
            "runtime::thread_handoff_tests::collector_thread_handoff_drop_allows_pending_major_and_minor_collection",
        ) { return; }
        let generational = std::env::var("GCR_CHANNEL_BUFFER_CHILD").as_deref() == Ok("1");
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let mut context = if generational {
            RuntimeContext::new_generational(32768, 2 << 20, vec![info])
        } else { RuntimeContext::new(2 << 20, vec![info]) };
        let heap = context.heap().clone();
        context.enter(|thread| unsafe {
            let roots = (&*(*thread).dyna_thread).scratch_scope();
            let witness = roots.push(ai_gc_alloc_fixed(thread, 0, 0));
            roots.get(witness).add(Full::SIZE).cast::<u64>().write(42);
            let handoff = ThreadHandoff::new(thread, heap.clone(), roots.get(witness));
            let slot = handoff.slot.unwrap();
            let before = if generational { heap.minor_collections() } else { heap.collections() };
            let other = heap.clone();
            let collector = std::thread::spawn(move || {
                let (state, _) = other.register_thread();
                if generational { other.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state); }
                else { other.mutator_triggered_gc::<IdentityPtrPolicy>(&state); }
                other.safe_deregister_thread(&state);
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !heap.gc_requested() {
                assert!(std::time::Instant::now() < deadline, "collector never requested the handoff owner");
                std::thread::yield_now();
            }
            drop(handoff);
            blocking_region(thread, || collector.join().unwrap());
            assert_eq!(heap.globals.get(slot), 0);
            assert_eq!(heap.thread_handoff_slots.lock().unwrap().as_slice(), &[slot]);
            assert_eq!(roots.get(witness).add(Full::SIZE).cast::<u64>().read(), 42);
            assert_eq!(if generational { heap.minor_collections() } else { heap.collections() }, before + 1);
        });
    }

    #[test]
    fn collector_thread_handoff_slots_reuse_after_rooted_transfer() {
        const CHILD: &str = "GCR_HANDOFF_REUSE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            use std::process::{Command, Stdio};
            use std::time::{Duration, Instant};
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "runtime::thread_handoff_tests::collector_thread_handoff_slots_reuse_after_rooted_transfer", "--nocapture"])
                .env(CHILD, "1").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    panic!("handoff reuse hung: {:?}", child.wait_with_output().unwrap());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        unsafe extern "C" fn collect_and_read(thread: *mut Thread, env: *mut u8) -> u64 {
            unsafe {
                let state = &*(*thread).dyna_thread;
                let roots = state.scratch_scope();
                let slot = roots.push(env);
                (&*(*thread).heap).mutator_triggered_gc::<IdentityPtrPolicy>(state);
                roots.get(slot).add(Full::SIZE).cast::<u64>().read()
            }
        }
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let mut context = RuntimeContext::new(65536, vec![info.clone()]);
        let heap = context.heap().clone();
        let thread = context.thread_ptr();
        let execute = || {
        let env = unsafe { heap.alloc_obj::<Full>(&info, 0) };
        unsafe { env.add(Full::SIZE).cast::<u64>().write(42); }
        let root = unsafe { heap.globals.add(env as u64) };
        let initial = heap.globals.len();
        for _ in 0..64 {
            let env = heap.globals.get(root) as *mut u8;
            let handle = unsafe { ai_thread_spawn(thread, env, collect_and_read as *const () as *const u8) };
            assert_eq!(unsafe { ai_thread_join(thread, handle) }, 42);
            assert_eq!(heap.globals.len(), initial + 1, "handoff roots grew with completed spawns");
            let vacant = heap.thread_handoff_slots.lock().unwrap();
            assert_eq!(vacant.len(), 1);
            assert_eq!(heap.globals.get(vacant[0]), 0);
        }
        assert_eq!(heap.collections(), 64);
        };
        unsafe { with_current_thread(thread, execute); }
    }
}

#[cfg(test)]
#[path = "runtime/completion_owner_tests.rs"]
mod completion_owner_tests;

#[cfg(test)]
#[path = "runtime/atomic_owner_tests.rs"]
mod atomic_owner_tests;

#[cfg(test)]
#[path = "runtime/channel_owner_tests.rs"]
mod channel_owner_tests;

#[cfg(test)]
#[path = "runtime/completion_reaper_tests.rs"]
mod completion_reaper_tests;

#[cfg(test)]
#[path = "runtime/embedding_wait_tests.rs"]
mod embedding_wait_tests;

#[cfg(test)]
mod native_pressure_tests;

#[cfg(test)]
#[path = "runtime/varlen_fast_tests.rs"]
mod varlen_fast_tests;

#[path = "runtime/string_range.rs"]
mod string_range;

#[path = "runtime/string_copy.rs"]
mod string_copy;
#[path="runtime/string_scan.rs"]
mod string_scan;
#[path="runtime/string_owned.rs"]
mod string_owned;
#[path="runtime/float_parse.rs"]
mod float_parse;
#[path="runtime/ffi_copy.rs"]
mod ffi_copy;
#[path="runtime/ffi_buffer.rs"]
mod ffi_buffer;
#[path="runtime/channel_buffer.rs"]
mod channel_buffer;

/// Maximum work per noncollecting scan selected by current generated code.
/// The legacy scan symbols remain noncollecting for older compiled callers.
pub const STRING_SCAN_LEAF_BYTES: usize = string_copy::COPY_BYTES;

#[path = "runtime/channel_scan.rs"]
mod channel_scan;

#[cfg(test)]
#[path = "runtime/admission_profile.rs"]
mod admission_profile;

#[cfg(test)]
mod abi_storage_tests {
    use super::*;

    #[inline(never)]
    fn move_context(context: RuntimeContext) -> RuntimeContext {
        std::hint::black_box(context)
    }

    #[test]
    fn collector_runtime_thread_storage_survives_owner_moves_and_polling() {
        let mut context = RuntimeContext::new(4096, vec![]);
        let address = context.thread_ptr();
        for _ in 0..8 {
            let mut owners = Vec::new();
            owners.push(move_context(context));
            owners.reserve_exact(64);
            context = move_context(owners.pop().unwrap());
            assert_eq!(context.thread_ptr(), address);
            context.thread.registration.dyna.request_poll();
            assert_eq!(unsafe { (*address).state.load(Ordering::Acquire) }, 1);
            context.thread.registration.dyna.clear_poll();
            assert_eq!(unsafe { (*address).state.load(Ordering::Acquire) }, 0);
        }
    }
}

#[cfg(test)]
mod blocking_sibling_tests {
    use super::*;
    use crate::gc::MutatorThread;

    #[test]
    fn collector_blocking_region_preserves_sibling_scopes_on_unwind() {
        let mut runtime = RuntimeContext::new(4096, vec![]);
        let heap = runtime.heap().clone();
        runtime.enter(|thread| unsafe {
            let sibling = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
            let blocked = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
            let origin = FrameOrigin::new(0, core::ptr::null());
            let mut caller_frame = Frame { parent: core::ptr::null_mut(), origin: &origin };
            let blocked_frame = Frame { parent: core::ptr::null_mut(), origin: &origin };
            let blocked_fp = &blocked_frame as *const Frame as *const u8;
            blocked.state().set_parked_jit_fp(blocked_fp);
            blocked.state().enter_blocked();
            (*thread).top_frame = &mut caller_frame;
            let caller = &*(*thread).dyna_thread;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                blocking_region(thread, || {
                    assert!(caller.is_blocked() && sibling.state().is_blocked());
                    assert!(blocked.state().is_blocked());
                    assert_eq!(caller.parked_jit_fp(), &caller_frame as *const Frame as *const u8);
                    assert_eq!(blocked.state().parked_jit_fp(), blocked_fp);
                    blocking_region(thread, || {
                        assert!(caller.is_blocked() && sibling.state().is_blocked());
                        assert_eq!(caller.parked_jit_fp(), &caller_frame as *const Frame as *const u8);
                    });
                    panic!("injected native body unwind");
                });
            }));
            assert!(result.is_err());
            assert!(caller.is_running() && sibling.state().is_running());
            assert!(caller.parked_jit_fp().is_null());
            assert!(blocked.state().is_blocked());
            assert_eq!(blocked.state().parked_jit_fp(), blocked_fp);
            (*thread).top_frame = core::ptr::null_mut();
            blocked.state().exit_blocked(&heap);
            blocked.state().clear_parked_jit_fp();
        });
    }
}

#[cfg(test)]
mod foreign_scope_tests {
    use super::*;
    use crate::gc::{MutatorThread, ObjHeader};

    #[test]
    fn collector_foreign_scopes_transfer_siblings_through_nested_callbacks() {
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let mut runtime = RuntimeContext::new(4096, vec![info]);
        let heap = runtime.heap().clone();
        let counter = heap.native_memory.registry.clone();
        runtime.enter(|thread| unsafe {
            let sibling = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
            let blocked = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
            blocked.state().enter_blocked();
            let caller = &*(*thread).dyna_thread;
            let object = ai_gc_alloc_fixed(thread, 0, 0);
            object.add(Full::SIZE).cast::<u64>().write(42);
            let scope = crate::gc::roots::RootScope::new(sibling.frame_chain(), 1);
            let root = scope.root::<u8>(object as u64);
            let check_root = || {
                let pointer = root.get() as *const u8;
                assert!(heap.from_space_contains(pointer));
                assert_eq!(pointer.add(Full::SIZE).cast::<u64>().read(), 42);
                assert!(blocked.state().is_blocked());
            };
            ai_ffi_enter(thread);
            assert!(caller.is_blocked() && sibling.state().is_blocked());
            heap.stw_collect::<IdentityPtrPolicy>();
            check_root();
            ai_ffi_reenter(thread);
            assert!(caller.is_running() && sibling.state().is_running());
            heap.mutator_triggered_gc::<IdentityPtrPolicy>(caller);
            check_root();
            ai_ffi_enter(thread);
            assert!(caller.is_blocked() && sibling.state().is_blocked());
            ai_ffi_reenter(thread);
            assert!(caller.is_running() && sibling.state().is_running());
            heap.mutator_triggered_gc::<IdentityPtrPolicy>(caller);
            check_root();
            ai_ffi_exit(thread);
            assert!(caller.is_blocked() && sibling.state().is_blocked());
            ai_ffi_leave(thread);
            assert!(caller.is_running() && sibling.state().is_running());
            ai_ffi_exit(thread);
            assert!(caller.is_blocked() && sibling.state().is_blocked());
            heap.stw_collect::<IdentityPtrPolicy>();
            check_root();
            ai_ffi_leave(thread);
            assert!(caller.is_running() && sibling.state().is_running());
            assert!(caller.parked_jit_fp().is_null());
            check_root();
            // Warmed calls reuse their accounted native scope backing.
            let before = counter.bytes();
            allocation_probe::start();
            ai_ffi_enter(thread);
            ai_ffi_leave(thread);
            assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            assert_eq!(counter.bytes(), before);
            blocked.state().exit_blocked(&heap);
        });
        drop(runtime);
        assert_eq!(counter.bytes(), heap.collector_object_lock_backing_bytes() + heap.collector_layout_backing_bytes() + heap.collector_abi_backing_bytes() + heap.retired_profile_backing_bytes() + heap.collector_worker_backing_bytes() + unsafe { heap.collector_tlab_metadata_bytes() },
            "only heap-owned layout table, lock array, profile totals, worker and TLAB registry backing may survive runtime retirement");
        drop(heap);
        assert_eq!(counter.bytes(), 0);
    }
}

#[cfg(test)]
#[path = "runtime/channel_sibling_tests.rs"]
mod channel_sibling_tests;
