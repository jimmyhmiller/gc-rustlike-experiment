use super::*;
use crate::gc::{AtomicRootSet, Full, IdentityPtrPolicy, ObjHeader, TypeInfo};
use crate::runtime::{allocation_probe, *};
use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

#[test]
fn collector_registry_compaction_reference_commit_and_disposal_are_separate() {
    for count in [0, 1, 17, 257] {
        let mut map = HashMap::with_capacity(16 * (count + 1));
        let mut set = HashSet::with_capacity(16 * (count + 1));
        let mut vector = Vec::with_capacity(16 * (count + 1));
        for i in 0..count {
            map.insert(i, i + 42);
            set.insert(i);
            vector.push(i);
        }
        let mut maps = Stage::new(plan(&map, count));
        let mut sets = Stage::new(plan(&set, count));
        let mut vectors = Stage::new(plan(&vector, count));
        allocation_probe::start();
        assert!(maps.install(&mut map, count));
        assert!(sets.install(&mut set, count));
        assert!(vectors.install(&mut vector, count));
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        for i in 0..count {
            assert_eq!(map[&i], i + 42);
            assert!(set.contains(&i));
            assert_eq!(vector[i], i);
        }
        assert!(maps.backing.as_ref().unwrap().is_empty());
        assert!(sets.backing.as_ref().unwrap().is_empty());
        assert!(vectors.backing.as_ref().unwrap().is_empty());
        allocation_probe::start();
        drop((maps, sets, vectors));
        let traffic = allocation_probe::finish_bytes();
        assert_eq!(traffic.0, 0);
        assert_eq!(traffic.2, 0);
        assert!(
            traffic.1 > 0 && traffic.3 > 0,
            "old backing must be disposed in the native phase"
        );
    }
}
#[test]
fn collector_registry_compaction_reference_rechecks_growth_and_sweeping() {
    let mut map = HashMap::with_capacity(2048);
    map.insert(0, 42);
    let mut stage = Stage::new(plan(&map, 1));
    for i in 1..257 {
        map.insert(i, i + 42);
    }
    allocation_probe::start();
    assert!(!stage.install(&mut map, 257));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(map.len(), 257);
    let mut stage = Stage::new(plan(&map, 257));
    map.retain(|key, _| *key == 0);
    allocation_probe::start();
    assert!(
        !stage.install(&mut map, 1),
        "stale historical peak must not be installed"
    );
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    let mut current = Stage::new(plan(&map, 1));
    assert!(current.install(&mut map, 1));
    assert_eq!(map[&0], 42);
    assert!(map.capacity() <= 8);
    assert!(
        !stage.install(&mut map, 1),
        "another compactor already serviced the table"
    );
}
#[test]
fn collector_registry_compaction_reference_preserves_future_binding_reservation() {
    let mut owned: Vec<usize> = Vec::with_capacity(8192);
    let mut stage = Stage::new(plan(&owned, 257));
    allocation_probe::start();
    assert!(stage.install(&mut owned, 257));
    for i in 0..257 {
        owned.push(i);
    }
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(owned.len(), 257);
}
#[test]
fn collector_registry_compaction_reference_all_owner_sweeps_leave_backing_untouched() {
    let retirement = native_retirement::Retirement::default();
    let memory = Arc::new(native_memory::NativeMemory::default());
    let mut atomics = AtomicCellRegistry::default();
    let mut channels = ChannelRegistry::default();
    let mut completions = CompletionRegistry::default();
    for _ in 0..257 {
        let cell = atomics.register(native_retirement::AtomicCell::new(0, &retirement));
        atomics.bind(cell, 8usize as *mut u8);
        let channel = channels.register(channel_owner_tests::queue(0, &retirement), &memory);
        channels.bind(
            channel,
            ChannelOwner {
                address: 8,
                type_id: 1,
            },
        );
        completions.register(
            Arc::new(JoinHandle {
                completion_notice: false,
                native_joined: std::sync::atomic::AtomicBool::new(false),
                pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
                reaper_registration: std::sync::OnceLock::new(),
                retirement: std::sync::OnceLock::new(),
                state: Mutex::new(JoinState::Complete(42)),
                owner: std::sync::atomic::AtomicUsize::new(8),
            }),
            &retirement,
        );
    }
    let capacities = (
        atomics.records.capacity(),
        channels.records.capacity(),
        completions.records.capacity(),
    );
    allocation_probe::start();
    atomics.sweep(|_| None);
    channels.sweep(|_| None);
    completions.sweep(|_| false);
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    // Hash-table capacity may decrease logically as removals leave tombstones;
    // the allocator probe proves that no physical backing was replaced/freed.
    let after = (
        atomics.records.capacity(),
        channels.records.capacity(),
        completions.records.capacity(),
    );
    assert!(after.0 > 8 && after.0 <= capacities.0);
    assert!(after.1 > 8 && after.1 <= capacities.1);
    assert!(after.2 > 8 && after.2 <= capacities.2);
    let atomics = Mutex::new(atomics);
    let channels = Mutex::new(channels);
    let completions = Mutex::new(completions);
    compact_atomics(&atomics);
    compact_channels(&channels);
    compact_completions(&completions);
    assert!(atomics.lock().unwrap().records.capacity() <= 8);
    assert!(channels.lock().unwrap().records.capacity() <= 8);
    assert!(completions.lock().unwrap().records.capacity() <= 8);
    retirement.drain();
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Phase {
    Allocate,
    Dispose,
}
struct Hook {
    registry: usize,
    phase: Phase,
    ready: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
static HOOK: Mutex<Option<Hook>> = Mutex::new(None);
pub(super) fn checkpoint(registry: usize, phase: Phase) {
    let hook = {
        let mut slot = HOOK.lock().unwrap();
        if slot
            .as_ref()
            .is_some_and(|h| h.registry == registry && h.phase == phase)
        {
            slot.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        hook.ready.send(()).unwrap();
        hook.release.recv().unwrap();
    }
}
#[test]
fn collector_registry_compaction_allocation_and_disposal_allow_another_collection() {
    const CHILD: &str = "GCR_REGISTRY_COMPACTION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        for generational in ["0", "1"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "runtime::registry_compaction::tests::collector_registry_compaction_allocation_and_disposal_allow_another_collection", "--nocapture"])
                .env(CHILD, generational).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    panic!(
                        "compaction timed out: {:?}",
                        child.wait_with_output().unwrap()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        return;
    }
    let generational = std::env::var(CHILD).unwrap() == "1";
    for phase in [Phase::Allocate, Phase::Dispose] {
        for entry in ["major", "minor", "stw", "concurrent", "extra", "pressure"] {
            if entry == "minor" && !generational {
                continue;
            }
            let (ready, waiting) = mpsc::channel();
            let (release, go) = mpsc::channel();
            let (publish, receive) = mpsc::channel();
            let (completed, finished) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
                let mut runtime = if generational {
                    RuntimeContext::new_generational(32768, 2 << 20, vec![info.clone()])
                } else {
                    RuntimeContext::new(2 << 20, vec![info.clone()])
                };
                let heap = runtime.heap().clone();
                runtime.enter(|thread| unsafe {
                    let roots = (&*(*thread).dyna_thread).scratch_scope();
                    let keeper = roots.push(ai_gc_alloc_fixed(thread, 0, 0));
                    roots.get(keeper).add(Full::SIZE).cast::<u64>().write(42);
                    let states: Vec<_> = if matches!(entry, "major" | "minor") {
                        (0..17).map(|_| heap.register_thread().0).collect()
                    } else {
                        Vec::new()
                    };
                    for (i, state) in states.iter().enumerate() {
                        if i % 3 == 0 {
                            state.enter_blocked();
                        }
                    }
                    for _ in 0..32 {
                        ai_atomic_i64_new(thread, 0);
                    }
                    {
                        let owner_roots = (&*(*thread).dyna_thread).scratch_scope();
                        let owner = owner_roots.push(ai_gc_alloc_fixed(thread, 0, 0));
                        for _ in 0..257 {
                            let cell = ai_atomic_i64_new(thread, 0);
                            ai_atomic_i64_bind_owner(thread, cell, owner_roots.get(owner));
                        }
                        if entry != "minor" {
                            heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(
                                &*(*thread).dyna_thread,
                            );
                        }
                    }
                    // This old-space object has no other root. Direct collect
                    // must keep its extra source published during maintenance.
                    let extra = AtomicRootSet::new();
                    let index = if entry == "extra" {
                        let value = heap.alloc_obj::<Full>(&info, 0);
                        value.add(Full::SIZE).cast::<u64>().write(4242);
                        extra.add(value as u64)
                    } else {
                        extra.add(0)
                    };
                    // Prepare actual retained native demand without invoking
                    // constructor pressure service before the hook is armed.
                    let pressure_backing = blocking_region(thread, || {
                        (entry == "pressure").then(|| {
                            crate::runtime::native_metadata::vector::<u64>(
                                131073,
                                MetadataAllocator::new(&heap.native_memory.registry),
                            )
                        })
                    });
                    *HOOK.lock().unwrap() = Some(Hook {
                        registry: &heap.atomic_cells as *const _ as usize,
                        phase,
                        ready,
                        release: go,
                    });
                    publish.send(heap.clone()).unwrap();
                    match entry {
                        "major" => {
                            heap.mutator_triggered_gc::<IdentityPtrPolicy>(&*(*thread).dyna_thread);
                        }
                        "minor" => {
                            heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(
                                &*(*thread).dyna_thread,
                            );
                        }
                        "stw" => {
                            blocking_region(thread, || heap.stw_collect::<IdentityPtrPolicy>());
                        }
                        "concurrent" => {
                            blocking_region(thread, || {
                                heap.concurrent_collect::<IdentityPtrPolicy>()
                            });
                        }
                        "pressure" => heap.mutator_gc_for_native_allocations::<IdentityPtrPolicy>(
                            &*(*thread).dyna_thread,
                            8,
                        ),
                        "extra" => blocking_region(thread, || {
                            heap.collect::<IdentityPtrPolicy>(&[&extra]);
                            completed.send(()).unwrap();
                        }),
                        _ => unreachable!(),
                    }
                    if entry != "extra" {
                        completed.send(()).unwrap();
                    }
                    assert_eq!(roots.get(keeper).add(Full::SIZE).cast::<u64>().read(), 42);
                    if entry == "extra" {
                        let value = extra.get(index) as *mut u8;
                        assert!(heap.from_space_contains(value));
                        assert_eq!(value.add(Full::SIZE).cast::<u64>().read(), 4242);
                    }
                    for (i, state) in states.iter().enumerate() {
                        assert_eq!(state.is_blocked(), i % 3 == 0);
                        heap.safe_deregister_thread(state);
                    }
                    assert!(heap.atomic_cells.lock().unwrap().records.capacity() <= 128);
                    assert!(heap.atomic_cells.lock().unwrap().owned.capacity() >= 32);
                    blocking_region(thread, || drop(pressure_backing));
                });
            });
            let heap = receive.recv_timeout(Duration::from_secs(5)).unwrap();
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(heap.native_registry_bytes() > heap.collector_object_lock_backing_bytes() + heap.collector_layout_backing_bytes() + heap.collector_abi_backing_bytes());
            assert!(heap.native_registry_peak_bytes() >= heap.native_registry_bytes());
            assert!(heap.atomic_cells.try_lock().is_ok());
            assert!(heap.channels.try_lock().is_ok());
            assert!(heap.thread_completions.try_lock().is_ok());
            unsafe {
                heap.stw_collect::<IdentityPtrPolicy>();
            }
            if entry == "extra" {
                // Root addresses may remain borrowed by another coordinator.
                let pause = heap.pause_world();
                release.send(()).unwrap();
                let returned_early = finished.recv_timeout(Duration::from_millis(100)).is_ok();
                drop(pause);
                worker.join().unwrap();
                // Concurrent cycles retain their source bitmap until Heap drop.
                assert_eq!(heap.native_registry_bytes(), heap.collector_object_lock_backing_bytes() + heap.collector_layout_backing_bytes() + heap.collector_abi_backing_bytes() + heap.retired_profile_backing_bytes() + heap.satb_queue.stats().bitmap_bytes + heap.collector_worker_backing_bytes() + unsafe { heap.collector_tlab_metadata_bytes() });
                assert!(
                    !returned_early,
                    "extra-root retirement escaped collector exclusion"
                );
            } else {
                release.send(()).unwrap();
                worker.join().unwrap();
                // Concurrent cycles retain their source bitmap until Heap drop.
                assert_eq!(heap.native_registry_bytes(), heap.collector_object_lock_backing_bytes() + heap.collector_layout_backing_bytes() + heap.collector_abi_backing_bytes() + heap.retired_profile_backing_bytes() + heap.satb_queue.stats().bitmap_bytes + heap.collector_worker_backing_bytes() + unsafe { heap.collector_tlab_metadata_bytes() });
            }
            let counter = heap.native_memory.registry.clone();
            drop(heap);
            assert_eq!(counter.bytes(), 0);
        }
    }
}
#[test]
fn collector_native_metadata_reference_compaction_rejects_foreign_counter() {
    use crate::runtime::{allocation_counter::AllocationCounter, native_metadata};
    let a = Arc::<AllocationCounter>::default();
    let b = Arc::<AllocationCounter>::default();
    let mut map = native_metadata::map(1024, MetadataAllocator::new(&a));
    map.insert(0, 42usize);
    let before = a.bytes();
    let mut stage = Stage::in_allocator(plan(&map, 1), MetadataAllocator::new(&b));
    assert!(b.bytes() > 0);
    allocation_probe::start();
    assert!(!stage.install(&mut map, 1));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(a.bytes(), before);
    assert_eq!(map[&0], 42);
    drop(stage);
    assert_eq!(b.bytes(), 0);
    drop(map);
    assert_eq!(a.bytes(), 0);
}

#[test]
fn collector_thread_census_compaction_staging_revalidates_and_releases_off_lock() {
    const CHILD: &str = "GCR_THREAD_CENSUS_COMPACTION_CHILD";
    const NAME: &str = "runtime::registry_compaction::tests::collector_thread_census_compaction_staging_revalidates_and_releases_off_lock";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME]).env(CHILD, "1")
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("census compaction hung: {:?}", child.wait_with_output().unwrap());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("test {NAME} ... ok")), "{output:?}");
        return;
    }
    for phase in [Phase::Allocate, Phase::Dispose] {
        let counter = Arc::<allocation_counter::AllocationCounter>::default();
        let mut values = native_metadata::vector::<u64>(128, MetadataAllocator::new(&counter));
        values.extend(0..3);
        let original_capacity = values.capacity();
        let census = Arc::new(Mutex::new(values));
        let (ready, waiting) = mpsc::channel();
        let (release, go) = mpsc::channel();
        *HOOK.lock().unwrap() = Some(Hook { registry: Arc::as_ptr(&census) as usize, phase, ready, release: go });
        let other = census.clone();
        let worker = std::thread::spawn(move || compact_vector(&other));
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut storage = census.try_lock().expect("native staging/disposal held census mutex");
        if phase == Phase::Allocate {
            // The six-entry stage was planned before this concurrent growth.
            storage.extend(3..257);
        } else {
            assert_eq!(counter.bytes(), (original_capacity + storage.capacity()) * 8);
            storage.push(3);
        }
        drop(storage);
        release.send(()).unwrap();
        worker.join().unwrap();
        let storage = census.lock().unwrap();
        let expected = if phase == Phase::Allocate {257} else {4};
        assert_eq!(storage.as_slice(), &(0..expected).collect::<Vec<_>>());
        assert_eq!(counter.bytes(), storage.capacity() * 8);
        drop(storage);
        drop(census);
        assert_eq!(counter.bytes(), 0);
    }
}
