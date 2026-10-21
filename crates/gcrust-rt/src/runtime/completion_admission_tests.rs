use super::*;
use crate::runtime::{JoinState, allocation_probe, native_retirement};
fn record(value: u64) -> Arc<JoinHandle> {
    Arc::new(JoinHandle {
        completion_notice: false,
        native_joined: std::sync::atomic::AtomicBool::new(true),
        pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
        reaper_registration: std::sync::OnceLock::new(),
        retirement: std::sync::OnceLock::new(),
        state: Mutex::new(JoinState::Complete(value)),
        owner: std::sync::atomic::AtomicUsize::new(1),
    })
}
#[test]
fn collector_registry_admission_reference_completion_pending_only() {
    let retirement = native_retirement::Retirement::default();
    let mut registry = CompletionRegistry::default();
    for i in 0..257 {
        registry.register(record(i), &retirement);
    }
    while registry.next_pending().is_some() {}
    registry.pending.shrink_to_fit();
    let address = registry
        .records
        .iter()
        .next()
        .map(|(&key, value)| (key, value as *const _ as usize))
        .unwrap();
    let owned = registry.owned.as_ptr();
    let plan = registry.growth_plan();
    assert_eq!(plan, [0, 2, 0]);
    let mut stage = RegistryStage::components(plan, Tables::components(plan));
    assert_eq!(stage.tables.records.capacity(), 0);
    assert_eq!(stage.tables.owned.capacity(), 0);
    allocation_probe::start();
    assert!(registry.install(&mut stage));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(
        &registry.records[&address.0] as *const _ as usize,
        address.1
    );
    assert_eq!(registry.owned.as_ptr(), owned);
    let extra = record(42);
    extra
        .retirement
        .get_or_init(|| crate::runtime::native_retirement::RecordLink::new(&retirement));
    allocation_probe::start();
    registry.register_reserved(extra);
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    let pending = registry.next_pending().unwrap();
    assert_eq!(pending.join(), 42);
    assert!(registry.next_pending().is_none());
    drop(pending);
    registry.clear();
    retirement.drain();
}
#[test]
fn collector_registry_admission_reference_completion_component_budget_is_independent() {
    let retirement = native_retirement::Retirement::default();
    let mut registry = CompletionRegistry::default();
    for i in 0..1024 {
        registry.register(record(i), &retirement);
    }
    while registry.pending.len() > 64 {
        registry.next_pending();
    }
    registry.pending.shrink_to_fit();
    registry.owned.reserve(registry.records.len() + 1);
    let plan = registry.growth_plan();
    assert_eq!(plan, [0, 130, 0]);
    let mut stage = RegistryStage::components(plan, Tables::components(plan));
    while registry.next_pending().is_some() {}
    // A competing compactor may release the now-empty pending backing.
    registry.pending.shrink_to_fit();
    allocation_probe::start();
    assert!(
        !registry.install(&mut stage),
        "large record count must not excuse a stale pending request"
    );
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(registry.records.len(), 1024);
    assert!(stage.tables.pending.is_empty());
    registry.clear();
    retirement.drain();
}
#[test]
fn collector_registry_admission_reference_completion_owned_only() {
    let retirement = native_retirement::Retirement::default();
    let mut registry = CompletionRegistry::default();
    for i in 0..64 {
        let r = record(i);
        let p = Arc::as_ptr(&r) as *mut JoinHandle;
        registry.register(r, &retirement);
        registry.bind(p, 8usize as *mut u8);
    }
    registry.owned.shrink_to_fit();
    registry.pending.reserve(1);
    let pending = registry.pending.as_ptr();
    let order = registry.pending.clone();
    let (key, address) = registry
        .records
        .iter()
        .next()
        .map(|(&k, v)| (k, v as *const _ as usize))
        .unwrap();
    let plan = registry.growth_plan();
    assert_eq!(plan, [0, 0, 130]);
    let mut stage = RegistryStage::components(plan, Tables::components(plan));
    allocation_probe::start();
    assert!(registry.install(&mut stage));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(&registry.records[&key] as *const _ as usize, address);
    assert_eq!(registry.pending.as_ptr(), pending);
    assert_eq!(registry.pending, order);
    registry.clear();
    retirement.drain();
}
#[test]
fn collector_registry_admission_reference_completion_records_only() {
    let retirement = native_retirement::Retirement::default();
    let mut registry = CompletionRegistry::default();
    for i in 0..64 {
        registry.register(record(i), &retirement);
    }
    while registry.records.len() < registry.records.capacity() {
        registry.register(record(42), &retirement);
    }
    registry.pending.reserve(1);
    registry.owned.reserve(registry.records.len() + 1);
    let pending = registry.pending.as_ptr();
    let owned = registry.owned.as_ptr();
    let order = registry.pending.clone();
    let plan = registry.growth_plan();
    assert!(plan[0] > 0);
    assert_eq!(plan[1..], [0, 0]);
    let mut stage = RegistryStage::components(plan, Tables::components(plan));
    allocation_probe::start();
    assert!(registry.install(&mut stage));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(registry.pending.as_ptr(), pending);
    assert_eq!(registry.owned.as_ptr(), owned);
    assert_eq!(registry.pending, order);
    for (index, key) in registry.pending.iter().enumerate() {
        assert_eq!(
            registry.records[key].pending_index.load(Ordering::Relaxed),
            index
        );
    }
    registry.clear();
    retirement.drain();
}
#[test]
fn collector_registry_admission_reference_completion_preflight_is_all_or_nothing() {
    let retirement = native_retirement::Retirement::default();
    let mut registry = CompletionRegistry::default();
    for i in 0..64 {
        registry.register(record(i), &retirement);
    }
    while registry.records.len() < registry.records.capacity() {
        registry.register(record(42), &retirement);
    }
    registry.pending.shrink_to_fit();
    let live = registry.records.len();
    let plan = [growth_request(0, live), 0, 0];
    let mut stage = RegistryStage::components(plan, Tables::components(plan));
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
fn collector_registry_admission_reference_completion_pending_growth_preserves_order() {
    let retirement = native_retirement::Retirement::default();
    let mut registry = CompletionRegistry::default();
    for i in 0..64 {
        registry.register(record(i), &retirement);
    }
    registry.owned.reserve(registry.records.len() + 1);
    assert_eq!(registry.pending.capacity(), registry.pending.len());
    let order = registry.pending.clone();
    let plan = registry.growth_plan();
    assert_eq!(plan, [0, 130, 0]);
    let mut stage = RegistryStage::components(plan, Tables::components(plan));
    allocation_probe::start();
    assert!(registry.install(&mut stage));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(registry.pending, order);
    for (index, key) in registry.pending.iter().enumerate() {
        assert_eq!(
            registry.records[key].pending_index.load(Ordering::Relaxed),
            index
        );
    }
    for value in (0..64).rev() {
        assert_eq!(registry.next_pending().unwrap().join(), value);
    }
    assert!(registry.next_pending().is_none());
    registry.clear();
    retirement.drain();
}
#[test]
fn collector_registry_admission_reference_completion_raw_history_reserves_future_owners() {
    let retirement = native_retirement::Retirement::default();
    let mut registry = CompletionRegistry::default();
    let mut pointers = Vec::new();
    for i in 0..64 {
        let r = record(i);
        pointers.push(Arc::as_ptr(&r) as *mut JoinHandle);
        registry.register(r, &retirement);
    }
    registry.pending.reserve(1);
    assert!(registry.owned.is_empty());
    assert_eq!(registry.owned.capacity(), registry.records.len());
    let plan = registry.growth_plan();
    assert_eq!(plan, [0, 0, 130]);
    let mut stage = RegistryStage::components(plan, Tables::components(plan));
    let extra = record(42);
    extra
        .retirement
        .get_or_init(|| crate::runtime::native_retirement::RecordLink::new(&retirement));
    let p = Arc::as_ptr(&extra) as *mut JoinHandle;
    allocation_probe::start();
    assert!(registry.install(&mut stage));
    registry.register_reserved(extra);
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
fn collector_native_metadata_reference_completion_rejects_foreign_counter() {
    let a = Arc::<crate::runtime::allocation_counter::AllocationCounter>::default();
    let b = Arc::<crate::runtime::allocation_counter::AllocationCounter>::default();
    let retirement = native_retirement::Retirement::default();
    let mut registry = CompletionRegistry::with_allocator(MetadataAllocator::new(&a));
    for i in 0..64 {
        registry.register(record(i), &retirement);
    }
    while registry.next_pending().is_some() {}
    registry.pending.shrink_to_fit();
    registry.owned.reserve(65);
    let before = a.bytes();
    let plan = registry.growth_plan();
    let mut stage =
        RegistryStage::components(plan, Tables::in_allocator(plan, MetadataAllocator::new(&b)));
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
