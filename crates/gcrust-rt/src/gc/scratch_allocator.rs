//! Process-wide accounting for OS-thread-owned collector scratch backing.
//!
//! The cache can serve several heaps, including nested jobs, and can outlive
//! every heap. Static accounting survives TLS teardown and needs no heap owner.
use allocator_api2::alloc::{AllocError, Allocator, Global};
use crate::runtime::allocation_counter::AllocationCounter;
use std::{alloc::Layout, ptr::NonNull};

static BACKING: AllocationCounter = AllocationCounter::new();

/// Requested Rust allocator bytes owned by collector scratch across all OS
/// threads in this process. This includes temporary nested/teardown fallback
/// buffers, and excludes managed spaces, allocator overhead and OS allocations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollectorScratchStats {
    pub requested_bytes: usize,
    pub peak_requested_bytes: usize,
}

/// Read the process-wide collector scratch ledger. Live and monotone peak are
/// individually atomic observations; concurrent allocation can occur between
/// the reads. The ledger is independent of each heap's native pressure budget.
pub fn collector_scratch_stats() -> CollectorScratchStats {
    CollectorScratchStats {
        requested_bytes: BACKING.bytes(),
        peak_requested_bytes: BACKING.peak(),
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ScratchAllocator;
impl ScratchAllocator {
    fn account(block: NonNull<[u8]>, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if !BACKING.try_charge(layout.size()) {
            // Unpublished backing must not survive an accounting overflow.
            unsafe { Global.deallocate(block.cast(), layout); }
            return Err(AllocError);
        }
        Ok(block)
    }
}
// SAFETY: Global owns every block and receives its original pointer/layout.
// The static counter only tracks the layout, never payloads or TLS state. The
// charge remains until physical free, including nested jobs and TLS teardown.
unsafe impl Allocator for ScratchAllocator {
    #[cold]
    #[inline(never)]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Self::account(Global.allocate(layout)?, layout)
    }
    #[cold]
    #[inline(never)]
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Self::account(Global.allocate_zeroed(layout)?, layout)
    }
    #[cold]
    #[inline(never)]
    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        unsafe { Global.deallocate(pointer, layout); }
        BACKING.release(layout.size());
    }
}
