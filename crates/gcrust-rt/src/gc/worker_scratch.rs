//! Bounded, thread-owned evacuation metadata. No copy claims live here.
//! Jobs are synchronous. Nested jobs and TLS teardown use uncached storage.
//! Every lease clears logical
//! contents before reuse, including during unwinding. Storage dies with the OS
//! thread; no heap, root source, or managed reference owns this cache.
use super::{LOCAL_BUFFER_CAPACITY, MAX_WORK_BATCH, Work};
use std::cell::RefCell;
#[path = "scratch_allocator.rs"]
mod accounting;
pub(super) use accounting::ScratchAllocator;
pub(super) type Buffer<T> = allocator_api2::vec::Vec<T, ScratchAllocator>;
pub use accounting::{CollectorScratchStats, collector_scratch_stats};


pub(super) struct Scratch {
    pub(super) batch: Buffer<Work>,
    pub(super) local: Buffer<Work>,
    pub(super) discovered: Buffer<Work>,
    pub(super) layouts: Buffer<(usize, usize)>,
    pub(super) addresses: Buffer<*mut u8>,
}
impl Default for Scratch { fn default() -> Self { Self::new() } }
impl Scratch {
    const fn new() -> Self {
        Self { batch: Buffer::new_in(ScratchAllocator), local: Buffer::new_in(ScratchAllocator), discovered: Buffer::new_in(ScratchAllocator), layouts: Buffer::new_in(ScratchAllocator), addresses: Buffer::new_in(ScratchAllocator) }
    }
    pub(super) fn prepare_batch(&mut self) {
        debug_assert!(self.batch.is_empty());
        self.batch.reserve(MAX_WORK_BATCH);
    }

    pub(super) fn prepare_copy(&mut self) {
        debug_assert!(
            self.discovered.is_empty() && self.layouts.is_empty() && self.addresses.is_empty()
        );
        debug_assert!(self.local.is_empty());
        self.local.reserve(MAX_WORK_BATCH);
        self.discovered.reserve(LOCAL_BUFFER_CAPACITY);
        self.layouts.reserve(LOCAL_BUFFER_CAPACITY);
        self.addresses.reserve(LOCAL_BUFFER_CAPACITY);
    }

    fn clear(&mut self) {
        self.batch.clear();
        self.local.clear();
        self.discovered.clear();
        self.layouts.clear();
        self.addresses.clear();
    }
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = const { RefCell::new(Scratch::new()) };
}
struct Lease<'a>(&'a mut Scratch);
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.0.clear();
    }
}

pub(super) fn with<R>(body: impl FnOnce(&mut Scratch) -> R) -> R {
    let mut body = Some(body);
    if let Ok(Some(result)) = SCRATCH.try_with(|scratch| {
        let Ok(mut scratch) = scratch.try_borrow_mut() else {
            return None;
        };
        let lease = Lease(&mut scratch);
        Some(body.take().unwrap()(lease.0))
    }) {
        return result;
    }
    // An embedding may collect a different heap recursively, or from a native
    // TLS destructor after this cache has been destroyed. Keep the outer job's
    // buffers isolated and do not resurrect destroyed thread-local storage.
    let mut scratch = Scratch::default();
    let lease = Lease(&mut scratch);
    body.take().unwrap()(lease.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::allocation_probe;
    use std::sync::{Arc, Barrier};

    fn collect_leaf(full: bool, tag: u64) {
        use crate::gc::{Compact, Full, Heap, IdentityPtrPolicy, ObjHeader, RootSet, TypeInfo};
        let info =
            TypeInfo::for_header(if full { Full::SIZE } else { Compact::SIZE }).with_raw_bytes(8);
        let heap = if full {
            Heap::new::<Full>(1 << 20, vec![info.clone()])
        } else {
            Heap::new::<Compact>(1 << 20, vec![info.clone()])
        };
        let object = if full {
            unsafe { heap.alloc_obj::<Full>(&info, 0) }
        } else {
            unsafe { heap.alloc_obj::<Compact>(&info, 0) }
        };
        unsafe {
            object.add(info.raw_data_offset()).cast::<u64>().write(tag);
        }
        let mut roots = RootSet::new();
        for _ in 0..17 {
            roots.add(object as u64);
        }
        for _ in 0..2 {
            unsafe {
                heap.collect::<IdentityPtrPolicy>(&[&roots, &roots]);
            }
            assert_eq!(heap.from_used(), info.allocation_size(0));
            for index in 0..17 {
                let object = roots.get(index) as *const u8;
                assert_eq!(roots.get(index), roots.get(0));
                assert_eq!(
                    unsafe { object.add(info.raw_data_offset()).cast::<u64>().read() },
                    tag
                );
            }
        }
    }

    #[test]
    fn collector_worker_scratch_collections_reuse_between_heaps_and_headers() {
        for tag in 0..8 {
            collect_leaf(tag % 2 == 0, tag);
        }
    }

    #[test]
    fn collector_worker_scratch_nested_pointer_policy_collection() {
        use crate::gc::{Full, Heap, IdentityPtrPolicy, ObjHeader, PtrPolicy, RootSet, TypeInfo};
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COMPLETED: AtomicUsize = AtomicUsize::new(0);
        struct Nested;
        impl PtrPolicy for Nested {
            fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
                IdentityPtrPolicy::try_decode_ptr(bits)
            }
            fn encode_ptr(pointer: *mut u8) -> u64 {
                // Encoding occurs inside the active worker's scratch lease.
                // The independent inner heap must use local fallback storage.
                assert!(SCRATCH.with(|scratch| scratch.try_borrow_mut().is_err()));
                collect_leaf(false, 77);
                COMPLETED.fetch_add(1, Ordering::Relaxed);
                IdentityPtrPolicy::encode_ptr(pointer)
            }
        }
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let heap = Heap::new::<Full>(1 << 20, vec![info.clone()]);
        let object = unsafe { heap.alloc_obj::<Full>(&info, 0) };
        unsafe {
            object.add(info.raw_data_offset()).cast::<u64>().write(42);
        }
        let mut roots = RootSet::new();
        roots.add(object as u64);
        unsafe {
            heap.collect::<Nested>(&[&roots]);
        }
        let object = roots.get(0) as *const u8;
        assert_eq!(
            unsafe { object.add(info.raw_data_offset()).cast::<u64>().read() },
            42
        );
        assert!(COMPLETED.load(Ordering::Relaxed) > 0);
    }

    fn fill(scratch: &mut Scratch) {
        assert!(
            scratch.batch.is_empty()
                && scratch.local.is_empty()
                && scratch.discovered.is_empty()
                && scratch.layouts.is_empty()
                && scratch.addresses.is_empty()
        );
        scratch.prepare_batch();
        scratch.prepare_copy();
        scratch.batch.extend((0..MAX_WORK_BATCH).map(Work::Slot));
        scratch.local.extend((0..MAX_WORK_BATCH).map(Work::Object));
        scratch
            .discovered
            .extend((0..LOCAL_BUFFER_CAPACITY).map(Work::Object));
        scratch
            .layouts
            .extend((0..LOCAL_BUFFER_CAPACITY).map(|_| (8, 8)));
        scratch
            .addresses
            .resize(LOCAL_BUFFER_CAPACITY, std::ptr::null_mut());
    }

    #[test]
    fn collector_worker_scratch_reuses_bounded_storage_without_allocator_traffic() {
        std::thread::spawn(|| {
            with(|_| ()); // initialize TLS before observing buffer requests
            allocation_probe::start();
            with(fill);
            let cold = allocation_probe::finish_bytes();
            let bytes = 2 * MAX_WORK_BATCH * std::mem::size_of::<Work>()
                + LOCAL_BUFFER_CAPACITY
                    * (std::mem::size_of::<Work>()
                        + std::mem::size_of::<(usize, usize)>()
                        + std::mem::size_of::<*mut u8>());
            assert_eq!(cold, (5, 0, bytes, 0));
            allocation_probe::start();
            for _ in 0..64 {
                with(fill);
            }
            let warm = allocation_probe::finish_bytes();
            assert_eq!(warm, (0, 0, 0, 0));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn collector_worker_scratch_idle_leases_reserve_only_batch() {
        std::thread::spawn(|| {
            with(|_| ());
            allocation_probe::start();
            with(|scratch| scratch.prepare_batch());
            let cold = allocation_probe::finish_bytes();
            assert_eq!(
                cold,
                (1, 0, MAX_WORK_BATCH * std::mem::size_of::<Work>(), 0)
            );
            allocation_probe::start();
            for _ in 0..64 {
                with(|scratch| scratch.prepare_batch());
            }
            let warm = allocation_probe::finish_bytes();
            assert_eq!(warm, (0, 0, 0, 0));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn collector_worker_scratch_clears_on_unwind() {
        assert!(
            std::panic::catch_unwind(|| with(|scratch| {
                fill(scratch);
                panic!("injected scratch job failure");
            }))
            .is_err()
        );
        with(fill); // every logical vector must be empty after the failed lease
    }

    #[test]
    fn collector_worker_scratch_nested_jobs_keep_outer_storage() {
        with(|outer| {
            fill(outer);
            let batch = outer.batch.as_ptr();
            with(|inner| {
                fill(inner);
                assert_ne!(batch, inner.batch.as_ptr());
            });
            assert_eq!(outer.batch.len(), MAX_WORK_BATCH);
            assert_eq!(outer.local.len(), MAX_WORK_BATCH);
            assert_eq!(outer.discovered.len(), LOCAL_BUFFER_CAPACITY);
            assert_eq!(outer.layouts.len(), LOCAL_BUFFER_CAPACITY);
            assert_eq!(outer.addresses.len(), LOCAL_BUFFER_CAPACITY);
        });
        with(fill);
    }

    #[test]
    fn collector_worker_scratch_supports_collection_during_tls_teardown() {
        struct Exit(std::sync::mpsc::Sender<bool>);
        impl Drop for Exit {
            fn drop(&mut self) {
                let destroyed = SCRATCH.try_with(|_| ()).is_err();
                with(fill);
                collect_leaf(true, 99);
                self.0.send(destroyed).unwrap();
            }
        }
        thread_local! {
            static EXIT: RefCell<Option<Exit>> = const { RefCell::new(None) };
        }
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Initialize the exit hook first so the cache destructor precedes
            // it. The exit hook must then allocate and release local buffers.
            EXIT.with(|exit| *exit.borrow_mut() = Some(Exit(send)));
            with(fill);
        })
        .join()
        .unwrap();
        assert!(
            receive
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
        );
    }

    #[test]
    fn collector_worker_scratch_is_thread_local() {
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|id| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    with(|scratch| {
                        scratch.prepare_batch();
                        scratch.batch.push(Work::Slot(id));
                        barrier.wait();
                        assert!(
                            matches!(scratch.batch.as_slice(), [Work::Slot(value)] if *value == id)
                        );
                    });
                    with(|scratch| assert!(scratch.batch.is_empty()));
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }
}

#[cfg(test)]
pub(super) fn test_buffer<T>(values: impl IntoIterator<Item=T>) -> Buffer<T> {
    let mut output = Buffer::new_in(ScratchAllocator);
    output.extend(values);
    output
}

#[cfg(test)]
mod accounting_tests {
    use super::*;
    use crate::runtime::allocation_probe;
    use std::sync::{Arc, Barrier};

    // Process-wide totals cannot be asserted against unrelated concurrent test
    // jobs. Each native regression runs in a fresh exact-filtered child. Miri
    // invokes one named test per process and cannot spawn an executable.
    fn child(full_name: &str) -> bool {
        #[cfg(miri)]
        { let _ = full_name; false }
        #[cfg(not(miri))]
        {
            let name = full_name.strip_prefix("gcrust_rt::").unwrap();
            if std::env::var("GCR_SCRATCH_TEST_CHILD").as_deref() == Ok(name) { return false; }
            let mut process = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--test-threads=1"])
                .env("GCR_SCRATCH_TEST_CHILD", name)
                .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
                .spawn().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while process.try_wait().unwrap().is_none() {
                if std::time::Instant::now() >= deadline {
                    process.kill().unwrap();
                    let output = process.wait_with_output().unwrap();
                    panic!("scratch regression timed out: {name}\n{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let output = process.wait_with_output().unwrap();
            assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
            true
        }
    }
    fn retained_bytes(scratch: &Scratch) -> usize {
        (scratch.batch.capacity() + scratch.local.capacity() + scratch.discovered.capacity()) * std::mem::size_of::<Work>()
            + scratch.layouts.capacity() * std::mem::size_of::<(usize, usize)>()
            + scratch.addresses.capacity() * std::mem::size_of::<*mut u8>()
    }

    #[test]
    fn collector_thread_scratch_actual_backing_and_empty_storage() {
        if child(concat!(module_path!(), "::collector_thread_scratch_actual_backing_and_empty_storage")) { return; }
        assert_eq!(collector_scratch_stats().requested_bytes, 0);
        allocation_probe::start();
        let mut scratch = Scratch::new();
        let mut zero = Buffer::new_in(ScratchAllocator);
        zero.resize(100, ());
        assert_eq!(collector_scratch_stats().requested_bytes, 0);
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        allocation_probe::start();
        scratch.prepare_batch(); scratch.prepare_copy();
        let bytes = retained_bytes(&scratch);
        assert_eq!(allocation_probe::finish_bytes(), (5, 0, bytes, 0));
        assert_eq!(collector_scratch_stats(), CollectorScratchStats { requested_bytes: bytes, peak_requested_bytes: bytes });
        allocation_probe::start(); drop(scratch);
        assert_eq!(allocation_probe::finish_bytes(), (0, 5, 0, bytes));
        assert_eq!(collector_scratch_stats().requested_bytes, 0);
        assert_eq!(collector_scratch_stats().peak_requested_bytes, bytes);
    }

    #[test]
    fn collector_thread_scratch_nested_unwind_and_thread_exit() {
        if child(concat!(module_path!(), "::collector_thread_scratch_nested_unwind_and_thread_exit")) { return; }
        assert_eq!(collector_scratch_stats().requested_bytes, 0);
        let bytes = std::thread::spawn(|| {
            let mut bytes = 0;
            with(|outer| {
                outer.prepare_batch(); outer.prepare_copy();
                outer.batch.push(Work::Slot(7));
                bytes = retained_bytes(outer);
                assert_eq!(collector_scratch_stats().requested_bytes, bytes);
                let unwind = std::panic::catch_unwind(|| with(|inner| {
                    inner.prepare_batch(); inner.prepare_copy();
                    assert_eq!(retained_bytes(inner), bytes);
                    assert_eq!(collector_scratch_stats().requested_bytes, 2 * bytes);
                    panic!("nested scratch unwind");
                }));
                assert!(unwind.is_err());
                assert_eq!(collector_scratch_stats().requested_bytes, bytes);
                assert!(matches!(outer.batch.as_slice(), [Work::Slot(7)]));
            });
            allocation_probe::start();
            with(|scratch| {
                assert!(scratch.batch.is_empty());
                scratch.prepare_batch(); scratch.prepare_copy();
            });
            assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            assert_eq!(collector_scratch_stats().requested_bytes, bytes);
            bytes
        }).join().unwrap();
        assert_eq!(collector_scratch_stats(), CollectorScratchStats { requested_bytes: 0, peak_requested_bytes: 2 * bytes });
    }

    #[test]
    fn collector_thread_scratch_concurrent_owners_release_independently() {
        if child(concat!(module_path!(), "::collector_thread_scratch_concurrent_owners_release_independently")) { return; }
        assert_eq!(collector_scratch_stats().requested_bytes, 0);
        let ready = Arc::new(Barrier::new(3));
        let release = Arc::new(Barrier::new(3));
        let threads: Vec<_> = (0..2).map(|_| {
            let ready = ready.clone(); let release = release.clone();
            std::thread::spawn(move || with(|scratch| {
                scratch.prepare_batch(); scratch.prepare_copy();
                let bytes = retained_bytes(scratch);
                ready.wait(); release.wait(); bytes
            }))
        }).collect();
        ready.wait();
        let observed = collector_scratch_stats();
        assert!(observed.requested_bytes > 0);
        assert_eq!(observed.requested_bytes, observed.peak_requested_bytes);
        release.wait();
        let bytes: usize = threads.into_iter().map(|thread| thread.join().unwrap()).sum();
        assert_eq!(observed.requested_bytes, bytes);
        assert_eq!(collector_scratch_stats(), CollectorScratchStats { requested_bytes: 0, peak_requested_bytes: bytes });
    }
    #[test]
    fn collector_thread_scratch_growth_and_iterator_own_actual_backing() {
        if child(concat!(module_path!(), "::collector_thread_scratch_growth_and_iterator_own_actual_backing")) { return; }
        assert_eq!(collector_scratch_stats().requested_bytes, 0);
        let mut values = Buffer::with_capacity_in(1, ScratchAllocator);
        values.push(17u64);
        let old = values.capacity() * 8;
        allocation_probe::start();
        values.reserve_exact(128);
        let bytes = values.capacity() * 8;
        assert_eq!(allocation_probe::finish_bytes(), (1, 1, bytes, old));
        assert_eq!(collector_scratch_stats(), CollectorScratchStats { requested_bytes: bytes, peak_requested_bytes: old + bytes });
        values.extend(18..34);
        let mut values = values.into_iter();
        assert_eq!(values.next(), Some(17));
        assert_eq!(values.len(), 16);
        assert_eq!(collector_scratch_stats().requested_bytes, bytes);
        assert!(values.by_ref().eq(18..34));
        assert_eq!(collector_scratch_stats().requested_bytes, bytes);
        allocation_probe::start(); drop(values);
        assert_eq!(allocation_probe::finish_bytes(), (0, 1, 0, bytes));
        assert_eq!(collector_scratch_stats().requested_bytes, 0);
    }

    #[test]
    fn collector_thread_scratch_metrics_keep_process_and_heap_scopes_separate() {
        if child(concat!(module_path!(), "::collector_thread_scratch_metrics_keep_process_and_heap_scopes_separate")) { return; }
        use crate::gc::{Full, Heap};
        assert_eq!(collector_scratch_stats().requested_bytes, 0);
        let mut scratch = Scratch::new(); scratch.prepare_batch();
        let bytes = retained_bytes(&scratch);
        let mut first = Some(Heap::new::<Full>(1 << 16, vec![]));
        let second = Heap::new::<Full>(1 << 16, vec![]);
        for heap in [first.as_ref().unwrap(), &second] {
            let native_before = heap.native_registry_bytes();
            let metrics = heap.metrics_json();
            assert!(metrics.contains(&format!("\"process_collector_scratch_bytes\":{bytes},\"process_collector_scratch_peak_bytes\":{bytes}}}")), "{metrics}");
            assert_eq!(heap.native_registry_bytes(), native_before, "process cache must not enter an arbitrary heap's ledger");
        }
        drop(first.take());
        assert_eq!(collector_scratch_stats().requested_bytes, bytes);
        drop(scratch);
        let metrics = second.metrics_json();
        assert!(metrics.contains(&format!("\"process_collector_scratch_bytes\":0,\"process_collector_scratch_peak_bytes\":{bytes}}}")), "{metrics}");
        assert_eq!(collector_scratch_stats().requested_bytes, 0);
    }

}
