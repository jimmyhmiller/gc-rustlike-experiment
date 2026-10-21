//! SATB chunk ownership across standalone and accounted buffer allocators.
use crate::runtime::metadata_allocator::MetadataAllocator;
use allocator_api2::vec::Vec as AllocVec;

pub(crate) type NativeValues = AllocVec<u64, MetadataAllocator>;

#[derive(Debug)]
pub(crate) enum SatbChunk {
    Standalone(Vec<u64>),
    Accounted(NativeValues),
}
impl SatbChunk {
    pub(super) fn capacity(&self) -> usize {
        match self { Self::Standalone(values) => values.capacity(), Self::Accounted(values) => values.capacity() }
    }
    pub(super) fn push(&mut self, value: u64) {
        match self { Self::Standalone(values) => values.push(value), Self::Accounted(values) => values.push(value) }
    }
}
impl std::ops::Deref for SatbChunk {
    type Target = [u64];
    fn deref(&self) -> &[u64] {
        match self { Self::Standalone(values) => values.as_slice(), Self::Accounted(values) => values.as_slice() }
    }
}
pub(crate) enum ChunkIter {
    Standalone(std::vec::IntoIter<u64>),
    Accounted(allocator_api2::vec::IntoIter<u64, MetadataAllocator>),
}
impl Iterator for ChunkIter {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        match self { Self::Standalone(values) => values.next(), Self::Accounted(values) => values.next() }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self { Self::Standalone(values) => values.size_hint(), Self::Accounted(values) => values.size_hint() }
    }
}
impl IntoIterator for SatbChunk {
    type Item = u64;
    type IntoIter = ChunkIter;
    fn into_iter(self) -> Self::IntoIter {
        match self {
            Self::Standalone(values) => ChunkIter::Standalone(values.into_iter()),
            Self::Accounted(values) => ChunkIter::Accounted(values.into_iter()),
        }
    }
}

// Monomorphized publication keeps the barrier's local push free of variant
// dispatch. Ownership transfers retain the allocator that owns each backing.
pub(super) trait SatbInput {
    fn values(&self) -> &[u64];
    fn clear(&mut self);
    fn discard_prefix(&mut self, count: usize);
    fn take_owned(&mut self) -> SatbChunk;
}
impl SatbInput for Vec<u64> {
    fn values(&self) -> &[u64] { self.as_slice() }
    fn clear(&mut self) { Vec::clear(self); }
    fn discard_prefix(&mut self, count: usize) { drop(self.drain(..count)); }
    fn take_owned(&mut self) -> SatbChunk { SatbChunk::Standalone(std::mem::take(self)) }
}
impl SatbInput for NativeValues {
    fn values(&self) -> &[u64] { self.as_slice() }
    fn clear(&mut self) { NativeValues::clear(self); }
    fn discard_prefix(&mut self, count: usize) { drop(self.drain(..count)); }
    fn take_owned(&mut self) -> SatbChunk {
        let empty = NativeValues::new_in(self.allocator().clone());
        SatbChunk::Accounted(std::mem::replace(self, empty))
    }
}

impl<'a> IntoIterator for &'a SatbChunk {
    type Item = &'a u64;
    type IntoIter = std::slice::Iter<'a, u64>;
    fn into_iter(self) -> Self::IntoIter { self.iter() }
}
