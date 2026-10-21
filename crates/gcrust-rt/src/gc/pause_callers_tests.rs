use super::*;
use crate::gc::{Full, IdentityPtrPolicy, MutatorThread};
use std::{cell::RefCell, sync::{mpsc, TryLockError}, time::{Duration, Instant}};

struct Probe {
    ready: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
thread_local! {
    static RESTORATION_PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
}
pub(super) fn restoration_step() {
    let probe = RESTORATION_PROBE.with(|slot| slot.borrow_mut().take());
    if let Some(probe) = probe {
        probe.ready.send(()).unwrap();
        probe.release.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}

#[test]
fn collector_pause_caller_partial_group_restoration_excludes_new_collections() {
    const CHILD: &str = "GCR_PAUSE_CALLER_RESTORATION_CHILD";
    const NAME: &str = "gc::heap::pause_callers_tests::collector_pause_caller_partial_group_restoration_excludes_new_collections";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"]).env(CHILD, "1")
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("group restoration timed out: {:?}", child.wait_with_output().unwrap());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("test {NAME} ... ok")), "{output:?}");
        return;
    }
    let heap = Arc::new(Heap::new::<Full>(4096, vec![]));
    let (ready, waiting) = mpsc::channel();
    let (release, go) = mpsc::channel();
    let other = heap.clone();
    let worker = std::thread::spawn(move || {
        let caller = MutatorThread::<IdentityPtrPolicy>::register(other.clone());
        let sibling = MutatorThread::<IdentityPtrPolicy>::register(other.clone());
        let parked = other.suspend_current_registrations();
        RESTORATION_PROBE.with(|slot| assert!(slot.replace(Some(Probe {ready, release: go})).is_none()));
        drop(parked);
        assert!(!caller.state().is_blocked() && !sibling.state().is_blocked());
    });
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    // A checkpoint after the first restore leaves the second state BLOCKED.
    // Observe exclusion without starting a GC that could strand the old code.
    let excluded = match heap.threads.try_lock() {
        Err(TryLockError::WouldBlock) => true,
        Ok(guard) => { drop(guard); false },
        Err(error) => panic!("unexpected census state: {error}"),
    };
    release.send(()).unwrap();
    worker.join().unwrap();
    assert!(excluded, "collection census was not excluded during partial group restoration");
    assert!(heap.threads.try_lock().is_ok());
}

fn polling_siblings_case(name: &str, heap_entry: bool) {
    const CHILD: &str = "GCR_POLLING_SIBLINGS_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"]).env(CHILD, "1")
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("sibling polling timed out: {:?}", child.wait_with_output().unwrap());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("test {name} ... ok")), "{output:?}");
        return;
    }
    let heap = Arc::new(Heap::new::<Full>(4096, vec![]));
    let (ready, states) = mpsc::channel();
    let (go, start) = mpsc::channel();
    let other = heap.clone();
    let worker = std::thread::spawn(move || {
        let caller = MutatorThread::<IdentityPtrPolicy>::register(other.clone());
        let sibling = MutatorThread::<IdentityPtrPolicy>::register(other.clone());
        let blocked = MutatorThread::<IdentityPtrPolicy>::register(other.clone());
        unsafe { blocked.state().enter_blocked(); }
        ready.send((crate::gc::ThreadStateHandle::as_ptr(caller.state()) as usize, crate::gc::ThreadStateHandle::as_ptr(sibling.state()) as usize, crate::gc::ThreadStateHandle::as_ptr(blocked.state()) as usize)).unwrap();
        start.recv_timeout(Duration::from_secs(5)).unwrap();
        unsafe {
            if heap_entry { other.safepoint(caller.state()); }
            else { caller.safepoint(); }
        }
        assert!(caller.state().is_running() && sibling.state().is_running());
        assert!(blocked.state().is_blocked());
        unsafe { blocked.state().exit_blocked(&other); }
    });
    let (caller, sibling, blocked) = states.recv_timeout(Duration::from_secs(5)).unwrap();
    // Only atomic/census observations cross threads; the owner lives to join.
    let (caller, sibling, blocked) = unsafe {
        (&*(caller as *const ThreadState), &*(sibling as *const ThreadState), &*(blocked as *const ThreadState))
    };
    // Use the real request/census protocol without starting evacuation: the
    // negative control can always release and join before reporting failure.
    let coordinator = heap.lock_coordinator();
    {
        let _census = heap.threads.lock().unwrap();
        heap.gc_requested.store(true, Ordering::Release);
        caller.request_poll(); sibling.request_poll(); blocked.request_poll();
    }
    go.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !caller.is_safely_at_safepoint() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    let all_parked = {
        let _census = heap.threads.lock().unwrap();
        caller.is_safely_at_safepoint() && sibling.is_safely_at_safepoint() && blocked.is_blocked()
    };
    {
        let _census = heap.threads.lock().unwrap();
        for state in [caller, sibling, blocked] { state.clear_poll(); }
        heap.gc_requested.store(false, Ordering::Release);
        for state in [caller, sibling, blocked] { unsafe { state.resume(); } }
    }
    drop(coordinator);
    worker.join().unwrap();
    assert!(all_parked, "ordinary safepoint left a same-OS-thread sibling RUNNING");
}

#[test]
fn collector_mutator_poll_parks_running_siblings() {
    polling_siblings_case("gc::heap::pause_callers_tests::collector_mutator_poll_parks_running_siblings", false);
}

#[test]
fn collector_heap_poll_parks_running_siblings() {
    polling_siblings_case("gc::heap::pause_callers_tests::collector_heap_poll_parks_running_siblings", true);
}

#[test]
fn collector_generated_poll_parks_embedding_sibling() {
    const CHILD: &str = "GCR_GENERATED_POLL_SIBLING_CHILD";
    const NAME: &str = "gc::heap::pause_callers_tests::collector_generated_poll_parks_embedding_sibling";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"]).env(CHILD, "1")
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("generated polling timed out: {:?}", child.wait_with_output().unwrap());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("test {NAME} ... ok")), "{output:?}");
        return;
    }
    let (ready, states) = mpsc::channel();
    let (go, start) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut runtime = crate::runtime::RuntimeContext::new(4096, vec![]);
        let heap = runtime.heap().clone();
        runtime.enter(|thread| {
            let sibling = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
            let caller = unsafe { &*(*thread).dyna_thread };
            ready.send((heap.clone(), caller as *const ThreadState as usize, crate::gc::ThreadStateHandle::as_ptr(sibling.state()) as usize)).unwrap();
            start.recv_timeout(Duration::from_secs(5)).unwrap();
            unsafe { crate::runtime::ai_gc_pollcheck_slow(thread); }
            assert!(caller.is_running() && sibling.state().is_running());
            assert!(caller.parked_jit_fp().is_null());
        });
    });
    let (heap, caller, sibling) = states.recv_timeout(Duration::from_secs(5)).unwrap();
    let (caller, sibling) = unsafe { (&*(caller as *const ThreadState), &*(sibling as *const ThreadState)) };
    let coordinator = heap.lock_coordinator();
    {
        let _census = heap.threads.lock().unwrap();
        heap.gc_requested.store(true, Ordering::Release);
        caller.request_poll(); sibling.request_poll();
    }
    go.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !caller.is_safely_at_safepoint() && Instant::now() < deadline { std::thread::yield_now(); }
    let all_parked = {
        let _census = heap.threads.lock().unwrap();
        caller.is_safely_at_safepoint() && sibling.is_safely_at_safepoint()
    };
    {
        let _census = heap.threads.lock().unwrap();
        for state in [caller, sibling] { state.clear_poll(); }
        heap.gc_requested.store(false, Ordering::Release);
        for state in [caller, sibling] { unsafe { state.resume(); } }
    }
    drop(coordinator);
    worker.join().unwrap();
    assert!(all_parked, "generated polling left an embedding sibling RUNNING");
}

#[test]
fn collector_runtime_sleep_parks_embedding_sibling() {
    const CHILD: &str = "GCR_RUNTIME_SLEEP_SIBLING_CHILD";
    const NAME: &str = "gc::heap::pause_callers_tests::collector_runtime_sleep_parks_embedding_sibling";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"]).env(CHILD, "1")
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("runtime sleep timed out: {:?}", child.wait_with_output().unwrap());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("test {NAME} ... ok")), "{output:?}");
        return;
    }
    let (ready, states) = mpsc::channel();
    let (go, start) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut runtime = crate::runtime::RuntimeContext::new(4096, vec![]);
        let heap = runtime.heap().clone();
        runtime.enter(|thread| {
            let sibling = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
            let caller = unsafe { &*(*thread).dyna_thread };
            ready.send((heap.clone(), caller as *const ThreadState as usize, crate::gc::ThreadStateHandle::as_ptr(sibling.state()) as usize)).unwrap();
            start.recv_timeout(Duration::from_secs(5)).unwrap();
            unsafe { crate::runtime::ai_thread_sleep(thread, 200); }
            assert!(caller.is_running() && sibling.state().is_running());
            assert!(caller.parked_jit_fp().is_null());
        });
    });
    let (heap, caller, sibling) = states.recv_timeout(Duration::from_secs(5)).unwrap();
    let (caller, sibling) = unsafe { (&*(caller as *const ThreadState), &*(sibling as *const ThreadState)) };
    let coordinator = heap.lock_coordinator();
    {
        let _census = heap.threads.lock().unwrap();
        heap.gc_requested.store(true, Ordering::Release);
        caller.request_poll(); sibling.request_poll();
    }
    go.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !caller.is_safely_at_safepoint() && Instant::now() < deadline { std::thread::yield_now(); }
    let all_parked = {
        let _census = heap.threads.lock().unwrap();
        caller.is_safely_at_safepoint() && sibling.is_safely_at_safepoint()
    };
    {
        let _census = heap.threads.lock().unwrap();
        for state in [caller, sibling] { state.clear_poll(); }
        heap.gc_requested.store(false, Ordering::Release);
        for state in [caller, sibling] { unsafe { state.resume(); } }
    }
    drop(coordinator);
    worker.join().unwrap();
    assert!(all_parked, "runtime sleep left an embedding sibling RUNNING");
}

#[test]
fn collector_foreign_call_parks_embedding_sibling() {
    const CHILD: &str = "GCR_FOREIGN_CALL_SIBLING_CHILD";
    const NAME: &str = "gc::heap::pause_callers_tests::collector_foreign_call_parks_embedding_sibling";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"]).env(CHILD, "1")
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("foreign call timed out: {:?}", child.wait_with_output().unwrap());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("test {NAME} ... ok")), "{output:?}");
        return;
    }
    let (ready, states) = mpsc::channel();
    let (go, start) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut runtime = crate::runtime::RuntimeContext::new(4096, vec![]);
        let heap = runtime.heap().clone();
        runtime.enter(|thread| {
            let sibling = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
            let caller = unsafe { &*(*thread).dyna_thread };
            ready.send((heap.clone(), caller as *const ThreadState as usize, crate::gc::ThreadStateHandle::as_ptr(sibling.state()) as usize)).unwrap();
            start.recv_timeout(Duration::from_secs(5)).unwrap();
            unsafe { crate::runtime::ai_ffi_enter(thread);
                std::thread::sleep(Duration::from_millis(200));
                crate::runtime::ai_ffi_leave(thread); }
            assert!(caller.is_running() && sibling.state().is_running());
            assert!(caller.parked_jit_fp().is_null());
        });
    });
    let (heap, caller, sibling) = states.recv_timeout(Duration::from_secs(5)).unwrap();
    let (caller, sibling) = unsafe { (&*(caller as *const ThreadState), &*(sibling as *const ThreadState)) };
    let coordinator = heap.lock_coordinator();
    {
        let _census = heap.threads.lock().unwrap();
        heap.gc_requested.store(true, Ordering::Release);
        caller.request_poll(); sibling.request_poll();
    }
    go.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !caller.is_safely_at_safepoint() && Instant::now() < deadline { std::thread::yield_now(); }
    let all_parked = {
        let _census = heap.threads.lock().unwrap();
        caller.is_safely_at_safepoint() && sibling.is_safely_at_safepoint()
    };
    {
        let _census = heap.threads.lock().unwrap();
        for state in [caller, sibling] { state.clear_poll(); }
        heap.gc_requested.store(false, Ordering::Release);
        for state in [caller, sibling] { unsafe { state.resume(); } }
    }
    drop(coordinator);
    worker.join().unwrap();
    assert!(all_parked, "foreign call left an embedding sibling RUNNING");
}
