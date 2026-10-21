//! Allocator-level registry backing accounting; no private container layouts.
pub(crate) use super::metadata_allocator::MetadataAllocator;
use std::collections::hash_map::RandomState;
pub(super) type Map<V> = hashbrown::HashMap<usize, V, RandomState, MetadataAllocator>;
pub(super) type Set = hashbrown::HashSet<usize, RandomState, MetadataAllocator>;
pub(super) type Vec<T> = allocator_api2::vec::Vec<T, MetadataAllocator>;
pub(super) fn map<V>(capacity: usize, allocator: MetadataAllocator) -> Map<V> {
    Map::with_capacity_and_hasher_in(capacity, RandomState::new(), allocator)
}
pub(super) fn set(capacity: usize, allocator: MetadataAllocator) -> Set {
    Set::with_capacity_and_hasher_in(capacity, RandomState::new(), allocator)
}
pub(super) fn vector<T>(capacity: usize, allocator: MetadataAllocator) -> Vec<T> {
    Vec::with_capacity_in(capacity, allocator)
}
impl super::AtomicCellRegistry {
    pub(super) fn with_allocator(a: MetadataAllocator) -> Self {
        Self {
            records: map(0, a.clone()),
            owned: vector(0, a),
        }
    }
}
impl super::ChannelRegistry {
    pub(super) fn with_allocator(a: MetadataAllocator) -> Self {
        Self {
            records: map(0, a.clone()),
            unbound: set(0, a.clone()),
            owned: set(0, a),
        }
    }
}
impl super::CompletionRegistry {
    pub(super) fn with_allocator(a: MetadataAllocator) -> Self {
        Self {
            records: map(0, a.clone()),
            pending: vector(0, a.clone()),
            owned: vector(0, a),
            reaper: None,
        }
    }
}

impl Default for super::AtomicCellRegistry {
    fn default() -> Self {
        Self::with_allocator(MetadataAllocator::default())
    }
}
impl Default for super::ChannelRegistry {
    fn default() -> Self {
        Self::with_allocator(MetadataAllocator::default())
    }
}
impl Default for super::CompletionRegistry {
    fn default() -> Self {
        Self::with_allocator(MetadataAllocator::default())
    }
}

#[cfg(test)]
#[path = "native_metadata_tests.rs"]
mod tests;
