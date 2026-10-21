//! Allocator-level requested-byte ownership, independent of Heap lifetime.
use super::allocation_counter::AllocationCounter;
use allocator_api2::alloc::{AllocError, Allocator, Global};
use std::{alloc::Layout, ptr::NonNull, sync::Arc};
#[derive(Clone, Default)]
pub(crate) struct MetadataAllocator<A = Global> {
    counter: Option<Arc<AllocationCounter>>,
    base: A,
}
impl MetadataAllocator {
    pub(crate) fn new(counter: &Arc<AllocationCounter>) -> Self {
        Self {
            counter: Some(counter.clone()),
            base: Global,
        }
    }
    /// Transfer a standard Global-owned vector without copying its payload.
    /// The allocator charge begins when this owner adopts the backing and ends
    /// when the vector (or its owning iterator) physically deallocates it.
    pub(crate) fn adopt_vec<T>(&self, values: Vec<T>) -> allocator_api2::vec::Vec<T, Self> {
        let layout = Layout::array::<T>(values.capacity()).expect("existing vector layout");
        if let Some(counter) = &self.counter {
            if !counter.try_charge(layout.size()) {
                std::alloc::handle_alloc_error(layout);
            }
        }
        let mut values = std::mem::ManuallyDrop::new(values);
        // SAFETY: std::Vec uses the global allocator; allocator_api2::Global
        // delegates to the same std::alloc allocation/deallocation functions.
        // Pointer, initialized length and capacity are transferred unchanged,
        // and ManuallyDrop prevents the original owner from freeing the block.
        // The requested-byte charge above matches its capacity layout (including
        // zero-sized elements); this allocator now owns the sole deallocation.
        unsafe {
            allocator_api2::vec::Vec::from_raw_parts_in(
                values.as_mut_ptr(), values.len(), values.capacity(), self.clone(),
            )
        }
    }

    pub(crate) fn same_counter(&self, other: &Self) -> bool {
        match (&self.counter, &other.counter) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}
impl<A: Allocator> MetadataAllocator<A> {
    // Both allocation paths must free unpublished storage if accounting overflows.
    fn account(&self, block: NonNull<[u8]>, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if let Some(counter) = &self.counter {
            if !counter.try_charge(layout.size()) {
                unsafe { self.base.deallocate(block.cast(), layout) };
                return Err(AllocError);
            }
        }
        Ok(block)
    }
}
// SAFETY: The base allocator owns returned blocks and receives the same pointer/layout
// at deallocation. Accounting never dereferences payloads or retains the heap.
// Clones share the counter; buffers can outlive their original registry/heap.
unsafe impl<A: Allocator> Allocator for MetadataAllocator<A> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.account(self.base.allocate(layout)?, layout)
    }
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // Preserve the base allocator's zeroed-allocation path (e.g. calloc).
        self.account(self.base.allocate_zeroed(layout)?, layout)
    }
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        unsafe {
            self.base.deallocate(ptr, layout);
        }
        if let Some(counter) = &self.counter {
            counter.release(layout.size());
        }
    }
}

#[cfg(test)]
#[path = "metadata_allocator_tests.rs"]
mod tests;
