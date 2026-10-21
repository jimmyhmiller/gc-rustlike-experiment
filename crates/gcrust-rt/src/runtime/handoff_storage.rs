//! Prepare native handoff storage without retaining an unrooted environment.
use super::metadata_allocator::MetadataAllocator;
use super::{Heap, Mutex, Thread, blocking_region};
use crate::gc::roots::{AtomicRootStorage, RootFrame};
type Vacancies = allocator_api2::vec::Vec<usize, MetadataAllocator>;

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Allocate,
    Discard,
}
#[cfg(test)]
fn checkpoint(heap: &Heap, phase: Phase) {
    tests::checkpoint(heap as *const Heap as usize, phase);
}

pub(crate) struct Slots {
    vacant: Vacancies,
    allocated: usize,
}
impl Default for Slots {
    fn default() -> Self {
        Self {
            vacant: Vacancies::new_in(MetadataAllocator::default()),
            allocated: 0,
        }
    }
}
impl Slots {
    #[cfg(test)]
    pub(crate) fn native_mutex() -> Mutex<Self> {
        Self::native_mutex_in(MetadataAllocator::default())
    }
    pub(crate) fn native_mutex_in(allocator: MetadataAllocator) -> Mutex<Self> {
        let mutex = Mutex::new(Self {
            vacant: Vacancies::new_in(allocator),
            allocated: 0,
        });
        drop(mutex.lock().unwrap());
        mutex
    }
    pub(crate) fn release(&mut self, index: usize) {
        assert!(self.vacant.len() < self.allocated, "handoff returned twice");
        assert!(
            self.vacant.capacity() >= self.allocated,
            "handoff return capacity missing"
        );
        self.vacant.push(index);
    }
    fn needed(&self) -> Option<usize> {
        (self.vacant.capacity() <= self.allocated).then(|| {
            self.allocated
                .checked_add(1)
                .and_then(|n| n.checked_mul(2))
                .expect("handoff slot capacity overflow")
        })
    }
    fn install(&mut self, staged: &mut Vacancies) {
        assert!(staged.is_empty());
        if self.needed().is_some()
            && staged.capacity() > self.allocated
            && self.vacant.allocator().same_counter(staged.allocator())
        {
            staged.extend(self.vacant.drain(..));
            std::mem::swap(&mut self.vacant, staged);
        }
    }
    #[cfg(test)]
    pub(crate) fn backing_bytes(&self) -> usize {
        self.vacant.capacity() * std::mem::size_of::<usize>()
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.vacant.len()
    }
    #[cfg(test)]
    pub(crate) fn as_slice(&self) -> &[usize] {
        &self.vacant
    }
}
#[cfg(test)]
impl std::ops::Index<usize> for Slots {
    type Output = usize;
    fn index(&self, index: usize) -> &usize {
        &self.vacant[index]
    }
}
struct Need {
    slots: Option<usize>,
    roots: Option<usize>,
}
/// No allocation, safepoint or managed-pointer retention on failure.
unsafe fn attempt(heap: &Heap, env: u64) -> Result<usize, Need> {
    let mut slots = heap.thread_handoff_slots.lock().unwrap();
    if let Some(index) = slots.vacant.pop() {
        unsafe {
            heap.globals.set(index, env);
        }
        return Ok(index);
    }
    if slots.needed().is_none() {
        let allocated = slots
            .allocated
            .checked_add(1)
            .expect("handoff count overflow");
        if let Ok(index) = unsafe { heap.globals.try_add_reserved(env) } {
            slots.allocated = allocated;
            return Ok(index);
        }
    }
    Err(Need {
        slots: slots.needed(),
        roots: heap.globals.capacity_for_next(),
    })
}

/// # Safety
/// Owning RUNNING parent; env belongs to this heap. No generated ABI changes:
/// spawn is already a relocating call, and the child reloads the global slot.
pub(super) unsafe fn admit(parent: *mut Thread, heap: &Heap, env: *mut u8) -> usize {
    if let Ok(index) = unsafe { attempt(heap, env as u64) } {
        return index;
    }
    let state = unsafe { &*(*parent).dyna_thread };
    let mut frame = RootFrame::<1>::new();
    frame.slots[0].set(env as u64);
    // Inline storage and a private guard preserve the env across all native
    // staging/disposal and unwinding without consuming a scratch-stack slot.
    unsafe {
        state.frame_chain().with_frame(&mut frame, |frame| {
            let mut root_stage: Option<AtomicRootStorage> = None;
            let mut slot_stage: Option<Vacancies> = None;
            loop {
                if root_stage.is_some() || slot_stage.is_some() {
                    {
                        let mut slots = heap.thread_handoff_slots.lock().unwrap();
                        // A child can have returned a slot while preparation ran.
                        if slots.vacant.is_empty() {
                            if let Some(stage) = slot_stage.as_mut() {
                                slots.install(stage);
                            }
                            if let Some(stage) = root_stage.as_mut() {
                                heap.globals.install_backing(stage);
                            }
                        }
                    }
                    // No root is published and no vacancy is claimed until all
                    // obsolete native buffers have been disposed while BLOCKED.
                    blocking_region(parent, || {
                        #[cfg(test)]
                        checkpoint(heap, Phase::Discard);
                        drop(root_stage.take());
                        drop(slot_stage.take());
                    });
                }
                match attempt(heap, frame.slots[0].get()) {
                    Ok(index) => return index,
                    Err(need) => {
                        blocking_region(parent, || {
                            #[cfg(test)]
                            checkpoint(heap, Phase::Allocate);
                            root_stage = need.roots.map(|capacity| {
                                AtomicRootStorage::with_capacity_in(
                                    capacity,
                                    heap.globals.allocator(),
                                )
                            });
                            slot_stage = need.slots.map(|capacity| {
                                let allocator = heap
                                    .thread_handoff_slots
                                    .lock()
                                    .unwrap()
                                    .vacant
                                    .allocator()
                                    .clone();
                                Vacancies::with_capacity_in(capacity, allocator)
                            });
                        });
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo},
        runtime::{
            RuntimeContext, ThreadHandoff, ai_gc_alloc_fixed, allocation_probe, channel_buffer,
        },
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    };
    struct Hook {
        heap: usize,
        phase: Phase,
        ready: Sender<()>,
        done: Arc<AtomicBool>,
    }
    static HOOK: Mutex<Option<Hook>> = Mutex::new(None);
    pub(super) fn checkpoint(heap: usize, phase: Phase) {
        let hook = {
            let mut guard = HOOK.lock().unwrap();
            if guard
                .as_ref()
                .is_some_and(|h| h.heap == heap && h.phase == phase)
            {
                guard.take()
            } else {
                None
            }
        };
        if let Some(hook) = hook {
            hook.ready.send(()).unwrap();
            while !hook.done.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
    }
    fn runtime_registration_backing(heap: &crate::gc::Heap) -> usize {
        // Fixture checkpoints own RUNNING registrations or observe BLOCKED
        // ones under the census mutex. Release it before any assertion.
        let registrations = {
            let census = heap.threads.lock().unwrap();
            census.capacity() * std::mem::size_of::<crate::gc::ThreadStateHandle>()
                + census.iter().map(|state| unsafe { state.native_backing_bytes() } + crate::gc::ThreadStateHandle::allocation_bytes()).sum::<usize>()
        };
        // Both callers retain exactly one RuntimeContext and its Thread ABI storage.
        heap.collector_object_lock_backing_bytes() + heap.collector_layout_backing_bytes() + heap.collector_abi_backing_bytes() + registrations + crate::abi_storage::AbiStorage::<std::cell::UnsafeCell<Thread>>::allocation_bytes() + heap.retired_profile_backing_bytes() + unsafe { heap.collector_tlab_metadata_bytes() }
    }

    #[test]
    fn collector_handoff_storage_reference_requested_bytes_survive_execution_until_heap_drop() {
        for generational in [false, true] {
            let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
            let mut runtime = if generational {
                RuntimeContext::new_generational(8192, 65536, vec![info])
            } else {
                RuntimeContext::new(65536, vec![info])
            };
            let heap = runtime.heap().clone();
            runtime.enter(|thread| unsafe {
                let env = ai_gc_alloc_fixed(thread, 0, 0);
                // The cfg(test) phase hook has its own lazily allocated mutex
                // backend; initialize instrumentation outside the measured path.
                drop(HOOK.lock().unwrap());
                allocation_probe::start();
                let handoff = ThreadHandoff::new(thread, heap.clone(), env);
                let traffic = allocation_probe::finish_bytes();
                let bytes = heap.globals.backing_bytes()
                    + heap.thread_handoff_slots.lock().unwrap().backing_bytes();
                assert_eq!(
                    traffic.0, 2,
                    "generational={generational}, traffic={traffic:?}, backing={bytes}"
                );
                assert_eq!(traffic.2, bytes);
                assert_eq!(heap.native_registry_bytes(), bytes + runtime_registration_backing(&heap));
                assert_eq!(heap.native_buffer_bytes(), 0);
                allocation_probe::start();
                drop(handoff);
                assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            });
            let retained = heap.native_registry_bytes();
            assert!(retained > 0);
            unsafe {
                runtime.finish_threads();
            }
            assert_eq!(heap.native_registry_bytes(), retained);
            let retained_storage = heap.globals.backing_bytes()
                + heap.thread_handoff_slots.lock().unwrap().backing_bytes();
            assert_eq!(heap.native_registry_bytes(), retained_storage + runtime_registration_backing(&heap));
            let retained_roots = heap.globals.backing_bytes()
                + heap.thread_handoff_slots.lock().unwrap().backing_bytes();
            let counter = heap.native_memory.registry.clone();
            let weak = Arc::downgrade(&heap);
            drop(runtime);
            assert_eq!(counter.bytes(), heap.collector_object_lock_backing_bytes() + heap.collector_layout_backing_bytes() + heap.collector_abi_backing_bytes() + retained_roots + heap.retired_profile_backing_bytes() + unsafe { heap.collector_tlab_metadata_bytes() });
            drop(heap);
            assert!(weak.upgrade().is_none());
            assert_eq!(counter.bytes(), 0);
        }
    }
    #[test]
    fn collector_handoff_storage_reference_foreign_accounting_stages_are_rejected() {
        let a = Arc::<super::super::allocation_counter::AllocationCounter>::default();
        let b = Arc::<super::super::allocation_counter::AllocationCounter>::default();
        let roots = crate::gc::roots::AtomicRootSet::with_allocator(MetadataAllocator::new(&a));
        let pool = Slots::native_mutex_in(MetadataAllocator::new(&a));
        let mut root_stage = AtomicRootStorage::with_capacity_in(8, MetadataAllocator::new(&b));
        let mut slot_stage = Vacancies::with_capacity_in(8, MetadataAllocator::new(&b));
        allocation_probe::start();
        assert!(!unsafe { roots.install_backing(&mut root_stage) });
        pool.lock().unwrap().install(&mut slot_stage);
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert!(roots.is_empty());
        assert_eq!(roots.backing_bytes(), 0);
        assert_eq!(pool.lock().unwrap().backing_bytes(), 0);
        assert_eq!(a.bytes(), 0);
        assert!(b.bytes() > 0);
        drop((root_stage, slot_stage));
        assert_eq!(b.bytes(), 0);
    }
    #[test]
    fn collector_handoff_storage_reference_staged_buffers_do_not_retain_heap() {
        let heap = Arc::new(Heap::new::<Full>(8192, vec![]));
        let weak = Arc::downgrade(&heap);
        let counter = heap.native_memory.registry.clone();
        let capacity = super::super::native_memory::MIN_THRESHOLD
            / std::mem::size_of::<std::sync::atomic::AtomicU64>()
            + 1;
        let roots = AtomicRootStorage::with_capacity_in(capacity, heap.globals.allocator());
        let vacancies = Vacancies::with_capacity_in(
            capacity,
            heap.thread_handoff_slots
                .lock()
                .unwrap()
                .vacant
                .allocator()
                .clone(),
        );
        let root_bytes = capacity * std::mem::size_of::<std::sync::atomic::AtomicU64>();
        let vacancy_bytes = capacity * std::mem::size_of::<usize>();
        assert_eq!(counter.bytes(), heap.collector_object_lock_backing_bytes() + heap.collector_layout_backing_bytes() + heap.collector_abi_backing_bytes() + root_bytes + vacancy_bytes);
        assert!(heap.native_memory.needs_collection(0));
        drop(heap);
        assert!(weak.upgrade().is_none());
        assert_eq!(counter.bytes(), root_bytes + vacancy_bytes);
        drop(roots);
        assert_eq!(counter.bytes(), vacancy_bytes);
        drop(vacancies);
        assert_eq!(counter.bytes(), 0);
    }
    #[test]
    fn collector_handoff_storage_reference_transfer_and_returns_do_not_allocate() {
        for count in [0, 1, 17, 257] {
            let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
            let heap = Heap::new::<Full>(1 << 20, vec![info.clone()]);
            let mut expected = Vec::new();
            for _ in 0..count {
                let object = unsafe { heap.alloc_obj::<Full>(&info, 0) };
                expected.push(object as u64);
                unsafe {
                    heap.globals.add(object as u64);
                }
            }
            while heap.globals.capacity_for_next().is_none() {
                unsafe {
                    heap.globals.add(0);
                }
                expected.push(0);
            }
            let mut root_stage = AtomicRootStorage::with_capacity_in(
                2 * (expected.len() + 1),
                heap.globals.allocator(),
            );
            let mut slots = Slots {
                vacant: Vacancies::with_capacity_in(count, MetadataAllocator::default()),
                allocated: count,
            };
            let original: Vec<_> = (0..count).step_by(3).collect();
            slots.vacant.extend(original.iter().copied());
            let mut slot_stage =
                Vacancies::with_capacity_in(2 * (count + 1), slots.vacant.allocator().clone());
            allocation_probe::start();
            assert!(unsafe { heap.globals.install_backing(&mut root_stage) });
            assert_eq!(
                unsafe { heap.globals.try_add_reserved(0) },
                Ok(expected.len())
            );
            slots.install(&mut slot_stage);
            assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            assert!(root_stage.is_empty() && slot_stage.is_empty());
            assert_eq!(slots.vacant.as_slice(), original.as_slice());
            for (index, bits) in expected.iter().enumerate() {
                assert_eq!(heap.globals.get(index), *bits);
            }
            allocation_probe::start();
            for index in 0..count {
                if index % 3 != 0 {
                    slots.release(index);
                }
            }
            assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            assert_eq!(slots.len(), count);
        }
    }
    #[test]
    fn collector_handoff_storage_reference_rejects_insufficient_and_obsolete_backings() {
        let roots = crate::gc::roots::AtomicRootSet::new();
        let mut small = AtomicRootStorage::with_capacity_in(1, roots.allocator());
        for _ in 0..17 {
            unsafe {
                roots.add(0);
            }
        }
        while roots.capacity_for_next().is_none() {
            unsafe {
                roots.add(0);
            }
        }
        assert!(!unsafe { roots.install_backing(&mut small) });
        assert!(small.is_empty());
        // Another admission already made room: do not replace it just because
        // a competing stage is large enough for the current length.
        unsafe {
            roots.add(0);
        }
        let mut stage = AtomicRootStorage::with_capacity_in(128, roots.allocator());
        assert!(roots.capacity_for_next().is_none());
        assert!(!unsafe { roots.install_backing(&mut stage) });
        let mut slots = Slots {
            vacant: Vacancies::with_capacity_in(17, MetadataAllocator::default()),
            allocated: 17,
        };
        slots.install(&mut Vacancies::with_capacity_in(
            1,
            slots.vacant.allocator().clone(),
        ));
        assert_eq!(slots.vacant.capacity(), 17);
        slots.vacant.reserve(128);
        let capacity = slots.vacant.capacity();
        slots.install(&mut Vacancies::with_capacity_in(
            64,
            slots.vacant.allocator().clone(),
        ));
        assert_eq!(slots.vacant.capacity(), capacity);
    }
    #[test]
    fn collector_handoff_storage_reference_prepared_mutexes_and_reuse() {
        let pool = Slots::native_mutex();
        let roots = crate::gc::roots::AtomicRootSet::new();
        allocation_probe::start();
        drop(pool.lock().unwrap());
        assert_eq!(roots.capacity_for_next(), Some(2));
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        let heap = Heap::new::<Full>(65536, vec![]);
        {
            let mut slots = heap.thread_handoff_slots.lock().unwrap();
            slots.vacant.reserve(1);
            slots.allocated = 1;
            slots.vacant.push(unsafe { heap.globals.add(0) });
        }
        allocation_probe::start();
        let index = unsafe { attempt(&heap, 0) }.ok().unwrap();
        heap.thread_handoff_slots.lock().unwrap().release(index);
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert_eq!(heap.globals.len(), 1);
        // Space in the global table is insufficient by itself: every new
        // handoff must also reserve a future return, even with no vacancies.
        {
            let mut slots = heap.thread_handoff_slots.lock().unwrap();
            slots.vacant = Vacancies::with_capacity_in(1, slots.vacant.allocator().clone());
            assert_eq!(slots.allocated, 1);
        }
        allocation_probe::start();
        assert!(unsafe { attempt(&heap, 0) }.is_err());
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert_eq!(heap.globals.len(), 1);
    }
    #[test]
    fn collector_handoff_storage_native_phases_preserve_the_only_env_root() {
        if !channel_buffer::tests::child(
            "runtime::handoff_storage::tests::collector_handoff_storage_native_phases_preserve_the_only_env_root",
        ) {
            return;
        }
        for phase in [Phase::Allocate, Phase::Discard] {
            let generational = std::env::var("GCR_CHANNEL_BUFFER_CHILD").as_deref() == Ok("1");
            let types = vec![TypeInfo::for_header(Full::SIZE).with_raw_bytes(8)];
            let mut context = if generational {
                RuntimeContext::new_generational(32768, 2 << 20, types)
            } else {
                RuntimeContext::new(2 << 20, types)
            };
            let heap = context.heap().clone();
            context.enter(|parent| unsafe {
                // Make disposal free real old buffers, rather than only the
                // zero-capacity initial backing. These model other active
                // handoffs whose null roots stay owned until this test returns.
                let initial = if phase == Phase::Discard {
                    heap.globals.add(0);
                    while heap.globals.capacity_for_next().is_none() {
                        heap.globals.add(0);
                    }
                    let count = heap.globals.len();
                    let mut slots = heap.thread_handoff_slots.lock().unwrap();
                    slots.allocated = count;
                    slots.vacant =
                        Vacancies::with_capacity_in(count, slots.vacant.allocator().clone());
                    count
                } else {
                    0
                };
                let env = ai_gc_alloc_fixed(parent, 0, 0);
                env.add(Full::SIZE).cast::<u64>().write(42);
                let old_address = env as usize;
                let before = if generational {
                    heap.minor_collections()
                } else {
                    heap.collections()
                };
                let (ready, waiting) = std::sync::mpsc::channel();
                let done = Arc::new(AtomicBool::new(false));
                *HOOK.lock().unwrap() = Some(Hook {
                    heap: Arc::as_ptr(&heap) as usize,
                    phase,
                    ready,
                    done: done.clone(),
                });
                let other = heap.clone();
                let completed = done.clone();
                let collector = std::thread::spawn(move || {
                    waiting.recv().unwrap();
                    assert!(
                        other.thread_handoff_slots.try_lock().is_ok(),
                        "native preparation retained the handoff-pool lock"
                    );
                    let added = if phase == Phase::Allocate {
                        for _ in 0..65 {
                            other.globals.add(0);
                        }
                        while other.globals.capacity_for_next().is_none() {
                            other.globals.add(0);
                        }
                        let count = other.globals.len();
                        let mut slots = other.thread_handoff_slots.lock().unwrap();
                        assert_eq!(slots.allocated, 0);
                        slots.allocated = count;
                        slots.vacant =
                            Vacancies::with_capacity_in(count, slots.vacant.allocator().clone());
                        count
                    } else {
                        initial
                    };
                    let (state, _) = other.register_thread();
                    if generational {
                        other.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state);
                    } else {
                        other.mutator_triggered_gc::<IdentityPtrPolicy>(&state);
                    }
                    other.safe_deregister_thread(&state);
                    completed.store(true, Ordering::Release);
                    added
                });
                let handoff = ThreadHandoff::new(parent, heap.clone(), env);
                assert!(done.load(Ordering::Acquire));
                let added = blocking_region(parent, || collector.join().unwrap());
                let index = handoff.slot.unwrap();
                let moved = heap.globals.get(index) as *mut u8;
                assert_ne!(
                    moved as usize, old_address,
                    "temporary env root was not reloaded"
                );
                assert_eq!(moved.add(Full::SIZE).cast::<u64>().read(), 42);
                assert_eq!(
                    if generational {
                        heap.minor_collections()
                    } else {
                        heap.collections()
                    },
                    before + 1
                );
                allocation_probe::start();
                drop(handoff);
                for index in 0..added {
                    heap.thread_handoff_slots.lock().unwrap().release(index);
                }
                assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
                assert_eq!(heap.globals.get(index), 0);
                assert_eq!(heap.thread_handoff_slots.lock().unwrap().len(), added + 1);
            });
        }
    }
}
