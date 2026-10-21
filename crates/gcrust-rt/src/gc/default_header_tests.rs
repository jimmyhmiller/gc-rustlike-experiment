use super::*;
use crate::gc::FORWARDING_BIT;

fn check<H: ObjHeader>() {
    let layout = HeaderLayout::of::<H>();
    for id in 0..=u16::MAX {
        let header = H::new(id);
        assert_eq!(header.type_id(), id);
        let pointer = &header as *const H as *const u8;
        for offset in [0, H::HEAP_FORWARDING_OFFSET] {
            let word = unsafe { pointer.add(offset).cast::<u64>().read_unaligned() };
            assert_eq!(
                word & FORWARDING_BIT,
                0,
                "fresh {} ID {id} sets forwarding marker at word {offset}",
                std::any::type_name::<H>()
            );
            let captured = unsafe {
                pointer
                    .add(H::HEAP_FORWARDING_OFFSET)
                    .cast::<u64>()
                    .read_unaligned()
            };
            assert_eq!(
                captured, id as u64,
                "type-ID word must use numeric low bits"
            );
            assert_eq!(unsafe { layout.type_id_from_word(pointer, captured) }, id);
        }
    }
}
#[test]
fn collector_default_compact_header_preserves_all_ids_without_forwarding_overlap() {
    check::<Compact>();
}
#[test]
fn collector_default_full_header_preserves_all_ids_without_forwarding_overlap() {
    check::<Full>();
}
struct Tagged;
impl crate::gc::PtrPolicy for Tagged {
    fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
        (bits != 0).then_some((bits & !3) as *mut u8)
    }
    fn encode_ptr(pointer: *mut u8) -> u64 {
        pointer as u64 | 3
    }
}

fn graph<H: ObjHeader, P: crate::gc::PtrPolicy>(mode: usize, tag: Option<u64>) {
    use crate::gc::{Heap, TypeInfo};
    let shape = TypeInfo::for_header(H::SIZE)
        .with_fields(2)
        .with_raw_bytes(8);
    let table = (0..=u16::MAX).map(|id| shape.clone().with_type_id(id)).collect();
    let heap = if mode == 0 {
        Heap::new::<H>(65536, table)
    } else {
        Heap::new_generational::<H>(8192, 65536, table)
    };
    let (state, _) = heap.register_thread();
    let ids = [1, 32767, 32768, 65535];
    unsafe {
        let nodes: Vec<_> = ids
            .iter()
            .map(|&id| {
                let info = shape.clone().with_type_id(id);
                let node = if mode == 2 {
                    heap.alloc_nursery_obj::<H>(&info, 0)
                } else {
                    heap.alloc_obj::<H>(&info, 0)
                };
                assert!(!node.is_null());
                let word = node.add(H::HEAP_FORWARDING_OFFSET).cast::<u64>().read();
                assert_eq!(word & FORWARDING_BIT, 0, "fresh object already forwarded");
                if let Some(tag) = tag {
                    node.cast::<u64>().write(tag);
                }
                node
            })
            .collect();
        for (i, &node) in nodes.iter().enumerate() {
            node.add(H::SIZE)
                .cast::<u64>()
                .write(P::encode_ptr(nodes[(i + 1) % ids.len()]));
            node.add(H::SIZE + 8)
                .cast::<u64>()
                .write(P::encode_ptr(nodes[(i + 2) % ids.len()]));
            node.add(H::SIZE + 16)
                .cast::<u64>()
                .write(0x1357_0000 + ids[i] as u64);
        }
        let root = heap.globals.add(P::encode_ptr(nodes[0]));
        for _ in 0..3 {
            match mode {
                0 => {
                    state.enter_blocked();
                    heap.stw_collect::<P>();
                    state.exit_blocked(&heap);
                }
                1 => heap.mutator_triggered_gc::<P>(&state),
                2 => {
                    heap.mutator_triggered_minor_gc::<P>(&state);
                    heap.mutator_triggered_gc::<P>(&state);
                }
                3 => {
                    state.enter_blocked();
                    heap.concurrent_collect::<P>();
                    state.exit_blocked(&heap);
                }
                _ => unreachable!(),
            }
            let first = P::try_decode_ptr(heap.globals.get(root)).unwrap();
            let mut node = first;
            for (i, &id) in ids.iter().enumerate() {
                assert!(heap.from_space_contains(node));
                assert_eq!(heap.obj_type_id(node), id);
                assert_eq!(
                    node.add(H::SIZE + 16).cast::<u64>().read(),
                    0x1357_0000 + id as u64
                );
                let alias = P::try_decode_ptr(node.add(H::SIZE + 8).cast::<u64>().read()).unwrap();
                assert_eq!(heap.obj_type_id(alias), ids[(i + 2) % ids.len()]);
                if let Some(tag) = tag {
                    assert_eq!(node.cast::<u64>().read(), tag);
                }
                let next = P::try_decode_ptr(node.add(H::SIZE).cast::<u64>().read()).unwrap();
                let shared = P::try_decode_ptr(next.add(H::SIZE).cast::<u64>().read()).unwrap();
                assert_eq!(alias, shared, "shared target was duplicated");
                node = next;
            }
            assert_eq!(node, first, "cycle identity changed");
        }
        heap.safe_deregister_thread(&state);
    }
}

#[test]
fn collector_default_headers_preserve_high_ids_shared_cycles_and_full_tags() {
    for mode in 0..4 {
        graph::<Compact, crate::gc::IdentityPtrPolicy>(mode, None);
        graph::<Compact, Tagged>(mode, None);
        graph::<Full, crate::gc::IdentityPtrPolicy>(mode, Some(0xaabb_ccdd));
        graph::<Full, Tagged>(mode, Some(0xaabb_ccdd));
    }
}

fn standalone<H: ObjHeader, P: crate::gc::PtrPolicy>(tag: Option<u64>) {
    use crate::gc::{AtomicRootSet, SemiSpace, TypeInfo};
    let shape = TypeInfo::for_header(H::SIZE)
        .with_fields(1)
        .with_raw_bytes(8);
    let table: Vec<_> = (0..=u16::MAX).map(|id| shape.clone().with_type_id(id)).collect();
    let mut heap = SemiSpace::new::<H>(8192);
    let roots = AtomicRootSet::new();
    let node = heap.alloc_obj::<H>(&shape.with_type_id(65535), 0);
    assert!(!node.is_null());
    unsafe {
        assert_eq!(
            node.cast::<u64>().read() & FORWARDING_BIT,
            0,
            "fresh standalone header is forwarded"
        );
        if let Some(tag) = tag {
            node.cast::<u64>().write(tag);
        }
        node.add(H::SIZE).cast::<u64>().write(P::encode_ptr(node));
        node.add(H::SIZE + 8).cast::<u64>().write(42);
        let root = roots.add(P::encode_ptr(node));
        for _ in 0..4 {
            heap.collect::<P>(&table, &mut [&roots]);
            let moved = P::try_decode_ptr(roots.get(root)).unwrap();
            assert!(heap.contains(moved));
            assert_eq!(moved.add(H::TYPE_ID_OFFSET).cast::<u16>().read(), 65535);
            assert_eq!(
                P::try_decode_ptr(moved.add(H::SIZE).cast::<u64>().read()),
                Some(moved)
            );
            assert_eq!(moved.add(H::SIZE + 8).cast::<u64>().read(), 42);
            if let Some(tag) = tag {
                assert_eq!(moved.cast::<u64>().read(), tag);
            }
        }
    }
}

#[test]
fn collector_default_headers_preserve_standalone_semispace_high_ids_and_tags() {
    standalone::<Compact, crate::gc::IdentityPtrPolicy>(None);
    standalone::<Compact, Tagged>(None);
    standalone::<Full, crate::gc::IdentityPtrPolicy>(Some(0x1357_9bdf));
    standalone::<Full, Tagged>(Some(0x1357_9bdf));
}
