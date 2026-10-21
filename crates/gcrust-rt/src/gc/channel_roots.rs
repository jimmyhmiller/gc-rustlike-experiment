//! Conditional native edges, activated by discovery of their managed owner.
use super::*;
use crate::runtime::{ChanCtrl, ChannelOwner};
#[path = "channel_plan.rs"]
mod storage;
use storage::{OwnerEdges, Plan, Records};

/// Capture native owners under the registry guard. Constructors sort the
/// snapshot after guard release before grouping or reading managed headers.
/// Only native metadata is read under the registry guard; no managed callbacks or
/// transitions or copy workers run until that guard has been released.
fn owned_channels<'a>(
    heap: &'a Heap,
    reserve_included: bool,
    include: impl Fn(ChannelOwner, &ChanCtrl) -> bool,
) -> Records<'a, Arc<ChanCtrl>> {
    let registry = heap.channels.lock().unwrap();
    // The predicate reads only paused metadata. Count first so an entirely
    // clean minor snapshot reserves nothing rather than historical capacity.
    let count = if reserve_included {
        registry
            .owned_iter()
            .filter(|(owner, channel)| include(*owner, channel))
            .count()
    } else {
        registry.owned_iter().len()
    };
    let mut records = Records::with_capacity_in(count, heap.native_memory.registry_allocator());
    for (owner, channel) in registry.owned_iter() {
        if include(owner, channel) {
            records.push((owner, channel.clone()));
        }
    }
    drop(registry);
    records
}
struct ChannelGroup {
    owner: usize,
    end: usize,
    source: bool,
}
struct InitialGroups<'a> {
    groups: std::iter::Enumerate<std::slice::Iter<'a, ChannelGroup>>,
    remaining: usize,
}
impl Iterator for InitialGroups<'_> {
    type Item = usize;
    fn next(&mut self) -> Option<usize> {
        if self.remaining == 0 {
            return None;
        }
        for (index, group) in self.groups.by_ref() {
            if !group.source {
                self.remaining -= 1;
                return Some(index);
            }
        }
        debug_assert_eq!(self.remaining, 0);
        None
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}
impl ExactSizeIterator for InitialGroups<'_> {}

pub(crate) struct ConditionalChannels<'a> {
    plan: Plan<'a, Arc<ChanCtrl>>,
}
impl<'a> ConditionalChannels<'a> {
    /// World stopped, no copy workers running. Uncollected generations are
    /// conservative roots; an exact combined-pause nursery filter overrides
    /// that conservatism before old-space collection followed by minor GC.
    pub unsafe fn new(
        heap: &'a Heap,
        minor: bool,
        nursery_live: Option<&crate::gc::heap::NurseryLive<'_>>,
    ) -> Self {
        let records = owned_channels(heap, minor, |owner, channel| {
            (!minor || channel.needs_minor_scan())
                && (minor
                    || !heap.is_nursery(owner.address as *mut u8)
                    || nursery_live.is_none_or(|live| live.contains(&owner.address)))
        });
        let plan = Plan::new(records, || heap.type_table_len(), |owner| {
            let address = owner.address as *mut u8;
            let in_source = if minor { heap.is_nursery(address) } else { heap.from_space().contains(address) };
            in_source && unsafe { heap.check_forwarded(address) }.is_none()
        }, heap.native_memory.registry_allocator());
        Self { plan }
    }
    pub fn initial_groups(&self) -> impl ExactSizeIterator<Item = usize> + '_ { self.plan.initial_groups() }
    pub fn source_group(&self, object: *mut u8, type_id: u16) -> Option<usize> { self.plan.source_group(object as usize, type_id) }
    pub fn has_source_owners(&self) -> bool { self.plan.has_source_owners() }
    /// Storage remains stable throughout the pause. Only winning source-owner
    /// claims activate source groups; initial groups are disjoint from them.
    pub unsafe fn scan(&self, index: usize, visit: &mut dyn FnMut(*mut u64)) {
        for channel in self.plan.group_channels(index) {
            unsafe { channel.scan_roots(visit); }
        }
    }
}

/// Read-only owner edges for preflights and heap inspection.
pub(crate) struct ChannelEdges<'a> {
    plan: OwnerEdges<'a, Arc<ChanCtrl>>,
}
impl<'a> ChannelEdges<'a> {
    pub fn new(heap: &'a Heap) -> Self {
        Self { plan: OwnerEdges::new(owned_channels(heap, false, |_, _| true), heap.native_memory.registry_allocator()) }
    }
    pub unsafe fn trace(&self, owner: *mut u8, visit: &mut dyn FnMut(*mut u64)) {
        if let Some(channels) = self.plan.channels_for(owner as usize) {
            for channel in channels { unsafe { channel.scan_roots(visit); } }
        }
    }
    /// A minor retains every old owner, even a logically unreachable one.
    pub unsafe fn scan_old_roots(&self, heap: &Heap, visit: &mut dyn FnMut(*mut u64)) {
        for (owner, channels) in self.plan.groups() {
            if !heap.is_nursery(owner as *mut u8) {
                for channel in channels { unsafe { channel.scan_roots(visit); } }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_channel_groups_reference_initial_iterator_exact_size() {
        for mask in 0..16 {
            let groups: Vec<_> = (0..4)
                .map(|index| ChannelGroup {
                    owner: index,
                    end: index + 1,
                    source: mask & (1 << index) != 0,
                })
                .collect();
            let expected: Vec<_> = (0..4).filter(|index| mask & (1 << index) == 0).collect();
            let mut iterator = InitialGroups {
                groups: groups.iter().enumerate(),
                remaining: expected.len(),
            };
            for (offset, &index) in expected.iter().enumerate() {
                assert_eq!(iterator.len(), expected.len() - offset);
                assert_eq!(iterator.size_hint(), (iterator.len(), Some(iterator.len())));
                assert_eq!(iterator.next(), Some(index));
            }
            assert_eq!(iterator.len(), 0);
            assert_eq!(iterator.next(), None);
            assert_eq!(iterator.next(), None);
        }
    }
}
