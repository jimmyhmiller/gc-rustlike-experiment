//! Owned canonical descriptors, including unused capacity and owned offset maps.
//!
//! Only shared slices escape. Appending requires exclusive Heap access. Keep
//! std::Vec's actual Global reallocation behavior; charge the observed capacity
//! change only after it succeeds. No private growth algorithm is modeled.
//! The complete graph drops before its ownership charge ends.
use super::{
    TypeInfo,
    header::{HeaderLayout, ObjHeader},
};
use crate::runtime::allocation_counter::AllocationCounter;
use std::{borrow::Cow, ops::Deref, sync::Arc};

pub(super) struct LayoutTable {
    values: Vec<TypeInfo>,
    backing: BackingOwner,
}
struct BackingOwner {
    counter: Arc<AllocationCounter>,
    bytes: usize,
}
impl Drop for BackingOwner {
    fn drop(&mut self) {
        if self.bytes != 0 {
            self.counter.release(self.bytes);
        }
    }
}
struct PendingOffsets<'a> {
    counter: &'a AllocationCounter,
    bytes: usize,
}
impl<'a> PendingOffsets<'a> {
    fn new(counter: &'a AllocationCounter, bytes: usize) -> Self {
        if bytes != 0 {
            assert!(
                counter.try_charge(bytes),
                "layout offset accounting overflow"
            );
        }
        Self { counter, bytes }
    }
}
impl Drop for PendingOffsets<'_> {
    fn drop(&mut self) {
        if self.bytes != 0 {
            self.counter.release(self.bytes);
        }
    }
}
#[inline]
fn owned_offsets(info: &TypeInfo) -> usize {
    let TypeInfo {
        type_id: _,
        header_size: _,
        value_field_count: _,
        raw_byte_count: _,
        varlen: _,
        align_log2: _,
        interior_ptrs,
        record_stride: _,
        record_ptrs,
    } = info;
    let owned = |offsets: &Cow<'static, [u16]>| match offsets {
        Cow::Borrowed(_) => 0,
        Cow::Owned(values) => values
            .capacity()
            .checked_mul(std::mem::size_of::<u16>())
            .expect("layout offset capacity overflow"),
    };
    owned(interior_ptrs).checked_add(owned(record_ptrs)).expect("layout offset capacity overflow")
}
/// Own the input while validating and inventorying it in one pass, before any
/// Heap backing is created. Private fields prevent callers from altering the
/// graph between validation and installation or supplying an invented charge.
pub(super) struct PreparedLayouts {
    values: Vec<TypeInfo>,
    offset_bytes: usize,
}
impl PreparedLayouts {
    pub(super) fn new<H: ObjHeader>(values: Vec<TypeInfo>) -> Self {
        assert!(
            values.len() <= u16::MAX as usize + 1,
            "heap type-id space exhausted"
        );
        let header = HeaderLayout::of::<H>();
        let mut offset_bytes = 0usize;
        for (index, info) in values.iter().enumerate() {
            assert_eq!(
                info.type_id as usize, index,
                "heap type id must equal its table index"
            );
            assert!(
                header.accepts(info, 0) && info.align_log2 == 3,
                "invalid heap type layout"
            );
            offset_bytes = offset_bytes
                .checked_add(owned_offsets(info))
                .expect("layout offset total overflow");
        }
        Self {
            values,
            offset_bytes,
        }
    }
}
impl LayoutTable {
    pub(super) fn from_prepared(
        prepared: PreparedLayouts,
        counter: &Arc<AllocationCounter>,
    ) -> Self {
        let PreparedLayouts {
            values,
            offset_bytes,
        } = prepared;
        let bytes = values
            .capacity()
            .checked_mul(std::mem::size_of::<TypeInfo>())
            .and_then(|outer| outer.checked_add(offset_bytes))
            .expect("layout total capacity overflow");
        let mut pending = PendingOffsets::new(counter, bytes);
        let backing = BackingOwner {
            counter: counter.clone(),
            bytes,
        };
        pending.bytes = 0;
        Self { values, backing }
    }
    #[cfg(test)]
    pub(super) fn new(values: Vec<TypeInfo>, counter: &Arc<AllocationCounter>) -> Self {
        Self::from_prepared(PreparedLayouts::new::<super::Full>(values), counter)
    }
    #[cfg(test)]
    pub(super) fn backing_bytes(&self) -> usize {
        self.values.capacity() * std::mem::size_of::<TypeInfo>()
            + self.values.iter().map(owned_offsets).sum::<usize>()
    }
    #[inline]
    pub(super) fn push(&mut self, value: TypeInfo) {
        let nested = owned_offsets(&value);
        let with_value = self
            .backing
            .bytes
            .checked_add(nested)
            .expect("layout offset total overflow");
        let mut pending = PendingOffsets::new(&self.backing.counter, nested);
        let old_capacity = self.values.capacity();
        self.values.push(value);
        self.backing.bytes = if self.values.capacity() == old_capacity {
            with_value
        } else {
            self.grown_bytes(old_capacity, with_value)
        };
        pending.bytes = 0;
    }
    #[cold]
    #[inline(never)]
    fn grown_bytes(&self, old_capacity: usize, with_value: usize) -> usize {
        // A successful realloc has replaced the old request. Use its actual
        // observed capacity, and never unwind with partially adopted backing.
        let delta = self
            .values
            .capacity()
            .checked_sub(old_capacity)
            .and_then(|n| n.checked_mul(std::mem::size_of::<TypeInfo>()))
            .unwrap_or_else(|| std::process::abort());
        let total = with_value
            .checked_add(delta)
            .unwrap_or_else(|| std::process::abort());
        if !self.backing.counter.try_charge(delta) {
            std::process::abort();
        }
        total
    }
}
impl Deref for LayoutTable {
    type Target = [TypeInfo];
    fn deref(&self) -> &[TypeInfo] {
        &self.values
    }
}
#[cfg(test)]
#[path = "layout_table_tests.rs"]
mod tests;
