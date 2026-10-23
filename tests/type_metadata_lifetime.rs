//! Observe actual deallocation of dynamic layout offsets after JIT-style and
//! native-entry runtime lifetimes, rather than inferring ownership from RSS.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use gcrust::gc::{Full, ObjHeader, TypeInfo};
use gcrust::runtime::{AotLayout, RuntimeContext, Thread, gcr_runtime_main_v3};
// Each integration crate uses only a subset of these shared fixture helpers.
#[allow(dead_code)]
mod support;

struct ObservedAllocator;
static WATCH: AtomicUsize = AtomicUsize::new(0);
static FREED: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for ObservedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if WATCH
            .compare_exchange(pointer as usize, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            FREED.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.dealloc(pointer, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: ObservedAllocator = ObservedAllocator;

fn watch(info: &TypeInfo) {
    assert!(!info.interior_ptrs.is_empty());
    assert_eq!(WATCH.load(Ordering::SeqCst), 0);
    FREED.store(0, Ordering::SeqCst);
    WATCH.store(info.interior_ptrs.as_ptr() as usize, Ordering::SeqCst);
}
fn assert_released() {
    assert_eq!(FREED.load(Ordering::SeqCst), 1, "offset allocation leaked");
    assert_eq!(WATCH.load(Ordering::SeqCst), 0);
}

extern "C" fn native_entry(thread: *mut Thread) -> i64 {
    let heap = unsafe { &*(*thread).heap };
    let info = heap.type_info_by_id(0);
    assert_eq!(info.interior_ptrs.as_ref(), &[24]);
    watch(info);
    42
}

#[test]
fn dynamic_layout_offsets_are_freed_after_repeated_executions() {
    // Inspects heap layout: every struct a heap object as written.
    gcrust::compile::set_optimize(false);
    const CHILD: &str = "GCR_METADATA_LIFETIME_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "dynamic_layout_offsets_are_freed_after_repeated_executions",
            ])
            .env(CHILD, "1")
            .env("GCR_NURSERY_MB", "1")
            .env("GCR_TENURED_MB", "4");
        let output = support::run_with_timeout(&mut child, 120);
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let source = r#"
        #[value] struct Inline { text: String, number: i64 }
        struct Holder { value: Inline }
        fn main() -> i64 {
            let holder = Holder { value: Inline { text: "owned", number: 42 } };
            holder.value.number
        }
    "#;
    let (module, _) = gcrust::compile::parse_with_prelude(source).unwrap();
    let resolved = gcrust::resolve::resolve_module(module).unwrap();
    let program = gcrust::lower::lower_program(&resolved.globals).unwrap();
    for _ in 0..32 {
        let table = gcrust::codegen::layouts_to_type_infos(&program);
        watch(
            table
                .iter()
                .find(|info| !info.interior_ptrs.is_empty())
                .unwrap(),
        );
        let runtime = RuntimeContext::new_generational(4096, 16384, table);
        assert_eq!(
            FREED.load(Ordering::SeqCst),
            0,
            "freed before execution retired"
        );
        let retained_heap = runtime.heap().clone();
        drop(runtime);
        assert_eq!(FREED.load(Ordering::SeqCst), 0, "heap still owns offsets");
        drop(retained_heap);
        assert_released();
    }

    let layouts = [AotLayout {
        ptr_fields: 0,
        raw_bytes: 16,
        varlen: 0,
        _pad: [0; 3],
    }];
    let metadata = gcrust::gc::reflect::encode(&[], &[], &[vec![24]], &[]).unwrap();
    for _ in 0..32 {
        assert_eq!(
            unsafe {
                gcr_runtime_main_v3(
                    layouts.as_ptr(),
                    layouts.len(),
                    metadata.as_ptr(),
                    metadata.len(),
                    native_entry,
                )
            },
            42
        );
        assert_released();
    }

    // Static and owned descriptors describe the same shape. A clone owns its
    // own offsets, and retiring it cannot release the original allocation.
    static OFFSETS: [u16; 1] = [24];
    static STATIC: TypeInfo =
        TypeInfo::for_header_with_interior_ptrs(Full::SIZE, &OFFSETS).with_raw_bytes(16);
    let owned = TypeInfo::for_header(Full::SIZE)
        .with_raw_bytes(16)
        .with_owned_interior_ptrs(vec![24]);
    assert_eq!(owned, STATIC);
    watch(&owned);
    let cloned = owned.clone();
    assert_ne!(owned.interior_ptrs.as_ptr(), cloned.interior_ptrs.as_ptr());
    drop(cloned);
    assert_eq!(FREED.load(Ordering::SeqCst), 0);
    drop(owned);
    assert_released();
}
