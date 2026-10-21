use crate::gc::{Full, Heap, IdentityPtrPolicy, MutatorThread, RootSource, ThreadState};
use std::io::Write;
use std::sync::{Arc, Mutex, TryLockError, mpsc};
use std::time::{Duration, Instant};

struct MetadataRoots<'a>(&'a Mutex<i32>);
unsafe impl RootSource for MetadataRoots<'_> {
    unsafe fn scan_roots(&self, _: &mut dyn FnMut(*mut u64)) {
        // Empty embedding root metadata is still protected by its native lock.
        let _metadata = self.0.lock().unwrap_or_else(|error| error.into_inner());
    }
}

fn embedding_scan_case(name: &str, poison: bool) {
    // Rust test names omit the crate segment included by module_path!.
    let name = name.split_once("::").unwrap().1;
    if std::env::var_os("GCR_LOCK_SAFE_SCAN_CHILD").is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("GCR_LOCK_SAFE_SCAN_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!("lock_safe blocked embedding root scan: {output:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed"),
            "child did not execute exactly one regression: {output:?}"
        );
        return;
    }
    let heap = Arc::new(Heap::new::<Full>(4096, vec![]));
    let mutex = Arc::new(Mutex::new(0));
    let held = mutex.lock().unwrap();
    let (send, receive) = mpsc::channel();
    let worker_heap = heap.clone();
    let worker_mutex = mutex.clone();
    let worker = std::thread::spawn(move || {
        let mutator = MutatorThread::<IdentityPtrPolicy>::register(worker_heap);
        // Only atomic state observations cross threads. Its owner outlives join.
        send.send(crate::gc::ThreadStateHandle::as_ptr(mutator.state()) as usize).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = unsafe { mutator.lock_safe(&worker_mutex) };
            *guard += 1;
        }));
        assert_eq!(result.is_err(), poison);
        assert!(!mutator.state().is_blocked());
        assert!(!mutator.state().is_at_safepoint());
    });
    let address = receive.recv_timeout(Duration::from_secs(5)).unwrap();
    let state = unsafe { &*(address as *const ThreadState) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !state.is_blocked() {
        assert!(Instant::now() < deadline, "waiter failed to block");
        std::thread::yield_now();
    }
    let pause = heap.pause_world();
    if poison {
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _held = held;
                panic!("poison contended metadata");
            }))
            .is_err()
        );
    } else {
        drop(held);
    }
    // Give the already parked waiter its native lock wakeup. Old lock_safe
    // retains ownership throughout the pause; a fixed waiter may release it
    // before it is observed. The scan below must finish in either schedule.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match mutex.try_lock() {
            Err(TryLockError::WouldBlock) => break,
            Ok(guard) => drop(guard),
            Err(TryLockError::Poisoned(error)) => drop(error.into_inner()),
        }
        std::thread::yield_now();
    }
    println!("Scanning embedding root metadata during a world pause");
    std::io::stdout().flush().unwrap();
    unsafe {
        MetadataRoots(&mutex).scan_roots(&mut |_| {});
    }
    drop(pause);
    worker.join().unwrap();
    assert_eq!(
        *mutex.lock().unwrap_or_else(|error| error.into_inner()),
        if poison { 0 } else { 1 }
    );
}

#[test]
fn collector_lock_safe_allows_embedding_root_scan_during_pause() {
    embedding_scan_case(
        concat!(
            module_path!(),
            "::collector_lock_safe_allows_embedding_root_scan_during_pause"
        ),
        false,
    );
}

#[test]
fn collector_lock_safe_contended_poison_allows_embedding_root_scan() {
    embedding_scan_case(
        concat!(
            module_path!(),
            "::collector_lock_safe_contended_poison_allows_embedding_root_scan"
        ),
        true,
    );
}

#[test]
fn collector_lock_safe_poison_preserves_running_state() {
    let mutex = Arc::new(Mutex::new(()));
    let other = mutex.clone();
    assert!(
        std::thread::spawn(move || {
            let _guard = other.lock().unwrap();
            panic!("poison metadata");
        })
        .join()
        .is_err()
    );
    let heap = Arc::new(Heap::new::<Full>(4096, vec![]));
    let mutator = MutatorThread::<IdentityPtrPolicy>::register(heap);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = unsafe { mutator.lock_safe(&mutex) };
        }))
        .is_err()
    );
    assert!(!mutator.state().is_blocked());
    assert!(!mutator.state().is_at_safepoint());
    let clean = Mutex::new(42);
    assert_eq!(*unsafe { mutator.lock_safe(&clean) }, 42);
}

fn sibling_wait_case(name: &str, poison: bool) {
    if std::env::var_os("GCR_LOCK_SAFE_SIBLING_CHILD").is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("GCR_LOCK_SAFE_SIBLING_CHILD", "1")
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("sibling wait timed out: {:?}", child.wait_with_output().unwrap());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("test {name} ... ok")), "{output:?}");
        return;
    }
    use crate::gc::{ObjHeader, RootFrame, TypeInfo};
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Arc::new(Heap::new::<Full>(16384, vec![info.clone()]));
    let mutex = Arc::new(Mutex::new(0));
    let held = mutex.lock().unwrap();
    let (send, receive) = mpsc::channel();
    let other = heap.clone(); let lock = mutex.clone();
    let worker = std::thread::spawn(move || {
        let caller = MutatorThread::<IdentityPtrPolicy>::register(other.clone());
        let sibling = MutatorThread::<IdentityPtrPolicy>::register(other.clone());
        let blocked = MutatorThread::<IdentityPtrPolicy>::register(other.clone());
        let first = RootFrame::<1>::new();
        let second = RootFrame::<1>::new();
        let _first_guard = unsafe { caller.frame_chain().push(&first) };
        let _second_guard = unsafe { sibling.frame_chain().push(&second) };
        unsafe {
            let a = caller.alloc_obj::<Full>(&info, 0);
            assert!(!a.is_null());
            a.add(Full::SIZE).cast::<u64>().write(40);
            first.slots[0].set(a as u64);
            let b = sibling.alloc_obj::<Full>(&info, 0);
            assert!(!b.is_null());
            b.add(Full::SIZE).cast::<u64>().write(2);
            second.slots[0].set(b as u64);
            blocked.state().enter_blocked();
        }
        send.send([crate::gc::ThreadStateHandle::as_ptr(caller.state()) as usize, crate::gc::ThreadStateHandle::as_ptr(sibling.state()) as usize, crate::gc::ThreadStateHandle::as_ptr(blocked.state()) as usize]).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = unsafe { caller.lock_safe(&lock) };
            *guard += 1;
        }));
        let restored = !caller.state().is_blocked() && !sibling.state().is_blocked() && blocked.state().is_blocked();
        unsafe { blocked.state().exit_blocked(&other); }
        let a = first.slots[0].get() as *const u8;
        let b = second.slots[0].get() as *const u8;
        let canonical = other.from_space_contains(a) && other.from_space_contains(b);
        let sum = if canonical { unsafe {
            a.add(Full::SIZE).cast::<u64>().read() + b.add(Full::SIZE).cast::<u64>().read()
        } } else { 0 };
        (result.is_err(), restored, canonical, sum)
    });
    let addresses = receive.recv_timeout(Duration::from_secs(5)).unwrap();
    // Owners live until join; only atomic observations cross threads here.
    let states = addresses.map(|address| unsafe { &*(address as *const ThreadState) });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !states[0].is_blocked() {
        assert!(Instant::now() < deadline, "caller failed to block");
        std::thread::yield_now();
    }
    // Group parking holds the census mutex through every state transition.
    // Synchronize with its completion before observing the sibling flags.
    let census = heap.threads.lock().unwrap();
    let siblings_parked = states[1].is_blocked() && states[2].is_blocked();
    drop(census);
    // The old path cannot safely complete a collection with a RUNNING sibling.
    // Release and join it before failing, rather than deliberately deadlocking.
    if siblings_parked { unsafe { heap.stw_collect::<IdentityPtrPolicy>(); } }
    if poison {
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = held;
            panic!("poison sibling wait");
        })).is_err());
    } else { drop(held); }
    let (panicked, restored, canonical, sum) = worker.join().unwrap();
    assert_eq!(panicked, poison);
    assert!(restored, "RUNNING/BLOCKED sibling states were not restored");
    assert!(canonical, "sibling frame roots point outside current from-space");
    assert_eq!(sum, 42, "sibling frame roots were not relocated correctly");
    assert!(siblings_parked, "RUNNING sibling was not suspended during lock_safe wait");
    assert_eq!(heap.collections(), 1);
}

#[test]
fn collector_lock_safe_parks_running_siblings_and_preserves_blocked_siblings() {
    sibling_wait_case("gc::lock_safe_tests::collector_lock_safe_parks_running_siblings_and_preserves_blocked_siblings", false);
}
#[test]
fn collector_lock_safe_contended_poison_restores_running_and_blocked_siblings() {
    sibling_wait_case("gc::lock_safe_tests::collector_lock_safe_contended_poison_restores_running_and_blocked_siblings", true);
}

#[test]
fn collector_lock_safe_warm_uncontended_path_does_not_park_or_allocate() {
    let heap = Arc::new(Heap::new::<Full>(4096, vec![]));
    let caller = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
    let sibling = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
    let blocked = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
    unsafe { blocked.state().enter_blocked(); }
    let generations = [caller.state().safepoint_gen(), sibling.state().safepoint_gen(), blocked.state().safepoint_gen()];
    let mutex = Mutex::new(41);
    // Native mutex backends can initialize lazily. Measure steady-state lock
    // acquisition separately from that platform-owned preparation allocation.
    drop(mutex.lock().unwrap());
    crate::runtime::allocation_probe::start();
    let mut guard = unsafe { caller.lock_safe(&mutex) };
    *guard += 1;
    drop(guard);
    let traffic = crate::runtime::allocation_probe::finish_bytes();
    assert_eq!(traffic, (0,0,0,0));
    assert_eq!(generations, [caller.state().safepoint_gen(), sibling.state().safepoint_gen(), blocked.state().safepoint_gen()]);
    assert!(!caller.state().is_blocked() && !sibling.state().is_blocked() && blocked.state().is_blocked());
    unsafe { blocked.state().exit_blocked(&heap); }
}
