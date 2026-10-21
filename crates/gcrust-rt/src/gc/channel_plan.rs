//! Collection-scoped native channel metadata, separate from managed header reads.
use super::{ChannelGroup, InitialGroups};
use crate::runtime::{ChannelOwner, metadata_allocator::MetadataAllocator};
use allocator_api2::vec::Vec as NativeVec;
use std::{hash::RandomState, ops::Range};

type Buffer<'a, T> = NativeVec<T, &'a MetadataAllocator>;
type Map<'a, V> = hashbrown::HashMap<usize, V, RandomState, &'a MetadataAllocator>;
pub(super) type Records<'a, C> = Buffer<'a, (ChannelOwner, C)>;

fn owner_count<C>(records: &[(ChannelOwner, C)]) -> usize {
    usize::from(!records.is_empty())
        + records.windows(2).filter(|pair| pair[0].0.address != pair[1].0.address).count()
}

pub(super) struct Plan<'a, C> {
    types: Buffer<'a, bool>,
    source: Map<'a, usize>,
    groups: Buffer<'a, ChannelGroup>,
    channels: Buffer<'a, C>,
}
impl<'a, C> Plan<'a, C> {
    pub(super) fn new(
        mut records: Records<'a, C>,
        type_count: impl Fn() -> usize,
        mut is_source: impl FnMut(ChannelOwner) -> bool,
        allocator: &'a MetadataAllocator,
    ) -> Self {
        records.sort_unstable_by_key(|(owner, _)| owner.address);
        let mut types = Buffer::new_in(allocator);
        let mut groups = Buffer::<ChannelGroup>::with_capacity_in(owner_count(&records), allocator);
        let mut channels = Buffer::with_capacity_in(records.len(), allocator);
        let mut source_count = 0;
        let mut previous = None;
        for (owner, channel) in records {
            if previous != Some(owner.address) {
                let source = is_source(owner);
                groups.push(ChannelGroup { owner: owner.address, end: channels.len(), source });
                if source {
                    if types.is_empty() { types.resize(type_count(), false); }
                    types[owner.type_id as usize] = true;
                    source_count += 1;
                }
                previous = Some(owner.address);
            }
            channels.push(channel);
            groups.last_mut().unwrap().end = channels.len();
        }
        // The consumed snapshot is physically released before source-table
        // allocation. Initial/forwarded owners require no source lookup entry.
        let mut source = Map::with_capacity_and_hasher_in(source_count, RandomState::new(), allocator);
        for (index, group) in groups.iter().enumerate() {
            if group.source { source.insert(group.owner, index); }
        }
        Self { types, source, groups, channels }
    }

    pub(super) fn initial_groups(&self) -> impl ExactSizeIterator<Item = usize> + '_ {
        InitialGroups { groups: self.groups.iter().enumerate(), remaining: self.groups.len() - self.source.len() }
    }
    pub(super) fn source_group(&self, object: usize, type_id: u16) -> Option<usize> {
        if !self.types.get(type_id as usize).copied().unwrap_or(false) { return None; }
        self.source.get(&object).copied()
    }
    pub(super) fn has_source_owners(&self) -> bool { !self.source.is_empty() }
    pub(super) fn group_channels(&self, index: usize) -> &[C] {
        let start = if index == 0 { 0 } else { self.groups[index - 1].end };
        &self.channels[start..self.groups[index].end]
    }
}

pub(super) struct OwnerEdges<'a, C> {
    owners: Map<'a, Range<usize>>,
    channels: Buffer<'a, C>,
}
impl<'a, C> OwnerEdges<'a, C> {
    pub(super) fn new(mut records: Records<'a, C>, allocator: &'a MetadataAllocator) -> Self {
        records.sort_unstable_by_key(|(owner, _)| owner.address);
        let mut result = Self {
            owners: Map::with_capacity_and_hasher_in(owner_count(&records), RandomState::new(), allocator),
            channels: Buffer::with_capacity_in(records.len(), allocator),
        };
        let mut previous = None;
        let mut start = 0;
        for (owner, channel) in records {
            if previous != Some(owner.address) {
                if let Some(address) = previous { result.owners.insert(address, start..result.channels.len()); }
                start = result.channels.len();
                previous = Some(owner.address);
            }
            result.channels.push(channel);
        }
        if let Some(address) = previous { result.owners.insert(address, start..result.channels.len()); }
        result
    }
    pub(super) fn channels_for(&self, owner: usize) -> Option<&[C]> {
        self.owners.get(&owner).map(|range| &self.channels[range.clone()])
    }
    pub(super) fn groups(&self) -> impl Iterator<Item = (usize, &[C])> + '_ {
        self.owners.iter().map(|(&owner, range)| (owner, &self.channels[range.clone()]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{allocation_counter::AllocationCounter, allocation_probe};
    use std::sync::Arc;

    fn records<'a>(values: &[Arc<usize>], allocator: &'a MetadataAllocator) -> Records<'a, Arc<usize>> {
        let mut result = Records::with_capacity_in(values.len(), allocator);
        for (index, value) in values.iter().enumerate() {
            let group = index % 3;
            result.push((ChannelOwner { address: 16 + 8 * group, type_id: group as u16 + 1 }, value.clone()));
        }
        result
    }
    fn ids(channels: &[Arc<usize>]) -> Vec<usize> {
        let mut result: Vec<_> = channels.iter().map(|value| **value).collect();
        result.sort_unstable();
        result
    }

    #[test]
    fn collector_channel_scratch_empty_allocates_and_clones_nothing() {
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        allocation_probe::start();
        let plan = Plan::<Arc<usize>>::new(Records::new_in(&allocator), || panic!("empty type callback"), |_| panic!("empty source callback"), &allocator);
        let edges = OwnerEdges::<Arc<usize>>::new(Records::new_in(&allocator), &allocator);
        assert!(!plan.has_source_owners());
        assert_eq!(plan.initial_groups().len(), 0);
        assert_eq!(edges.groups().count(), 0);
        drop((plan, edges));
        assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 2);
    }

    #[test]
    fn collector_channel_scratch_plan_tracks_backing_and_snapshot_release() {
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let values: Vec<_> = (0..17).map(Arc::new).collect();
        let input = records(&values, &allocator);
        let before = counter.bytes();
        let mut source_calls = 0;
        allocation_probe::start();
        let plan = Plan::new(input, || 4, |owner| { source_calls += 1; owner.address != 16 }, &allocator);
        let requests = allocation_probe::finish_bytes();
        assert_eq!(source_calls, 3);
        assert_eq!(counter.bytes() + requests.3, before + requests.2);
        let pre_lookup = plan.types.capacity() + plan.groups.capacity() * std::mem::size_of::<ChannelGroup>()
            + plan.channels.capacity() * std::mem::size_of::<Arc<usize>>();
        assert_eq!(counter.peak(), (before + pre_lookup).max(counter.bytes()), "snapshot must be freed before source lookup allocation");
        assert!(counter.bytes() > 0);
        assert_eq!(Arc::strong_count(&counter), 2);
        let mut initial = plan.initial_groups();
        assert_eq!(initial.len(), 1);
        assert_eq!(initial.next(), Some(0));
        assert_eq!(initial.len(), 0);
        assert_eq!(initial.next(), None);
        assert_eq!(initial.next(), None);
        assert!(plan.source_group(16, 1).is_none());
        assert!(plan.source_group(24, 1).is_none());
        assert_eq!(plan.source_group(24, 2), Some(1));
        assert_eq!(plan.source_group(32, 3), Some(2));
        for group in 0..3 { assert_eq!(ids(plan.group_channels(group)), (0..17).filter(|index| index % 3 == group).collect::<Vec<_>>()); }
        assert!(values.iter().all(|value| Arc::strong_count(value) == 2));
        drop(initial);
        drop(plan);
        assert_eq!(counter.bytes(), 0);
        assert!(values.iter().all(|value| Arc::strong_count(value) == 1));
    }

    #[test]
    fn collector_channel_scratch_edges_track_backing_and_exact_membership() {
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let values: Vec<_> = (0..17).map(Arc::new).collect();
        let input = records(&values, &allocator);
        let before = counter.bytes();
        allocation_probe::start();
        let edges = OwnerEdges::new(input, &allocator);
        let requests = allocation_probe::finish_bytes();
        assert_eq!(counter.bytes() + requests.3, before + requests.2);
        assert_eq!(counter.peak(), before + requests.2);
        assert_eq!(Arc::strong_count(&counter), 2);
        assert!(edges.channels_for(99).is_none());
        assert_eq!(edges.groups().count(), 3);
        for group in 0..3 {
            assert_eq!(ids(edges.channels_for(16 + 8 * group).unwrap()), (0..17).filter(|index| index % 3 == group).collect::<Vec<_>>());
        }
        drop(edges);
        assert_eq!(counter.bytes(), 0);
        assert!(values.iter().all(|value| Arc::strong_count(value) == 1));
    }

    #[test]
    fn collector_channel_scratch_partial_construction_unwind_releases_owners() {
        let counter = Arc::<AllocationCounter>::default();
        let allocator = MetadataAllocator::new(&counter);
        let values: Vec<_> = (0..17).map(Arc::new).collect();
        let input = records(&values, &allocator);
        let result = std::panic::catch_unwind(|| {
            let _plan = Plan::new(input, || 4, |owner| {
                assert_ne!(owner.address, 24, "source classifier fixture");
                true
            }, &allocator);
        });
        assert!(result.is_err());
        assert_eq!(counter.bytes(), 0);
        assert_eq!(Arc::strong_count(&counter), 2);
        assert!(values.iter().all(|value| Arc::strong_count(value) == 1));
    }
}
