use super::*;
use crate::runtime::{
    ChannelOwner, allocation_probe, channel_owner_tests, native_memory, native_retirement,
};
#[test]
fn collector_registry_admission_reference_channel_raw_only() {
    let retirement = native_retirement::Retirement::default();
    let memory = Arc::new(native_memory::NativeMemory::default());
    let mut registry = ChannelRegistry::default();
    for _ in 0..64 {
        let p = registry.register(channel_owner_tests::queue(0, &retirement), &memory);
        registry.bind(
            p,
            ChannelOwner {
                address: 8,
                type_id: 1,
            },
        );
    }
    registry.unbound.shrink_to_fit();
    let address = registry
        .records
        .iter()
        .next()
        .map(|(&key, value)| (key, value as *const _ as usize))
        .unwrap();
    let owner = registry.owned.iter().next().copied().unwrap();
    let owner_address = registry.owned.get(&owner).unwrap() as *const _ as usize;
    let plan = registry.growth_plan();
    assert_eq!(plan, [0, 2, 0]);
    let mut stage = stage_plan(plan, 0);
    assert_eq!(stage.tables.records.capacity(), 0);
    assert_eq!(stage.tables.owned.capacity(), 0);
    let channel = channel_owner_tests::queue(0, &retirement);
    allocation_probe::start();
    assert!(registry.install(&mut stage));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(
        &registry.records[&address.0] as *const _ as usize,
        address.1
    );
    assert_eq!(
        registry.owned.get(&owner).unwrap() as *const _ as usize,
        owner_address
    );
    allocation_probe::start();
    let p = registry.register_reserved(channel, &memory);
    registry.bind(
        p,
        ChannelOwner {
            address: 16,
            type_id: 1,
        },
    );
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    registry.clear();
    retirement.drain();
    assert_eq!(memory.bytes(), 0);
}
#[test]
fn collector_registry_admission_reference_channel_owned_only() {
    let retirement = native_retirement::Retirement::default();
    let memory = Arc::new(native_memory::NativeMemory::default());
    let mut registry = ChannelRegistry::default();
    for _ in 0..64 {
        let p = registry.register(channel_owner_tests::queue(0, &retirement), &memory);
        registry.bind(
            p,
            ChannelOwner {
                address: 8,
                type_id: 1,
            },
        );
    }
    while registry.owned.len() < registry.owned.capacity() {
        let p = registry.register(channel_owner_tests::queue(0, &retirement), &memory);
        registry.bind(
            p,
            ChannelOwner {
                address: 8,
                type_id: 1,
            },
        );
    }
    registry.records.reserve(1);
    registry.unbound.reserve(1);
    registry.owned.shrink_to_fit();
    assert_eq!(registry.owned.capacity(), registry.records.len());
    let address = registry
        .records
        .iter()
        .next()
        .map(|(&key, value)| (key, value as *const _ as usize))
        .unwrap();
    let plan = registry.growth_plan();
    assert_eq!(plan[0..2], [0, 0]);
    assert!(plan[2] > 0);
    let mut stage = stage_plan(plan, 0);
    assert_eq!(stage.tables.records.capacity(), 0);
    assert_eq!(stage.tables.unbound.capacity(), 0);
    allocation_probe::start();
    assert!(registry.install(&mut stage));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(
        &registry.records[&address.0] as *const _ as usize,
        address.1
    );
    registry.clear();
    retirement.drain();
    assert_eq!(memory.bytes(), 0);
}
#[test]
fn collector_registry_admission_reference_channel_records_only() {
    let retirement = native_retirement::Retirement::default();
    let memory = Arc::new(native_memory::NativeMemory::default());
    let mut registry = ChannelRegistry::default();
    for _ in 0..64 {
        registry.register(channel_owner_tests::queue(0, &retirement), &memory);
    }
    while registry.records.len() < registry.records.capacity() {
        registry.register(channel_owner_tests::queue(0, &retirement), &memory);
    }
    registry.unbound.reserve(1);
    registry.owned.reserve(registry.records.len() + 1);
    let key = *registry.unbound.iter().next().unwrap();
    let address = registry.unbound.get(&key).unwrap() as *const _ as usize;
    let plan = registry.growth_plan();
    assert!(plan[0] > 0);
    assert_eq!(plan[1..], [0, 0]);
    let mut stage = stage_plan(plan, 0);
    allocation_probe::start();
    assert!(registry.install(&mut stage));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(
        registry.unbound.get(&key).unwrap() as *const _ as usize,
        address
    );
    registry.clear();
    retirement.drain();
    assert_eq!(memory.bytes(), 0);
}
#[test]
fn collector_registry_admission_reference_channel_preflight_is_all_or_nothing() {
    let retirement = native_retirement::Retirement::default();
    let memory = Arc::new(native_memory::NativeMemory::default());
    let mut registry = ChannelRegistry::default();
    for _ in 0..64 {
        registry.register(channel_owner_tests::queue(0, &retirement), &memory);
    }
    while registry.records.len() < registry.records.capacity() {
        registry.register(channel_owner_tests::queue(0, &retirement), &memory);
    }
    // With raw membership also exhausted, a record-only stage is insufficient.
    registry.unbound.shrink_to_fit();
    let live = registry.records.len();
    let plan = [growth_request(0, live), 0, 0];
    let mut stage = stage_plan(plan, 0);
    assert!(registry.unbound.capacity() <= registry.unbound.len());
    let (key, address) = registry
        .records
        .iter()
        .next()
        .map(|(&k, v)| (k, v as *const _ as usize))
        .unwrap();
    allocation_probe::start();
    assert!(!registry.install(&mut stage));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(&registry.records[&key] as *const _ as usize, address);
    assert!(stage.tables.records.is_empty());
    registry.clear();
    retirement.drain();
    assert_eq!(memory.bytes(), 0);
}
#[test]
fn collector_registry_admission_reference_channel_raw_history_reserves_future_owners() {
    let retirement = native_retirement::Retirement::default();
    let memory = Arc::new(native_memory::NativeMemory::default());
    let mut registry = ChannelRegistry::default();
    let mut pointers = Vec::new();
    for _ in 0..64 {
        pointers.push(registry.register(channel_owner_tests::queue(0, &retirement), &memory));
    }
    while registry.records.len() < registry.records.capacity() {
        pointers.push(registry.register(channel_owner_tests::queue(0, &retirement), &memory));
    }
    registry.records.reserve(1);
    registry.unbound.reserve(1);
    assert!(registry.owned.is_empty());
    assert_eq!(registry.owned.capacity(), registry.records.len());
    let live = registry.records.len();
    let plan = registry.growth_plan();
    assert_eq!(plan, [0, 0, 2 * (live + 1)]);
    let mut stage = stage_plan(plan, 0);
    let extra = channel_owner_tests::queue(0, &retirement);
    allocation_probe::start();
    assert!(registry.install(&mut stage));
    let p = registry.register_reserved(extra, &memory);
    registry.bind(
        p,
        ChannelOwner {
            address: 8,
            type_id: 1,
        },
    );
    for &p in &pointers {
        registry.bind(
            p,
            ChannelOwner {
                address: 8,
                type_id: 1,
            },
        );
    }
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(registry.owned.len(), live + 1);
    assert!(registry.unbound.is_empty());
    registry.clear();
    retirement.drain();
    assert_eq!(memory.bytes(), 0);
}
#[test]
fn collector_native_metadata_reference_channel_rejects_foreign_counter() {
    let a = Arc::<crate::runtime::allocation_counter::AllocationCounter>::default();
    let b = Arc::<crate::runtime::allocation_counter::AllocationCounter>::default();
    let retirement = native_retirement::Retirement::default();
    let memory = Arc::new(native_memory::NativeMemory::default());
    let mut registry = ChannelRegistry::with_allocator(MetadataAllocator::new(&a));
    for _ in 0..64 {
        let p = registry.register(channel_owner_tests::queue(0, &retirement), &memory);
        registry.bind(
            p,
            ChannelOwner {
                address: 8,
                type_id: 1,
            },
        );
    }
    registry.unbound.shrink_to_fit();
    let before = a.bytes();
    let mut stage = stage_in(registry.growth_plan(), MetadataAllocator::new(&b), 0);
    assert!(b.bytes() > 0);
    allocation_probe::start();
    assert!(!registry.install(&mut stage));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(a.bytes(), before);
    drop(stage);
    assert_eq!(b.bytes(), 0);
    registry.clear();
    retirement.drain();
    assert_eq!(a.bytes(), 0);
    assert_eq!(memory.bytes(), 0);
}
