//! Scoped native FFI ownership, accounting and collector-aware allocation.
use super::{Thread, blocking_region, native_memory, string_copy};
use crate::gc::IdentityPtrPolicy;
use std::alloc::Layout;

// The payload keeps its requested allocation shape. A separate opaque token
// owns the charge and geometry without shifting power-of-two allocator bins.
struct Buffer {
    native: *mut u8,
    layout: Layout,
    charge: native_memory::Charge,
}
fn geometry(bytes: i64) -> (Layout, usize) {
    let size = usize::try_from(bytes)
        .expect("invalid foreign buffer size")
        .max(1);
    let payload = Layout::from_size_align(size, 8).expect("foreign buffer exceeds address space");
    let charged = payload
        .size()
        .checked_add(Layout::new::<Buffer>().size())
        .expect("foreign buffer reservation exceeds address space");
    (payload, charged)
}
pub(super) unsafe fn allocate(thread: *mut Thread, bytes: i64) -> *mut u8 {
    let (layout, charged) = geometry(bytes);
    let heap = unsafe { &*(*thread).heap };
    let state = unsafe { &*(*thread).dyna_thread };
    unsafe {
        state.set_parked_jit_fp((*thread).top_frame as *const u8);
        heap.mutator_gc_for_native_allocations::<IdentityPtrPolicy>(state, charged);
        state.clear_parked_jit_fp();
    }
    let memory = &heap.native_memory;
    let charge = memory.charge(charged);
    #[cfg(test)]
    let heap_id = heap as *const crate::gc::Heap as usize;
    let body = move || unsafe {
        #[cfg(test)]
        tests::checkpoint(heap_id, false);
        let native = std::alloc::alloc_zeroed(layout);
        if native.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Box::into_raw(Box::new(Buffer {
            native,
            layout,
            charge,
        }))
        .cast()
    };
    if charged > string_copy::COPY_BYTES {
        unsafe { blocking_region(thread, body) }
    } else {
        body()
    }
}
/// Noncollecting access to a live native ownership token; no managed data.
pub(super) unsafe fn data(token: *const u8) -> *mut u8 {
    unsafe { (*token.cast::<Buffer>()).native }
}
pub(super) unsafe fn release(thread: *mut Thread, token: *mut u8) {
    // Native token metadata is stable; no managed pointer enters the body.
    let charged =
        unsafe { (*token.cast::<Buffer>()).layout.size() } + Layout::new::<Buffer>().size();
    #[cfg(test)]
    let heap_id = unsafe { (*thread).heap } as usize;
    let body = move || unsafe {
        let Buffer {
            native,
            layout,
            charge,
        } = token.cast::<Buffer>().read();
        #[cfg(test)]
        tests::checkpoint(heap_id, true);
        std::alloc::dealloc(native, layout);
        std::alloc::dealloc(token, Layout::new::<Buffer>());
        drop(charge);
    };
    if charged > string_copy::COPY_BYTES {
        unsafe { blocking_region(thread, body) }
    } else {
        body()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        gc::{Full, ObjHeader, TypeInfo},
        runtime::{self, RuntimeContext},
    };
    use std::{
        process::{Command, Stdio},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc::Sender,
        },
        time::{Duration, Instant},
    };
    struct Hook {
        heap: usize,
        release: bool,
        ready: Sender<()>,
        completed: Arc<AtomicBool>,
    }
    static HOOK: Mutex<Option<Hook>> = Mutex::new(None);
    pub(super) fn checkpoint(heap: usize, release: bool) {
        let hook = {
            let mut slot = HOOK.lock().unwrap();
            if slot
                .as_ref()
                .is_some_and(|h| h.heap == heap && h.release == release)
            {
                slot.take()
            } else {
                None
            }
        };
        if let Some(hook) = hook {
            hook.ready.send(()).unwrap();
            while !hook.completed.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
    }
    fn child(name: &str) -> bool {
        if std::env::var_os("GCR_FFI_BUFFER_CHILD").is_some() {
            return true;
        }
        for mode in ["0", "1"] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .env("GCR_FFI_BUFFER_CHILD", mode)
                .env("GCR_GC_VERIFY", "1")
                .env("GCRUST_POISON", "1")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    panic!(
                        "native buffer failed collector progress: {:?}",
                        child.wait_with_output().unwrap()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let result = child.wait_with_output().unwrap();
            assert!(result.status.success(), "{result:?}");
        }
        false
    }
    fn runtime(mode: bool) -> RuntimeContext {
        let types = vec![TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0)];
        if mode {
            RuntimeContext::new_generational(32768, 2 << 20, types)
        } else {
            RuntimeContext::new(2 << 20, types)
        }
    }
    #[test]
    fn collector_ffi_buffer_reference_layout_accounting() {
        for mode in [false, true] {
            let mut rt = runtime(mode);
            let heap = rt.heap().clone();
            rt.enter(|thread| unsafe {
                for size in [0, 1, 7, 8, 9, 32768, 32769, 65537] {
                    let (_, charged) = geometry(size);
                    let token = allocate(thread, size);
                    let native = data(token);
                    assert!(!native.is_null());
                    assert_eq!(native as usize % 8, 0);
                    assert_eq!(heap.native_buffer_bytes(), charged);
                    let bytes = std::slice::from_raw_parts_mut(native, size.max(1) as usize);
                    assert!(bytes.iter().all(|&b| b == 0));
                    bytes.fill(0xa5);
                    release(thread, token);
                    assert_eq!(heap.native_buffer_bytes(), 0);
                }
                assert!(heap.native_buffer_peak_bytes() >= geometry(65537).1);
                let old = runtime::ai_ffi_buffer_alloc_v1(65537);
                assert_eq!(
                    heap.native_buffer_bytes(),
                    0,
                    "legacy allocations have no ledger contract"
                );
                runtime::ai_ffi_buffer_free_v1(old, 65537);
                assert_eq!((&*(*thread).dyna_thread).scratch_mark(), 0);
            });
        }
    }
    #[test]
    fn collector_ffi_buffer_native_phases_allow_collection() {
        if !child("runtime::ffi_buffer::tests::collector_ffi_buffer_native_phases_allow_collection")
        {
            return;
        }
        let mut rt = runtime(std::env::var("GCR_FFI_BUFFER_CHILD").as_deref() == Ok("1"));
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            let mut native = std::ptr::null_mut();
            for releasing in [false, true] {
                let (ready, waiting) = std::sync::mpsc::channel();
                let completed = Arc::new(AtomicBool::new(false));
                *HOOK.lock().unwrap() = Some(Hook {
                    heap: Arc::as_ptr(&heap) as usize,
                    release: releasing,
                    ready,
                    completed: completed.clone(),
                });
                let other = heap.clone();
                let done = completed.clone();
                let before = heap.collections();
                let collector = std::thread::spawn(move || {
                    waiting.recv().unwrap();
                    other.stw_collect::<IdentityPtrPolicy>();
                    let observed = other.native_buffer_bytes();
                    done.store(true, Ordering::Release);
                    assert_eq!(
                        observed,
                        geometry(65537).1,
                        "in-flight backing must remain charged"
                    );
                });
                if releasing {
                    release(thread, native);
                } else {
                    native = allocate(thread, 65537);
                }
                assert!(completed.load(Ordering::Acquire) && heap.collections() > before);
                blocking_region(thread, || collector.join().unwrap());
                assert_eq!(
                    heap.native_buffer_bytes(),
                    if releasing { 0 } else { geometry(65537).1 }
                );
            }
            assert_eq!((&*(*thread).dyna_thread).scratch_mark(), 0);
        });
    }
    #[test]
    fn collector_ffi_buffer_pressure_keeps_live_storage_and_managed_roots() {
        for mode in [false, true] {
            let mut rt = runtime(mode);
            let heap = rt.heap().clone();
            rt.enter(|thread| unsafe {
                let roots = (&*(*thread).dyna_thread).scratch_scope();
                let owner = roots.push(runtime::alloc_string_from_text(
                    thread,
                    0,
                    "kept through native pressure",
                ));
                let first_token = allocate(thread, 600 << 10);
                let first = data(first_token);
                std::slice::from_raw_parts_mut(first, 600 << 10).fill(0xa5);
                let before = heap.native_pressure_collections();
                let second_token = allocate(thread, 600 << 10);
                let second = data(second_token);
                assert!(heap.native_pressure_collections() > before);
                assert_eq!(
                    runtime::str_bytes(roots.get(owner)),
                    b"kept through native pressure"
                );
                assert!(
                    std::slice::from_raw_parts(first, 600 << 10)
                        .iter()
                        .all(|&b| b == 0xa5)
                );
                assert!(
                    std::slice::from_raw_parts(second, 600 << 10)
                        .iter()
                        .all(|&b| b == 0)
                );
                assert_eq!(heap.native_buffer_bytes(), 2 * geometry(600 << 10).1);
                release(thread, first_token);
                assert_eq!(heap.native_buffer_bytes(), geometry(600 << 10).1);
                release(thread, second_token);
                assert_eq!(heap.native_buffer_bytes(), 0);
            });
        }
    }
    #[test]
    fn collector_ffi_buffer_legacy_and_small_paths_remain_noncollecting() {
        if !child(
            "runtime::ffi_buffer::tests::collector_ffi_buffer_legacy_and_small_paths_remain_noncollecting",
        ) {
            return;
        }
        let mut rt = runtime(std::env::var("GCR_FFI_BUFFER_CHILD").as_deref() == Ok("1"));
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            let before = heap.collections();
            let other = heap.clone();
            let collector = std::thread::spawn(move || other.stw_collect::<IdentityPtrPolicy>());
            while !heap.gc_requested() {
                std::thread::yield_now();
            }
            let old = runtime::ai_ffi_buffer_alloc_v1(65537);
            runtime::ai_ffi_buffer_free_v1(old, 65537);
            let small = allocate(thread, 8);
            release(thread, small);
            assert_eq!(heap.collections(), before);
            string_copy::poll(thread);
            blocking_region(thread, || collector.join().unwrap());
        });
    }
    #[test]
    fn collector_ffi_buffer_leaf_path_does_not_lock_channel_registry() {
        if !child(
            "runtime::ffi_buffer::tests::collector_ffi_buffer_leaf_path_does_not_lock_channel_registry",
        ) {
            return;
        }
        let mut rt = runtime(std::env::var("GCR_FFI_BUFFER_CHILD").as_deref() == Ok("1"));
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            let registry = heap.channels.lock().unwrap();
            let token = allocate(thread, 8);
            assert_eq!(heap.native_buffer_bytes(), geometry(8).1);
            assert!(heap.native_buffer_peak_bytes() >= heap.native_buffer_bytes());
            assert_eq!(heap.native_pressure_collections(), 0);
            assert_eq!(*data(token), 0);
            release(thread, token);
            assert_eq!(heap.native_buffer_bytes(), 0);
            drop(registry);
        });
    }
}
