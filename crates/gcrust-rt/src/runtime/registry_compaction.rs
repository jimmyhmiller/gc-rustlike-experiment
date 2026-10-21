//! Stage sparse native backing off-lock; install only after size revalidation.
use super::{
    AtomicCellRegistry, ChannelRegistry, CompletionRegistry, Heap,
    native_metadata::{self, MetadataAllocator},
};
use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};

trait Storage {
    fn len(&self) -> usize;
    fn capacity(&self) -> usize;
    fn empty(capacity: usize, allocator: MetadataAllocator) -> Self;
    fn allocator(&self) -> MetadataAllocator {
        MetadataAllocator::default()
    }
    fn transfer(&mut self, destination: &mut Self);
}
impl<T> Storage for Vec<T> {
    fn len(&self) -> usize {
        self.len()
    }
    fn capacity(&self) -> usize {
        self.capacity()
    }
    fn empty(capacity: usize, _allocator: MetadataAllocator) -> Self {
        Self::with_capacity(capacity)
    }
    fn transfer(&mut self, destination: &mut Self) {
        destination.extend(self.drain(..));
    }
}
impl<V> Storage for HashMap<usize, V> {
    fn len(&self) -> usize {
        self.len()
    }
    fn capacity(&self) -> usize {
        self.capacity()
    }
    fn empty(capacity: usize, _allocator: MetadataAllocator) -> Self {
        Self::with_capacity(capacity)
    }
    fn transfer(&mut self, destination: &mut Self) {
        for (key, value) in self.drain() {
            assert!(destination.insert(key, value).is_none());
        }
    }
}
impl Storage for HashSet<usize> {
    fn len(&self) -> usize {
        self.len()
    }
    fn capacity(&self) -> usize {
        self.capacity()
    }
    fn empty(capacity: usize, _allocator: MetadataAllocator) -> Self {
        Self::with_capacity(capacity)
    }
    fn transfer(&mut self, destination: &mut Self) {
        destination.extend(self.drain());
    }
}
impl<T> Storage for native_metadata::Vec<T> {
    fn len(&self) -> usize {
        self.len()
    }
    fn capacity(&self) -> usize {
        self.capacity()
    }
    fn empty(capacity: usize, a: MetadataAllocator) -> Self {
        native_metadata::vector(capacity, a)
    }
    fn allocator(&self) -> MetadataAllocator {
        self.allocator().clone()
    }
    fn transfer(&mut self, destination: &mut Self) {
        destination.extend(self.drain(..));
    }
}
impl<V> Storage for native_metadata::Map<V> {
    fn len(&self) -> usize {
        self.len()
    }
    fn capacity(&self) -> usize {
        self.capacity()
    }
    fn empty(capacity: usize, a: MetadataAllocator) -> Self {
        native_metadata::map(capacity, a)
    }
    fn allocator(&self) -> MetadataAllocator {
        self.allocator().clone()
    }
    fn transfer(&mut self, destination: &mut Self) {
        for (key, value) in self.drain() {
            assert!(destination.insert(key, value).is_none());
        }
    }
}
impl Storage for native_metadata::Set {
    fn len(&self) -> usize {
        self.len()
    }
    fn capacity(&self) -> usize {
        self.capacity()
    }
    fn empty(capacity: usize, a: MetadataAllocator) -> Self {
        native_metadata::set(capacity, a)
    }
    fn allocator(&self) -> MetadataAllocator {
        self.allocator().clone()
    }
    fn transfer(&mut self, destination: &mut Self) {
        destination.extend(self.drain());
    }
}
fn sparse(capacity: usize, required: usize) -> bool {
    capacity > 8 && capacity > required.saturating_mul(4)
}
fn plan<T: Storage>(storage: &T, required: usize) -> Option<usize> {
    assert!(required >= storage.len());
    sparse(storage.capacity(), required).then(|| required.saturating_mul(2))
}
struct Stage<T> {
    requested: usize,
    backing: Option<T>,
}
impl<T: Storage> Stage<T> {
    #[cfg(test)]
    fn new(requested: Option<usize>) -> Self {
        Self::in_allocator(requested, MetadataAllocator::default())
    }
    fn in_allocator(requested: Option<usize>, allocator: MetadataAllocator) -> Self {
        Self {
            requested: requested.unwrap_or(0),
            backing: requested.map(|n| T::empty(n, allocator)),
        }
    }
    fn install(&mut self, storage: &mut T, required: usize) -> bool {
        assert!(required >= storage.len());
        let Some(backing) = self.backing.as_mut() else {
            return false;
        };
        assert_eq!(backing.len(), 0);
        // Admission and another collection can both run while native allocation
        // is in progress. Never allocate on commit or install a stale peak size.
        if !sparse(storage.capacity(), required)
            || self.requested > required.saturating_add(1).saturating_mul(4)
            || backing.capacity() < required
            || !backing.allocator().same_counter(&storage.allocator())
        {
            return false;
        }
        storage.transfer(backing);
        std::mem::swap(storage, backing);
        true
    }
}

impl AtomicCellRegistry {
    fn compaction_plan(&self) -> [Option<usize>; 2] {
        [
            plan(&self.records, self.records.len()),
            plan(&self.owned, self.records.len()),
        ]
    }
    pub(crate) fn needs_compaction(&self) -> bool {
        self.compaction_plan().iter().any(Option::is_some)
    }
}
impl ChannelRegistry {
    fn compaction_plan(&self) -> [Option<usize>; 3] {
        [
            plan(&self.records, self.records.len()),
            plan(&self.owned, self.records.len()),
            plan(&self.unbound, self.unbound.len()),
        ]
    }
    pub(crate) fn needs_compaction(&self) -> bool {
        self.compaction_plan().iter().any(Option::is_some)
    }
}
impl CompletionRegistry {
    fn compaction_plan(&self) -> [Option<usize>; 3] {
        [
            plan(&self.records, self.records.len()),
            plan(&self.owned, self.records.len()),
            plan(&self.pending, self.pending.len()),
        ]
    }
    pub(crate) fn needs_compaction(&self) -> bool {
        self.compaction_plan().iter().any(Option::is_some)
    }
}

// The backing stages never contain borrowed managed addresses. All entries move
// under their registry's mutex, so concurrent GC sees either complete table.
// Old/unused backing is dropped only after that mutex guard has been released.
pub(crate) fn compact_all(heap: &Heap) {
    compact_vector(&heap.threads);
    compact_atomics(&heap.atomic_cells);
    compact_channels(&heap.channels);
    compact_completions(&heap.thread_completions);
}
pub(crate) fn vector_needs_compaction<T>(mutex: &Mutex<native_metadata::Vec<T>>) -> bool {
    let storage = mutex.lock().unwrap();
    plan(&*storage, storage.len()).is_some()
}

/// ThreadState allocations stay stable when census entries move. Stage new
/// backing outside the census lock, then revalidate before moving the entries.
/// Destroy the replaced backing after releasing the lock.
pub(crate) fn compact_vector<T>(mutex: &Mutex<native_metadata::Vec<T>>) {
    let (requested, allocator) = {
        let storage = mutex.lock().unwrap();
        (plan(&*storage, storage.len()), storage.allocator().clone())
    };
    #[cfg(test)]
    tests::checkpoint(mutex as *const _ as usize, tests::Phase::Allocate);
    let mut stage = Stage::in_allocator(requested, allocator);
    {
        let mut storage = mutex.lock().unwrap();
        let live = storage.len();
        stage.install(&mut *storage, live);
    }
    #[cfg(test)]
    tests::checkpoint(mutex as *const _ as usize, tests::Phase::Dispose);
    drop(stage);
}

pub(super) fn compact_atomics(mutex: &Mutex<AtomicCellRegistry>) {
    let (plan, allocator) = {
        let registry = mutex.lock().unwrap();
        (
            registry.compaction_plan(),
            registry.records.allocator().clone(),
        )
    };
    let [records, owned] = plan;
    #[cfg(test)]
    tests::checkpoint(mutex as *const _ as usize, tests::Phase::Allocate);
    let mut records = Stage::in_allocator(records, allocator.clone());
    let mut owned = Stage::in_allocator(owned, allocator.clone());
    {
        let mut registry = mutex.lock().unwrap();
        let live = registry.records.len();
        records.install(&mut registry.records, live);
        owned.install(&mut registry.owned, live);
    }
    #[cfg(test)]
    tests::checkpoint(mutex as *const _ as usize, tests::Phase::Dispose);
    drop((records, owned));
}
pub(super) fn compact_channels(mutex: &Mutex<ChannelRegistry>) {
    let (plan, allocator) = {
        let registry = mutex.lock().unwrap();
        (
            registry.compaction_plan(),
            registry.records.allocator().clone(),
        )
    };
    let [records, owned, unbound] = plan;
    let mut records = Stage::in_allocator(records, allocator.clone());
    let mut owned = Stage::in_allocator(owned, allocator.clone());
    let mut unbound = Stage::in_allocator(unbound, allocator.clone());
    {
        let mut registry = mutex.lock().unwrap();
        let live = registry.records.len();
        let raw = registry.unbound.len();
        records.install(&mut registry.records, live);
        owned.install(&mut registry.owned, live);
        unbound.install(&mut registry.unbound, raw);
    }
    drop((records, owned, unbound));
}
pub(super) fn compact_completions(mutex: &Mutex<CompletionRegistry>) {
    let (plan, allocator) = {
        let registry = mutex.lock().unwrap();
        (
            registry.compaction_plan(),
            registry.records.allocator().clone(),
        )
    };
    let [records, owned, pending] = plan;
    let mut records = Stage::in_allocator(records, allocator.clone());
    let mut owned = Stage::in_allocator(owned, allocator.clone());
    let mut pending = Stage::in_allocator(pending, allocator.clone());
    {
        let mut registry = mutex.lock().unwrap();
        let live = registry.records.len();
        let waiting = registry.pending.len();
        records.install(&mut registry.records, live);
        owned.install(&mut registry.owned, live);
        // Dense order and each record's pending_index remain unchanged.
        pending.install(&mut registry.pending, waiting);
    }
    drop((records, owned, pending));
}

#[cfg(test)]
#[path = "registry_compaction_tests.rs"]
mod tests;
