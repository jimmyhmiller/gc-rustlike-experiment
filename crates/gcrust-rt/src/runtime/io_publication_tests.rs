use super::*;
use crate::gc::{Full, IdentityPtrPolicy, ObjHeader, PtrPolicy, TypeInfo};
use crate::runtime::{RuntimeContext, ai_slot_bits64_load};
use std::sync::atomic::{AtomicBool, AtomicPtr};
use std::sync::{Arc, OnceLock};

static HEAP: AtomicPtr<crate::gc::Heap> = AtomicPtr::new(std::ptr::null_mut());
static ENTRY_READY: OnceLock<std::sync::mpsc::Sender<()>> = OnceLock::new();

/// Test-only rendezvous after publication to scratch roots, before entry stores.
/// It does not park or poll: production's loop must service the request itself.
pub(super) unsafe fn before_entries(thread: *mut Thread) {
    let heap = HEAP.load(Ordering::Acquire);
    if !heap.is_null() && heap == unsafe { (*thread).heap } {
        ENTRY_READY.get().unwrap().send(()).unwrap();
        while !unsafe { &*heap }.gc_requested() {
            std::thread::yield_now();
        }
    }
}

#[test]
fn collector_host_response_construction_polls_and_preserves_entries() {
    const NAME: &str = "runtime::io::publication_tests::collector_host_response_construction_polls_and_preserves_entries";
    const HELPER: &str = "GCR_IO_PUBLICATION_HELPER";
    if std::env::var_os(HELPER).is_none() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env(HELPER, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!(
                    "I/O construction failed to poll: {:?}",
                    child.wait_with_output().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    const ENTRIES: usize = 32768;
    static PARTIAL: AtomicBool = AtomicBool::new(false);
    struct ObservePartial;
    impl PtrPolicy for ObservePartial {
        fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
            let pointer = IdentityPtrPolicy::try_decode_ptr(bits)?;
            let heap = unsafe { &*HEAP.load(Ordering::Acquire) };
            // Only inspect an as-yet-unforwarded array while the world is
            // stopped. The scratch root exposes construction before completion.
            if heap.gc_requested()
                && heap.from_space_contains(pointer)
                && unsafe { heap.obj_type_id(pointer) } == 1
                && unsafe { pointer.add(Full::SIZE).cast::<u64>().read() } == ENTRIES as u64
            {
                let last = unsafe {
                    &*pointer
                        .add(STR_DATA_OFF + (ENTRIES - 1) * 8)
                        .cast::<AtomicU64>()
                };
                if last.load(Ordering::SeqCst) == 0 {
                    PARTIAL.store(true, Ordering::Release);
                }
            }
            Some(pointer)
        }
        fn encode_ptr(pointer: *mut u8) -> u64 {
            IdentityPtrPolicy::encode_ptr(pointer)
        }
    }
    let string = TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0);
    let array = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_varlen_values(0);
    let response = TypeInfo::for_header(Full::SIZE)
        .with_type_id(2)
        .with_fields(2)
        .with_raw_bytes(16);
    let mut runtime = RuntimeContext::new(1 << 24, vec![string, array, response]);
    let expected: Vec<_> = (0..ENTRIES)
        .map(|index| format!("entry-{index}-λ"))
        .collect();
    runtime.set_arguments(expected.iter().map(std::ffi::OsString::from).collect());
    let thread = runtime.thread_ptr();
    let execute = || {
        let heap = runtime.heap().clone();
        let empty = unsafe { alloc_string_from_text(thread, 0, "") };
        let empty_root = unsafe { heap.globals.add(empty as u64) };
        HEAP.store(Arc::as_ptr(&heap) as *mut _, Ordering::Release);
        let collector_heap = heap.clone();
        let (ready, waiting) = std::sync::mpsc::channel();
        let (entry_ready, entry_waiting) = std::sync::mpsc::channel();
        ENTRY_READY.set(entry_ready).unwrap();
        let collector = std::thread::spawn(move || {
            ready.send(()).unwrap();
            entry_waiting.recv().unwrap();
            unsafe { collector_heap.concurrent_collect::<ObservePartial>() };
        });
        waiting.recv().unwrap();
        let empty = heap.globals.get(empty_root) as *const u8;
        let result = unsafe { ai_host_call(thread, ARGS, 2, 0, 1, empty, empty, 0) };
        let root = unsafe { heap.globals.add(result as u64) };
        unsafe { blocking_region(thread, || collector.join().unwrap()) };
        HEAP.store(std::ptr::null_mut(), Ordering::Release);
        assert!(
            PARTIAL.load(Ordering::Acquire),
            "collection never saw a partial rooted array"
        );
        assert_eq!(heap.collections(), 1);
        let result = heap.globals.get(root) as *mut u8;
        assert_eq!(unsafe { ai_slot_bits64_load(thread, result, 32) }, 0);
        let array = unsafe { ai_slot_bits64_load(thread, result, 24) } as *mut u8;
        assert_eq!(
            unsafe { array.add(16).cast::<u64>().read() },
            ENTRIES as u64
        );
        for (index, text) in expected.iter().enumerate() {
            let entry =
                unsafe { ai_slot_bits64_load(thread, array, STR_DATA_OFF + index * 8) } as *const u8;
            assert_eq!(
                unsafe { str_bytes(entry) },
                text.as_bytes(),
                "entry {index}"
            );
        }
    };
    unsafe { crate::runtime::with_current_thread(thread, execute) }
}
