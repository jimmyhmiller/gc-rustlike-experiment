//! Explicit, ignored measurement of production publication with prepared inputs.
//! Synthetic terminal records isolate index growth from OS thread creation.
use super::*;
use crate::gc::{Full, ObjHeader, TypeInfo};
use std::time::Instant;
fn completed(value: u64, retirement: &native_retirement::Retirement) -> Arc<JoinHandle> {
    let record = Arc::new(JoinHandle {
        completion_notice: false,
        native_joined: std::sync::atomic::AtomicBool::new(true),
        pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
        reaper_registration: std::sync::OnceLock::new(),
        retirement: std::sync::OnceLock::new(),
        state: Mutex::new(JoinState::Complete(value)),
        owner: std::sync::atomic::AtomicUsize::new(1),
    });
    record
        .retirement
        .get_or_init(|| native_retirement::RecordLink::new(retirement));
    record
}
#[test]
#[ignore = "explicit native admission measurement; requires GCR_ADMISSION_PROFILE"]
fn collector_registry_admission_profile() {
    let config = std::env::var("GCR_ADMISSION_PROFILE").unwrap();
    let (kind, count) = config.split_once(':').unwrap();
    let count: usize = count.parse().unwrap();
    assert!(["atomic-owned", "channel-raw", "completion-pending"].contains(&kind));
    assert!((1..=32768).contains(&count));
    let mut nanos = Vec::with_capacity(8);
    let mut traffic = Vec::with_capacity(8);
    for round in 0..=8 {
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let mut runtime = RuntimeContext::new(32 << 20, vec![info.clone(), info.with_type_id(1)]);
        let heap = runtime.heap().clone();
        let (elapsed, allocation) = runtime.enter(|thread| unsafe {
            let roots = (&*(*thread).dyna_thread).scratch_scope();
            let owner = roots.push(ai_gc_alloc_fixed(thread, 0, 0));
            // This heap is exclusive to the profile and native workers cannot
            // collect it. Capture the owner before parking; no GC occurs in
            // fixture setup/publication (checked below).
            let owner_address = roots.get(owner);
            // Exclusive fixture setup is untimed. Only the production publish
            // path is measured; its inputs and native ownership are prepared.
            let cell = blocking_region(thread, || {
                let mut registry = heap.atomic_cells.lock().unwrap();
                if kind == "atomic-owned" {
                    for _ in 0..count {
                        let p = registry.register(native_retirement::AtomicCell::new(
                            0,
                            &heap.native_retirement,
                        ));
                        registry.bind(p, owner_address);
                    }
                    registry.owned.shrink_to_fit();
                }
                native_retirement::AtomicCell::new(42, &heap.native_retirement)
            });
            let channel = blocking_region(thread, || {
                let mut registry = heap.channels.lock().unwrap();
                if kind == "channel-raw" {
                    for _ in 0..count {
                        let p = registry.register(
                            channel_owner_tests::queue(0, &heap.native_retirement),
                            &heap.native_memory,
                        );
                        registry.bind(
                            p,
                            ChannelOwner {
                                address: owner_address as usize,
                                type_id: 1,
                            },
                        );
                    }
                    registry.unbound.shrink_to_fit();
                }
                channel_owner_tests::queue(0, &heap.native_retirement)
            });
            let record = blocking_region(thread, || {
                let mut registry = heap.thread_completions.lock().unwrap();
                if kind == "completion-pending" {
                    for _ in 0..count {
                        registry.register(
                            completed(0, &heap.native_retirement),
                            &heap.native_retirement,
                        );
                    }
                    while registry.next_pending().is_some() {}
                    registry.pending.shrink_to_fit();
                    registry.owned.reserve(count + 1);
                }
                registry.ensure_reaper();
                let record = completed(42, &heap.native_retirement);
                registry.reaper.as_ref().unwrap().monitor().prepare(&record);
                record
            });
            allocation_probe::start();
            let start = Instant::now();
            match kind {
                "atomic-owned" => {
                    let p = atomic_registry_growth::publish(thread, &heap, cell);
                    assert_eq!((*p).load(Ordering::Relaxed), 42);
                }
                "channel-raw" => {
                    channel_registry_growth::publish(thread, &heap, channel);
                }
                "completion-pending" => {
                    completion_registry_growth::publish(thread, &heap, record);
                }
                _ => unreachable!(),
            }
            let elapsed = start.elapsed().as_nanos();
            let allocation = allocation_probe::finish_bytes();
            let live = match kind {
                "atomic-owned" => heap.atomic_cells.lock().unwrap().records.len(),
                "channel-raw" => heap.channels.lock().unwrap().records.len(),
                _ => heap.thread_completions.lock().unwrap().records.len(),
            };
            assert_eq!(live, count + 1);
            assert_eq!(heap.collections(), 0);
            (elapsed, allocation)
        });
        drop(runtime);
        assert_eq!(heap.native_buffer_bytes(), 0);
        if round != 0 {
            nanos.push(elapsed);
            traffic.push([allocation.0, allocation.1, allocation.2, allocation.3]);
        }
    }
    println!(
        "ADMISSION_PROFILE {{\"kind\":\"{kind}\",\"count\":{count},\"nanos\":{nanos:?},\"traffic\":{traffic:?}}}"
    );
}
