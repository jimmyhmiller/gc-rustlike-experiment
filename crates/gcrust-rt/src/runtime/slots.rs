//! Complete non-safepointing accesses to mutable managed slots.
use super::Thread;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

macro_rules! slots {
    ($load:ident, $store:ident, $bits:ty, $atomic:ty) => {
        /// Load a slot under canonical access with matching-width SC ordering.
        ///
        /// # Safety
        /// The caller owns a RUNNING registration. Object is a live published
        /// base in that heap; offset identifies a valid initialized aligned slot
        /// of this width outside the header. All writers obey the same atomic
        /// and GC protocols. No nested guard, safepoint or managed callback.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $load(
            thread: *mut Thread,
            object: *mut u8,
            offset: usize,
        ) -> $bits {
            unsafe {
                let access = (&*(*thread).heap).object_access(object);
                (&*access.as_ptr().add(offset).cast::<$atomic>()).load(Ordering::SeqCst)
            }
        }
        /// Store untraced bits under canonical access with matching-width SC ordering.
        ///
        /// # Safety
        /// As the matching load; this is scalar storage, never a traced slot.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $store(
            thread: *mut Thread,
            object: *mut u8,
            offset: usize,
            value: $bits,
        ) {
            unsafe {
                let access = (&*(*thread).heap).object_access(object);
                (&*access.as_ptr().add(offset).cast::<$atomic>()).store(value, Ordering::SeqCst);
            }
        }
    };
}
slots!(ai_slot_bits8_load, ai_slot_bits8_store, u8, AtomicU8);
slots!(ai_slot_bits16_load, ai_slot_bits16_store, u16, AtomicU16);
slots!(ai_slot_bits32_load, ai_slot_bits32_store, u32, AtomicU32);
slots!(ai_slot_bits64_load, ai_slot_bits64_store, u64, AtomicU64);

/// Replace a traced reference, protecting the whole SATB/store/card operation.
///
/// # Safety
/// As the 64-bit slot load. The slot contains an untagged managed base or null;
/// value is null or a live published managed base kept reachable during this
/// operation. The caller is the owning RUNNING registration in this heap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_slot_ref_store(
    thread: *mut Thread,
    object: *mut u8,
    offset: usize,
    value: *mut u8,
) {
    unsafe {
        let heap = &*(*thread).heap;
        let access = heap.object_access(object);
        let slot = &*access.as_ptr().add(offset).cast::<AtomicU64>();
        let mut old = slot.load(Ordering::SeqCst);
        loop {
            if heap.barriers_active() {
                (&*(*thread).dyna_thread).log_satb(heap, old);
            }
            match slot.compare_exchange(old, value as u64, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(actual) => old = actual,
            }
        }
        if heap.has_nursery() && heap.is_nursery(value) {
            heap.mark_card_dirty(slot as *const AtomicU64 as *const u8);
        }
    }
}
