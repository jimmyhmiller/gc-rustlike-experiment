use super::{
    records::{Link, Source},
    tests as hooks, *,
};
use crate::{
    gc::IdentityPtrPolicy,
    runtime::{self, allocation_probe, blocking_region},
};
use std::sync::atomic::Ordering;

#[test]
fn collector_channel_retirement_reference_sweep_and_clear_do_not_free_records() {
    for count in [1, 17, 257] {
        let monitor = Arc::new(Monitor::new());
        let memory = Arc::new(runtime::native_memory::NativeMemory::default());
        let mut registry = runtime::ChannelRegistry::default();
        let prepare = |value| {
            runtime::channel_owner_tests::queue_with_link(
                value,
                Link::prepared(Source::new(monitor.clone())),
            )
        };
        let raw: Vec<_> = (0..=count)
            .map(|i| registry.register(prepare(i as u64), &memory))
            .collect();
        let mut weak = None;
        for _ in 0..count {
            let control = prepare(42);
            if weak.is_none() {
                weak = Some(Arc::downgrade(&control));
            }
            let handle = registry.register(control, &memory);
            registry.bind(
                handle,
                runtime::ChannelOwner {
                    address: 8,
                    type_id: 1,
                },
            );
        }
        let bytes = memory.bytes();
        allocation_probe::start();
        registry.sweep(|_| None);
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert_eq!(registry.records.len(), raw.len());
        assert!(registry.owned.is_empty());
        assert_eq!(
            memory.bytes(),
            bytes,
            "collector freed inline queue backing"
        );
        assert!(weak.as_ref().unwrap().upgrade().is_some());
        for (i, handle) in raw.iter().enumerate() {
            let control = &registry.records[&(*handle as usize)].0;
            assert_eq!(
                control.slots.read().unwrap().as_ref().unwrap()[0].load(Ordering::SeqCst),
                i as u64
            );
        }
        registry.clear();
        monitor.state.lock().unwrap().owner = false;
        monitor.run();
        assert_eq!(memory.bytes(), 0);
        assert!(weak.unwrap().upgrade().is_none());
        assert_eq!(monitor.state.lock().unwrap().pending, 0);
    }
}

#[test]
fn collector_channel_retirement_reference_mixed_mailboxes_share_one_source() {
    let service = Retirement::default();
    let memory = Arc::new(runtime::native_memory::NativeMemory::default());
    let control = runtime::channel_sync::construct(1, 0, 0, memory.charge(8), &service);
    let weak_control = Arc::downgrade(&control);
    let cell = AtomicCell::new(42, &service);
    let weak_cell = Arc::downgrade(&cell);
    let buffer = runtime::native_memory::NativeSlots::zeroed_reserved(
        4097,
        memory.charge(4097 * 8),
        &service,
    );
    let (monitor, waiting, release) = hooks::arm(&service);
    let mut controls = ChannelBatch::default();
    unsafe {
        RecordLink::retire(control, &mut controls);
    }
    drop(controls);
    waiting
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let mut cells = AtomicBatch::default();
    unsafe {
        cell.retire(&mut cells);
    }
    drop(cells);
    let mut buffers = Batch::default();
    buffer.retire_into(&mut buffers);
    drop(buffers);
    {
        let state = monitor.state.lock().unwrap();
        assert_eq!(
            (state.pending, state.producers, state.submissions),
            (3, 1, 3)
        );
    }
    assert!(weak_control.upgrade().is_some());
    assert_eq!(weak_cell.upgrade().unwrap().load(Ordering::SeqCst), 42);
    release.send(()).unwrap();
    service.drain();
    assert!(weak_control.upgrade().is_none());
    assert!(weak_cell.upgrade().is_none());
    assert_eq!(memory.bytes(), 0);
    service.shutdown();
    assert!(monitor.finished.load(Ordering::Acquire));
}

#[test]
fn collector_channel_retirement_native_metadata_pause_allows_collection() {
    if !hooks::child(
        "runtime::native_retirement::channel_records_tests::collector_channel_retirement_native_metadata_pause_allows_collection",
    ) {
        return;
    }
    let mut context = hooks::runtime();
    let heap = context.heap().clone();
    context.enter(|thread| unsafe {
        let control = hooks::queue(thread, 1, true);
        let weak = Arc::downgrade(&heap.channels.lock().unwrap().records[&(control as usize)].0);
        let (monitor, waiting, release) = hooks::arm(&heap.native_retirement);
        let before = heap.collections();
        let other = heap.clone();
        let collector = std::thread::spawn(move || other.stw_collect::<IdentityPtrPolicy>());
        blocking_region(thread, || {
            collector.join().unwrap();
            waiting
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert!(heap.channels.try_lock().unwrap().is_empty());
            assert!(monitor.state.try_lock().is_ok());
            {
                let retained = weak
                    .upgrade()
                    .expect("collector destroyed channel metadata");
                assert_eq!(
                    Arc::strong_count(&retained),
                    2,
                    "collector workspace retained a control"
                );
                assert_eq!(retained.owner.load(Ordering::Acquire), 0);
                assert_eq!(retained.slots.read().unwrap().as_ref().unwrap().len(), 1);
            }
            assert_eq!(heap.native_buffer_bytes(), 8);
            heap.stw_collect::<IdentityPtrPolicy>();
            assert_eq!(heap.collections() - before, 2);
            release.send(()).unwrap();
            heap.native_retirement.drain();
            assert!(weak.upgrade().is_none());
            assert_eq!(heap.native_buffer_bytes(), 0);
        });
    });
}
