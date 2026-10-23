#[path = "shared_metadata.rs"]
mod shared_metadata;
pub(crate) use shared_metadata::{SharedMetadata, SharedMetadataWeak};
use core::cell::{Cell, RefCell};
#[path = "initialized_ranges.rs"]
mod initialized_ranges;
pub(crate) use initialized_ranges::InitializedRanges;
use std::sync::Mutex;
#[cfg(test)]
use std::sync::Arc;

use crate::gc::field::{init_header, read_type_id, read_varlen_count, write_varlen_count};
use crate::gc::header::{Compact, HeaderLayout, ObjHeader};
use crate::gc::type_info::{TypeInfo, VarLenKind};

// ─── Traits ──────────────────────────────────────────────────────────

/// Allocator trait for heap objects.
///
/// Takes a `&TypeInfo` (not `&'static`) — the allocator only needs the
/// layout info for the duration of the allocation call.
pub trait Alloc {
    /// Allocate space for an object described by `info` with `varlen_len`
    /// variable-length elements. Returns a zeroed pointer, or null if
    /// the allocation cannot be satisfied. Atomic managed arenas support
    /// eight-byte alignment; legacy bump arenas also support larger alignments.
    fn alloc(&self, info: &TypeInfo, varlen_len: usize) -> *mut u8;
}

/// Heap walking trait for GC.
pub trait HeapWalker {
    /// Walk all live objects, calling `visitor(obj_ptr, type_id)` for each.
    ///
    /// # Safety
    /// All objects must be valid (headers initialized, varlen counts written).
    /// Mutators must be stopped or the caller must have exclusive arena access.
    /// Keep allocation extents stable throughout capture and do not collect or
    /// invalidate objects from the visitor.
    unsafe fn walk(&self, type_table: &[TypeInfo], visitor: &mut dyn FnMut(*mut u8, &TypeInfo));
}

// ─── alloc_obj helper ────────────────────────────────────────────────

/// Allocate an object, initialize its header, and write the varlen count.
///
/// Returns null if the underlying allocator returns null.
///
/// # Safety
/// - `allocator` must return properly aligned, zeroed memory.
/// - `info` must accurately describe the object layout and `H` must match the
///   arena's configured header, including its type-id offset.
/// - No collection, reset or walk may overlap reservation/initialization.
///   Initialize all pointer slots and register the object before a safepoint.
pub unsafe fn alloc_obj<H: ObjHeader>(
    allocator: &dyn Alloc,
    info: &TypeInfo,
    varlen_len: usize,
) -> *mut u8 {
    if !HeaderLayout::of::<H>().accepts(info, varlen_len) {
        return core::ptr::null_mut();
    }
    let ptr = allocator.alloc(info, varlen_len);
    if ptr.is_null() {
        return ptr;
    }
    unsafe {
        init_header::<H>(ptr, info.type_id);
        if info.varlen != VarLenKind::None {
            write_varlen_count(ptr, info, varlen_len);
        }
    }
    ptr
}

// ─── AllocWindow (JIT-facing allocation mirror) ──────────────────────

/// The three words the JIT's INLINE allocation fast path reads, mirrored
/// from the active from-space and updated only under stop-the-world (at
/// space flips). Layout is ABI: codegen reads fields at fixed offsets.
///
///   offset 0: cursor — pointer to the active space's atomic cursor
///             (the JIT `atomicrmw add`s it directly)
///   offset 8: base   — the active space's base address
///   offset 16: limit — allocation limit in bytes; 0 forces EVERY
///             allocation through the out-of-line slow path (used by
///             gc-stress mode so collect-per-alloc still fires)
#[repr(C)]
pub struct AllocWindow {
    pub cursor: core::sync::atomic::AtomicPtr<u8>,
    pub base: core::sync::atomic::AtomicPtr<u8>,
    pub limit: AtomicUsize,
}

impl AllocWindow {
    pub fn empty() -> Self {
        AllocWindow {
            cursor: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
            base: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
            limit: AtomicUsize::new(0),
        }
    }
    /// Point the window at `space` (called at construction and at every
    /// flip, both under STW or before any mutator runs).
    /// # Safety
    /// `space` must outlive all window consumers. No consumer may read or use
    /// the window while its three-part allocation mapping is being replaced.
    pub unsafe fn point_at(&self, space: &AtomicBumpAllocator, limit: usize) {
        assert!(limit <= space.size(), "allocation window exceeds its arena");
        self.cursor.store(
            space.cursor.as_raw().cast::<u8>(),
            Ordering::Release,
        );
        self.base.store(space.base, Ordering::Release);
        self.limit.store(limit, Ordering::Release);
    }
}

// ─── BumpAllocator ───────────────────────────────────────────────────

/// A bump (linear) allocator for heap objects.
///
/// Allocates by bumping a cursor forward. No per-object deallocation —
/// the entire region is freed at once via `reset()` (after GC evacuation)
/// or on `Drop`.
///
/// Uses `Cell<usize>` for the cursor so `alloc(&self)` works without
/// `&mut self`. Single-threaded only (`!Sync`).
pub struct BumpAllocator {
    base: *mut u8,
    cursor: Cell<usize>,
    allocations: RefCell<Vec<usize>>,
    size: usize,
    type_id_offset: usize,
    header: HeaderLayout,
    owned: bool,
}

// Safety: BumpAllocator can be moved between threads (Send),
// but cannot be shared (&self across threads) because of Cell (!Sync).
unsafe impl Send for BumpAllocator {}

impl BumpAllocator {
    /// Create a new bump allocator that owns a region of `size` bytes.
    ///
    /// The header type `H` determines the `type_id_offset` used by
    /// the heap walker.
    pub fn new<H: ObjHeader>(size: usize) -> Self {
        assert!(size > 0, "arena size must be positive");
        let layout = std::alloc::Layout::from_size_align(size, 8).unwrap();
        let base = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!base.is_null(), "BumpAllocator: allocation failed");
        Self {
            base,
            cursor: Cell::new(0),
            allocations: RefCell::new(Vec::new()),
            size,
            type_id_offset: H::TYPE_ID_OFFSET,
            header: HeaderLayout::of::<H>(),
            owned: true,
        }
    }

    /// Wrap an externally-owned memory region as a bump allocator.
    ///
    /// # Safety
    /// - `base` must point to a valid, zeroed region of at least `size` bytes.
    /// - The region must remain valid for the lifetime of this allocator.
    /// - The caller is responsible for freeing the region.
    pub unsafe fn from_region<H: ObjHeader>(base: *mut u8, size: usize) -> Self {
        assert!(
            !base.is_null()
                && size > 0
                && size <= isize::MAX as usize
                && (base as usize).checked_add(size).is_some(),
            "invalid arena region"
        );
        Self {
            base,
            cursor: Cell::new(0),
            allocations: RefCell::new(Vec::new()),
            size,
            type_id_offset: H::TYPE_ID_OFFSET,
            header: HeaderLayout::of::<H>(),
            owned: false,
        }
    }

    /// Reset the cursor to 0. After this, the entire region can be reused.
    ///
    /// Typically called after GC evacuation has copied all live objects out.
    ///
    /// # Safety
    /// No old object access or heap walk may overlap reset or subsequent reuse.
    /// All previous allocations must be retired.
    pub unsafe fn reset(&self) {
        self.allocations.borrow_mut().clear();
        self.cursor.set(0);
    }

    /// Number of bytes currently used.
    pub fn used(&self) -> usize {
        self.cursor.get()
    }

    /// Number of bytes remaining.
    pub fn remaining(&self) -> usize {
        self.size - self.cursor.get()
    }

    /// Base pointer of the region.
    pub fn base(&self) -> *mut u8 {
        self.base
    }

    /// Total size of the region in bytes.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Check if a pointer falls within this allocator's region.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let addr = ptr as usize;
        let base_addr = self.base as usize;
        addr >= base_addr && addr < base_addr + self.size
    }

    /// The type_id_offset stored at construction.
    pub fn type_id_offset(&self) -> usize {
        self.type_id_offset
    }
}

impl BumpAllocator {
    /// Return the exact start of an allocated object. The collector releases
    /// the metadata borrow before scanning, so evacuation may append objects.
    pub(crate) fn allocated_object(&self, index: usize) -> Option<*mut u8> {
        self.allocations
            .borrow()
            .get(index)
            .map(|&offset| unsafe { self.base.add(offset) })
    }
}

impl Alloc for BumpAllocator {
    fn alloc(&self, info: &TypeInfo, varlen_len: usize) -> *mut u8 {
        if !self.header.accepts(info, varlen_len) {
            return core::ptr::null_mut();
        }
        let Some(obj_size) = info.checked_allocation_size(varlen_len) else {
            return core::ptr::null_mut();
        };
        let align = 1usize << info.align_log2;

        let cur = self.cursor.get();
        // Align cursor up
        let Some(address) = (self.base as usize).checked_add(cur) else {
            return core::ptr::null_mut();
        };
        let Some(padded) = address.checked_add(align - 1) else {
            return core::ptr::null_mut();
        };
        let aligned = (padded & !(align - 1)) - self.base as usize;
        let Some(new_cursor) = aligned.checked_add(obj_size) else {
            return core::ptr::null_mut();
        };

        if new_cursor > self.size {
            return core::ptr::null_mut();
        }

        // Zero the allocation (the region may have been reset but contain
        // stale data from previous use).
        let ptr = unsafe { self.base.add(aligned) };
        unsafe {
            core::ptr::write_bytes(ptr, 0, obj_size);
        }

        self.allocations.borrow_mut().push(aligned);
        self.cursor.set(new_cursor);
        ptr
    }
}

impl HeapWalker for BumpAllocator {
    unsafe fn walk(&self, type_table: &[TypeInfo], visitor: &mut dyn FnMut(*mut u8, &TypeInfo)) {
        for &offset in self.allocations.borrow().iter() {
            let ptr = unsafe { self.base.add(offset) };
            let type_id = unsafe { read_type_id(ptr, self.type_id_offset) };
            visitor(ptr, &type_table[type_id as usize]);
        }
    }
}

impl Drop for BumpAllocator {
    fn drop(&mut self) {
        if self.owned {
            let layout = std::alloc::Layout::from_size_align(self.size, 8).unwrap();
            unsafe {
                std::alloc::dealloc(self.base, layout);
            }
        }
    }
}

// ─── AtomicBumpAllocator ────────────────────────────────────────────

use core::sync::atomic::{AtomicUsize, Ordering};
use crate::runtime::metadata_allocator::MetadataAllocator;

type BufferRegistry = allocator_api2::vec::Vec<SharedMetadata<BufferExtent>, MetadataAllocator>;

/// Thread-safe bump allocator using an atomic cursor.
///
/// Unlike `BumpAllocator` (which uses `Cell` and is `!Sync`), this
/// allocator uses `AtomicUsize` for the cursor and can be shared
/// across threads. Used for:
/// - Shared from-space allocation (multiple mutator threads)
/// - To-space allocation during parallel GC copying
///
/// Uses `fetch_add` with a CAS retry loop for alignment.
pub struct AtomicBumpAllocator {
    epoch: AtomicUsize,
    buffers: Mutex<BufferRegistry>,
    base: *mut u8,
    // Keep exported cursor aliases valid when the allocator or Heap moves.
    cursor: crate::abi_storage::AbiStorage<AtomicUsize>,
    size: usize,
    type_id_offset: usize,
    header: HeaderLayout,
    owned: bool,
}

// Safety: AtomicBumpAllocator is designed for cross-thread use.
// The base pointer is immutable after construction, and the cursor
// uses atomic operations.
unsafe impl Send for AtomicBumpAllocator {}
unsafe impl Sync for AtomicBumpAllocator {}

impl AtomicBumpAllocator {
    #[cfg(test)]
    pub(crate) fn collector_cursor_backing_bytes(&self) -> usize {
        crate::abi_storage::AbiStorage::<AtomicUsize>::allocation_bytes()
    }
    /// Create a new atomic bump allocator that owns a region of `size` bytes.
    pub fn new<H: ObjHeader>(size: usize) -> Self {
        Self::with_metadata_allocator::<H>(size, MetadataAllocator::default())
    }

    /// Own an arena while retaining an accounting owner for its TLAB registry.
    /// Managed region bytes and record/control allocations have separate scopes.
    pub(crate) fn with_metadata_allocator<H: ObjHeader>(size: usize, allocator: MetadataAllocator) -> Self {
        assert!(size > 0, "arena size must be positive");
        assert!(
            HeaderLayout::of::<H>().supported_by_atomic_arena(),
            "atomic arena headers must be at most eight-byte aligned"
        );
        let layout = std::alloc::Layout::from_size_align(size, 8).unwrap();
        let base = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!base.is_null(), "AtomicBumpAllocator: allocation failed");
        let cursor = crate::abi_storage::AbiStorage::new_in(AtomicUsize::new(0), &allocator);
        Self {
            epoch: AtomicUsize::new(0),
            buffers: Mutex::new(BufferRegistry::new_in(allocator)),
            base,
            cursor,
            size,
            type_id_offset: H::TYPE_ID_OFFSET,
            header: HeaderLayout::of::<H>(),
            owned: true,
        }
    }

    /// Reset the cursor to 0, making the entire region available for reuse.
    ///
    /// Note: this does NOT zero the memory. Newly allocated objects will
    /// see stale data in uninitialized fields. Callers must initialize
    /// all GC-traceable fields before the next collection.
    ///
    /// # Safety
    /// No allocation, access to old objects, or heap walk may overlap reset.
    /// All previous allocations and TLAB publications must be retired.
    pub unsafe fn reset(&self) {
        self.buffers.lock().unwrap().clear();
        self.epoch.fetch_add(1, Ordering::Release);
        self.cursor.store(0, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn buffer_registry_backing_bytes(&self) -> usize {
        self.buffers.lock().unwrap().capacity() * core::mem::size_of::<SharedMetadata<BufferExtent>>()
    }

    #[cfg(test)]
    pub(crate) fn inventory_buffer_records(&self, records: &mut std::collections::HashMap<usize, usize>) {
        for record in self.buffers.lock().unwrap().iter() {
            records.insert(SharedMetadata::allocation_identity(record), SharedMetadata::<BufferExtent>::allocation_bytes());
        }
    }

    /// Number of bytes currently used.
    pub fn used(&self) -> usize {
        // The JIT's inline fast path bumps with fetch_add and may
        // overshoot `size` when it loses the race for the last bytes
        // (those claims take the slow path); clamp so walkers and
        // accounting never see a cursor past the space.
        self.cursor.load(Ordering::Acquire).min(self.size)
    }

    /// Identifies the allocation generation. Read while the arena is quiescent
    /// when retaining metadata about object addresses across collections.
    pub(crate) fn epoch_ptr(&self) -> *const AtomicUsize { &self.epoch }

    pub(crate) fn epoch(&self) -> usize {
        self.epoch.load(Ordering::Acquire)
    }

    /// Number of bytes remaining.
    pub fn remaining(&self) -> usize {
        self.size
            .saturating_sub(self.cursor.load(Ordering::Acquire))
    }

    /// Base pointer of the region.
    pub fn base(&self) -> *mut u8 {
        self.base
    }

    /// Total size of the region in bytes.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Check if a pointer falls within this allocator's region.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let addr = ptr as usize;
        let base_addr = self.base as usize;
        addr >= base_addr && addr < base_addr + self.size
    }

    /// The type_id_offset stored at construction.
    pub fn type_id_offset(&self) -> usize {
        self.type_id_offset
    }
}

impl AtomicBumpAllocator {
    /// Reserve a batch of exact object extents with one cursor CAS. All bytes
    /// must be initialized before publication, and no linear walk may run until
    /// reservations complete. No unused copying-buffer tail is introduced.
    pub(crate) unsafe fn reserve_batch(
        &self,
        layouts: &[(usize, usize)],
        addresses: &mut allocator_api2::vec::Vec<*mut u8, impl allocator_api2::alloc::Allocator>,
    ) -> bool {
        loop {
            let cur = self.cursor.load(Ordering::Relaxed);
            let mut end = cur;
            addresses.clear();
            for &(size, align) in layouts {
                if align != 8 || size == 0 || size % 8 != 0 {
                    return false;
                }
                let Some(padded) = end.checked_add(align - 1) else {
                    return false;
                };
                let aligned = padded & !(align - 1);
                let Some(next) = aligned.checked_add(size) else {
                    return false;
                };
                if next > self.size {
                    return false;
                }
                addresses.push(unsafe { self.base.add(aligned) });
                end = next;
            }
            if self
                .cursor
                .compare_exchange_weak(cur, end, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Reserve an object extent without clearing it. Collector copying must
    /// initialize every byte before publishing the object or walking the heap.
    /// Concurrent linear walks are forbidden until all reservations are filled.
    pub(crate) unsafe fn alloc_uninitialized(&self, info: &TypeInfo, varlen_len: usize) -> *mut u8 {
        if info.align_log2 != 3 {
            return core::ptr::null_mut();
        }
        if !self.header.accepts(info, varlen_len) {
            return core::ptr::null_mut();
        }
        let Some(obj_size) = info.checked_allocation_size(varlen_len) else {
            return core::ptr::null_mut();
        };
        let align = 1usize << info.align_log2;
        loop {
            let cur = self.cursor.load(Ordering::Relaxed);
            let Some(padded) = cur.checked_add(align - 1) else {
                return core::ptr::null_mut();
            };
            let aligned = padded & !(align - 1);
            let Some(new_cursor) = aligned.checked_add(obj_size) else {
                return core::ptr::null_mut();
            };
            if new_cursor > self.size {
                return core::ptr::null_mut();
            }
            if self
                .cursor
                .compare_exchange_weak(cur, new_cursor, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return unsafe { self.base.add(aligned) };
            }
        }
    }
}

/// A reserved TLAB with a published initialized prefix. Only the buffer owner
/// advances initialized; collectors read it after the safepoint handshake.
struct BufferExtent {
    start: usize,
    end: usize,
    initialized: AtomicUsize,
}

/// The largest buffer a thread reserves at once. A thread's buffers double
/// from 2 KiB with allocation demand up to this, so a busy thread rarely
/// returns to the shared allocator.
const MAX_BUFFER_BYTES: usize = 256 * 1024;

/// Owning-mutator allocation window used by compiled code. All fields except
/// the pointed-to epoch, stress flag, and initialized prefix are owner-only.
/// The pause coordinator closes generated windows before resumption; native
/// allocation validates arena epochs before reopening them. The prefix
/// is published before the next safepoint; collectors never read it concurrently
/// with an allocating mutator. Disabled windows have limit zero.
#[repr(C)]
pub struct InlineTlab {
    pub cursor: usize,
    pub limit: usize,
    pub initialized: *const AtomicUsize,
    pub epoch: *const AtomicUsize,
    pub expected_epoch: usize,
    pub base: *mut u8,
    pub stress: *const core::sync::atomic::AtomicBool,
    pub counters: *mut super::thread::SiteCounter,
    pub counters_len: usize,
}
impl Default for InlineTlab {
    fn default() -> Self {
        Self { cursor: 0, limit: 0, initialized: core::ptr::null(), epoch: core::ptr::null(),
            expected_epoch: 0, base: core::ptr::null_mut(), stress: core::ptr::null(),
            counters: core::ptr::null_mut(), counters_len: 0 }
    }
}
pub mod inline_tlab_offsets {
    pub const CURSOR: usize = 0;
    pub const LIMIT: usize = 8;
    pub const INITIALIZED: usize = 16;
    pub const EPOCH: usize = 24;
    pub const EXPECTED_EPOCH: usize = 32;
    pub const BASE: usize = 40;
    pub const STRESS: usize = 48;
    pub const COUNTERS: usize = 56;
    pub const COUNTERS_LEN: usize = 64;
}
const _: () = {
    assert!(core::mem::offset_of!(InlineTlab, cursor) == inline_tlab_offsets::CURSOR);
    assert!(core::mem::offset_of!(InlineTlab, limit) == inline_tlab_offsets::LIMIT);
    assert!(core::mem::offset_of!(InlineTlab, initialized) == inline_tlab_offsets::INITIALIZED);
    assert!(core::mem::offset_of!(InlineTlab, epoch) == inline_tlab_offsets::EPOCH);
    assert!(core::mem::offset_of!(InlineTlab, expected_epoch) == inline_tlab_offsets::EXPECTED_EPOCH);
    assert!(core::mem::offset_of!(InlineTlab, base) == inline_tlab_offsets::BASE);
    assert!(core::mem::offset_of!(InlineTlab, stress) == inline_tlab_offsets::STRESS);
    assert!(core::mem::offset_of!(InlineTlab, counters) == inline_tlab_offsets::COUNTERS);
    assert!(core::mem::offset_of!(InlineTlab, counters_len) == inline_tlab_offsets::COUNTERS_LEN);
};

/// Owning-thread-only allocation state. Descriptors survive thread teardown in
/// the allocator until reset, so abandoned tails remain accounted for.
pub(crate) struct Tlab {
    allocator: usize,
    epoch: usize,
    pub(crate) window: InlineTlab,
    extent: Option<SharedMetadata<BufferExtent>>,
    target_bytes: usize,
}
impl Tlab {
    #[cfg(test)]
    pub(crate) fn metadata_record(&self) -> Option<(usize, usize)> {
        self.extent.as_ref().map(|record| (
            SharedMetadata::allocation_identity(record),
            SharedMetadata::<BufferExtent>::allocation_bytes(),
        ))
    }

    pub(crate) fn new() -> Self {
        Self {
            allocator: 0,
            epoch: 0,
            window: InlineTlab::default(),
            extent: None,
            target_bytes: 2048,
        }
    }
    /// Standalone checked adapter for allocator tests. Production allocations
    /// receive a heap-bound validated request and use alloc_sized directly.
    #[cfg(test)]
    pub(crate) fn alloc(
        &mut self,
        space: &AtomicBumpAllocator,
        info: &TypeInfo,
        len: usize,
    ) -> *mut u8 {
        if info.align_log2 != 3 || !space.header.accepts(info, len) {
            return core::ptr::null_mut();
        }
        let Some(size) = info.checked_allocation_size(len) else {
            return core::ptr::null_mut();
        };
        // Large allocations use exact shared reservations.
        if size > 8192 { return space.alloc(info, len); }
        unsafe { self.alloc_sized(space, size) }
    }

    /// Allocate a size from a layout already validated against this arena.
    /// Caller must own this TLAB between safepoints. Size must be positive,
    /// eight-byte aligned, at most 8192 bytes, and cover the complete object.
    #[inline]
    pub(crate) unsafe fn alloc_sized(&mut self, space: &AtomicBumpAllocator, size: usize) -> *mut u8 {
        debug_assert!(size > 0 && size <= 8192 && size % 8 == 0);
        let align = 8;
        let identity = space as *const _ as usize;
        let epoch = space.epoch.load(Ordering::Acquire);
        if self.allocator != identity || self.epoch != epoch {
            self.window.limit = 0;
            self.extent = None;
            self.window.expected_epoch = epoch;
            self.allocator = identity;
            self.epoch = epoch;
        }
        loop {
            if let Some(extent) = &self.extent {
                let aligned = match self.window.cursor.checked_add(align - 1) {
                    Some(n) => n & !(align - 1),
                    None => return core::ptr::null_mut(),
                };
                if let Some(end) = aligned.checked_add(size).filter(|&end| end <= extent.end) {
                    let ptr = unsafe { space.base.add(aligned) };
                    unsafe {
                        core::ptr::write_bytes(ptr, 0, size);
                    }
                    self.window.cursor = end;
                    if !self.window.epoch.is_null() {
                        // A pause may close a still-current reservation. Native
                        // epoch validation above makes reopening it safe.
                        self.window.limit = extent.end;
                    }
                    extent.initialized.store(end, Ordering::Relaxed);
                    return ptr;
                }
            }
            if self.extent.is_some() {
                self.target_bytes = (self.target_bytes * 2).min(MAX_BUFFER_BYTES);
            }
            self.window.limit = 0;
            self.extent = None;
            let extent = match space.reserve_buffer(size, align, self.target_bytes) {
                Some(extent) => extent,
                None => return core::ptr::null_mut(),
            };
            self.window.cursor = extent.start;
            if !self.window.epoch.is_null() {
                self.window.limit = extent.end;
                self.window.initialized = &extent.initialized;
            }
            self.extent = Some(extent);
        }
    }
}
impl AtomicBumpAllocator {
    // Refill is rare relative to allocation from an initialized TLAB. Keep
    // native allocation, locking and metadata growth out of the hot function's
    // register allocation and instruction footprint.
    #[cold]
    #[inline(never)]
    fn reserve_buffer(
        &self,
        minimum: usize,
        align: usize,
        target: usize,
    ) -> Option<SharedMetadata<BufferExtent>> {
        loop {
            let cur = self.cursor.load(Ordering::Relaxed);
            let start = cur.checked_add(align - 1)? & !(align - 1);
            let remaining = self.size.checked_sub(start)?;
            if remaining < minimum {
                return None;
            }
            let bytes = remaining.min(target.max(minimum)) & !7;
            if bytes < minimum {
                return None;
            }
            let end = start.checked_add(bytes)?;
            if self
                .cursor
                .compare_exchange_weak(cur, end, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            let mut buffers = self.buffers.lock().unwrap();
            let extent = SharedMetadata::new_in(BufferExtent {
                start,
                end,
                initialized: AtomicUsize::new(start),
            }, buffers.allocator());
            buffers.push(extent.clone());
            return Some(extent);
        }
    }

    /// Initialized object regions, excluding every active or retired TLAB tail.
    /// Caller must have stopped mutators before interpreting these boundaries.
    pub(crate) fn initialized_ranges(&self) -> InitializedRanges<allocator_api2::alloc::Global> {
        self.initialized_ranges_in(allocator_api2::alloc::Global)
    }

    pub(crate) fn initialized_ranges_in<A: allocator_api2::alloc::Allocator>(&self, allocator: A) -> InitializedRanges<A> {
        let used = self.used();
        let buffers = self.buffers.lock().unwrap();
        InitializedRanges::capture(used, &buffers, allocator)
    }

    /// Same validity/quiescence contract as HeapWalker::walk. Capture completes
    /// and releases the buffer registry lock before any visitor is invoked.
    pub(crate) unsafe fn walk_in<A: allocator_api2::alloc::Allocator>(&self, type_table: &[TypeInfo], allocator: A, visitor: &mut dyn FnMut(*mut u8, &TypeInfo)) {
        let ranges = self.initialized_ranges_in(allocator);
        unsafe { self.walk_ranges(type_table, ranges, visitor) };
    }

    unsafe fn walk_ranges<A: allocator_api2::alloc::Allocator>(&self, type_table: &[TypeInfo], ranges: InitializedRanges<A>, visitor: &mut dyn FnMut(*mut u8, &TypeInfo)) {
        for range in &ranges {
            let mut offset = range.start;
            while offset < range.end {
                let ptr = unsafe { self.base.add(offset) };
                let type_id = unsafe { read_type_id(ptr, self.type_id_offset) };
                let info = &type_table[type_id as usize];

                let varlen_len = match info.varlen {
                    VarLenKind::None => 0,
                    _ => unsafe { read_varlen_count(ptr, info) },
                };
                let obj_size = info.allocation_size(varlen_len);

                assert!(
                    obj_size <= range.end - offset,
                    "object crosses initialized allocation extent"
                );
                visitor(ptr, info);

                let align = 1usize << info.align_log2;
                offset = ((offset + obj_size) + align - 1) & !(align - 1);
            }
        }
    }

}

impl Alloc for AtomicBumpAllocator {
    fn alloc(&self, info: &TypeInfo, varlen_len: usize) -> *mut u8 {
        let ptr = unsafe { self.alloc_uninitialized(info, varlen_len) };
        if !ptr.is_null() {
            unsafe {
                core::ptr::write_bytes(ptr, 0, info.allocation_size(varlen_len));
            }
        }
        ptr
    }
}

impl HeapWalker for AtomicBumpAllocator {
    unsafe fn walk(&self, type_table: &[TypeInfo], visitor: &mut dyn FnMut(*mut u8, &TypeInfo)) {
        unsafe { self.walk_ranges(type_table, self.initialized_ranges(), visitor) };
    }
}

impl Drop for AtomicBumpAllocator {
    fn drop(&mut self) {
        if self.owned {
            let layout = std::alloc::Layout::from_size_align(self.size, 8).unwrap();
            unsafe {
                std::alloc::dealloc(self.base, layout);
            }
        }
    }
}

// ─── FFI ─────────────────────────────────────────────────────────────

/// FFI: allocate from a bump allocator (raw, no header init).
///
/// # Safety
/// - `allocator` must point to a valid `BumpAllocator`.
/// - `info` must point to a valid `TypeInfo`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bump_alloc(
    allocator: *const BumpAllocator,
    info: *const TypeInfo,
    varlen_len: usize,
) -> *mut u8 {
    let allocator = unsafe { &*allocator };
    let info = unsafe { &*info };
    allocator.alloc(info, varlen_len)
}

/// FFI: allocate from a bump allocator and initialize a Compact header.
///
/// # Safety
/// - `allocator` must point to a valid `BumpAllocator`.
/// - `info` must point to a valid `TypeInfo`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bump_alloc_init_compact(
    allocator: *const BumpAllocator,
    info: *const TypeInfo,
    varlen_len: usize,
) -> *mut u8 {
    let allocator = unsafe { &*allocator };
    let info = unsafe { &*info };
    unsafe { alloc_obj::<Compact>(allocator, info, varlen_len) }
}

/// FFI: reset a bump allocator's cursor to 0.
///
/// # Safety
/// `allocator` must point to a valid `BumpAllocator`; all old allocations
/// must be retired, with no overlapping object access or heap walk.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bump_reset(allocator: *const BumpAllocator) {
    let allocator = unsafe { &*allocator };
    unsafe { allocator.reset() };
}

#[cfg(test)]
mod reservation_tests {
    use super::*;
    use crate::gc::Full;

    #[test]
    fn exhausted_cursor_cannot_wrap_during_reservation() {
        let allocator = AtomicBumpAllocator::new::<Full>(4096);
        let info = TypeInfo::for_header(Full::SIZE);
        for cursor in [usize::MAX, usize::MAX - 7, usize::MAX - 15] {
            allocator.cursor.store(cursor, Ordering::Relaxed);
            assert!(unsafe { allocator.alloc_uninitialized(&info, 0) }.is_null());
            assert_eq!(allocator.cursor.load(Ordering::Relaxed), cursor);
        }
        unsafe { allocator.reset() };
    }
    #[test]
    fn batch_reservation_uses_exact_extents_and_does_not_advance_on_failure() {
        let allocator = AtomicBumpAllocator::new::<Full>(128);
        let mut addresses = allocator_api2::vec::Vec::new();
        assert!(unsafe { allocator.reserve_batch(&[(24, 8), (32, 8), (16, 8)], &mut addresses) });
        assert_eq!(allocator.used(), 72);
        assert_eq!(
            addresses
                .iter()
                .map(|&ptr| ptr as usize - allocator.base() as usize)
                .collect::<Vec<_>>(),
            vec![0, 24, 56]
        );
        assert!(!unsafe { allocator.reserve_batch(&[(24, 8), (64, 8)], &mut addresses) });
        assert_eq!(allocator.used(), 72);
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use crate::gc::Full;
    #[test]
    fn exported_cursor_address_survives_allocator_moves() {
        let allocator = AtomicBumpAllocator::new::<Full>(4096);
        let window = AllocWindow::empty();
        unsafe {
            window.point_at(&allocator, 4096);
        }
        let exported = window.cursor.load(Ordering::Acquire);
        let allocator = Box::new(allocator);
        assert_eq!(
            exported,
            &*allocator.cursor as *const AtomicUsize as *mut u8
        );
        let info = TypeInfo::for_header(Full::SIZE);
        assert!(!allocator.alloc(&info, 0).is_null());
        assert_eq!(
            unsafe { (&*exported.cast::<AtomicUsize>()).load(Ordering::Acquire) },
            info.allocation_size(0)
        );
    }
}

#[cfg(test)]
mod tlab_tests {
    use super::*;
    use crate::gc::Full;

    #[test]
    fn collector_range_snapshot_walker_releases_registry_lock_before_callbacks() {
        use crate::runtime::{allocation_counter::AllocationCounter, metadata_allocator::MetadataAllocator};
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let space = AtomicBumpAllocator::new::<Full>(128 * 1024);
        let info = TypeInfo::for_header(Full::SIZE).with_fields(1);
        let mut tlabs: Vec<_> = (0..4).map(|_| Tlab::new()).collect();
        for tlab in &mut tlabs {
            let object = tlab.alloc(&space, &info, 0);
            assert!(!object.is_null());
            unsafe { init_header::<Full>(object, 0) };
        }
        let mut visits = 0;
        unsafe { space.walk_in(&[info], &allocator, &mut |_, _| {
            assert!(counter.bytes() > 0, "outer walk keeps its backing charged");
            assert!(space.buffers.try_lock().is_ok(), "visitor must not inherit the buffer lock");
            let nested = space.initialized_ranges_in(&allocator);
            assert_eq!(nested.len(), 4);
            drop(nested);
            visits += 1;
        }); }
        assert_eq!(visits, 4);
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 2);
    }

    #[test]
    fn initialized_walk_skips_poisoned_active_and_retired_tails() {
        let space = AtomicBumpAllocator::new::<Full>(128 * 1024);
        let info = TypeInfo::for_header(Full::SIZE).with_fields(1);
        let mut first = Tlab::new();
        let mut second = Tlab::new();
        for tlab in [&mut first, &mut second] {
            let obj = tlab.alloc(&space, &info, 0);
            unsafe {
                init_header::<Full>(obj, 0);
            }
        }
        // Make treating a buffer's unused bytes as object headers fail loudly.
        for extent in space.buffers.lock().unwrap().iter() {
            let start = extent.initialized.load(Ordering::Relaxed);
            unsafe {
                core::ptr::write_bytes(space.base.add(start), 0xff, extent.end - start);
            }
        }
        drop(first);
        let mut count = 0;
        unsafe {
            space.walk(&[info.clone()], &mut |_, _| count += 1);
        }
        assert_eq!(count, 2);
        // Reusing an active buffer must zero newly allocated fields despite the
        // poisoned tail, then expand only the initialized prefix.
        let obj = second.alloc(&space, &info, 0);
        unsafe {
            init_header::<Full>(obj, 0);
            assert_eq!(obj.add(16).cast::<u64>().read(), 0);
        }
        count = 0;
        unsafe {
            space.walk(&[info], &mut |_, _| count += 1);
        }
        assert_eq!(count, 3);
    }

    #[test]
    fn reset_invalidates_a_partially_used_buffer() {
        let space = AtomicBumpAllocator::new::<Full>(65536);
        let info = TypeInfo::for_header(Full::SIZE);
        let mut tlab = Tlab::new();
        let old = tlab.alloc(&space, &info, 0);
        unsafe {
            init_header::<Full>(old, 0);
        }
        unsafe { space.reset() };
        let fresh = tlab.alloc(&space, &info, 0);
        assert_eq!(old, fresh);
        unsafe {
            init_header::<Full>(fresh, 0);
        }
        assert_eq!(space.buffers.lock().unwrap().len(), 1);
        let mut count = 0;
        unsafe {
            space.walk(&[info], &mut |_, _| count += 1);
        }
        assert_eq!(count, 1);
    }
    #[test]
    fn buffers_start_small_and_grow_with_allocation_demand() {
        let space = AtomicBumpAllocator::new::<Full>(512 * 1024);
        let info = TypeInfo::for_header(Full::SIZE);
        let mut tlab = Tlab::new();
        let first = tlab.alloc(&space, &info, 0);
        unsafe {
            init_header::<Full>(first, 0);
        }
        assert_eq!(space.used(), 2048);
        for _ in 0..20000 {
            let ptr = tlab.alloc(&space, &info, 0);
            assert!(!ptr.is_null());
            unsafe {
                init_header::<Full>(ptr, 0);
            }
        }
        let buffers = space.buffers.lock().unwrap();
        assert_eq!(buffers[0].end - buffers[0].start, 2048);
        assert!(
            buffers
                .iter()
                .all(|buffer| buffer.end - buffer.start <= MAX_BUFFER_BYTES)
        );
        assert_eq!(
            buffers.last().unwrap().end - buffers.last().unwrap().start,
            MAX_BUFFER_BYTES
        );
    }
}

#[cfg(test)]
mod allocation_boundary_tests {
    use super::*;
    use crate::gc::header::Full;

    #[test]
    fn invalid_extents_leave_bump_and_atomic_cursors_unchanged() {
        let bump = BumpAllocator::new::<Full>(4096);
        let atomic = AtomicBumpAllocator::new::<Full>(4096);
        let bytes = TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0);
        let over_aligned = bytes.clone().with_align_log2(6);
        let mut invalid_align = bytes.clone();
        invalid_align.align_log2 = usize::BITS as u8;
        for allocator in [&bump as &dyn Alloc, &atomic as &dyn Alloc] {
            assert!(allocator.alloc(&bytes, usize::MAX).is_null());
            assert!(allocator.alloc(&invalid_align, 0).is_null());

        }
        assert!(atomic.alloc(&over_aligned, 0).is_null());
        assert_eq!(bump.used(), 0);
        assert_eq!(atomic.used(), 0);
        assert!(!bump.alloc(&bytes, 8).is_null());
        assert!(!atomic.alloc(&bytes, 8).is_null());
        assert_eq!(bump.used(), bytes.checked_allocation_size(8).unwrap());
        assert_eq!(atomic.used(), bump.used());
    }

    #[test]
    fn invalid_tlab_extents_do_not_reserve_or_publish_buffers() {
        let space = AtomicBumpAllocator::new::<Full>(4096);
        let bytes = TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0);
        let mut tlab = Tlab::new();
        assert!(tlab.alloc(&space, &bytes, usize::MAX).is_null());
        assert!(tlab.alloc(&space, &bytes.clone().with_align_log2(6), 0).is_null());
        assert_eq!(space.used(), 0);
        assert!(space.buffers.lock().unwrap().is_empty());
        assert!(!tlab.alloc(&space, &bytes, 8).is_null());
        let before = space.used();
        let initialized = space.buffers.lock().unwrap()[0]
            .initialized
            .load(Ordering::Acquire);
        assert!(tlab.alloc(&space, &bytes, usize::MAX).is_null());
        assert_eq!(space.used(), before);
        assert_eq!(
            space.buffers.lock().unwrap()[0]
                .initialized
                .load(Ordering::Acquire),
            initialized
        );
    }

    #[test]
    fn invalid_batch_alignment_does_not_advance_shared_cursor() {
        let space = AtomicBumpAllocator::new::<Full>(4096);
        let mut addresses = allocator_api2::vec::Vec::new();
        for layouts in [&[(24, 8), (64, 64)][..], &[(0, 8)][..], &[(16, 0)][..]] {
            assert!(!unsafe { space.reserve_batch(layouts, &mut addresses) });
            assert_eq!(space.used(), 0);
        }
        assert!(unsafe { space.reserve_batch(&[(24, 8)], &mut addresses) });
        assert_eq!(space.used(), 24);
    }


    #[test]
    fn legacy_bump_walk_skips_mixed_alignment_padding() {
        let mut storage = vec![0u8; 4096 + 64];
        let base = unsafe { storage.as_mut_ptr().add(8) };
        let bump = unsafe { BumpAllocator::from_region::<Compact>(base, 4096) };
        let normal = TypeInfo::for_header(Compact::SIZE).with_fields(1);
        let mut aligned = normal.clone().with_align_log2(6);
        aligned.type_id = 1;
        let mut expected = Vec::new();
        for info in [&normal, &aligned, &normal, &aligned] {
            let ptr = unsafe { alloc_obj::<Compact>(&bump, info, 0) };
            assert!(!ptr.is_null());
            assert_eq!(ptr as usize % (1usize << info.align_log2), 0);
            expected.push(ptr);
        }
        let mut visited = Vec::new();
        unsafe { bump.walk(&[normal.clone(), aligned.clone()], &mut |ptr, _| visited.push(ptr)); }
        assert_eq!(visited, expected);
        unsafe { bump.reset(); }
        visited.clear();
        unsafe { bump.walk(&[normal, aligned], &mut |ptr, _| visited.push(ptr)); }
        assert!(visited.is_empty());
    }

}

#[cfg(test)]
mod header_contract_tests {
    use super::*;
    use crate::gc::{Full, SemiSpace};

    #[repr(C, align(64))]
    #[derive(Clone, Copy)]
    struct AlignedHeader {
        id: u16,
        padding: [u8; 62],
    }
    // All 64 bytes are initialized, the type-id word is aligned at offset zero,
    // and the forwarding bit is clear. The arena must honor 64-byte alignment.
    unsafe impl ObjHeader for AlignedHeader {
        const SIZE: usize = 64;
        const TYPE_ID_OFFSET: usize = 0;
        fn new(id: u16) -> Self {
            Self {
                id,
                padding: [0; 62],
            }
        }
        fn type_id(&self) -> u16 {
            self.id
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct OffsetHeader {
        id: u16,
        padding: [u8; 6],
        extra: u64,
    }
    // Same physical size/alignment as Full, but type id is in its first word.
    unsafe impl ObjHeader for OffsetHeader {
        const SIZE: usize = 16;
        const TYPE_ID_OFFSET: usize = 0;
        fn new(id: u16) -> Self {
            Self {
                id,
                padding: [0; 6],
                extra: 0,
            }
        }
        fn type_id(&self) -> u16 {
            self.id
        }
    }

    #[test]
    fn same_sized_header_with_different_type_id_offset_is_rejected() {
        let info = TypeInfo::for_header(Full::SIZE).with_type_id(1);
        let heap = crate::gc::Heap::new::<Full>(64, vec![info.clone().with_type_id(0), info.clone()]);
        // Exclusive heap access. Size/alignment alone are insufficient: later
        // collection would otherwise read the type id at the wrong offset.
        assert!(unsafe { heap.alloc_obj::<OffsetHeader>(&info, 0) }.is_null());
        assert!(unsafe { heap.alloc_nursery_obj::<OffsetHeader>(&info, 0) }.is_null());
        assert_eq!(heap.from_used(), 0);
        let gc = SemiSpace::new::<Full>(64);
        assert!(gc.alloc_obj::<OffsetHeader>(&info, 0).is_null());
        assert_eq!(gc.from_used(), 0);
    }

    #[test]
    fn mismatched_header_is_rejected_before_reservation_or_initialization() {
        let bump = BumpAllocator::new::<Compact>(64);
        let compact = TypeInfo::for_header(Compact::SIZE);
        let full = TypeInfo::for_header(Full::SIZE);
        // Previously the first call wrote a 16-byte Full into an eight-byte
        // allocation; the second crossed the arena's configured header layout.
        assert!(unsafe { alloc_obj::<Full>(&bump, &compact, 0) }.is_null());
        assert!(unsafe { alloc_obj::<Full>(&bump, &full, 0) }.is_null());
        assert_eq!(bump.used(), 0);
        assert!(!unsafe { alloc_obj::<Compact>(&bump, &compact, 0) }.is_null());
        assert_eq!(bump.used(), Compact::SIZE);

        let gc = SemiSpace::new::<Compact>(64);
        assert!(gc.alloc_obj::<Full>(&full, 0).is_null());
        assert_eq!(gc.from_used(), 0);
        assert!(!gc.alloc_obj::<Compact>(&compact, 0).is_null());
    }

    #[test]
    fn physical_header_alignment_is_checked_and_supported_legacy_alignment_survives() {
        let bump = BumpAllocator::new::<AlignedHeader>(256);
        let under_aligned = TypeInfo::for_header(AlignedHeader::SIZE);
        assert!(unsafe { alloc_obj::<AlignedHeader>(&bump, &under_aligned, 0) }.is_null());
        assert_eq!(bump.used(), 0);
        let aligned = under_aligned.with_align_log2(6);
        let ptr = unsafe { alloc_obj::<AlignedHeader>(&bump, &aligned, 0) };
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % 64, 0);
        let mut visited = Vec::new();
        // Exclusive arena access; the one allocation has a complete header.
        unsafe {
            bump.walk(&[aligned], &mut |obj, _| visited.push(obj));
        }
        assert_eq!(visited, [ptr]);
        assert!(
            std::panic::catch_unwind(|| AtomicBumpAllocator::new::<AlignedHeader>(256)).is_err()
        );
    }

    #[test]
    fn zero_sized_arenas_are_rejected_before_native_allocation() {
        assert!(std::panic::catch_unwind(|| BumpAllocator::new::<Compact>(0)).is_err());
        assert!(std::panic::catch_unwind(|| AtomicBumpAllocator::new::<Compact>(0)).is_err());
    }
}

#[cfg(test)]
mod buffer_registry_storage_tests {
    use super::*;
    use crate::runtime::{allocation_counter::AllocationCounter, allocation_probe};
    #[test]
    fn collector_tlab_registry_empty_owner_and_exact_backing_request() {
        let counter = Arc::<AllocationCounter>::default();
        let arena = AtomicBumpAllocator::with_metadata_allocator::<crate::gc::Full>(
            4096,
            MetadataAllocator::new(&counter),
        );
        assert_eq!(counter.bytes(), arena.collector_cursor_backing_bytes());
        assert_eq!(Arc::strong_count(&counter), 3);
        let mut buffers = arena.buffers.lock().unwrap();
        assert_eq!(buffers.capacity(), 0);
        // Acquire the registry lock before observing vector requests: macOS
        // mutex first-use control allocation is a separate ownership domain.
        allocation_probe::start();
        buffers.reserve_exact(7);
        let request = allocation_probe::finish_bytes();
        assert_eq!((request.0, request.1, request.3), (1, 0, 0));
        assert_eq!(counter.bytes(), request.2 + arena.collector_cursor_backing_bytes());
        assert_eq!(
            counter.bytes(),
            buffers.capacity() * std::mem::size_of::<SharedMetadata<BufferExtent>>() + arena.collector_cursor_backing_bytes()
        );
        drop(buffers);
        drop(arena);
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 1);
    }
    #[test]
    fn collector_tlab_registry_reset_retains_charge_and_reuses_backing() {
        let counter = Arc::<AllocationCounter>::default();
        let arena = AtomicBumpAllocator::with_metadata_allocator::<crate::gc::Full>(
            4096,
            MetadataAllocator::new(&counter),
        );
        let old: Vec<_> = (0..9)
            .map(|_| arena.reserve_buffer(16, 8, 64).unwrap())
            .collect();
        let capacity = arena.buffers.lock().unwrap().capacity();
        let retained = counter.bytes();
        assert_eq!(
            retained,
            capacity * std::mem::size_of::<SharedMetadata<BufferExtent>>() + arena.collector_cursor_backing_bytes()
                + old.len() * SharedMetadata::<BufferExtent>::allocation_bytes()
        );
        assert!(old.iter().all(|r| SharedMetadata::strong_count(r) == 2));
        // No objects were allocated and no mutator or walker is active. These
        // retained metadata observers never access the managed region.
        unsafe {
            arena.reset();
        }
        assert_eq!(arena.used(), 0);
        assert_eq!(counter.bytes(), retained);
        assert!(old.iter().all(|r| SharedMetadata::strong_count(r) == 1));
        let new: Vec<_> = (0..9)
            .map(|_| arena.reserve_buffer(16, 8, 64).unwrap())
            .collect();
        assert_eq!(counter.bytes(), retained + new.len() * SharedMetadata::<BufferExtent>::allocation_bytes());
        assert_eq!(arena.buffers.lock().unwrap().capacity(), capacity);
        for (old, new) in old.iter().zip(&new) {
            assert!(!SharedMetadata::ptr_eq(old, new));
            assert_eq!((old.start, old.end), (new.start, new.end));
        }
        drop(arena);
        assert_eq!(counter.bytes(), (old.len() + new.len()) * SharedMetadata::<BufferExtent>::allocation_bytes());
        assert_eq!(Arc::strong_count(&counter), 1 + old.len() + new.len());
        assert!(new.iter().all(|r| SharedMetadata::strong_count(r) == 1));
        drop(old);
        assert_eq!(counter.bytes(), new.len() * SharedMetadata::<BufferExtent>::allocation_bytes());
        drop(new);
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 1);
    }
    #[test]
    fn collector_tlab_registry_counter_outlives_constructor_borrow() {
        let (arena, observer) = {
            let counter = Arc::<AllocationCounter>::default();
            let arena = AtomicBumpAllocator::with_metadata_allocator::<crate::gc::Full>(
                4096,
                MetadataAllocator::new(&counter),
            );
            let record = arena.reserve_buffer(16, 8, 64).unwrap();
            assert_eq!((record.start, record.end), (0, 64));
            (arena, Arc::downgrade(&counter))
        };
        {
            let counter = observer.upgrade().expect("registry owns its counter");
            assert!(counter.bytes() > 0);
            assert_eq!(Arc::strong_count(&counter), 4);
        }
        drop(arena);
        assert!(observer.upgrade().is_none());
    }
}
