//! Stage native registry storage without holding a collector-visible lock.
use super::native_metadata::{self, MetadataAllocator};
use super::registry_stage::{RegistryStage, growth_request};
use super::{ChanCtrl, ChannelRegistry, Heap, Mutex, Thread, blocking_region};
use std::sync::Arc;

#[cfg(test)]
pub(super) fn stage(capacity: usize, heap_id: usize) -> RegistryStage<ChannelRegistry> {
    stage_plan([capacity; 3], heap_id)
}
#[cfg(test)]
fn stage_plan(plan: [usize; 3], _heap_id: usize) -> RegistryStage<ChannelRegistry> {
    stage_in(plan, MetadataAllocator::default(), _heap_id)
}
fn stage_in(
    plan: [usize; 3],
    allocator: MetadataAllocator,
    _heap_id: usize,
) -> RegistryStage<ChannelRegistry> {
    #[cfg(test)]
    tests::checkpoint(_heap_id, false);
    RegistryStage::components(
        plan,
        ChannelRegistry {
            records: native_metadata::map(plan[0], allocator.clone()),
            unbound: native_metadata::set(plan[1], allocator.clone()),
            owned: native_metadata::set(plan[2], allocator.clone()),
        },
    )
}

fn discard(tables: RegistryStage<ChannelRegistry>, _heap_id: usize) {
    #[cfg(test)]
    tests::checkpoint(_heap_id, true);
    drop(tables);
}

impl ChannelRegistry {
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
            && self.unbound.capacity() > self.unbound.len()
            // Any unbound channel can bind later, without allocating.
            && self.owned.capacity() > self.records.len()
    }

    #[cfg(test)]
    pub(super) fn growth_capacity(&self) -> Option<usize> {
        (!self.can_register()).then(|| {
            self.records
                .len()
                .checked_add(1)
                .and_then(|n| n.checked_mul(2))
                .expect("channel registry capacity overflow")
        })
    }

    fn growth_plan(&self) -> [usize; 3] {
        let live = self.records.len();
        let raw = self.unbound.len();
        [
            growth_request(self.records.capacity(), live),
            growth_request(self.unbound.capacity(), raw),
            growth_request(self.owned.capacity(), live),
        ]
    }

    /// Recheck after native allocation: concurrent registration may have used
    /// more capacity, or GC may have removed entries. Move, never clone, live
    /// controls only when that component still needs growth. Preflight all
    /// exhausted components before moving any entry. Obsolete and unused empty
    /// backing remains staged for disposal outside the publication lock.
    fn install(&mut self, staged: &mut RegistryStage<Self>) -> bool {
        let live = self.records.len();
        let raw = self.unbound.len();
        if !staged.within_budget([live, raw, live]) {
            return false;
        }
        let tables = &mut staged.tables;
        assert!(tables.records.is_empty() && tables.unbound.is_empty() && tables.owned.is_empty());
        // Preflight every exhausted component before moving any ownership.
        if (self.records.capacity() <= live
            && (tables.records.capacity() <= live
                || !self
                    .records
                    .allocator()
                    .same_counter(tables.records.allocator())))
            || (self.unbound.capacity() <= raw
                && (tables.unbound.capacity() <= raw
                    || !self
                        .unbound
                        .allocator()
                        .same_counter(tables.unbound.allocator())))
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
        if self.unbound.capacity() <= raw {
            tables.unbound.extend(self.unbound.drain());
            std::mem::swap(&mut self.unbound, &mut tables.unbound);
        }
        if self.owned.capacity() <= live {
            tables.owned.extend(self.owned.drain());
            std::mem::swap(&mut self.owned, &mut tables.owned);
        }
        debug_assert!(self.can_register());
        true
    }
}

/// The control is still unpublished and contains no managed values. The caller
/// keeps its witness rooted across every transition and reloads it if reused.
pub(super) unsafe fn publish(
    thread: *mut Thread,
    heap: &Heap,
    channel: Arc<ChanCtrl>,
) -> *mut ChanCtrl {
    let heap_id = heap as *const Heap as usize;
    // The common path needs only the publication lock. Grow on demand rather
    // than taking a speculative planning lock before every native constructor.
    let mut tables = None;
    loop {
        let mut registry = heap.channels.lock().unwrap();
        if !registry.can_register() {
            if let Some(tables) = tables.as_mut() {
                registry.install(tables);
            }
        }
        if registry.can_register() {
            let handle = registry.register_reserved(channel, &heap.native_memory);
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

    struct Hook {
        heap: usize,
        discard: bool,
        ready: Sender<()>,
        completed: Arc<AtomicBool>,
    }
    static HOOK: Mutex<Option<Hook>> = Mutex::new(None);
    pub(super) fn checkpoint(heap: usize, discard: bool) {
        let hook = {
            let mut slot = HOOK.lock().unwrap();
            if slot
                .as_ref()
                .is_some_and(|h| h.heap == heap && h.discard == discard)
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
    fn collector_channel_registry_reference_native_mutex_is_prepared() {
        let mutex = ChannelRegistry::native_mutex();
        allocation_probe::start();
        drop(mutex.lock().unwrap());
        let counts = allocation_probe::finish();
        assert_eq!(counts, (0, 0));
    }

    #[test]
    fn collector_channel_registry_reference_commit_does_not_allocate() {
        let memory = Arc::new(native_memory::NativeMemory::default());
        let retirement = native_retirement::Retirement::default();
        for count in [0, 1, 17, 257] {
            let mut registry = ChannelRegistry::default();
            let mut handles = Vec::new();
            for i in 0..count {
                let handle = registry.register(channel_owner_tests::queue(0, &retirement), &memory);
                if i % 2 == 0 {
                    registry.bind(
                        handle,
                        ChannelOwner {
                            address: 8,
                            type_id: 1,
                        },
                    );
                }
                handles.push(handle);
            }
            let mut too_small = stage(0, 0);
            let already_ready = registry.can_register();
            allocation_probe::start();
            let installed = registry.install(&mut too_small);
            let counts = allocation_probe::finish();
            assert_eq!(installed, already_ready);
            assert_eq!(counts, (0, 0));
            assert_eq!(registry.records.len(), count);

            let mut tables = stage((count + 1) * 2, 0);
            let channel = channel_owner_tests::queue(0, &retirement);
            channel_sync::prepare(&channel);
            allocation_probe::start();
            let installed = registry.install(&mut tables);
            let handle = registry.register_reserved(channel, &memory);
            let counts = allocation_probe::finish();
            assert!(installed);
            assert_eq!(counts, (0, 0));
            assert!(
                tables.tables.records.is_empty()
                    && tables.tables.owned.is_empty()
                    && tables.tables.unbound.is_empty()
            );
            assert_eq!(registry.records.len(), count + 1);
            assert!(registry.unbound.contains(&(handle as usize)));
            for (i, handle) in handles.into_iter().enumerate() {
                let record = &registry.records[&(handle as usize)];
                assert_eq!(Arc::as_ptr(&record.0), handle.cast_const());
                assert_eq!(Arc::strong_count(&record.0), 1);
                assert_eq!(record.1, (i % 2 == 0).then_some(1));
                assert_eq!(registry.owned.contains(&(handle as usize)), i % 2 == 0);
                assert_eq!(registry.unbound.contains(&(handle as usize)), i % 2 != 0);
            }
            registry.clear();
            retirement.drain();
            assert_eq!(memory.bytes(), 0);
        }
    }

    #[test]
    fn collector_channel_registry_native_phases_allow_collection_and_retry() {
        if !channel_buffer::tests::child(
            "runtime::channel_registry_growth::tests::collector_channel_registry_native_phases_allow_collection_and_retry",
        ) {
            return;
        }
        for discarding in [false, true] {
            let types = vec![
                TypeInfo::for_header(Full::SIZE).with_varlen_values(0),
                TypeInfo::for_header(Full::SIZE)
                    .with_type_id(1)
                    .with_raw_bytes(8),
            ];
            let mut context = if std::env::var("GCR_CHANNEL_BUFFER_CHILD").as_deref() == Ok("1") {
                RuntimeContext::new_generational(32768, 2 << 20, types)
            } else {
                RuntimeContext::new(2 << 20, types)
            };
            let heap = context.heap().clone();
            context.enter(|thread| unsafe {
                let roots = (&*(*thread).dyna_thread).scratch_scope();
                let witness = roots.push(ai_gc_alloc_varlen(thread, 0, 0, 0));
                let first = ai_chan_new(thread, 1, roots.get(witness), 1);
                let message = ai_gc_alloc_fixed(thread, 1, 0);
                message.add(Full::SIZE).cast::<u64>().write(42);
                ai_chan_send(thread, roots.get(witness), first, message);
                while heap.channels.lock().unwrap().can_register() {
                    ai_chan_new(thread, 1, roots.get(witness), 1);
                }
                let before = heap.channels.lock().unwrap().records.len();
                let collections_before = heap.collections();
                let (ready, waiting) = std::sync::mpsc::channel();
                let completed = Arc::new(AtomicBool::new(false));
                let retry_completed = Arc::new(AtomicBool::new(false));
                let retry_ready = ready.clone();
                *HOOK.lock().unwrap() = Some(Hook {
                    heap: Arc::as_ptr(&heap) as usize,
                    discard: discarding,
                    ready,
                    completed: completed.clone(),
                });
                let other = heap.clone();
                let done = completed.clone();
                let retry_done = retry_completed.clone();
                let collector = std::thread::spawn(move || {
                    waiting.recv().unwrap();
                    // A registration race makes the precomputed stage stale.
                    // These synthetic queues contain no managed pointers.
                    let added = if !discarding {
                        let mut registry = other.channels.lock().unwrap();
                        for _ in 0..65 {
                            registry.register(
                                channel_owner_tests::queue(0, &other.native_retirement),
                                &other.native_memory,
                            );
                        }
                        // Leave the shared tables full as well as making the
                        // stage too small. Otherwise publication could reuse
                        // the competitor's spare capacity without retrying.
                        while registry.can_register() {
                            registry.register(
                                channel_owner_tests::queue(0, &other.native_retirement),
                                &other.native_memory,
                            );
                        }
                        registry.records.len() - before
                    } else {
                        0
                    };
                    other.stw_collect::<IdentityPtrPolicy>();
                    let length = other.channels.lock().unwrap().records.len();
                    if !discarding {
                        *HOOK.lock().unwrap() = Some(Hook {
                            heap: Arc::as_ptr(&other) as usize,
                            discard: false,
                            ready: retry_ready,
                            completed: retry_done.clone(),
                        });
                    }
                    done.store(true, Ordering::Release);
                    assert_eq!(length, before + added + usize::from(discarding));
                    if !discarding {
                        // Collection must also complete during allocation of
                        // the replacement stage on the retry path.
                        waiting.recv().unwrap();
                        other.stw_collect::<IdentityPtrPolicy>();
                        retry_done.store(true, Ordering::Release);
                    }
                    added
                });
                let handle = ai_chan_new(thread, 1, roots.get(witness), 1);
                assert!(completed.load(Ordering::Acquire));
                assert!(discarding || retry_completed.load(Ordering::Acquire));
                let added = blocking_region(thread, || collector.join().unwrap());
                assert_eq!(
                    heap.collections(),
                    collections_before + if discarding { 1 } else { 2 }
                );
                assert!(
                    heap.channels
                        .lock()
                        .unwrap()
                        .records
                        .contains_key(&(handle as usize))
                );
                assert_eq!(
                    heap.channels.lock().unwrap().records.len(),
                    before + added + 1
                );
                let received = ai_chan_recv(thread, roots.get(witness), first);
                assert_eq!(received.add(Full::SIZE).cast::<u64>().read(), 42);
                assert_eq!(ai_object_type_id(thread, roots.get(witness)), 0);
                ai_chan_close(thread, first);
            });
            drop(context);
            assert_eq!(heap.native_buffer_bytes(), 0);
        }
    }

    #[test]
    fn collector_channel_registry_reference_replans_after_gc_shrink() {
        let memory = Arc::new(native_memory::NativeMemory::default());
        let retirement = native_retirement::Retirement::default();
        let mut registry = ChannelRegistry::default();
        for _ in 0..257 {
            let handle = registry.register(channel_owner_tests::queue(0, &retirement), &memory);
            registry.bind(
                handle,
                ChannelOwner {
                    address: 8,
                    type_id: 1,
                },
            );
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
        registry_compaction::compact_channels(&native);
        registry = native.into_inner().unwrap();
        let mut small = stage(registry.growth_capacity().unwrap(), 0);
        assert!(registry.install(&mut small));
        registry.register_reserved(channel_owner_tests::queue(0, &retirement), &memory);
        assert!(
            registry.records.capacity() <= 8
                && registry.owned.capacity() <= 8
                && registry.unbound.capacity() <= 8
        );
    }
}

#[cfg(test)]
#[path = "channel_admission_tests.rs"]
mod component_tests;
