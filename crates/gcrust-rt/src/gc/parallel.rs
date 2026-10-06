//! Parallel evacuation with exclusive copy ownership and quiescence detection.
//! All mutators are stopped throughout this module's execution.
use super::*;
use std::collections::{HashSet, VecDeque};
use std::sync::Condvar;
use std::sync::atomic::AtomicU64;
#[path = "worker_pool.rs"]
mod worker_pool;
pub(super) use worker_pool::WorkerPool;

#[derive(Clone, Copy)]
enum Work {
    Slot(usize),
    Object(usize),
}
struct State {
    queue: VecDeque<Work>,
    active: usize,
    failed: bool,
}
struct WorkQueue {
    state: Mutex<State>,
    wake: Condvar,
}
impl WorkQueue {
    fn take(&self, batch: &mut Vec<Work>) -> bool {
        batch.clear();
        let mut s = self.state.lock().unwrap();
        loop {
            if s.failed {
                return false;
            }
            if !s.queue.is_empty() {
                s.active += 1;
                let n = s.queue.len().min(32);
                batch.extend(s.queue.drain(..n));
                return true;
            }
            if s.active == 0 {
                self.wake.notify_all();
                return false;
            }
            s = self.wake.wait(s).unwrap();
        }
    }
    fn finish(&self, discovered: &mut Vec<Work>) {
        let mut s = self.state.lock().unwrap();
        s.queue.extend(discovered.drain(..));
        s.active -= 1;
        self.wake.notify_all();
    }
    fn fail(&self) {
        self.state.lock().unwrap().failed = true;
        self.wake.notify_all();
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
    align: usize,
    claim: Claim<'a>,
}

impl Heap {
    /// Enumerate root sources on the coordinator: RootSource need not be Sync.
    /// Deduplicate slots because registrations and frame walks can overlap.
    /// Pointer addresses are transferred only within the scoped pause, while
    /// root owners remain alive and every mutator is suspended.
    pub(super) unsafe fn parallel_major<P: PtrPolicy>(&self, extras: &[&dyn RootSource]) {
        unsafe { self.parallel_evacuate::<P>(extras, false, Vec::new()) };
    }

    pub(super) unsafe fn parallel_minor<P: PtrPolicy>(&self) {
        let ns = self.nursery_state.as_ref().unwrap();
        let table = &ns.card_tables[self.from_idx.load(Ordering::Acquire)];
        let space = self.from_space();
        let used = space.used();
        let starts = unsafe {
            Self::build_object_start_index(
                space,
                used,
                table,
                self.type_id_offset,
                &self.type_table,
            )
        };
        let ranges = space.initialized_ranges();
        let mut objects = HashSet::new();
        for (card, addr) in table.iter_dirty() {
            let Some(&start) = starts.get(card) else {
                continue;
            };
            let end = addr as usize + 512;
            let mut offset = start;
            let mut region = ranges.partition_point(|range| range.end <= offset);
            while offset < used && region < ranges.len() {
                offset = offset.max(ranges[region].start);
                if offset >= ranges[region].end { region += 1; continue; }
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
                if obj as usize + size > addr as usize {
                    objects.insert(obj as usize);
                }
                let align = 1usize << info.align_log2;
                offset = (offset + size + align - 1) & !(align - 1);
            }
        }
        unsafe {
            self.parallel_evacuate::<P>(&[], true, objects.into_iter().map(Work::Object).collect())
        };
    }

    unsafe fn parallel_evacuate<P: PtrPolicy>(
        &self,
        extras: &[&dyn RootSource],
        minor: bool,
        initial: Vec<Work>,
    ) {
        let mut roots = HashSet::new();
        let mut visit = |slot: *mut u64| {
            roots.insert(slot as usize);
        };
        unsafe { self.scan_persistent_roots(&mut visit); }
        for ts in self.threads.lock().unwrap().iter() {
            unsafe { ts.scan_roots(&mut visit); }
            let fp = ts.parked_jit_fp();
            if !fp.is_null() {
                self.walk_jit_frame(fp, &mut visit);
            }
        }
        if !extras.is_empty() {
            self.saw_extra_roots.store(true, Ordering::Relaxed);
        }
        for source in extras {
            unsafe { source.scan_roots(&mut visit); }
        }
        for &source in self.permanent_extras.lock().unwrap().iter() {
            unsafe {
                (&*source).scan_roots(&mut visit);
            }
        }
        if let Some(ns) = self.nursery_state.as_ref().filter(|_| !minor) {
            unsafe {
                ns.nursery.walk(&self.type_table, &mut |obj, info| {
                    scan_object(obj, info, &mut visit);
                });
            }
        }
        let queue = WorkQueue {
            state: Mutex::new(State {
                queue: roots.into_iter().map(Work::Slot).chain(initial).collect(),
                active: 0,
                failed: false,
            }),
            wake: Condvar::new(),
        };
        let configured = std::env::var("GCR_GC_WORKERS").ok();
        let workers = configured
            .clone()
            .map(|s| {
                s.parse::<usize>()
                    .expect("GCR_GC_WORKERS must be a positive integer")
            })
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map_or(1, usize::from)
                    .min(8)
            });
        assert!(workers > 0, "GCR_GC_WORKERS must be positive");
        // Small cycles run on the coordinator. Parallel cycles reuse helpers;
        // every participant completes before space swap or nursery reset.
        let workers = if configured.is_none()
            && (if minor {
                self.nursery_state.as_ref().unwrap().nursery.used()
            } else {
                self.from_used()
            }) < 256 * 1024
        {
            1
        } else {
            workers
        };
        let run = |_worker_id: usize| {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut batch = Vec::with_capacity(32);
                let mut discovered = Vec::with_capacity(64);
                let mut pending = Vec::with_capacity(64);
                let mut layouts = Vec::with_capacity(64);
                let mut addresses = Vec::with_capacity(64);
                while queue.take(&mut batch) {
                    for task in batch.drain(..) {
                        match task {
                            Work::Slot(slot) => unsafe {
                                self.parallel_slot_mode::<P>(
                                    slot as *mut u64,
                                    &mut discovered,
                                    &mut pending,
                                    minor,
                                );
                            },
                            Work::Object(addr) => unsafe {
                                let obj = addr as *mut u8;
                                let info =
                                    self.type_info_by_id(read_type_id(obj, self.type_id_offset));
                                let mut nursery_edge = false;
                                scan_object(obj, info, |slot| {
                                    self.parallel_slot_mode::<P>(
                                        slot,
                                        &mut discovered,
                                        &mut pending,
                                        minor,
                                    );
                                    if let Some(ptr) = P::try_decode_ptr(
                                        (&*slot.cast::<AtomicU64>()).load(Ordering::Relaxed),
                                    ) {
                                        nursery_edge |= self.is_nursery(ptr);
                                    }
                                });
                                if !minor && nursery_edge {
                                    let ns = self.nursery_state.as_ref().unwrap();
                                    ns.card_tables[1 - self.from_idx.load(Ordering::Acquire)]
                                        .mark_dirty(obj);
                                }
                            },
                        }
                    }
                    unsafe {
                        self.publish_batch::<P>(
                            &mut pending,
                            &mut layouts,
                            &mut addresses,
                            &mut discovered,
                            minor,
                        );
                    }
                    queue.finish(&mut discovered);
                }
            }));
            if let Err(payload) = result {
                queue.fail();
                std::panic::resume_unwind(payload);
            }
        };
        if workers == 1 {
            run(0);
        } else {
            self.collector_workers
                .get_or_init(WorkerPool::new)
                .run(workers - 1, &run);
        }
    }

    #[cfg(test)]
    unsafe fn parallel_slot<P: PtrPolicy>(&self, slot: *mut u64, work: &mut Vec<Work>) {
        // Direct contention test has no queue to retry BUSY slots, so explicitly
        // complete its own claims and retry until this slot is relocated.
        let mut pending = Vec::new();
        let mut layouts = Vec::new();
        let mut addresses = Vec::new();
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

    unsafe fn parallel_slot_mode<'a, P: PtrPolicy>(
        &'a self,
        slot: *mut u64,
        work: &mut Vec<Work>,
        pending: &mut Vec<PendingCopy<'a>>,
        minor: bool,
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
        let header = unsafe { &*(old.add(self.type_id_offset) as *const AtomicU64) };
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
            let type_id = original as u16;
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
                align: 1usize << info.align_log2,
                claim,
            });
            return;
        }
    }

    unsafe fn publish_batch<P: PtrPolicy>(
        &self,
        pending: &mut Vec<PendingCopy<'_>>,
        layouts: &mut Vec<(usize, usize)>,
        addresses: &mut Vec<*mut u8>,
        work: &mut Vec<Work>,
        minor: bool,
    ) {
        if pending.is_empty() {
            return;
        }
        layouts.clear();
        layouts.extend(pending.iter().map(|copy| (copy.size, copy.align)));
        let destination = if minor {
            self.from_space()
        } else {
            self.to_space()
        };
        assert!(
            unsafe { destination.reserve_batch(layouts, addresses) },
            "to-space exhausted during parallel collection"
        );
        for (mut copy, &new) in pending.drain(..).zip(addresses.iter()) {
            unsafe {
                // Exclude the claimed header from memcpy: all accesses to the
                // old word are atomic while collector workers run.
                std::ptr::copy_nonoverlapping(copy.old, new, self.type_id_offset);
                new.add(self.type_id_offset)
                    .cast::<u64>()
                    .write(copy.claim.original);
                let tail = self.type_id_offset + 8;
                std::ptr::copy_nonoverlapping(copy.old.add(tail), new.add(tail), copy.size - tail);
            }
            copy.claim
                .header
                .store(new as u64 | FORWARDING_BIT, Ordering::Release);
            copy.claim.published = true;
            unsafe {
                (&*copy.slot.cast::<AtomicU64>()).store(P::encode_ptr(new), Ordering::Relaxed);
            }
            work.push(Work::Object(new as usize));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{Compact, Full, IdentityPtrPolicy, RootSet};
    use std::sync::Barrier;

    #[test]
    fn contended_evacuation_has_one_copy_and_preserves_header_and_payload() {
        for full in [false, true] {
            let header_size = if full { Full::SIZE } else { Compact::SIZE };
            let info = TypeInfo::for_header(header_size)
                .with_type_id(1)
                .with_varlen_bytes(0);
            let heap = if full {
                Heap::new::<Full>(4 * 1024 * 1024, vec![info, info])
            } else {
                Heap::new::<Compact>(4 * 1024 * 1024, vec![info, info])
            };
            let old = if full {
                heap.alloc_obj::<Full>(&info, 1024 * 1024)
            } else {
                heap.alloc_obj::<Compact>(&info, 1024 * 1024)
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
                            let mut work = Vec::new();
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
            assert_eq!(results.iter().map(|r| r.1).sum::<usize>(), 1);
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
                Heap::new::<Full>(2 * 1024 * 1024, vec![info])
            } else {
                Heap::new::<Compact>(2 * 1024 * 1024, vec![info])
            };
            let count = 16000;
            let objects: Vec<_> = (0..count)
                .map(|_| {
                    if full {
                        heap.alloc_obj::<Full>(&info, 0)
                    } else {
                        heap.alloc_obj::<Compact>(&info, 0)
                    }
                })
                .collect();
            let mut roots = RootSet::new();
            for (i, &obj) in objects.iter().enumerate() {
                unsafe {
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
            state: Mutex::new(State {
                queue: VecDeque::from([Work::Slot(1)]),
                active: 0,
                failed: false,
            }),
            wake: Condvar::new(),
        };
        let mut batch = Vec::new();
        assert!(q.take(&mut batch));
        assert_eq!(batch.len(), 1);
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                let mut batch = Vec::new();
                assert!(q.take(&mut batch), "active producer prevents termination");
                batch
            });
            q.finish(&mut vec![Work::Object(2)]);
            assert!(matches!(waiter.join().unwrap()[0], Work::Object(2)));
        });
        q.finish(&mut vec![]);
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
        let heap = Heap::new::<Full>(96 * 1024 * 1024, vec![info]);
        let objects: Vec<_> = (0..count)
            .map(|_| heap.alloc_obj::<Full>(&info, bytes))
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
        for _ in 0..9 {
            unsafe {
                heap.collect::<IdentityPtrPolicy>(&[&root]);
            }
            assert_eq!(heap.from_used(), count * info.allocation_size(bytes));
        }
        let mut pauses: Vec<_> = heap
            .gc_events()
            .into_iter()
            .skip(2)
            .map(|e| e.pause_ns)
            .collect();
        pauses.sort_unstable();
        eprintln!(
            "copy benchmark: workers={} live_bytes={} median_pause_ns={}",
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
        let heap = Heap::new_generational::<Full>(1024 * 1024, 2 * 1024 * 1024, vec![node, array]);
        let (thread, _) = heap.register_thread();
        let count = 12000;
        let nodes: Vec<_> = (0..count)
            .map(|_| heap.alloc_nursery_obj::<Full>(&node, 0))
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
        let old = heap.alloc_obj::<Full>(&array, count);
        for (i, &child) in nodes.iter().enumerate() {
            let slot = unsafe { old.add(array.varlen_element_offset(i)) };
            unsafe {
                slot.cast::<u64>().write(Tagged::encode_ptr(child));
            }
            heap.mark_card_dirty(slot);
        }
        heap.globals.add(Tagged::encode_ptr(old));
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
        let heap = Heap::new::<Full>(4096, vec![info]);
        let old = [
            heap.alloc_obj::<Full>(&info, 0) as usize,
            heap.alloc_obj::<Full>(&info, 0) as usize,
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
                            let mut work = Vec::new();
                            let mut pending = Vec::new();
                            let mut layouts = Vec::new();
                            let mut addresses = Vec::new();
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
