//! Current-thread Rust allocator probe, enabled only in the runtime test binary.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
thread_local! {
    static COUNTS: Cell<Option<Counts>> = const { Cell::new(None) };
}
#[derive(Clone, Copy, Default)]
struct Counts {
    allocations: usize,
    deallocations: usize,
    allocated_bytes: usize,
    deallocated_bytes: usize,
}
struct ProbeAllocator;
fn record(allocation: bool, allocated: usize, deallocated: usize) {
    let _ = COUNTS.try_with(|counts| {
        if let Some(mut value) = counts.get() {
            value.allocations = value.allocations.saturating_add(usize::from(allocation));
            value.deallocations = value.deallocations.saturating_add(usize::from(!allocation));
            value.allocated_bytes = value.allocated_bytes.saturating_add(allocated);
            value.deallocated_bytes = value.deallocated_bytes.saturating_add(deallocated);
            counts.set(Some(value));
        }
    });
}
unsafe impl GlobalAlloc for ProbeAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(true, layout.size(), 0);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(true, layout.size(), 0);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // Preserve call-count semantics: realloc is one allocation call. Its
        // byte traffic replaces the previous layout with the requested size.
        record(true, size, layout.size());
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(false, 0, layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: ProbeAllocator = ProbeAllocator;
pub(crate) fn start() {
    COUNTS.with(|counts| assert!(counts.replace(Some(Counts::default())).is_none()));
}
pub(crate) fn finish() -> (usize, usize) {
    let value = COUNTS.with(|counts| counts.replace(None).unwrap());
    (value.allocations, value.deallocations)
}
/// Current-thread Rust allocator traffic, not process RSS or OS allocations.
/// Realloc contributes its new size to allocated bytes and old size to freed
/// bytes. The net is retained bytes only if the measured scope owns every free.
pub(crate) fn finish_bytes() -> (usize, usize, usize, usize) {
    let value = COUNTS.with(|counts| counts.replace(None).unwrap());
    (
        value.allocations,
        value.deallocations,
        value.allocated_bytes,
        value.deallocated_bytes,
    )
}
