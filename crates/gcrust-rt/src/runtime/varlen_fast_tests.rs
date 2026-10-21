use super::*;
use crate::gc::{ObjHeader, TypeInfo, VarLenKind};

#[test]
fn collector_varlen_local_success_initializes_header_count_and_storage() {
    for generational in [false, true] {
        for kind in [VarLenKind::Bytes, VarLenKind::Values] {
            let mut info = TypeInfo::for_header(Full::SIZE)
                .with_type_id(1)
                .with_fields(1)
                .with_raw_bytes(16)
                .with_owned_interior_ptrs(vec![24]);
            info.varlen = kind;
            let mut rt = if generational {
                RuntimeContext::new_generational(
                    32768,
                    65536,
                    vec![info.clone().with_type_id(0), info.clone()],
                )
            } else {
                RuntimeContext::new(65536, vec![info.clone().with_type_id(0), info.clone()])
            };
            let heap = rt.heap().clone();
            rt.enter(|thread| {
                let state = unsafe { &*(*thread).dyna_thread };
                let max_local = match kind {
                    VarLenKind::Values => (8192 - info.varlen_count_offset() - 8) / 8,
                    _ => 8192 - info.varlen_count_offset() - 8,
                };
                for length in [0, 1, 7, 16, 255, max_local] {
                    let canonical = heap.type_info_by_id(1);
                    let request = heap.object_request::<Full>(canonical, length).unwrap();
                    let before = (heap.collections(), heap.minor_collections());
                    let pointer = unsafe { request.try_allocate_local(state) };
                    assert!(!pointer.is_null());
                    assert_eq!((heap.collections(), heap.minor_collections()), before);
                    assert_eq!(unsafe { heap.obj_type_id(pointer) }, 1);
                    assert_eq!(
                        unsafe { crate::gc::read_varlen_count(pointer, canonical) },
                        length
                    );
                    let count_offset = canonical.varlen_count_offset();
                    assert!(
                        unsafe {
                            std::slice::from_raw_parts(
                                pointer.add(Full::SIZE),
                                count_offset - Full::SIZE,
                            )
                        }
                        .iter()
                        .all(|b| *b == 0)
                    );
                    let size = canonical.checked_allocation_size(length).unwrap();
                    assert!(
                        unsafe {
                            std::slice::from_raw_parts(
                                pointer.add(count_offset + 8),
                                size - count_offset - 8,
                            )
                        }
                        .iter()
                        .all(|b| *b == 0)
                    );
                }
                let before = heap.nursery_used() + heap.from_used();
                let large = heap
                    .object_request::<Full>(heap.type_info_by_id(1), max_local + 1)
                    .unwrap();
                assert!(unsafe { large.try_allocate_local(state) }.is_null());
                assert_eq!(heap.nursery_used() + heap.from_used(), before);
            });
        }
    }
}

#[test]
fn collector_varlen_runtime_stress_collects_and_counts_once_per_object() {
    for generational in [false, true] {
        let info = TypeInfo::for_header(Full::SIZE).with_varlen_values(0);
        let mut rt = if generational {
            RuntimeContext::new_generational(4096, 65536, vec![info])
        } else {
            RuntimeContext::new(65536, vec![info])
        };
        let heap = rt.heap().clone();
        heap.set_gc_every_alloc(true);
        rt.enter(|thread| {
            let state = unsafe { &*(*thread).dyna_thread };
            let request = heap
                .object_request::<Full>(heap.type_info_by_id(0), 3)
                .unwrap();
            assert!(unsafe { request.try_allocate_local(state) }.is_null());
            let mut expected_bytes = 0;
            for length in [0, 1, 7, 64] {
                let before = if generational {
                    heap.minor_collections()
                } else {
                    heap.collections()
                };
                let pointer = unsafe { ai_gc_alloc_varlen(thread, 0, length, 0) };
                let after = if generational {
                    heap.minor_collections()
                } else {
                    heap.collections()
                };
                assert_eq!(after, before + 1);
                assert_eq!(
                    unsafe { crate::gc::read_varlen_count(pointer, heap.type_info_by_id(0)) },
                    length as usize
                );
                expected_bytes += heap.type_info_by_id(0).allocation_size(length as usize) as u64;
            }
            let counters = unsafe { state.alloc_site_counters() };
            assert_eq!(counters[0].count, 4);
            assert_eq!(counters[0].bytes, expected_bytes);
        });
    }
}

#[test]
fn collector_varlen_fast_and_slow_allocations_keep_exact_counters_and_roots() {
    let info = TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0);
    let mut rt = RuntimeContext::new_generational(1024, 65536, vec![info]);
    let heap = rt.heap().clone();
    rt.enter(|thread| {
        let mut expected_bytes = 0;
        let root = unsafe { heap.globals.add(0) };
        for length in [8, 8, 10000, 8, 16384, 8] {
            let pointer = unsafe { ai_gc_alloc_varlen(thread, 0, length, 0) };
            assert_eq!(
                unsafe { crate::gc::read_varlen_count(pointer, heap.type_info_by_id(0)) },
                length as usize
            );
            unsafe {
                pointer
                    .add(heap.type_info_by_id(0).varlen_element_offset(0))
                    .write(42);
            }
            unsafe {
                heap.globals.set(root, pointer as u64);
            }
            expected_bytes += heap.type_info_by_id(0).allocation_size(length as usize) as u64;
            unsafe {
                heap.mutator_triggered_minor_gc::<crate::gc::IdentityPtrPolicy>(
                    &*(*thread).dyna_thread,
                );
            }
            let moved = heap.globals.get(root) as *mut u8;
            assert_eq!(
                unsafe {
                    moved
                        .add(heap.type_info_by_id(0).varlen_element_offset(0))
                        .read()
                },
                42
            );
        }
        let counters = unsafe { (&*(*thread).dyna_thread).alloc_site_counters() };
        assert_eq!(counters[0].count, 6);
        assert_eq!(counters[0].bytes, expected_bytes);
    });
}
