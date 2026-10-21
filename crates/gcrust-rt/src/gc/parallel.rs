//! Parallel evacuation with exclusive copy ownership and quiescence detection.
//! All mutators are stopped throughout this module's execution.
use super::*;
#[path = "work_ring.rs"]
mod work_ring;
use work_ring::Ring;
#[path = "remembered_work.rs"]
mod remembered_work;
use remembered_work::RememberedWork;
use allocator_api2::alloc::Allocator;
use crate::runtime::metadata_allocator::MetadataAllocator;
#[path = "root_slots.rs"]
mod root_slots;
#[path = "worker_scratch.rs"]
mod worker_scratch;
pub use worker_scratch::{CollectorScratchStats, collector_scratch_stats};
#[path = "pending_copies.rs"]
mod pending_copies;
use pending_copies::PendingCopies;
#[cfg(test)]
#[path = "resolved_retry_tests.rs"]
mod resolved_retry_tests;
use std::sync::Condvar;
use std::sync::atomic::{AtomicU64, AtomicUsize};
#[path = "worker_pool.rs"]
mod worker_pool;
pub(super) use worker_pool::WorkerPool;

#[derive(Clone, Copy)]
enum Work {
    Slot(usize),
    Object(usize),
    Channel(usize),
}
struct State<A: Allocator + Clone = MetadataAllocator> {
    queue: Ring<Work, A>,
    active: usize,
}
const MAX_WORK_BATCH: usize = 128;
const MAX_LOCAL_WORK: usize = MAX_WORK_BATCH * 2;
const MAX_LOCAL_COPY_BYTES: usize = 32 * 1024;
const MAX_SMALL_OBJECT_SLOTS: usize = 4;
const LOCAL_BUFFER_CAPACITY: usize = MAX_WORK_BATCH * MAX_SMALL_OBJECT_SLOTS;

// Copying potential plus remembered-field scanning estimates parallel work.
// Explicit worker counts bypass this automatic policy.
fn automatic_workers(work_bytes: usize, available: usize) -> usize {
    const BYTES_PER_WORKER: usize = 2 * 1024 * 1024;
    (work_bytes / BYTES_PER_WORKER)
        .max(1)
        .min(available.clamp(1, 8))
}

#[derive(Clone, Copy)]
enum WorkerPolicy {
    Explicit(usize),
    Auto(usize),
}
impl WorkerPolicy {
    fn needs_scan_estimate(self) -> bool {
        matches!(self, Self::Auto(available) if available > 1)
    }
    fn workers(self, work_bytes: impl FnOnce() -> usize) -> usize {
        match self {
            Self::Explicit(workers) => workers,
            Self::Auto(available) if available <= 1 => 1,
            Self::Auto(available) => automatic_workers(work_bytes(), available),
        }
    }
}

fn worker_policy() -> WorkerPolicy {
    if let Ok(text) = std::env::var("GCR_GC_WORKERS") {
        let workers = text
            .parse::<usize>()
            .expect("GCR_GC_WORKERS must be a positive integer");
        assert!(workers > 0, "GCR_GC_WORKERS must be positive");
        WorkerPolicy::Explicit(workers)
    } else {
        WorkerPolicy::Auto(
            std::thread::available_parallelism()
                .map_or(1, usize::from)
                .clamp(1, 8),
        )
    }
}

fn traced_bytes(info: &TypeInfo, varlen_len: usize) -> usize {
    (info.value_field_count as usize)
        .saturating_add(info.interior_ptrs.len())
        .saturating_add(if info.varlen == VarLenKind::Values {
            varlen_len
        } else {
            0
        })
        .saturating_mul(std::mem::size_of::<u64>())
}

#[cfg(test)]
mod worker_policy_tests {
    use super::automatic_workers;

    #[test]
    fn collector_parallel_worker_policy_scales_with_source_work() {
        let mib = 1024 * 1024;
        for (bytes, workers) in [
            (0, 1),
            (mib, 1),
            (2 * mib, 1),
            (4 * mib - 1, 1),
            (4 * mib, 2),
            (8 * mib, 4),
            (16 * mib, 8),
        ] {
            assert_eq!(automatic_workers(bytes, 12), workers);
        }
    }

    #[test]
    fn collector_parallel_worker_policy_respects_hardware_and_work_bounds() {
        for available in [0, 1, 2, 3, 8, 12, usize::MAX] {
            let mut previous = 0;
            for bytes in [0, 1, 1 << 20, 4 << 20, 16 << 20, usize::MAX] {
                let workers = automatic_workers(bytes, available);
                assert!(workers >= previous && workers >= 1 && workers <= available.clamp(1, 8));
                if workers > 1 {
                    assert!(bytes / workers >= 2 * 1024 * 1024);
                }
                previous = workers;
            }
        }
    }

    #[test]
    fn collector_parallel_worker_policy_actual_small_collection_honors_override() {
        use crate::gc::{Full, Heap, IdentityPtrPolicy, ObjHeader, RootSet, TypeInfo};
        let explicit = std::env::var("GCR_GC_WORKERS")
            .ok()
            .map(|value| value.parse::<usize>().unwrap());
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let heap = Heap::new::<Full>(1 << 20, vec![info.clone()]);
        let mut roots = RootSet::new();
        for index in 0..8 {
            let object = unsafe { heap.alloc_obj::<Full>(&info, 0) };
            unsafe {
                object
                    .add(info.raw_data_offset())
                    .cast::<u64>()
                    .write(index);
            }
            roots.add(object as u64);
        }
        for _ in 0..3 {
            unsafe {
                heap.collect::<IdentityPtrPolicy>(&[&roots]);
            }
            assert_eq!(
                heap.collector_workers.get().is_some(),
                explicit.is_some_and(|n| n > 1)
            );
            for index in 0..8 {
                let object = roots.get(index) as *const u8;
                assert_eq!(
                    unsafe { object.add(info.raw_data_offset()).cast::<u64>().read() },
                    index as u64
                );
            }
        }
    }

    #[test]
    fn collector_parallel_worker_policy_counts_traced_layout_only() {
        use crate::gc::{Full, ObjHeader, TypeInfo, VarLenKind};
        let mut info = TypeInfo::for_header(Full::SIZE)
            .with_fields(2)
            .with_raw_bytes(16)
            .with_interior_ptrs(&[32, 40]);
        assert_eq!(super::traced_bytes(&info, 1 << 20), 32);
        info.varlen = VarLenKind::Bytes;
        assert_eq!(super::traced_bytes(&info, 1 << 20), 32);
        info.varlen = VarLenKind::Values;
        assert_eq!(super::traced_bytes(&info, 100), 832);
        assert_eq!(super::traced_bytes(&info, usize::MAX), usize::MAX);
        assert_eq!(automatic_workers(24usize.saturating_add(4 << 20), 8), 2);
        for (policy, expected) in [
            (super::WorkerPolicy::Explicit(4), 4),
            (super::WorkerPolicy::Auto(1), 1),
        ] {
            assert!(!policy.needs_scan_estimate());
            assert_eq!(
                policy.workers(|| panic!("unused estimate evaluated")),
                expected
            );
        }
        assert!(super::WorkerPolicy::Auto(8).needs_scan_estimate());
        assert_eq!(super::WorkerPolicy::Auto(8).workers(|| 4 << 20), 2);
    }

    #[test]
    fn collector_parallel_worker_policy_remembered_plan_deduplicates_cards() {
        use crate::gc::{Full, Heap, ObjHeader, TypeInfo};
        use std::collections::HashSet;
        let values = TypeInfo::for_header(Full::SIZE)
            .with_fields(2)
            .with_raw_bytes(16)
            .with_interior_ptrs(&[32, 40])
            .with_varlen_values(2);
        let bytes = TypeInfo::for_header(Full::SIZE)
            .with_type_id(1)
            .with_varlen_bytes(0);
        let heap =
            Heap::new_generational::<Full>(1 << 20, 2 << 20, vec![values.clone(), bytes.clone()]);
        let count = 4096;
        let array = unsafe { heap.alloc_obj::<Full>(&values, count) };
        let blob = unsafe { heap.alloc_obj::<Full>(&bytes, 65536) };
        assert!(!array.is_null() && !blob.is_null());
        for offset in [16, 24, 32, 40] {
            unsafe {
                array.add(offset).cast::<u64>().write(0);
            }
        }
        for index in 0..count {
            unsafe {
                array
                    .add(values.varlen_element_offset(index))
                    .cast::<u64>()
                    .write(0);
            }
        }
        for (object, size) in [
            (array, values.allocation_size(count)),
            (blob, bytes.allocation_size(65536)),
        ] {
            for offset in (0..size).step_by(256) {
                heap.mark_card_dirty(unsafe { object.add(offset) });
            }
        }
        // Exclusive single-threaded heap access is quiescent; no collector or
        // registered mutator can invalidate these initialized objects.
        let (plan, estimate) = unsafe { heap.minor_remembered_work::<true>() };
        let actual: HashSet<_> = plan
            .into_iter()
            .map(|task| match task {
                super::Work::Object(address) => address,
                _ => panic!("unexpected remembered task"),
            })
            .collect();
        assert_eq!(actual, HashSet::from([array as usize, blob as usize]));
        let mut visits = 0;
        unsafe {
            super::scan_object(array, &values, |_| visits += 1);
            super::scan_object(blob, &bytes, |_| visits += 1);
        }
        assert_eq!(estimate, visits * 8);
        assert_eq!(estimate, (count + 4) * 8);
        let (manual_plan, manual_estimate) = unsafe { heap.minor_remembered_work::<false>() };
        let manual: HashSet<_> = manual_plan
            .into_iter()
            .map(|task| match task {
                super::Work::Object(address) => address,
                _ => panic!("unexpected remembered task"),
            })
            .collect();
        assert_eq!(manual, actual);
        assert_eq!(manual_estimate, 0);
    }

    #[test]
    fn collector_parallel_worker_policy_tiny_nursery_wide_old_graph() {
        use crate::gc::{Full, Heap, IdentityPtrPolicy, ObjHeader, TypeInfo};
        let node = TypeInfo::for_header(Full::SIZE)
            .with_fields(128)
            .with_raw_bytes(8);
        let leaf = TypeInfo::for_header(Full::SIZE)
            .with_type_id(1)
            .with_raw_bytes(8);
        let heap =
            Heap::new_generational::<Full>(1 << 20, 8 << 20, vec![node.clone(), leaf.clone()]);
        let (thread, _) = heap.register_thread();
        let child = unsafe { heap.alloc_nursery_obj::<Full>(&leaf, 0) };
        unsafe {
            child.add(leaf.raw_data_offset()).cast::<u64>().write(42);
        }
        let mut nodes = Vec::new();
        for index in 0..4096 {
            let object = unsafe { heap.alloc_obj::<Full>(&node, 0) };
            assert!(!object.is_null());
            for field in 0..128 {
                unsafe {
                    object
                        .add(node.value_field_offset(field))
                        .cast::<u64>()
                        .write(0);
                }
            }
            unsafe {
                object
                    .add(node.value_field_offset(0))
                    .cast::<u64>()
                    .write(child as u64);
                object
                    .add(node.raw_data_offset())
                    .cast::<u64>()
                    .write(index);
                heap.globals.add(object as u64);
            }
            heap.mark_card_dirty(unsafe { object.add(node.value_field_offset(0)) });
            nodes.push(object);
        }
        let explicit = std::env::var("GCR_GC_WORKERS")
            .ok()
            .map(|s| s.parse::<usize>().unwrap());
        let parallel = explicit.map(|count| count > 1).unwrap_or_else(|| {
            std::thread::available_parallelism().is_ok_and(|count| count.get() > 1)
        });
        let before = heap.from_used();
        unsafe {
            heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&thread);
        }
        assert_eq!(heap.collector_workers.get().is_some(), parallel);
        assert_eq!(heap.from_used(), before + leaf.allocation_size(0));
        assert_eq!(heap.nursery_used(), 0);
        for (index, object) in nodes.into_iter().enumerate() {
            let child =
                unsafe { object.add(node.value_field_offset(0)).cast::<u64>().read() } as *const u8;
            assert!(!heap.is_nursery(child));
            assert_eq!(
                unsafe { child.add(leaf.raw_data_offset()).cast::<u64>().read() },
                42
            );
            assert_eq!(
                unsafe { object.add(node.raw_data_offset()).cast::<u64>().read() },
                index as u64
            );
            for field in 1..128 {
                assert_eq!(
                    unsafe {
                        object
                            .add(node.value_field_offset(field))
                            .cast::<u64>()
                            .read()
                    },
                    0
                );
            }
        }
    }
}

struct WorkQueue<A: Allocator + Clone = MetadataAllocator> {
    workers: usize,
    state: Mutex<State<A>>,
    wake: Condvar,
    sleepers: AtomicUsize,
}
impl<A: Allocator + Clone> WorkQueue<A> {
    fn take(&self, batch: &mut worker_scratch::Buffer<Work>) -> bool {
        batch.clear();
        let mut s = self.state.lock().unwrap();
        loop {
            if !s.queue.is_empty() {
                s.active += 1;
                // Share a scarce frontier instead of letting one participant
                // drain it. A wide frontier amortizes shared-lock traffic over
                // larger batches, with a bound on tasks per batch.
                let n = (s.queue.len() / self.workers).clamp(1, MAX_WORK_BATCH);
                s.queue.drain_into(n, batch);
                return true;
            }
            if s.active == 0 {
                self.wake.notify_all();
                return false;
            }
            // Publish demand before releasing the predicate mutex. A producer
            // donates under that same mutex, so a wake cannot be lost between
            // demand publication and sleeping. A stale hint only affects sharing:
            // bounded overflow and final finish still publish all remaining work.
            self.sleepers.fetch_add(1, Ordering::Release);
            s = self.wake.wait(s).unwrap();
            self.sleepers.fetch_sub(1, Ordering::Release);
        }
    }
    /// Publish work during a long object scan without retiring its producer.
    /// Other workers may drain this frontier while the producer still owns
    /// claims or has unvisited fields; active prevents premature quiescence.
    fn publish(&self, discovered: &mut worker_scratch::Buffer<Work>) {
        if discovered.is_empty() { return; }
        let mut s = self.state.lock().unwrap();
        let publish_to_empty = s.queue.is_empty();
        s.queue.extend_from_slice(discovered);
        discovered.clear();
        if publish_to_empty { self.wake.notify_all(); }
    }

    /// Retain at most one additional batch of published objects locally.
    /// Preserve a full processing batch before donating overflow to sleepers:
    /// narrow graphs should not fragment into contended single-task handoffs.
    /// Retained work belongs to this active producer until processed or shared.
    fn continue_or_take(&self, discovered: &mut worker_scratch::Buffer<Work>, local: &mut worker_scratch::Buffer<Work>, batch: &mut worker_scratch::Buffer<Work>, copied_bytes: usize) -> bool {
        batch.clear();
        if (!discovered.is_empty() || !local.is_empty()) && copied_bytes <= MAX_LOCAL_COPY_BYTES
            && discovered.iter().all(|task| matches!(task, Work::Object(_))) {
            let sleepers = self.sleepers.load(Ordering::Acquire);
            let keep = (discovered.len() + local.len()).min(MAX_WORK_BATCH);
            let recent = discovered.len().min(keep);
            batch.extend(discovered.drain(discovered.len() - recent..));
            let retained = local.len().min(keep - recent);
            batch.extend(local.drain(local.len() - retained..));
            if sleepers == 0 {
                let retain = discovered.len().min(MAX_WORK_BATCH - local.len());
                local.extend(discovered.drain(discovered.len() - retain..));
                self.publish(discovered);
            } else {
                self.publish_pair(discovered, local);
            }
            debug_assert!(local.len() <= MAX_WORK_BATCH);
            true
        } else {
            // BUSY retries and large copies must not remain hidden in local
            // storage. Publish retained work before retiring the active producer.
            self.publish(local);
            self.finish(discovered);
            self.take(batch)
        }
    }

    fn publish_pair(&self, first: &mut worker_scratch::Buffer<Work>, second: &mut worker_scratch::Buffer<Work>) {
        if first.is_empty() && second.is_empty() { return; }
        let mut state = self.state.lock().unwrap();
        let empty = state.queue.is_empty();
        state.queue.extend_from_slice(first);
        first.clear();
        state.queue.extend_from_slice(second);
        second.clear();
        if empty { self.wake.notify_all(); }
    }

    fn finish(&self, discovered: &mut worker_scratch::Buffer<Work>) {
        let mut s = self.state.lock().unwrap();
        let publish_to_empty = s.queue.is_empty() && !discovered.is_empty();
        s.queue.extend_from_slice(discovered);
        discovered.clear();
        s.active -= 1;
        // A waiter only sleeps on an empty queue with active producers. Wake
        // it when that predicate changes: new work appears, or all producers
        // finish. Existing queued work already has its publication notification.
        if publish_to_empty || (s.queue.is_empty() && s.active == 0) {
            self.wake.notify_all();
        }
    }
}

// If allocation/layout checking panics, restore the original header so no
// competitor remains spinning on an abandoned claim. The cycle still fails;
// its partially relocated heap must never resume application execution.
struct Claim<'a> {
    header: &'a AtomicU64,
    original: u64,
    published: bool,
}
impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if !self.published {
            self.header.store(self.original, Ordering::Release);
        }
    }
}

struct PendingCopy<'a> {
    old: *mut u8,
    slot: *mut u64,
    size: usize,
    align_log2: u8,
    needs_scan: bool,
    claim: Claim<'a>,
}

impl Heap {
    #[cfg(test)]
    pub(crate) fn collector_worker_backing_bytes(&self) -> usize {
        self.collector_workers.get().map_or(0, WorkerPool::backing_bytes)
    }

    /// Enumerate root sources on the coordinator: RootSource need not be Sync.
    /// Deduplicate slots because registrations and frame walks can overlap.
    /// Pointer addresses are transferred only within the scoped pause, while
    /// root owners remain alive and every mutator is suspended.
    pub(super) unsafe fn parallel_major<P: PtrPolicy>(&self, extras: &[&dyn RootSource]) {
        unsafe { self.parallel_major_with_nursery_roots::<P>(extras, None) };
    }

    pub(super) unsafe fn parallel_major_with_nursery_roots<P: PtrPolicy>(
        &self, extras: &[&dyn RootSource], nursery_live: Option<&NurseryLive<'_>>,
    ) {
        unsafe { self.parallel_evacuate::<P>(extras, false, RememberedWork::default(), 0, nursery_live, worker_policy()) };
    }

    pub(super) unsafe fn parallel_minor<P: PtrPolicy>(&self) {
        let policy = worker_policy();
        let (initial, scan_bytes) = if policy.needs_scan_estimate() {
            unsafe { self.minor_remembered_work::<true>() }
        } else {
            unsafe { self.minor_remembered_work::<false>() }
        };
        unsafe { self.parallel_evacuate::<P>(&[], true, initial, scan_bytes, None, policy) };
    }

    /// # Safety
    /// Same quiescence and card-barrier requirements as minor evacuation. This
    /// read-only traversal leaves cards intact for the ensuing collection.
    pub(super) unsafe fn scan_minor_remembered_roots(&self, visit: &mut dyn FnMut(*mut u64)) {
        let (objects, _) = unsafe { self.minor_remembered_work::<false>() };
        for task in objects {
            let Work::Object(address) = task else { unreachable!() };
            let object = address as *mut u8;
            let info = self.type_info_by_id(unsafe { read_type_id(object, self.type_id_offset) });
            unsafe { scan_object(object, info, &mut *visit); }
        }
    }

    /// # Safety
    /// The heap is quiescent and no copy workers are running. Returned addresses
    /// remain valid through the ensuing minor collection.
    unsafe fn minor_remembered_work<const COUNT_SCAN: bool>(&self) -> (RememberedWork<'_>, usize) {
        let ns = self.nursery_state.as_ref().unwrap();
        let table = &ns.card_tables[self.from_idx.load(Ordering::Acquire)];
        // An empty remembered set has no tenured slots to enumerate. Keep
        // overlap metadata lazy: the next dirty collection extends it over all
        // initialized ranges accumulated since its previous update/reset.
        let mut dirty = table.iter_dirty().peekable();
        if dirty.peek().is_none() {
            return (RememberedWork::default(), 0);
        }
        let space = self.from_space();
        let used = space.used();
        let ranges = space.initialized_ranges_in(self.native_memory.registry_allocator());
        let mut starts = ns.object_indexes[self.from_idx.load(Ordering::Acquire)]
            .lock()
            .unwrap();
        unsafe {
            starts.update(space, &ranges, &self.type_table, table.card_size());
        }
        let mut objects = hashbrown::HashSet::with_hasher_in(std::hash::RandomState::new(), self.native_memory.registry_allocator());
        let mut scan_bytes = 0usize;
        for (card, addr) in dirty {
            let Some(start) = starts.get(card) else {
                continue;
            };
            let end = addr as usize + table.card_size();
            let mut offset = start;
            let mut region = ranges.partition_point(|range| range.end <= offset);
            while offset < used && region < ranges.len() {
                offset = offset.max(ranges[region].start);
                if offset >= ranges[region].end {
                    region += 1;
                    continue;
                }
                let obj = unsafe { space.base().add(offset) };
                if obj as usize >= end {
                    break;
                }
                let info = self.type_info_by_id(unsafe { read_type_id(obj, self.type_id_offset) });
                let len = match info.varlen {
                    VarLenKind::None => 0,
                    _ => unsafe { read_varlen_count(obj, info) },
                };
                let size = info.allocation_size(len);
                if obj as usize + size > addr as usize && objects.insert(obj as usize) {
                    if COUNT_SCAN {
                        scan_bytes = scan_bytes.saturating_add(traced_bytes(info, len));
                    }
                }
                let align = 1usize << info.align_log2;
                offset = (offset + size + align - 1) & !(align - 1);
            }
        }
        drop(starts);
        (RememberedWork::from_objects(objects), scan_bytes)
    }
    unsafe fn parallel_evacuate<P: PtrPolicy>(
        &self,
        extras: &[&dyn RootSource],
        minor: bool,
        initial: RememberedWork<'_>,
        remembered_scan_bytes: usize,
        nursery_live: Option<&NurseryLive<'_>>,
        policy: WorkerPolicy,
    ) {
        let channels = unsafe { ConditionalChannels::new(self, minor, nursery_live) };
        // Global backing slots are distinct and are always enrolled. Reserve
        // their known cardinality before scanning to avoid repeated rehashes.
        let mut roots = root_slots::RootSlots::with_counter(self.globals.len(), &self.native_memory.registry);
        let mut visit = |slot: *mut u64| {
            roots.insert(slot as usize);
        };
        unsafe { self.scan_collection_roots(&mut visit, minor); }
        if !extras.is_empty() {
            self.saw_extra_roots.store(true, Ordering::Relaxed);
        }
        for source in extras {
            unsafe { source.scan_roots(&mut visit); }
        }
        if let Some(ns) = self.nursery_state.as_ref().filter(|_| !minor) {
            unsafe {
                ns.nursery.walk_in(&self.type_table, self.native_memory.registry_allocator(), &mut |obj, info| {
                    if nursery_live.is_none_or(|live| live.contains(&(obj as usize))) {
                        scan_object(obj, info, &mut visit);
                    }
                });
            }
        }
        let workers = policy.workers(|| {
            let source_bytes = if minor {
                self.nursery_state.as_ref().unwrap().nursery.used()
            } else {
                self.from_used()
            };
            source_bytes.saturating_add(remembered_scan_bytes)
        });
        // Parallel cycles reuse helpers. Every participant completes before
        // space swap or nursery reset, regardless of selection policy.
        let queue = WorkQueue {
            workers,
            state: Mutex::new(State {
                queue: Ring::from_iter_in(
                    roots.into_iter().map(Work::Slot).chain(initial)
                        .chain(channels.initial_groups().map(Work::Channel)),
                    self.native_memory.registry_allocator(),
                ),
                active: 0,
            }),
            wake: Condvar::new(), sleepers: AtomicUsize::new(0),
        };
        let run = |_worker_id: usize| {
            // Contain job panics before the reusable worker pool can retain or
            // drop their payloads. Partial relocation has no rollback; abort
            // the whole process instead of resuming or waiting on failed jobs.
            abort_on_collection_panic(|| {
                worker_scratch::with(|scratch| {
                    scratch.prepare_batch();
                    let mut has_batch = queue.take(&mut scratch.batch);
                    if !has_batch { return; }
                    scratch.prepare_copy();
                    let worker_scratch::Scratch { batch, discovered, layouts, addresses, local } = scratch;
                    // Claims borrow the current heap and never enter reusable storage.
                    let mut pending = PendingCopies::with_allocator(self.native_memory.registry_allocator());
                    while has_batch {
                        for task in batch.drain(..) {
                            match task {
                                Work::Slot(slot) => unsafe {
                                    self.parallel_slot_mode_with_channels::<P>(
                                        slot as *mut u64,
                                        discovered,
                                        &mut pending,
                                        minor, Some(&channels),
                                    );
                                },
                                Work::Channel(index) => unsafe {
                                    channels.scan(index, &mut |slot| {
                                        self.parallel_slot_mode_with_channels::<P>(slot, discovered, &mut pending, minor, Some(&channels));
                                        self.publish_full_local_buffers::<P>(&queue, &mut pending, layouts, addresses, discovered, minor);
                                    });
                                },
                                Work::Object(addr) => unsafe {
                                    let obj = addr as *mut u8;
                                    let info =
                                        self.type_info_by_id(read_type_id(obj, self.type_id_offset));
                                    // A task batch of small fixed objects fits the
                                    // local buffers without per-field flush checks.
                                    // Stream larger scans between clean boundaries.
                                    let stream = info.varlen == VarLenKind::Values
                                        || info.value_field_count as usize + info.interior_ptrs.len()
                                            > MAX_SMALL_OBJECT_SLOTS;
                                    let nursery_edge = if stream {
                                        self.scan_parallel_task::<P, true>(
                                            obj, info, &queue, &mut pending, layouts,
                                            addresses, discovered, minor, &channels,
                                        )
                                    } else {
                                        self.scan_parallel_task::<P, false>(
                                            obj, info, &queue, &mut pending, layouts,
                                            addresses, discovered, minor, &channels,
                                        )
                                    };
                                    if !minor && nursery_edge {
                                        let ns = self.nursery_state.as_ref().unwrap();
                                        ns.card_tables[1 - self.from_idx.load(Ordering::Acquire)]
                                            .mark_dirty(obj);
                                    }
                                },
                            }
                            // A copy claim may add one conditional-root task. Flush
                            // between fast object scans to retain the buffer bound.
                            if channels.has_source_owners() {
                                unsafe { self.publish_full_local_buffers::<P>(&queue, &mut pending, layouts, addresses, discovered, minor); }
                            }
                        }
                        debug_assert!(discovered.len() + pending.len() <= LOCAL_BUFFER_CAPACITY);
                        let copied_bytes = unsafe {
                            self.publish_batch::<P>(
                                &mut pending,
                                layouts,
                                addresses,
                                discovered,
                                minor,
                            )
                        };
                        unsafe { self.resolve_published_retries::<P>(discovered, minor); }
                        has_batch = queue.continue_or_take(discovered, local, batch, copied_bytes);
                    }
                });
            });
        };
        if workers == 1 {
            run(0);
        } else {
            self.collector_workers
                .get_or_init(|| WorkerPool::with_allocator(crate::runtime::metadata_allocator::MetadataAllocator::new(&self.native_memory.registry)))
                .run(workers - 1, &run);
        }
    }

    #[cfg(test)]
    unsafe fn parallel_slot<P: PtrPolicy>(&self, slot: *mut u64, work: &mut worker_scratch::Buffer<Work>) {
        // Direct contention test has no queue to retry BUSY slots, so explicitly
        // complete its own claims and retry until this slot is relocated.
        let mut pending = PendingCopies::default();
        let mut layouts = worker_scratch::test_buffer([]);
        let mut addresses = worker_scratch::test_buffer([]);
        loop {
            unsafe {
                self.parallel_slot_mode::<P>(slot, work, &mut pending, false);
                self.publish_batch::<P>(&mut pending, &mut layouts, &mut addresses, work, false);
            }
            let ptr = P::try_decode_ptr(unsafe { slot.read() }).unwrap();
            if self.to_space().contains(ptr) {
                break;
            }
            work.retain(|task| !matches!(task, Work::Slot(_)));
            std::thread::yield_now();
        }
    }

    #[cfg(test)]
    unsafe fn parallel_slot_mode<'a, P: PtrPolicy>(
        &'a self, slot: *mut u64, work: &mut worker_scratch::Buffer<Work>, pending: &mut PendingCopies<'a, impl Allocator>, minor: bool,
    ) { unsafe { self.parallel_slot_mode_with_channels::<P>(slot, work, pending, minor, None); } }

    unsafe fn parallel_slot_mode_with_channels<'a, P: PtrPolicy>(
        &'a self,
        slot: *mut u64,
        work: &mut worker_scratch::Buffer<Work>,
        pending: &mut PendingCopies<'a, impl Allocator>,
        minor: bool,
        channels: Option<&ConditionalChannels<'_>>,
    ) {
        let atomic_slot = unsafe { &*slot.cast::<AtomicU64>() };
        let bits = atomic_slot.load(Ordering::Relaxed);
        let Some(old) = P::try_decode_ptr(bits) else {
            return;
        };
        if if minor {
            !self.is_nursery(old)
        } else {
            self.is_nursery(old) || !self.from_space().contains(old)
        } {
            return;
        }
        let header = unsafe { &*(old.add(self.header.forwarding_offset) as *const AtomicU64) };
        loop {
            let original = header.load(Ordering::Acquire);
            if original == FORWARDING_BIT {
                // Never wait while holding other claims. Their publication is
                // deferred until the batch is reserved; waiting here could form
                // an ownership cycle between workers. Retry this slot as work.
                work.push(Work::Slot(slot as usize));
                return;
            }
            if original & FORWARDING_BIT != 0 {
                atomic_slot.store(
                    P::encode_ptr((original & !FORWARDING_BIT) as *mut u8),
                    Ordering::Relaxed,
                );
                return;
            }
            let type_id = unsafe { self.header.type_id_from_word(old, original) };
            assert!(
                (type_id as usize) < self.type_table.len(),
                "GC precise-layout violation: traced slot points at {old:p} whose header type_id={type_id} is out of range (type_table len {}). A non-pointer reached a traced slot — fix the layout/rooting; do not mask it.",
                self.type_table.len()
            );
            let info = self.type_info_by_id(type_id);
            if header
                .compare_exchange(
                    original,
                    FORWARDING_BIT,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }
            // Construct the claim guard before any fallible layout/allocation.
            let claim = Claim {
                header,
                original,
                published: false,
            };
            let len = match info.varlen {
                VarLenKind::None => 0,
                _ => unsafe { read_varlen_count(old, info) },
            };
            pending.push(PendingCopy {
                old,
                slot,
                size: info.allocation_size(len),
                align_log2: info.align_log2,
                needs_scan: info.value_field_count != 0
                    || !info.interior_ptrs.is_empty()
                    || (info.varlen == VarLenKind::Values && len != 0),
                claim,
            });
            if let Some(group) = channels.and_then(|channels| channels.source_group(old, type_id)) {
                work.push(Work::Channel(group));
            }
            return;
        }
    }

    /// Specialize the callback so small fixed scans have no streaming checks
    /// or publication-buffer arguments live across individual field visits.
    unsafe fn scan_parallel_task<'a, P: PtrPolicy, const STREAM: bool>(
        &'a self,
        obj: *mut u8,
        info: &TypeInfo,
        queue: &WorkQueue<impl Allocator + Clone>,
        pending: &mut PendingCopies<'a, impl Allocator>,
        layouts: &mut worker_scratch::Buffer<(usize, usize)>,
        addresses: &mut worker_scratch::Buffer<*mut u8>,
        discovered: &mut worker_scratch::Buffer<Work>,
        minor: bool,
        channels: &ConditionalChannels<'_>,
    ) -> bool {
        if STREAM {
            unsafe { self.publish_local_work::<P>(queue, pending, layouts, addresses, discovered, minor); }
        }
        let mut nursery_edge = false;
        unsafe { scan_object(obj, info, |slot| {
            self.parallel_slot_mode_with_channels::<P>(slot, discovered, pending, minor, Some(channels));
            if STREAM {
                self.publish_full_local_buffers::<P>(queue, pending, layouts, addresses, discovered, minor);
            }
            if let Some(ptr) = P::try_decode_ptr((&*slot.cast::<AtomicU64>()).load(Ordering::Relaxed)) {
                nursery_edge |= self.is_nursery(ptr);
            }
        }); }
        if STREAM {
            unsafe { self.publish_local_work::<P>(queue, pending, layouts, addresses, discovered, minor); }
        }
        nursery_edge
    }

    /// Keep per-worker claims and newly discovered work bounded even when one
    /// variable-length object has arbitrarily many traced fields.
    #[inline]
    unsafe fn publish_full_local_buffers<P: PtrPolicy>(
        &self,
        queue: &WorkQueue<impl Allocator + Clone>,
        pending: &mut PendingCopies<'_, impl Allocator>,
        layouts: &mut worker_scratch::Buffer<(usize, usize)>,
        addresses: &mut worker_scratch::Buffer<*mut u8>,
        discovered: &mut worker_scratch::Buffer<Work>,
        minor: bool,
    ) {
        if pending.len() + discovered.len() >= MAX_LOCAL_WORK {
            unsafe { self.publish_local_work::<P>(queue, pending, layouts, addresses, discovered, minor); }
        }
        debug_assert!(pending.len() + discovered.len() < MAX_LOCAL_WORK);
    }

    unsafe fn publish_local_work<P: PtrPolicy>(
        &self,
        queue: &WorkQueue<impl Allocator + Clone>,
        pending: &mut PendingCopies<'_, impl Allocator>,
        layouts: &mut worker_scratch::Buffer<(usize, usize)>,
        addresses: &mut worker_scratch::Buffer<*mut u8>,
        discovered: &mut worker_scratch::Buffer<Work>,
        minor: bool,
    ) {
        debug_assert!(pending.len() + discovered.len() <= LOCAL_BUFFER_CAPACITY);
        unsafe { self.publish_batch::<P>(pending, layouts, addresses, discovered, minor); }
        unsafe { self.resolve_published_retries::<P>(discovered, minor); }
        queue.publish(discovered);
    }

    /// Complete retries whose copy owner has already published. A still-BUSY
    /// owner retains its queued retry; never wait while another job owns it.
    /// # Safety
    /// The world is stopped and every slot remains live through evacuation.
    unsafe fn resolve_published_retries<P: PtrPolicy>(&self, work: &mut worker_scratch::Buffer<Work>, minor: bool) {
        work.retain(|task| {
            let Work::Slot(slot) = *task else {
                return true;
            };
            let slot = unsafe { &*(slot as *const AtomicU64) };
            let Some(old) = P::try_decode_ptr(slot.load(Ordering::Relaxed)) else {
                return false;
            };
            if if minor {
                !self.is_nursery(old)
            } else {
                self.is_nursery(old) || !self.from_space().contains(old)
            } {
                return false;
            }
            let header = unsafe { &*old.add(self.header.forwarding_offset).cast::<AtomicU64>() };
            let word = header.load(Ordering::Acquire);
            if word == FORWARDING_BIT || word & FORWARDING_BIT == 0 {
                return true;
            }
            slot.store(
                P::encode_ptr((word & !FORWARDING_BIT) as *mut u8),
                Ordering::Relaxed,
            );
            false
        });
    }

    unsafe fn publish_batch<P: PtrPolicy>(
        &self,
        pending: &mut PendingCopies<'_, impl Allocator>,
        layouts: &mut worker_scratch::Buffer<(usize, usize)>,
        addresses: &mut worker_scratch::Buffer<*mut u8>,
        work: &mut worker_scratch::Buffer<Work>,
        minor: bool,
    ) -> usize {
        if pending.is_empty() {
            return 0;
        }
        layouts.clear();
        layouts.extend(pending.iter().map(|copy| (copy.size, 1usize << copy.align_log2)));
        let destination = if minor {
            self.from_space()
        } else {
            self.to_space()
        };
        assert!(
            unsafe { destination.reserve_batch(layouts, addresses) },
            "to-space exhausted during parallel collection"
        );
        // Drain each segment directly: no prefix migration and no combined
        // iterator branch in the spilled batch's per-copy loop.
        let pending_copies::Drain { inline, spill } = pending.drain();
        let prefix = inline.len();
        let mut bytes = 0;
        if prefix != 0 {
            bytes += unsafe { self.publish_copies::<P>(inline, &addresses[..prefix], work) };
        }
        if spill.len() != 0 {
            bytes += unsafe { self.publish_copies::<P>(spill, &addresses[prefix..], work) };
        }
        bytes
    }

    #[inline]
    unsafe fn publish_copies<'a, P: PtrPolicy>(
        &self,
        copies: impl Iterator<Item = PendingCopy<'a>>,
        addresses: &[*mut u8],
        work: &mut worker_scratch::Buffer<Work>,
    ) -> usize {
        let mut copied_bytes = 0;
        for (mut copy, &new) in copies.zip(addresses.iter()) {
            copied_bytes += copy.size;
            unsafe {
                // Exclude the claimed header from memcpy: all accesses to the
                // old word are atomic while collector workers run.
                std::ptr::copy_nonoverlapping(copy.old, new, self.header.forwarding_offset);
                new.add(self.header.forwarding_offset)
                    .cast::<u64>()
                    .write(copy.claim.original);
                let tail = self.header.forwarding_offset + 8;
                std::ptr::copy_nonoverlapping(copy.old.add(tail), new.add(tail), copy.size - tail);
            }
            copy.claim
                .header
                .store(new as u64 | FORWARDING_BIT, Ordering::Release);
            copy.claim.published = true;
            unsafe {
                (&*copy.slot.cast::<AtomicU64>()).store(P::encode_ptr(new), Ordering::Relaxed);
            }
            // Conditional native edges were queued when claiming the owner.
            // A payload without traced slots has no further work to publish.
            if copy.needs_scan {
                work.push(Work::Object(new as usize));
            }
        }
        copied_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{Compact, Full, IdentityPtrPolicy, RootSet};
    use std::sync::Barrier;

    #[test]
    fn contended_evacuation_has_one_copy_and_preserves_header_and_payload() {
        for (full, fields) in [(false, 0), (false, 1), (true, 0), (true, 1)] {
            let header_size = if full { Full::SIZE } else { Compact::SIZE };
            let info = TypeInfo::for_header(header_size)
                .with_type_id(1)
                .with_varlen_bytes(fields);
            let heap = if full {
                Heap::new::<Full>(4 * 1024 * 1024, vec![info.clone().with_type_id(0), info.clone()])
            } else {
                Heap::new::<Compact>(4 * 1024 * 1024, vec![info.clone().with_type_id(0), info.clone()])
            };
            let old = if full {
                unsafe { heap.alloc_obj::<Full>(&info, 1024 * 1024) }
            } else {
                unsafe { heap.alloc_obj::<Compact>(&info, 1024 * 1024) }
            };
            unsafe {
                std::ptr::write_bytes(old.add(info.varlen_element_offset(0)), 0xa5, 1024 * 1024);
            }
            let start = Barrier::new(8);
            let old_address = old as usize;
            let results = std::thread::scope(|scope| {
                let tasks: Vec<_> = (0..8)
                    .map(|_| {
                        scope.spawn(|| {
                            let mut slot = old_address as u64;
                            let mut work = worker_scratch::test_buffer([]);
                            start.wait();
                            unsafe {
                                heap.parallel_slot::<IdentityPtrPolicy>(&mut slot, &mut work);
                            }
                            (slot, work.len())
                        })
                    })
                    .collect();
                tasks
                    .into_iter()
                    .map(|t| t.join().unwrap())
                    .collect::<Vec<_>>()
            });
            assert!(results.iter().all(|r| r.0 == results[0].0));
            assert_eq!(results.iter().map(|r| r.1).sum::<usize>(), usize::from(fields != 0));
            assert_eq!(heap.to_space().used(), info.allocation_size(1024 * 1024));
            let copy = results[0].0 as *mut u8;
            assert_eq!(unsafe { read_type_id(copy, heap.type_id_offset) }, 1);
            assert!(
                unsafe {
                    std::slice::from_raw_parts(copy.add(info.varlen_element_offset(0)), 1024 * 1024)
                }
                .iter()
                .all(|&b| b == 0xa5)
            );
        }
    }

    #[test]
    fn parallel_major_preserves_shared_cycles_and_exact_live_size() {
        for full in [false, true] {
            let header_size = if full { Full::SIZE } else { Compact::SIZE };
            let info = TypeInfo::for_header(header_size)
                .with_fields(2)
                .with_raw_bytes(8);
            let heap = if full {
                Heap::new::<Full>(2 * 1024 * 1024, vec![info.clone()])
            } else {
                Heap::new::<Compact>(2 * 1024 * 1024, vec![info.clone()])
            };
            let count = 16000;
            let objects: Vec<_> = (0..count)
                .map(|_| {
                    if full {
                        unsafe { heap.alloc_obj::<Full>(&info, 0) }
                    } else {
                        unsafe { heap.alloc_obj::<Compact>(&info, 0) }
                    }
                })
                .collect();
            let mut roots = RootSet::new();
            for (i, &obj) in objects.iter().enumerate() {
                unsafe {
                    if full { obj.add(Full::ENUM_TAG_OFFSET).cast::<u32>().write(u32::MAX - i as u32); }
                    obj.add(info.value_field_offset(0))
                        .cast::<u64>()
                        .write(objects[(i + 1) % count] as u64);
                    obj.add(info.value_field_offset(1))
                        .cast::<u64>()
                        .write(objects[0] as u64);
                    obj.add(info.raw_data_offset())
                        .cast::<u64>()
                        .write(i as u64);
                }
                roots.add(obj as u64);
            }
            // Duplicate registration intentionally exercises slot deduplication.
            for _ in 0..3 {
                unsafe {
                    heap.collect::<IdentityPtrPolicy>(&[&roots, &roots]);
                }
                assert_eq!(heap.from_used(), count * info.allocation_size(0));
                for i in 0..count {
                    let obj = roots.get(i) as *mut u8;
                    unsafe {
                        assert_eq!(
                            obj.add(info.value_field_offset(0)).cast::<u64>().read(),
                            roots.get((i + 1) % count)
                        );
                        if full { assert_eq!(obj.add(Full::ENUM_TAG_OFFSET).cast::<u32>().read(), u32::MAX - i as u32); }
                        assert_eq!(
                            obj.add(info.value_field_offset(1)).cast::<u64>().read(),
                            roots.get(0)
                        );
                        assert_eq!(
                            obj.add(info.raw_data_offset()).cast::<u64>().read(),
                            i as u64
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn queue_waits_for_active_work_to_publish_more_tasks() {
        let q = WorkQueue {
            workers: 2,
            state: Mutex::new(State {
                queue: Ring::from([Work::Slot(1)]),
                active: 0,
            }),
            wake: Condvar::new(), sleepers: AtomicUsize::new(0),
        };
        let mut batch = worker_scratch::test_buffer([]);
        assert!(q.take(&mut batch));
        assert_eq!(batch.len(), 1);
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                let mut batch = worker_scratch::test_buffer([]);
                assert!(q.take(&mut batch), "active producer prevents termination");
                batch
            });
            q.finish(&mut worker_scratch::test_buffer([Work::Object(2)]));
            assert!(matches!(waiter.join().unwrap()[0], Work::Object(2)));
        });
        q.finish(&mut worker_scratch::test_buffer([]));
        assert!(!q.take(&mut batch));
    }
    #[test]
    #[ignore = "collector throughput measurement; run explicitly in release"]
    fn parallel_copy_throughput() {
        measure_collection(4095, 16384);
    }

    #[test]
    #[ignore = "small-object collector throughput measurement; run explicitly in release"]
    fn parallel_small_object_throughput() {
        measure_collection(262143, 0);
    }

    fn measure_collection(count: usize, bytes: usize) {
        let info = TypeInfo::for_header(Full::SIZE).with_varlen_bytes(2);
        let heap = Heap::new::<Full>(96 * 1024 * 1024, vec![info.clone()]);
        let objects: Vec<_> = (0..count)
            .map(|_| unsafe { heap.alloc_obj::<Full>(&info, bytes) })
            .collect();
        for (i, &obj) in objects.iter().enumerate() {
            for field in 0..2 {
                let child = 2 * i + 1 + field;
                unsafe {
                    obj.add(info.value_field_offset(field as u16))
                        .cast::<u64>()
                        .write(if child < count {
                            objects[child] as u64
                        } else {
                            0
                        });
                }
            }
        }
        let mut root = RootSet::new();
        root.add(objects[0] as u64);
        // Longer explicitly requested runs support sampling a single live
        // collector process. Ordinary benchmark artifacts keep nine events.
        let collections = std::env::var("GCR_GC_PROFILE_COLLECTIONS")
            .map(|value| value.parse::<usize>().expect("invalid profile collection count"))
            .unwrap_or(9);
        assert!((3..=10_000).contains(&collections));
        for _ in 0..collections {
            unsafe {
                heap.collect::<IdentityPtrPolicy>(&[&root]);
            }
            assert_eq!(heap.from_used(), count * info.allocation_size(bytes));
        }
        let raw_pauses: Vec<_> = heap.gc_events().into_iter().map(|e| e.pause_ns).collect();
        let mut pauses: Vec<_> = raw_pauses.iter().copied().skip(2).collect();
        pauses.sort_unstable();
        eprintln!(
            "copy benchmark: workers={} live_bytes={} median_pause_ns={} raw_pause_ns={raw_pauses:?}",
            std::env::var("GCR_GC_WORKERS").unwrap_or_default(),
            heap.from_used(),
            pauses[pauses.len() / 2]
        );
    }
    struct Tagged;
    impl PtrPolicy for Tagged {
        fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
            if bits & 7 == 3 {
                Some((bits & !7) as *mut u8)
            } else {
                None
            }
        }
        fn encode_ptr(ptr: *mut u8) -> u64 {
            ptr as u64 | 3
        }
    }

    #[test]
    fn minor_deduplicates_multicard_objects_and_promotes_tagged_cycles_once() {
        let node = TypeInfo::for_header(Full::SIZE)
            .with_type_id(0)
            .with_fields(2);
        let array = TypeInfo::for_header(Full::SIZE)
            .with_type_id(1)
            .with_varlen_values(0);
        let heap = Heap::new_generational::<Full>(1024 * 1024, 2 * 1024 * 1024, vec![node.clone(), array.clone()]);
        let (thread, _) = heap.register_thread();
        let count = 12000;
        let nodes: Vec<_> = (0..count)
            .map(|_| unsafe { heap.alloc_nursery_obj::<Full>(&node, 0) })
            .collect();
        for (i, &obj) in nodes.iter().enumerate() {
            unsafe {
                obj.add(node.value_field_offset(0))
                    .cast::<u64>()
                    .write(Tagged::encode_ptr(nodes[(i + 1) % count]));
                obj.add(node.value_field_offset(1))
                    .cast::<u64>()
                    .write(Tagged::encode_ptr(nodes[0]));
            }
        }
        let old = unsafe { heap.alloc_obj::<Full>(&array, count) };
        for (i, &child) in nodes.iter().enumerate() {
            let slot = unsafe { old.add(array.varlen_element_offset(i)) };
            unsafe {
                slot.cast::<u64>().write(Tagged::encode_ptr(child));
            }
            heap.mark_card_dirty(slot);
        }
        unsafe { heap.globals.add(Tagged::encode_ptr(old)) };
        let initial = heap.from_used();
        unsafe {
            heap.mutator_triggered_minor_gc::<Tagged>(&thread);
        }
        assert_eq!(heap.from_used(), initial + count * node.allocation_size(0));
        let moved: Vec<_> = (0..count)
            .map(|i| unsafe { old.add(array.varlen_element_offset(i)).cast::<u64>().read() })
            .collect();
        for i in 0..count {
            let obj = Tagged::try_decode_ptr(moved[i]).unwrap();
            assert!(heap.is_tenured(obj));
            unsafe {
                assert_eq!(
                    obj.add(node.value_field_offset(0)).cast::<u64>().read(),
                    moved[(i + 1) % count]
                );
                assert_eq!(
                    obj.add(node.value_field_offset(1)).cast::<u64>().read(),
                    moved[0]
                );
            }
        }
        // Major must preserve the same tagging and reference identity too.
        unsafe {
            heap.mutator_triggered_gc::<Tagged>(&thread);
        }
        assert_eq!(heap.from_used(), initial + count * node.allocation_size(0));
        unsafe { heap.safe_deregister_thread(&thread) };
    }
    #[test]
    fn crossing_worker_claims_are_deferred_and_publish_without_wait_cycles() {
        let info = TypeInfo::for_header(Full::SIZE).with_fields(1);
        let heap = Heap::new::<Full>(4096, vec![info.clone()]);
        let old = [
            unsafe { heap.alloc_obj::<Full>(&info, 0) } as usize,
            unsafe { heap.alloc_obj::<Full>(&info, 0) } as usize,
        ];
        let barrier = Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..2)
                .map(|id| {
                    scope.spawn({
                        let heap = &heap;
                        let barrier = &barrier;
                        move || {
                            let mut own = old[id] as u64;
                            let mut other = old[1 - id] as u64;
                            let mut work = worker_scratch::test_buffer([]);
                            let mut pending = PendingCopies::default();
                            let mut layouts = worker_scratch::test_buffer([]);
                            let mut addresses = worker_scratch::test_buffer([]);
                            unsafe {
                                heap.parallel_slot_mode::<IdentityPtrPolicy>(
                                    &mut own,
                                    &mut work,
                                    &mut pending,
                                    false,
                                );
                            }
                            barrier.wait();
                            unsafe {
                                heap.parallel_slot_mode::<IdentityPtrPolicy>(
                                    &mut other,
                                    &mut work,
                                    &mut pending,
                                    false,
                                );
                            }
                            assert!(matches!(work.as_slice(), [Work::Slot(_)]));
                            // Both workers own a copy and need the other's copy. The
                            // second slot must be deferred rather than blocking either.
                            barrier.wait();
                            unsafe {
                                heap.publish_batch::<IdentityPtrPolicy>(
                                    &mut pending,
                                    &mut layouts,
                                    &mut addresses,
                                    &mut work,
                                    false,
                                );
                            }
                            barrier.wait();
                            unsafe {
                                heap.parallel_slot_mode::<IdentityPtrPolicy>(
                                    &mut other,
                                    &mut work,
                                    &mut pending,
                                    false,
                                );
                            }
                            assert!(pending.is_empty());
                            (own, other)
                        }
                    })
                })
                .collect();
            tasks
                .into_iter()
                .map(|task| task.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results[0].0, results[1].1);
        assert_eq!(results[1].0, results[0].1);
        assert_eq!(heap.to_space().used(), 2 * info.allocation_size(0));
    }
}

#[cfg(test)]
mod queue_model_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn verify_local_frontier() {
        let queue = WorkQueue {
            workers: 1,
            state: Mutex::new(State { queue: Ring::with_capacity(LOCAL_BUFFER_CAPACITY), active: 1 }),
            wake: Condvar::new(), sleepers: AtomicUsize::new(0),
        };
        let mut batch = worker_scratch::Buffer::with_capacity_in(MAX_WORK_BATCH, worker_scratch::ScratchAllocator);
        let mut local = worker_scratch::Buffer::with_capacity_in(MAX_WORK_BATCH, worker_scratch::ScratchAllocator);
        let mut discovered = worker_scratch::Buffer::with_capacity_in(LOCAL_BUFFER_CAPACITY, worker_scratch::ScratchAllocator);
        discovered.extend((0..LOCAL_BUFFER_CAPACITY).map(Work::Object));
        drop(queue.state.lock().unwrap());
        // Apple platforms lazily allocate native condition-variable state.
        // Observe local-buffer reuse after preparing both monitor primitives.
        queue.wake.notify_all();
        crate::runtime::allocation_probe::start();
        assert!(queue.continue_or_take(&mut discovered, &mut local, &mut batch, 0));
        let traffic = crate::runtime::allocation_probe::finish_bytes();
        assert_eq!(traffic, (0, 0, 0, 0));
        assert_eq!(batch.len(), MAX_WORK_BATCH);
        assert_eq!(local.len(), MAX_WORK_BATCH);
        assert!(discovered.is_empty());
        let state = queue.state.lock().unwrap();
        assert_eq!(state.active, 1, "retained work must keep its producer active");
        let mut all: Vec<_> = batch.iter().chain(&local).chain(&state.queue).map(|task| {
            let Work::Object(value) = task else { panic!("unexpected task") }; *value
        }).collect();
        all.sort_unstable();
        assert_eq!(all, (0..LOCAL_BUFFER_CAPACITY).collect::<Vec<_>>());
    }

    fn verify_parked_donation() {
        let queue = WorkQueue {
            workers: 2,
            state: Mutex::new(State { queue: [Work::Slot(0)].into(), active: 0 }),
            wake: Condvar::new(), sleepers: AtomicUsize::new(0),
        };
        let mut batch = worker_scratch::test_buffer([]);
        assert!(queue.take(&mut batch));
        let mut local = worker_scratch::test_buffer([Work::Object(42)]);
        let mut discovered = worker_scratch::test_buffer((100..100 + MAX_WORK_BATCH).map(Work::Object));
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let queue = &queue;
            scope.spawn(move || {
                let mut donated = worker_scratch::test_buffer([]);
                assert!(queue.take(&mut donated));
                assert!(matches!(donated.as_slice(), [Work::Object(42)]));
                queue.finish(&mut worker_scratch::test_buffer([]));
                sent.send(()).unwrap();
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while queue.sleepers.load(Ordering::Acquire) == 0 {
                assert!(std::time::Instant::now() < deadline, "helper never parked");
                std::thread::yield_now();
            }
            assert!(queue.continue_or_take(&mut discovered, &mut local, &mut batch, 0));
            assert!(local.is_empty() && discovered.is_empty());
            assert_eq!(batch.len(), MAX_WORK_BATCH);
            assert!(matches!(batch.first(), Some(Work::Object(100))));
            received.recv_timeout(std::time::Duration::from_secs(2)).expect("parked helper did not receive retained work");
            assert_eq!(queue.state.lock().unwrap().active, 1);
            queue.finish(&mut worker_scratch::test_buffer([]));
        });
        assert!(!queue.take(&mut batch));
        assert_eq!(queue.sleepers.load(Ordering::Acquire), 0);
    }

    fn verify_narrow_frontier() {
        let queue = WorkQueue {
            workers: 2,
            state: Mutex::new(State { queue: [Work::Slot(0)].into(), active: 0 }),
            wake: Condvar::new(), sleepers: AtomicUsize::new(0),
        };
        let mut batch = worker_scratch::test_buffer([]);
        assert!(queue.take(&mut batch));
        let mut local = worker_scratch::test_buffer([]);
        let mut discovered = worker_scratch::test_buffer([Work::Object(1), Work::Object(2)]);
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let queue = &queue;
            scope.spawn(move || {
                let mut donated = worker_scratch::test_buffer([]);
                assert!(!queue.take(&mut donated), "narrow frontier was fragmented");
                sent.send(()).unwrap();
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while queue.sleepers.load(Ordering::Acquire) == 0 {
                assert!(std::time::Instant::now() < deadline, "helper never parked");
                std::thread::yield_now();
            }
            assert!(queue.continue_or_take(&mut discovered, &mut local, &mut batch, 0));
            assert_eq!(batch.len(), 2);
            assert!(local.is_empty() && discovered.is_empty());
            {
                let state = queue.state.lock().unwrap();
                assert_eq!(state.active, 1);
                assert!(state.queue.is_empty());
            }
            queue.finish(&mut worker_scratch::test_buffer([]));
            received.recv_timeout(std::time::Duration::from_secs(2)).expect("helper did not observe quiescence");
        });
        assert_eq!(queue.sleepers.load(Ordering::Acquire), 0);
    }

    #[test]
    fn collector_queue_visits_chain_and_branching_graphs_once_and_terminates() {
        const HELPER: &str = "GCR_QUEUE_MODEL_HELPER";
        if let Ok(model) = std::env::var(HELPER) {
            let (shape, workers) = model.split_once(':').unwrap();
            let workers: usize = workers.parse().unwrap();
            if shape == "local" { verify_local_frontier(); return; }
            if shape == "donation" { verify_parked_donation(); return; }
            if shape == "narrow" { verify_narrow_frontier(); return; }
            let count = if shape == "chain" { 2048 } else { 8191 };
            let visited: Vec<_> = (0..count).map(|_| AtomicUsize::new(0)).collect();
            let queue = WorkQueue { workers, state: Mutex::new(State {
                queue: [Work::Slot(0)].into_iter().collect(), active: 0,
            }), wake: Condvar::new(), sleepers: AtomicUsize::new(0) };
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    let queue = &queue;
                    let visited = &visited;
                    scope.spawn(move || {
                        let mut batch = worker_scratch::test_buffer([]);
                        let mut discovered = worker_scratch::test_buffer([]);
                        let mut local = worker_scratch::test_buffer([]);
                        let mut has_batch = queue.take(&mut batch);
                        while has_batch {
                            for work in batch.drain(..) {
                                let node = match work { Work::Slot(node) | Work::Object(node) => node, Work::Channel(_) => unreachable!("synthetic queue task") };
                                assert_eq!(visited[node].fetch_add(1, Ordering::Relaxed), 0);
                                if node % 127 == 0 { std::thread::yield_now(); }
                                if shape == "chain" {
                                    if node + 1 < count { discovered.push(Work::Object(node + 1)); }
                                } else if shape == "star" {
                                    if node == 0 {
                                        discovered.extend((1..count).map(Work::Slot));
                                    }
                                } else {
                                    for next in [node * 2 + 1, node * 2 + 2] {
                                        if next < count { discovered.push(Work::Object(next)); }
                                    }
                                }
                            }
                            let copied_bytes = discovered.len() * 40;
                            has_batch = queue.continue_or_take(&mut discovered, &mut local, &mut batch, copied_bytes);
                        }
                    });
                }
            });
            assert!(visited.iter().all(|n| n.load(Ordering::Relaxed) == 1));
            let state = queue.state.lock().unwrap();
            assert_eq!(state.active, 0);
            assert!(state.queue.is_empty());
            return;
        }
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        for shape in ["chain", "tree", "star", "local", "donation", "narrow"] {
            for workers in [1, 2, 4, 8] {
                let mut child = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "gc::heap::parallel::queue_model_tests::collector_queue_visits_chain_and_branching_graphs_once_and_terminates"])
                    .env(HELPER, format!("{shape}:{workers}")).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
                let deadline = Instant::now() + Duration::from_secs(15);
                while child.try_wait().unwrap().is_none() {
                    if Instant::now() >= deadline {
                        child.kill().unwrap();
                        let output = child.wait_with_output().unwrap();
                        panic!("{shape}/{workers}: queue lost work or termination wakeup: {output:?}");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let output = child.wait_with_output().unwrap();
                assert!(output.status.success(), "{shape}/{workers}: {output:?}");
            }
        }
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use crate::gc::{Full, IdentityPtrPolicy};

    #[test]
    fn collector_mid_scan_publication_keeps_producer_active_until_completion() {
        let queue = WorkQueue {
            workers: 2,
            state: Mutex::new(State { queue: [Work::Slot(1)].into(), active: 0 }),
            wake: Condvar::new(), sleepers: AtomicUsize::new(0),
        };
        let mut first = worker_scratch::test_buffer([]);
        let mut second = worker_scratch::test_buffer([]);
        assert!(queue.take(&mut first));
        queue.publish(&mut worker_scratch::test_buffer([Work::Slot(2)]));
        assert!(queue.take(&mut second));
        queue.finish(&mut worker_scratch::test_buffer([]));
        // The second batch is done, but the first producer still owns a scan.
        assert_eq!(queue.state.lock().unwrap().active, 1);
        queue.publish(&mut worker_scratch::test_buffer([Work::Slot(3)]));
        assert!(queue.take(&mut second));
        queue.finish(&mut worker_scratch::test_buffer([]));
        queue.finish(&mut worker_scratch::test_buffer([]));
        assert!(!queue.take(&mut first));
        assert_eq!(queue.state.lock().unwrap().active, 0);
    }

    #[test]
    fn collector_wide_varlen_cycles_preserve_aliases_and_exact_live_size() {
        const HELPER: &str = "GCR_WIDE_GC_HELPER";
        let Ok(mode) = std::env::var(HELPER) else {
            use std::process::{Command, Stdio};
            use std::time::{Duration, Instant};
            for workers in [1, 2, 4, 8] {
                for mode in ["major", "minor"] {
                    let mut child = Command::new(std::env::current_exe().unwrap())
                        .args(["--exact", "gc::heap::parallel::streaming_tests::collector_wide_varlen_cycles_preserve_aliases_and_exact_live_size"])
                        .env(HELPER, mode).env("GCR_GC_WORKERS", workers.to_string())
                        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
                    let deadline = Instant::now() + Duration::from_secs(15);
                    while child.try_wait().unwrap().is_none() {
                        if Instant::now() >= deadline {
                            child.kill().unwrap();
                            let output = child.wait_with_output().unwrap();
                            panic!("wide graph hung ({mode}/{workers}): {output:?}");
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let output = child.wait_with_output().unwrap();
                    assert!(output.status.success(), "{mode}/{workers}: {output:?}");
                }
            }
            return;
        };
        let parent_info = TypeInfo::for_header(Full::SIZE).with_varlen_values(0);
        let child_info = TypeInfo::for_header(Full::SIZE).with_type_id(1)
            .with_fields(1).with_raw_bytes(8);
        let heap = Heap::new_generational::<Full>(4 << 20, 4 << 20, vec![parent_info.clone(), child_info.clone()]);
        let fields = 65536;
        let count = 8192;
        let allocate = |info, len| unsafe {
            if mode == "minor" { heap.alloc_nursery_obj::<Full>(info, len) }
            else { heap.alloc_obj::<Full>(info, len) }
        };
        let parent = allocate(&parent_info, fields);
        assert!(!parent.is_null());
        let children: Vec<_> = (0..count).map(|index| {
            let child = allocate(&child_info, 0);
            assert!(!child.is_null());
            unsafe {
                child.add(child_info.value_field_offset(0)).cast::<u64>().write(parent as u64);
                child.add(child_info.raw_data_offset()).cast::<u64>().write(index as u64);
            }
            child
        }).collect();
        for index in 0..fields {
            unsafe { parent.add(parent_info.varlen_element_offset(index)).cast::<u64>()
                .write(children[index % count] as u64); }
        }
        let root = unsafe { heap.globals.add(parent as u64) };
        let (state, _) = heap.register_thread();
        for pass in 0..3 {
            let previous = heap.globals.get(root);
            unsafe {
                if mode == "minor" && pass == 0 {
                    heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state);
                } else { heap.mutator_triggered_gc::<IdentityPtrPolicy>(&state); }
            }
            let parent = heap.globals.get(root) as *mut u8;
            assert_ne!(parent as u64, previous);
            assert_eq!(unsafe { read_varlen_count(parent, &parent_info) }, fields);
            assert_eq!(heap.from_used(), parent_info.allocation_size(fields) + count * child_info.allocation_size(0));
            for index in 0..fields {
                unsafe {
                    let child = parent.add(parent_info.varlen_element_offset(index)).cast::<*mut u8>().read();
                    assert!(heap.from_space().contains(child));
                    assert_eq!(child.add(child_info.raw_data_offset()).cast::<u64>().read(), (index % count) as u64);
                    assert_eq!(child.add(child_info.value_field_offset(0)).cast::<u64>().read(), parent as u64);
                    let canonical = parent.add(parent_info.varlen_element_offset(index % count)).cast::<*mut u8>().read();
                    assert_eq!(child, canonical);
                }
            }
        }
        unsafe { heap.safe_deregister_thread(&state); }
    }
}
