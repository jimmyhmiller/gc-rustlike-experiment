//! Matching-width scalar Atom accesses, excluded from concurrent copying.
use super::Thread;
use crate::gc::{Full, ObjHeader};
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

macro_rules! scalar_atom {
    ($load:ident, $cas:ident, $bits:ty, $atomic:ty) => {
        /// Load a scalar Atom's bits with matching-width SeqCst ordering.
        ///
        /// # Safety
        /// The caller owns a RUNNING registration; thread/atom are live in its
        /// heap. The Full-header object has an initialized, naturally aligned
        /// payload of this width. All accesses use compatible atomics and GC
        /// exclusion. No nested access scope, safepoint or managed callback.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $load(thread: *mut Thread, atom: *mut u8) -> $bits {
            unsafe {
                let heap = &*(*thread).heap;
                let access = heap.object_access(atom);
                let slot = &*access.as_ptr().add(Full::SIZE).cast::<$atomic>();
                slot.load(Ordering::SeqCst)
            }
        }

        /// Compare/exchange scalar bits with matching-width SeqCst ordering.
        ///
        /// # Safety
        /// As the corresponding scalar load. This field is untraced scalar
        /// storage; managed-reference payloads must use the reference CAS ABI.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $cas(
            thread: *mut Thread,
            atom: *mut u8,
            expected: $bits,
            new: $bits,
        ) -> u8 {
            unsafe {
                let heap = &*(*thread).heap;
                let access = heap.object_access(atom);
                let slot = &*access.as_ptr().add(Full::SIZE).cast::<$atomic>();
                slot.compare_exchange(expected, new, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok() as u8
            }
        }
    };
}

scalar_atom!(ai_atom_bits8_load, ai_atom_bits8_cas, u8, AtomicU8);
scalar_atom!(ai_atom_bits16_load, ai_atom_bits16_cas, u16, AtomicU16);
scalar_atom!(ai_atom_bits32_load, ai_atom_bits32_cas, u32, AtomicU32);
scalar_atom!(ai_atom_bits64_load, ai_atom_bits64_cas, u64, AtomicU64);
