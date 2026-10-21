use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
};

static HEAP: AtomicPtr<Heap> = AtomicPtr::new(std::ptr::null_mut());
static PAUSED: AtomicBool = AtomicBool::new(false);
static RELEASE: AtomicBool = AtomicBool::new(false);
struct PausedIdentity;
impl PtrPolicy for PausedIdentity {
    fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
        let pointer = IdentityPtrPolicy::try_decode_ptr(bits);
        let heap = HEAP.load(Ordering::Acquire);
        if pointer.is_some() && !heap.is_null() {
            let heap = unsafe { &*heap };
            if heap.barriers_active()
                && !heap.gc_requested()
                && !PAUSED.swap(true, Ordering::AcqRel)
            {
                while !RELEASE.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            }
        }
        pointer
    }
    fn encode_ptr(pointer: *mut u8) -> u64 {
        pointer as u64
    }
}

#[test]
fn collector_satb_repeated_overwrites_bound_global_retention() {
    const CHILD: &str = "GCR_SATB_PRESSURE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gc::satb_pressure_tests::collector_satb_repeated_overwrites_bound_global_retention"])
            .env(CHILD, "1").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!(
                    "SATB pressure regression timed out: {:?}",
                    child.wait_with_output().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    let info = TypeInfo::for_header(Full::SIZE)
        .with_fields(2)
        .with_raw_bytes(8);
    let heap = Arc::new(Heap::new::<Full>(1 << 20, vec![info.clone()]));
    let parent = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let child = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let hidden = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    unsafe {
        parent.add(Full::SIZE).cast::<u64>().write(child as u64);
        parent
            .add(Full::SIZE + 8)
            .cast::<u64>()
            .write(hidden as u64);
        hidden.add(Full::SIZE + 16).cast::<u64>().write(42);
    }
    let root = unsafe { heap.globals.add(parent as u64) };
    HEAP.store(Arc::as_ptr(&heap).cast_mut(), Ordering::Release);
    let worker_heap = heap.clone();
    let (ready, started) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mt = MutatorThread::<PausedIdentity>::register(worker_heap.clone());
        ready.send(()).unwrap();
        while !PAUSED.load(Ordering::Acquire) {
            unsafe {
                mt.safepoint();
            }
            std::thread::yield_now();
        }
        let parent = worker_heap.globals.get(root) as *mut u8;
        let access = unsafe { worker_heap.object_access(parent) };
        let slot = unsafe { &*access.as_ptr().add(Full::SIZE + 8).cast::<AtomicU64>() };
        let hidden = slot.load(Ordering::SeqCst);
        for _ in 0..65_536 {
            unsafe {
                mt.write_barrier(hidden);
            }
            slot.store(0, Ordering::SeqCst);
            unsafe {
                mt.write_barrier(0);
            }
            slot.store(hidden, Ordering::SeqCst);
        }
        unsafe {
            mt.write_barrier(hidden);
        }
        slot.store(0, Ordering::SeqCst);
        drop(access);
        let stats = worker_heap.satb_queue.stats();
        if let Some(path) = std::env::var_os("GCR_SATB_PRESSURE_EVIDENCE") {
            std::fs::write(path, format!("{stats:?}\n")).unwrap();
        }
        RELEASE.store(true, Ordering::Release);
        while worker_heap.barriers_active() {
            unsafe {
                mt.safepoint();
            }
            std::thread::yield_now();
        }
        stats
    });
    started.recv().unwrap();
    unsafe {
        heap.concurrent_collect::<PausedIdentity>();
    }
    let stats = worker.join().unwrap();
    let pending = stats.pending_values;
    HEAP.store(std::ptr::null_mut(), Ordering::Release);
    assert_eq!(
        pending, 1,
        "duplicate barrier logs grow with overwrite count"
    );
    assert_eq!(stats.unique_values, 1);
    assert_eq!(stats.peak_pending_values, 1);
    assert_eq!(stats.pending_chunks, 1);
    assert_eq!(stats.pending_capacity_bytes, 256 * 8);
    assert_eq!(stats.bitmap_bytes, (1 << 20) / 64);
    assert_eq!(
        heap.from_used(),
        3 * info.allocation_size(0),
        "SATB-only referent lost"
    );
    assert!(heap.satb_queue.is_empty());
    unsafe {
        heap.stw_collect::<IdentityPtrPolicy>();
    }
    assert_eq!(
        heap.from_used(),
        2 * info.allocation_size(0),
        "floating garbage retained beyond cycle"
    );
}
