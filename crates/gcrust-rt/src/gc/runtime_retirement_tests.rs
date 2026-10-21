use super::*;
use crate::runtime::{RuntimeContext, Thread, ai_gc_alloc_fixed, ai_thread_join, ai_thread_spawn};

unsafe extern "C" fn unused_child(_: *mut Thread, _: *mut u8) -> u64 {
    42
}

fn run_child(test: &str, key: &str, case: &str) -> std::process::Output {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(key, case)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Drain while the child runs: repeated injected failures can exceed a
    // pipe's capacity, and waiting before reading would deadlock the test.
    fn read(mut pipe: impl std::io::Read) -> Vec<u8> {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).unwrap();
        bytes
    }
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let stdout = std::thread::spawn(move || read(stdout));
    let stderr = std::thread::spawn(move || read(stderr));
    let deadline = Instant::now() + Duration::from_secs(15);
    let timed_out = loop {
        if child.try_wait().unwrap().is_some() {
            break false;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break true;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = std::process::Output {
        status: child.wait().unwrap(),
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    };
    assert!(!timed_out, "runtime retirement hung: {output:?}");
    output
}

#[test]
fn collector_failed_child_setup_retires_its_registration() {
    if std::env::var_os("GCR_CHILD_RETIREMENT_CASE").is_none() {
        let output = run_child(
            "gc::heap::runtime_retirement_tests::collector_failed_child_setup_retires_its_registration",
            "GCR_CHILD_RETIREMENT_CASE",
            "1",
        );
        assert!(output.status.success(), "{output:?}");
        return;
    }
    let info = TypeInfo::for_header(crate::gc::Full::SIZE).with_raw_bytes(8);
    let mut context = RuntimeContext::new(8192, vec![info]);
    let heap = context.heap().clone();
    let thread = context.thread_ptr();
    let execute = || {
    let env = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
    // Delay registration until the parent has admitted the env. Poison only
    // after the parent's handoff-pool access, to fail the child after its poll
    // target is installed and the global root has transferred to scratch.
    let coordinator = heap.lock_coordinator();
    let handle = unsafe { ai_thread_spawn(thread, env, unused_child as *const u8) };
    let injected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _pool = heap.thread_handoff_slots.lock().unwrap();
        panic!("injected handoff pool failure");
    }));
    assert!(injected.is_err());
    drop(coordinator);
    assert_eq!(unsafe { ai_thread_join(thread, handle) }, 0);
    // Inspect membership only. The broken path's poll target is already freed;
    // never request a poll or collect unless retirement is first proven.
    let states = heap.threads.lock().unwrap().len();
    assert_eq!(states, 1, "failed child left its registration in the heap");
    unsafe {
        context.force_collect(&*thread);
    }
    assert_eq!(
        heap.from_used(),
        0,
        "failed setup retained the handed-off env"
    );
    };
    unsafe { crate::runtime::with_current_thread(thread, execute); }
    drop(context);
    assert!(heap.threads.lock().unwrap().is_empty());
}

#[test]
fn collector_runtime_cleanup_failures_abort_before_storage_retirement() {
    if std::env::var_os("GCR_RUNTIME_RETIREMENT_FAILURE").is_none() {
        for case in ["registration", "drain", "drain-direct", "handoff"] {
            let output = run_child(
                "gc::heap::runtime_retirement_tests::collector_runtime_cleanup_failures_abort_before_storage_retirement",
                "GCR_RUNTIME_RETIREMENT_FAILURE",
                case,
            );
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(output.status.signal(), Some(6), "{case}: {output:?}");
            }
            #[cfg(not(unix))]
            assert!(
                !output.status.success() && output.status.code() != Some(88),
                "{output:?}"
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("PoisonError"), "{case}: {output:?}");
            assert!(
                !stderr.contains("panic in a destructor during cleanup")
                    && !stderr.contains("thread caused non-unwinding panic")
                    && !stderr.contains("ThreadSanitizer:"),
                "{case}: {output:?}"
            );
        }
        return;
    }
    let case = std::env::var("GCR_RUNTIME_RETIREMENT_FAILURE").unwrap();
    if case == "handoff" {
        let info = TypeInfo::for_header(crate::gc::Full::SIZE).with_raw_bytes(8);
        let mut context = RuntimeContext::new(8192, vec![info]);
        let heap = context.heap().clone();
        let thread = context.thread_ptr();
        let execute = || {
        let env = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
        let coordinator = heap.lock_coordinator();
        let handle = unsafe { ai_thread_spawn(thread, env, unused_child as *const u8) };
        let injected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _registry = heap.threads.lock().unwrap();
            let _pool = heap.thread_handoff_slots.lock().unwrap();
            panic!("injected admission and handoff cleanup failure");
        }));
        assert!(injected.is_err());
        drop(coordinator);
        let _ = unsafe { (&*handle).join() };
        std::mem::forget(context);
        std::process::exit(88);
        };
        unsafe { crate::runtime::with_current_thread(thread, execute); }
    }
    let mut context = RuntimeContext::new(8192, vec![]);
    let heap = context.heap().clone();
    context.enter(|thread| unsafe { (*(*thread).dyna_thread).record_alloc(0, 24) });
    let injected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if case == "registration" {
            let _counters = heap.retired_alloc_counters.lock().unwrap();
            panic!("injected retirement accounting failure");
        } else {
            let _completions = heap.thread_completions.lock().unwrap();
            panic!("injected child drain failure");
        }
    }));
    assert!(injected.is_err());
    // An incorrectly propagating cleanup panic is caught here. The required
    // outcome is the runtime's fatal boundary, not test-harness failure.
    if case == "drain-direct" {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            context.finish_threads();
        }));
        std::mem::forget(context);
    } else {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(context)));
    }
    std::process::exit(88);
}

#[test]
fn collector_failed_child_admission_releases_and_reuses_handoff_roots() {
    if std::env::var_os("GCR_ADMISSION_HANDOFF_CHILD").is_none() {
        let output = run_child(
            "gc::heap::runtime_retirement_tests::collector_failed_child_admission_releases_and_reuses_handoff_roots",
            "GCR_ADMISSION_HANDOFF_CHILD",
            "1",
        );
        assert!(output.status.success(), "{output:?}");
        return;
    }
    let info = TypeInfo::for_header(crate::gc::Full::SIZE).with_raw_bytes(8);
    for generational in [false, true] {
        let mut context = if generational {
            RuntimeContext::new_generational(1024, 8192, vec![info.clone()])
        } else {
            RuntimeContext::new(8192, vec![info.clone()])
        };
        let heap = context.heap().clone();
        let initial = heap.globals.len();
        let thread = context.thread_ptr();
        let execute = || {
        for _ in 0..64 {
            let env = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
            let coordinator = heap.lock_coordinator();
            let handle = unsafe { ai_thread_spawn(thread, env, unused_child as *const u8) };
            let injected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _threads = heap.threads.lock().unwrap();
                panic!("injected admission registry failure");
            }));
            assert!(injected.is_err());
            drop(coordinator);
            // No collector or other mutator exists. Admission must fail before
            // user code can run, so this native join needs no GC-blocking scope.
            assert_eq!(unsafe { (&*handle).join() }, 0);
            // Only this test poisoned the mutex, without modifying membership.
            // Restore the known unchanged registry to inspect/collect afterward;
            // production code does not recover poisoned registry state.
            heap.threads.clear_poison();
            assert_eq!(heap.threads.lock().unwrap().len(), 1);
            let vacancy = {
                let pool = heap.thread_handoff_slots.lock().unwrap();
                assert_eq!(pool.len(), 1, "failed admission lost its handoff slot");
                pool[0]
            };
            assert_eq!(heap.globals.get(vacancy), 0);
            assert_eq!(heap.globals.len(), initial + 1);
            unsafe {
                if generational {
                    heap.mutator_triggered_minor_gc::<crate::gc::IdentityPtrPolicy>(
                        &*(*thread).dyna_thread,
                    );
                }
                context.force_collect(&*thread);
            }
            assert_eq!(heap.from_used(), 0, "failed admission retained its env");
            assert_eq!(heap.nursery_used(), 0);
        }
        };
        unsafe { crate::runtime::with_current_thread(thread, execute); }
        drop(context);
        assert!(heap.threads.lock().unwrap().is_empty());
    }
}


#[test]
fn collector_handoff_root_reference_clear_waits_for_last_forwarding_store() {
    use std::{sync::{atomic::AtomicU64, mpsc}, time::Duration};
    let info = TypeInfo::for_header(crate::gc::Full::SIZE).with_raw_bytes(8);
    let heap = Arc::new(Heap::new::<crate::gc::Full>(8192, vec![info.clone()]));
    let object = unsafe { heap.alloc_obj::<crate::gc::Full>(&info, 0) };
    let index = unsafe { heap.globals.add(object as u64) };
    // Evacuation enrolls root addresses under their backing lock, then works
    // after visitation has released that lock, still holding the coordinator.
    let coordinator = heap.lock_coordinator();
    let mut slot = core::ptr::null_mut();
    unsafe { heap.globals.scan_roots(&mut |address| { slot = address; }); }
    let slot = unsafe { &*slot.cast::<AtomicU64>() };
    let loaded = slot.load(Ordering::Relaxed);
    assert_eq!(loaded, object as u64);
    let other = heap.clone();
    let (started, start) = mpsc::channel();
    let (done, finished) = mpsc::channel();
    let cleanup = std::thread::spawn(move || {
        started.send(()).unwrap();
        unsafe { other.clear_thread_handoff_root(index); }
        done.send(()).unwrap();
    });
    start.recv_timeout(Duration::from_secs(5)).unwrap();
    let retired_early = finished.recv_timeout(Duration::from_millis(100)).is_ok();
    // This models the worker's unconditional final forwarding store. The
    // loaded pointer is valid throughout: no object or backing is relocated.
    slot.store(loaded, Ordering::Relaxed);
    drop(coordinator);
    cleanup.join().unwrap();
    assert!(!retired_early, "root retirement raced an active evacuation");
    assert_eq!(heap.globals.get(index), 0, "forwarding resurrected a retired handoff");
}
