//! Allocator-owned registration handles, independent of the owning heap's lifetime.
use super::ThreadState;
use crate::{
    gc::alloc::{SharedMetadata, SharedMetadataWeak},
    runtime::metadata_allocator::MetadataAllocator,
};
use std::ops::Deref;

/// Shared ownership of one stable GC thread registration.
///
/// Cloning a handle does not allocate. Its record and control stay charged to
/// the originating heap's native ledger until their physical deallocation.
#[repr(transparent)]
#[derive(Clone)]
pub struct ThreadStateHandle(SharedMetadata<ThreadState>);

/// An observer that retains the registration allocation without retaining its
/// live payload or buffers. Once the last strong handle dies, upgrade fails;
/// the complete record allocation remains charged until the last weak drop.
#[repr(transparent)]
#[derive(Clone)]
pub struct ThreadStateWeak(SharedMetadataWeak<ThreadState>);

impl ThreadStateHandle {
    #[inline]
    pub(crate) fn new_in(value: ThreadState, allocator: &MetadataAllocator) -> Self {
        Self(SharedMetadata::new_in(value, allocator))
    }
    /// Borrow the stable payload address. The pointer owns no reference count
    /// and may be dereferenced only while a strong handle keeps the record live.
    #[inline]
    pub fn as_ptr(this: &Self) -> *const ThreadState {
        SharedMetadata::as_ptr(&this.0)
    }
    /// Check whether two handles own the same registration.
    #[inline]
    pub fn ptr_eq(left: &Self, right: &Self) -> bool {
        SharedMetadata::ptr_eq(&left.0, &right.0)
    }
    /// A diagnostic count snapshot; concurrent clones/drops may change it.
    #[inline]
    pub fn strong_count(this: &Self) -> usize {
        SharedMetadata::strong_count(&this.0)
    }
    /// Observe this registration without retaining the live payload.
    #[inline]
    pub fn downgrade(this: &Self) -> ThreadStateWeak {
        ThreadStateWeak(SharedMetadata::downgrade(&this.0))
    }
    #[cfg(test)]
    pub(crate) fn allocation_bytes() -> usize {
        SharedMetadata::<ThreadState>::allocation_bytes()
    }
}
impl Deref for ThreadStateHandle {
    type Target = ThreadState;
    #[inline]
    fn deref(&self) -> &ThreadState {
        &self.0
    }
}
impl ThreadStateWeak {
    /// Obtain a live owner, or None after payload destruction has begun.
    #[inline]
    pub fn upgrade(&self) -> Option<ThreadStateHandle> {
        self.0.upgrade().map(ThreadStateHandle)
    }
}
