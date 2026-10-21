//! Sole-owned ABI backing with a pointer-sized movable owner.
//!
//! Nonzero backing keeps its allocator in the same allocation as its payload.
//! Published payload aliases survive moving this raw owner. Zero-sized values
//! need no allocation or allocator lifetime and retain their aligned dangling
//! Box pointer; no control record is accessed for that case.
use crate::runtime::metadata_allocator::MetadataAllocator;
use allocator_api2::{alloc::Allocator, boxed::Box};
use std::{
    alloc::Layout,
    marker::PhantomData,
    mem::{self, ManuallyDrop},
    ops::Deref,
    ptr::{self, NonNull},
};
#[repr(C)]
struct Control<T> {
    allocator: ManuallyDrop<MetadataAllocator>,
    value: T,
}
pub(crate) struct AbiStorage<T> {
    pointer: NonNull<Control<T>>,
    _owns: PhantomData<T>,
}
impl<T> AbiStorage<T> {
    #[cfg(test)]
    pub(crate) fn new(value: T) -> Self {
        Self::new_in(value, &MetadataAllocator::default())
    }
    pub(crate) fn new_in(value: T, allocator: &MetadataAllocator) -> Self {
        let pointer = if mem::size_of::<T>() == 0 {
            let (pointer, _) = Box::into_non_null(Box::new_in(value, allocator));
            // Only cast back to T for zero-sized owners; never dereference this
            // pointer as Control<T>, whose fields were not allocated.
            pointer.cast()
        } else {
            let (pointer, _) = Box::into_non_null(Box::new_in(
                Control {
                    allocator: ManuallyDrop::new(allocator.clone()),
                    value,
                },
                allocator,
            ));
            pointer
        };
        Self {
            pointer,
            _owns: PhantomData,
        }
    }
    pub(crate) fn as_raw(&self) -> *mut T {
        if mem::size_of::<T>() == 0 {
            self.pointer.as_ptr().cast()
        } else {
            // SAFETY: nonzero owners contain one initialized Control<T>.
            // Form the field pointer without creating a payload reference.
            unsafe { ptr::addr_of_mut!((*self.pointer.as_ptr()).value) }
        }
    }
    #[cfg(test)]
    pub(crate) fn allocation_bytes() -> usize {
        if mem::size_of::<T>() == 0 {
            0
        } else {
            Layout::new::<Control<T>>().size()
        }
    }
}
impl<T> Deref for AbiStorage<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: this sole owner keeps the initialized payload live. Shared
        // mutation still requires T's interior-mutability synchronization.
        unsafe { &*self.as_raw() }
    }
}
impl<T> Drop for AbiStorage<T> {
    fn drop(&mut self) {
        if mem::size_of::<T>() == 0 {
            // SAFETY: the original aligned zero-sized Box pointer remains
            // valid for T's one destruction, with no physical block to free.
            unsafe { ptr::drop_in_place(self.as_raw()) };
            return;
        }
        // SAFETY: this is the unique live Control owner. Move its allocator
        // into an independent guard before destroying the payload. The guard
        // frees the original block even if T::drop unwinds.
        let allocator = unsafe { ManuallyDrop::take(&mut (*self.pointer.as_ptr()).allocator) };
        let _release = AllocationRelease {
            pointer: self.pointer.cast(),
            layout: Layout::new::<Control<T>>(),
            allocator,
        };
        // SAFETY: higher-level owners retired published aliases first; the
        // payload was initialized once and has not been destroyed.
        unsafe { ptr::drop_in_place(self.as_raw()) };
    }
}
struct AllocationRelease {
    pointer: NonNull<u8>,
    layout: Layout,
    allocator: MetadataAllocator,
}
impl Drop for AllocationRelease {
    fn drop(&mut self) {
        // SAFETY: pointer and layout identify the original allocation; its
        // payload has finished destruction or unwound. This guard owns the
        // matching allocator through physical deallocation and charge release.
        unsafe { self.allocator.deallocate(self.pointer, self.layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn collector_abi_storage_preserves_exported_atomic_across_moves() {
        let mut storage = AbiStorage::new(AtomicUsize::new(0));
        let pointer = storage.as_raw();
        for value in 1..=8 {
            let mut owners = Vec::new();
            owners.push(storage);
            owners.reserve_exact(64);
            storage = owners.pop().unwrap();
            assert_eq!(storage.as_raw(), pointer);
            unsafe {
                (*pointer).store(value, Ordering::Release);
            }
            assert_eq!(storage.load(Ordering::Acquire), value);
        }
    }

    #[test]
    fn collector_abi_storage_preserves_alignment_and_drops_once() {
        #[repr(align(256))]
        struct Payload<'a>(&'a AtomicUsize);
        impl Drop for Payload<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let drops = AtomicUsize::new(0);
        let storage = AbiStorage::new(Payload(&drops));
        assert_eq!(storage.as_raw().addr() % 256, 0);
        let mut owners = Vec::new();
        owners.push(storage);
        owners.reserve_exact(64);
        let moved = owners.pop().unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(moved);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn collector_abi_storage_accounts_actual_request_through_payload_drop() {
        use crate::runtime::{allocation_counter::AllocationCounter, allocation_probe};
        use std::sync::Arc;
        #[repr(align(256))]
        struct Payload {
            counter: Arc<AllocationCounter>,
            data: [u8; 257],
        }
        impl Drop for Payload {
            fn drop(&mut self) {
                assert_eq!(self.counter.bytes(), AbiStorage::<Self>::allocation_bytes());
            }
        }
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        allocation_probe::start();
        let storage = AbiStorage::new_in(
            Payload {
                counter: counter.clone(),
                data: [42; 257],
            },
            &allocator,
        );
        assert_eq!(
            allocation_probe::finish_bytes(),
            (1, 0, AbiStorage::<Payload>::allocation_bytes(), 0)
        );
        assert_eq!(counter.bytes(), AbiStorage::<Payload>::allocation_bytes());
        assert_eq!(storage.as_raw().addr() % 256, 0);
        assert!(storage.data.iter().all(|&byte| byte == 42));
        drop(allocator);
        allocation_probe::start();
        drop(storage);
        assert_eq!(
            allocation_probe::finish_bytes(),
            (0, 1, 0, AbiStorage::<Payload>::allocation_bytes())
        );
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 1);
    }

    #[test]
    fn collector_abi_storage_unwind_releases_physical_backing_and_counter() {
        use crate::runtime::{allocation_counter::AllocationCounter, allocation_probe};
        use std::sync::Arc;
        struct Panics(Arc<AllocationCounter>);
        impl Drop for Panics {
            fn drop(&mut self) {
                assert_eq!(self.0.bytes(), AbiStorage::<Self>::allocation_bytes());
                panic!("payload drop");
            }
        }
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let storage = AbiStorage::new_in(Panics(counter.clone()), &allocator);
        // Panic machinery allocates independently; exact physical release is
        // checked by the final counter and the allocator's own release tests.
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(storage))).is_err());
        assert_eq!(counter.bytes(), 0);
        drop(allocator);
        assert_eq!(Arc::strong_count(&counter), 1);
        allocation_probe::start();
        let empty = AbiStorage::new_in((), &MetadataAllocator::new(&counter));
        assert_eq!(counter.bytes(), 0);
        drop(empty);
        #[repr(align(256))]
        struct AlignedEmpty;
        let aligned = AbiStorage::new_in(AlignedEmpty, &MetadataAllocator::new(&counter));
        assert_eq!(aligned.as_raw().addr() % 256, 0);
        drop(aligned);
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert_eq!(Arc::strong_count(&counter), 1);
        // A zero-sized payload still has exactly one destructor, even though
        // the owner needs no control allocation or retained allocator.
        static ZERO_DROPS: AtomicUsize = AtomicUsize::new(0);
        struct EmptyDrop;
        impl Drop for EmptyDrop {
            fn drop(&mut self) {
                ZERO_DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }
        struct EmptyPanic;
        impl Drop for EmptyPanic {
            fn drop(&mut self) {
                ZERO_DROPS.fetch_add(1, Ordering::Relaxed);
                panic!("zero-sized drop");
            }
        }
        assert_eq!(ZERO_DROPS.load(Ordering::Relaxed), 0);
        allocation_probe::start();
        let owner = AbiStorage::new_in(EmptyDrop, &MetadataAllocator::new(&counter));
        assert_eq!(Arc::strong_count(&counter), 1);
        let mut moved = Some(owner);
        drop(moved.take());
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert_eq!(ZERO_DROPS.load(Ordering::Relaxed), 1);
        let owner = AbiStorage::new_in(EmptyPanic, &MetadataAllocator::new(&counter));
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(owner))).is_err());
        assert_eq!(ZERO_DROPS.load(Ordering::Relaxed), 2);
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 1);
    }

    #[test]
    fn collector_abi_storage_accounted_exported_pointer_survives_owner_moves() {
        use crate::runtime::allocation_counter::AllocationCounter;
        use std::sync::Arc;
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let mut owner = AbiStorage::new_in(AtomicUsize::new(0), &allocator);
        let pointer = owner.as_raw();
        drop(allocator);
        for value in 1..=64 {
            let mut owners = Vec::new();
            owners.push(owner);
            owners.reserve_exact(32);
            owner = owners.pop().unwrap();
            unsafe { (*pointer).store(value, Ordering::Release) };
            assert_eq!(owner.load(Ordering::Acquire), value);
            assert_eq!(owner.as_raw(), pointer);
            assert_eq!(
                counter.bytes(),
                AbiStorage::<AtomicUsize>::allocation_bytes()
            );
        }
        drop(owner);
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 1);
    }
}
