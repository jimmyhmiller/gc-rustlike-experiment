use super::*;
use crate::{
    gc::{Full, ObjHeader},
    runtime::allocation_probe,
};

fn descriptors() -> Vec<TypeInfo> {
    let mut live = Vec::with_capacity(19);
    live.extend([16, 24]);
    let reserved = Vec::with_capacity(23);
    let mut table = Vec::with_capacity(17);
    let mut first = TypeInfo::for_header(Full::SIZE).with_raw_bytes(16);
    first.interior_ptrs = Cow::Owned(live);
    table.push(first);
    table.push(
        TypeInfo::for_header_with_interior_ptrs(Full::SIZE, &[16])
            .with_raw_bytes(8)
            .with_type_id(1),
    );
    let mut last = TypeInfo::for_header(Full::SIZE)
        .with_raw_bytes(8)
        .with_type_id(2);
    last.interior_ptrs = Cow::Owned(reserved);
    table.push(last);
    table
}
#[test]
fn collector_layout_table_actual_adoption_and_nested_physical_release() {
    let counter = Arc::<AllocationCounter>::default();
    allocation_probe::start();
    let values = descriptors();
    let original = allocation_probe::finish_bytes();
    let bytes = original.2 - original.3;
    let table_pointer = values.as_ptr();
    let offset_pointer = values[0].interior_ptrs.as_ptr();
    allocation_probe::start();
    let table = LayoutTable::new(values, &counter);
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(counter.bytes(), bytes);
    assert_eq!(counter.peak(), bytes);
    assert_eq!(table.as_ptr(), table_pointer);
    assert_eq!(table[0].interior_ptrs.as_ptr(), offset_pointer);
    assert_eq!(&*table[0].interior_ptrs, &[16, 24]);
    assert_eq!(&*table[1].interior_ptrs, &[16]);
    allocation_probe::start();
    drop(table);
    let released = allocation_probe::finish_bytes();
    assert_eq!(
        (released.0, released.1, released.2, released.3),
        (0, original.0, 0, bytes)
    );
    assert_eq!(counter.bytes(), 0);
}
#[test]
fn collector_layout_table_growth_preserves_backing_and_descriptors() {
    let counter = Arc::<AllocationCounter>::default();
    let mut table = LayoutTable::new(descriptors(), &counter);
    let offsets = table[0].interior_ptrs.as_ptr();
    let old = counter.bytes();
    let mut extra = Vec::with_capacity(11);
    extra.push(16);
    let mut info = TypeInfo::for_header(Full::SIZE)
        .with_raw_bytes(8)
        .with_type_id(3);
    info.interior_ptrs = Cow::Owned(extra);
    allocation_probe::start();
    table.push(info);
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(counter.bytes(), old + 22);
    let before_growth = counter.bytes();
    allocation_probe::start();
    for id in 4..34 {
        table.push(
            TypeInfo::for_header(Full::SIZE)
                .with_raw_bytes(8)
                .with_type_id(id),
        );
    }
    let growth = allocation_probe::finish_bytes();
    assert_eq!((growth.0, growth.1), (1, 0));
    assert_eq!(counter.bytes(), before_growth + growth.2 - growth.3);
    assert_eq!(counter.peak(), before_growth + growth.2 - growth.3);
    assert_eq!(table[0].interior_ptrs.as_ptr(), offsets);
    for (id, info) in table.iter().enumerate() {
        assert_eq!(info.type_id, id as u16);
    }
    assert_eq!(&*table[3].interior_ptrs, &[16]);
    drop(table);
    assert_eq!(counter.bytes(), 0);
}
#[test]
fn collector_layout_table_empty_reserved_and_counter_owner_lifetime() {
    let counter = Arc::<AllocationCounter>::default();
    allocation_probe::start();
    let empty = LayoutTable::new(Vec::new(), &counter);
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(counter.bytes(), 0);
    drop(empty);
    allocation_probe::start();
    let values = Vec::<TypeInfo>::with_capacity(17);
    let backing = allocation_probe::finish_bytes();
    let table = LayoutTable::new(values, &counter);
    assert!(table.is_empty());
    assert_eq!(counter.bytes(), backing.2);
    let weak = Arc::downgrade(&counter);
    drop(counter);
    assert_eq!(weak.upgrade().unwrap().bytes(), backing.2);
    drop(table);
    assert!(weak.upgrade().is_none());
}
#[test]
fn collector_layout_table_failed_offset_adoption_preserves_existing_owner() {
    let counter = Arc::<AllocationCounter>::default();
    let mut table = LayoutTable::new(Vec::new(), &counter);
    assert!(counter.try_charge(usize::MAX));
    let mut offsets = Vec::with_capacity(11);
    offsets.push(16);
    let mut info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    info.interior_ptrs = Cow::Owned(offsets);
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| table.push(info)));
    assert!(failure.is_err());
    assert!(table.is_empty());
    assert_eq!(counter.bytes(), usize::MAX);
    drop(table);
    assert_eq!(counter.bytes(), usize::MAX);
    counter.release(usize::MAX);
    assert_eq!(counter.bytes(), 0);
}

#[test]
fn collector_layout_table_registered_offsets_survive_growth_and_moving_collections() {
    use crate::gc::{Heap, IdentityPtrPolicy};
    for generational in [false, true] {
        let mut heap = if generational {
            Heap::new_generational::<Full>(1024, 8192, vec![])
        } else {
            Heap::new::<Full>(8192, vec![])
        };
        let counter = heap.native_memory.registry.clone();
        let mut offsets = Vec::with_capacity(19);
        offsets.extend([16, 24]);
        let mut parent_info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(16);
        parent_info.interior_ptrs = Cow::Owned(offsets);
        assert_eq!(heap.dynamic_add_type(parent_info), 0);
        assert_eq!(
            heap.dynamic_add_type(TypeInfo::for_header(Full::SIZE).with_raw_bytes(8)),
            1
        );
        let parent_info = heap.type_info_by_id(0).clone();
        let leaf_info = heap.type_info_by_id(1).clone();
        let (parent, leaf) = unsafe {
            let parent = if generational {
                heap.alloc_nursery_obj::<Full>(&parent_info, 0)
            } else {
                heap.alloc_obj::<Full>(&parent_info, 0)
            };
            let leaf = if generational {
                heap.alloc_nursery_obj::<Full>(&leaf_info, 0)
            } else {
                heap.alloc_obj::<Full>(&leaf_info, 0)
            };
            assert!(!parent.is_null() && !leaf.is_null());
            leaf.add(Full::SIZE).cast::<u64>().write(42);
            parent.add(16).cast::<u64>().write(leaf as u64);
            parent.add(24).cast::<u64>().write(leaf as u64);
            (parent, leaf)
        };
        assert!(!leaf.is_null());
        let root = unsafe { heap.globals.add(parent as u64) };
        let check = |heap: &Heap| {
            let parent = heap.globals.get(root) as *const u8;
            assert!(heap.from_space_contains(parent));
            let left = unsafe { parent.add(16).cast::<u64>().read() } as *const u8;
            let right = unsafe { parent.add(24).cast::<u64>().read() } as *const u8;
            assert_eq!(left, right);
            assert!(heap.from_space_contains(parent));
            assert!(heap.from_space_contains(left));
            assert_eq!(unsafe { left.add(Full::SIZE).cast::<u64>().read() }, 42);
            assert_eq!(
                heap.from_used(),
                parent_info.allocation_size(0) + leaf_info.allocation_size(0)
            );
        };
        let (state, _) = heap.register_thread();
        unsafe {
            if generational {
                heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&state)
            } else {
                heap.mutator_triggered_gc::<IdentityPtrPolicy>(&state)
            }
        };
        check(&heap);
        unsafe { heap.safe_deregister_thread(&state) };
        drop(state);
        for _ in 2..34 {
            heap.dynamic_add_type(TypeInfo::for_header(Full::SIZE).with_raw_bytes(8));
        }
        assert_eq!(&*heap.type_info_by_id(0).interior_ptrs, &[16, 24]);
        let (state, _) = heap.register_thread();
        for _ in 0..3 {
            unsafe { heap.mutator_triggered_gc::<IdentityPtrPolicy>(&state) };
            check(&heap);
        }
        unsafe { heap.safe_deregister_thread(&state) };
        drop(state);
        drop(heap);
        assert_eq!(counter.bytes(), 0);
    }
}
