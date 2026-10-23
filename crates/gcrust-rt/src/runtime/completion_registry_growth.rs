//! Stage completion admission without allocating under collector-visible locks.
use super::native_metadata::{self, MetadataAllocator};
use super::{
    Arc, CompletionRegistry, Heap, JoinHandle, Mutex, Thread, blocking_region, completion_notice,
    completion_reaper,
    registry_stage::{RegistryStage, growth_request},
};
use std::sync::atomic::Ordering;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Reaper,
    Notice,
    Spawn,
    Record,
    Catalogue,
    Stage,
    Discard,
    NoticePublish,
}
#[cfg(test)]
pub(super) fn checkpoint(heap: usize, phase: Phase) {
    tests::checkpoint(heap, phase);
}

pub(super) fn new_record(
    join: std::thread::JoinHandle<u64>,
    _heap_id: usize,
    retirement: &super::native_retirement::Retirement,
) -> Arc<JoinHandle> {
    #[cfg(test)]
    checkpoint(_heap_id, Phase::Record);
    let record = Arc::new(JoinHandle {
        completion_notice: true,
        daemon: std::sync::atomic::AtomicBool::new(false),
        native_joined: std::sync::atomic::AtomicBool::new(false),
        pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
        reaper_registration: std::sync::OnceLock::new(),
        retirement: std::sync::OnceLock::new(),
        state: Mutex::new(super::JoinState::Running(join)),
        owner: std::sync::atomic::AtomicUsize::new(1),
    });
    record
        .retirement
        .get_or_init(|| super::native_retirement::RecordLink::new(retirement));
    drop(record.state.lock().unwrap());
    record
}

struct Tables {
    records: native_metadata::Map<Arc<JoinHandle>>,
    pending: native_metadata::Vec<usize>,
    owned: native_metadata::Vec<usize>,
}
impl Tables {
    #[cfg(test)]
    fn new(capacity: usize) -> Self {
        Self::components([capacity; 3])
    }
    #[cfg(test)]
    fn components(plan: [usize; 3]) -> Self {
        Self::in_allocator(plan, MetadataAllocator::default())
    }
    fn in_allocator(plan: [usize; 3], allocator: MetadataAllocator) -> Self {
        Self {
            records: native_metadata::map(plan[0], allocator.clone()),
            pending: native_metadata::vector(plan[1], allocator.clone()),
            owned: native_metadata::vector(plan[2], allocator.clone()),
        }
    }
}
impl CompletionRegistry {
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
            && self.pending.capacity() > self.pending.len()
            && self.owned.capacity() > self.records.len()
    }
    #[cfg(test)]
    fn growth_capacity(&self) -> usize {
        self.records
            .len()
            .checked_add(1)
            .and_then(|n| n.checked_mul(2))
            .expect("completion registry capacity overflow")
    }
    fn growth_plan(&self) -> [usize; 3] {
        let live = self.records.len();
        [
            growth_request(self.records.capacity(), live),
            growth_request(self.pending.capacity(), self.pending.len()),
            growth_request(self.owned.capacity(), live),
        ]
    }
    fn install(&mut self, staged: &mut RegistryStage<Tables>) -> bool {
        let live = self.records.len();
        let pending = self.pending.len();
        if !staged.within_budget([live, pending, live]) {
            return false;
        }
        let tables = &mut staged.tables;
        assert!(tables.records.is_empty() && tables.pending.is_empty() && tables.owned.is_empty());
        if (self.records.capacity() <= live
            && (tables.records.capacity() <= live
                || !self
                    .records
                    .allocator()
                    .same_counter(tables.records.allocator())))
            || (self.pending.capacity() <= pending
                && (tables.pending.capacity() <= pending
                    || !self
                        .pending
                        .allocator()
                        .same_counter(tables.pending.allocator())))
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
        if self.pending.capacity() <= pending {
            // Preserve dense order and the positions stored in native records.
            tables.pending.extend(self.pending.drain(..));
            std::mem::swap(&mut self.pending, &mut tables.pending);
        }
        if self.owned.capacity() <= live {
            tables.owned.extend(self.owned.drain(..));
            std::mem::swap(&mut self.owned, &mut tables.owned);
        }
        debug_assert!(self.can_register());
        true
    }
}

pub(super) fn remove_pending(
    records: &native_metadata::Map<Arc<JoinHandle>>,
    pending: &mut native_metadata::Vec<usize>,
    key: usize,
) {
    let record = records.get(&key).expect("removed completion missing");
    let index = record.pending_index.swap(usize::MAX, Ordering::Relaxed);
    if index == usize::MAX {
        return;
    }
    assert_eq!(pending[index], key, "completion pending index mismatch");
    let tail = pending.pop().unwrap();
    if index < pending.len() {
        pending[index] = tail;
        records[&tail].pending_index.store(index, Ordering::Relaxed);
    }
}

/// Parent's existing admission covers monitor installation and publication.
/// No managed pointer is retained in this native preparation phase.
unsafe fn admission_monitor(
    thread: *mut Thread,
    heap: &Heap,
) -> (
    Arc<completion_reaper::Monitor>,
    Option<completion_reaper::Reaper>,
) {
    let _heap_id = heap as *const Heap as usize;
    let mut candidate = None;
    loop {
        let mut registry = heap.thread_completions.lock().unwrap();
        if registry.reaper.is_none() {
            if let Some(reaper) = candidate.take() {
                registry.reaper = Some(reaper);
            }
        }
        if let Some(reaper) = &registry.reaper {
            let monitor = reaper.monitor();
            drop(registry);
            return (monitor, candidate);
        }
        drop(registry);
        candidate = Some(unsafe {
            blocking_region(thread, || {
                #[cfg(test)]
                checkpoint(_heap_id, Phase::Reaper);
                completion_reaper::Reaper::new()
            })
        });
    }
}

pub(super) unsafe fn create(
    thread: *mut Thread,
    heap: &Heap,
    mut handoff: super::ThreadHandoff,
    arguments: Arc<Vec<std::ffi::OsString>>,
    code_bits: usize,
) -> (completion_notice::CompletionNotice, Arc<JoinHandle>) {
    let (monitor, unused) = unsafe { admission_monitor(thread, heap) };
    let heap_id = heap as *const Heap as usize;
    unsafe {
        blocking_region(thread, || {
            #[cfg(test)]
            if unused.is_some() {
                checkpoint(heap_id, Phase::Discard);
            }
            drop(unused);
            #[cfg(test)]
            checkpoint(heap_id, Phase::Notice);
            let (notice, on_exit) = completion_notice::CompletionNotice::new(monitor.clone());
            // Creating this closure moves only native owners and the already
            // registered handoff slot. The child reloads its managed env later.
            let child = move || -> u64 {
                let _notify = on_exit;
                let thread = super::RuntimeThread::new(handoff.heap.clone(), arguments);
                let tptr = thread.thread.get();
                let dyna = &thread.registration.dyna;
                super::with_current_thread(tptr, || {
                    let scratch = dyna.scratch_scope();
                    let slot = handoff.transfer_to_scratch(dyna);
                    drop(handoff);
                    let env = scratch.get(slot);
                    let body: super::ClosureFn = core::mem::transmute(code_bits);
                    body(tptr, env)
                })
            };
            #[cfg(test)]
            checkpoint(heap_id, Phase::Spawn);
            let join = std::thread::spawn(child);
            let record = new_record(join, heap_id, &heap.native_retirement);
            #[cfg(test)]
            checkpoint(heap_id, Phase::Catalogue);
            monitor.prepare(&record);
            (notice, record)
        })
    }
}

pub(super) unsafe fn publish(thread: *mut Thread, heap: &Heap, completion: Arc<JoinHandle>) {
    let _heap_id = heap as *const Heap as usize;
    let mut staged = None;
    loop {
        let mut registry = heap.thread_completions.lock().unwrap();
        if !registry.can_register() {
            if let Some(tables) = staged.as_mut() {
                registry.install(tables);
            }
        }
        if registry.can_register() {
            registry.register_reserved(completion);
            drop(registry);
            if let Some(tables) = staged {
                unsafe {
                    blocking_region(thread, || {
                        #[cfg(test)]
                        checkpoint(_heap_id, Phase::Discard);
                        drop(tables)
                    })
                };
            }
            return;
        }
        let plan = registry.growth_plan();
        let allocator = registry.records.allocator().clone();
        drop(registry);
        staged = Some(unsafe {
            blocking_region(thread, || {
                if staged.is_some() {
                    #[cfg(test)]
                    checkpoint(_heap_id, Phase::Discard);
                }
                drop(staged);
                #[cfg(test)]
                checkpoint(_heap_id, Phase::Stage);
                RegistryStage::components(plan, Tables::in_allocator(plan, allocator))
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
        atomic::{AtomicBool, AtomicUsize},
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
            let mut h = HOOK.lock().unwrap();
            if h.as_ref()
                .is_some_and(|h| h.heap == heap && h.phase == phase)
            {
                h.take()
            } else {
                None
            }
        };
        if let Some(h) = hook {
            h.ready.send(()).unwrap();
            while !h.done.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
    }
    fn record(value: u64, owner: usize) -> Arc<JoinHandle> {
        Arc::new(JoinHandle {
            completion_notice: false,
            daemon: std::sync::atomic::AtomicBool::new(false),
            native_joined: std::sync::atomic::AtomicBool::new(false),
            pending_index: AtomicUsize::new(usize::MAX),
            reaper_registration: std::sync::OnceLock::new(),
            retirement: std::sync::OnceLock::new(),
            state: Mutex::new(JoinState::Complete(value)),
            owner: AtomicUsize::new(owner),
        })
    }
    #[test]
    fn collector_completion_registry_reference_transfer_and_late_binding_do_not_allocate() {
        for count in [0, 1, 17, 257] {
            let retirement = super::super::native_retirement::Retirement::default();
            let mut registry = CompletionRegistry::default();
            registry.ensure_reaper();
            let monitor = registry.reaper.as_ref().unwrap().monitor();
            let mut keys = Vec::new();
            for i in 0..count {
                let r = record(i as u64, if i == 0 { 8 } else { 1 });
                keys.push(Arc::as_ptr(&r) as usize);
                registry.register(r, &retirement);
            }
            let capacity = 2 * (count + 1);
            let mut stage = RegistryStage::new(capacity, Tables::new(capacity));
            let extra = record(73, 1);
            extra
                .retirement
                .get_or_init(|| super::super::native_retirement::RecordLink::new(&retirement));
            let key = Arc::as_ptr(&extra) as usize;
            allocation_probe::start();
            assert!(registry.install(&mut stage));
            registry.register_reserved(extra);
            assert!(Arc::ptr_eq(
                &monitor,
                &registry.reaper.as_ref().unwrap().monitor()
            ));
            let traffic = allocation_probe::finish_bytes();
            assert_eq!(traffic, (0, 0, 0, 0));
            assert!(
                stage.tables.records.is_empty()
                    && stage.tables.pending.is_empty()
                    && stage.tables.owned.is_empty()
            );
            for (index, key) in registry.pending.iter().enumerate() {
                assert_eq!(
                    registry.records[key].pending_index.load(Ordering::Relaxed),
                    index
                );
            }
            for (index, &key) in keys.iter().enumerate() {
                assert_eq!(
                    registry.records[&key].pending_index.load(Ordering::Relaxed),
                    index
                );
                assert_eq!(Arc::strong_count(&registry.records[&key]), 1);
            }
            registry.owned.reserve(8 * registry.records.len());
            registry.sweep(|_| true);
            let native = Mutex::new(std::mem::take(&mut registry));
            registry_compaction::compact_completions(&native);
            registry = native.into_inner().unwrap();
            allocation_probe::start();
            for &key in keys.iter().skip(1) {
                registry.bind(key as *mut JoinHandle, 8usize as *mut u8);
            }
            registry.bind(key as *mut JoinHandle, 8usize as *mut u8);
            let traffic = allocation_probe::finish_bytes();
            assert_eq!(traffic, (0, 0, 0, 0));
            assert_eq!(registry.owned.len(), count + 1);
        }
    }
    #[test]
    fn collector_completion_registry_reference_pending_positions_match_swap_removal_and_shutdown() {
        for count in [1, 17, 257] {
            let retirement = super::super::native_retirement::Retirement::default();
            let mut registry = CompletionRegistry::default();
            let records: Vec<_> = (0..count).map(|i| record(i as u64, 8)).collect();
            for r in &records {
                registry.register(r.clone(), &retirement);
            }
            let first = registry.next_pending().unwrap();
            assert_eq!(first.pending_index.load(Ordering::Relaxed), usize::MAX);
            let removed: std::collections::HashSet<_> = records
                .iter()
                .step_by(3)
                .map(|r| Arc::as_ptr(r) as usize)
                .collect();
            registry.sweep(|r| !removed.contains(&(Arc::as_ptr(r) as usize)));
            for (index, &key) in registry.pending.iter().enumerate() {
                assert_eq!(
                    registry.records[&key].pending_index.load(Ordering::Relaxed),
                    index
                );
            }
            let mut observed = std::collections::HashSet::new();
            observed.insert(Arc::as_ptr(&first) as usize);
            while let Some(r) = registry.next_pending() {
                assert!(observed.insert(Arc::as_ptr(&r) as usize));
                assert_eq!(r.pending_index.load(Ordering::Relaxed), usize::MAX);
            }
            for r in &records {
                assert_eq!(
                    observed.contains(&(Arc::as_ptr(r) as usize)),
                    !removed.contains(&(Arc::as_ptr(r) as usize)) || Arc::ptr_eq(r, &first)
                );
            }
            assert!(registry.pending.is_empty());
        }
    }
    #[test]
    fn collector_completion_registry_reference_replans_after_gc_shrink() {
        let retirement = super::super::native_retirement::Retirement::default();
        let mut registry = CompletionRegistry::default();
        for _ in 0..257 {
            registry.register(record(0, 8), &retirement);
        }
        let capacity = registry.growth_capacity();
        let mut stage = RegistryStage::new(capacity, Tables::new(capacity));
        registry.sweep(|_| false);
        assert!(!registry.install(&mut stage));
        let native = Mutex::new(std::mem::take(&mut registry));
        registry_compaction::compact_completions(&native);
        registry = native.into_inner().unwrap();
        let capacity = registry.growth_capacity();
        let mut stage = RegistryStage::new(capacity, Tables::new(capacity));
        assert!(registry.install(&mut stage));
        let extra = record(42, 1);
        extra
            .retirement
            .get_or_init(|| super::super::native_retirement::RecordLink::new(&retirement));
        registry.register_reserved(extra);
        assert!(
            registry.records.capacity() <= 8
                && registry.pending.capacity() <= 8
                && registry.owned.capacity() <= 8
        );
    }
    #[test]
    fn collector_completion_registry_reference_native_mutexes_are_prepared() {
        let mutex = CompletionRegistry::native_mutex();
        let record = new_record(
            std::thread::spawn(|| 42),
            0,
            &super::super::native_retirement::Retirement::default(),
        );
        allocation_probe::start();
        drop(mutex.lock().unwrap());
        record.is_complete();
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert_eq!(record.join(), 42);
    }
    unsafe extern "C" fn child(_thread: *mut Thread, env: *mut u8) -> u64 {
        unsafe { env.add(Full::SIZE).cast::<u64>().read() }
    }
    #[test]
    fn collector_completion_registry_native_phases_allow_collection() {
        if !channel_buffer::tests::child(
            "runtime::completion_registry_growth::tests::collector_completion_registry_native_phases_allow_collection",
        ) {
            return;
        }
        for phase in [
            Phase::Reaper,
            Phase::Notice,
            Phase::Spawn,
            Phase::Record,
            Phase::Catalogue,
            Phase::Stage,
            Phase::Discard,
            Phase::NoticePublish,
        ] {
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
                let (ready, waiting) = std::sync::mpsc::channel();
                let done = Arc::new(AtomicBool::new(false));
                let before = if generational {
                    heap.minor_collections()
                } else {
                    heap.collections()
                };
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
                    let added = if phase == Phase::Reaper {
                        let competitor = completion_reaper::Reaper::new();
                        let mut registry = other.thread_completions.lock().unwrap();
                        assert!(registry.reaper.replace(competitor).is_none());
                        0
                    } else if phase == Phase::Stage {
                        let mut registry = other.thread_completions.lock().unwrap();
                        for _ in 0..65 {
                            registry.register(record(0, 1), &other.native_retirement);
                        }
                        while registry.can_register() {
                            registry.register(record(0, 1), &other.native_retirement);
                        }
                        registry.records.len()
                    } else {
                        0
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
                let handle = ai_thread_spawn(thread, roots.get(witness), child as *const u8);
                assert!(done.load(Ordering::Acquire), "phase {phase:?} did not run");
                let added = blocking_region(thread, || collector.join().unwrap());
                {
                    let registry = heap.thread_completions.lock().unwrap();
                    assert_eq!(registry.records.len(), added + 1);
                    for (index, key) in registry.pending.iter().enumerate() {
                        assert_eq!(
                            registry.records[key].pending_index.load(Ordering::Relaxed),
                            index
                        );
                    }
                }
                assert_eq!(
                    if generational {
                        heap.minor_collections()
                    } else {
                        heap.collections()
                    },
                    before + 1
                );
                assert_eq!(ai_thread_join(thread, handle), 42);
                assert_eq!(roots.get(witness).add(Full::SIZE).cast::<u64>().read(), 42);
                ai_thread_bind_owner(thread, handle, roots.get(witness));
            });
        }
    }
}

#[cfg(test)]
#[path = "completion_admission_tests.rs"]
mod component_tests;
