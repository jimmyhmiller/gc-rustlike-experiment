//! Stage native registry storage without holding a collector-visible lock.
use super::native_metadata::{self, MetadataAllocator};
use super::registry_stage::{RegistryStage, growth_request};
use super::{AtomicCellRegistry, AtomicI64, Heap, Mutex, Thread, blocking_region};
use std::sync::Arc;

pub(super) fn new_cell(value: i64, heap: &Heap) -> Arc<super::native_retirement::AtomicCell> {
    #[cfg(test)]
    tests::checkpoint(heap as *const Heap as usize, tests::Phase::Cell);
    super::native_retirement::AtomicCell::new(value, &heap.native_retirement)
}

#[cfg(test)]
pub(super) fn stage(capacity: usize, heap_id: usize) -> RegistryStage<AtomicCellRegistry, 2> {
    stage_plan([capacity; 2], heap_id)
}
#[cfg(test)]
fn stage_plan(plan: [usize; 2], _heap_id: usize) -> RegistryStage<AtomicCellRegistry, 2> {
    stage_in(plan, MetadataAllocator::default(), _heap_id)
}
fn stage_in(
    plan: [usize; 2],
    allocator: MetadataAllocator,
    _heap_id: usize,
) -> RegistryStage<AtomicCellRegistry, 2> {
    #[cfg(test)]
    tests::checkpoint(_heap_id, tests::Phase::Stage);
    RegistryStage::components(
        plan,
        AtomicCellRegistry {
            records: native_metadata::map(plan[0], allocator.clone()),
            owned: native_metadata::vector(plan[1], allocator.clone()),
        },
    )
}

fn discard(tables: RegistryStage<AtomicCellRegistry, 2>, _heap_id: usize) {
    #[cfg(test)]
    tests::checkpoint(_heap_id, tests::Phase::Discard);
    drop(tables);
}

impl AtomicCellRegistry {
    /// Heap initialization precedes mutator publication. Prepare the platform
    /// mutex backend here, rather than on the first RUNNING registration.
    #[cfg(test)]
    pub(crate) fn native_mutex() -> Mutex<Self> {
        Self::native_mutex_in(MetadataAllocator::default())
    }
    pub(crate) fn native_mutex_in(allocator: MetadataAllocator) -> Mutex<Self> {
        let mutex = Mutex::new(Self::with_allocator(allocator));
        drop(mutex.lock().unwrap());
        mutex
    }

    pub(super) fn can_register(&self) -> bool {
        self.records.capacity() > self.records.len()
            // Any unbound atomic cell can bind later, without allocating.
            && self.owned.capacity() > self.records.len()
    }

    #[cfg(test)]
    pub(super) fn growth_capacity(&self) -> Option<usize> {
        (!self.can_register()).then(|| {
            self.records
                .len()
                .checked_add(1)
                .and_then(|n| n.checked_mul(2))
                .expect("atomic cell registry capacity overflow")
        })
    }

    fn growth_plan(&self) -> [usize; 2] {
        let live = self.records.len();
        [
            growth_request(self.records.capacity(), live),
            growth_request(self.owned.capacity(), live),
        ]
    }

    /// Recheck after native allocation: concurrent registration may have used
    /// more capacity, or GC may have removed entries. Move, never clone, live
    /// controls only when that component still needs growth. Preflight all
    /// exhausted components before moving any entry. Obsolete and unused empty
    /// backing remains staged for disposal outside the publication lock.
    fn install(&mut self, staged: &mut RegistryStage<Self, 2>) -> bool {
        let live = self.records.len();
        if !staged.within_budget([live, live]) {
            return false;
        }
        let tables = &mut staged.tables;
        assert!(tables.records.is_empty() && tables.owned.is_empty());
        // Preflight every exhausted component before moving any ownership.
        if (self.records.capacity() <= live
            && (tables.records.capacity() <= live
                || !self
                    .records
                    .allocator()
                    .same_counter(tables.records.allocator())))
            || (self.owned.capacity() <= live
                && (tables.owned.capacity() <= live
                    || !self
                        .owned
                        .allocator()
                        .same_counter(tables.owned.allocator())))
        {
            return false;
        }
        if self.records.capacity() <= live {
            for (key, record) in self.records.drain() {
                tables.records.insert(key, record);
            }
            std::mem::swap(&mut self.records, &mut tables.records);
        }
        if self.owned.capacity() <= live {
            tables.owned.extend(self.owned.drain(..));
            std::mem::swap(&mut self.owned, &mut tables.owned);
        }
        debug_assert!(self.can_register());
        true
    }
}

/// The cell is unpublished and contains only a scalar. Its Arc stays alive
/// through native transitions; once registered it has execution lifetime until
/// binding to a managed owner. No managed addresses cross these transitions.
pub(super) unsafe fn publish(
    thread: *mut Thread,
    heap: &Heap,
    cell: Arc<super::native_retirement::AtomicCell>,
) -> *mut AtomicI64 {
    let heap_id = heap as *const Heap as usize;
    // The common path needs only the publication lock. Grow on demand rather
    // than taking a speculative planning lock before every native constructor.
    let mut tables = None;
    loop {
        let mut registry = heap.atomic_cells.lock().unwrap();
        if !registry.can_register() {
            if let Some(tables) = tables.as_mut() {
                registry.install(tables);
            }
        }
        if registry.can_register() {
            let handle = registry.register_reserved(cell);
            drop(registry);
            // All native deallocation happens off-lock and BLOCKED, including
            // unused stages when another registrant supplied enough capacity.
            if let Some(tables) = tables {
                unsafe { blocking_region(thread, || discard(tables, heap_id)) };
            }
            return handle;
        }
        let plan = registry.growth_plan();
        let allocator = registry.records.allocator().clone();
        drop(registry);
        tables = Some(unsafe {
            blocking_region(thread, || {
                if let Some(tables) = tables {
                    discard(tables, heap_id);
                }
                stage_in(plan, allocator, heap_id)
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo},
        runtime::*,
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    };

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum Phase {
        Cell,
        Stage,
        Discard,
    }
    struct Hook {
        heap: usize,
        phase: Phase,
        ready: Sender<()>,
        completed: Arc<AtomicBool>,
    }
    static HOOK: Mutex<Option<Hook>> = Mutex::new(None);
    pub(super) fn checkpoint(heap: usize, phase: Phase) {
        let hook = {
            let mut slot = HOOK.lock().unwrap();
            if slot
                .as_ref()
                .is_some_and(|h| h.heap == heap && h.phase == phase)
            {
                slot.take()
            } else {
                None
            }
        };
        if let Some(hook) = hook {
            hook.ready.send(()).unwrap();
            while !hook.completed.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
    }

    #[test]
    fn collector_atomic_registry_reference_mutex_is_prepared() {
        let mutex = AtomicCellRegistry::native_mutex();
        allocation_probe::start();
        drop(mutex.lock().unwrap());
        assert_eq!(allocation_probe::finish(), (0, 0));
    }

    #[test]
    fn collector_atomic_registry_reference_commit_and_late_bind_do_not_allocate() {
        for count in [0, 1, 17, 257] {
            let retirement = native_retirement::Retirement::default();
            let mut registry = AtomicCellRegistry::default();
            let mut handles = Vec::new();
            for i in 0..count {
                let handle =
                    registry.register(native_retirement::AtomicCell::new(i as i64, &retirement));
                if i == 0 {
                    registry.bind(handle, 8usize as *mut u8);
                }
                handles.push(handle);
            }
            let mut tables = stage(2 * (count + 1), 0);
            let cell = native_retirement::AtomicCell::new(73, &retirement);
            allocation_probe::start();
            assert!(registry.install(&mut tables));
            let handle = registry.register_reserved(cell);
            let traffic = allocation_probe::finish_bytes();
            assert_eq!(traffic, (0, 0, 0, 0));
            assert!(tables.tables.records.is_empty() && tables.tables.owned.is_empty());
            for (i, &handle) in handles.iter().enumerate() {
                let record = &registry.records[&(handle as usize)];
                assert_eq!(record.0.pointer(), handle);
                assert_eq!(Arc::strong_count(&record.0), 1);
                assert_eq!(record.0.load(Ordering::SeqCst), i as i64);
                assert_eq!(record.1, (i == 0).then_some(8));
            }
            // Force sparse-capacity maintenance while unbound records remain.
            registry.owned.reserve(8 * registry.records.len());
            registry.sweep(Some);
            let native = Mutex::new(std::mem::take(&mut registry));
            registry_compaction::compact_atomics(&native);
            registry = native.into_inner().unwrap();
            assert!(registry.owned.capacity() >= registry.records.len());
            allocation_probe::start();
            for (i, &handle) in handles.iter().enumerate() {
                if i != 0 {
                    registry.bind(handle, 8usize as *mut u8);
                }
            }
            registry.bind(handle, 8usize as *mut u8);
            let traffic = allocation_probe::finish_bytes();
            assert_eq!(traffic, (0, 0, 0, 0));
            assert_eq!(registry.owned.len(), count + 1);
            assert_eq!(
                registry.records[&(handle as usize)]
                    .0
                    .load(Ordering::SeqCst),
                73
            );
        }
    }

    #[test]
    fn collector_atomic_registry_native_phases_allow_collection_and_retry() {
        if !channel_buffer::tests::child(
            "runtime::atomic_registry_growth::tests::collector_atomic_registry_native_phases_allow_collection_and_retry",
        ) {
            return;
        }
        for phase in [Phase::Cell, Phase::Stage, Phase::Discard] {
            let generational = std::env::var("GCR_CHANNEL_BUFFER_CHILD").as_deref() == Ok("1");
            let types = vec![TypeInfo::for_header(Full::SIZE).with_raw_bytes(8)];
            let mut context = if generational {
                RuntimeContext::new_generational(32768, 2 << 20, types)
            } else {
                RuntimeContext::new(2 << 20, types)
            };
            let heap = context.heap().clone();
            context.enter(|thread| unsafe {
                let roots = (&*(*thread).dyna_thread).scratch_scope();
                let witness = roots.push(ai_gc_alloc_fixed(thread, 0, 0));
                roots.get(witness).add(Full::SIZE).cast::<u64>().write(42);
                let first = ai_atomic_i64_new(thread, 17);
                while heap.atomic_cells.lock().unwrap().can_register() {
                    ai_atomic_i64_new(thread, 0);
                }
                let before = heap.atomic_cells.lock().unwrap().records.len();
                let previous_owner = roots.get(witness) as usize;
                let collections_before = if generational {
                    heap.minor_collections()
                } else {
                    heap.collections()
                };
                let (ready, waiting) = std::sync::mpsc::channel();
                let completed = Arc::new(AtomicBool::new(false));
                let retry_completed = Arc::new(AtomicBool::new(false));
                *HOOK.lock().unwrap() = Some(Hook {
                    heap: Arc::as_ptr(&heap) as usize,
                    phase,
                    ready: ready.clone(),
                    completed: completed.clone(),
                });
                let other = heap.clone();
                let done = completed.clone();
                let retry_done = retry_completed.clone();
                let collector = std::thread::spawn(move || {
                    waiting.recv().unwrap();
                    let added = if phase == Phase::Stage {
                        let mut registry = other.atomic_cells.lock().unwrap();
                        for _ in 0..65 {
                            registry.register(native_retirement::AtomicCell::new(
                                0,
                                &other.native_retirement,
                            ));
                        }
                        while registry.can_register() {
                            registry.register(native_retirement::AtomicCell::new(
                                0,
                                &other.native_retirement,
                            ));
                        }
                        registry.records.len() - before
                    } else {
                        0
                    };
                    let (state, _) = other.register_thread();
                    let collect = || {
                        if generational {
                            // An empty nursery legitimately skips a minor.
                            // Give both checkpoints real young work to collect.
                            let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
                            let garbage = other.alloc_nursery_obj::<Full>(&info, 0);
                            assert!(!garbage.is_null());
                            garbage.add(info.raw_data_offset()).cast::<u64>().write(0);
                            other.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state)
                        } else {
                            other.mutator_triggered_gc::<IdentityPtrPolicy>(&state)
                        }
                    };
                    collect();
                    assert_eq!(
                        other.atomic_cells.lock().unwrap().records.len(),
                        before + added + usize::from(phase == Phase::Discard)
                    );
                    if phase == Phase::Stage {
                        *HOOK.lock().unwrap() = Some(Hook {
                            heap: Arc::as_ptr(&other) as usize,
                            phase,
                            ready,
                            completed: retry_done.clone(),
                        });
                    }
                    done.store(true, Ordering::Release);
                    if phase == Phase::Stage {
                        // Park this collector while the mutator prepares its
                        // replacement stage and signals the second checkpoint.
                        state.enter_blocked();
                        waiting.recv().unwrap();
                        state.exit_blocked(&other);
                        collect();
                        retry_done.store(true, Ordering::Release);
                    }
                    other.safe_deregister_thread(&state);
                    added
                });
                let handle = ai_atomic_i64_new(thread, 73);
                assert!(completed.load(Ordering::Acquire));
                assert!(phase != Phase::Stage || retry_completed.load(Ordering::Acquire));
                let added = blocking_region(thread, || collector.join().unwrap());
                let collections_after = if generational {
                    heap.minor_collections()
                } else {
                    heap.collections()
                };
                assert_eq!(
                    collections_after - collections_before,
                    if phase == Phase::Stage { 2 } else { 1 }
                );
                if generational || phase != Phase::Stage {
                    assert_ne!(roots.get(witness) as usize, previous_owner);
                }

                assert_eq!(
                    heap.atomic_cells.lock().unwrap().records.len(),
                    before + added + 1
                );
                assert_eq!(ai_atomic_i64_load(thread, first), 17);
                assert_eq!(ai_atomic_i64_load(thread, handle), 73);
                assert_eq!(roots.get(witness).add(Full::SIZE).cast::<u64>().read(), 42);
                assert_eq!(ai_object_type_id(thread, roots.get(witness)), 0);
                // Publication survives collection as an unbound record. Bind
                // only the reloaded owner and verify the ordinary atomic ABI.
                ai_atomic_i64_bind_owner(thread, handle, roots.get(witness));
                assert_eq!(ai_atomic_i64_fetch_add(thread, handle, 1), 73);
                assert_eq!(ai_atomic_i64_load(thread, handle), 74);
            });
        }
    }

    #[test]
    fn collector_atomic_registry_reference_replans_after_gc_shrink() {
        let retirement = native_retirement::Retirement::default();
        let mut registry = AtomicCellRegistry::default();
        for _ in 0..257 {
            let handle = registry.register(native_retirement::AtomicCell::new(0, &retirement));
            registry.bind(handle, 8usize as *mut u8);
        }
        let mut tables = stage(2 * (registry.records.len() + 1), 0);
        registry.sweep(|_| None);
        assert!(registry.records.is_empty());
        assert!(
            !registry.install(&mut tables),
            "installed a historical-peak stage after GC"
        );
        // GC retires owners first; backing replacement is a separate native phase.
        let native = Mutex::new(std::mem::take(&mut registry));
        registry_compaction::compact_atomics(&native);
        registry = native.into_inner().unwrap();
        let mut small = stage(registry.growth_capacity().unwrap(), 0);
        assert!(registry.install(&mut small));
        registry.register_reserved(native_retirement::AtomicCell::new(42, &retirement));
        assert!(registry.records.capacity() <= 8 && registry.owned.capacity() <= 8);
    }
}

#[cfg(test)]
#[path = "atomic_admission_tests.rs"]
mod component_tests;
