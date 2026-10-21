use super::*;
use crate::runtime::{allocation_probe, native_retirement};
fn filled(retirement: &native_retirement::Retirement, count: usize) -> AtomicCellRegistry {
    let mut registry = AtomicCellRegistry::default();
    for i in 0..count {
        let p = registry.register(native_retirement::AtomicCell::new(i as i64, retirement));
        registry.bind(p, 8usize as *mut u8);
    }
    registry
}
#[test]
fn collector_registry_admission_reference_atomic_owned_only() {
    let retirement = native_retirement::Retirement::default();
    let mut registry = filled(&retirement, 64);
    registry.owned.shrink_to_fit();
    let address = registry
        .records
        .iter()
        .next()
        .map(|(&key, value)| (key, value as *const _ as usize))
        .unwrap();
    let plan = registry.growth_plan();
    assert_eq!(plan, [0, 130]);
    allocation_probe::start();
    let mut stage = stage_plan(plan, 0);
    assert_eq!(
        allocation_probe::finish().0,
        1,
        "only the owned vector needs backing"
    );
    assert_eq!(stage.tables.records.capacity(), 0);
    assert!(stage.tables.owned.capacity() >= 130);
    allocation_probe::start();
    assert!(registry.install(&mut stage));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(
        &registry.records[&address.0] as *const _ as usize,
        address.1
    );
    let cell = native_retirement::AtomicCell::new(99, &retirement);
    allocation_probe::start();
    let pointer = registry.register_reserved(cell);
    registry.bind(pointer, 16usize as *mut u8);
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    registry.clear();
    retirement.drain();
}
#[test]
fn collector_registry_admission_reference_atomic_records_only() {
    let retirement = native_retirement::Retirement::default();
    let mut registry = filled(&retirement, 64);
    while registry.records.len() < registry.records.capacity() {
        let p = registry.register(native_retirement::AtomicCell::new(0, &retirement));
        registry.bind(p, 8usize as *mut u8);
    }
    registry.owned.reserve(1);
    let owned = registry.owned.as_ptr();
    let order = registry.owned.clone();
    let plan = registry.growth_plan();
    assert!(plan[0] > 0);
    assert_eq!(plan[1..], [0]);
    let mut stage = stage_plan(plan, 0);
    assert_eq!(stage.tables.owned.capacity(), 0);
    allocation_probe::start();
    assert!(registry.install(&mut stage));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(registry.owned.as_ptr(), owned);
    assert_eq!(registry.owned, order);
    registry.clear();
    retirement.drain();
}
#[test]
fn collector_registry_admission_reference_atomic_missing_component_retries_without_partial_commit()
{
    let retirement = native_retirement::Retirement::default();
    let mut registry = filled(&retirement, 64);
    registry.owned.shrink_to_fit();
    let mut stage = stage_plan(registry.growth_plan(), 0);
    while registry.records.len() < registry.records.capacity() {
        registry.register(native_retirement::AtomicCell::new(0, &retirement));
    }
    let owned = registry.owned.as_ptr();
    allocation_probe::start();
    assert!(!registry.install(&mut stage));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(registry.owned.as_ptr(), owned);
    assert!(stage.tables.owned.is_empty());
    let mut retry = stage_plan(registry.growth_plan(), 0);
    assert!(registry.install(&mut retry));
    assert!(registry.can_register());
    registry.clear();
    retirement.drain();
}
#[test]
fn collector_registry_admission_reference_atomic_preflight_is_all_or_nothing() {
    let retirement = native_retirement::Retirement::default();
    let mut registry = filled(&retirement, 64);
    while registry.records.len() < registry.records.capacity() {
        let p = registry.register(native_retirement::AtomicCell::new(0, &retirement));
        registry.bind(p, 8usize as *mut u8);
    }
    registry.owned.shrink_to_fit();
    let plan = registry.growth_plan();
    assert!(plan[0] > 0 && plan[1] > 0);
    // Simulate a newly exhausted component absent from the earlier snapshot.
    let mut stage = stage_plan([plan[0], 0], 0);
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
}
#[test]
fn collector_registry_admission_reference_atomic_raw_history_reserves_future_owners() {
    let retirement = native_retirement::Retirement::default();
    let mut registry = AtomicCellRegistry::default();
    let mut pointers = Vec::new();
    for _ in 0..64 {
        pointers.push(registry.register(native_retirement::AtomicCell::new(0, &retirement)));
    }
    assert!(registry.owned.is_empty());
    assert_eq!(registry.owned.capacity(), registry.records.len());
    let plan = registry.growth_plan();
    assert_eq!(plan, [0, 130]);
    let mut stage = stage_plan(plan, 0);
    let extra = native_retirement::AtomicCell::new(42, &retirement);
    allocation_probe::start();
    assert!(registry.install(&mut stage));
    let p = registry.register_reserved(extra);
    registry.bind(p, 8usize as *mut u8);
    for &p in &pointers {
        registry.bind(p, 8usize as *mut u8);
    }
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(registry.owned.len(), 65);
    registry.clear();
    retirement.drain();
}
#[test]
fn collector_native_metadata_reference_atomic_rejects_foreign_counter() {
    let a = Arc::<crate::runtime::allocation_counter::AllocationCounter>::default();
    let b = Arc::<crate::runtime::allocation_counter::AllocationCounter>::default();
    let retirement = native_retirement::Retirement::default();
    let mut registry = AtomicCellRegistry::with_allocator(MetadataAllocator::new(&a));
    for _ in 0..64 {
        registry.register(native_retirement::AtomicCell::new(0, &retirement));
    }
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
}
