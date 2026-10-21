//! Shared ownership of one allocator-accounted metadata allocation.
//!
//! The payload dies with its last strong owner. Weak observers retain the
//! complete allocation and its allocator charge until their own final release.
use crate::runtime::metadata_allocator::MetadataAllocator;
use allocator_api2::{alloc::Allocator, boxed::Box};
use std::{
    alloc::Layout,
    marker::PhantomData,
    mem::{ManuallyDrop, MaybeUninit},
    ops::Deref,
    ptr::{self, NonNull},
    sync::atomic::{AtomicUsize, Ordering},
};

#[repr(C)]
struct Control<T> {
    strong: AtomicUsize,
    // Includes one implicit weak count while any strong owner exists.
    weak: AtomicUsize,
    allocator: ManuallyDrop<MetadataAllocator>,
    // The control remains valid after the payload's destruction.
    value: MaybeUninit<T>,
}

pub(crate) struct SharedMetadata<T> {
    pointer: NonNull<Control<T>>,
    _owns: PhantomData<T>,
}

pub(crate) struct SharedMetadataWeak<T> {
    pointer: NonNull<Control<T>>,
    _owns: PhantomData<T>,
}

// SAFETY: counts are atomic; payload access is shared and its destruction can
// happen on any releasing thread. The Global-backed allocator is Send + Sync.
unsafe impl<T: Send + Sync> Send for SharedMetadata<T> {}
unsafe impl<T: Send + Sync> Sync for SharedMetadata<T> {}
// SAFETY: a weak owner only accesses counts or atomically obtains a strong
// owner, which has the same Send + Sync requirements as above.
unsafe impl<T: Send + Sync> Send for SharedMetadataWeak<T> {}
unsafe impl<T: Send + Sync> Sync for SharedMetadataWeak<T> {}

fn increment(count: &AtomicUsize) {
    if count
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
            old.checked_add(1).filter(|&new| new <= isize::MAX as usize)
        })
        .is_err()
    {
        std::process::abort();
    }
}

impl<T> SharedMetadata<T> {
    pub(crate) fn new_in(value: T, allocator: &MetadataAllocator) -> Self {
        let block = Box::new_in(
            Control {
                strong: AtomicUsize::new(1),
                weak: AtomicUsize::new(1),
                allocator: ManuallyDrop::new(allocator.clone()),
                value: MaybeUninit::new(value),
            },
            allocator,
        );
        let (pointer, _) = Box::into_non_null(block);
        Self {
            pointer,
            _owns: PhantomData,
        }
    }

    pub(crate) fn downgrade(this: &Self) -> SharedMetadataWeak<T> {
        // SAFETY: this strong owner retains the implicit weak count and block.
        increment(unsafe { &*ptr::addr_of!((*this.pointer.as_ptr()).weak) });
        SharedMetadataWeak {
            pointer: this.pointer,
            _owns: PhantomData,
        }
    }

    pub(crate) fn as_ptr(this: &Self) -> *const T {
        // SAFETY: a strong owner retains an initialized payload; forming its
        // field pointer does not create references to other control fields.
        unsafe { ptr::addr_of!((*this.pointer.as_ptr()).value).cast::<T>() }
    }

    pub(crate) fn strong_count(this: &Self) -> usize {
        // SAFETY: this owner retains the block throughout the count read.
        unsafe { &*ptr::addr_of!((*this.pointer.as_ptr()).strong) }.load(Ordering::Relaxed)
    }

    pub(crate) fn ptr_eq(left: &Self, right: &Self) -> bool {
        left.pointer == right.pointer
    }

    #[cfg(test)]
    pub(crate) fn allocation_bytes() -> usize {
        Layout::new::<Control<T>>().size()
    }
    #[cfg(test)]
    pub(crate) fn allocation_identity(this: &Self) -> usize {
        this.pointer.as_ptr().addr()
    }

    #[cold]
    #[inline(never)]
    unsafe fn drop_last(&mut self) {
        // Transfer the existing implicit count into an unwind guard. Explicit
        // weak observers may disappear during T::drop, but cannot free its block
        // before that destructor returns or unwinds through this guard.
        let _implicit = SharedMetadataWeak {
            pointer: self.pointer,
            _owns: PhantomData,
        };
        // SAFETY: strong reached zero once and acquired prior owner releases;
        // upgrades cannot resurrect it. This is T's unique destruction.
        unsafe { ptr::drop_in_place(Self::as_ptr(self).cast_mut()) };
    }
}

impl<T> Clone for SharedMetadata<T> {
    #[inline]
    fn clone(&self) -> Self {
        // SAFETY: this owner keeps the positive strong count alive.
        increment(unsafe { &*ptr::addr_of!((*self.pointer.as_ptr()).strong) });
        Self {
            pointer: self.pointer,
            _owns: PhantomData,
        }
    }
}
impl<T> Deref for SharedMetadata<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: only strong handles expose the initialized shared payload.
        unsafe { &*Self::as_ptr(self) }
    }
}
impl<T> Drop for SharedMetadata<T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: this handle owns one live strong count.
        let strong = unsafe { &*ptr::addr_of!((*self.pointer.as_ptr()).strong) };
        if strong.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        // The acquire load observes the release sequence, including every
        // prior owner's accesses. This form is also modeled by ThreadSanitizer.
        strong.load(Ordering::Acquire);
        // SAFETY: this was the unique transition to zero.
        unsafe { self.drop_last() };
    }
}

impl<T> SharedMetadataWeak<T> {
    pub(crate) fn upgrade(&self) -> Option<SharedMetadata<T>> {
        // SAFETY: this weak count retains the control after payload destruction.
        let strong = unsafe { &*ptr::addr_of!((*self.pointer.as_ptr()).strong) };
        let mut count = strong.load(Ordering::Relaxed);
        loop {
            if count == 0 {
                return None;
            }
            if count >= isize::MAX as usize {
                std::process::abort();
            }
            match strong.compare_exchange_weak(
                count,
                count + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(SharedMetadata {
                        pointer: self.pointer,
                        _owns: PhantomData,
                    });
                }
                Err(current) => count = current,
            }
        }
    }
}
impl<T> Clone for SharedMetadataWeak<T> {
    fn clone(&self) -> Self {
        // SAFETY: this weak owner retains a positive weak count.
        increment(unsafe { &*ptr::addr_of!((*self.pointer.as_ptr()).weak) });
        Self {
            pointer: self.pointer,
            _owns: PhantomData,
        }
    }
}
impl<T> Drop for SharedMetadataWeak<T> {
    fn drop(&mut self) {
        // SAFETY: this handle owns one live weak count. The implicit weak count
        // protects the complete payload destructor, including unwind paths.
        let weak = unsafe { &*ptr::addr_of!((*self.pointer.as_ptr()).weak) };
        if weak.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        weak.load(Ordering::Acquire);
        // SAFETY: the implicit count has gone, so T was destroyed. No other
        // weak/strong handle can access this block. Move out the allocator and
        // free the original pointer/layout without dropping T a second time.
        let allocator = unsafe { ManuallyDrop::take(&mut (*self.pointer.as_ptr()).allocator) };
        unsafe { allocator.deallocate(self.pointer.cast(), Layout::new::<Control<T>>()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{allocation_counter::AllocationCounter, allocation_probe};
    use std::sync::Arc;

    #[test]
    fn collector_shared_metadata_exact_backing_and_last_owner_release() {
        #[repr(align(128))]
        struct Aligned([u8; 257]);
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        allocation_probe::start();
        let first = SharedMetadata::new_in(Aligned([42; 257]), &allocator);
        let requests = allocation_probe::finish_bytes();
        assert_eq!(
            requests,
            (1, 0, SharedMetadata::<Aligned>::allocation_bytes(), 0)
        );
        assert_eq!(counter.bytes(), requests.2);
        assert_eq!((&*first as *const Aligned).addr() % 128, 0);
        assert!(first.0.iter().all(|&byte| byte == 42));
        allocation_probe::start();
        let second = first.clone();
        assert!(SharedMetadata::ptr_eq(&first, &second));
        assert_eq!(SharedMetadata::strong_count(&first), 2);
        drop(first);
        assert_eq!(SharedMetadata::strong_count(&second), 1);
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        drop(allocator);
        assert_eq!(Arc::strong_count(&counter), 2);
        assert_eq!(counter.bytes(), requests.2);
        assert_eq!(second.0[256], 42);
        drop(second);
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 1);
    }

    #[test]
    fn collector_shared_metadata_destructor_unwind_releases_backing_and_owner() {
        struct Panics([u8; 32]);
        impl Drop for Panics {
            fn drop(&mut self) {
                assert_eq!(self.0[0], 42);
                panic!("shared metadata destructor fixture");
            }
        }
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let value = SharedMetadata::new_in(Panics([42; 32]), &allocator);
        let last = value.clone();
        drop(value);
        drop(allocator);
        assert!(counter.bytes() > 0);
        assert!(std::panic::catch_unwind(|| drop(last)).is_err());
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 1);
    }

    #[test]
    fn collector_shared_metadata_zero_sized_value_keeps_its_control_owned() {
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let value = SharedMetadata::new_in((), &allocator);
        assert_eq!(counter.bytes(), SharedMetadata::<()>::allocation_bytes());
        assert!(counter.bytes() > 0);
        drop(allocator);
        assert_eq!(*value, ());
        drop(value);
        assert_eq!(counter.bytes(), 0);
    }

    #[test]
    fn collector_shared_metadata_concurrent_owners_destroy_value_once() {
        struct Value {
            observed: AtomicUsize,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Value {
            fn drop(&mut self) {
                assert_eq!(self.observed.load(Ordering::Relaxed), 256);
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let drops = Arc::new(AtomicUsize::new(0));
        let value = SharedMetadata::new_in(
            Value {
                observed: AtomicUsize::new(0),
                drops: drops.clone(),
            },
            &allocator,
        );
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let owner = value.clone();
                scope.spawn(move || {
                    for _ in 0..64 {
                        let temporary = owner.clone();
                        temporary.observed.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(SharedMetadata::strong_count(&value), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(value);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(counter.bytes(), 0);
    }

    #[test]
    fn collector_shared_metadata_last_release_acquires_prior_owner_writes() {
        use std::{cell::UnsafeCell, sync::atomic::AtomicBool};
        struct Value {
            slots: [UnsafeCell<usize>; 4],
            drops: Arc<AtomicUsize>,
        }
        // SAFETY: each slot is written by one uniquely assigned worker. Only
        // the final shared owner reads them in Drop, after acquiring all prior
        // owner releases. No value access occurs after an owner's release.
        unsafe impl Sync for Value {}
        impl Drop for Value {
            fn drop(&mut self) {
                for (index, slot) in self.slots.iter().enumerate() {
                    // SAFETY: last-owner acquire has synchronized each write.
                    assert_eq!(unsafe { *slot.get() }, index + 42);
                }
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let drops = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(AtomicBool::new(false));
        let value = SharedMetadata::new_in(
            Value {
                slots: std::array::from_fn(|_| UnsafeCell::new(0)),
                drops: drops.clone(),
            },
            &allocator,
        );
        std::thread::scope(|scope| {
            for index in 0..4 {
                let owner = value.clone();
                let start = start.clone();
                scope.spawn(move || {
                    while !start.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    // SAFETY: this worker alone writes its assigned slot. The
                    // final owner reads only after refcount synchronization.
                    unsafe {
                        *owner.slots[index].get() = index + 42;
                    }
                    drop(owner);
                });
            }
            // Every writer owns a handle before this release. The last worker
            // destroys Value before its thread exits; a main-thread join cannot
            // supply the synchronization required by the destructor reads.
            drop(value);
            start.store(true, Ordering::Release);
        });
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(counter.bytes(), 0);
    }

    #[test]
    fn collector_shared_metadata_weak_observers_retain_exact_charge_after_payload_drop() {
        struct Value {
            drops: Arc<AtomicUsize>,
            bytes: [u8; 257],
        }
        impl Drop for Value {
            fn drop(&mut self) {
                assert_eq!(self.bytes[256], 42);
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let drops = Arc::new(AtomicUsize::new(0));
        let first = SharedMetadata::new_in(
            Value {
                drops: drops.clone(),
                bytes: [42; 257],
            },
            &allocator,
        );
        let charge = SharedMetadata::<Value>::allocation_bytes();
        assert_eq!(counter.bytes(), charge);
        allocation_probe::start();
        let weak = SharedMetadata::downgrade(&first);
        let last_weak = weak.clone();
        let second = weak.upgrade().unwrap();
        assert!(SharedMetadata::ptr_eq(&first, &second));
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        drop(first);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(second);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(weak.upgrade().is_none());
        assert_eq!(counter.bytes(), charge);
        drop(weak);
        drop(allocator);
        assert_eq!(Arc::strong_count(&counter), 2);
        assert_eq!(counter.bytes(), charge);
        assert!(last_weak.upgrade().is_none());
        drop(last_weak);
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn collector_shared_metadata_weak_survives_payload_destructor_unwind() {
        struct Panics;
        impl Drop for Panics {
            fn drop(&mut self) {
                panic!("weak payload destructor fixture")
            }
        }
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let owner = SharedMetadata::new_in(Panics, &allocator);
        let weak = SharedMetadata::downgrade(&owner);
        drop(allocator);
        assert!(std::panic::catch_unwind(|| drop(owner)).is_err());
        assert!(weak.upgrade().is_none());
        assert_eq!(
            counter.bytes(),
            SharedMetadata::<Panics>::allocation_bytes()
        );
        drop(weak);
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 1);
    }

    #[test]
    fn collector_shared_metadata_weak_upgrade_races_final_strong_release() {
        struct Value {
            reads: AtomicUsize,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Value {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let drops = Arc::new(AtomicUsize::new(0));
        let owner = SharedMetadata::new_in(
            Value {
                reads: AtomicUsize::new(0),
                drops: drops.clone(),
            },
            &allocator,
        );
        let weak = SharedMetadata::downgrade(&owner);
        let barrier = std::sync::Barrier::new(5);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let observer = weak.clone();
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..256 {
                        if let Some(owner) = observer.upgrade() {
                            owner.reads.fetch_add(1, Ordering::Relaxed);
                            std::thread::yield_now();
                        } else {
                            break;
                        }
                    }
                });
            }
            barrier.wait();
            drop(owner);
        });
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(weak.upgrade().is_none());
        assert!(counter.bytes() > 0);
        drop(weak);
        assert_eq!(counter.bytes(), 0);
    }
}
