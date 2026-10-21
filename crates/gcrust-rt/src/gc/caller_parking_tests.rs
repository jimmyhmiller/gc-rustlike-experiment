use super::*;
use crate::runtime::allocation_probe;

#[test]
fn collector_caller_parking_reference_has_no_allocator_traffic() {
    for count in [0, 1, 17, 257] {
        let heap = Heap::new::<crate::gc::Full>(1 << 16, vec![]);
        let states: Vec<_> = (0..count).map(|_| heap.register_thread().0).collect();
        drop(heap.threads.lock().unwrap());
        // Prepare the platform safepoint locks independently of this operation.
        for state in &states {
            unsafe {
                state.enter_blocked();
                state.exit_blocked(&heap);
            }
        }
        for (i, state) in states.iter().enumerate() {
            if count > 1 && i % 3 == 0 {
                unsafe {
                    state.enter_blocked();
                }
            }
        }
        allocation_probe::start();
        let mut callers = heap.suspend_current_registrations();
        let traffic = allocation_probe::finish_bytes();
        assert_eq!(traffic, (0, 0, 0, 0), "parking {count} registrations");
        assert!(states.iter().all(|s| s.is_blocked()));
        allocation_probe::start();
        let nested = heap.suspend_current_registrations();
        drop(nested);
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert!(states.iter().all(|s| s.is_blocked()));
        allocation_probe::start();
        callers.restore();
        drop(callers);
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        for (i, state) in states.iter().enumerate() {
            assert_eq!(state.is_blocked(), count > 1 && i % 3 == 0);
            assert_eq!(crate::gc::ThreadStateHandle::strong_count(state), 2, "retained caller link");
            if count > 1 && i % 3 == 0 {
                unsafe {
                    state.exit_blocked(&heap);
                }
            }
            unsafe {
                heap.safe_deregister_thread(state);
            }
        }
    }
}

#[test]
fn collector_caller_parking_reference_nested_new_registration_and_unwind() {
    let heap = Heap::new::<crate::gc::Full>(1 << 16, vec![]);
    let (first, _) = heap.register_thread();
    let (outer_blocked, _) = heap.register_thread();
    unsafe {
        outer_blocked.enter_blocked();
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _outer = heap.suspend_current_registrations();
        let (later, _) = heap.register_thread();
        {
            let _nested = heap.suspend_current_registrations();
            assert!(first.is_blocked() && later.is_blocked() && outer_blocked.is_blocked());
        }
        assert!(later.is_running());
        assert!(first.is_blocked() && outer_blocked.is_blocked());
        unsafe {
            heap.safe_deregister_thread(&later);
        }
        panic!("exercise caller restoration");
    }));
    assert!(result.is_err());
    assert!(first.is_running());
    assert!(outer_blocked.is_blocked());
    assert_eq!(crate::gc::ThreadStateHandle::strong_count(&first), 2);
    unsafe {
        outer_blocked.exit_blocked(&heap);
        heap.safe_deregister_thread(&outer_blocked);
        heap.safe_deregister_thread(&first);
    }
}

#[test]
fn collector_caller_parking_allows_pending_collection_and_relocates_every_owner() {
    use crate::gc::{Full, IdentityPtrPolicy};
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    const NAME: &str = "gc::heap::caller_parking_tests::collector_caller_parking_allows_pending_collection_and_relocates_every_owner";
    const CHILD: &str = "GCR_CALLER_PARKING_CHILD";
    if std::env::var_os(CHILD).is_none() {
        for minor in [false, true] {
            for workers in [1, 4] {
                let mut child = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", NAME])
                    .env(CHILD, if minor { "minor" } else { "major" })
                    .env("GCR_GC_WORKERS", workers.to_string())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(15);
                while child.try_wait().unwrap().is_none() {
                    if Instant::now() >= deadline {
                        child.kill().unwrap();
                        panic!(
                            "caller parking hung: {:?}",
                            child.wait_with_output().unwrap()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let output = child.wait_with_output().unwrap();
                assert!(output.status.success(), "{output:?}");
            }
        }
        return;
    }
    let minor = std::env::var(CHILD).unwrap() == "minor";
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Arc::new(Heap::new_generational::<Full>(
        1 << 16,
        1 << 20,
        vec![info.clone()],
    ));
    let states: Vec<_> = (0..17).map(|_| heap.register_thread().0).collect();
    let roots: Vec<_> = states
        .iter()
        .map(|s| unsafe { s.scratch_scope() })
        .collect();
    let mut previous = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        let obj = unsafe {
            if minor {
                heap.alloc_nursery_obj::<Full>(&info, 0)
            } else {
                heap.alloc_obj::<Full>(&info, 0)
            }
        };
        assert!(!obj.is_null());
        unsafe {
            obj.add(Full::SIZE).cast::<u64>().write(i as u64 + 42);
            assert_eq!(root.push(obj), 0);
        }
        previous.push(obj as usize);
    }
    for (i, state) in states.iter().enumerate() {
        if i % 3 == 0 {
            unsafe {
                state.enter_blocked();
            }
        }
    }
    let other = heap.clone();
    let collector = std::thread::spawn(move || unsafe {
        let (state, _) = other.register_thread();
        if minor {
            other.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state);
        } else {
            other.mutator_triggered_gc::<IdentityPtrPolicy>(&state);
        }
        other.safe_deregister_thread(&state);
    });
    while !heap.gc_requested() {
        std::thread::yield_now();
    }
    let callers = heap.suspend_current_registrations();
    collector.join().unwrap();
    drop(callers);
    for (i, state) in states.iter().enumerate() {
        assert_eq!(state.is_blocked(), i % 3 == 0);
        if i % 3 == 0 {
            unsafe {
                state.exit_blocked(&heap);
            }
        }
        let moved = roots[i].get(0);
        assert_ne!(moved as usize, previous[i]);
        assert_eq!(
            unsafe { moved.add(Full::SIZE).cast::<u64>().read() },
            i as u64 + 42
        );
    }
    assert_eq!(
        if minor {
            heap.minor_collections()
        } else {
            heap.collections()
        },
        1
    );
    drop(roots);
    for state in states {
        unsafe {
            heap.safe_deregister_thread(&state);
        }
    }
}
