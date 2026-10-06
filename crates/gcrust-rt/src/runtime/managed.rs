//! SC linearization for mutable inline aggregate slots.
//!
//! Scalar/reference slots use native SC atomics. Aggregate slots are accessed
//! under one address stripe; unrelated objects can proceed independently. An SC
//! RMW at unlock is the aggregate access's linearization event in the same system
//! order as scalar atomics. No safepoint or allocation is allowed while holding
//! a stripe. Waiters publish their roots and park; relocation changes the stripe
//! address, so acquisition revalidates after resumption and retries as necessary.
use super::{Thread, blocking_region};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

const STRIPES: usize = 64;

struct Stripe {
    held: Mutex<bool>,
    available: Condvar,
}

pub(crate) struct ManagedAccess {
    stripes: [Stripe; STRIPES],
    order: AtomicU64,
}

impl ManagedAccess {
    pub(crate) fn new() -> Self {
        Self {
            stripes: std::array::from_fn(|_| Stripe {
                held: Mutex::new(false),
                available: Condvar::new(),
            }),
            order: AtomicU64::new(0),
        }
    }

    fn index(object: *const u8) -> usize {
        // Mix allocation alignment bits so adjacent, similarly sized objects
        // spread across stripes; identity is revalidated after any GC pause.
        let address = object as usize >> 4;
        (address ^ (address >> 6)) & (STRIPES - 1)
    }

    fn acquire(&self, index: usize) {
        let stripe = &self.stripes[index];
        let mut held = stripe.held.lock().unwrap();
        while *held {
            held = stripe.available.wait(held).unwrap();
        }
        *held = true;
    }

    fn release(&self, index: usize, linearize: bool) {
        if linearize {
            self.order.fetch_add(1, Ordering::SeqCst);
        }
        let stripe = &self.stripes[index];
        let mut held = stripe.held.lock().unwrap();
        assert!(*held, "managed aggregate stripe was not held");
        *held = false;
        stripe.available.notify_one();
    }
}

/// Acquire an aggregate object's stripe and return its post-relocation address.
///
/// # Safety
/// The caller is the owning running mutator, with a valid rooted object. It must
/// perform only the snapshot load/store and barrier before ai_managed_unlock:
/// no allocation, safepoint, foreign call, suspension or nested acquisition.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_managed_lock(thread: *mut Thread, object: *mut u8) -> *mut u8 {
    unsafe {
        assert!(
            (*thread).managed_lock.is_none(),
            "nested managed aggregate access"
        );
        let heap = &*(*thread).heap;
        let dyna = &*(*thread).dyna_thread;
        let roots = dyna.scratch_scope();
        let slot = roots.push(object);
        loop {
            let before = roots.get(slot);
            let index = ManagedAccess::index(before);
            blocking_region(thread, || heap.managed_access.acquire(index));
            let current = roots.get(slot);
            if ManagedAccess::index(current) != index {
                heap.managed_access.release(index, false);
                continue;
            }
            (*thread).managed_lock = Some(index);
            return current;
        }
    }
}

/// Linearize and release the current aggregate snapshot access.
///
/// # Safety
/// Must be paired on the same mutator with ai_managed_lock. All accesses to the
/// protected slot (including its generational barrier) must already be complete.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_managed_unlock(thread: *mut Thread) {
    unsafe {
        let index = (*thread)
            .managed_lock
            .take()
            .expect("unpaired managed aggregate unlock");
        (&*(*thread).heap).managed_access.release(index, true);
    }
}

/// Copy the FFI buffer one indivisible SC array element at a time. The native
/// side is private/unshared and may be unaligned. The managed side has natural
/// element alignment. Whole arrays are not an indivisible snapshot.
///
/// # Safety
/// Both buffers span byte_len initialized bytes; managed has natural alignment
/// for bits (1/8/16/32/64). No collection may run during this nonallocating copy.
pub(super) unsafe fn copy_scalar_buffer(
    managed: *mut u8,
    native: *mut u8,
    byte_len: i64,
    bits: i64,
    copy_out: bool,
) {
    let stride = match bits {
        1 | 8 => 1,
        16 => 2,
        32 => 4,
        64 => 8,
        _ => panic!("invalid scalar buffer width"),
    };
    assert!(
        byte_len >= 0 && byte_len % stride == 0,
        "invalid scalar buffer size"
    );
    macro_rules! copy {
        ($integer:ty, $atomic:ty) => {{
            for offset in (0..byte_len as usize).step_by(stride as usize) {
                unsafe {
                    let cell = &*managed.add(offset).cast::<$atomic>();
                    let raw = native.add(offset).cast::<$integer>();
                    if copy_out {
                        let mut value = core::ptr::read_unaligned(raw);
                        if bits == 1 {
                            value &= 1;
                        }
                        cell.store(value, Ordering::SeqCst);
                    } else {
                        let mut value = cell.load(Ordering::SeqCst);
                        if bits == 1 {
                            value &= 1;
                        }
                        core::ptr::write_unaligned(raw, value);
                    }
                }
            }
        }};
    }
    match bits {
        1 | 8 => copy!(u8, std::sync::atomic::AtomicU8),
        16 => copy!(u16, std::sync::atomic::AtomicU16),
        32 => copy!(u32, std::sync::atomic::AtomicU32),
        64 => copy!(u64, AtomicU64),
        _ => unreachable!(),
    }
}
