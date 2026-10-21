//! Accounting for immutable, installed reflection tables.
//!
//! Public metadata uses ordinary Vec/String owners. Installation moves the
//! entire graph into this private owner without changing or copying backing.
//! Only shared slices escape; no nested allocation can grow or be replaced.
//! Count actual capacities, including unused capacity, and release the charge
//! after the complete graph has physically dropped. This does not include
//! caller-owned clones, borrowed static data, or allocator implementation costs.
use super::reflect::{AllocSite, FieldMeta, FieldTy, TypeKind, TypeMeta, ValueMeta, VariantMeta};
use crate::runtime::allocation_counter::AllocationCounter;
use std::{mem::size_of, sync::Arc};

pub(super) struct InstalledTable<T> {
    // Field order is significant: Vec and every nested allocation drop before
    // BackingCharge. No mutable access or owning iterator is exposed.
    values: Vec<T>,
    _charge: BackingCharge,
}
struct BackingCharge {
    counter: Option<Arc<AllocationCounter>>,
    bytes: usize,
}
impl Drop for BackingCharge {
    fn drop(&mut self) {
        if let Some(counter) = &self.counter {
            counter.release(self.bytes);
        }
    }
}
impl<T: Backing> InstalledTable<T> {
    pub(super) fn new(values: Vec<T>, counter: &Arc<AllocationCounter>) -> Self {
        let bytes = vector_bytes(&values);
        assert!(
            counter.try_charge(bytes),
            "installed metadata accounting overflow"
        );
        Self {
            values,
            _charge: BackingCharge {
                counter: (bytes != 0).then(|| counter.clone()),
                bytes,
            },
        }
    }
}
impl<T> InstalledTable<T> {
    pub(super) fn as_slice(&self) -> &[T] {
        &self.values
    }
}
// Private to the collector. Each implementation exhaustively destructures its
// supported public type so new owning fields require an accounting decision.
pub(super) trait Backing {
    fn nested_bytes(&self) -> usize;
}
fn add(a: usize, b: usize) -> usize {
    a.checked_add(b)
        .expect("installed metadata capacity overflow")
}
fn vector_bytes<T: Backing>(values: &Vec<T>) -> usize {
    let backing = values
        .capacity()
        .checked_mul(size_of::<T>())
        .expect("installed metadata capacity overflow");
    values
        .iter()
        .fold(backing, |n, value| add(n, value.nested_bytes()))
}
impl Backing for (u16, u16) {
    fn nested_bytes(&self) -> usize {
        0
    }
}
impl Backing for AllocSite {
    fn nested_bytes(&self) -> usize {
        let Self {
            function,
            type_id: _,
            location,
        } = self;
        add(function.capacity(), location.capacity())
    }
}
impl Backing for TypeMeta {
    fn nested_bytes(&self) -> usize {
        let Self {
            type_id: _,
            name,
            kind,
        } = self;
        add(name.capacity(), kind.nested_bytes())
    }
}
impl Backing for ValueMeta {
    fn nested_bytes(&self) -> usize {
        let Self {
            value_id: _,
            name,
            kind,
        } = self;
        add(name.capacity(), kind.nested_bytes())
    }
}
impl Backing for TypeKind {
    fn nested_bytes(&self) -> usize {
        match self {
            Self::Struct { fields } => vector_bytes(fields),
            Self::Enum {
                tag_offset: _,
                variants,
            } => vector_bytes(variants),
            Self::Opaque => 0,
        }
    }
}
impl Backing for VariantMeta {
    fn nested_bytes(&self) -> usize {
        let Self {
            name,
            tag: _,
            fields,
        } = self;
        add(name.capacity(), vector_bytes(fields))
    }
}
impl Backing for FieldMeta {
    fn nested_bytes(&self) -> usize {
        let Self {
            name,
            offset: _,
            ty,
        } = self;
        add(name.capacity(), ty.nested_bytes())
    }
}
impl Backing for FieldTy {
    fn nested_bytes(&self) -> usize {
        match self {
            Self::Ref(_) | Self::Scalar(_) | Self::Value(_) => 0,
            Self::ProjectedValue {
                value_id: _,
                reference_offsets,
            } => vector_bytes(reference_offsets),
        }
    }
}

#[cfg(test)]
#[path = "installed_metadata_tests.rs"]
mod tests;
