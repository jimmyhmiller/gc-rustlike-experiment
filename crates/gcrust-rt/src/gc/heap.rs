#[path = "coordinator.rs"]
mod coordinator;
use coordinator::{Coordinator, Guard as CoordinatorGuard};
pub use parallel::{CollectorScratchStats, collector_scratch_stats};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use crate::gc::failure::abort_on_collection_panic;
use crate::gc::field::{read_type_id, read_varlen_count};
use crate::gc::header::{HeaderLayout, ObjHeader};
use crate::gc::reflect::{AllocSite, TypeMeta, ValueMeta};
use crate::gc::roots::{AtomicRootSet, RootSource};
use crate::gc::scan::scan_object;
use crate::gc::type_info::{TypeInfo, VarLenKind};

use crate::gc::alloc::{Alloc, AtomicBumpAllocator};
#[cfg(test)]
use crate::gc::alloc::HeapWalker;
use crate::gc::barrier::SATBQueue;
use crate::gc::card_table::CardTable;
use crate::gc::semi_space::FORWARDING_BIT;
use crate::gc::semi_space::PtrPolicy;
use crate::gc::ptr_policy::IdentityPtrPolicy;
use crate::gc::statemap::{StatemapTracer, TraceState};
use crate::gc::thread::{SiteCounter, ThreadState, ThreadStateHandle};
use crate::gc::telemetry::{GcTelemetry, kind_index, write_event_jsonl};
#[cfg(test)]
use crate::gc::telemetry::pause_percentile;

#[path = "concurrent_access.rs"]
mod concurrent_access;
pub use concurrent_access::ObjectAccess;
use concurrent_access::{CopyClaim, ObjectLocks};

#[path = "scoped_snapshot.rs"]
mod scoped_snapshot;
use scoped_snapshot::ScopedSnapshot;
type ThreadSnapshot<'a> = ScopedSnapshot<
    ThreadStateHandle,
    &'a crate::runtime::metadata_allocator::MetadataAllocator,
>;

#[path = "promotion_scratch.rs"]
mod promotion_scratch;
pub(crate) use promotion_scratch::NurseryLive;
use promotion_scratch::PromotionPending;

// Bound owner-lock hold time and coordinator scratch even for very wide arrays.
const CONCURRENT_SCAN_SLOTS: usize = 32;

// ─── Heap ───────────────────────────────────────────────────────────

/// Whether the precise-layout DETECTOR is armed: always in debug builds, and in
/// release when `GCR_GC_VERIFY` is set (to anything but empty/`0`). When armed,
/// the forward/promote loop asserts that every traced slot pointing into a
/// collected space targets a real object (in-range `type_id`) and panics with a
/// clear diagnostic otherwise — surfacing a scalar that leaked into a traced
/// slot (a layout/rooting bug). This is a DETECTOR, never a silent skip: a
/// moving collector must trust precise layout, not heuristically re-identify
/// pointers (a false-positive int whose bits look like a header would be
/// relocated). Release-default trusts the layout on the hot path (the bounds
/// index still faults loudly if the invariant is ever violated); the env flag
/// lets a corruption be diagnosed in the field without a debug rebuild.
#[inline]
pub(crate) fn gc_verify_armed() -> bool {
    if cfg!(debug_assertions) {
        return true;
    }
    static STATE: AtomicU8 = AtomicU8::new(2); // 2 = uninitialised
    match STATE.load(Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => {
            let on = std::env::var_os("GCR_GC_VERIFY")
                .map(|v| !v.is_empty() && v != "0")
                .unwrap_or(false);
            STATE.store(on as u8, Ordering::Relaxed);
            on
        }
    }
}

/// Thread-safe heap with stop-the-world semi-space collection.
///
/// Multiple mutator threads allocate concurrently from shared from-space
/// (via `AtomicBumpAllocator`). When space is exhausted, one thread
/// triggers STW collection: all threads park at safepoints, the GC
/// copies live objects to to-space, swaps spaces, and resumes threads.
/// GC phase for concurrent collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GcPhase {
    /// No GC in progress. Write barriers are disabled.
    Idle = 0,
    /// Concurrent copying in progress. Write barriers are active.
    Copying = 1,
}

/// The kind of a collection, for the GC log / pause summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcKind {
    /// Young-generation scavenge (generational minor GC).
    Minor,
    /// Stop-the-world copy of the old generation (semi-space major GC).
    Major,
    /// Concurrent major collection.
    Concurrent,
}

impl GcKind {
    pub fn as_str(self) -> &'static str {
        match self {
            GcKind::Minor => "minor",
            GcKind::Major => "major",
            GcKind::Concurrent => "concurrent",
        }
    }
}

/// One garbage-collection event, recorded once per collection for the GC log and
/// pause-time summary (`GCR_GC_LOG` / `GCR_GC_STATS`). This is COLD-PATH
/// observability: an event is pushed once per collection (collections are
/// infrequent and already serialize on the gc lock), never on the allocation
/// hot path. `before_bytes`/`after_bytes` are the collected space's occupancy
/// around the collection; `reclaimed = before - after - promoted`.
#[derive(Debug, Clone, Copy)]
pub struct GcEvent {
    pub kind: GcKind,
    /// 0-based lifetime sequence number, saturating at `u64::MAX`.
    pub seq: u64,
    /// Stop-the-world pause duration in nanoseconds (the copy/scan work).
    /// Saturates at `u64::MAX` nanoseconds.
    pub pause_ns: u64,
    /// Elapsed work/cycle duration. Concurrent cycles include their running
    /// mutator interval here; pause_ns includes only their two world pauses.
    /// Saturates at `u64::MAX` nanoseconds.
    pub elapsed_ns: u64,
    /// Collected space's occupancy (bytes) before the collection.
    pub before_bytes: u64,
    /// Collected space's occupancy (bytes) after the collection.
    pub after_bytes: u64,
    /// Bytes promoted to the old generation (minor GC; 0 for major).
    pub promoted_bytes: u64,
}



/// State for the generational nursery (young generation).
struct NurseryState {
    /// The nursery bump allocator (young generation).
    nursery: AtomicBumpAllocator,
    /// Card tables for each tenured space — tracks old→young pointer writes.
    /// Index matches the `spaces` array: card_tables[0] covers spaces[0], etc.
    card_tables: [CardTable; 2],
    object_indexes: [Mutex<super::object_index::ObjectStartIndex>; 2],
    /// Number of minor collections performed.
    minor_collections: AtomicUsize,
}

pub(crate) type ThreadCensus = allocator_api2::vec::Vec<ThreadStateHandle, crate::runtime::metadata_allocator::MetadataAllocator>;

pub struct Heap {
    spaces: [AtomicBumpAllocator; 2],
    /// JIT-facing allocation mirror: cursor-pointer/base/limit of the
    /// ACTIVE from-space, re-pointed at every flip (under STW) and set
    /// to limit=0 under gc-stress so the inline fast path always defers
    /// to the slow path (which runs the collect-per-alloc hook). Raw-owned
    /// storage preserves exported aliases when this Heap moves.
    alloc_window: crate::abi_storage::AbiStorage<crate::gc::alloc::AllocWindow>,
    /// Index into `spaces` for the current from-space (0 or 1).
    from_idx: AtomicUsize,
    /// Registered mutator threads. The GC scans their roots during STW.
    pub(crate) threads: Mutex<ThreadCensus>,

    /// Global roots (interned symbols, module-level constants, etc.)
    pub globals: AtomicRootSet,

    /// Completion records: legacy raw handles live for the root execution;
    /// bound safe handles may retire once joined and their weak owner is dead.
    pub(crate) thread_completions: Mutex<crate::runtime::CompletionRegistry>,
    /// Vacant global slots reserved solely for thread environment handoffs.
    /// Cleared before reuse; active handoffs remain ordinary scanned globals.
    pub(crate) thread_handoff_slots: Mutex<crate::runtime::handoff_storage::Slots>,
    /// The `globals` slot holding the program's table of `static` cells, or
    /// `usize::MAX` before the program installs one.
    pub(crate) statics_slot: AtomicUsize,
    /// Native channels, retained until root children have drained.
    pub(crate) channels: Mutex<crate::runtime::ChannelRegistry>,
    /// Shared queue/FFI backing ledger; queries do not lock the channel registry.
    pub(crate) native_memory: Arc<crate::runtime::native_memory::NativeMemory>,
    // Declared after channels: queued native backing survives registry teardown.
    pub(crate) native_retirement: crate::runtime::native_retirement::Retirement,
    /// Raw cells live for the execution; bound cells weakly follow GC owners.
    pub(crate) atomic_cells: Mutex<crate::runtime::AtomicCellRegistry>,
    pub(crate) managed_access: crate::runtime::managed::ManagedAccess,

    /// Node-resident `state` bindings: content hash -> index into
    /// `globals`. Each entry's value (typically an `Atom` pointer) is a GC
    /// root, kept live and relocated by `globals`'s scan. The map gives
    /// the language-level `state` primitive its by-identity lookup: a
    /// hash is installed at most once per heap (idempotent), and every
    /// `StateRef` resolves through here to the one live cell. Per-heap (not
    /// process-global) so distinct nodes in one process don't collide.
    pub state_slots: Mutex<std::collections::HashMap<[u8; 32], usize>>,

    /// Permanent extra `RootSource`s registered by the embedder. Scanned
    /// during EVERY collection — including mutator-triggered (alloc-driven)
    /// GC paths that don't take per-call extras. Use this for root sets
    /// whose lifetime equals the heap's, like a JIT module's literal pool
    /// or interpreter constant tables. Stored as raw pointers; the embedder
    /// is responsible for keeping each pointee alive for the heap's
    /// lifetime.
    permanent_extras: Mutex<permanent_roots::Sources>,
    /// Borrowed per-call sources published before their owning mutator parks.
    /// A private guard removes each entry before the borrowed slice expires.
    scoped_extras: ScopedExtraRegistry,

    /// Flag polled by mutator threads: "should I enter safepoint?"
    gc_requested: AtomicBool,

    /// Serializes coordinator operations; see `lock_coordinator` for the
    /// deliberate non-poisoning policy of this data-free exclusion lock.
    gc_lock: Coordinator,
    native_registry_compaction: AtomicBool,

    type_id_offset: usize,
    header: HeaderLayout,
    object_locks: ObjectLocks,
    #[cfg(test)]
    concurrent_copy_observer: Option<Arc<dyn Fn(concurrent_access::CopyStage, *mut u8, *mut u8) + Send + Sync>>,
    type_table: super::layout_table::LayoutTable,
    /// Cold, optional reflection metadata (type/field names + field types),
    /// parallel to `type_table` by `type_id`. Set once at startup via
    /// [`Heap::set_type_meta`]; empty when the embedder supplies none (tests,
    /// non-reflective clients). Reads are lock-free after the one-time set.
    type_meta: OnceLock<super::installed_metadata::InstalledTable<TypeMeta>>,
    /// Cold reflection metadata for inline `#[value]` aggregates, indexed by
    /// `value_id`. Set alongside `type_meta`; used to recursively render
    /// flattened value fields in a heap dump.
    value_meta: OnceLock<super::installed_metadata::InstalledTable<ValueMeta>>,
    /// Cold allocation-site table (Target-1b), indexed by the `site_id` passed
    /// to `ai_gc_alloc_*`. Each entry names the `(function, type_id)` that the
    /// site allocates. Set once at startup via [`Heap::set_alloc_sites`]; empty
    /// when the embedder supplies none (the per-thread counters still
    /// accumulate, just unlabelled). Reads are lock-free after the one-time set.
    alloc_sites: OnceLock<super::installed_metadata::InstalledTable<AllocSite>>,
    /// Allocation-site counters salvaged from threads that have deregistered
    /// (joined) before a profile is taken. Without this, a worker thread's
    /// allocations would silently vanish from the profile when it exits. Folded
    /// in at [`deregister_thread`](Self::deregister_thread) /
    /// [`safe_deregister_thread`](Self::safe_deregister_thread) (under the
    /// threads lock, on the departing thread itself — no concurrent
    /// `record_alloc`), summed alongside the live threads in
    /// [`alloc_site_profile`](Self::alloc_site_profile). Indexed by `site_id`.
    retired_alloc_counters: Mutex<allocator_api2::vec::Vec<SiteCounter, crate::runtime::metadata_allocator::MetadataAllocator>>,
    /// Records per-call root history. Snapshots include active scoped sources,
    /// but cannot reconstruct an embedder's live root obligations outside those
    /// registration intervals, including low-level collect(extra_roots) calls.
    /// Keep reachability attribution conservatively incomplete after such use.
    saw_extra_roots: AtomicBool,
    collections: AtomicUsize,
    /// Combined active-arena occupancy sampled while the world is stopped.
    /// Includes allocator/TLAB reservation slack, not inactive copy space/RSS.
    sampled_heap_peak: AtomicUsize,

    /// Bounded recent events, lifetime aggregates and optional streamed trace.
    /// Updated once per collection, never on the allocation hot path.
    gc_events: Mutex<GcTelemetry>,


    /// Current GC phase. Mutators check this to know if write barriers
    /// should be active.
    gc_phase: AtomicU8,
    /// Set (and never cleared) once a second mutator registers: until then
    /// no other thread can race on an object, so generated code may access
    /// inline aggregates without a tear-free snapshot.
    multi_mutator: AtomicU8,

    /// Global SATB queue. Mutator threads flush their local SATB buffers
    /// here, and the GC drains it during collection.
    pub satb_queue: SATBQueue,

    /// Optional statemap tracer for GC visualization.
    pub tracer: Option<Arc<StatemapTracer>>,

    /// When true, trigger GC on every allocation (stress testing).
    gc_every_alloc: AtomicBool,

    /// Generational nursery state. `None` for non-generational heaps.
    nursery_state: Option<NurseryState>,

    /// Optional walker for the JIT frame chain pointed to by each
    /// parked thread's `parked_jit_fp`. dynalloc cannot itself
    /// understand JIT-frame layouts (that's dynlower's job), so the
    /// embedder installs this hook at runtime via `set_jit_frame_walker`.
    /// `None` means "no JIT integration" — used by tests and
    /// non-JIT clients.
    ///
    /// SIGNATURE: `(jit_fp, visitor) -> ()`. The walker calls `visitor`
    /// with each `*mut u64` slot it finds, exactly as a `RootSource`
    /// would.
    jit_frame_walker: std::sync::atomic::AtomicPtr<()>,
    collector_workers: OnceLock<parallel::WorkerPool>,
}

// Safety: All fields are either Sync (atomics, mutexes) or accessed
// only under proper synchronization. Scoped extra descriptors borrow an owning
// mutator's storage: they are published before it parks, scanned only while its
// owner is quiescent, and removed under the registry lock before the borrow ends.
unsafe impl Sync for Heap {}
unsafe impl Send for Heap {}

/// One row of the allocation-site profile (Target-1b): a `(function, type)`
/// site with its cumulative allocation count and byte total, summed across all
/// threads. Totals saturate at `u64::MAX`; saturated values are lower bounds.
/// Produced by [`Heap::alloc_site_profile`].
#[derive(Clone, Debug, PartialEq)]
pub struct AllocSiteStat {
    /// The compile-time site id (index into the alloc-site table).
    pub site_id: u32,
    /// The function containing the site (compiled symbol name), or a synthetic
    /// `<site N>` if no site table was installed.
    pub function: String,
    /// The allocated object's `type_id`, if known from the site table.
    pub type_id: Option<u16>,
    /// The source type name (from reflection metadata), or a synthetic fallback.
    pub type_name: String,
    /// Source location `file:line:col` from the site table (span-threading), or
    /// empty if unavailable.
    pub location: String,
    /// Objects allocated at this site, summed across all threads.
    pub count: u64,
    /// Bytes allocated at this site, summed across all threads.
    pub bytes: u64,
}

impl Heap {
    #[cfg(test)]
    pub(crate) fn collector_abi_backing_bytes(&self) -> usize {
        crate::abi_storage::AbiStorage::<crate::gc::alloc::AllocWindow>::allocation_bytes()
            + self.spaces.iter().map(AtomicBumpAllocator::collector_cursor_backing_bytes).sum::<usize>()
            + self.nursery_state.as_ref().map_or(0, |nursery| nursery.nursery.collector_cursor_backing_bytes())
    }
    #[cfg(test)]
    pub(crate) fn collector_layout_backing_bytes(&self) -> usize {
        self.type_table.backing_bytes()
    }
    /// Create a new heap with two spaces of `space_size` bytes each.
    pub fn new<H: ObjHeader>(space_size: usize, type_table: Vec<TypeInfo>) -> Self {
        let type_table = super::layout_table::PreparedLayouts::new::<H>(type_table);
        let native_memory = Arc::<crate::runtime::native_memory::NativeMemory>::default();
        let heap = Heap {
            spaces: [
                AtomicBumpAllocator::with_metadata_allocator::<H>(space_size, crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry)),
                AtomicBumpAllocator::with_metadata_allocator::<H>(space_size, crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry)),
            ],
            alloc_window: crate::abi_storage::AbiStorage::new_in(crate::gc::alloc::AllocWindow::empty(), native_memory.registry_allocator()),
            from_idx: AtomicUsize::new(0),
            threads: Mutex::new(allocator_api2::vec::Vec::new_in(crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry))),
            globals: AtomicRootSet::with_allocator(crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry)),
            state_slots: Mutex::new(std::collections::HashMap::new()),
            thread_completions: crate::runtime::CompletionRegistry::native_mutex_in(crate::runtime::native_metadata::MetadataAllocator::new(&native_memory.registry)),
            thread_handoff_slots: crate::runtime::handoff_storage::Slots::native_mutex_in(crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry)),
            statics_slot: AtomicUsize::new(usize::MAX),
            channels: crate::runtime::ChannelRegistry::native_mutex_in(crate::runtime::native_metadata::MetadataAllocator::new(&native_memory.registry)),
            native_memory: native_memory.clone(),
            native_retirement: Default::default(),
            atomic_cells: crate::runtime::AtomicCellRegistry::native_mutex_in(crate::runtime::native_metadata::MetadataAllocator::new(&native_memory.registry)),
            managed_access: crate::runtime::managed::ManagedAccess::new(),
            permanent_extras: permanent_roots::new_registry(&native_memory.registry),
            scoped_extras: ScopedExtraRegistry::default(),
            gc_requested: AtomicBool::new(false),
            gc_lock: Coordinator::new(),
            native_registry_compaction: AtomicBool::new(false),
            type_id_offset: H::TYPE_ID_OFFSET,
            header: HeaderLayout::of::<H>(),
            object_locks: ObjectLocks::new_in(native_memory.registry_allocator()),
            #[cfg(test)]
            concurrent_copy_observer: None,
            type_table: super::layout_table::LayoutTable::from_prepared(type_table, &native_memory.registry),
            type_meta: OnceLock::new(),
            value_meta: OnceLock::new(),
            alloc_sites: OnceLock::new(),
            retired_alloc_counters: Mutex::new(allocator_api2::vec::Vec::new_in(crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry))),
            saw_extra_roots: AtomicBool::new(false),
            collections: AtomicUsize::new(0),
            sampled_heap_peak: AtomicUsize::new(0),
            gc_events: Mutex::new(GcTelemetry::new()),
            gc_phase: AtomicU8::new(GcPhase::Idle as u8),
            multi_mutator: AtomicU8::new(0),
            satb_queue: SATBQueue::with_allocator(crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry)),
            tracer: None,
            gc_every_alloc: AtomicBool::new(false),
            nursery_state: None,
            jit_frame_walker: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            collector_workers: OnceLock::new(),
        };
        unsafe {
            heap.alloc_window.point_at(&heap.spaces[0], space_size);
        }
        heap
    }

    /// Create a new generational heap with a nursery (young generation)
    /// and two tenured spaces (old generation).
    ///
    /// New allocations go to the nursery. When the nursery fills, a minor
    /// GC promotes survivors to tenured from-space. When tenured space fills,
    /// a major GC (STW or concurrent) collects the old generation.
    pub fn new_generational<H: ObjHeader>(
        nursery_size: usize,
        tenured_size: usize,
        type_table: Vec<TypeInfo>,
    ) -> Self {
        let type_table = super::layout_table::PreparedLayouts::new::<H>(type_table);
        let native_memory = Arc::<crate::runtime::native_memory::NativeMemory>::default();
        let spaces = [
            AtomicBumpAllocator::with_metadata_allocator::<H>(tenured_size, crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry)),
            AtomicBumpAllocator::with_metadata_allocator::<H>(tenured_size, crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry)),
        ];
        let card_tables = [
            CardTable::new(spaces[0].base(), tenured_size),
            CardTable::new(spaces[1].base(), tenured_size),
        ];
        let heap = Heap {
            spaces,
            alloc_window: crate::abi_storage::AbiStorage::new_in(crate::gc::alloc::AllocWindow::empty(), native_memory.registry_allocator()),
            from_idx: AtomicUsize::new(0),
            threads: Mutex::new(allocator_api2::vec::Vec::new_in(crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry))),
            globals: AtomicRootSet::with_allocator(crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry)),
            state_slots: Mutex::new(std::collections::HashMap::new()),
            thread_completions: crate::runtime::CompletionRegistry::native_mutex_in(crate::runtime::native_metadata::MetadataAllocator::new(&native_memory.registry)),
            thread_handoff_slots: crate::runtime::handoff_storage::Slots::native_mutex_in(crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry)),
            statics_slot: AtomicUsize::new(usize::MAX),
            channels: crate::runtime::ChannelRegistry::native_mutex_in(crate::runtime::native_metadata::MetadataAllocator::new(&native_memory.registry)),
            native_memory: native_memory.clone(),
            native_retirement: Default::default(),
            atomic_cells: crate::runtime::AtomicCellRegistry::native_mutex_in(crate::runtime::native_metadata::MetadataAllocator::new(&native_memory.registry)),
            managed_access: crate::runtime::managed::ManagedAccess::new(),
            permanent_extras: permanent_roots::new_registry(&native_memory.registry),
            scoped_extras: ScopedExtraRegistry::default(),
            gc_requested: AtomicBool::new(false),
            gc_lock: Coordinator::new(),
            native_registry_compaction: AtomicBool::new(false),
            type_id_offset: H::TYPE_ID_OFFSET,
            header: HeaderLayout::of::<H>(),
            object_locks: ObjectLocks::new_in(native_memory.registry_allocator()),
            #[cfg(test)]
            concurrent_copy_observer: None,
            type_table: super::layout_table::LayoutTable::from_prepared(type_table, &native_memory.registry),
            type_meta: OnceLock::new(),
            value_meta: OnceLock::new(),
            alloc_sites: OnceLock::new(),
            retired_alloc_counters: Mutex::new(allocator_api2::vec::Vec::new_in(crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry))),
            saw_extra_roots: AtomicBool::new(false),
            collections: AtomicUsize::new(0),
            sampled_heap_peak: AtomicUsize::new(0),
            gc_events: Mutex::new(GcTelemetry::new()),
            gc_phase: AtomicU8::new(GcPhase::Idle as u8),
            multi_mutator: AtomicU8::new(0),
            satb_queue: SATBQueue::with_allocator(crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry)),
            tracer: None,
            gc_every_alloc: AtomicBool::new(false),
            nursery_state: Some(NurseryState {
                nursery: AtomicBumpAllocator::with_metadata_allocator::<H>(nursery_size, crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry)),
                card_tables,
                object_indexes: std::array::from_fn(|_| Mutex::new(
                    super::object_index::ObjectStartIndex::with_allocator(
                        crate::runtime::metadata_allocator::MetadataAllocator::new(&native_memory.registry),
                    ),
                )),
                minor_collections: AtomicUsize::new(0),
            }),
            jit_frame_walker: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            collector_workers: OnceLock::new(),
        };
        // Generational mode allocates via the nursery path, which the
        // JIT inline window does not model — leave the window CLOSED
        // (limit 0) so inline allocation always defers to the runtime.
        unsafe {
            heap.alloc_window.point_at(&heap.spaces[0], 0);
        }
        heap
    }

    /// Install a walker for JIT frames pointed to by parked threads.
    /// Called by frontends with JIT integration (e.g. dynlang) at
    /// runtime-construction time. The walker is consulted at every
    /// GC: each registered thread's `parked_jit_fp` is read; if
    /// non-null, the walker is invoked with that fp and a visitor.
    ///
    /// # Safety
    /// The walker must remain valid for the lifetime of the Heap and match all
    /// published JIT frame layouts. Install/replace it only before mutators run
    /// or under a world pause with no walker invocation in progress.
    /// The supplied `*mut u64` slots must be live for the duration of
    /// the visitor call (they are, by the safepoint contract). Each integration
    /// must publish its current JIT frame before entering a safepoint/BLOCKED
    /// scope or a world-pause API that can suspend the current registration.
    pub unsafe fn set_jit_frame_walker(
        &self,
        walker: unsafe fn(*const u8, &mut dyn FnMut(*mut u64)),
    ) {
        let p = walker as *mut ();
        self.jit_frame_walker
            .store(p, std::sync::atomic::Ordering::Release);
    }

    /// Walk a single parked-thread JIT frame using the registered
    /// walker. No-op if no walker is installed.
    fn walk_jit_frame(&self, jit_fp: *const u8, visitor: &mut dyn FnMut(*mut u64)) {
        let walker = self
            .jit_frame_walker
            .load(std::sync::atomic::Ordering::Acquire);
        if walker.is_null() {
            return;
        }
        let walker: unsafe fn(*const u8, &mut dyn FnMut(*mut u64)) =
            unsafe { std::mem::transmute(walker as *const ()) };
        unsafe { walker(jit_fp, visitor) };
    }

    /// Visit every GC root — exactly the set the collector scans in
    /// `collect_inner` Phase 1 (the global root set, each registered thread's
    /// host roots + its parked JIT frame, permanent and active scoped extras) — calling
    /// `visit(obj)` with each non-null object a root slot points at. READ-ONLY:
    /// it reads root slots and reports the pointed-at objects; it never relocates
    /// (unlike the collector's `process_slot`).
    ///
    /// This is the *real* GC root set — the correct basis for snapshot
    /// reachability / dominators. (The heap dump's in-degree-0 "roots" are a
    /// structural proxy, not the true roots.) At a mid-execution snapshot the
    /// parked threads' frames are rich live roots; at program end (threads
    /// deregistered, the entry frame returned) the roots are just the globals +
    /// permanent extras — which honestly reflects that most of the heap is, by
    /// then, uncollected garbage.
    ///
    /// # Safety
    /// Must be called with the world quiescent — under a [`pause_world`] STW
    /// pause — because it reads each thread's frame chain and `parked_jit_fp`,
    /// stable only while that thread is parked. Objects must be valid (as for the
    /// heap dump). Do not call while holding `gc_lock` other than via the pause.
    /// Uses `IdentityPtrPolicy`; use `visit_roots_with_policy` for tagged roots.
    pub unsafe fn visit_roots(&self, visit: &mut dyn FnMut(*mut u8)) {
        unsafe { self.visit_roots_with_policy::<IdentityPtrPolicy>(visit); }
    }

    /// Visit roots decoded with the same policy used by this heap's collector.
    /// Non-pointer values are omitted. Root sources remain unchanged.
    ///
    /// # Safety
    /// As `visit_roots`; `P` must match the encoding of every registered root.
    pub unsafe fn visit_roots_with_policy<P: PtrPolicy>(&self, visit: &mut dyn FnMut(*mut u8)) {
        let mut do_slot = |slot: *mut u64| {
            if let Some(pointer) = P::try_decode_ptr(unsafe { slot.read() }) {
                if !pointer.is_null() { visit(pointer); }
            }
        };
        unsafe { self.scan_registered_roots(&mut do_slot); }
    }

    /// Enumerate the same root families for each collector and diagnostics.
    /// # Safety
    /// All registered owners are quiescent; visitor obeys RootSource's slot
    /// lifetime and pointer-policy contract. JIT frames must be published.
    unsafe fn scan_registered_roots(&self, visit: &mut dyn FnMut(*mut u64)) {
        unsafe { self.scan_collection_roots(visit, false); }
    }
    /// Same root families; only minor copying may omit clean native queues.
    /// # Safety
    /// Same quiescence, slot lifetime and published-frame contract as above.
    unsafe fn scan_collection_roots(&self, visit: &mut dyn FnMut(*mut u64), minor: bool) {
        #[cfg(test)]
        crate::runtime::channel_root_probe::record(minor);
        unsafe { self.scan_persistent_roots(visit, minor); }
        for state in self.threads.lock().unwrap().iter() {
            unsafe { state.scan_roots(visit); }
            let fp = state.parked_jit_fp();
            if !fp.is_null() { self.walk_jit_frame(fp, visit); }
        }
        unsafe { self.scan_scoped_roots(visit); }
        for &source in self.permanent_extras.lock().unwrap().iter() {
            unsafe { (&*source).scan_roots(visit); }
        }
    }

    /// Whether per-call roots have ever been supplied. Snapshots conservatively
    /// mark attribution incomplete; active scoped sources are still enumerated.
    pub fn saw_extra_roots(&self) -> bool {
        self.saw_extra_roots.load(Ordering::Relaxed)
    }

    /// Enable or disable GC-on-every-allocation (stress testing mode).
    pub fn set_gc_every_alloc(&self, enabled: bool) {
        self.gc_every_alloc.store(enabled, Ordering::Release);
        if enabled {
            // Close the JIT inline-allocation window: every allocation
            // takes the out-of-line path, which runs the
            // collect-before-every-alloc stress hook.
            self.alloc_window.limit.store(0, Ordering::Release);
        }
    }

    /// Register a layout before sharing the heap, assigning its stable type id.
    /// Exclusive access prevents table reallocation from racing readers or
    /// invalidating borrowed layouts. A shared runtime must regain exclusive
    /// ownership before adding types; a world pause alone does not end Rust
    /// borrows of existing entries.
    ///
    /// ```compile_fail
    /// use gcrust_rt::gc::{Compact, Heap, ObjHeader, TypeInfo};
    /// let heap = Heap::new::<Compact>(4096, vec![]);
    /// let shared = &heap;
    /// shared.dynamic_add_type(TypeInfo::for_header(Compact::SIZE));
    /// ```
    pub fn dynamic_add_type(&mut self, mut ti: TypeInfo) -> u16 {
        let id = u16::try_from(self.type_table.len()).expect("heap type-id space exhausted");
        ti.type_id = id;
        assert!(
            self.header.accepts(&ti, 0) && ti.align_log2 == 3,
            "invalid heap type layout"
        );
        self.type_table.push(ti);
        id
    }

    pub(crate) fn accepts_allocation(&self, info: &TypeInfo, len: usize) -> bool {
        self.header.accepts(info, len)
            && info.align_log2 == 3
            && self.type_table.get(info.type_id as usize) == Some(info)
    }

    pub(crate) fn accepts_header<H: ObjHeader>(&self, info: &TypeInfo, len: usize) -> bool {
        self.header.is::<H>() && self.accepts_allocation(info, len)
    }

    /// Validate once and bind the request to this heap's immutable canonical
    /// descriptor. The token retains layout/length, never an arena reservation:
    /// retrying it after GC selects the current allocation space again.
    #[inline]
    pub(crate) fn allocation_request<'h>(&'h self, info: &TypeInfo, len: usize) -> Option<AllocationRequest<'h>> {
        let canonical = self.type_table.get(info.type_id as usize)?;
        if !std::ptr::eq(canonical, info) && canonical != info { return None; }
        // Header/alignment invariants were checked when this table was built
        // or extended. Variable length and total pointer offset still vary.
        let size = canonical.checked_size_for_valid_layout(len)?;
        Some(AllocationRequest { heap: self, info: canonical, len, size })
    }

    #[inline]
    pub(crate) fn object_request<H: ObjHeader>(&self, info: &TypeInfo, len: usize) -> Option<ObjectRequest<'_, H>> {
        if !self.header.is::<H>() { return None; }
        Some(ObjectRequest { allocation: self.allocation_request(info, len)?, _header: std::marker::PhantomData })
    }

    /// Read the current length of `type_table`. Mostly useful in tests
    /// for verifying `dynamic_add_type` worked.
    pub fn type_table_len(&self) -> usize {
        self.type_table.len()
    }

    /// Enumerate fixed native queue roots. Caller holds a world pause and keeps
    /// registered channel allocations alive until all evacuation work finishes.
    pub(crate) unsafe fn scan_persistent_roots(&self, visitor: &mut dyn FnMut(*mut u64), minor: bool) {
        unsafe { self.globals.scan_roots(visitor); }
        unsafe { self.channels.lock().unwrap().scan_unbound_roots(visitor, minor); }
    }

    /// Snapshot conditional native edges while stopped for a graph walk.
    pub(crate) fn channel_edges(&self) -> ChannelEdges<'_> { ChannelEdges::new(self) }

    /// Read a heap object's `type_id` from its header. Lets the runtime
    /// walk a value's shape the same way the GC does (e.g. structural
    /// hashing/equality of arbitrary values).
    ///
    /// # Safety
    /// `obj` must point to a valid heap object with an initialized header.
    pub unsafe fn obj_type_id(&self, obj: *const u8) -> u16 {
        unsafe { read_type_id(obj, self.type_id_offset) }
    }

    /// `TypeInfo` (layout) for a `type_id`. Pairs with [`obj_type_id`] to
    /// walk an object's fields by shape.
    pub fn type_info_by_id(&self, type_id: u16) -> &TypeInfo {
        &self.type_table[type_id as usize]
    }

    /// Install the reflection metadata table (type/field names + field types).
    /// Called once at startup, before any mutator runs. The full table is
    /// supplied at once; a partial or absent table simply means reflection
    /// queries return `None`. A second call is ignored (the table is immutable
    /// once set).
    pub fn set_type_meta(&self, meta: Vec<TypeMeta>) {
        let _ = self.type_meta.set(super::installed_metadata::InstalledTable::new(meta, &self.native_memory.registry));
    }

    /// Reflection metadata for a `type_id`, or `None` if no metadata was
    /// installed or the id is out of range. Pairs with [`obj_type_id`] to render
    /// an object with its source names.
    pub fn type_meta_by_id(&self, type_id: u16) -> Option<&TypeMeta> {
        self.type_meta.get()?.as_slice().get(type_id as usize)
    }

    /// The full reflection metadata table (empty slice if none installed).
    pub fn type_meta_all(&self) -> &[TypeMeta] {
        self.type_meta.get().map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Install the value-aggregate reflection metadata (indexed by `value_id`).
    /// Set once at startup alongside [`set_type_meta`](Self::set_type_meta).
    pub fn set_value_meta(&self, meta: Vec<ValueMeta>) {
        let _ = self.value_meta.set(super::installed_metadata::InstalledTable::new(meta, &self.native_memory.registry));
    }

    /// Reflection metadata for an inline `#[value]` aggregate by `value_id`, or
    /// `None` if none was installed or the id is out of range.
    pub fn value_meta_by_id(&self, value_id: u16) -> Option<&ValueMeta> {
        self.value_meta.get()?.as_slice().get(value_id as usize)
    }

    /// Install the allocation-site table (Target-1b), indexed by `site_id`.
    /// Called once at startup, before any mutator runs. Mirrors
    /// [`set_type_meta`](Self::set_type_meta); a second call is ignored.
    pub fn set_alloc_sites(&self, sites: Vec<AllocSite>) {
        let _ = self.alloc_sites.set(super::installed_metadata::InstalledTable::new(sites, &self.native_memory.registry));
    }

    /// The installed allocation-site table (empty slice if none installed).
    pub fn alloc_sites_all(&self) -> &[AllocSite] {
        self.alloc_sites.get().map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Fold one (about-to-deregister) thread's per-site counters into the
    /// heap-level retired accumulator. Called under the threads lock, on the
    /// departing thread, so there is no concurrent `record_alloc`.
    fn fold_retired_alloc_counters(&self, state: &ThreadStateHandle) {
        // Safety: the owning thread is deregistering (not allocating), and we
        // hold the threads lock, so no `record_alloc` runs concurrently.
        let counters = unsafe { state.alloc_site_counters() };
        if counters.is_empty() {
            return;
        }
        let mut retired = self.retired_alloc_counters.lock().unwrap();
        if counters.len() > retired.len() {
            retired.resize(counters.len(), SiteCounter::default());
        }
        for (i, c) in counters.iter().enumerate() {
            retired[i].count = retired[i].count.saturating_add(c.count);
            retired[i].bytes = retired[i].bytes.saturating_add(c.bytes);
        }
    }

    /// Test inventory of registry backing and records retained by arenas or
    /// registered states, including stale records surviving an arena reset.
    /// Externally retained deregistered states need their own fixture inventory.
    /// # Safety
    /// All registrations are owned by this caller or remain quiescent for this
    /// observation. No collection or metadata retirement may overlap capture.
    #[cfg(test)]
    pub(crate) unsafe fn collector_tlab_metadata_bytes(&self) -> usize {
        let mut records = std::collections::HashMap::new();
        let mut backing = 0;
        for arena in self.spaces.iter().chain(self.nursery_state.iter().map(|ns| &ns.nursery)) {
            backing += arena.buffer_registry_backing_bytes();
            arena.inventory_buffer_records(&mut records);
        }
        for state in self.threads.lock().unwrap().iter() {
            if let Some((identity, bytes)) = unsafe { state.tlab_metadata_record() } {
                records.insert(identity, bytes);
            }
        }
        backing + records.values().sum::<usize>()
    }

    #[cfg(test)]
    pub(crate) fn retired_profile_backing_bytes(&self) -> usize {
        self.retired_alloc_counters.lock().unwrap().capacity() * std::mem::size_of::<SiteCounter>()
    }

    /// Build the allocation-site profile (Target-1b): sum each site's
    /// `(count, bytes)` across every live thread plus the retired accumulator,
    /// join with the site table (function + type id) and reflection metadata
    /// (type name), and return the sites sorted by bytes descending.
    ///
    /// Sites with zero allocations are omitted. Counters with no matching site
    /// entry (e.g. profiling table not installed) are reported under a synthetic
    /// `<site N>` / `<unknown>` label rather than dropped — no silent loss.
    ///
    /// NOTE: this covers allocations made through the compiled-code allocation
    /// path (`ai_gc_alloc_*`). Runtime-internal allocations — `ai_str_concat`,
    /// `ai_str_substring`, the `ai_str_from_*` conversions, file/IO buffers, and
    /// any other runtime fn that allocates via the heap directly rather than
    /// through `ai_gc_alloc_*` — are **not** attributed to a site in v1.
    ///
    /// Data-race safety: the live per-thread counters are plain (non-atomic)
    /// integers written by their owning thread on the allocation path. We read
    /// them under a [`pause_world`](Self::pause_world) stop-the-world pause, so
    /// every *other* mutator is parked at a safepoint — not mid-`record_alloc`
    /// and not mid-`Vec` grow — while we read. This is the same discipline the
    /// collector uses to read parked threads' non-atomic state (`frame_chain`,
    /// `satb_buffer`); the park establishes the happens-before that makes the
    /// non-atomic reads sound. The pause is cheap on the common path (no other
    /// thread to park) and this is a cold (program-end / tooling) call. Keeping
    /// the read at STW is what lets the hot path stay a plain, uncontended
    /// increment rather than an atomic. Because the live-thread snapshot and the
    /// retired accumulator are cloned together under the threads lock (inside the
    /// pause), a thread can never be observed in both — no double counting.
    pub fn alloc_site_profile(&self) -> Vec<AllocSiteStat> {
        // Sum the live per-thread counters under a stop-the-world pause, plus the
        // retired accumulator. The pause guard holds `gc_lock` and parks every
        // other mutator; we must not allocate on the GC heap or trigger a GC
        // inside it (we don't — only counter reads + host-heap Vec ops). Drop the
        // pause before building the report so the resumed world overlaps the
        // (race-free, owned-data) formatting below.
        let totals: Vec<SiteCounter> = {
            let _pause = self.pause_world();
            // Observe the {live, retired} partition ATOMICALLY: clone the live
            // thread set and the retired accumulator under the same threads-lock
            // critical section. A thread folds into `retired` only while holding
            // the threads lock (deregister), so it cannot appear in both the
            // snapshot and the retired clone — closing the double-count window.
            // Lock order gc_lock(pause) → threads → retired matches every other
            // path (pause_world: gc→threads; deregister: threads→retired).
            let (snapshot, mut totals): (ThreadSnapshot<'_>, Vec<SiteCounter>) = {
                let threads = self.threads.lock().unwrap();
                let retired = self.retired_alloc_counters.lock().unwrap().iter().copied().collect();
                (ScopedSnapshot::capture(threads.iter().cloned(), self.native_memory.registry_allocator()), retired)
            };
            for ts in &snapshot {
                // Safety: every other mutator is parked at a safepoint (so not
                // writing its counters), and the requesting thread is here (not
                // allocating); the park establishes happens-before for the read.
                let counters = unsafe { ts.alloc_site_counters() };
                if counters.len() > totals.len() {
                    totals.resize(counters.len(), SiteCounter::default());
                }
                for (i, c) in counters.iter().enumerate() {
                    totals[i].count = totals[i].count.saturating_add(c.count);
                    totals[i].bytes = totals[i].bytes.saturating_add(c.bytes);
                }
            }
            totals
        };

        let sites = self.alloc_sites_all();
        let mut out = Vec::new();
        for (id, c) in totals.iter().enumerate() {
            if c.count == 0 {
                continue;
            }
            let (function, type_id, location) = match sites.get(id) {
                Some(s) => (s.function.clone(), Some(s.type_id), s.location.clone()),
                None => (format!("<site {id}>"), None, String::new()),
            };
            let type_name = match type_id {
                Some(tid) => self
                    .type_meta_by_id(tid)
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| format!("<type {tid}>")),
                None => "<unknown>".to_string(),
            };
            out.push(AllocSiteStat {
                site_id: id as u32,
                function,
                type_id,
                type_name,
                location,
                count: c.count,
                bytes: c.bytes,
            });
        }
        out.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then_with(|| b.count.cmp(&a.count))
                .then_with(|| a.function.cmp(&b.function))
        });
        out
    }

    /// Render the allocation-site profile as a human-readable table (used by
    /// `GCR_ALLOC_PROFILE=1`). Empty string if nothing was allocated through a
    /// profiled site. See [`alloc_site_profile`](Self::alloc_site_profile) for
    /// the v1 coverage caveat (compiled-code sites only).
    pub fn alloc_site_profile_report(&self) -> String {
        use std::fmt::Write;
        let stats = self.alloc_site_profile();
        if stats.is_empty() {
            return String::new();
        }
        let total_bytes: u64 = stats.iter().map(|s| s.bytes).fold(0u64, u64::saturating_add);
        let total_count: u64 = stats.iter().map(|s| s.count).fold(0u64, u64::saturating_add);
        let mut out = String::new();
        let _ = writeln!(
            out,
            "gc-rust allocation-site profile (compiled-code sites only; runtime-internal allocations \
             — string/substring/concat, str-from-*, file/IO buffers — are NOT attributed to a site)"
        );
        let _ = writeln!(
            out,
            "  {:>14}  {:>10}  {:<22}  {:<22}  {}",
            "bytes", "count", "type", "function", "location"
        );
        for s in &stats {
            let loc = if s.location.is_empty() { "-" } else { s.location.as_str() };
            let _ = writeln!(
                out,
                "  {:>14}  {:>10}  {:<22}  {:<22}  {}",
                s.bytes, s.count, s.type_name, s.function, loc
            );
        }
        let _ = writeln!(
            out,
            "  {:>14}  {:>10}  (total over {} sites)",
            total_bytes,
            total_count,
            stats.len()
        );
        out
    }

    /// Walk every object currently allocated in the live spaces — the tenured
    /// from-space plus the nursery (if generational) — calling `visitor(obj,
    /// type_info)` for each. This is the engine for heap-exploration tooling:
    /// paired with [`type_meta_by_id`](Self::type_meta_by_id) it can render the
    /// whole object graph with source names.
    ///
    /// Note: this visits all *allocated* objects in those spaces, which between
    /// collections may include not-yet-reclaimed garbage in the nursery. For a
    /// strictly-live snapshot, trigger a collection first. Must not run
    /// concurrently with a collection (call at a safepoint or program end).
    ///
    /// # Safety
    /// All objects in the live spaces must be valid (headers initialized, varlen
    /// counts written) — true for any object the mutator has finished
    /// allocating. Keep all mutators stopped, or hold exclusive heap access,
    /// while capturing and interpreting allocation extents. The visitor must
    /// not trigger collection or invalidate visited objects.
    pub unsafe fn walk_live_objects(&self, visitor: &mut dyn FnMut(*mut u8, &TypeInfo)) {
        unsafe {
            self.from_space().walk_in(&self.type_table, self.native_memory.registry_allocator(), visitor);
            if let Some(ns) = &self.nursery_state {
                ns.nursery.walk_in(&self.type_table, self.native_memory.registry_allocator(), visitor);
            }
        }
    }

    /// Check if GC should be triggered on every allocation.
    #[inline(always)]
    pub fn gc_every_alloc(&self) -> bool {
        self.gc_every_alloc.load(Ordering::Relaxed)
    }

    /// Get the current from-space.
    #[inline(always)]
    fn from_space(&self) -> &AtomicBumpAllocator {
        unsafe {
            self.spaces
                .get_unchecked(self.from_idx.load(Ordering::Acquire))
        }
    }

    /// Get the current to-space.
    #[inline(always)]
    fn to_space(&self) -> &AtomicBumpAllocator {
        unsafe {
            self.spaces
                .get_unchecked(1 - self.from_idx.load(Ordering::Acquire))
        }
    }

    /// Process weak native owners before collected source headers disappear.
    /// The world is stopped and every copying worker has completed. The
    /// parallel evacuation call has returned, dropping its temporary channel
    /// snapshot Arcs before registry ownership is handed to native retirement.
    /// No native join or user finalizer may execute here.
    unsafe fn sweep_native_owners(&self, minor: bool) {
        let source = if minor {
            &self.nursery_state.as_ref().expect("minor without nursery").nursery
        } else { self.from_space() };
        let destination = if minor { self.from_space() } else { self.to_space() };
        let mut relocate = |owner: *mut u8| {
            if source.contains(owner) {
                unsafe { self.check_forwarded(owner) }.map(|forwarded| {
                    assert!(destination.contains(forwarded), "unpublished weak native owner");
                    forwarded
                })
            } else {
                assert!(self.from_space().contains(owner)
                    || (!minor && self.to_space().contains(owner))
                    || (!minor && self.is_nursery(owner)),
                    "weak native owner outside active arenas");
                Some(owner)
            }
        };
        let mut compact = crate::runtime::registry_compaction::vector_needs_compaction(&self.threads);
        let mut completions = self.thread_completions.lock().unwrap();
        completions.sweep(|completion| !completion.relocate_owner_and_reclaimable(&mut relocate));
        compact |= completions.needs_compaction();
        drop(completions);
        let mut atomics = self.atomic_cells.lock().unwrap();
        atomics.sweep(&mut relocate);
        compact |= atomics.needs_compaction();
        drop(atomics);
        let mut channels = self.channels.lock().unwrap();
        if minor { unsafe { channels.sweep_minor(relocate); } }
        else { channels.sweep(relocate); }
        compact |= channels.needs_compaction();
        if compact { self.native_registry_compaction.store(true, Ordering::Release); }
    }

    /// Swap from-space and to-space by flipping the index. Runs under
    /// stop-the-world only; also re-points the JIT allocation window at
    /// the new from-space (preserving a CLOSED window — limit 0 — for
    /// stress/generational modes).
    fn swap_spaces(&self) {
        let old = self.from_idx.load(Ordering::Acquire);
        self.from_idx.store(1 - old, Ordering::Release);
        let closed = self.alloc_window.limit.load(Ordering::Acquire) == 0;
        let space = self.from_space();
        let limit = if closed { 0 } else { space.size() };
        unsafe {
            self.alloc_window.point_at(space, limit);
        }
    }

    /// Create a new heap with statemap tracing enabled.
    pub fn new_with_tracer<H: ObjHeader>(space_size: usize, type_table: Vec<TypeInfo>, tracer: Arc<StatemapTracer>) -> Self {
        let mut heap = Self::new::<H>(space_size, type_table);
        heap.tracer = Some(tracer);
        heap
    }

    /// Record a trace event if a tracer is attached.
    #[inline(always)]
    #[allow(dead_code)]
    fn trace(&self, entity: &str, state: TraceState) {
        if let Some(ref tracer) = self.tracer {
            tracer.record(entity, state);
        }
    }

    /// Record a trace event for the GC entity.
    #[inline(always)]
    fn trace_gc(&self, state: TraceState) {
        if let Some(ref tracer) = self.tracer {
            tracer.record_gc(state);
        }
    }

    /// Record a trace event using the registered state's stable identity.
    #[inline(always)]
    fn trace_thread(&self, thread: &ThreadState, state: TraceState) {
        if let Some(ref tracer) = self.tracer {
            tracer.record_thread(thread.trace_id(), state);
        }
    }

    /// This mutex carries exclusion, not recoverable heap state. A user may
    /// unwind while holding WorldPause; its destructor resumes every mutator
    /// before releasing the lock. That completed restoration leaves the heap
    /// consistent, although std marks the mutex poisoned. Collection/relocation
    /// and pause restoration failures abort while still holding this lock.
    /// Other runtime mutexes retain their ordinary poisoning policy.
    fn lock_coordinator(&self) -> CoordinatorGuard<'_> {
        self.gc_lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Complete every collector forwarding write before registry ownership is
    /// detached. Only ownership swaps belong in this interval; retirement and
    /// physical disposal must follow after releasing the coordinator.
    /// # Safety
    /// Caller is unregistered or all its registrations are BLOCKED. The action
    /// must not allocate, dispose native storage, or wait for managed execution.
    pub(crate) unsafe fn with_native_registry_exclusion<R>(&self, action: impl FnOnce() -> R) -> R {
        let _coordinator = self.lock_coordinator();
        action()
    }

    /// Native backing maintenance runs without coordinator exclusion.
    /// # Safety
    /// Caller is unregistered or all its registrations are BLOCKED; all root
    /// publications from collection entry remain live through this interval.
    unsafe fn compact_native_registries(&self) {
        if !self.native_registry_compaction.load(Ordering::Acquire)
            || !self.native_registry_compaction.swap(false, Ordering::AcqRel)
        { return; }
        abort_on_collection_panic(|| crate::runtime::registry_compaction::compact_all(self));
    }

    /// Retire a handoff root after the last possible forwarding store.
    /// # Safety
    /// The caller is unregistered or BLOCKED on this heap, and exclusively owns
    /// this handoff slot until it has been cleared. No derived managed pointer
    /// may be used after this safepoint without reloading its root.
    pub(crate) unsafe fn clear_thread_handoff_root(&self, index: usize) {
        // Parallel root addresses outlive enumeration's backing guard. Wait
        // through the entire evacuation, not merely its initial root visit.
        let _coordinator = self.lock_coordinator();
        unsafe { self.globals.set(index, 0); }
    }

    // ─── Thread registration ────────────────────────────────────

    /// Register a new mutator thread. Returns a shared state handle and its
    /// stable diagnostic identity, unique across heaps and never reused.
    ///
    /// If a GC is currently active, waits for it to finish first.
    /// This prevents a newly registered thread from calling safepoint()
    /// during an active GC that doesn't know about it — which would
    /// cause the thread to block in enter_safepoint with nobody to
    /// resume it. Existing RUNNING registrations on this OS thread are
    /// suspended while waiting, then restored before return. Like any
    /// safepoint, JIT embedders must publish frames before calling and reload
    /// derived pointers afterward.
    pub fn register_thread(&self) -> (ThreadStateHandle, usize) {
        // A previous registration on this OS thread must not remain RUNNING
        // while we wait for a collector that may already be waiting for it.
        let mut callers = self.suspend_current_registrations();
        let _gc_guard = self.lock_coordinator();
        let allocator = crate::runtime::metadata_allocator::MetadataAllocator::new(&self.native_memory.registry);
        let ts = ThreadStateHandle::new_in(ThreadState::with_allocator(allocator.clone()), &allocator);
        let id = ts.trace_id();
        {
            let mut threads = self.threads.lock().unwrap();
            threads.push(ts.clone());
            if threads.len() > 1 { self.multi_mutator.store(1, Ordering::SeqCst); }
        }
        // Keep gc_lock until every current registration is RUNNING again:
        // a collector must not start between restoring the old and new states.
        callers.restore();
        (ts, id)
    }

    /// Run `f` while holding the threads lock, but only if no collection
    /// window is open; returns `false` (without running `f`) otherwise.
    ///
    /// Collectors raise `gc_requested` under this same lock before
    /// sampling thread states, so a state transition performed inside `f`
    /// is atomic w.r.t. the collector's safepoint census: if
    /// `gc_requested` is still clear while we hold the lock, no in-flight
    /// collection has sampled (or will sample) the caller's pre-transition
    /// state. Same pattern as `register_thread` / `safe_deregister_thread`.
    pub fn transition_outside_gc(&self, f: impl FnOnce()) -> bool {
        let _threads = self.threads.lock().unwrap();
        if self.gc_requested() || self.barriers_active() {
            return false;
        }
        f();
        true
    }

    /// Complete every poll/notification before blocked mutators may retire
    /// their runtime Thread (which owns the flag storage). Registration and
    /// BLOCKED->RUNNING use this same lock, so the release is one census event.
    fn resume_world(&self, snapshot: &ThreadSnapshot<'_>, excluded: Option<usize>) {
        // All root updates precede resumption. Compiled callers can retain
        // working references when no world pause occurred during a call.
        let _threads = self.threads.lock().unwrap();
        // Arena resets can retire a generated reservation. Invalidate every
        // census member, including the triggering collector and BLOCKED workers,
        // before any thread can allocate or deregister. Generated capacity checks
        // then force native epoch validation without a per-object atomic read.
        for state in snapshot {
            unsafe { state.invalidate_inline_tlab(); state.advance_relocation_epoch();
            }
        }
        for state in snapshot {
            if excluded == Some(ThreadStateHandle::as_ptr(state) as usize) { continue; }
            state.clear_poll();
        }
        self.gc_requested.store(false, Ordering::Release);
        for state in snapshot {
            if excluded == Some(ThreadStateHandle::as_ptr(state) as usize) { continue; }
            unsafe { state.resume() };
        }
    }

    fn detach_empty_thread_census(threads: &mut ThreadCensus) -> Option<ThreadCensus> {
        if !threads.is_empty() || threads.capacity() == 0 { return None; }
        let empty = ThreadCensus::new_in(threads.allocator().clone());
        Some(std::mem::replace(threads, empty))
    }

    /// Deregister a mutator thread by its ThreadState pointer.
    ///
    /// The thread must not be running mutator code when this is called.
    /// Typically called after the thread has finished all work.
    /// # Safety
    /// Registered root owner is quiescent, absent from any active GC census, and retired permanently.
    pub unsafe fn deregister_thread(&self, state: &ThreadStateHandle) {
        let mut threads = self.threads.lock().unwrap();
        let ptr = ThreadStateHandle::as_ptr(state);
        if let Some(pos) = threads.iter().position(|t| ThreadStateHandle::as_ptr(t) == ptr) {
            // Salvage this thread's alloc-site counters before it's dropped, so
            // its allocations still appear in a later profile.
            self.fold_retired_alloc_counters(&threads[pos]);
            unsafe { state.set_poll_flag(std::ptr::null_mut()); }
            threads.swap_remove(pos);
        }
        let retired = Self::detach_empty_thread_census(&mut threads);
        drop(threads);
        drop(retired);
    }

    /// Retire an owning mutator after any active collection has finished.
    /// Existing RUNNING registrations on this OS thread are suspended before
    /// waiting for the collector, then restored under the collector lock. This
    /// includes sibling registrations: parking only the retiring state can
    /// otherwise deadlock a collector waiting for a sibling on the same thread.
    /// Already-BLOCKED states retain their enclosing scopes.
    /// A failure while modifying retirement state aborts before releasing the
    /// coordinator lock or restoring callers; partial retirement cannot unwind.
    ///
    /// # Safety
    /// Caller owns this registered state and keeps all roots live throughout.
    /// Publish JIT frame chains before calling and reload derived pointers
    /// afterward. The retired state must not resume managed execution.
    pub unsafe fn safe_deregister_thread(&self, state: &ThreadStateHandle) {
        unsafe { self.retire_thread(state, || {}); }
    }

    /// Runtime contexts may already be idle/BLOCKED. Clear their transient
    /// roots only under collector exclusion, before detaching ABI poll storage.
    pub(crate) unsafe fn safe_deregister_runtime_thread(&self, state: &ThreadStateHandle) {
        unsafe { self.retire_thread(state, || { state.retire_runtime_roots(); }); }
    }

    unsafe fn retire_thread(&self, state: &ThreadStateHandle, cleanup: impl FnOnce()) {
        let mut callers = self.suspend_current_registrations();
        let _gc_guard = self.lock_coordinator();
        // Retirement cannot unwind through the caller-restoration guard after
        // partially modifying or poisoning the registry. Keep exclusion and
        // caller storage outside this fatal boundary.
        let retired_census = abort_on_collection_panic(|| {
            cleanup();
            let mut threads = self.threads.lock().unwrap();
            let ptr = ThreadStateHandle::as_ptr(state);
            if let Some(pos) = threads.iter().position(|t| ThreadStateHandle::as_ptr(t) == ptr) {
                self.fold_retired_alloc_counters(&threads[pos]);
                unsafe { state.set_poll_flag(std::ptr::null_mut()); }
                threads.swap_remove(pos);
            }
            let retired = Self::detach_empty_thread_census(&mut threads);
            drop(threads);
            callers.restore();
            retired
        });
        // A detached census is empty: no surviving registration needs its
        // storage. Release allocator backing after both registry and collector
        // locks, including when external observers retain the retired states.
        drop(_gc_guard);
        drop(retired_census);
    }

    // ─── Allocation ─────────────────────────────────────────────

    /// Allocate from the shared from-space (atomic bump).
    ///
    /// Returns null if from-space is exhausted. Caller should
    /// trigger GC via `collect()` and retry.
    /// # Safety
    /// Caller must be a registered RUNNING mutator of this heap, or hold
    /// exclusive access with no collector/mutator activity. A world pause is
    /// not permission to allocate: it may belong to another thread.
    /// Keep every live reference rooted and finish initialization before any
    /// safepoint, collection, heap walk, or publication. Raw allocation must
    /// initialize the matching header and variable-length count itself.
    pub unsafe fn alloc(&self, info: &TypeInfo, varlen_len: usize) -> *mut u8 {
        if !self.accepts_allocation(info, varlen_len) {
            return core::ptr::null_mut();
        }
        self.from_space().alloc(info, varlen_len)
    }

    /// Allocate and initialize header + varlen count.
    /// # Safety
    /// Caller must be a registered RUNNING mutator of this heap, or hold
    /// exclusive access with no collector/mutator activity. A world pause is
    /// not permission to allocate: it may belong to another thread.
    /// Keep every live reference rooted and finish initialization before any
    /// safepoint, collection, heap walk, or publication. Raw allocation must
    /// initialize the matching header and variable-length count itself.
    /// In a generational heap this allocates in tenured space. Mark cards for
    /// every initialized or updated slot containing a nursery reference.
    ///
    /// ```compile_fail
    /// use gcrust_rt::gc::{Full, Heap, ObjHeader, TypeInfo};
    /// let info = TypeInfo::for_header(Full::SIZE);
    /// let heap = Heap::new::<Full>(4096, vec![info]);
    /// heap.alloc_obj::<Full>(&info, 0);
    /// ```
    pub unsafe fn alloc_obj<H: ObjHeader>(&self, info: &TypeInfo, varlen_len: usize) -> *mut u8 {
        if !self.accepts_header::<H>(info, varlen_len) {
            return core::ptr::null_mut();
        }
        unsafe { crate::gc::alloc::alloc_obj::<H>(self.from_space(), info, varlen_len) }
    }

    /// Register a `RootSource` that will be scanned during EVERY collection
    /// — including the alloc-triggered (mutator-triggered) GC paths that
    /// don't take per-call extras. The pointer must remain valid for the
    /// heap's lifetime.
    ///
    /// This is the fix for a class of dangling-root bugs where embedders
    /// (e.g. JIT modules with `LiteralPool` roots) registered an
    /// extras source via the wrapping runtime — but mutator-triggered
    /// GC inside `alloc_obj` ignored those wrapper-level extras and
    /// collected the embedder's roots as garbage. Use `permanent_extras`
    /// for any RootSource whose lifetime equals the heap's.
    ///
    /// # Safety
    /// `source` must outlive the `Heap`. The heap stores it as a raw
    /// pointer; calling code is responsible for keeping the pointee alive
    /// (typically by holding it behind a `Box` or owned field). Register while
    /// owning a RUNNING mutator in this heap, or with exclusive quiescent access,
    /// before another collector can reclaim objects held only by this source.
    /// Sources must contain live values for this heap's pointer policy, expose
    /// nonoverlapping slots, and stay quiescent throughout collection. Growth
    /// may park and relocate objects: publish frame chains and all other live
    /// roots before calling, and reload derived pointers after return.
    pub unsafe fn register_permanent_extra(&self, source: *const dyn RootSource) {
        unsafe { permanent_roots::register(self, source); }
    }

    /// # Safety
    /// As ScopedExtraRegistry::register: roots stay published until all late
    /// collector writes finish, including the native maintenance interval.
    unsafe fn register_scoped_extras<'h, 'n, 'r>(
        &'h self, entry: std::pin::Pin<&'n ScopedExtraSource<'r>>,
    ) -> Option<ScopedExtraGuard<'h, 'n, 'r>> {
        let guard = unsafe { self.scoped_extras.register(entry) };
        if guard.is_some() { self.saw_extra_roots.store(true, Ordering::Relaxed); }
        guard
    }

    unsafe fn scan_scoped_roots(&self, visit: &mut dyn FnMut(*mut u64)) {
        unsafe { self.scoped_extras.scan(visit); }
    }

    // ─── Generational helpers ────────────────────────────────────

    /// Address of the JIT-facing allocation window (stable: boxed).
    pub fn alloc_window_ptr(&self) -> *const crate::gc::alloc::AllocWindow {
        self.alloc_window.as_raw().cast_const()
    }

    /// Re-point the allocation window at the active from-space. MUST be
    /// called once the heap has reached its final address (the
    /// constructors run before the move into `Arc`, so the cursor
    /// pointer they record would dangle). Preserves a CLOSED window
    /// (limit 0: stress / generational).
    /// # Safety
    /// No mutator or collector may read/use the window during this operation;
    /// hold exclusive heap access or a world pause and do not allocate in it.
    pub unsafe fn sync_alloc_window(&self) {
        let closed = self.alloc_window.limit.load(Ordering::Acquire) == 0
            || self.nursery_state.is_some()
            || self.gc_every_alloc();
        let space = self.from_space();
        let limit = if closed { 0 } else { space.size() };
        unsafe {
            self.alloc_window.point_at(space, limit);
        }
    }

    /// Check if this heap has a nursery (generational mode).
    #[inline(always)]
    pub fn has_nursery(&self) -> bool {
        self.nursery_state.is_some()
    }

    /// Check if a pointer is in the nursery.
    #[inline(always)]
    pub fn is_nursery(&self, ptr: *const u8) -> bool {
        match &self.nursery_state {
            Some(ns) => ns.nursery.contains(ptr),
            None => false,
        }
    }

    /// Check if a pointer is in tenured space (either from or to).
    #[inline(always)]
    pub fn is_tenured(&self, ptr: *const u8) -> bool {
        self.from_space().contains(ptr) || self.to_space().contains(ptr)
    }

    /// Configure an owning mutator's compiled allocation window once the heap
    /// and ThreadState are at stable addresses. Only nursery allocation is
    /// exposed: semispace flips change the active arena and use the runtime.
    pub(crate) unsafe fn configure_inline_tlab(&self, thread: &ThreadState) -> *mut crate::gc::InlineTlab {
        let window = unsafe { thread.inline_tlab_ptr() };
        if let Some(ns) = &self.nursery_state {
            unsafe {
                (*window).epoch = ns.nursery.epoch_ptr();
                (*window).expected_epoch = ns.nursery.epoch();
                (*window).base = ns.nursery.base();
                (*window).stress = &self.gc_every_alloc;
            }
        }
        window
    }

    /// Fixed-layout runtime allocation without revalidating the canonical
    /// type descriptor on every object. Construction validates the entire
    /// table; dynamic additions obey the same contract. This fast path never
    /// collects, so frame publication is needed only on the slow path.
    /// Caller must be this heap's owning RUNNING mutator, between safepoints.
    pub(crate) unsafe fn try_alloc_runtime_fixed(&self, thread: &ThreadState, type_id: u16) -> *mut u8 {
        if self.gc_every_alloc() || !self.header.is::<super::header::Full>() {
            return core::ptr::null_mut();
        }
        let info = self.type_info_by_id(type_id);
        if info.varlen != VarLenKind::None { return core::ptr::null_mut(); }
        let size = info.allocation_size(0);
        if size > 8192 { return core::ptr::null_mut(); }
        let space = self.nursery_state.as_ref().map_or_else(|| self.from_space(), |ns| &ns.nursery);
        let ptr = unsafe { thread.alloc_local_sized(space, size) };
        if !ptr.is_null() { unsafe { crate::gc::field::init_header::<super::header::Full>(ptr, type_id); } }
        ptr
    }

    /// Allocate through the owning thread's local buffer.
    /// # Safety
    /// Called only by this state's owning mutator while running, before any
    /// safepoint. The state must remain bound to this heap.
    pub(crate) unsafe fn alloc_mutator(
        &self,
        thread: &ThreadState,
        info: &TypeInfo,
        len: usize,
    ) -> *mut u8 {
        let Some(request) = self.allocation_request(info, len) else { return core::ptr::null_mut(); };
        unsafe { request.allocate(thread) }
    }

    pub(crate) unsafe fn alloc_mutator_obj<H: ObjHeader>(
        &self, thread: &ThreadState, info: &TypeInfo, len: usize,
    ) -> *mut u8 {
        let Some(request) = self.object_request::<H>(info, len) else { return core::ptr::null_mut(); };
        unsafe { request.allocate(thread) }
    }

    /// Allocate from the nursery if generational, otherwise from from-space.
    /// # Safety
    /// Caller must be a registered RUNNING mutator of this heap, or hold
    /// exclusive access with no collector/mutator activity. A world pause is
    /// not permission to allocate: it may belong to another thread.
    /// Keep every live reference rooted and finish initialization before any
    /// safepoint, collection, heap walk, or publication. Raw allocation must
    /// initialize the matching header and variable-length count itself.
    pub unsafe fn alloc_nursery(&self, info: &TypeInfo, varlen_len: usize) -> *mut u8 {
        if !self.accepts_allocation(info, varlen_len) {
            return core::ptr::null_mut();
        }
        match &self.nursery_state {
            Some(ns) => ns.nursery.alloc(info, varlen_len),
            None => self.from_space().alloc(info, varlen_len),
        }
    }

    /// Allocate and init header in the nursery if generational, otherwise from-space.
    /// # Safety
    /// Caller must be a registered RUNNING mutator of this heap, or hold
    /// exclusive access with no collector/mutator activity. A world pause is
    /// not permission to allocate: it may belong to another thread.
    /// Keep every live reference rooted and finish initialization before any
    /// safepoint, collection, heap walk, or publication. Raw allocation must
    /// initialize the matching header and variable-length count itself.
    pub unsafe fn alloc_nursery_obj<H: ObjHeader>(
        &self,
        info: &TypeInfo,
        varlen_len: usize,
    ) -> *mut u8 {
        if !self.accepts_header::<H>(info, varlen_len) {
            return core::ptr::null_mut();
        }
        match &self.nursery_state {
            Some(ns) => unsafe { crate::gc::alloc::alloc_obj::<H>(&ns.nursery, info, varlen_len) },
            None => unsafe { crate::gc::alloc::alloc_obj::<H>(self.from_space(), info, varlen_len) },
        }
    }

    /// Allocate directly in tenured from-space (for promotion during minor GC).
    /// # Safety
    /// Caller must be a registered RUNNING mutator of this heap, or hold
    /// exclusive access with no collector/mutator activity. A world pause is
    /// not permission to allocate: it may belong to another thread.
    /// Keep every live reference rooted and finish initialization before any
    /// safepoint, collection, heap walk, or publication. Raw allocation must
    /// initialize the matching header and variable-length count itself.
    pub unsafe fn alloc_tenured(&self, info: &TypeInfo, varlen_len: usize) -> *mut u8 {
        unsafe { self.alloc(info, varlen_len) }
    }

    /// Mark the card covering a tenured address, in either physical space.
    /// Forwarded canonical objects live in the destination during concurrent
    /// copying. Native/nursery/out-of-range addresses have no tenured card.
    #[inline(always)]
    pub fn mark_card_dirty(&self, addr: *const u8) {
        if let Some(ns) = &self.nursery_state {
            if let Some(index) = self.spaces.iter().position(|space| space.contains(addr)) {
                ns.card_tables[index].mark_dirty(addr);
            }
        }
    }

    /// Number of minor collections performed.
    pub fn minor_collections(&self) -> usize {
        match &self.nursery_state {
            Some(ns) => ns.minor_collections.load(Ordering::Relaxed),
            None => 0,
        }
    }

    /// Nursery bytes used.
    pub fn nursery_used(&self) -> usize {
        match &self.nursery_state {
            Some(ns) => ns.nursery.used(),
            None => 0,
        }
    }

    /// Whether a validated object extent can ever fit in the nursery.
    pub(crate) fn allocation_fits_nursery(&self, bytes: usize) -> bool {
        self.nursery_state.as_ref().is_some_and(|ns| bytes <= ns.nursery.size())
    }

    // ─── Safepoint polling ──────────────────────────────────────

    /// Check if GC has been requested. Mutator threads should call
    /// this at safepoints (loop backedges, allocation slow paths).
    #[inline(always)]
    pub fn gc_requested(&self) -> bool {
        self.gc_requested.load(Ordering::Acquire)
    }

    /// # Safety
    /// Caller owns this registered state. All current-OS-thread registrations
    /// for this heap must publish live references before parking and reload
    /// derived pointers afterward.
    pub unsafe fn safepoint(&self, _thread: &ThreadState) {
        if self.gc_requested.load(Ordering::Acquire) {
            // Publish the whole OS-thread group before waiting. Polling must
            // resume between concurrent collection's pause phases, while its
            // coordinator remains held and write barriers remain active.
            let mut callers = self.suspend_current_registrations();
            loop {
                let census = self.threads.lock().unwrap();
                if !self.gc_requested() {
                    // No new pause can sample a partially restored group.
                    callers.restore_after_pause();
                    break;
                }
                drop(census);
                std::thread::yield_now();
            }
        }
    }

    // ─── Collection ─────────────────────────────────────────────

    /// Bytes used in from-space.
    pub fn from_used(&self) -> usize {
        self.from_space().used()
    }

    /// Base pointer of from-space (for diagnostics).
    pub fn from_base(&self) -> *const u8 {
        self.from_space().base()
    }

    /// Base pointer of to-space (for diagnostics).
    pub fn to_base(&self) -> *const u8 {
        self.to_space().base()
    }

    /// Check if a pointer is specifically in tenured from-space (not nursery).
    pub fn from_space_contains(&self, ptr: *const u8) -> bool {
        self.from_space().contains(ptr)
    }

    /// Check if a pointer is in from-space (or nursery for generational heaps).
    pub fn contains(&self, ptr: *const u8) -> bool {
        self.from_space().contains(ptr) || self.is_nursery(ptr)
    }

    /// Check if a pointer is in either tenured space or nursery.
    pub fn contains_either(&self, ptr: *const u8) -> bool {
        self.from_space().contains(ptr) || self.to_space().contains(ptr) || self.is_nursery(ptr)
    }

    /// Total size of each space.
    pub fn space_size(&self) -> usize {
        self.from_space().size()
    }

    /// Largest fixed arena that can hold one requested object. This is an
    /// extent limit, not a guarantee that its live set fits after collection.
    pub(crate) fn max_object_allocation_bytes(&self) -> usize {
        self.nursery_state.as_ref().map_or(self.space_size(), |ns| self.space_size().max(ns.nursery.size()))
    }

    /// Number of collections performed.
    pub fn collections(&self) -> usize {
        self.collections.load(Ordering::Relaxed)
    }

    /// Maximum combined active old/nursery occupancy observed at a collection
    /// pause or inspection pause. Includes reserved TLAB tails; excludes the
    /// inactive copy arena and native memory. This is a sampled high-water mark.
    pub fn sampled_heap_peak_bytes(&self) -> usize {
        self.sampled_heap_peak.load(Ordering::Relaxed)
    }

    /// Caller holds gc_lock with all mutators quiescent, so both generations
    /// are sampled from one census rather than across a promotion/reset race.
    unsafe fn sample_heap_occupancy(&self) {
        let occupied = self.from_used().checked_add(self.nursery_used())
            .expect("combined managed occupancy overflow");
        self.sampled_heap_peak.fetch_max(occupied, Ordering::Relaxed);
    }

    /// Record a collection event (cold path). `t0` is the [`std::time::Instant`]
    /// captured at the start of the stop-the-world copy/scan work; the pause is
    /// its elapsed time. Pushed under the events lock once per collection — never
    /// on the allocation hot path.
    pub(crate) fn record_gc_event(
        &self,
        kind: GcKind,
        t0: std::time::Instant,
        before_bytes: u64,
        after_bytes: u64,
        promoted_bytes: u64,
    ) {
        self.record_gc_duration(kind, t0.elapsed(), before_bytes, after_bytes, promoted_bytes);
    }

    fn record_gc_duration(
        &self, kind: GcKind, elapsed: std::time::Duration,
        before_bytes: u64, after_bytes: u64, promoted_bytes: u64,
    ) {
        self.record_gc_timings(kind, elapsed, elapsed, before_bytes, after_bytes, promoted_bytes);
    }

    fn record_gc_timings(
        &self, kind: GcKind, pause: std::time::Duration, elapsed: std::time::Duration,
        before_bytes: u64, after_bytes: u64, promoted_bytes: u64,
    ) {
        let pause_ns = pause.as_nanos().min(u64::MAX as u128) as u64;
        let elapsed_ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;
        self.gc_events.lock().unwrap().record(GcEvent {
            kind, seq: 0, pause_ns, elapsed_ns, before_bytes, after_bytes, promoted_bytes,
        });
    }

    // Public telemetry access can wait on a trace sink held by another caller.
    // Park all current registrations before touching its mutex/native I/O. The
    // telemetry guard must drop before restoring RUNNING, including on unwind.
    // Collector event recording deliberately retains its separate paused path.
    fn with_telemetry<R>(&self, inspect: impl FnOnce(&mut GcTelemetry) -> R) -> R {
        let _callers = self.suspend_current_registrations();
        let mut telemetry = self.gc_events.lock().unwrap();
        let result = inspect(&mut telemetry);
        #[cfg(test)]
        telemetry_heap_tests::operation_finished();
        result
    }

    /// Snapshot of the most recent [`crate::gc::GC_EVENT_HISTORY_LIMIT`] GC
    /// events, oldest first. Sequence numbers retain their lifetime positions
    /// (saturating at u64::MAX). Lifetime totals do not depend on this window.
    /// Configure a streamed trace before collecting to preserve every event.
    /// May suspend current mutators while waiting. Publish JIT frames before
    /// calling and reload derived managed pointers afterward, as at a safepoint.
    pub fn gc_events(&self) -> Vec<GcEvent> {
        self.with_telemetry(|telemetry| telemetry.events())
    }

    /// Stream a complete trace to a file, with bounded native buffering.
    /// Must be configured before the first collection. Creation/configuration
    /// errors are returned without replacing an existing trace or file.
    /// Call [`Self::flush_gc_log`] at the end to observe any write/flush error.
    /// May suspend current mutators while waiting. Publish JIT frames before
    /// calling and reload derived managed pointers afterward, as at a safepoint.
    pub fn start_gc_log(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        self.with_telemetry(|telemetry| {
            telemetry.check_trace_start()?;
            let file = std::fs::File::create(path)?;
            telemetry.start_trace(Box::new(file))
        })
    }

    /// Configure an embedding-provided trace sink before the first collection.
    /// Runtime buffering is bounded; the sink's storage policy is its owner's
    /// responsibility. A failed sink stops receiving events; lifetime accounting
    /// continues and [`Self::flush_gc_log`] reports the first error persistently.
    /// The sink runs under collection/telemetry locks: it must not reenter the
    /// managed runtime or this heap. Blocking sink I/O can delay a collection;
    /// streaming is opt-in and has no effect on the allocation hot path.
    /// May suspend current mutators while waiting. Publish JIT frames before
    /// calling and reload derived managed pointers afterward, as at a safepoint.
    pub fn set_gc_log_writer<W: std::io::Write + Send + 'static>(&self, writer: W) -> std::io::Result<()> {
        self.with_telemetry(|telemetry| {
            telemetry.check_trace_start()?;
            telemetry.start_trace(Box::new(writer))
        })
    }

    /// Flush the streamed trace, returning any earlier write or flush error.
    /// This does not close the sink; collection and further flushes may continue.
    /// May suspend current mutators while waiting. Publish JIT frames before
    /// calling and reload derived managed pointers afterward, as at a safepoint.
    pub fn flush_gc_log(&self) -> std::io::Result<()> {
        self.with_telemetry(|telemetry| telemetry.flush_trace())
    }

    /// JSON-lines snapshot of the retained recent events. This bounded snapshot
    /// may begin at a nonzero sequence number. `GCR_GC_LOG` uses the streaming
    /// path instead, preserving every collection from startup.
    /// May suspend current mutators while waiting. Publish JIT frames before
    /// calling and reload derived managed pointers afterward, as at a safepoint.
    pub fn gc_log_jsonl(&self) -> String {
        let mut bytes = Vec::new();
        for event in self.gc_events() { write_event_jsonl(&mut bytes, event).unwrap(); }
        String::from_utf8(bytes).expect("GC event JSON is ASCII")
    }

    /// Human-readable lifetime GC totals and nearest-rank pause quantiles.
    /// Quantiles are exact while all events fit in the recent window. After
    /// rollover, a bounded lifetime histogram reports upper estimates with
    /// <1/64 relative value error, explicitly labeled in the output. Cumulative
    /// durations and byte/count totals saturate at u64::MAX (lower bounds).
    /// May suspend current mutators while waiting. Publish JIT frames before
    /// calling and reload derived managed pointers afterward, as at a safepoint.
    pub fn gc_stats_summary(&self) -> String {
        let snapshot = self.with_telemetry(|telemetry| telemetry.snapshot());
        let mut out = String::new();
        for kind in [GcKind::Minor, GcKind::Major, GcKind::Concurrent] {
            let stat = snapshot.by_kind[kind_index(kind)];
            if stat.count == 0 { continue; }
            let ms = |ns: u64| ns as f64 / 1.0e6;
            let quantile_note = if stat.quantile_error_ratio < 0.0 {
                " | quantiles upper bounds (sample census saturated; error unbounded)"
            } else if stat.quantile_error_ratio > 0.0 {
                " | quantiles upper estimates (<1.5625% relative value error)"
            } else { "" };
            out.push_str(&format!(
                "gc-rust: {} {} collections | pause p50 {:.3}ms p99 {:.3}ms max {:.3}ms | reclaimed {} B | promoted {} B{}\n",
                stat.count, kind.as_str(), ms(stat.p50), ms(stat.p99), ms(stat.pause_max),
                stat.reclaimed, stat.promoted, quantile_note));
        }
        out
    }

    /// Machine-readable per-run metrics for `gcr bench` (written when
    /// `GCR_METRICS_FILE` is set). A flat JSON object of scalar metrics
    /// aggregated from lifetime counters + allocation-site table — the rich,
    /// runtime-only numbers (GC cycles, pause distribution, allocation churn,
    /// peak heap) that you cannot get out of a native Rust/Go binary. Serde-free,
    /// to keep the minimal runtime LLVM/serde-free. Cumulative GC time and
    /// reclaimed/promoted byte totals saturate at `u64::MAX`; such values
    /// represent lower bounds. Allocation-site totals likewise saturate at `u64::MAX`.
    /// Quantiles are exact through 1024 events; after rollover they are lifetime
    /// histogram upper estimates. `gc_pause_quantile_error_ratio` is 0 for exact
    /// values, 1/64 for histogram estimates, or -1 if even the 128-bit histogram
    /// sample census overflows (then quantiles fall back to the recorded max).
    /// May suspend current mutators while waiting. Publish JIT frames before
    /// calling and reload derived managed pointers afterward, as at a safepoint.
    pub fn metrics_json(&self) -> String {
        let snapshot = self.with_telemetry(|telemetry| telemetry.snapshot());
        let stat = snapshot.total;
        let minor = snapshot.by_kind[0].count;
        let major = snapshot.by_kind[1].count.saturating_add(snapshot.by_kind[2].count);
        let ms = |ns: u64| ns as f64 / 1.0e6;
        let (mut alloc_objects, mut alloc_bytes) = (0u64, 0u64);
        for s in self.alloc_site_profile() {
            alloc_objects = alloc_objects.saturating_add(s.count);
            alloc_bytes = alloc_bytes.saturating_add(s.bytes);
        }
        // The profile's inspection pause also samples combined occupancy, so
        // runs without a collection still report their current allocated heap.
        let peak = self.sampled_heap_peak_bytes();
        let native = &self.native_memory;
        let registry_bytes = native.registry.bytes();
        let registry_peak = native.registry.peak().max(registry_bytes);
        // Scratch belongs to OS threads and can outlive any particular heap.
        let scratch = collector_scratch_stats();
        format!(
            "{{\"gc_minor\":{},\"gc_major\":{},\"gc_pause_total_ms\":{:.4},\
             \"gc_pause_max_ms\":{:.4},\"gc_pause_p50_ms\":{:.4},\"gc_pause_p99_ms\":{:.4},\"gc_elapsed_total_ms\":{:.4},\
             \"gc_reclaimed_bytes\":{},\"gc_promoted_bytes\":{},\"peak_heap_bytes\":{},\
             \"alloc_objects\":{},\"alloc_bytes\":{},\"gc_events_retained\":{},\"gc_pause_quantile_error_ratio\":{},\"native_buffer_bytes\":{},\"native_buffer_peak_bytes\":{},\"native_registry_bytes\":{},\"native_registry_peak_bytes\":{},\"native_pressure_collections\":{},\"process_collector_scratch_bytes\":{},\"process_collector_scratch_peak_bytes\":{}}}",
            minor,
            major,
            ms(stat.pause_total),
            ms(stat.pause_max),
            ms(stat.p50),
            ms(stat.p99),
            ms(stat.elapsed_total),
            stat.reclaimed,
            stat.promoted,
            peak,
            alloc_objects,
            alloc_bytes,
            snapshot.retained,
            stat.quantile_error_ratio,
            native.bytes(),
            native.peak(),
            registry_bytes,
            registry_peak,
            native.collections(),
            scratch.requested_bytes,
            scratch.peak_requested_bytes.max(scratch.requested_bytes),
        )
    }

    /// Run a stop-the-world collection.
    ///
    /// A panic during collection aborts the process: partially rewritten roots
    /// and objects cannot be rolled back or exposed to resumed mutators.
    ///
    /// This is designed to be called by one thread (the triggering thread)
    /// while all other threads are at safepoints, OR by a dedicated GC
    /// thread after requesting all mutators to stop.
    ///
    /// # Safety
    /// - All objects must match their registered layouts, including initialized
    ///   headers, lengths and reference representations for `P`.
    /// - Every root source must expose live, writable, nonoverlapping slots that
    ///   remain valid and quiescent until return, including native maintenance.
    ///   No managed allocation, mutation, or independent root scan may overlap
    ///   collection.
    /// - All mutator threads must be at safepoints (not running mutator code).
    /// - `extra_roots` can provide additional root sources beyond the
    ///   registered threads and globals.
    pub unsafe fn collect<P: PtrPolicy>(&self, extra_roots: &[&dyn RootSource]) {
        // A competing collection may run during off-pause native maintenance.
        // Keep the supplied root sources published through that entire interval.
        let scoped = std::pin::pin!(ScopedExtraSource::new(extra_roots));
        let _extras = unsafe { self.register_scoped_extras(scoped.as_ref()) };
        {
            let _gc_guard = self.lock_coordinator();
            abort_on_collection_panic(|| unsafe { self.collect_inner::<P>(&[]) });
        }
        unsafe { self.compact_native_registries(); }
        if let Some(extras) = _extras {
            // Root addresses can outlive enumeration on parallel workers. Wait
            // through every forwarding store before releasing borrowed storage.
            let _guard = self.lock_coordinator();
            drop(extras);
        }
    }

    /// Internal collection logic — caller must hold `gc_lock`.
    unsafe fn collect_inner<P: PtrPolicy>(&self, extra_roots: &[&dyn RootSource]) {
        unsafe { self.collect_inner_with_nursery_roots::<P>(extra_roots, None); }
    }

    /// A nursery filter is valid only when computed from the complete root
    /// closure during this same pause, followed by minor collection before
    /// resumption. Dead nursery objects may retain stale old-space bits after
    /// this major; that unreachable storage must be reset in the same pause.
    unsafe fn collect_inner_with_nursery_roots<P: PtrPolicy>(
        &self,
        extra_roots: &[&dyn RootSource],
        nursery_live: Option<&NurseryLive<'_>>,
    ) {
        unsafe { self.sample_heap_occupancy(); }
        // GCRUST_TRACEGC=1: per-collection accounting. FROM-used at entry must
        // be >= the survivors left by the previous collection — if it ever
        // goes BACKWARDS, the mutator's bump cursor was reset/misplaced and
        // new allocations OVERLAPPED survivors (the clobbered-slot class).
        if std::env::var_os("GCRUST_TRACEGC").is_some() {
            static LAST_SURVIVORS: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);
            let fu = self.from_space().used();
            let last = LAST_SURVIVORS.load(Ordering::Relaxed);
            eprintln!("[gc] #{} from-used {} (prev survivors {})",
                      self.collections(), fu, last);
            if fu < last {
                eprintln!("[gc] !!! CURSOR WENT BACKWARDS: from-used {} < prev survivors {} — OVERLAP!", fu, last);
                std::process::abort();
            }
            // record survivors AFTER this collection via a deferred read: we
            // can't easily hook post-collect here, so read to_space().used()
            // at the END of collect_inner (see the paired store below).
        }
        // PREWALK (GCRUST_PREWALK=1): before touching anything, linearly parse
        // FROM-space up to the bump cursor and validate every object header —
        // a garbage/invalid type_id here means a mutator (e.g. a JIT inline
        // allocation) corrupted the heap BEFORE this collection; abort loudly
        // at the first bad object instead of crashing mysteriously later.
        if std::env::var_os("GCRUST_PREWALK").is_some() {
            let base = self.from_space().base();
            let used = self.from_space().used();
            let ntypes = self.type_table_len();
            let mut n = 0usize;
            for range in &self.from_space().initialized_ranges_in(self.native_memory.registry_allocator()) {
            let mut off = range.start;
            while off < range.end {
                let obj = unsafe { base.add(off) };
                let tid = unsafe { *(obj.add(self.type_id_offset) as *const u16) };
                if (tid as usize) >= ntypes {
                    eprintln!(
                        "[gcrust PREWALK] INVALID type_id {} at from-space offset {} (obj #{}, used {}) — heap corrupted pre-GC",
                        tid, off, n, used
                    );
                    std::process::abort();
                }
                let info = self.type_info_by_id(tid);
                let varlen_len = match info.varlen {
                    VarLenKind::None => 0,
                    _ => unsafe { read_varlen_count(obj, info) },
                };
                let obj_size = info.allocation_size(varlen_len);
                if obj_size == 0 || obj_size > range.end - off {
                    eprintln!(
                        "[gcrust PREWALK] BAD size {} (tid {}) at offset {} (obj #{}, used {})",
                        obj_size, tid, off, n, used
                    );
                    std::process::abort();
                }
                let align = 1usize << info.align_log2;
                off = (off + obj_size + align - 1) & !(align - 1);
                n += 1;
            }
            }
        }
        // GC-log timing (cold path): pause = the copy/scan work below; `before`
        // is from-space occupancy now, `after` is occupancy after the swap.
        // Started after the opt-in checks above so their cost isn't charged to
        // the measured pause.
        let gc_t0 = std::time::Instant::now();
        let gc_before = self.from_used() as u64;
        self.clear_destination_cards();
        unsafe { self.parallel_major_with_nursery_roots::<P>(extra_roots, nursery_live) };

        if std::env::var_os("GCRUST_TRACEGC").is_some() {
            let su = self.to_space().used();
            eprintln!("[gc] #{} survivors {}", self.collections(), su);
        }
        // VERIFY (GCRUST_VERIFY=1): re-walk to-space; every traced slot must
        // be null/immediate or point inside to-space or the nursery (a stale
        // old from-space pointer
        // here is exactly the dangling-ref class behind the jsir bench-mode
        // SIGSEGVs — catch it AT THE COLLECTION, with the object identified).
        if std::env::var_os("GCRUST_VERIFY").is_some() {
            let base = self.to_space().base() as usize;
            let end = base + self.to_space().used();
            let mut off = 0usize;
            while off < self.to_space().used() {
                let obj = unsafe { self.to_space().base().add(off) };
                let type_id = unsafe { read_type_id(obj, self.type_id_offset) };
                assert!(
                    (type_id as usize) < self.type_table.len(),
                    "GC VERIFY: bad type_id {} at to-space+{:#x}",
                    type_id, off
                );
                let info = &self.type_table[type_id as usize];
                unsafe {
                    scan_object(obj, info, |slot| {
                        let bits = *slot;
                        if let Some(p) = P::try_decode_ptr(bits) {
                            let pa = p as usize;
                            assert!(
                                (pa >= base && pa < end) || self.is_nursery(p),
                                "GC VERIFY: object type_id={} at to-space+{:#x} holds                                  out-of-space ref {:#x} (slot {:p}); to-space [{:#x},{:#x})",
                                type_id, off, pa, slot, base, end
                            );
                        }
                    });
                }
                let varlen_len = match info.varlen {
                    VarLenKind::None => 0,
                    _ => unsafe { read_varlen_count(obj, info) },
                };
                let obj_size = info.allocation_size(varlen_len);
                let align = 1usize << info.align_log2;
                off = (off + obj_size + align - 1) & !(align - 1);
            }
        }

        // Phase 3: swap spaces (flip atomic index — no UB, no ptr::swap)
        unsafe { self.sweep_native_owners(false); }
        self.swap_spaces();
        unsafe { self.to_space().reset() };
        // POISON (GCRUST_POISON=1): fill the just-evacuated space with a
        // pattern so STALE POINTERS read garbage immediately instead of
        // plausible zeros from later reuse (the jsir navier divergence read
        // an all-zero 16900-slot array through a stale ref ~14 iterations
        // after the collection that moved it).
        if std::env::var_os("GCRUST_POISON").is_some() {
            let sp = self.to_space();
            unsafe {
                std::ptr::write_bytes(sp.base(), 0xBF, sp.size());
            }
        }
        self.collections.fetch_add(1, Ordering::Relaxed);
        // GC-log: after the swap, from-space holds the survivors (post-collect
        // occupancy). Major collection promotes nothing here.
        self.record_gc_event(GcKind::Major, gc_t0, gc_before, self.from_used() as u64, 0);

        // The new from-space remembered set was rebuilt during Cheney scanning.
        // Keep its old-to-young edges until the next minor collection.
    }

    /// Trigger a STW collection cycle: request all threads to stop,
    /// wait for them, collect, then resume them.
    ///
    /// # Safety
    /// Same as `collect`. The calling thread must NOT be registered
    /// as a mutator (or must have already entered its safepoint).
    /// Collection failures use the fatal policy documented on [`Heap::collect`].
    pub unsafe fn stw_collect<P: PtrPolicy>(&self) {
        {
            let _gc_guard = self.lock_coordinator();
            abort_on_collection_panic(|| unsafe { self.stw_collect_locked::<P>() });
        }
        unsafe { self.compact_native_registries(); }
    }

    /// Caller holds gc_lock and contains collection panics before resuming anyone.
    unsafe fn stw_collect_locked<P: PtrPolicy>(&self) {

        self.trace_gc(TraceState::GcWaitingForSafepoints);

        // Snapshot thread refs and set gc_requested atomically under threads lock.
        // This prevents a thread from deregistering between gc_requested being set
        // and the snapshot being taken.
        let thread_snapshot: ThreadSnapshot<'_> = {
            let threads = self.threads.lock().unwrap();
            self.gc_requested.store(true, Ordering::Release);
            ScopedSnapshot::capture(threads.iter().cloned(), self.native_memory.registry_allocator())
        };
        // Raise each thread's JIT poll flag so a thread spinning in
        // JIT'd code parks at its next safepoint poll, then wait.
        for ts in thread_snapshot.iter() {
            ts.request_poll();
        }
        for ts in &thread_snapshot {
            while !ts.is_safely_at_safepoint() {
                std::thread::yield_now();
            }
            self.trace_thread(ts, TraceState::AtSafepoint);
        }

        self.trace_gc(TraceState::GcStw);
        unsafe { self.collect_inner::<P>(&[]) };

        self.trace_gc(TraceState::GcResuming);
        self.resume_world(&thread_snapshot, None);
        for ts in &thread_snapshot {
            self.trace_thread(ts, TraceState::Running);
        }
        self.trace_gc(TraceState::GcIdle);
    }

    /// Stop the world WITHOUT collecting, returning a guard that resumes
    /// every parked thread when dropped.
    ///
    /// Parks every *other* registered mutator thread at a safepoint (the
    /// requesting thread — identified by OS thread id — is excluded so it
    /// can keep running). Used to make a non-allocating critical section
    /// (e.g. installing new JIT code: `engine.add_module`, growing the
    /// merging shape maps) safe against
    /// concurrent JIT execution and GC.
    ///
    /// Holds `gc_lock` for the guard's lifetime, so no collection can run
    /// concurrently either.
    ///
    /// # Safety / contract
    /// The critical section guarded by the returned `WorldPause` MUST NOT
    /// allocate or trigger a GC: doing so would re-enter `gc_lock` (held
    /// here) and deadlock. Run any allocating / JIT-executing work after
    /// the guard is dropped.
    /// Current RUNNING registrations are suspended before lock acquisition and
    /// restored by the guard. JIT integrations must publish their frame chains
    /// before calling, as at any safepoint; derived pointers may be relocated
    /// while waiting and must be reloaded. Rust frame-chain roots stay registered.
    #[must_use]
    pub fn pause_world(&self) -> WorldPause<'_> {
        let cur = std::thread::current().id();
        // Waiting for gc_lock while RUNNING can deadlock with its collector,
        // which is waiting for us to reach a safepoint. Suspend every current
        // registration before waiting; previously BLOCKED scopes stay blocked.
        let callers = self.suspend_current_registrations();
        let gc_guard = self.lock_coordinator();
        // Keep both guards outside the panic boundary: setup failure must
        // abort before releasing exclusion or restoring current registrations.
        let parked = abort_on_collection_panic(|| {
            let parked = {
                let threads = self.threads.lock().unwrap();
                self.gc_requested.store(true, Ordering::Release);
                // Current registrations were suspended above. Only siblings need
                // polling/resuming; capture their owners under the registry lock.
                ScopedSnapshot::capture(
                    threads.iter().filter(|ts| ts.os_thread() != cur).cloned(),
                    self.native_memory.registry_allocator(),
                )
            };
            for ts in &parked {
                ts.request_poll();
            }
            for ts in &parked {
                while !ts.is_safely_at_safepoint() {
                    std::thread::yield_now();
                }
            }
            unsafe { self.sample_heap_occupancy(); }
            parked
        });
        WorldPause { heap: self, parked, _gc_guard: gc_guard, callers }
    }

    /// Publish current RUNNING registrations before a native wait. Collector
    /// entry points restore under coordinator exclusion. Other native waits
    /// release their native guards first; restoration waits out any active GC.
    pub(super) fn suspend_current_registrations(&self) -> PauseCallers<'_> {
        let cur = std::thread::current().id();
        let mut callers = PauseCallers { heap: self, head: None };
        let threads = self.threads.lock().unwrap();
        for thread in threads.iter().filter(|t| t.os_thread() == cur && t.is_running()) {
            // No native allocation may delay parking any sibling registration.
            // The existing records carry ownership of the guard's chain.
            unsafe { thread.link_collection_caller(callers.head.take()); }
            callers.head = Some(thread.clone());
            unsafe { thread.enter_blocked(); }
        }
        callers
    }

    /// Run native work while the current OS thread's RUNNING registrations
    /// are suspended. Existing blocked scopes remain owned by their caller.
    /// # Safety
    /// Publish roots for all current registrations on this heap before entry;
    /// native work may not access managed storage, and derived pointers must
    /// be reloaded after return. Native ownership returned by the body must
    /// permit collection while registrations are being restored.
    pub(crate) unsafe fn with_current_registrations_blocked<R>(&self, body: impl FnOnce() -> R) -> R {
        let _callers = self.suspend_current_registrations();
        body()
    }

    /// Transfer a native suspension across an ABI boundary. The owning token
    /// retains the heap and restores its complete group when consumed.
    /// # Safety
    /// Publish all current registrations' roots and keep their frames valid
    /// until the token is consumed; none may independently resume or retire.
    pub(crate) unsafe fn suspend_owned(self: &Arc<Self>) -> OwnedPauseCallers {
        let mut callers = self.suspend_current_registrations();
        OwnedPauseCallers { heap: self.clone(), head: callers.head.take() }
    }

    /// Suspend every same-OS-thread registration before contending for a
    /// collection. The guard restores them before releasing gc_lock, including
    /// early returns after another collector has serviced this request.
    fn lock_mutator_collection(&self, kind: GcKind) -> (MutatorGcLock<'_>, bool) {
        let count = || match kind {
            GcKind::Major | GcKind::Concurrent => self.collections(),
            GcKind::Minor => self.minor_collections(),
        };
        let before = count();
        let callers = self.suspend_current_registrations();
        let gc_guard = self.lock_coordinator();
        let already_collected = count() != before;
        (MutatorGcLock { _gc_guard: Some(gc_guard), callers }, already_collected)
    }

    /// Bytes remaining in from-space.
    pub fn from_remaining(&self) -> usize {
        self.from_space().remaining()
    }

    /// Trigger STW collection from a mutator thread.
    ///
    /// All RUNNING registrations on this OS thread park before waiting for
    /// the collector lock. A completed major collection can service a waiting
    /// non-stress request; stress requests each perform their own collection.
    ///
    /// # Safety
    /// Same as `collect`; caller owns the registered triggering state. Publish
    /// every current JIT frame chain before calling and reload derived pointers
    /// afterward. Convenience wrapper: no extra root sources.
    /// Collection failures use the fatal policy documented on [`Heap::collect`].
    pub unsafe fn mutator_triggered_gc<P: PtrPolicy>(&self, triggering_thread: &ThreadState) {
        unsafe { self.mutator_triggered_gc_with_extras::<P>(triggering_thread, &[]) }
    }

    /// Read each registered `ThreadState`'s `parked_jit_fp`. Returns
    /// only non-null FPs (threads currently parked at a JIT
    /// safepoint). Used by the JIT safepoint session to walk every
    /// parked thread's JIT frame as a root source during STW GC,
    /// ensuring the GC's relocations update spill slots in those
    /// frames.
    pub fn parked_thread_jit_fps(&self) -> Vec<*const u8> {
        let threads = self.threads.lock().unwrap();
        threads
            .iter()
            .filter_map(|ts| {
                let fp = ts.parked_jit_fp();
                if fp.is_null() { None } else { Some(fp) }
            })
            .collect()
    }

    /// Like [`mutator_triggered_gc`], but also scans `extra_roots`
    /// after the registered threads. Used by JIT frontends to thread
    /// per-safepoint frame roots and module-level literal pools into
    /// the trace alongside the standard root set.
    /// # Safety
    /// As `mutator_triggered_gc`; the caller owns a registered RUNNING state.
    /// Extra root storage remains live and quiescent through every collection
    /// while waiting and while this call runs. Sources are registered before
    /// parking and unregistered on return; future collections require their own
    /// registration. Heap-lifetime sources belong in register_permanent_extra.
    /// Collection failures use the fatal policy documented on [`Heap::collect`].
    pub unsafe fn mutator_triggered_gc_with_extras<P: PtrPolicy>(
        &self,
        triggering_thread: &ThreadState,
        extra_roots: &[&dyn RootSource],
    ) {
        unsafe { self.mutator_major_with_extras::<P>(triggering_thread, extra_roots, None); }
    }

    /// Service allocation exhaustion. A competing concurrent cycle may retain
    /// SATB floating garbage and leave insufficient room even though a fresh
    /// stopped-world collection can reclaim it. Coalesce only when the active
    /// arena has room for this validated request after the competing cycle.
    ///
    /// # Safety
    /// As mutator_triggered_gc; required_bytes is a validated object extent.
    pub(crate) unsafe fn mutator_gc_for_allocation<P: PtrPolicy>(
        &self,
        triggering_thread: &ThreadState,
        required_bytes: usize,
    ) {
        unsafe { self.mutator_major_with_extras::<P>(triggering_thread, &[], Some(required_bytes)); }
    }

    unsafe fn mutator_major_with_extras<P: PtrPolicy>(
        &self,
        triggering_thread: &ThreadState,
        extra_roots: &[&dyn RootSource],
        required_bytes: Option<usize>,
    ) {
        let scoped = std::pin::pin!(ScopedExtraSource::new(extra_roots));
        let _extra_registration = unsafe { self.register_scoped_extras(scoped.as_ref()) };
        let (gc_guard, already_collected) = self.lock_mutator_collection(GcKind::Major);
        let coalesced = already_collected && !self.gc_every_alloc()
            && required_bytes.is_none_or(|bytes| self.from_remaining() >= bytes);
        if !coalesced {
            abort_on_collection_panic(|| unsafe {
                self.mutator_major_locked::<P>(triggering_thread);
            });
        }
        gc_guard.finish();
    }

    /// Caller holds the mutator collection guard outside the panic boundary.
    unsafe fn mutator_major_locked<P: PtrPolicy>(&self, triggering_thread: &ThreadState) {
        // We won the race — we're the GC thread now.
        // GC-log timing (cold path): the pause spans parking + copy/scan below.
        let gc_t0 = std::time::Instant::now();
        let gc_before = self.from_used() as u64;
        self.clear_destination_cards();

        // Snapshot thread refs and set gc_requested atomically (under threads lock).
        // This prevents a thread from deregistering between gc_requested being set
        // and the snapshot being taken — which would leave a "ghost" in the snapshot
        // that never reaches safepoint.
        let trigger_ptr = triggering_thread as *const ThreadState as usize;
        let thread_snapshot: ThreadSnapshot<'_> = {
            let threads = self.threads.lock().unwrap();
            self.gc_requested.store(true, Ordering::Release);
            ScopedSnapshot::capture(threads.iter().cloned(), self.native_memory.registry_allocator())
        };

        // Raise every other thread's JIT poll flag so a thread spinning
        // in JIT'd code parks at its next safepoint poll, then wait. If
        // this were missing, an allocation-free JIT loop on another
        // thread would never poll `gc_requested` and we'd spin forever
        // in `is_safely_at_safepoint`.
        for ts in thread_snapshot.iter() {
            if ThreadStateHandle::as_ptr(ts) as usize == trigger_ptr {
                continue;
            }
            ts.request_poll();
        }
        // Wait for all threads EXCEPT ourselves to reach safepoints
        for ts in thread_snapshot.iter() {
            if ThreadStateHandle::as_ptr(ts) as usize == trigger_ptr {
                continue;
            }
            while !ts.is_safely_at_safepoint() {
                std::thread::yield_now();
            }
        }

        unsafe { self.sample_heap_occupancy(); self.parallel_major::<P>(&[]) };

        // Phase 3: swap spaces
        unsafe { self.sweep_native_owners(false); }
        self.swap_spaces();
        unsafe { self.to_space().reset() };
        self.collections.fetch_add(1, Ordering::Relaxed);
        self.record_gc_event(GcKind::Major, gc_t0, gc_before, self.from_used() as u64, 0);

        // The new from-space remembered set was rebuilt during Cheney scanning.
        // Keep its old-to-young edges until the next minor collection.

        // Debug: verify all root slots point to from-space after swap
        if cfg!(debug_assertions) && self.gc_every_alloc() {
            for ts in thread_snapshot.iter() {
                unsafe { ts.scan_roots(&mut |slot| {
                    let bits = *slot;
                    if let Some(ptr) = P::try_decode_ptr(bits) {
                        assert!(
                            self.from_space().contains(ptr),
                            "POST-SWAP: root slot {:p} points to {:p} which is NOT in from-space \
                             [{:p}..+{}). In to-space: {}. collections={}",
                            slot,
                            ptr,
                            self.from_space().base(),
                            self.from_space().size(),
                            self.to_space().contains(ptr),
                            self.collections.load(Ordering::Relaxed),
                        );
                    }
                }); }
            }
        }

        // Clear request flag and resume all other threads.
        self.resume_world(&thread_snapshot, Some(trigger_ptr));

    }

    // ─── GC Phase ─────────────────────────────────────────────

    /// Stable phase flag for generated access fast paths. Valid only while
    /// this heap stays at its registered address and the mutator owns it.
    pub(crate) fn copying_phase_ptr(&self) -> *const AtomicU8 {
        &self.gc_phase
    }

    /// Stable flag for generated aggregate fast paths: nonzero once more
    /// than one mutator has registered.
    pub(crate) fn multi_mutator_ptr(&self) -> *const AtomicU8 {
        &self.multi_mutator
    }

    /// The nursery's (base address, size); (0, 0) without a nursery. Fixed
    /// for the heap's lifetime.
    pub(crate) fn nursery_range(&self) -> (usize, usize) {
        match &self.nursery_state {
            Some(ns) => (ns.nursery.base() as usize, ns.nursery.size()),
            None => (0, 0),
        }
    }

    /// Current GC phase.
    pub fn gc_phase(&self) -> GcPhase {
        match self.gc_phase.load(Ordering::Acquire) {
            1 => GcPhase::Copying,
            _ => GcPhase::Idle,
        }
    }

    /// Check if write barriers should be active.
    ///
    /// Relaxed ordering is correct here because:
    /// - Before STW #1: mutations are captured in the snapshot copy, no SATB needed.
    /// - After STW #1 resume: the safepoint Mutex provides a full memory barrier,
    ///   so gc_phase=Copying is visible to all mutators before they resume.
    #[inline(always)]
    pub fn barriers_active(&self) -> bool {
        self.gc_phase.load(Ordering::Relaxed) != GcPhase::Idle as u8
    }

    /// The type_id_offset for this heap's header type.
    pub fn type_id_offset(&self) -> usize {
        self.type_id_offset
    }

    /// Byte offset of the aligned word used for Heap forwarding.
    pub fn forwarding_offset(&self) -> usize {
        self.header.forwarding_offset
    }

    /// Compatibility hook that repeats an eight-byte write into a forwarded copy.
    ///
    /// This after-store hook does not synchronize the original write or order
    /// competing old/new aliases. It is not a standalone concurrent-write API.
    /// Protect the entire original operation with [`Heap::object_access`].
    ///
    /// # Safety
    /// `obj` is a valid object base and `field_offset` identifies an aligned,
    /// writable eight-byte field. The caller establishes the complete access
    /// protocol, including copying exclusion, language-level synchronization,
    /// rooting and reference/card/SATB barriers as appropriate.
    #[inline(always)]
    pub unsafe fn replication_barrier(&self, obj: *mut u8, field_offset: usize, new_bits: u64) {
        if !self.barriers_active() {
            return;
        }
        // Check if this object has been forwarded to to-space
        if let Some(forwarded) = unsafe { self.check_forwarded_atomic(obj) } {
            unsafe {
                let atomic_slot =
                    &*(forwarded.add(field_offset) as *const std::sync::atomic::AtomicU64);
                atomic_slot.store(new_bits, Ordering::Relaxed);
            };
        }
    }

    /// Read barrier: follow forwarding pointer if object has been relocated.
    ///
    /// During concurrent GC, an object's type_info slot may contain a
    /// forwarding pointer (bit 63 set). This resolves an already-published
    /// relocation; it does not protect a later load/store from copying. Use
    /// object_access for the entire operation during concurrent collection.
    ///
    /// Fast path when no GC is active: returns the pointer unchanged
    /// (single atomic load + branch).
    ///
    /// # Safety
    /// - `ptr` must point to a valid heap object or be null.
    #[inline(always)]
    pub unsafe fn read_barrier(&self, ptr: *mut u8) -> *mut u8 {
        if ptr.is_null() {
            return ptr;
        }
        // Fast path: no GC active
        if !self.barriers_active() {
            return ptr;
        }
        // Slow path: check forwarding pointer
        unsafe { crate::gc::barrier::read_barrier_atomic(ptr, self.header.forwarding_offset) }
    }

    // ─── Concurrent Collection ──────────────────────────────────

    /// Run a concurrent collection cycle.
    ///
    /// Protocol:
    /// 1. Short STW pause #1: snapshot roots, enable write barriers
    /// 2. Concurrent copy phase: copy objects while mutators run
    /// 3. Short STW pause #2: drain SATB buffers, process remaining
    ///    gray objects, swap spaces, disable barriers, resume
    ///
    /// # Safety
    /// All objects/roots satisfy `collect`'s layout, liveness and pointer-policy
    /// requirements. Registered mutators may run during copying, but every
    /// managed access must use the complete [`Heap::object_access`] protocol,
    /// with normal atomic/aggregate, reference and card/SATB synchronization.
    /// Mutators poll outside access scopes and reload roots after each pause.
    /// The calling coordinator is unregistered or already BLOCKED. Generated
    /// and runtime access integration remains in progress; an after-store hook
    /// or pointer-resolution read barrier alone does not satisfy this protocol.
    /// Collection failures use the fatal policy documented on [`Heap::collect`].
    pub unsafe fn concurrent_collect<P: PtrPolicy>(&self) {
        {
            let _gc_guard = self.lock_coordinator();
            abort_on_collection_panic(|| unsafe { self.concurrent_collect_locked::<P>() });
        }
        unsafe { self.compact_native_registries(); }
    }

    /// Caller holds gc_lock and contains collection panics before resuming anyone.
    unsafe fn concurrent_collect_locked<P: PtrPolicy>(&self) {
        // GC-log timing (cold path). For a concurrent collection this spans the
        // whole cycle, not just an STW pause; still a useful per-collection cost.
        let gc_t0 = std::time::Instant::now();
        // No mutator can observe Copying until every access stripe has its
        // platform backing. Initialization is once per heap, off access paths.
        self.object_locks.prepare();
        let gc_before = self.from_used() as u64;
        self.clear_destination_cards();

        // ── STW Pause #1: Snapshot roots ─────────────────────────

        self.trace_gc(TraceState::GcWaitingForSafepoints);
        self.satb_queue.begin_cycle::<P>(self.from_space().base(), self.from_space().size());
        self.gc_phase
            .store(GcPhase::Copying as u8, Ordering::Release);

        // Snapshot thread refs and set gc_requested atomically under threads lock.
        let thread_snapshot: ThreadSnapshot<'_> = {
            let threads = self.threads.lock().unwrap();
            self.gc_requested.store(true, Ordering::Release);
            ScopedSnapshot::capture(threads.iter().cloned(), self.native_memory.registry_allocator())
        };

        for state in &thread_snapshot { state.request_poll(); }
        for ts in &thread_snapshot {
            while !ts.is_safely_at_safepoint() {
                std::thread::yield_now();
            }
            self.trace_thread(ts, TraceState::AtSafepoint);
        }

        self.trace_gc(TraceState::GcStw);
        unsafe { self.sample_heap_occupancy(); }
        // Snapshot roots into to-space (short work under STW)
        unsafe { self.scan_registered_roots(&mut |slot| {
            self.process_slot::<P>(slot);
        }); }

        // Nursery objects as roots during concurrent major GC
        if let Some(ns) = &self.nursery_state {
            unsafe {
                ns.nursery.walk_in(&self.type_table, self.native_memory.registry_allocator(), &mut |obj, info| {
                    self.scan_major_object::<P>(obj, info);
                });
            }
        }

        // Resume threads — they now run with write barriers active.
        // Snapshot generations so STW #2 can distinguish stale safepoints.
        self.trace_gc(TraceState::GcResuming);
        let stw1_gens = ScopedSnapshot::capture(
            thread_snapshot.iter().map(|ts| ts.safepoint_gen()),
            self.native_memory.registry_allocator(),
        );
        self.resume_world(&thread_snapshot, None);
        let first_pause = gc_t0.elapsed();

        // ── Concurrent Copy Phase ────────────────────────────────
        self.trace_gc(TraceState::GcConcurrent);

        // Scan canonical to-space objects to discover old-space children.
        // Bounded batches share one canonical owner's access stripe, then
        // release it before claiming/copying children (no nested stripe wait).
        // Mutator accesses resolve old aliases under the same exclusion. Do
        // not rewrite parent pointer slots here: the final stopped-world root
        // and complete to-space passes close over late writes and new objects.
        // SATB logs additionally retain overwritten references until that pass.
        let mut scan_offset = 0;
        while scan_offset < self.to_space().used() {
            let to_obj = unsafe { self.to_space().base().add(scan_offset) };
            let type_id = unsafe { read_type_id(to_obj, self.type_id_offset) };
            let info = &self.type_table[type_id as usize];

            // Read fields from the to-space copy to discover from-space
            // children. For each from-space pointer, copy the target to
            // to-space (installs forwarding pointer) but don't update the
            // parent's field — that's deferred to STW #2's re-scan.
            unsafe {
                self.scan_object_concurrent::<P>(to_obj, info);
            }

            let varlen_len = match info.varlen {
                VarLenKind::None => 0,
                _ => unsafe { read_varlen_count(to_obj, info) },
            };
            let obj_size = info.allocation_size(varlen_len);
            let align = 1usize << info.align_log2;
            scan_offset = (scan_offset + obj_size + align - 1) & !(align - 1);
        }

        // ── STW Pause #2: Final drain + swap ────────────────────

        let second_pause_started = std::time::Instant::now();
        self.trace_gc(TraceState::GcWaitingForSafepoints);
        {
            let _threads = self.threads.lock().unwrap();
            self.gc_requested.store(true, Ordering::Release);
            for state in &thread_snapshot { state.request_poll(); }
        }

        for (ts, stw1_gen) in thread_snapshot.iter().zip(stw1_gens.iter()) {
            // Wait for thread to be at a NEW safepoint (gen > stw1_gen)
            // AND truly blocked (no stale resume pending).
            //
            // Threads in STATE_BLOCKED are accepted unconditionally:
            // they cannot be running mutator code, so by definition no
            // new allocations or root mutations have occurred since
            // STW #1. Re-scanning their (unchanged) roots is sound
            // even though their gen hasn't advanced.
            loop {
                if ts.is_blocked() && ts.is_safely_at_safepoint() {
                    break;
                }
                if ts.safepoint_gen() > *stw1_gen && ts.is_safely_at_safepoint() {
                    break;
                }
                std::thread::yield_now();
            }
            self.trace_thread(ts, TraceState::AtSafepoint);
        }

        self.trace_gc(TraceState::GcStw);
        unsafe { self.sample_heap_occupancy(); }

        // Re-scan all roots. Between STW #1 and now, mutators may have:
        // - Allocated new objects in from-space and stored them in roots
        // - Changed root slots to point to different objects
        // Re-scanning catches all of these.
        unsafe { self.scan_registered_roots(&mut |slot| {
            self.process_slot::<P>(slot);
        }); }

        // Re-scan nursery objects as roots
        if let Some(ns) = &self.nursery_state {
            unsafe {
                ns.nursery.walk_in(&self.type_table, self.native_memory.registry_allocator(), &mut |obj, info| {
                    self.scan_major_object::<P>(obj, info);
                });
            }
        }

        // Drain SATB queue (flushed by mutators) + thread-local SATB buffers
        for bits in self.satb_queue.drain_chunks().into_iter().flatten() {
            if let Some(ptr) = P::try_decode_ptr(bits) {
                if self.from_space().contains(ptr) {
                    unsafe { self.copy_or_forward::<P>(ptr) };
                }
            }
        }

        // Drain per-thread SATB buffers (threads are at safepoints — safe)
        for ts in thread_snapshot.iter() {
            let buf = unsafe { ts.satb_buffer() };
            for bits in buf.drain_values() {
                if let Some(ptr) = P::try_decode_ptr(bits) {
                    if self.from_space().contains(ptr) {
                        unsafe { self.copy_or_forward::<P>(ptr) };
                    }
                }
            }
        }

        // Full re-scan of to-space. During the concurrent phase, mutators
        // may have stored from-space pointers into to-space objects' fields
        // (e.g., allocating a new object and storing it in a field of an
        // already-copied object). The SATB barrier only logs old values,
        // so these new from-space pointers are not captured. Re-scanning
        // from offset 0 catches all such pointers. process_slot is
        // idempotent — already-correct to-space pointers are skipped.
        scan_offset = 0;
        while scan_offset < self.to_space().used() {
            let obj = unsafe { self.to_space().base().add(scan_offset) };
            let type_id = unsafe { read_type_id(obj, self.type_id_offset) };
            let info = &self.type_table[type_id as usize];

            unsafe {
                self.scan_major_object::<P>(obj, info);
            }

            let varlen_len = match info.varlen {
                VarLenKind::None => 0,
                _ => unsafe { read_varlen_count(obj, info) },
            };
            let obj_size = info.allocation_size(varlen_len);
            let align = 1usize << info.align_log2;
            scan_offset = (scan_offset + obj_size + align - 1) & !(align - 1);
        }

        // Conditional queues are processed only while stopped: slot storage
        // may be retired by a running close/receive. Source-owner discovery
        // activates further queues through the parallel copy worklist.
        let conditional_channels = self.channels.lock().unwrap().has_owned();
        if conditional_channels {
            unsafe { self.parallel_major::<P>(&[]); }
        }

        // Swap spaces
        unsafe { self.sweep_native_owners(false); }
        self.swap_spaces();
        unsafe { self.to_space().reset() };
        self.collections.fetch_add(1, Ordering::Relaxed);
        let gc_after = self.from_used() as u64;

        // The new from-space remembered set was rebuilt during Cheney scanning.
        // Keep its old-to-young edges until the next minor collection.

        // Debug: verify all root slots point to from-space after swap
        if cfg!(debug_assertions) && self.gc_every_alloc() {
            for ts in thread_snapshot.iter() {
                unsafe { ts.scan_roots(&mut |slot| {
                    let bits = *slot;
                    if let Some(ptr) = P::try_decode_ptr(bits) {
                        assert!(
                            self.from_space().contains(ptr),
                            "CONCURRENT POST-SWAP: root slot {:p} -> {:p} NOT in from [{:p}..+{}). \
                             In to: {}. collections={}",
                            slot,
                            ptr,
                            self.from_space().base(),
                            self.from_space().size(),
                            self.to_space().contains(ptr),
                            self.collections.load(Ordering::Relaxed),
                        );
                    }
                }); }
            }
        }

        // Disable write barriers and resume threads
        self.trace_gc(TraceState::GcResuming);
        self.satb_queue.finish_cycle();
        self.gc_phase.store(GcPhase::Idle as u8, Ordering::Release);
        self.resume_world(&thread_snapshot, None);
        self.record_gc_timings(
            GcKind::Concurrent, first_pause + second_pause_started.elapsed(),
            gc_t0.elapsed(), gc_before, gc_after, 0,
        );
        for ts in &thread_snapshot {
            self.trace_thread(ts, TraceState::Running);
        }
        self.trace_gc(TraceState::GcIdle);
    }

    /// Copy `size` bytes word-by-word using Relaxed atomic loads/stores.
    /// Guarantees each 64-bit field is non-torn even if a mutator is
    /// concurrently writing to `src`.
    unsafe fn atomic_copy_words(src: *const u8, dst: *mut u8, size: usize) {
        use std::sync::atomic::AtomicU64;
        debug_assert!(size % 8 == 0);
        let words = size / 8;
        for i in 0..words {
            unsafe {
                let s = &*((src as *const AtomicU64).add(i));
                let d = &*((dst as *const AtomicU64).add(i));
                d.store(s.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }
    }

    // ─── Internal GC machinery ──────────────────────────────────

    unsafe fn process_slot<P: PtrPolicy>(&self, slot: *mut u64) {
        // The world is stopped. The safepoint release/acquire handshake
        // orders prior mutator writes; relaxed atomic access also supports
        // slots published atomically by the runtime. Non-atomic stack-root
        // writes are safe here because their owner is quiescent.
        let bits = unsafe {
            let atomic = &*(slot as *const std::sync::atomic::AtomicU64);
            atomic.load(Ordering::Relaxed)
        };
        if let Some(ptr) = P::try_decode_ptr(bits) {
            if self.is_nursery(ptr) {
                return;
            }
            if self.from_space().contains(ptr) {
                let new_ptr = unsafe { self.copy_or_forward::<P>(ptr) };
                unsafe {
                    let atomic = &*(slot as *const std::sync::atomic::AtomicU64);
                    atomic.store(P::encode_ptr(new_ptr), Ordering::Relaxed);
                };
            }
        }
    }

    /// Start the next major's remembered set on the inactive tenured space.
    /// The active space's cards are still available to any subsequent minor GC.
    fn clear_destination_cards(&self) {
        if let Some(ns) = &self.nursery_state {
            let destination = 1 - self.from_idx.load(Ordering::Acquire);
            ns.card_tables[destination].clear_all();
        }
    }

    /// Forward references and rebuild old-to-young cards in the same object
    /// scan. Major GC moves tenured objects but leaves the nursery in place.
    /// Clearing all cards after that move would lose their nursery edges.
    /// Mark the OBJECT START card, as mutator barriers do, so objects spanning
    /// several cards are scanned from their true start by the next minor.
    ///
    /// # Safety
    /// The world is stopped; obj is fully initialized and described by info.
    unsafe fn scan_major_object<P: PtrPolicy>(&self, obj: *mut u8, info: &TypeInfo) {
        let track_nursery_edges = self.nursery_state.is_some() && self.to_space().contains(obj);
        let mut has_nursery_edge = false;
        unsafe {
            scan_object(obj, info, |slot| {
                self.process_slot::<P>(slot);
                if track_nursery_edges {
                    if let Some(ptr) = P::try_decode_ptr(slot.read()) {
                        has_nursery_edge |= self.is_nursery(ptr);
                    }
                }
            });
        }
        if has_nursery_edge {
            let ns = self.nursery_state.as_ref().unwrap();
            let destination = 1 - self.from_idx.load(Ordering::Acquire);
            ns.card_tables[destination].mark_dirty(obj);
        }
    }

    /// Discover children from bounded snapshots of canonical pointer slots.
    /// Only immutable object geometry is read without the owner's stripe.
    /// No owner guard survives a child copy or pointer-policy callback.
    unsafe fn scan_object_concurrent<P: PtrPolicy>(&self, owner: *mut u8, info: &TypeInfo) {
        let mut slots = [core::ptr::null_mut(); CONCURRENT_SCAN_SLOTS];
        let mut used = 0;
        unsafe {
            scan_object(owner, info, |slot| {
                slots[used] = slot;
                used += 1;
                if used == slots.len() {
                    self.process_slots_concurrent::<P>(owner, &slots);
                    used = 0;
                }
            });
            self.process_slots_concurrent::<P>(owner, &slots[..used]);
        }
    }

    /// Snapshot at most one bounded batch under canonical owner exclusion,
    /// then release it before decoding/copying children. Parent rewrites remain
    /// deferred to the final stopped-world pass; SATB retains overwritten edges.
    unsafe fn process_slots_concurrent<P: PtrPolicy>(&self, owner: *mut u8, slots: &[*mut u64]) {
        if slots.is_empty() {
            return;
        }
        assert!(slots.len() <= CONCURRENT_SCAN_SLOTS);
        let mut snapshot = [0u64; CONCURRENT_SCAN_SLOTS];
        {
            let _access = self.object_locks.one(owner);
            for (destination, &slot) in snapshot.iter_mut().zip(slots) {
                *destination =
                    unsafe { (&*slot.cast::<std::sync::atomic::AtomicU64>()).load(Ordering::Relaxed) };
            }
        }
        for &bits in &snapshot[..slots.len()] {
            if let Some(ptr) = P::try_decode_ptr(bits) {
                if !self.is_nursery(ptr) && self.from_space().contains(ptr) {
                    unsafe {
                        self.copy_or_forward_atomic::<P>(ptr);
                    }
                }
            }
        }
    }

    unsafe fn check_forwarded(&self, old: *mut u8) -> Option<*mut u8> {
        let slot = unsafe { old.add(self.header.forwarding_offset) as *const u64 };
        let word = unsafe { *slot };
        if word & FORWARDING_BIT != 0 {
            Some((word & !FORWARDING_BIT) as *mut u8)
        } else {
            None
        }
    }

    /// Atomic check for forwarding pointer (for concurrent use).
    unsafe fn check_forwarded_atomic(&self, old: *mut u8) -> Option<*mut u8> {
        let resolved = unsafe { crate::gc::barrier::read_barrier_atomic(old, self.header.forwarding_offset) };
        (resolved != old).then_some(resolved)
    }

    unsafe fn install_forwarding(&self, old: *mut u8, new: *mut u8) {
        let slot = unsafe { old.add(self.header.forwarding_offset) as *mut u64 };
        unsafe { *slot = (new as u64) | FORWARDING_BIT };
    }

    unsafe fn copy_or_forward<P: PtrPolicy>(&self, old: *mut u8) -> *mut u8 {
        if let Some(forwarded) = unsafe { self.check_forwarded(old) } {
            return forwarded;
        }

        let type_id = unsafe { read_type_id(old, self.type_id_offset) };
        // PRECISE-LAYOUT INVARIANT (no conservative pointer identification):
        // the monomorphizing front end places every scalar in the untraced raw
        // region and zeroes unused enum pointer-slots, so a TRACED slot only
        // ever holds a pointer-or-null. A slot pointing into from-space
        // therefore ALWAYS targets a real object with an in-range type_id. An
        // out-of-range type_id means a non-pointer (scalar) leaked into a
        // traced slot — a layout/rooting bug. We must NOT silently `return old`
        // (that is conservative-GC pointer identification: unsound in a moving
        // collector — a false-positive int whose bits look like a valid header
        // would still be relocated — and it masks the real bug). Under
        // debug/stress (or release with GCR_GC_VERIFY=1) we panic loudly as a
        // DETECTOR; release-default trusts the layout and the bounds index
        // below faults loudly if the invariant is ever broken. Never silent.
        if gc_verify_armed() {
            assert!(
                (type_id as usize) < self.type_table.len(),
                "GC precise-layout violation: traced slot points at {old:p} whose \
                 header type_id={type_id} is out of range (type_table len {}). A \
                 non-pointer reached a traced slot — fix the layout/rooting; do not \
                 mask it.",
                self.type_table.len(),
            );
        }
        let info = &self.type_table[type_id as usize];
        let varlen_len = match info.varlen {
            VarLenKind::None => 0,
            _ => unsafe { read_varlen_count(old, info) },
        };
        let size = info.allocation_size(varlen_len);

        let new = self.to_space().alloc(info, varlen_len);
        assert!(!new.is_null(), "to-space exhausted during collection");

        unsafe {
            core::ptr::copy_nonoverlapping(old, new, size);
        }

        unsafe { self.install_forwarding(old, new) };

        new
    }

    /// Concurrent copy with exclusive ownership and ordered access exclusion.
    unsafe fn copy_or_forward_atomic<P: PtrPolicy>(&self, old: *mut u8) -> *mut u8 {
        use std::sync::atomic::AtomicU64;
        #[cfg(test)]
        if let Some(observer) = &self.concurrent_copy_observer {
            observer(concurrent_access::CopyStage::BeforeClaim, old, core::ptr::null_mut());
        }
        let word = unsafe { &*old.add(self.header.forwarding_offset).cast::<AtomicU64>() };
        // Forwarding is immutable until the stopped-world swap. Acquire pairs
        // with publication of the fully initialized destination. Marker-only
        // claims still use the locked/retrying path below.
        let published = word.load(Ordering::Acquire);
        if published & FORWARDING_BIT != 0 && published != FORWARDING_BIT {
            return (published & !FORWARDING_BIT) as *mut u8;
        }
        let mut claim = loop {
            let locked = self.object_locks.one(old);
            let original = word.load(Ordering::Acquire);
            if original == FORWARDING_BIT {
                drop(locked);
                std::thread::yield_now();
                continue;
            }
            if original & FORWARDING_BIT != 0 {
                return (original & !FORWARDING_BIT) as *mut u8;
            }
            word.store(FORWARDING_BIT, Ordering::Release);
            break CopyClaim { word, original, published: false };
        };
        // The marker excludes other copiers and makes object-access readers
        // retry. No stripe is held while reserving the destination or while
        // waiting to acquire the pair in its global order.
        let type_id = unsafe { self.header.type_id_from_word(old, claim.original) };
        assert!((type_id as usize) < self.type_table.len(),
            "GC precise-layout violation (concurrent): header type_id={type_id} is out of range (type_table len {})", self.type_table.len());
        let info = &self.type_table[type_id as usize];
        let len = match info.varlen { VarLenKind::None => 0, _ => unsafe { read_varlen_count(old, info) } };
        let size = info.allocation_size(len);
        let new = self.to_space().alloc(info, len);
        assert!(!new.is_null(), "to-space exhausted during collection");
        let _access = self.object_locks.pair(old, new);
        unsafe {
            Self::atomic_copy_words(old, new, size);
            new.add(self.header.forwarding_offset).cast::<u64>().write(claim.original);
        }
        #[cfg(test)]
        if let Some(observer) = &self.concurrent_copy_observer { observer(concurrent_access::CopyStage::BeforePublication, old, new); }
        word.store(new as u64 | FORWARDING_BIT, Ordering::Release);
        claim.published = true;
        new
    }

    // ─── Minor GC (Generational) ────────────────────────────────

    /// Accounted native backing/reservations, including FFI ownership records
    /// and queue buffers held by transient owners after registry removal.
    /// Excludes other native metadata, allocator overhead and RSS.
    pub fn native_buffer_bytes(&self) -> usize {
        self.native_memory.bytes()
    }
    pub fn native_buffer_peak_bytes(&self) -> usize {
        self.native_memory.peak()
    }
    /// Requested backing bytes in native registries, including staged/retired
    /// buffers and hash-table control storage. Excludes record/control Arc
    /// allocations, other runtime metadata, allocator overhead and RSS.
    pub fn native_registry_bytes(&self) -> usize { self.native_memory.registry.bytes() }
    pub fn native_registry_peak_bytes(&self) -> usize { self.native_memory.registry.peak() }

    pub fn native_pressure_collections(&self) -> usize {
        self.native_memory.collections()
    }

    /// Service accounted native growth before allocation. Caller is the RUNNING
    /// owner with all roots published, and reloads pointers after this call.
    pub(crate) unsafe fn mutator_gc_for_native_allocations<P: PtrPolicy>(
        &self,
        thread: &ThreadState,
        additional: usize,
    ) {
        let memory = &self.native_memory;
        if !memory.needs_collection(additional) {
            return;
        }
        // Pressure service is separate from collector exclusion: all waiting
        // registrations remain BLOCKED through native drain and threshold
        // adaptation. Serializing these decisions prevents an older request
        // from overwriting a newer request's growth policy after its wait.
        let mut callers = self.suspend_current_registrations();
        let _pressure = abort_on_collection_panic(|| memory.pressure_service.lock().unwrap());
        abort_on_collection_panic(|| self.native_retirement.prepare_pressure());
        abort_on_collection_panic(|| self.native_retirement.drain());
        let guard = self.lock_coordinator();
        if !memory.needs_collection(additional) {
            abort_on_collection_panic(|| callers.restore());
            return;
        }
        abort_on_collection_panic(|| unsafe {
            if self.has_nursery() {
                self.mutator_minor_locked::<P>(thread, true);
            } else {
                self.mutator_major_locked::<P>(thread);
            }
        });
        drop(guard);
        unsafe { self.compact_native_registries(); }
        // Dead backing may only now have reached the native mailbox. Do not
        // treat that pending garbage as future retained live demand. The world
        // and coordinator remain available while this caller waits for release.
        abort_on_collection_panic(|| self.native_retirement.drain());
        loop {
            let guard = self.lock_coordinator();
            let idle = abort_on_collection_panic(|| self.native_retirement.try_idle(|| {
                memory.collected(additional);
            }));
            if idle {
                abort_on_collection_panic(|| callers.restore());
                break;
            }
            // Another collector/old observer published after the previous
            // drain. Wait again without pinning collector exclusion.
            drop(guard);
            abort_on_collection_panic(|| self.native_retirement.drain());
        }
    }

    /// Run a minor (nursery) collection triggered by a mutator thread.
    ///
    /// All live nursery objects are promoted to tenured from-space.
    /// The nursery is then reset.
    ///
    /// # Safety
    /// - All objects in the nursery and tenured spaces must have valid headers.
    /// - Caller owns the registered triggering state. All same-thread frame
    ///   chains stay published and stable, with derived pointers reloaded after.
    /// Collection failures use the fatal policy documented on [`Heap::collect`].
    pub unsafe fn mutator_triggered_minor_gc<P: PtrPolicy>(&self, triggering_thread: &ThreadState) {
        let ns = match &self.nursery_state {
            Some(ns) => ns,
            None => return,
        };

        let (gc_guard, already_collected) = self.lock_mutator_collection(GcKind::Minor);
        // Under gc_lock: an empty nursery or competing cycle services a
        // non-stress request. Native backing maintenance still runs afterwards.
        if self.gc_every_alloc() || (!already_collected && ns.nursery.used() != 0) {
            abort_on_collection_panic(|| unsafe {
                self.mutator_minor_locked::<P>(triggering_thread, false);
            });
        }
        gc_guard.finish();
    }

    /// Caller holds the mutator collection guard outside the panic boundary.
    unsafe fn mutator_minor_locked<P: PtrPolicy>(
        &self, triggering_thread: &ThreadState, force_major: bool,
    ) {
        let ns = self.nursery_state.as_ref().unwrap();
        // Snapshot threads and set gc_requested
        let trigger_ptr = triggering_thread as *const ThreadState as usize;
        let thread_snapshot: ThreadSnapshot<'_> = {
            let threads = self.threads.lock().unwrap();
            self.gc_requested.store(true, Ordering::Release);
            ScopedSnapshot::capture(threads.iter().cloned(), self.native_memory.registry_allocator())
        };

        // Request every worker before waiting, including allocation-free loops.
        for ts in thread_snapshot.iter() {
            if ThreadStateHandle::as_ptr(ts) as usize != trigger_ptr {
                ts.request_poll();
            }
        }
        // Wait for all threads EXCEPT ourselves to reach safepoints
        for ts in thread_snapshot.iter() {
            if ThreadStateHandle::as_ptr(ts) as usize == trigger_ptr {
                continue;
            }
            while !ts.is_safely_at_safepoint() {
                std::thread::yield_now();
            }
        }

        let gc_t0 = std::time::Instant::now();
        let mut major_elapsed = std::time::Duration::ZERO;
        unsafe { self.sample_heap_occupancy(); }
        // Revalidate promotion capacity only AFTER all mutators have stopped:
        // nursery occupancy can grow while we request their safepoints. If a
        // major is needed, keep this same pause and census through promotion.
        // Resuming/reparking between major and minor adds work and races the
        // capacity estimate against new allocations.
        if force_major || (ns.nursery.used() > self.from_space().remaining()
            && unsafe { self.nursery_survivors_exceed::<P>(self.from_space().remaining()) })
        {
            // Whole-graph marking prevents unreachable nursery wrappers from
            // retaining old garbage and defeating the required reclamation.
            let live = unsafe { self.reachable_nursery_objects::<P>() };
            let major_started = std::time::Instant::now();
            unsafe { self.collect_inner_with_nursery_roots::<P>(&[], Some(&live)) };
            major_elapsed = major_started.elapsed();
        }

        // Charge preflight/marking to the minor, excluding the separately
        // recorded major. All work remains inside this one world pause.
        let gc_before_nursery = ns.nursery.used() as u64;
        let gc_before_tenured = self.from_used() as u64;

        unsafe { self.parallel_minor::<P>(); self.sweep_native_owners(true); }
        let from_idx = self.from_idx.load(Ordering::Acquire);

        // Phase 4: Reset nursery and clear card table
        unsafe { ns.nursery.reset() };
        ns.card_tables[from_idx].clear_all();
        ns.minor_collections.fetch_add(1, Ordering::Relaxed);
        // GC-log: nursery is reset (after ≈ 0); tenured growth = promoted volume.
        let promoted = (self.from_used() as u64).saturating_sub(gc_before_tenured);
        self.record_gc_duration(
            GcKind::Minor,
            gc_t0.elapsed().saturating_sub(major_elapsed),
            gc_before_nursery,
            ns.nursery.used() as u64,
            promoted,
        );

        // Resume threads
        self.resume_world(&thread_snapshot, Some(trigger_ptr));

    }

    /// Complete root closure across both generations, while stopped under
    /// gc_lock. Return young members to seed a major immediately followed by
    /// nursery evacuation in this same pause.
    unsafe fn reachable_nursery_objects<P: PtrPolicy>(&self) -> NurseryLive<'_> {
        unsafe fn enqueue<P: PtrPolicy>(
            heap: &Heap, slot: *mut u64,
            seen: &mut NurseryLive<'_>, pending: &mut PromotionPending<'_>,
        ) {
            let bits = unsafe { (&*slot.cast::<std::sync::atomic::AtomicU64>()).load(Ordering::Relaxed) };
            if let Some(object) = P::try_decode_ptr(bits) {
                if (heap.from_space().contains(object) || heap.is_nursery(object))
                    && seen.insert(object as usize)
                {
                    pending.push(object);
                }
            }
        }
        let edges = self.channel_edges();
        let mut seen = NurseryLive::with_hasher_in(
            std::hash::RandomState::new(), self.native_memory.registry_allocator(),
        );
        let mut pending = PromotionPending::new_in(self.native_memory.registry_allocator());
        unsafe {
            self.scan_registered_roots(&mut |slot| enqueue::<P>(self, slot, &mut seen, &mut pending));
        }
        while let Some(object) = pending.pop() {
            let info = self.type_info_by_id(unsafe { self.obj_type_id(object) });
            unsafe {
                let mut visit = |slot| enqueue::<P>(self, slot, &mut seen, &mut pending);
                scan_object(object, info, &mut visit);
                edges.trace(object, &mut visit);
            }
        }
        seen.retain(|address| self.is_nursery(*address as *const u8));
        seen
    }

    /// Read-only promotion preflight, only after the occupancy upper bound
    /// exceeds available old space. All mutators are stopped and gc_lock is
    /// held. Minor collection retains every tenured object, so its nursery
    /// edges must be included even when that old owner is unreachable.
    unsafe fn nursery_survivors_exceed<P: PtrPolicy>(&self, limit: usize) -> bool {
        let edges = self.channel_edges();
        let mut seen = NurseryLive::with_hasher_in(
            std::hash::RandomState::new(), self.native_memory.registry_allocator(),
        );
        let mut pending = PromotionPending::new_in(self.native_memory.registry_allocator());
        {
            let mut seed = |slot: *mut u64| {
                let bits = unsafe { (&*slot.cast::<std::sync::atomic::AtomicU64>()).load(Ordering::Relaxed) };
                if let Some(object) = P::try_decode_ptr(bits) {
                    if self.is_nursery(object) && seen.insert(object as usize) {
                        pending.push(object);
                    }
                }
            };
            unsafe {
                self.scan_registered_roots(&mut seed);
                edges.scan_old_roots(self, &mut seed);
                // The same remembered objects seed evacuation. Scanning all old
                // objects here makes every near-capacity minor proportional to
                // the accumulated tenured heap, even when no cards are dirty.
                self.scan_minor_remembered_roots(&mut seed);
            }
        }
        let mut remaining = limit;
        while let Some(object) = pending.pop() {
            let info = self.type_info_by_id(unsafe { self.obj_type_id(object) });
            let length = match info.varlen {
                VarLenKind::None => 0,
                _ => unsafe { read_varlen_count(object, info) },
            };
            let size = info.allocation_size(length);
            if size > remaining { return true; }
            remaining -= size;
            unsafe {
                let mut visit = |slot: *mut u64| {
                    let bits = (&*slot.cast::<std::sync::atomic::AtomicU64>()).load(Ordering::Relaxed);
                    if let Some(child) = P::try_decode_ptr(bits) {
                        if self.is_nursery(child) && seen.insert(child as usize) {
                            pending.push(child);
                        }
                    }
                };
                scan_object(object, info, &mut visit);
                edges.trace(object, &mut visit);
            }
        }
        false
    }


}

/// Guard returned by [`Heap::pause_world`]. While alive, every other
/// registered mutator thread is parked at a safepoint and `gc_lock` is
/// held (so no collection runs). On drop it lowers each parked thread's
/// poll flag, clears the GC request, and resumes them.
pub struct WorldPause<'a> {
    heap: &'a Heap,
    parked: ThreadSnapshot<'a>,
    /// Held for the guard's lifetime to exclude concurrent collection.
    _gc_guard: CoordinatorGuard<'a>,
    callers: PauseCallers<'a>,
}

/// A validated layout capability bound to its heap. Private fields prevent
/// bypassing descriptor validation; arena identity is resolved for every use.
pub(crate) struct AllocationRequest<'h> {
    heap: &'h Heap,
    info: &'h TypeInfo,
    len: usize,
    size: usize,
}
impl AllocationRequest<'_> {
    /// # Safety
    /// Owning RUNNING mutator registered with this heap; live references stay
    /// rooted and the returned object is initialized before publication/GC.
    #[inline]
    pub(crate) unsafe fn allocate(&self, thread: &ThreadState) -> *mut u8 {
        let space = self.heap.nursery_state.as_ref().map_or_else(|| self.heap.from_space(), |ns| &ns.nursery);
        if self.size > 8192 || self.heap.gc_every_alloc() { return space.alloc(self.info, self.len); }
        unsafe { thread.alloc_local_sized(space, self.size) }
    }
}

pub(crate) struct ObjectRequest<'h, H: ObjHeader> {
    allocation: AllocationRequest<'h>,
    _header: std::marker::PhantomData<H>,
}
impl<H: ObjHeader> ObjectRequest<'_, H> {
    /// Try only the bounded local-buffer path; never collect, publish a frame,
    /// or allocate directly in tenured storage. Runtime callers publish roots
    /// before taking a slow path after null. Stress always takes that path.
    /// # Safety
    /// As allocate: owning RUNNING mutator with this heap's live registration.
    #[inline]
    pub(crate) unsafe fn try_allocate_local(&self, thread: &ThreadState) -> *mut u8 {
        if self.allocation.heap.gc_every_alloc() || self.allocation.size > 8192 {
            return core::ptr::null_mut();
        }
        unsafe { self.allocate(thread) }
    }

    /// # Safety
    /// As AllocationRequest::allocate. H was matched to the heap by construction.
    #[inline]
    pub(crate) unsafe fn allocate(&self, thread: &ThreadState) -> *mut u8 {
        let ptr = unsafe { self.allocation.allocate(thread) };
        if !ptr.is_null() { unsafe {
            crate::gc::field::init_header::<H>(ptr, self.allocation.info.type_id);
            if self.allocation.info.varlen != VarLenKind::None {
                crate::gc::field::write_varlen_count(ptr, self.allocation.info, self.allocation.len);
            }
        } }
        ptr
    }
}

struct MutatorGcLock<'a> {
    _gc_guard: Option<CoordinatorGuard<'a>>,
    callers: PauseCallers<'a>,
}
impl MutatorGcLock<'_> {
    fn finish(mut self) {
        let heap = self.callers.heap;
        if heap.native_registry_compaction.load(Ordering::Acquire) {
            // Keep callers BLOCKED through native work. Other collectors can
            // run, but no redundant RUNNING/parking transition is necessary.
            drop(self._gc_guard.take());
            unsafe { heap.compact_native_registries(); }
        }
    }
}
impl Drop for MutatorGcLock<'_> {
    fn drop(&mut self) {
        abort_on_collection_panic(|| {
            if self._gc_guard.is_none() {
                self._gc_guard = Some(self.callers.heap.lock_coordinator());
            }
            self.callers.restore();
        });
    }
}

pub(crate) struct OwnedPauseCallers {
    heap: Arc<Heap>,
    head: Option<ThreadStateHandle>,
}
impl Drop for OwnedPauseCallers {
    fn drop(&mut self) {
        // Reuse the same whole-census restoration and fatal failure policy.
        drop(PauseCallers { heap: &self.heap, head: self.head.take() });
    }
}

pub(super) struct PauseCallers<'a> {
    heap: &'a Heap,
    head: Option<ThreadStateHandle>,
}
impl PauseCallers<'_> {
    /// Caller holds the census lock and has verified the relevant collection
    /// window is closed. Restore the complete group before releasing the lock.
    fn restore_after_pause(&mut self) {
        while let Some(state) = self.head.take() {
            self.head = unsafe { state.take_collection_caller() };
            unsafe { state.resume_blocked_after_pause(); }
            #[cfg(test)]
            if self.head.is_some() { pause_callers_tests::restoration_step(); }
        }
    }

    /// The coordinator must exclude collection until the complete group is
    /// RUNNING again; an intermediate RUNNING sibling cannot poll while this
    /// OS thread is still restoring another registration.
    fn restore(&mut self) {
        while let Some(thread) = self.head.take() {
            // Detach before restoring so a later or nested guard can reuse the
            // link, and chain destruction never recurses on the native stack.
            self.head = unsafe { thread.take_collection_caller() };
            unsafe { thread.exit_blocked(self.heap); }
            #[cfg(test)]
            if self.head.is_some() { pause_callers_tests::restoration_step(); }
        }
    }
}
impl Drop for PauseCallers<'_> {
    fn drop(&mut self) {
        if self.head.is_none() { return; }
        // Native work can be nested under an existing coordinator owner.
        // Exclude new pauses with one census transition instead of recursively
        // acquiring that non-reentrant mutex. All members stay BLOCKED until
        // both moving-pause and concurrent-copy windows have closed.
        let heap = self.heap;
        abort_on_collection_panic(|| {
            while !heap.transition_outside_gc(|| self.restore_after_pause()) {
                std::thread::yield_now();
            }
        });
    }
}

impl Drop for WorldPause<'_> {
    fn drop(&mut self) {
        // User inspection may unwind, but a failed restoration cannot safely
        // release the coordinator lock and let another operation proceed.
        abort_on_collection_panic(|| {
            self.heap.resume_world(&self.parked, None);
            // Restore all current registrations before releasing gc_lock.
            self.callers.restore();
        });
    }
}

#[path = "parallel.rs"]
mod parallel;

#[cfg(test)]
mod scoped_extra_tests {
    use super::*;
    use crate::gc::IdentityPtrPolicy;

    struct ObservedRoots {
        slots: AtomicRootSet,
        visits: AtomicUsize,
    }
    unsafe impl RootSource for ObservedRoots {
        unsafe fn scan_roots(&self, visit: &mut dyn FnMut(*mut u64)) {
            self.visits.fetch_add(1, Ordering::Relaxed);
            unsafe { self.slots.scan_roots(visit); }
        }
    }

    #[test]
    fn collector_scoped_registry_reference_publication_and_removal_allocate_nothing() {
        let info = TypeInfo::for_header(crate::gc::Full::SIZE);
        let heap = Heap::new::<crate::gc::Full>(8192, vec![info.clone()]);
        let object = unsafe { heap.alloc_obj::<crate::gc::Full>(&info, 0) };
        let roots = AtomicRootSet::new();
        unsafe { roots.add(object as u64); }
        let sources: &[&dyn RootSource] = &[&roots];
        let node = std::pin::pin!(ScopedExtraSource::new(sources));
        // Warm the source itself; the registry is prepared by Heap construction.
        unsafe { roots.scan_roots(&mut |_| {}); }
        crate::runtime::allocation_probe::start();
        for _ in 0..1024 {
            let guard = unsafe { heap.register_scoped_extras(node.as_ref()).unwrap() };
            let mut visits = 0;
            unsafe { heap.scan_scoped_roots(&mut |_| visits += 1); }
            assert_eq!(visits, 1);
            drop(guard);
        }
        assert_eq!(crate::runtime::allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert!(heap.scoped_extras.is_empty());
    }

    #[test]
    fn collector_scoped_extras_survive_competing_collections_and_retire() {
        const HELPER: &str = "GCR_SCOPED_EXTRA_HELPER";
        if let Ok(mode) = std::env::var(HELPER) {
            let stress = mode.ends_with("stress");
            let minor = mode.starts_with("minor");
            let info = TypeInfo::for_header(crate::gc::Full::SIZE).with_type_id(0).with_raw_bytes(8);
            let heap = Arc::new(Heap::new_generational::<crate::gc::Full>(1 << 16, 1 << 16, vec![info.clone()]));
            heap.set_gc_every_alloc(stress);
            let (owner, _) = heap.register_thread();
            let object = unsafe {
                if minor { heap.alloc_nursery_obj::<crate::gc::Full>(&info, 0) }
                else { heap.alloc_obj::<crate::gc::Full>(&info, 0) }
            };
            assert!(!object.is_null());
            unsafe { object.add(16).cast::<u64>().write(42); }
            let roots = ObservedRoots { slots: AtomicRootSet::new(), visits: AtomicUsize::new(0) };
            unsafe { roots.slots.add(object as u64); }
            let other = if !mode.starts_with("own") {
                let other_heap = heap.clone();
                let pause = mode.starts_with("pause");
                let concurrent = mode.starts_with("concurrent");
                let original = object as usize;
                let thread = std::thread::spawn(move || {
                    if pause {
                        let _pause = other_heap.pause_world();
                        let mut found = false;
                        unsafe { other_heap.visit_roots(&mut |ptr| { found |= ptr as usize == original; }); }
                        assert!(found, "world pause missed scoped extra roots");
                    } else if minor {
                        let (worker, _) = other_heap.register_thread();
                        unsafe {
                            other_heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&worker);
                            other_heap.safe_deregister_thread(&worker);
                        }
                    } else if concurrent { unsafe { other_heap.concurrent_collect::<IdentityPtrPolicy>(); } }
                    else { unsafe { other_heap.stw_collect::<IdentityPtrPolicy>(); } }
                });
                while !heap.gc_requested() { std::thread::yield_now(); }
                Some(thread)
            } else { None };
            unsafe { heap.mutator_triggered_gc_with_extras::<IdentityPtrPolicy>(&owner, &[&roots]); }
            if let Some(other) = other { other.join().unwrap(); }
            assert!(!owner.is_blocked());
            let moved = roots.slots.get(0) as *const u8;
            assert!(heap.from_space().contains(moved), "temporary-only root was not relocated");
            assert_eq!(heap.from_used(), info.allocation_size(0), "temporary-only object was reclaimed");
            assert_eq!(unsafe { moved.add(16).cast::<u64>().read() }, 42);
            assert!(heap.scoped_extras.is_empty());
            let visits = roots.visits.load(Ordering::Relaxed);
            assert!(visits > 0);
            let retained = unsafe { heap.globals.add(moved as u64) };
            unsafe { heap.mutator_triggered_gc::<IdentityPtrPolicy>(&owner); }
            assert_eq!(roots.visits.load(Ordering::Relaxed), visits, "expired scoped source was visited");
            drop(roots);
            unsafe { heap.mutator_triggered_gc::<IdentityPtrPolicy>(&owner); }
            let live = heap.globals.get(retained) as *const u8;
            assert_eq!(unsafe { live.add(16).cast::<u64>().read() }, 42);
            unsafe { heap.safe_deregister_thread(&owner); }
            return;
        }
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        for mode in ["own", "major", "minor", "pause", "concurrent", "own-stress", "major-stress", "minor-stress", "pause-stress", "concurrent-stress"] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "gc::heap::scoped_extra_tests::collector_scoped_extras_survive_competing_collections_and_retire"])
                .env(HELPER, mode).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!("{mode}: scoped-root coordination hung: {output:?}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{mode}: {output:?}");
        }
    }
}

#[cfg(test)]
mod concurrent_coordination_tests {
    use super::*;
    use crate::gc::{Full, IdentityPtrPolicy};
    use std::sync::atomic::AtomicU64;

    #[test]
    fn collector_concurrent_preserves_permanent_and_jit_roots_in_both_pauses() {
        #[repr(C)]
        struct Frame { root: AtomicU64, visits: AtomicUsize }
        unsafe fn walk(fp: *const u8, visit: &mut dyn FnMut(*mut u64)) {
            let frame = unsafe { &*fp.cast::<Frame>() };
            frame.visits.fetch_add(1, Ordering::Relaxed);
            visit(frame.root.as_ptr());
        }
        let permanent = Box::new(AtomicRootSet::new());
        let frame = Box::new(Frame { root: AtomicU64::new(0), visits: AtomicUsize::new(0) });
        let info = TypeInfo::for_header(Full::SIZE).with_type_id(0).with_raw_bytes(8);
        let heap = Heap::new::<Full>(1 << 16, vec![info.clone()]);
        let (state, _) = heap.register_thread();
        for value in [11u64, 22] {
            let object = unsafe { heap.alloc_obj::<Full>(&info, 0) };
            assert!(!object.is_null());
            unsafe { object.add(16).cast::<u64>().write(value); }
            if value == 11 { unsafe { permanent.add(object as u64); } }
            else { frame.root.store(object as u64, Ordering::Relaxed); }
        }
        unsafe {
            heap.register_permanent_extra(&*permanent);
            heap.set_jit_frame_walker(walk);
            state.set_parked_jit_fp((&*frame as *const Frame).cast());
            state.enter_blocked();
        }
        std::thread::scope(|scope| {
            scope.spawn(|| unsafe { heap.concurrent_collect::<IdentityPtrPolicy>(); }).join().unwrap();
        });
        unsafe { state.exit_blocked(&heap); }
        assert_eq!(heap.from_used(), 2 * info.allocation_size(0));
        assert_eq!(frame.visits.load(Ordering::Relaxed), 2);
        for (bits, value) in [(permanent.get(0), 11), (frame.root.load(Ordering::Relaxed), 22)] {
            let object = bits as *const u8;
            assert!(heap.from_space().contains(object));
            assert_eq!(unsafe { object.add(16).cast::<u64>().read() }, value);
        }
        unsafe { state.clear_parked_jit_fp(); heap.safe_deregister_thread(&state); }
    }

    #[test]
    fn collector_concurrent_polls_both_pauses_for_allocation_free_mutators() {
        const HELPER: &str = "GCR_CONCURRENT_POLL_HELPER";
        if std::env::var_os(HELPER).is_some() {
            let heap = Arc::new(Heap::new::<Full>(1 << 16, vec![]));
            let done = Arc::new(AtomicBool::new(false));
            let (ready, registered) = std::sync::mpsc::channel();
            let worker_heap = heap.clone();
            let worker_done = done.clone();
            let worker = std::thread::spawn(move || {
                let (state, _) = worker_heap.register_thread();
                let flag = Box::new(AtomicU8::new(0));
                unsafe { state.set_poll_flag((&*flag as *const AtomicU8).cast_mut()); }
                ready.send(()).unwrap();
                let mut pauses = 0;
                while !worker_done.load(Ordering::Acquire) {
                    if flag.load(Ordering::Acquire) != 0 {
                        unsafe { state.enter_safepoint(); }
                        pauses += 1;
                    } else { std::thread::yield_now(); }
                }
                unsafe { worker_heap.safe_deregister_thread(&state); }
                pauses
            });
            registered.recv().unwrap();
            unsafe { heap.concurrent_collect::<IdentityPtrPolicy>(); }
            done.store(true, Ordering::Release);
            assert!(worker.join().unwrap() >= 2);
            return;
        }
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gc::heap::concurrent_coordination_tests::collector_concurrent_polls_both_pauses_for_allocation_free_mutators"])
            .env(HELPER, "1").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!("concurrent collector missed a poll request: {output:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}

#[cfg(test)]
mod allocation_request_tests {
    use super::*;
    use crate::gc::{Compact, Full, IdentityPtrPolicy};

    #[test]
    fn collector_canonical_requests_reject_forged_interior_layouts() {
        let info = TypeInfo::for_header(Full::SIZE).with_fields(1).with_raw_bytes(16)
            .with_varlen_bytes(1).with_owned_interior_ptrs(vec![24, 32]);
        let heap = Heap::new::<Full>(65536, vec![info.clone()]);
        let canonical = heap.type_info_by_id(0);
        let equivalent = info.clone().with_interior_ptrs(&[24, 32]);
        assert!(!std::ptr::eq(canonical, &equivalent));
        for length in [0, 1, 7, 8, 1024, usize::MAX] {
            let expected = info.checked_allocation_size(length);
            assert_eq!(heap.allocation_request(canonical, length).map(|r| r.size), expected);
            assert_eq!(heap.allocation_request(&equivalent, length).map(|r| r.size), expected);
        }
        for forged in [info.clone().with_interior_ptrs(&[]),
            info.clone().with_interior_ptrs(&[0]), info.clone().with_interior_ptrs(&[24]),
            info.clone().with_fields(0), info.clone().with_type_id(65535),
            info.clone().with_raw_bytes(24), info.clone().with_align_log2(4)] {
            assert!(heap.allocation_request(&forged, 8).is_none());
        }
    }

    #[test]
    fn allocation_requests_validate_inputs_and_reselect_space_after_collection() {
        let info = TypeInfo::for_header(Full::SIZE).with_type_id(0).with_raw_bytes(16).with_varlen_bytes(0);
        let heap = Heap::new::<Full>(1 << 16, vec![info.clone()]);
        let (state, _) = heap.register_thread();
        assert!(heap.object_request::<Compact>(&info, 17).is_none());
        assert!(heap.allocation_request(&info.clone().with_raw_bytes(24), 17).is_none());
        assert!(heap.allocation_request(&info, usize::MAX).is_none());
        assert_eq!(heap.from_used(), 0);
        let request = heap.object_request::<Full>(&info, 17).unwrap();
        let original = unsafe { request.allocate(&state) };
        assert!(!original.is_null());
        let payload = info.varlen_count_offset() + 8;
        unsafe { std::ptr::write_bytes(original.add(payload), 0xA5, 17); }
        let root = unsafe { heap.globals.add(original as u64) };
        unsafe { heap.mutator_triggered_gc::<IdentityPtrPolicy>(&state); }
        assert_ne!(heap.globals.get(root), original as u64);
        let next = unsafe { request.allocate(&state) };
        assert!(!next.is_null() && heap.from_space().contains(next));
        assert_eq!(unsafe { read_type_id(next, Full::TYPE_ID_OFFSET) }, 0);
        assert_eq!(unsafe { read_varlen_count(next, &info) }, 17);
        assert!(unsafe { std::slice::from_raw_parts(next.add(payload), 17) }.iter().all(|&byte| byte == 0));
        let large = heap.object_request::<Full>(&info, 9000).unwrap();
        let pointer = unsafe { large.allocate(&state) };
        assert!(!pointer.is_null() && heap.from_space().contains(pointer));
        assert_eq!(unsafe { read_varlen_count(pointer, &info) }, 9000);
        unsafe { heap.safe_deregister_thread(&state); }
    }
}

#[cfg(test)]
#[path = "custom_header_policy_tests.rs"]
mod custom_header_policy_tests;

#[cfg(test)]
mod custom_header_tests {
    use super::*;
    use crate::gc::IdentityPtrPolicy;

    #[repr(transparent)]
    #[derive(Clone, Copy)]
    pub(super) struct OffsetHeader<const N: usize, const ID: usize, const FORWARD: usize>([u16; N]);

    // Whole initialized words, stable array layout, an aligned in-bounds ID,
    // and clear bit 63 in both reserved words for every type ID used below.
    unsafe impl<const N: usize, const ID: usize, const FORWARD: usize> ObjHeader
        for OffsetHeader<N, ID, FORWARD>
    {
        const SIZE: usize = N * 2;
        const TYPE_ID_OFFSET: usize = ID;
        const HEAP_FORWARDING_OFFSET: usize = FORWARD;
        fn new(id: u16) -> Self {
            let mut words = [0x1234; N];
            words[ID / 2] = id;
            Self(words)
        }
        fn type_id(&self) -> u16 { self.0[ID / 2] }
    }

    fn layout_collections<H: ObjHeader>(mode: u8) {
        let info = TypeInfo::for_header(H::SIZE).with_type_id(1)
            .with_fields(2).with_raw_bytes(8);
        let table = vec![info.clone().with_type_id(0), info.clone()];
        let heap = if mode == 1 {
            Heap::new_generational::<H>(1 << 18, 1 << 18, table)
        } else { Heap::new::<H>(1 << 18, table) };
        let count = 256;
        let objects: Vec<_> = (0..count).map(|_| unsafe {
            if mode == 1 { heap.alloc_nursery_obj::<H>(&info, 0) }
            else { heap.alloc_obj::<H>(&info, 0) }
        }).collect();
        assert!(objects.iter().all(|p| !p.is_null()));
        let expected = H::new(1);
        let expected_bytes = unsafe {
            std::slice::from_raw_parts((&expected as *const H).cast::<u8>(), H::SIZE)
        };
        // One rooted cycle with many aliases to its first node. All other
        // objects must be found transitively, including by the atomic copier.
        for (index, &object) in objects.iter().enumerate() {
            unsafe {
                object.add(info.value_field_offset(0)).cast::<u64>()
                    .write(objects[(index + 1) % count] as u64);
                object.add(info.value_field_offset(1)).cast::<u64>().write(objects[0] as u64);
                object.add(info.raw_data_offset()).cast::<u64>().write(index as u64);
            }
        }
        let root = unsafe { heap.globals.add(objects[0] as u64) };
        let (state, _) = heap.register_thread();
        for pass in 0..3 {
            let old = heap.globals.get(root) as *mut u8;
            unsafe {
                match (mode, pass) {
                    (1, 0) => heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state),
                    (2, _) => {
                        state.enter_blocked();
                        heap.concurrent_collect::<IdentityPtrPolicy>();
                        state.exit_blocked(&heap);
                    }
                    _ => heap.mutator_triggered_gc::<IdentityPtrPolicy>(&state),
                }
            }
            let first = heap.globals.get(root) as *mut u8;
            assert_ne!(old, first);
            assert_eq!(unsafe { crate::gc::barrier::read_barrier_atomic(old, heap.forwarding_offset()) }, first);
            let mut current = first;
            for index in 0..count {
                unsafe {
                    assert_eq!(read_type_id(current, H::TYPE_ID_OFFSET), 1);
                    assert_eq!(std::slice::from_raw_parts(current, H::SIZE), expected_bytes);
                    assert_eq!(current.add(info.raw_data_offset()).cast::<u64>().read(), index as u64);
                    assert_eq!(current.add(info.value_field_offset(1)).cast::<u64>().read(), first as u64);
                    current = current.add(info.value_field_offset(0)).cast::<*mut u8>().read();
                }
            }
            assert_eq!(current, first);
            assert_eq!(heap.from_used(), count * info.allocation_size(0));
        }
        unsafe { heap.safe_deregister_thread(&state); }
    }

    #[test]
    fn collector_custom_header_type_id_decoding_uses_native_bytes() {
        fn check<H: ObjHeader>() {
            let object = H::new(513);
            let pointer = (&object as *const H).cast::<u8>();
            let word = unsafe { pointer.add(H::HEAP_FORWARDING_OFFSET).cast::<u64>().read_unaligned() };
            assert_eq!(unsafe { HeaderLayout::of::<H>().type_id_from_word(pointer, word) }, 513);
        }
        check::<OffsetHeader<4, 2, 0>>();
        check::<OffsetHeader<8, 10, 0>>();
        check::<OffsetHeader<8, 2, 8>>();
    }

    fn matrix(mode: u8) {
        const HELPER: &str = "GCR_CUSTOM_HEADER_HELPER";
        if std::env::var_os(HELPER).is_none() {
            use std::process::{Command, Stdio};
            use std::time::{Duration, Instant};
            let test = match mode {
                0 => "collector_custom_header_major_preserves_metadata_and_shared_cycles",
                1 => "collector_custom_header_minor_promotes_then_major_moves",
                _ => "collector_custom_header_concurrent_preserves_metadata_and_shared_cycles",
            };
            for workers in [1, 2, 4, 8] {
                let mut child = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", &format!("gc::heap::custom_header_tests::{test}")])
                    .env(HELPER, "1").env("GCR_GC_WORKERS", workers.to_string())
                    .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
                let deadline = Instant::now() + Duration::from_secs(15);
                while child.try_wait().unwrap().is_none() {
                    if Instant::now() >= deadline {
                        child.kill().unwrap();
                        let output = child.wait_with_output().unwrap();
                        panic!("custom-header collection hung with {workers} workers: {output:?}");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let output = child.wait_with_output().unwrap();
                assert!(output.status.success(), "{workers} workers: {output:?}");
            }
            return;
        }
        layout_collections::<OffsetHeader<4, 2, 0>>(mode);
        layout_collections::<OffsetHeader<8, 10, 0>>(mode);
        layout_collections::<OffsetHeader<8, 2, 8>>(mode);
    }

    #[test]
    fn collector_custom_header_major_preserves_metadata_and_shared_cycles() { matrix(0); }
    #[test]
    fn collector_custom_header_minor_promotes_then_major_moves() { matrix(1); }
    #[test]
    fn collector_custom_header_concurrent_preserves_metadata_and_shared_cycles() { matrix(2); }
}

#[cfg(test)]
mod gc_telemetry_tests {
    use super::*;
    use crate::gc::{Full, IdentityPtrPolicy, ObjHeader};
    use std::time::Duration;

    #[test]
    fn collector_pause_percentiles_use_nearest_rank_in_text_and_json() {
        assert_eq!(pause_percentile(&[], 50), 0);
        assert_eq!(pause_percentile(&[17], 99), 17);
        assert_eq!(pause_percentile(&[10, 20], 50), 10);
        let heap = Heap::new::<Full>(4096, vec![]);
        for millis in 1..=100 {
            heap.record_gc_duration(GcKind::Major, Duration::from_millis(millis), 1000, 500, 0);
        }
        assert!(heap.gc_stats_summary().contains("pause p50 50.000ms p99 99.000ms max 100.000ms"));
        let metrics = heap.metrics_json();
        assert!(metrics.contains("\"gc_pause_p50_ms\":50.0000"));
        assert!(metrics.contains("\"gc_pause_p99_ms\":99.0000"));
    }

    #[test]
    fn collector_gc_telemetry_saturates_duration_conversion() {
        let heap = Heap::new::<Full>(4096, vec![]);
        let beyond_nanoseconds = Duration::from_nanos(u64::MAX) + Duration::from_nanos(1);
        heap.record_gc_timings(GcKind::Major, beyond_nanoseconds, Duration::MAX, 0, 0, 0);
        let event = &heap.gc_events()[0];
        assert_eq!(event.pause_ns, u64::MAX);
        assert_eq!(event.elapsed_ns, u64::MAX);
    }

    #[test]
    fn collector_gc_telemetry_saturates_cumulative_totals() {
        let heap = Heap::new::<Full>(4096, vec![]);
        for _ in 0..2 {
            heap.record_gc_timings(GcKind::Major, Duration::from_nanos(u64::MAX),
                Duration::from_nanos(u64::MAX), u64::MAX, 0, 0);
            heap.record_gc_timings(GcKind::Major, Duration::ZERO, Duration::ZERO,
                0, 0, u64::MAX);
        }
        let summary = heap.gc_stats_summary();
        assert!(summary.contains("4 major collections"));
        assert!(summary.contains(&format!("reclaimed {} B | promoted {} B", u64::MAX, u64::MAX)));
        let metrics = heap.metrics_json();
        let max_ms = format!("{:.4}", u64::MAX as f64 / 1.0e6);
        for key in ["gc_pause_total_ms", "gc_pause_max_ms", "gc_elapsed_total_ms"] {
            assert!(metrics.contains(&format!("\"{key}\":{max_ms}")), "{metrics}");
        }
        for key in ["gc_reclaimed_bytes", "gc_promoted_bytes"] {
            assert!(metrics.contains(&format!("\"{key}\":{}", u64::MAX)), "{metrics}");
        }
    }

    #[test]
    fn collector_elapsed_cycle_time_stays_separate_from_pause_metrics() {
        let heap = Heap::new::<Full>(4096, vec![]);
        heap.record_gc_timings(GcKind::Concurrent, Duration::from_millis(5), Duration::from_secs(1), 1000, 500, 0);
        let events = heap.gc_events();
        assert_eq!(events[0].pause_ns, 5_000_000);
        assert_eq!(events[0].elapsed_ns, 1_000_000_000);
        let log = heap.gc_log_jsonl();
        assert!(log.contains("\"pause_ns\":5000000,\"elapsed_ns\":1000000000"));
        let metrics = heap.metrics_json();
        assert!(metrics.contains("\"gc_pause_total_ms\":5.0000"));
        assert!(metrics.contains("\"gc_elapsed_total_ms\":1000.0000"));
    }

    #[test]
    fn collector_concurrent_pause_excludes_running_copy_interval() {
        use std::sync::atomic::AtomicPtr;
        static HEAP: AtomicPtr<Heap> = AtomicPtr::new(std::ptr::null_mut());
        static SLEPT: AtomicBool = AtomicBool::new(false);
        struct DelayedIdentity;
        impl PtrPolicy for DelayedIdentity {
            fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
                let pointer = IdentityPtrPolicy::try_decode_ptr(bits);
                let heap = HEAP.load(Ordering::Acquire);
                if pointer.is_some() && !heap.is_null() {
                    let heap = unsafe { &*heap };
                    if heap.barriers_active() && !heap.gc_requested() && !SLEPT.swap(true, Ordering::AcqRel) {
                        std::thread::sleep(Duration::from_millis(30));
                    }
                }
                pointer
            }
            fn encode_ptr(pointer: *mut u8) -> u64 { IdentityPtrPolicy::encode_ptr(pointer) }
        }
        let node = TypeInfo::for_header(Full::SIZE).with_fields(1);
        let heap = Heap::new::<Full>(4096, vec![node.clone()]);
        let parent = unsafe { heap.alloc_obj::<Full>(&node, 0) };
        let child = unsafe { heap.alloc_obj::<Full>(&node, 0) };
        unsafe {
            parent.add(Full::SIZE).cast::<*mut u8>().write(child);
            heap.globals.add(parent as u64);
        }
        HEAP.store((&heap as *const Heap).cast_mut(), Ordering::Release);
        unsafe { heap.concurrent_collect::<DelayedIdentity>(); }
        HEAP.store(std::ptr::null_mut(), Ordering::Release);
        assert!(SLEPT.load(Ordering::Acquire));
        let events = heap.gc_events();
        assert_eq!(events.len(), 1);
        assert!(events[0].elapsed_ns.saturating_sub(events[0].pause_ns) >= 20_000_000);
        assert_eq!(heap.from_used(), 2 * node.allocation_size(0));
    }
}

#[cfg(test)]
mod heap_peak_tests {
    use super::*;
    use crate::gc::{Full, IdentityPtrPolicy, ObjHeader};

    #[test]
    fn collector_sampled_heap_peak_reports_allocations_without_collections() {
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        for generational in [false, true] {
            let heap = if generational { Heap::new_generational::<Full>(1024, 4096, vec![info.clone()]) }
                else { Heap::new::<Full>(4096, vec![info.clone()]) };
            assert!(!unsafe { heap.alloc_obj::<Full>(&info, 0) }.is_null());
            assert!(!unsafe { heap.alloc_nursery_obj::<Full>(&info, 0) }.is_null());
            assert!(heap.gc_events().is_empty());
            assert!(heap.metrics_json().contains("\"peak_heap_bytes\":48"));
            assert_eq!(heap.sampled_heap_peak_bytes(), 48);
        }
    }

    #[test]
    fn collector_sampled_heap_peak_preserves_combined_pre_minor_occupancy() {
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let heap = Heap::new_generational::<Full>(1024, 4096, vec![info.clone()]);
        let (state, _) = heap.register_thread();
        let old = unsafe { heap.alloc_obj::<Full>(&info, 0) };
        unsafe { heap.globals.add(old as u64); }
        assert!(!unsafe { heap.alloc_nursery_obj::<Full>(&info, 0) }.is_null());
        unsafe { heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state); }
        assert_eq!(heap.nursery_used(), 0);
        assert_eq!(heap.from_used(), 24);
        assert_eq!(heap.gc_events()[0].before_bytes, 24); // collected space only
        assert!(heap.metrics_json().contains("\"peak_heap_bytes\":48"));
        unsafe { heap.safe_deregister_thread(&state); }
    }

    #[test]
    fn collector_sampled_heap_peak_preserves_combined_pre_major_occupancy() {
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let heap = Heap::new_generational::<Full>(1024, 4096, vec![info.clone()]);
        assert!(!unsafe { heap.alloc_obj::<Full>(&info, 0) }.is_null());
        let young = unsafe { heap.alloc_nursery_obj::<Full>(&info, 0) };
        unsafe { heap.globals.add(young as u64); heap.stw_collect::<IdentityPtrPolicy>(); }
        assert_eq!(heap.from_used(), 0);
        assert_eq!(heap.nursery_used(), 24);
        assert_eq!(heap.gc_events()[0].before_bytes, 24);
        assert!(heap.metrics_json().contains("\"peak_heap_bytes\":48"));
    }
}

#[cfg(test)]
#[path = "telemetry_heap_tests.rs"]
mod telemetry_heap_tests;

#[cfg(test)]
#[path = "coordinator_unwind_tests.rs"]
mod coordinator_unwind_tests;

#[cfg(test)]
#[path = "runtime_retirement_tests.rs"]
mod runtime_retirement_tests;

#[path = "channel_roots.rs"]
mod channel_roots;
pub(crate) use channel_roots::{ConditionalChannels, ChannelEdges};

#[cfg(test)]
#[path = "caller_parking_tests.rs"]
mod caller_parking_tests;

#[path = "scoped_roots.rs"]
mod scoped_roots;
use scoped_roots::{Registry as ScopedExtraRegistry, Source as ScopedExtraSource, Guard as ScopedExtraGuard};

#[path = "permanent_roots.rs"]
mod permanent_roots;

#[cfg(test)]
mod abi_window_tests {
    use super::*;
    use crate::gc::Full;

    #[test]
    fn collector_exported_window_survives_heap_moves_and_allocation() {
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let heap = Heap::new::<Full>(4096, vec![info.clone()]);
        let exported = heap.alloc_window_ptr();
        let mut owners = Vec::new();
        owners.push(heap);
        owners.reserve_exact(64);
        let heap = Box::new(owners.pop().unwrap());
        assert_eq!(heap.alloc_window_ptr(), exported);
        let window = unsafe { &*exported };
        let cursor = window.cursor.load(Ordering::Acquire).cast::<AtomicUsize>();
        assert_eq!(unsafe { (*cursor).load(Ordering::Acquire) }, 0);
        let object = unsafe { heap.alloc_obj::<Full>(&info, 0) };
        assert!(!object.is_null());
        assert_eq!(
            unsafe { (*cursor).load(Ordering::Acquire) },
            info.allocation_size(0)
        );
        assert_eq!(
            window.base.load(Ordering::Acquire),
            heap.from_space().base()
        );
        assert_eq!(window.limit.load(Ordering::Acquire), 4096);
    }
}

#[cfg(test)]
#[path = "thread_identity_tests.rs"]
mod thread_identity_tests;

#[cfg(test)]
#[path = "thread_census_tests.rs"]
mod thread_census_tests;

#[cfg(test)]
#[path = "pause_callers_tests.rs"]
mod pause_callers_tests;

#[cfg(test)]
#[path = "retired_profile_tests.rs"]
mod retired_profile_tests;

#[cfg(test)]
mod promotion_scratch_heap_tests {
    use super::*;
    use crate::gc::Full;
    use crate::runtime::allocation_probe;

    #[test]
    fn collector_promotion_preflight_scans_only_remembered_old_objects() {
        let info = TypeInfo::for_header(Full::SIZE).with_fields(2);
        let heap = Heap::new_generational::<Full>(4096, 128 * 1024, vec![info.clone()]);
        let _guard = heap.gc_lock.lock().unwrap();
        let mut old = core::ptr::null_mut();
        for _ in 0..2000 {
            old = unsafe { heap.alloc_obj::<Full>(&info, 0) };
            assert!(!old.is_null());
        }
        let first = unsafe { heap.alloc_nursery_obj::<Full>(&info, 0) };
        let second = unsafe { heap.alloc_nursery_obj::<Full>(&info, 0) };
        unsafe {
            first.add(Full::SIZE).cast::<*mut u8>().write(second);
            second.add(Full::SIZE).cast::<*mut u8>().write(first);
            old.add(Full::SIZE).cast::<*mut u8>().write(first);
        }
        heap.mark_card_dirty(unsafe { old.add(Full::SIZE) });
        let cards = &heap.nursery_state.as_ref().unwrap().card_tables[0];
        assert_eq!(cards.iter_dirty().count(), 1);
        let mut slots = 0;
        unsafe { heap.scan_minor_remembered_roots(&mut |_| slots += 1); }
        assert!(slots > 0 && slots < 128, "scanned {slots} slots for one dirty card");
        // The old owner is deliberately unrooted: a minor retains it anyway.
        // Its cycle must be counted once and the cards must survive preflight.
        let bytes = info.allocation_size(0);
        assert!(unsafe { heap.nursery_survivors_exceed::<IdentityPtrPolicy>(bytes) });
        assert!(!unsafe { heap.nursery_survivors_exceed::<IdentityPtrPolicy>(2 * bytes) });
        assert_eq!(cards.iter_dirty().count(), 1);
    }

    #[test]
    fn collector_promotion_marking_tracks_returned_live_backing_and_old_roots() {
        let info = TypeInfo::for_header(Full::SIZE).with_fields(2);
        let heap = Heap::new_generational::<Full>(1 << 18, 1 << 18, vec![info.clone()]);
        // No registered/concurrent mutators. Keep the collector lock throughout
        // both read-only traversals, exactly as the production preflight does.
        let _guard = heap.gc_lock.lock().unwrap();
        let mut objects = Vec::new();
        for _ in 0..258 {
            let object = unsafe { heap.alloc_nursery_obj::<Full>(&info, 0) };
            assert!(!object.is_null());
            unsafe {
                object.add(Full::SIZE).cast::<u64>().write(0);
                object.add(Full::SIZE + 8).cast::<u64>().write(0);
            }
            objects.push(object);
        }
        for index in 0..257 {
            unsafe {
                objects[index].add(Full::SIZE).cast::<u64>().write(objects[(index + 1) % 257] as u64);
                objects[index].add(Full::SIZE + 8).cast::<u64>().write(objects[0] as u64);
                heap.globals.add(objects[index] as u64);
            }
        }
        // Unreachable old owners remain in a minor, so their young children
        // count toward promotion preflight but not whole-graph major liveness.
        let old = unsafe { heap.alloc_obj::<Full>(&info, 0) };
        assert!(!old.is_null());
        unsafe {
            old.add(Full::SIZE).cast::<u64>().write(objects[257] as u64);
            old.add(Full::SIZE + 8).cast::<u64>().write(0);
        }
        heap.mark_card_dirty(unsafe { old.add(Full::SIZE) });
        // Warm platform lock initialization before measuring physical backing.
        drop(unsafe { heap.reachable_nursery_objects::<IdentityPtrPolicy>() });
        assert!(!unsafe { heap.nursery_survivors_exceed::<IdentityPtrPolicy>(usize::MAX) });
        let baseline = heap.native_memory.registry.bytes();
        let owners = Arc::strong_count(&heap.native_memory.registry);
        allocation_probe::start();
        let live = unsafe { heap.reachable_nursery_objects::<IdentityPtrPolicy>() };
        let traffic = allocation_probe::finish_bytes();
        assert_eq!(live.len(), 257);
        assert!(objects[..257].iter().all(|object| live.contains(&(*object as usize))));
        assert!(!live.contains(&(objects[257] as usize)));
        assert_eq!(heap.native_memory.registry.bytes() - baseline, traffic.2 - traffic.3);
        assert!(heap.native_memory.registry.bytes() > baseline);
        assert_eq!(Arc::strong_count(&heap.native_memory.registry), owners);
        drop(live);
        assert_eq!(heap.native_memory.registry.bytes(), baseline);
        let bytes = info.allocation_size(0);
        for (limit, expected) in [(0, true), (257 * bytes, true), (258 * bytes, false), (usize::MAX, false)] {
            assert_eq!(unsafe { heap.nursery_survivors_exceed::<IdentityPtrPolicy>(limit) }, expected);
            assert_eq!(heap.native_memory.registry.bytes(), baseline);
        }
        assert!(heap.gc_events().is_empty());
    }
}
