use super::*;
use crate::gc::{Compact, Full, IdentityPtrPolicy, ObjHeader};
use crate::runtime::allocation_probe;

fn task_keys(work: &[Work]) -> Vec<(u8, usize)> {
    work.iter()
        .map(|task| match *task {
            Work::Slot(n) => (0, n),
            Work::Object(n) => (1, n),
            Work::Channel(n) => (2, n),
        })
        .collect()
}
unsafe fn allocate(heap: &Heap, full: bool, young: bool, info: &TypeInfo, len: usize) -> *mut u8 {
    unsafe {
        match (full, young) {
            (true, true) => heap.alloc_nursery_obj::<Full>(info, len),
            (false, true) => heap.alloc_nursery_obj::<Compact>(info, len),
            (true, false) => heap.alloc_obj::<Full>(info, len),
            (false, false) => heap.alloc_obj::<Compact>(info, len),
        }
    }
}

#[test]
fn collector_resolved_retries_preserve_busy_unclaimed_and_other_work() {
    for full in [false, true] {
        for minor in [false, true] {
            let info = TypeInfo::for_header(if full { Full::SIZE } else { Compact::SIZE })
                .with_raw_bytes(8);
            let heap = if minor {
                if full {
                    Heap::new_generational::<Full>(1 << 20, 4 << 20, vec![info.clone()])
                } else {
                    Heap::new_generational::<Compact>(1 << 20, 4 << 20, vec![info.clone()])
                }
            } else if full {
                Heap::new::<Full>(4 << 20, vec![info.clone()])
            } else {
                Heap::new::<Compact>(4 << 20, vec![info.clone()])
            };
            let object = unsafe { allocate(&heap, full, minor, &info, 0) };
            let unclaimed = unsafe { allocate(&heap, full, minor, &info, 0) };
            unsafe { object.add(info.raw_data_offset()).cast::<u64>().write(42) };
            let mut first = object as u64;
            let mut alias = object as u64;
            let mut untouched = unclaimed as u64;
            let mut scalar = 0u64;
            let mut updated = 0u64;
            let alias_address = &mut alias as *mut u64 as usize;
            let unclaimed_address = &mut untouched as *mut u64 as usize;
            let updated_address = &mut updated as *mut u64 as usize;
            let mut work = worker_scratch::test_buffer([
                Work::Object(99),
                Work::Slot(alias_address),
                Work::Channel(7),
                Work::Slot(unclaimed_address),
                Work::Slot(&mut scalar as *mut u64 as usize),
            ]);
            let mut pending = PendingCopies::default();
            let mut discovered = worker_scratch::test_buffer([]);
            unsafe {
                heap.parallel_slot_mode_with_channels::<IdentityPtrPolicy>(
                    &mut first,
                    &mut discovered,
                    &mut pending,
                    minor,
                    None,
                )
            };
            assert_eq!(pending.len(), 1);
            let header = unsafe {
                &*object
                    .add(heap.header.forwarding_offset)
                    .cast::<AtomicU64>()
            };
            assert_eq!(header.load(Ordering::Acquire), FORWARDING_BIT);
            allocation_probe::start();
            unsafe { heap.resolve_published_retries::<IdentityPtrPolicy>(&mut work, minor) };
            let traffic = allocation_probe::finish_bytes();
            assert_eq!(traffic, (0, 0, 0, 0));
            assert_eq!(alias, object as u64);
            assert_eq!(untouched, unclaimed as u64);
            assert_eq!(
                task_keys(&work),
                vec![(1, 99), (0, alias_address), (2, 7), (0, unclaimed_address)]
            );
            let mut layouts = worker_scratch::test_buffer([]);
            let mut addresses = worker_scratch::test_buffer([]);
            unsafe {
                heap.publish_batch::<IdentityPtrPolicy>(
                    &mut pending,
                    &mut layouts,
                    &mut addresses,
                    &mut discovered,
                    minor,
                )
            };
            assert!(pending.is_empty());
            assert_ne!(first, object as u64);
            updated = first;
            // Another visitor has already completed this slot. Do not interpret
            // the destination as a source header or keep an unnecessary task.
            work.push(Work::Slot(updated_address));
            allocation_probe::start();
            unsafe { heap.resolve_published_retries::<IdentityPtrPolicy>(&mut work, minor) };
            let traffic = allocation_probe::finish_bytes();
            assert_eq!(traffic, (0, 0, 0, 0));
            assert_eq!(alias, first);
            assert_eq!(updated, first);
            assert_eq!(untouched, unclaimed as u64);
            assert_eq!(
                task_keys(&work),
                vec![(1, 99), (2, 7), (0, unclaimed_address)]
            );
            assert_eq!(
                unsafe {
                    (first as *const u8)
                        .add(info.raw_data_offset())
                        .cast::<u64>()
                        .read()
                },
                42
            );
        }
    }
}

#[test]
fn collector_resolved_retries_skip_uncollected_generations() {
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let heap = Heap::new_generational::<Full>(1 << 20, 4 << 20, vec![info.clone()]);
    let old = unsafe { heap.alloc_obj::<Full>(&info, 0) };
    let young = unsafe { heap.alloc_nursery_obj::<Full>(&info, 0) };
    let mut old_slot = old as u64;
    let mut young_slot = young as u64;
    let mut work = worker_scratch::test_buffer([Work::Slot(&mut old_slot as *mut u64 as usize)]);
    unsafe { heap.resolve_published_retries::<IdentityPtrPolicy>(&mut work, true) };
    assert!(work.is_empty());
    assert_eq!(old_slot, old as u64);
    work.push(Work::Slot(&mut young_slot as *mut u64 as usize));
    unsafe { heap.resolve_published_retries::<IdentityPtrPolicy>(&mut work, false) };
    assert!(work.is_empty());
    assert_eq!(young_slot, young as u64);
}

#[test]
fn collector_resolved_retries_streamed_alias_graph_moves_across_generations() {
    const COUNT: usize = 257;
    for full in [false, true] {
        let header = if full { Full::SIZE } else { Compact::SIZE };
        let node = TypeInfo::for_header(header)
            .with_fields(2)
            .with_raw_bytes(8);
        let array = TypeInfo::for_header(header)
            .with_type_id(1)
            .with_varlen_values(0);
        let heap = if full {
            Heap::new_generational::<Full>(1 << 20, 4 << 20, vec![node.clone(), array.clone()])
        } else {
            Heap::new_generational::<Compact>(1 << 20, 4 << 20, vec![node.clone(), array.clone()])
        };
        let (thread, _) = heap.register_thread();
        let nodes: Vec<_> = (0..COUNT)
            .map(|_| unsafe { allocate(&heap, full, true, &node, 0) })
            .collect();
        let root = unsafe { allocate(&heap, full, true, &array, COUNT) };
        for (index, &object) in nodes.iter().enumerate() {
            unsafe {
                object
                    .add(node.value_field_offset(0))
                    .cast::<u64>()
                    .write(nodes[(index + 1) % COUNT] as u64);
                object
                    .add(node.value_field_offset(1))
                    .cast::<u64>()
                    .write(nodes[(index + 17) % COUNT] as u64);
                object
                    .add(node.raw_data_offset())
                    .cast::<u64>()
                    .write(index as u64);
                root.add(array.varlen_element_offset(index))
                    .cast::<u64>()
                    .write(object as u64);
            }
        }
        drop(nodes);
        for _ in 0..3 {
            unsafe { heap.globals.add(root as u64) };
        }
        for round in 0..4 {
            unsafe {
                if round == 0 {
                    heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&thread)
                } else {
                    heap.mutator_triggered_gc::<IdentityPtrPolicy>(&thread)
                }
            }
            assert_eq!(heap.nursery_used(), 0);
            assert_eq!(
                heap.from_used(),
                COUNT * node.allocation_size(0) + array.allocation_size(COUNT)
            );
            let root = heap.globals.get(0) as *const u8;
            assert_eq!(heap.globals.get(1), root as u64);
            assert_eq!(heap.globals.get(2), root as u64);
            for index in 0..COUNT {
                let object = unsafe {
                    root.add(array.varlen_element_offset(index))
                        .cast::<u64>()
                        .read()
                } as *const u8;
                assert_eq!(
                    unsafe { object.add(node.raw_data_offset()).cast::<u64>().read() },
                    index as u64
                );
                for (field, offset) in [(0, 1), (1, 17)] {
                    let child = unsafe {
                        object
                            .add(node.value_field_offset(field))
                            .cast::<u64>()
                            .read()
                    } as *const u8;
                    assert_eq!(
                        unsafe { child.add(node.raw_data_offset()).cast::<u64>().read() },
                        ((index + offset) % COUNT) as u64
                    );
                }
            }
        }
        assert_eq!(heap.minor_collections(), 1);
        assert_eq!(heap.collections(), 3);
    }
}
