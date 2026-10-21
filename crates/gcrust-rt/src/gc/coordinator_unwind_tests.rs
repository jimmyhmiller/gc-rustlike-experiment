use super::*;
use crate::gc::{Compact, Full, IdentityPtrPolicy};

fn check<H: ObjHeader>(mode: usize) {
    let info = TypeInfo::for_header(H::SIZE).with_raw_bytes(8);
    let heap = Heap::new_generational::<H>(1024, 8192, vec![info.clone()]);
    let (state, _) = heap.register_thread();
    let object = unsafe { heap.alloc_nursery_obj::<H>(&info, 0) };
    assert!(!object.is_null());
    unsafe {
        object.add(H::SIZE).cast::<u64>().write(42);
    }
    let root = unsafe { heap.globals.add(object as u64) };
    for _ in 0..2 {
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _pause = heap.pause_world();
            panic!("injected inspection unwind");
        }));
        assert!(unwind.is_err());
        assert!(!heap.gc_requested());
        assert!(!state.is_safely_at_safepoint());
        unsafe {
            match mode {
                0 => {
                    state.enter_blocked();
                    heap.collect::<IdentityPtrPolicy>(&[]);
                    state.exit_blocked(&heap);
                }
                1 => {
                    state.enter_blocked();
                    heap.stw_collect::<IdentityPtrPolicy>();
                    state.exit_blocked(&heap);
                }
                2 => heap.mutator_triggered_gc::<IdentityPtrPolicy>(&state),
                3 => heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state),
                4 => {
                    state.enter_blocked();
                    heap.concurrent_collect::<IdentityPtrPolicy>();
                    state.exit_blocked(&heap);
                }
                _ => unreachable!(),
            }
            let moved = heap.globals.get(root) as *const u8;
            assert_eq!(moved.add(H::SIZE).cast::<u64>().read(), 42);
        }
        // Registration, retirement and another inspection must also work.
        let (extra, _) = heap.register_thread();
        unsafe {
            heap.safe_deregister_thread(&extra);
        }
        let _pause = heap.pause_world();
    }
    unsafe {
        heap.globals.set(root, 0);
        heap.safe_deregister_thread(&state);
    }
}

#[test]
fn collector_inspection_unwind_preserves_all_coordinator_entry_points() {
    if std::env::var_os("GCR_COORDINATOR_UNWIND_CHILD").is_none() {
        use std::{
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gc::heap::coordinator_unwind_tests::collector_inspection_unwind_preserves_all_coordinator_entry_points", "--nocapture"])
            .env("GCR_COORDINATOR_UNWIND_CHILD", "1")
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!(
                    "coordinator recovery hung: {:?}",
                    child.wait_with_output().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    for mode in 0..5 {
        check::<Full>(mode);
        check::<Compact>(mode);
    }
}
