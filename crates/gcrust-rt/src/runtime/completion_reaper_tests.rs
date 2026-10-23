use super::*;
use crate::gc::{Compact, ObjHeader, PtrPolicy};
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn bounded(name: &str) -> bool {
    bounded_exit(name, false)
}

fn bounded_exit(name: &str, abort: bool) -> bool {
    if std::env::var_os("GCR_REAPER_CHILD").is_some() {
        return true;
    }
    for workers in [1, 4] {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("runtime::completion_reaper_tests::{name}"),
                "--nocapture",
            ])
            .env("GCR_REAPER_CHILD", "1")
            .env("GCR_GC_WORKERS", workers.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        fn read(mut pipe: impl std::io::Read) -> Vec<u8> {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).unwrap();
            bytes
        }
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let out = std::thread::spawn(move || read(stdout));
        let err = std::thread::spawn(move || read(stderr));
        let end = Instant::now() + Duration::from_secs(25);
        let timed_out = loop {
            if child.try_wait().unwrap().is_some() {
                break false;
            }
            if Instant::now() >= end {
                child.kill().unwrap();
                break true;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let status = child.wait().unwrap();
        let out = out.join().unwrap();
        let err = err.join().unwrap();
        #[cfg(unix)]
        let expected = {
            use std::os::unix::process::ExitStatusExt;
            if abort {
                status.signal() == Some(6)
            } else {
                status.success()
            }
        };
        #[cfg(not(unix))]
        let expected = if abort {
            !status.success()
        } else {
            status.success()
        };
        let witness = !abort
            || String::from_utf8_lossy(&err).contains("orphan reaper cleanup failure fixture");
        assert!(
            !timed_out && expected && witness,
            "workers={workers}, timeout={timed_out}, {status}: {} {}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&err)
        );
    }
    false
}
fn until(mut predicate: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < end, "native completion did not progress");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn finished(owner: usize) -> Arc<JoinHandle> {
    let native = std::thread::spawn(|| 42);
    until(|| native.is_finished());
    Arc::new(JoinHandle {
        completion_notice: false,
        daemon: std::sync::atomic::AtomicBool::new(false),
        native_joined: std::sync::atomic::AtomicBool::new(false),
        pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
        reaper_registration: std::sync::OnceLock::new(),
        retirement: std::sync::OnceLock::new(),
        state: Mutex::new(JoinState::Running(native)),
        owner: std::sync::atomic::AtomicUsize::new(owner),
    })
}
struct Tagged;
impl PtrPolicy for Tagged {
    fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
        (bits != 0).then_some((bits & !3) as *mut u8)
    }
    fn encode_ptr(ptr: *mut u8) -> u64 {
        ptr as u64 | 3
    }
}
fn monitor(heap: &Heap) -> Arc<completion_reaper::Monitor> {
    heap.thread_completions
        .lock()
        .unwrap()
        .reaper
        .as_ref()
        .unwrap()
        .monitor()
}
fn case<H: ObjHeader, P: PtrPolicy>(mode: usize) {
    let info = TypeInfo::for_header(H::SIZE).with_raw_bytes(8);
    let heap = Heap::new_generational::<H>(1024, 8192, vec![info.clone()]);
    heap.set_gc_every_alloc(matches!(mode, 2 | 4));
    let (state, _) = heap.register_thread();
    let owner = unsafe {
        if matches!(mode, 2 | 4) {
            heap.alloc_nursery_obj::<H>(&info, 0)
        } else {
            heap.alloc_obj::<H>(&info, 0)
        }
    };
    let record = finished(owner as usize);
    let weak = Arc::downgrade(&record);
    let raw = finished(1);
    let raw_weak = Arc::downgrade(&raw);
    {
        let mut registry = heap.thread_completions.lock().unwrap();
        registry.register(record, &heap.native_retirement);
        registry.register(raw, &heap.native_retirement);
    }
    let watch = monitor(&heap);
    let collect = |mode| unsafe {
        match mode {
            0 => {
                state.enter_blocked();
                heap.stw_collect::<P>();
                state.exit_blocked(&heap);
            }
            1 => heap.mutator_triggered_gc::<P>(&state),
            2 | 4 => heap.mutator_triggered_minor_gc::<P>(&state),
            3 => {
                state.enter_blocked();
                heap.concurrent_collect::<P>();
                state.exit_blocked(&heap);
            }
            _ => unreachable!(),
        }
    };
    if mode == 4 {
        collect(1);
        assert!(watch.is_empty());
        assert!(
            matches!(
                *weak.upgrade().unwrap().state.lock().unwrap(),
                JoinState::Running(_)
            ),
            "GC alone must not enqueue an owner from an uncollected generation"
        );
    }
    collect(mode);
    until(|| watch.is_empty());
    assert!(weak.upgrade().unwrap().is_complete());
    collect(mode);
    until(|| weak.upgrade().is_none());
    assert_eq!(heap.thread_completions.lock().unwrap().records.len(), 1);
    let raw = raw_weak.upgrade().unwrap();
    assert!(matches!(*raw.state.lock().unwrap(), JoinState::Running(_)));
    assert_eq!(raw.join(), 42);
    drop(raw);
    heap.thread_completions.lock().unwrap().clear();
    unsafe {
        heap.deregister_thread(&state);
    }
}
#[test]
fn collector_orphan_reaper_reclaims_finished_threads_across_copy_paths() {
    if !bounded("collector_orphan_reaper_reclaims_finished_threads_across_copy_paths") {
        return;
    }
    for mode in 0..5 {
        case::<Full, IdentityPtrPolicy>(mode);
        case::<Full, Tagged>(mode);
        case::<Compact, IdentityPtrPolicy>(mode);
        case::<Compact, Tagged>(mode);
    }
}

type Gate = Arc<(Mutex<bool>, Condvar)>;
struct ExitGate {
    gate: Gate,
    entered: std::sync::mpsc::Sender<()>,
}
impl Drop for ExitGate {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        let (lock, cv) = &*self.gate;
        let mut open = lock.lock().unwrap();
        while !*open {
            open = cv.wait(open).unwrap();
        }
    }
}
thread_local! {static EXIT:std::cell::RefCell<Option<ExitGate>>=const {std::cell::RefCell::new(None)};}
fn blocked_exit(owner: usize) -> (Arc<JoinHandle>, Gate) {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let release = gate.clone();
    let (entered, wait) = std::sync::mpsc::channel();
    let native = std::thread::spawn(move || {
        EXIT.with(|cell| {
            *cell.borrow_mut() = Some(ExitGate {
                gate: release,
                entered,
            })
        });
        77
    });
    wait.recv_timeout(Duration::from_secs(5)).unwrap();
    until(|| native.is_finished());
    (
        Arc::new(JoinHandle {
            completion_notice: false,
            daemon: std::sync::atomic::AtomicBool::new(false),
            native_joined: std::sync::atomic::AtomicBool::new(false),
            pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
            reaper_registration: std::sync::OnceLock::new(),
            retirement: std::sync::OnceLock::new(),
            state: Mutex::new(JoinState::Running(native)),
            owner: std::sync::atomic::AtomicUsize::new(owner),
        }),
        gate,
    )
}
fn release(gate: &Gate) {
    let (lock, cv) = &**gate;
    *lock.lock().unwrap() = true;
    cv.notify_all();
}

#[test]
fn collector_orphan_reaper_keeps_gc_live_during_tls_exit_and_shutdown_waits() {
    if !bounded("collector_orphan_reaper_keeps_gc_live_during_tls_exit_and_shutdown_waits") {
        return;
    }
    let (ready, receive) = std::sync::mpsc::channel();
    let (done, finished) = std::sync::mpsc::channel();
    let root = std::thread::spawn(move || {
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let mut runtime = RuntimeContext::new(8192, vec![info.clone()]);
        let owner = unsafe { runtime.heap().alloc_obj::<Full>(&info, 0) };
        let (record, gate) = blocked_exit(owner as usize);
        let key = Arc::as_ptr(&record) as usize;
        let weak = Arc::downgrade(&record);
        runtime
            .heap()
            .thread_completions
            .lock()
            .unwrap()
            .register(record, &runtime.heap().native_retirement);
        unsafe {
            runtime
                .heap()
                .mutator_triggered_gc::<IdentityPtrPolicy>(&runtime.thread.registration.dyna);
        }
        let watch = monitor(runtime.heap());
        until(|| watch.is_joining(key));
        ready
            .send((runtime.thread.registration.heap.clone(), weak, gate))
            .unwrap();
        unsafe {
            runtime.finish_threads();
        }
        drop(runtime);
        done.send(()).unwrap();
    });
    let (heap, weak, gate) = receive.recv_timeout(Duration::from_secs(5)).unwrap();
    for _ in 0..3 {
        unsafe {
            heap.stw_collect::<IdentityPtrPolicy>();
        }
    }
    assert!(finished.try_recv().is_err());
    assert!(weak.upgrade().is_some());
    release(&gate);
    finished.recv_timeout(Duration::from_secs(5)).unwrap();
    root.join().unwrap();
    until(|| weak.upgrade().is_none());
    let registry = heap.thread_completions.lock().unwrap();
    assert!(
        registry.records.is_empty() && registry.pending.is_empty() && registry.reaper.is_none()
    );
}

#[test]
fn collector_orphan_reaper_drains_backlog_without_another_collection() {
    if !bounded("collector_orphan_reaper_drains_backlog_without_another_collection") {
        return;
    }
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Heap::new::<Full>(16384, vec![info.clone()]);
    let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let (blocker, gate) = blocked_exit(owner as usize);
    let key = Arc::as_ptr(&blocker) as usize;
    let weak = Arc::downgrade(&blocker);
    heap.thread_completions.lock().unwrap().register(blocker, &heap.native_retirement);
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    let watch = monitor(&heap);
    until(|| watch.is_joining(key));
    for _ in 0..256 {
        let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
        heap.thread_completions
            .lock()
            .unwrap()
            .register(finished(owner as usize), &heap.native_retirement);
    }
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    assert_eq!(watch.len(), 257);
    release(&gate);
    // Exceed the bounded scan batch, then require all native cleanup without
    // allocating or collecting again: GC frequency must not limit throughput.
    until(|| watch.is_empty());
    assert!(watch.max_batch() <= 64 && watch.max_batch() > 1);
    assert!(
        heap.thread_completions
            .lock()
            .unwrap()
            .records
            .values()
            .all(|record| record.is_complete())
    );
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    until(|| weak.upgrade().is_none());
    assert!(heap.thread_completions.lock().unwrap().records.is_empty());
}

#[test]
fn collector_orphan_reaper_independent_heaps_shutdown_without_cross_heap_waits() {
    if !bounded("collector_orphan_reaper_independent_heaps_shutdown_without_cross_heap_waits") {
        return;
    }
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Heap::new::<Full>(8192, vec![info.clone()]);
    let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let (blocker, gate) = blocked_exit(owner as usize);
    let key = Arc::as_ptr(&blocker) as usize;
    heap.thread_completions.lock().unwrap().register(blocker, &heap.native_retirement);
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    let watch = monitor(&heap);
    until(|| watch.is_joining(key));
    let (done, receive) = std::sync::mpsc::channel();
    let other = std::thread::spawn(move || {
        let mut runtime = RuntimeContext::new(8192, vec![info.clone()]);
        let owner = unsafe { runtime.heap().alloc_obj::<Full>(&info, 0) };
        let record = finished(owner as usize);
        let weak = Arc::downgrade(&record);
        runtime
            .heap()
            .thread_completions
            .lock()
            .unwrap()
            .register(record, &runtime.heap().native_retirement);
        unsafe {
            runtime
                .heap()
                .mutator_triggered_gc::<IdentityPtrPolicy>(&runtime.thread.registration.dyna);
        }
        unsafe {
            runtime.finish_threads();
        }
        drop(runtime);
        until(|| weak.upgrade().is_none());
        done.send(()).unwrap();
    });
    receive.recv_timeout(Duration::from_secs(5)).unwrap();
    other.join().unwrap();
    release(&gate);
    until(|| watch.is_empty());
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    assert!(heap.thread_completions.lock().unwrap().records.is_empty());
}

#[test]
fn collector_orphan_reaper_skips_unfinished_threads_without_stranding_ready_ones() {
    if !bounded("collector_orphan_reaper_skips_unfinished_threads_without_stranding_ready_ones") {
        return;
    }
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Heap::new::<Full>(8192, vec![info.clone()]);
    let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let owner2 = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let (release, wait) = std::sync::mpsc::channel();
    let blocked = Arc::new(JoinHandle {
        completion_notice: false,
        daemon: std::sync::atomic::AtomicBool::new(false),
        native_joined: std::sync::atomic::AtomicBool::new(false),
        pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
        reaper_registration: std::sync::OnceLock::new(),
        retirement: std::sync::OnceLock::new(),
        state: Mutex::new(JoinState::Running(std::thread::spawn(move || {
            wait.recv().unwrap();
            77
        }))),
        owner: std::sync::atomic::AtomicUsize::new(owner as usize),
    });
    let blocked_key = Arc::as_ptr(&blocked) as usize;
    let ready = finished(owner2 as usize);
    {
        let mut registry = heap.thread_completions.lock().unwrap();
        registry.register(blocked.clone(), &heap.native_retirement);
        registry.register(ready.clone(), &heap.native_retirement);
    }
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    let watch = monitor(&heap);
    until(|| ready.is_complete() && watch.len() == 1 && watch.is_deferred(blocked_key));
    assert!(!blocked.is_complete());
    assert_eq!(watch.len(), 1);
    release.send(()).unwrap();
    until(|| watch.is_empty());
    assert_eq!(blocked.join(), 77);
    assert_eq!(ready.join(), 42);
    drop(blocked);
    drop(ready);
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    assert!(heap.thread_completions.lock().unwrap().records.is_empty());
}

#[test]
fn collector_orphan_reaper_drops_native_panic_payloads_outside_collection() {
    if !bounded("collector_orphan_reaper_drops_native_panic_payloads_outside_collection") {
        return;
    }
    struct Probe {
        heap: Arc<Heap>,
        done: std::sync::mpsc::Sender<()>,
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            let _pause = self.heap.pause_world();
            self.done.send(()).unwrap();
        }
    }
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Arc::new(Heap::new::<Full>(8192, vec![info.clone()]));
    let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let (done, receive) = std::sync::mpsc::channel();
    let target = heap.clone();
    let native = std::thread::spawn(move || std::panic::panic_any(Probe { heap: target, done }));
    until(|| native.is_finished());
    let record = Arc::new(JoinHandle {
        completion_notice: false,
        daemon: std::sync::atomic::AtomicBool::new(false),
        native_joined: std::sync::atomic::AtomicBool::new(false),
        pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
        reaper_registration: std::sync::OnceLock::new(),
        retirement: std::sync::OnceLock::new(),
        state: Mutex::new(JoinState::Running(native)),
        owner: std::sync::atomic::AtomicUsize::new(owner as usize),
    });
    let weak = Arc::downgrade(&record);
    heap.thread_completions
        .lock()
        .unwrap()
        .register(record.clone(), &heap.native_retirement);
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    let watch = monitor(&heap);
    until(|| watch.is_empty());
    receive.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(record.join(), 0);
    assert_eq!(record.join(), 0);
    drop(record);
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    until(|| weak.upgrade().is_none());
}
unsafe extern "C" fn scalar_child(_: *mut Thread, _: *mut u8) -> u64 {
    42
}
#[test]
fn collector_orphan_reaper_reclaims_actual_unobserved_spawn_churn() {
    if !bounded("collector_orphan_reaper_reclaims_actual_unobserved_spawn_churn") {
        return;
    }
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let mut runtime = RuntimeContext::new(8192, vec![info]);
    let active_thread = runtime.thread_ptr();
    let execute = || {
    for _ in 0..128 {
        let thread = runtime.thread_ptr();
        let env = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
        let handle = unsafe { ai_thread_spawn(thread, env, scalar_child as *const u8) };
        let owner = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
        unsafe {
            ai_thread_bind_owner(thread, handle, owner);
        }
        let weak = {
            let registry = runtime.heap().thread_completions.lock().unwrap();
            Arc::downgrade(&registry.records[&(handle as usize)])
        };
        unsafe {
            runtime
                .heap()
                .mutator_triggered_gc::<IdentityPtrPolicy>(&runtime.thread.registration.dyna);
        }
        let watch = monitor(runtime.heap());
        // An empty notice catalogue also permits bodies that have not exited.
        // Wait for actual native completion, then the transient worker borrow.
        until(|| weak.upgrade().is_none_or(|record| record.is_complete()));
        until(|| watch.is_empty());
        unsafe {
            runtime
                .heap()
                .mutator_triggered_gc::<IdentityPtrPolicy>(&runtime.thread.registration.dyna);
        }
        until(|| weak.upgrade().is_none());
        assert_eq!(runtime.heap().from_used(), 0);
        let registry = runtime.heap().thread_completions.lock().unwrap();
        assert!(registry.records.is_empty() && registry.pending.is_empty());
    }
    {
        let registry = runtime.heap().thread_completions.lock().unwrap();
        assert!(registry.records.capacity() <= 8 && registry.owned.capacity() <= 8);
    }
    };
    unsafe { with_current_thread(active_thread, execute); }
    unsafe {
        runtime.finish_threads();
    }
    assert!(
        runtime
            .heap()
            .thread_completions
            .lock()
            .unwrap()
            .reaper
            .is_none()
    );
}
#[test]
fn collector_orphan_reaper_cleanup_panics_abort_the_process() {
    if !bounded_exit(
        "collector_orphan_reaper_cleanup_panics_abort_the_process",
        true,
    ) {
        return;
    }
    struct Bomb;
    impl Drop for Bomb {
        fn drop(&mut self) {
            panic!("orphan reaper cleanup failure fixture");
        }
    }
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Heap::new::<Full>(8192, vec![info.clone()]);
    let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let native = std::thread::spawn(|| std::panic::panic_any(Bomb));
    until(|| native.is_finished());
    let record = Arc::new(JoinHandle {
        completion_notice: false,
        daemon: std::sync::atomic::AtomicBool::new(false),
        native_joined: std::sync::atomic::AtomicBool::new(false),
        pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
        reaper_registration: std::sync::OnceLock::new(),
        retirement: std::sync::OnceLock::new(),
        state: Mutex::new(JoinState::Running(native)),
        owner: std::sync::atomic::AtomicUsize::new(owner as usize),
    });
    heap.thread_completions.lock().unwrap().register(record, &heap.native_retirement);
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    loop {
        std::thread::park();
    }
}

static EXIT_DESCENDANT_ALLOW: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static EXIT_DESCENDANT_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static EXIT_DESCENDANT_DONE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
unsafe extern "C" fn exit_descendant(thread: *mut Thread, _: *mut u8) -> u64 {
    EXIT_DESCENDANT_STARTED.store(true, std::sync::atomic::Ordering::Release);
    while !EXIT_DESCENDANT_ALLOW.load(std::sync::atomic::Ordering::Acquire) {
        unsafe {
            ai_thread_sleep(thread, 1);
        }
    }
    EXIT_DESCENDANT_DONE.store(true, std::sync::atomic::Ordering::Release);
    42
}
struct ReaperExitAdmission {
    heap: Arc<Heap>,
    arguments: Arc<Vec<std::ffi::OsString>>,
}
impl Drop for ReaperExitAdmission {
    fn drop(&mut self) {
        let parent = RuntimeThread::new(self.heap.clone(), self.arguments.clone());
        let thread = parent.thread.get();
        let execute = || {
        let env = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
        unsafe {
            ai_thread_spawn(thread, env, exit_descendant as *const u8);
        }
        };
        unsafe { with_current_thread(thread, execute); }
    }
}
thread_local! {static REAPER_EXIT_ADMISSION:std::cell::RefCell<Option<ReaperExitAdmission>>=const{std::cell::RefCell::new(None)};}
#[test]
fn collector_orphan_reaper_shutdown_waits_for_worker_tls_and_late_descendants() {
    if !bounded("collector_orphan_reaper_shutdown_waits_for_worker_tls_and_late_descendants") {
        return;
    }
    struct Install {
        heap: Arc<Heap>,
        arguments: Arc<Vec<std::ffi::OsString>>,
    }
    impl Drop for Install {
        fn drop(&mut self) {
            REAPER_EXIT_ADMISSION.with(|slot| {
                *slot.borrow_mut() = Some(ReaperExitAdmission {
                    heap: self.heap.clone(),
                    arguments: self.arguments.clone(),
                })
            });
        }
    }
    let (ready, receive) = std::sync::mpsc::channel();
    let (done, finished) = std::sync::mpsc::channel();
    let root = std::thread::spawn(move || {
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let mut runtime = RuntimeContext::new(8192, vec![info.clone()]);
        let weak_heap = Arc::downgrade(&runtime.thread.registration.heap);
        let owner = unsafe { runtime.heap().alloc_obj::<Full>(&info, 0) };
        let install = Install {
            heap: runtime.thread.registration.heap.clone(),
            arguments: runtime.thread.arguments.clone(),
        };
        let native = std::thread::spawn(move || std::panic::panic_any(install));
        until(|| native.is_finished());
        runtime
            .heap()
            .thread_completions
            .lock()
            .unwrap()
            .register(Arc::new(JoinHandle {
                completion_notice: false,
                daemon: std::sync::atomic::AtomicBool::new(false),
                native_joined: std::sync::atomic::AtomicBool::new(false),
                pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
                reaper_registration: std::sync::OnceLock::new(),
                retirement: std::sync::OnceLock::new(),
                state: Mutex::new(JoinState::Running(native)),
                owner: std::sync::atomic::AtomicUsize::new(owner as usize),
            }), &runtime.heap().native_retirement);
        unsafe {
            runtime
                .heap()
                .mutator_triggered_gc::<IdentityPtrPolicy>(&runtime.thread.registration.dyna);
        }
        let watch = monitor(runtime.heap());
        until(|| watch.is_empty());
        ready.send(weak_heap).unwrap();
        unsafe {
            runtime.finish_threads();
        }
        assert!(EXIT_DESCENDANT_DONE.load(std::sync::atomic::Ordering::Acquire));
        {
            let registry = runtime.heap().thread_completions.lock().unwrap();
            assert!(
                registry.records.is_empty()
                    && registry.pending.is_empty()
                    && registry.reaper.is_none()
            );
        }
        drop(runtime);
        done.send(()).unwrap();
    });
    let weak_heap = receive.recv_timeout(Duration::from_secs(5)).unwrap();
    until(|| EXIT_DESCENDANT_STARTED.load(std::sync::atomic::Ordering::Acquire));
    {
        let heap = weak_heap.upgrade().unwrap();
        unsafe {
            heap.stw_collect::<IdentityPtrPolicy>();
        }
    }
    assert!(
        finished.try_recv().is_err(),
        "shutdown returned before its late child completed"
    );
    EXIT_DESCENDANT_ALLOW.store(true, std::sync::atomic::Ordering::Release);
    finished.recv_timeout(Duration::from_secs(5)).unwrap();
    root.join().unwrap();
    assert!(weak_heap.upgrade().is_none());
}

#[test]
fn collector_orphan_reaper_retries_observer_contention_without_another_collection() {
    if !bounded("collector_orphan_reaper_retries_observer_contention_without_another_collection") {
        return;
    }
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Heap::new::<Full>(8192, vec![info.clone()]);
    let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let record = finished(owner as usize);
    let weak = Arc::downgrade(&record);
    heap.thread_completions
        .lock()
        .unwrap()
        .register(record.clone(), &heap.native_retirement);
    let guard = record.state.lock().unwrap();
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    let watch = monitor(&heap);
    assert_eq!(watch.len(), 1);
    // A temporary observer lock must defer the probe, not discard the orphan.
    // Releasing the lock is enough for cleanup; no later GC is required.
    drop(guard);
    until(|| watch.is_empty());
    assert!(record.is_complete());
    assert_eq!(record.join(), 42);
    drop(record);
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    until(|| weak.upgrade().is_none());
}

#[test]
fn collector_orphan_reaper_gc_prunes_observed_orphans_while_worker_is_busy() {
    if !bounded("collector_orphan_reaper_gc_prunes_observed_orphans_while_worker_is_busy") {
        return;
    }
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Heap::new::<Full>(8192, vec![info.clone()]);
    let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let (blocker, gate) = blocked_exit(owner as usize);
    let key = Arc::as_ptr(&blocker) as usize;
    heap.thread_completions.lock().unwrap().register(blocker, &heap.native_retirement);
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    let watch = monitor(&heap);
    until(|| watch.is_joining(key));
    let owner = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let record = finished(owner as usize);
    let weak = Arc::downgrade(&record);
    heap.thread_completions
        .lock()
        .unwrap()
        .register(record.clone(), &heap.native_retirement);
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    assert_eq!(watch.len(), 2);
    // Shutdown can observe a handle before the background worker reaches it.
    // GC must remove that now-complete weak index entry even if cleanup stalls.
    assert_eq!(record.join(), 42);
    drop(record);
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    until(|| weak.upgrade().is_none());
    assert_eq!(watch.len(), 1);
    release(&gate);
    until(|| watch.is_empty());
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    assert!(heap.thread_completions.lock().unwrap().records.is_empty());
}

#[test]
fn collector_orphan_reaper_shutdown_waits_for_worker_tls_without_blocking_gc() {
    if !bounded("collector_orphan_reaper_shutdown_waits_for_worker_tls_without_blocking_gc") {
        return;
    }
    struct InstallGate {
        gate: Gate,
        entered: std::sync::mpsc::Sender<()>,
    }
    impl Drop for InstallGate {
        fn drop(&mut self) {
            EXIT.with(|slot| {
                *slot.borrow_mut() = Some(ExitGate {
                    gate: self.gate.clone(),
                    entered: self.entered.clone(),
                })
            });
        }
    }
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let release_gate = gate.clone();
    let (entered, wait) = std::sync::mpsc::channel();
    let (ready, receive) = std::sync::mpsc::channel();
    let (done, finished) = std::sync::mpsc::channel();
    let root = std::thread::spawn(move || {
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let mut runtime = RuntimeContext::new(8192, vec![info.clone()]);
        let owner = unsafe { runtime.heap().alloc_obj::<Full>(&info, 0) };
        let native =
            std::thread::spawn(move || std::panic::panic_any(InstallGate { gate, entered }));
        until(|| native.is_finished());
        runtime
            .heap()
            .thread_completions
            .lock()
            .unwrap()
            .register(Arc::new(JoinHandle {
                completion_notice: false,
                daemon: std::sync::atomic::AtomicBool::new(false),
                native_joined: std::sync::atomic::AtomicBool::new(false),
                pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
                reaper_registration: std::sync::OnceLock::new(),
                retirement: std::sync::OnceLock::new(),
                state: Mutex::new(JoinState::Running(native)),
                owner: std::sync::atomic::AtomicUsize::new(owner as usize),
            }), &runtime.heap().native_retirement);
        unsafe {
            runtime
                .heap()
                .mutator_triggered_gc::<IdentityPtrPolicy>(&runtime.thread.registration.dyna);
        }
        let watch = monitor(runtime.heap());
        until(|| watch.is_empty());
        ready
            .send(runtime.thread.registration.heap.clone())
            .unwrap();
        unsafe {
            runtime.finish_threads();
        }
        drop(runtime);
        done.send(()).unwrap();
    });
    let heap = receive.recv_timeout(Duration::from_secs(5)).unwrap();
    wait.recv_timeout(Duration::from_secs(5)).unwrap();
    for _ in 0..3 {
        unsafe {
            heap.stw_collect::<IdentityPtrPolicy>();
        }
    }
    assert!(
        finished.try_recv().is_err(),
        "shutdown detached a worker still executing TLS cleanup"
    );
    release(&release_gate);
    finished.recv_timeout(Duration::from_secs(5)).unwrap();
    root.join().unwrap();
    assert!(heap.thread_completions.lock().unwrap().reaper.is_none());
}

unsafe extern "C" fn completed_value(_: *mut Thread, env: *mut u8) -> u64 {
    unsafe { env.add(Full::SIZE).cast::<u64>().read() }
}

#[test]
fn collector_completion_reaper_finishes_live_and_raw_handles_without_gc() {
    if !bounded("collector_completion_reaper_finishes_live_and_raw_handles_without_gc") {
        return;
    }
    let env_info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let owner_info = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_raw_bytes(8);
    let mut context = RuntimeContext::new_generational(8192, 65536, vec![env_info, owner_info]);
    let heap = context.heap().clone();
    context.enter(|thread| unsafe {
        let state = &*(*thread).dyna_thread;
        let roots = state.scratch_scope();
        let env = ai_gc_alloc_fixed(thread, 0, 0);
        env.add(Full::SIZE).cast::<u64>().write(81);
        let env = roots.push(env);
        let mut handles = Vec::new();
        for i in 0..32 {
            let handle = ai_thread_spawn(
                thread,
                roots.get(env),
                completed_value as *const () as *const u8,
            );
            if i % 2 == 0 {
                let owner = ai_gc_alloc_fixed(thread, 1, 0);
                heap.globals.add(owner as u64);
                ai_thread_bind_owner(thread, handle, owner);
            }
            handles.push(handle);
        }
        until(|| {
            heap.thread_completions
                .lock()
                .unwrap()
                .records
                .values()
                .all(|r| r.is_complete())
        });
        assert_eq!(heap.collections(), 0);
        assert_eq!(heap.minor_collections(), 0);
        assert_eq!(heap.thread_completions.lock().unwrap().records.len(), 32);
        for handle in handles {
            assert_eq!(ai_thread_join(thread, handle), 81);
            assert_eq!(ai_thread_join(thread, handle), 81);
        }
    });
}

#[test]
fn collector_completion_notice_handles_both_publication_orders_and_races() {
    if !bounded("collector_completion_notice_handles_both_publication_orders_and_races") {
        return;
    }
    let reaper = completion_reaper::Reaper::new();
    let monitor = reaper.monitor();
    for order in 0..5 {
        for _ in 0..32 {
            let record = finished(1);
            monitor.prepare(&record);
            let (notice, done) = completion_notice::CompletionNotice::new(monitor.clone());
            match order {
                0 => {
                    drop(done);
                    notice.publish(&record);
                }
                1 => {
                    notice.publish(&record);
                    drop(done);
                }
                2 => {
                    let barrier = Arc::new(std::sync::Barrier::new(2));
                    let child_barrier = barrier.clone();
                    let child = std::thread::spawn(move || {
                        let _done = done;
                        child_barrier.wait();
                    });
                    barrier.wait();
                    notice.publish(&record);
                    child.join().unwrap();
                }
                3 => {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _done = done;
                        panic!("completion notice setup-unwind fixture");
                    }));
                    assert!(outcome.is_err());
                    notice.publish(&record);
                }
                _ => {
                    let never_called = move || drop(done);
                    drop(never_called);
                    notice.publish(&record);
                }
            }
            until(|| record.is_complete());
            assert_eq!(record.join(), 42);
        }
    }
    until(|| monitor.is_empty());
}

static NOTICE_STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static NOTICE_ALLOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
unsafe extern "C" fn active_notice_body(thread: *mut Thread, _: *mut u8) -> u64 {
    NOTICE_STARTED.fetch_add(1, Ordering::Release);
    while !NOTICE_ALLOW.load(Ordering::Acquire) {
        unsafe {
            ai_thread_sleep(thread, 1);
        }
    }
    42
}

#[test]
fn collector_completion_notice_keeps_active_bodies_out_of_polling_catalogue() {
    if !bounded("collector_completion_notice_keeps_active_bodies_out_of_polling_catalogue") {
        return;
    }
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let mut context = RuntimeContext::new(65536, vec![info]);
    let heap = context.heap().clone();
    context.enter(|thread| unsafe {
        struct Release;
        impl Drop for Release {
            fn drop(&mut self) {
                NOTICE_ALLOW.store(true, Ordering::Release);
            }
        }
        let _release = Release;
        let roots = (&*(*thread).dyna_thread).scratch_scope();
        let env = roots.push(ai_gc_alloc_fixed(thread, 0, 0));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let handle = ai_thread_spawn(
                thread,
                roots.get(env),
                active_notice_body as *const () as *const u8,
            );
            let owner = ai_gc_alloc_fixed(thread, 0, 0);
            ai_thread_bind_owner(thread, handle, owner);
            handles.push(handle);
        }
        until(|| NOTICE_STARTED.load(Ordering::Acquire) == 4);
        let monitor = monitor(&heap);
        assert!(
            monitor.is_empty(),
            "active bodies were admitted to completion polling"
        );
        assert!(
            heap.thread_completions
                .lock()
                .unwrap()
                .records
                .values()
                .all(|r| !r.is_complete())
        );
        assert_eq!(heap.collections(), 0);
        let state = &*(*thread).dyna_thread;
        state.set_parked_jit_fp((*thread).top_frame as *const u8);
        heap.mutator_triggered_gc::<IdentityPtrPolicy>(state);
        state.clear_parked_jit_fp();
        assert!(
            heap.thread_completions
                .lock()
                .unwrap()
                .records
                .values()
                .all(|r| r.owner.load(Ordering::Acquire) == 0)
        );
        assert!(
            monitor.is_empty(),
            "GC added notified active bodies to polling"
        );
        NOTICE_ALLOW.store(true, Ordering::Release);
        until(|| {
            heap.thread_completions
                .lock()
                .unwrap()
                .records
                .values()
                .all(|r| r.is_complete())
        });
        for handle in handles {
            assert_eq!(ai_thread_join(thread, handle), 42);
        }
        until(|| monitor.is_empty());
    });
}

static NOTICE_TLS_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static NOTICE_TLS_ALLOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
struct NoticeTls;
impl Drop for NoticeTls {
    fn drop(&mut self) {
        NOTICE_TLS_STARTED.store(true, Ordering::Release);
        while !NOTICE_TLS_ALLOW.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
thread_local! {
    static NOTICE_TLS: std::cell::RefCell<Option<NoticeTls>> = const { std::cell::RefCell::new(None) };
}
unsafe extern "C" fn install_notice_tls(_: *mut Thread, _: *mut u8) -> u64 {
    NOTICE_TLS.with(|slot| *slot.borrow_mut() = Some(NoticeTls));
    42
}

#[test]
fn collector_completion_notice_waits_for_native_tls_and_preserves_live_owner_during_gc() {
    if !bounded(
        "collector_completion_notice_waits_for_native_tls_and_preserves_live_owner_during_gc",
    ) {
        return;
    }
    struct Release;
    impl Drop for Release {
        fn drop(&mut self) {
            NOTICE_TLS_ALLOW.store(true, Ordering::Release);
        }
    }
    let _release = Release;
    let (ready, receive) = std::sync::mpsc::channel();
    let (done, finished) = std::sync::mpsc::channel();
    let root = std::thread::spawn(move || {
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let mut context = RuntimeContext::new(65536, vec![info]);
        let heap = context.heap().clone();
        context.enter(|thread| unsafe {
            let roots = (&*(*thread).dyna_thread).scratch_scope();
            let env = roots.push(ai_gc_alloc_fixed(thread, 0, 0));
            let handle = ai_thread_spawn(
                thread,
                roots.get(env),
                install_notice_tls as *const () as *const u8,
            );
            let owner = ai_gc_alloc_fixed(thread, 0, 0);
            let owner_root = heap.globals.add(owner as u64);
            ai_thread_bind_owner(thread, handle, owner);
            let watch = monitor(&heap);
            until(|| {
                NOTICE_TLS_STARTED.load(Ordering::Acquire) && watch.is_joining(handle as usize)
            });
            assert!(
                !heap.thread_completions.lock().unwrap().records[&(handle as usize)].is_complete()
            );
            ready
                .send((heap.clone(), owner_root, handle as usize))
                .unwrap();
        });
        drop(context);
        done.send(()).unwrap();
    });
    let (heap, owner_root, handle) = receive.recv_timeout(Duration::from_secs(5)).unwrap();
    for _ in 0..3 {
        unsafe {
            heap.stw_collect::<IdentityPtrPolicy>();
        }
        let owner = heap.thread_completions.lock().unwrap().records[&handle]
            .owner
            .load(Ordering::Acquire);
        assert_eq!(owner, heap.globals.get(owner_root) as usize);
    }
    assert!(
        finished.try_recv().is_err(),
        "body-exit notice allowed code retirement before native TLS exit"
    );
    NOTICE_TLS_ALLOW.store(true, Ordering::Release);
    finished.recv_timeout(Duration::from_secs(5)).unwrap();
    root.join().unwrap();
    assert!(heap.thread_completions.lock().unwrap().records.is_empty());
}
