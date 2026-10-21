use super::*;
use crate::{
    gc::{Compact, Heap},
    runtime::allocation_probe,
};
use std::sync::{Barrier, OnceLock};

fn name(text: &str, capacity: usize) -> String {
    let mut name = String::with_capacity(capacity);
    name.push_str(text);
    name
}
fn fields() -> Vec<FieldMeta> {
    let mut offsets = Vec::with_capacity(19);
    offsets.extend([(0, 8), (8, 16)]);
    let mut fields = Vec::with_capacity(11);
    for (i, ty) in [
        FieldTy::Ref(0),
        FieldTy::Scalar(crate::gc::reflect::ScalarKind::I64),
        FieldTy::Value(0),
        FieldTy::ProjectedValue {
            value_id: 0,
            reference_offsets: offsets,
        },
    ]
    .into_iter()
    .enumerate()
    {
        fields.push(FieldMeta {
            name: name(["ref", "scalar", "value", "projected"][i], 37),
            offset: i as u16 * 8,
            ty,
        });
    }
    fields
}
fn metadata() -> (Vec<TypeMeta>, Vec<ValueMeta>, Vec<AllocSite>) {
    let mut variants = Vec::with_capacity(7);
    variants.push(VariantMeta {
        name: name("Some", 59),
        tag: 1,
        fields: fields(),
    });
    variants.push(VariantMeta {
        name: name("None", 61),
        tag: 0,
        fields: Vec::with_capacity(13),
    });
    let mut types = Vec::with_capacity(9);
    types.push(TypeMeta {
        type_id: 0,
        name: name("Enum", 101),
        kind: TypeKind::Enum {
            tag_offset: 16,
            variants,
        },
    });
    types.push(TypeMeta {
        type_id: 1,
        name: name("Struct", 103),
        kind: TypeKind::Struct { fields: fields() },
    });
    types.push(TypeMeta {
        type_id: 2,
        name: name("", 107),
        kind: TypeKind::Opaque,
    });
    let mut values = Vec::with_capacity(5);
    values.push(ValueMeta {
        value_id: 0,
        name: name("Value", 109),
        kind: TypeKind::Struct { fields: fields() },
    });
    let mut sites = Vec::with_capacity(17);
    sites.push(AllocSite {
        function: name("f", 113),
        type_id: 0,
        location: name("file:1:2", 127),
    });
    (types, values, sites)
}

#[test]
fn collector_installed_metadata_actual_capacities_and_physical_release() {
    let counter = Arc::<AllocationCounter>::default();
    allocation_probe::start();
    let (types, values, sites) = metadata();
    let physical = allocation_probe::finish_bytes();
    let requested = physical.2 - physical.3;
    assert!(requested > 4096);
    let type_address = types.as_ptr();
    let name_address = types[0].name.as_ptr();
    allocation_probe::start();
    let tables = (
        InstalledTable::new(types, &counter),
        InstalledTable::new(values, &counter),
        InstalledTable::new(sites, &counter),
    );
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(counter.bytes(), requested);
    assert_eq!(counter.peak(), requested);
    assert_eq!(tables.0.as_slice().as_ptr(), type_address);
    assert_eq!(tables.0.as_slice()[0].name.as_ptr(), name_address);
    assert_eq!(tables.0.as_slice()[0].name, "Enum");
    assert_eq!(tables.2.as_slice()[0].location, "file:1:2");
    allocation_probe::start();
    drop(tables);
    let release = allocation_probe::finish_bytes();
    assert_eq!((release.0, release.2, release.3), (0, 0, requested));
    assert_eq!(release.1, physical.0);
    assert_eq!(counter.bytes(), 0);
}

#[test]
fn collector_installed_metadata_empty_and_unused_backing() {
    let counter = Arc::<AllocationCounter>::default();
    let empty = InstalledTable::<TypeMeta>::new(Vec::new(), &counter);
    assert_eq!(counter.bytes(), 0);
    assert_eq!(Arc::strong_count(&counter), 1);
    assert!(empty.as_slice().is_empty());
    drop(empty);
    allocation_probe::start();
    let reserved = Vec::<TypeMeta>::with_capacity(17);
    let physical = allocation_probe::finish_bytes();
    let table = InstalledTable::new(reserved, &counter);
    assert!(table.as_slice().is_empty());
    assert_eq!(counter.bytes(), physical.2);
    drop(table);
    assert_eq!(counter.bytes(), 0);
}

#[test]
fn collector_installed_metadata_contended_set_preserves_winner_and_releases_loser() {
    let counter = Arc::<AllocationCounter>::default();
    let table = OnceLock::<InstalledTable<AllocSite>>::new();
    let barrier = Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let jobs: Vec<_> = (0..2)
            .map(|i| {
                let counter = &counter;
                let table = &table;
                let barrier = &barrier;
                scope.spawn(move || {
                    allocation_probe::start();
                    let mut sites = Vec::with_capacity(5 + i);
                    sites.push(AllocSite {
                        function: name(if i == 0 { "first" } else { "second" }, 61 + i),
                        type_id: 0,
                        location: name("", 37 + i),
                    });
                    let physical = allocation_probe::finish_bytes();
                    let owned = InstalledTable::new(sites, counter);
                    barrier.wait();
                    let installed = table.set(owned).is_ok();
                    (installed, physical.2 - physical.3)
                })
            })
            .collect();
        jobs.into_iter()
            .map(|job| job.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|r| r.0).count(), 1);
    assert_eq!(counter.bytes(), results.iter().find(|r| r.0).unwrap().1);
    assert!(matches!(
        table.get().unwrap().as_slice()[0].function.as_str(),
        "first" | "second"
    ));
    drop(table);
    assert_eq!(counter.bytes(), 0);
}

#[test]
fn collector_installed_metadata_heap_adoption_and_rejected_replacement() {
    let heap = Heap::new::<Compact>(65536, vec![]);
    let counter = heap.native_memory.registry.clone();
    let initial = counter.bytes();
    allocation_probe::start();
    let (types, values, sites) = metadata();
    let physical = allocation_probe::finish_bytes();
    heap.set_type_meta(types);
    heap.set_value_meta(values);
    heap.set_alloc_sites(sites);
    assert_eq!(counter.bytes() - initial, physical.2 - physical.3);
    let borrowed = heap.type_meta_all();
    let address = borrowed.as_ptr();
    allocation_probe::start();
    let replacements = metadata();
    let replacement_traffic = allocation_probe::finish_bytes();
    allocation_probe::start();
    heap.set_type_meta(replacements.0);
    heap.set_value_meta(replacements.1);
    heap.set_alloc_sites(replacements.2);
    let rejected = allocation_probe::finish_bytes();
    assert_eq!(
        (rejected.0, rejected.2, rejected.3),
        (0, 0, replacement_traffic.2 - replacement_traffic.3)
    );
    assert_eq!(heap.type_meta_all().as_ptr(), address);
    assert_eq!(borrowed[0].name, "Enum");
    assert_eq!(counter.bytes() - initial, physical.2 - physical.3);
    assert_eq!(heap.value_meta_by_id(0).unwrap().name, "Value");
    assert_eq!(heap.alloc_sites_all()[0].function, "f");
    drop(heap);
    assert_eq!(counter.bytes(), 0);
}

#[test]
fn collector_installed_metadata_retention_participates_in_pressure() {
    let heap = Heap::new::<Compact>(65536, vec![]);
    let counter = heap.native_memory.registry.clone();
    assert!(!heap.native_memory.needs_collection(0));
    let mut huge_name = String::with_capacity(2 << 20);
    huge_name.push_str("large source descriptor");
    heap.set_type_meta(vec![TypeMeta {
        type_id: 0,
        name: huge_name,
        kind: TypeKind::Opaque,
    }]);
    assert!(heap.native_registry_bytes() >= 2 << 20);
    assert!(heap.native_memory.needs_collection(0));
    drop(heap);
    assert_eq!(counter.bytes(), 0);
}
