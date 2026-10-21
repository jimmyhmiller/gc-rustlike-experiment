//! Collection cannot unwind into a heap whose relocation was only partly done.

/// Keep this boundary inside any guard that restores mutators. A failed moving
/// collection has no rollback log; unwinding those guards can either deadlock
/// waiting for its request flag or resume access to inconsistent object/root
/// locations. The original panic hook supplies diagnostics. Do not run another
/// formatter, acquire runtime locks or drop an arbitrary panic payload here.
pub(super) fn abort_on_collection_panic<T>(collect: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(collect)) {
        Ok(value) => value,
        Err(payload) => {
            std::mem::forget(payload);
            std::process::abort();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gc::{
        Compact, Full, Heap, IdentityPtrPolicy, ObjHeader, PtrPolicy, RootSource, SemiSpace,
        TypeInfo,
    };
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    const CHILD: &str = "GCR_COLLECTION_FAILURE_CHILD";

    pub(crate) fn expect_abort(test: &str, case: &str, workers: usize, diagnostic: &str) {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--include-ignored", "--nocapture"])
            .env(CHILD, case)
            .env("GCR_GC_WORKERS", workers.to_string())
            .env("GCR_GC_VERIFY", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!("collection failure hung ({case}/{workers}): {output:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("ThreadSanitizer:"),
            "sanitizer report in expected-fatal child {case}: {output:?}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(
                output.status.signal(),
                Some(6),
                "{case}/{workers}: {output:?}"
            );
        }
        #[cfg(not(unix))]
        assert!(
            !output.status.success() && output.status.code() != Some(88),
            "{output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(diagnostic),
            "{case}: {output:?}"
        );
    }

    pub(crate) fn is_child() -> bool {
        std::env::var_os(CHILD).is_some()
    }

    struct PanicRoots {
        visits: AtomicUsize,
        fail_at: usize,
        old_object: AtomicUsize,
    }
    // No root storage is enumerated or invalidated. A panic is permitted by
    // RootSource's lifetime/initialization contract and must fail the cycle.
    unsafe impl RootSource for PanicRoots {
        unsafe fn scan_roots(&self, _: &mut dyn FnMut(*mut u64)) {
            if self.visits.fetch_add(1, Ordering::Relaxed) == self.fail_at {
                if self.fail_at == 1 {
                    let old = self.old_object.load(Ordering::Relaxed) as *mut u8;
                    assert!(!old.is_null());
                    assert_ne!(
                        unsafe {
                            crate::gc::read_barrier_atomic(old, Full::HEAP_FORWARDING_OFFSET)
                        },
                        old,
                        "final-pause failure must follow actual relocation"
                    );
                }
                panic!("injected root callback failure");
            }
        }
    }

    #[test]
    fn collector_failures_abort_before_mutator_restoration() {
        let Ok(case) = std::env::var(CHILD) else {
            for workers in [1, 4] {
                for case in [
                    "direct",
                    "stw",
                    "major",
                    "extra",
                    "minor",
                    "concurrent",
                    "concurrent-final",
                    "legacy",
                ] {
                    expect_abort(
                        "gc::failure::tests::collector_failures_abort_before_mutator_restoration",
                        case,
                        workers,
                        "injected root callback failure",
                    );
                }
            }
            return;
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            let roots = PanicRoots {
                visits: AtomicUsize::new(0),
                old_object: AtomicUsize::new(0),
                fail_at: usize::from(case == "concurrent-final"),
            };
            if case == "legacy" {
                let mut gc = SemiSpace::new::<Compact>(4096);
                gc.collect::<IdentityPtrPolicy>(&[], &mut [&roots]);
                return;
            }
            let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
            let heap = Heap::new_generational::<Full>(4096, 4096, vec![info.clone()]);
            if case != "extra" && case != "direct" {
                heap.register_permanent_extra(&roots);
            }
            let (state, _) = heap.register_thread();
            let object = if case == "minor" {
                heap.alloc_nursery_obj::<Full>(&info, 0)
            } else {
                heap.alloc_obj::<Full>(&info, 0)
            };
            assert!(!object.is_null());
            heap.globals.add(object as u64);
            roots.old_object.store(object as usize, Ordering::Relaxed);
            match case.as_str() {
                "direct" => {
                    state.enter_blocked();
                    heap.collect::<IdentityPtrPolicy>(&[&roots]);
                }
                "stw" => {
                    state.enter_blocked();
                    heap.stw_collect::<IdentityPtrPolicy>();
                }
                "major" => heap.mutator_triggered_gc::<IdentityPtrPolicy>(&state),
                "extra" => {
                    heap.mutator_triggered_gc_with_extras::<IdentityPtrPolicy>(&state, &[&roots])
                }
                "minor" => heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state),
                "concurrent" | "concurrent-final" => {
                    state.enter_blocked();
                    heap.concurrent_collect::<IdentityPtrPolicy>();
                }
                _ => unreachable!(),
            }
        }));
        // Catching the panic or returning normally would expose a failed heap.
        std::process::exit(88);
    }

    static HELPER_FAILED: AtomicBool = AtomicBool::new(false);
    static HELPERS_ENTERED: AtomicUsize = AtomicUsize::new(0);
    struct WorkerPayload;
    impl Drop for WorkerPayload {
        fn drop(&mut self) {
            std::process::exit(88);
        }
    }
    struct WorkerFailure;
    impl PtrPolicy for WorkerFailure {
        fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
            if std::thread::current()
                .name()
                .is_some_and(|name| name.starts_with("gcr-gc-"))
            {
                if std::env::var(CHILD).unwrap() == "helper-payload" {
                    HELPERS_ENTERED.fetch_add(1, Ordering::AcqRel);
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while HELPERS_ENTERED.load(Ordering::Acquire) < 3 {
                        assert!(
                            Instant::now() < deadline,
                            "not all helpers entered decoding"
                        );
                        std::thread::yield_now();
                    }
                    HELPER_FAILED.store(true, Ordering::Release);
                    std::panic::panic_any(WorkerPayload);
                }
                HELPER_FAILED.store(true, Ordering::Release);
                panic!("injected collector helper failure");
            }
            // Keep a coordinator batch active until a helper really enters
            // decoding, so this regression cannot pass via coordinator failure.
            let deadline = Instant::now() + Duration::from_secs(5);
            while !HELPER_FAILED.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "helper never entered decoding");
                std::thread::yield_now();
            }
            IdentityPtrPolicy::try_decode_ptr(bits)
        }
        fn encode_ptr(ptr: *mut u8) -> u64 {
            IdentityPtrPolicy::encode_ptr(ptr)
        }
    }

    #[test]
    fn collector_worker_panics_abort_without_dropping_payloads() {
        if !is_child() {
            for case in ["helper", "helper-payload"] {
                expect_abort(
                    "gc::failure::tests::collector_worker_panics_abort_without_dropping_payloads",
                    case,
                    4,
                    "injected collector helper failure",
                );
            }
            return;
        }
        if std::env::var(CHILD).unwrap() == "helper-payload" {
            std::panic::set_hook(Box::new(|_| eprintln!("injected collector helper failure")));
        }
        let info = TypeInfo::for_header(Full::SIZE);
        let heap = Heap::new::<Full>(1 << 16, vec![info.clone()]);
        for _ in 0..256 {
            let object = unsafe { heap.alloc_obj::<Full>(&info, 0) };
            assert!(!object.is_null());
            unsafe {
                heap.globals.add(object as u64);
            }
        }
        let (state, _) = heap.register_thread();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            heap.mutator_triggered_gc::<WorkerFailure>(&state);
        }));
        std::process::exit(88);
    }

    #[test]
    fn collector_promotion_exhaustion_is_fatal() {
        if !is_child() {
            for workers in [1, 4] {
                expect_abort(
                    "gc::failure::tests::collector_promotion_exhaustion_is_fatal",
                    "promotion",
                    workers,
                    "to-space exhausted",
                );
            }
            return;
        }
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let heap = Heap::new_generational::<Full>(4096, 64, vec![info.clone()]);
        for _ in 0..16 {
            let object = unsafe { heap.alloc_nursery_obj::<Full>(&info, 0) };
            assert!(!object.is_null());
            unsafe {
                heap.globals.add(object as u64);
            }
        }
        let (state, _) = heap.register_thread();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state);
        }));
        std::process::exit(88);
    }

    #[test]
    fn collector_panic_payload_is_not_dropped() {
        if !is_child() {
            expect_abort(
                "gc::failure::tests::collector_panic_payload_is_not_dropped",
                "payload",
                1,
                "injected payload failure",
            );
            return;
        }
        struct Payload;
        impl Drop for Payload {
            fn drop(&mut self) {
                std::process::exit(88);
            }
        }
        std::panic::set_hook(Box::new(|_| eprintln!("injected payload failure")));
        abort_on_collection_panic(|| std::panic::panic_any(Payload));
        std::process::exit(88);
    }

    #[test]
    fn collector_successful_gc_during_unwind_preserves_original_panic() {
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let heap = Heap::new::<Full>(4096, vec![info.clone()]);
        let object = unsafe { heap.alloc_obj::<Full>(&info, 0) };
        let root = unsafe { heap.globals.add(object as u64) };
        struct CollectOnDrop<'a>(&'a Heap);
        impl Drop for CollectOnDrop<'_> {
            fn drop(&mut self) {
                unsafe {
                    self.0.collect::<IdentityPtrPolicy>(&[]);
                }
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _collect = CollectOnDrop(&heap);
            panic!("original caller panic");
        }));
        assert_eq!(
            result.unwrap_err().downcast_ref::<&str>(),
            Some(&"original caller panic")
        );
        assert_ne!(heap.globals.get(root), object as u64);
        assert_eq!(heap.collections(), 1);
    }
}
