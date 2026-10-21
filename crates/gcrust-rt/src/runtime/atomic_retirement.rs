//! Stable scalar cells using the shared native record retirement protocol.
#[cfg(test)]
use super::records::Source;
use super::{
    Retirement,
    records::{self, Link, Record},
};
use std::{
    ops::Deref,
    sync::{Arc, atomic::AtomicI64},
};
pub(super) type Queue = records::Queue<Cell>;
pub(in crate::runtime) type Batch = records::Batch<Cell>;
#[repr(C)]
pub(crate) struct Cell {
    value: AtomicI64,
    link: Link<Self>,
}
// SAFETY: native scalar observers never touch the private ownership link.
unsafe impl Record for Cell {
    fn retirement(&self) -> &Link<Self> {
        &self.link
    }
    fn mailbox(state: &mut super::State) -> &mut Queue {
        &mut state.cells
    }
}
impl Cell {
    pub(in crate::runtime) fn new(value: i64, retirement: &Retirement) -> Arc<Self> {
        Arc::new(Self {
            value: AtomicI64::new(value),
            link: Link::new(retirement),
        })
    }
    pub(in crate::runtime) fn pointer(&self) -> *mut AtomicI64 {
        &self.value as *const AtomicI64 as *mut AtomicI64
    }
    /// # Safety
    /// Exclusively removed from its registry, once; transfers ownership to batch.
    pub(in crate::runtime) unsafe fn retire(self: Arc<Self>, batch: &mut Batch) {
        unsafe {
            Link::retire(self, batch);
        }
    }
}
impl Deref for Cell {
    type Target = AtomicI64;
    fn deref(&self) -> &AtomicI64 {
        &self.value
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Monitor, tests as hooks};
    use super::*;
    use crate::gc::IdentityPtrPolicy;
    use crate::runtime::{
        ai_atomic_i64_bind_owner, ai_atomic_i64_load, ai_atomic_i64_new, ai_gc_alloc_fixed,
        allocation_probe, blocking_region,
    };
    use std::{
        sync::atomic::Ordering,
        time::{Duration, Instant},
    };

    fn prepared(value: i64, monitor: &Arc<Monitor>) -> Arc<Cell> {
        let source = Source::new(monitor.clone());
        Arc::new(Cell {
            value: AtomicI64::new(value),
            link: Link::prepared(source),
        })
    }
    #[test]
    fn collector_atomic_retirement_reference_publication_and_batch_merge_do_not_allocate() {
        for count in [0, 1, 17, 257] {
            let monitor = Arc::new(Monitor::new());
            let mut cells: Vec<_> = (0..count).map(|i| prepared(i as i64, &monitor)).collect();
            let weak: Vec<_> = cells.iter().map(Arc::downgrade).collect();
            let mut batch = Batch::default();
            allocation_probe::start();
            let split = count / 2;
            for _ in 0..split {
                unsafe {
                    cells.pop().unwrap().retire(&mut batch);
                }
            }
            drop(batch);
            let mut batch = Batch::default();
            while let Some(cell) = cells.pop() {
                unsafe {
                    cell.retire(&mut batch);
                }
            }
            drop(batch);
            assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            let mut queue = {
                let mut state = monitor.state.lock().unwrap();
                assert_eq!((state.pending, state.producers), (count, count));
                assert_eq!(
                    state.submissions,
                    usize::from(split != 0) + usize::from(count > split)
                );
                std::mem::take(&mut state.cells)
            };
            let mut seen = vec![false; count];
            while let Some(cell) = queue.pop() {
                let value = unsafe { (*cell.pointer()).load(Ordering::SeqCst) } as usize;
                assert!(!seen[value]);
                seen[value] = true;
                assert!(cell.link.is_drained());
                drop(cell);
            }
            assert!(seen.into_iter().all(|value| value));
            assert!(weak.iter().all(|w| w.upgrade().is_none()));
        }
    }
    #[test]
    fn collector_atomic_retirement_reference_sweep_has_no_record_allocator_traffic() {
        for count in [1, 17, 257] {
            let monitor = Arc::new(Monitor::new());
            let mut registry = crate::runtime::AtomicCellRegistry::default();
            let raw: Vec<_> = (0..=count)
                .map(|i| registry.register(prepared(i as i64, &monitor)))
                .collect();
            let mut weak = None;
            for i in 0..count {
                let cell = prepared(42 + i as i64, &monitor);
                if weak.is_none() {
                    weak = Some(Arc::downgrade(&cell));
                }
                let pointer = registry.register(cell);
                registry.bind(pointer, 8usize as *mut u8);
            }
            allocation_probe::start();
            registry.sweep(|_| None);
            assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            assert_eq!(registry.records.len(), raw.len());
            assert!(registry.owned.is_empty());
            assert_eq!(
                weak.as_ref()
                    .unwrap()
                    .upgrade()
                    .unwrap()
                    .load(Ordering::SeqCst),
                42
            );
            for (i, pointer) in raw.into_iter().enumerate() {
                assert_eq!(unsafe { (*pointer).load(Ordering::SeqCst) }, i as i64);
            }
            // The unstarted reference monitor proves retirement ownership.
            // This final native-only clear also tests whole-registry handoff.
            registry.clear();
            monitor.state.lock().unwrap().owner = false;
            monitor.run();
            assert!(weak.unwrap().upgrade().is_none());
            assert_eq!(monitor.state.lock().unwrap().pending, 0);
        }
    }
    #[test]
    fn collector_atomic_retirement_reference_record_storage_and_stable_abi() {
        let service = Retirement::default();
        let warm = Cell::new(0, &service);
        allocation_probe::start();
        let cell = Cell::new(42, &service);
        let traffic = allocation_probe::finish_bytes();
        assert_eq!((traffic.0, traffic.1, traffic.3), (1, 0, 0));
        assert!(traffic.2 >= std::mem::size_of::<Cell>());
        assert_eq!(
            cell.pointer().cast::<Cell>().cast_const(),
            Arc::as_ptr(&cell)
        );
        unsafe {
            (*cell.pointer()).store(81, Ordering::SeqCst);
        }
        assert_eq!(cell.load(Ordering::SeqCst), 81);
        eprintln!(
            "atomic record bytes={}, requested allocation bytes={}",
            std::mem::size_of::<Cell>(),
            traffic.2
        );
        drop(cell);
        drop(warm);
        service.shutdown();
    }
    #[test]
    fn collector_atomic_retirement_reference_shares_worker_and_drain_with_buffers() {
        use crate::runtime::native_memory::{NativeMemory, NativeSlots};
        let service = Retirement::default();
        let memory = Arc::new(NativeMemory::default());
        let mut cells: Vec<_> = (0..17).map(|i| Cell::new(i, &service)).collect();
        let weak: Vec<_> = cells.iter().map(Arc::downgrade).collect();
        let buffers: Vec<_> = (0..3)
            .map(|_| NativeSlots::zeroed_reserved(4097, memory.charge(4097 * 8), &service))
            .collect();
        let (monitor, waiting, release) = hooks::arm(&service);
        let mut first = Batch::default();
        unsafe {
            cells.pop().unwrap().retire(&mut first);
        }
        drop(first);
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut rest = Batch::default();
        for cell in cells {
            unsafe {
                cell.retire(&mut rest);
            }
        }
        drop(rest);
        let mut backing = super::super::Batch::default();
        for buffer in buffers {
            buffer.retire_into(&mut backing);
        }
        drop(backing);
        {
            let state = monitor.state.lock().unwrap();
            assert_eq!(
                (state.pending, state.producers, state.submissions),
                (20, 1, 3)
            );
        }
        release.send(()).unwrap();
        service.drain();
        assert!(weak.iter().all(|w| w.upgrade().is_none()));
        assert_eq!(memory.bytes(), 0);
        service.shutdown();
        assert!(monitor.finished.load(Ordering::Acquire));
    }

    #[test]
    fn collector_atomic_retirement_reference_cancellation_mixed_epochs_and_observers() {
        let service = Retirement::default();
        let old = Cell::new(42, &service);
        let old_monitor = service.monitor().unwrap();
        let old_weak = Arc::downgrade(&old);
        service.shutdown(); // The reserved producer keeps the old worker alive.
        let new = Cell::new(81, &service);
        let new_monitor = service.monitor().unwrap();
        assert!(!Arc::ptr_eq(&old_monitor, &new_monitor));
        let observer = new.clone();
        let new_weak = Arc::downgrade(&new);
        let cancelled = Cell::new(0, &service);
        drop(cancelled); // Unpublished native construction can unwind/cancel.
        let mut batch = Batch::default();
        unsafe {
            old.retire(&mut batch);
            new.retire(&mut batch);
        }
        drop(batch);
        old_monitor.drain();
        service.drain();
        assert!(old_weak.upgrade().is_none());
        assert_eq!(observer.load(Ordering::SeqCst), 81);
        assert_eq!(Arc::strong_count(&observer), 1);
        drop(observer);
        assert!(new_weak.upgrade().is_none());
        service.shutdown();
        assert!(new_monitor.finished.load(Ordering::Acquire));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !old_monitor.finished.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "old native retirement worker did not finish"
            );
            std::thread::yield_now();
        }
    }
    #[test]
    fn collector_atomic_retirement_registry_sweep_does_not_wait_for_native_destruction() {
        if !hooks::child(
            "runtime::native_retirement::atomics::tests::collector_atomic_retirement_registry_sweep_does_not_wait_for_native_destruction",
        ) {
            return;
        }
        let mut context = hooks::runtime();
        let heap = context.heap().clone();
        context.enter(|thread| unsafe {
            let cell = ai_atomic_i64_new(thread, 42);
            {
                let roots = (&*(*thread).dyna_thread).scratch_scope();
                let owner = ai_gc_alloc_fixed(thread, 1, 0);
                ai_atomic_i64_bind_owner(thread, cell, owner);
                roots.push(owner);
                if heap.has_nursery() {
                    heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&*(*thread).dyna_thread);
                }
            }
            let weak =
                Arc::downgrade(&heap.atomic_cells.lock().unwrap().records[&(cell as usize)].0);
            let (monitor, waiting, release) = hooks::arm(&heap.native_retirement);
            let before = heap.collections();
            let other = heap.clone();
            let collector = std::thread::spawn(move || other.stw_collect::<IdentityPtrPolicy>());
            blocking_region(thread, || {
                collector.join().unwrap();
                waiting.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!(heap.atomic_cells.try_lock().unwrap().is_empty());
                assert!(monitor.state.try_lock().is_ok());
                assert_eq!(weak.upgrade().unwrap().load(Ordering::SeqCst), 42);
                // The worker remains stopped before actual deallocation. The
                // coordinator and registry must be available for another GC.
                heap.stw_collect::<IdentityPtrPolicy>();
                assert_eq!(heap.collections() - before, 2);
                release.send(()).unwrap();
                heap.native_retirement.drain();
                assert!(weak.upgrade().is_none());
            });
            // Teardown also retires legacy unbound records through the worker.
            let raw = ai_atomic_i64_new(thread, 81);
            assert_eq!(ai_atomic_i64_load(thread, raw), 81);
        });
        drop(context);
        assert!(heap.atomic_cells.lock().unwrap().is_empty());
    }
}
