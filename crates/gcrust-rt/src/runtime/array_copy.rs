//! `array_copy`: bulk element copy between managed arrays.
use super::{abort_with_diagnostic, Thread};
use crate::gc::{Heap, TypeInfo, VarLenKind};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

/// Copy `count` elements of `stride` bytes from `src[src_pos..]` to
/// `dst[dst_pos..]`, as if one at a time in the order that is correct when
/// the ranges overlap. Each element is copied without tearing its aligned
/// words; references are barriered like single stores.
///
/// # Safety
/// `thread` is this OS thread's RUNNING mutator; `src` and `dst` are live
/// arrays of the same element type whose elements are `stride` bytes. Does not
/// safepoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ai_array_copy(
    thread: *mut Thread,
    src: *mut u8,
    src_pos: i64,
    dst: *mut u8,
    dst_pos: i64,
    count: i64,
    stride: i64,
) {
    unsafe {
        let heap = &*(*thread).heap;
        let info = heap.type_info_by_id(heap.obj_type_id(dst));
        let stride = stride.max(1) as usize;
        // Type ids and varlen counts never change, so either copy serves.
        let len = |array: *mut u8| {
            let raw = core::ptr::read(array.add(info.varlen_count_offset()) as *const u64) as usize;
            if info.varlen == VarLenKind::Values { raw } else { raw / stride }
        };
        let (src_len, dst_len) = (len(src), len(dst));
        let in_bounds = |pos: i64, len: usize| pos >= 0 && count >= 0 && (pos as u128 + count as u128) <= len as u128;
        if !in_bounds(src_pos, src_len) || !in_bounds(dst_pos, dst_len) {
            let message = format!(
                "gc-rust: array copy out of bounds: {count} elements from {src_pos} (length {src_len}) to {dst_pos} (length {dst_len})"
            );
            abort_with_diagnostic(thread, message.as_bytes());
        }
        let count = count as usize;
        if count == 0 {
            return;
        }
        let base = info.varlen_count_offset() + 8;
        let (src_off, dst_off) = (base + src_pos as usize * stride, base + dst_pos as usize * stride);
        // Copy high to low when the destination overlaps above the source.
        let backwards = src == dst && dst_pos > src_pos;
        let has_refs = info.varlen == VarLenKind::Values || !info.record_ptrs.is_empty();
        if !heap.barriers_active() {
            if has_refs {
                copy_units(src.add(src_off), dst.add(dst_off), count * stride, stride, backwards);
            } else {
                // Aligned scalar elements; memmove does not split them.
                core::ptr::copy(src.add(src_off), dst.add(dst_off), count * stride);
            }
            mark_cards(heap, info, dst, dst_off, count, stride);
            return;
        }
        // While the collector is active, each element moves through a buffer
        // under one object access at a time.
        let mut buffer = [0u64; 16];
        assert!(stride <= core::mem::size_of_val(&buffer), "array copy element too large");
        let scratch = buffer.as_mut_ptr().cast::<u8>();
        for step in 0..count {
            let i = if backwards { count - 1 - step } else { step };
            {
                let access = heap.object_access(src);
                copy_units(access.as_ptr().add(src_off + i * stride), scratch, stride, stride, false);
            }
            let access = heap.object_access(dst);
            let target = access.as_ptr().add(dst_off + i * stride);
            for_each_ref(info, stride, |off| {
                let old = (&*target.add(off).cast::<AtomicU64>()).load(Ordering::Relaxed);
                (&*(*thread).dyna_thread).log_satb(heap, old);
            });
            copy_units(scratch, target, stride, stride, false);
            mark_cards(heap, info, access.as_ptr(), dst_off + i * stride, 1, stride);
        }
    }
}

/// The record-relative offsets of the references in one element.
fn for_each_ref(info: &TypeInfo, stride: usize, mut visit: impl FnMut(usize)) {
    if info.varlen == VarLenKind::Values {
        (0..stride).step_by(8).for_each(&mut visit);
    } else {
        info.record_ptrs.iter().for_each(|&off| visit(off as usize));
    }
}

/// Dirty the cards of `dst` elements that now refer into the nursery.
unsafe fn mark_cards(heap: &Heap, info: &TypeInfo, dst: *mut u8, offset: usize, count: usize, stride: usize) {
    if !heap.has_nursery() || heap.is_nursery(dst) || (info.varlen != VarLenKind::Values && info.record_ptrs.is_empty()) {
        return;
    }
    for i in 0..count {
        let element = unsafe { dst.add(offset + i * stride) };
        for_each_ref(info, stride, |off| {
            let slot = unsafe { element.add(off) };
            let value = unsafe { (&*slot.cast::<AtomicU64>()).load(Ordering::Relaxed) } as *const u8;
            if !value.is_null() && heap.is_nursery(value) {
                heap.mark_card_dirty(slot);
            }
        });
    }
}

/// Copy `bytes` in the widest units `stride` allows, each atomically.
unsafe fn copy_units(src: *const u8, dst: *mut u8, bytes: usize, stride: usize, backwards: bool) {
    macro_rules! units {
        ($atomic:ty, $width:expr) => {{
            let n = bytes / $width;
            for step in 0..n {
                let i = if backwards { n - 1 - step } else { step };
                let value = unsafe { (&*src.add(i * $width).cast::<$atomic>()).load(Ordering::Relaxed) };
                unsafe { (&*dst.add(i * $width).cast::<$atomic>()).store(value, Ordering::Relaxed) };
            }
        }};
    }
    match stride {
        s if s % 8 == 0 => units!(AtomicU64, 8),
        s if s % 4 == 0 => units!(AtomicU32, 4),
        s if s % 2 == 0 => units!(AtomicU16, 2),
        _ => units!(AtomicU8, 1),
    }
}
