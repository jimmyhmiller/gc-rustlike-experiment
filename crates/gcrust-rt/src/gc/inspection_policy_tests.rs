use super::*;
use crate::gc::reflect::{TypeMeta, ValueMeta};
use crate::gc::{Compact, Full, IdentityPtrPolicy, ObjHeader, TypeInfo};

struct LowTag;
impl PtrPolicy for LowTag {
    fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
        (bits & 7 == 3).then_some((bits & !7) as *mut u8)
    }
    fn encode_ptr(pointer: *mut u8) -> u64 {
        pointer as u64 | 3
    }
}
struct HighTag;
impl PtrPolicy for HighTag {
    fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
        (bits >> 48 == 0x7ffc)
            .then_some((bits & 0x0000_ffff_ffff_ffff) as *mut u8)
    }
    fn encode_ptr(pointer: *mut u8) -> u64 {
        pointer as u64 | 0x7ffc_0000_0000_0000
    }
}

fn inspect<H: ObjHeader, P: PtrPolicy>(generational: bool) {
    let offset = H::SIZE as u16;
    let parent = TypeInfo::for_header(H::SIZE)
        .with_fields(2)
        .with_raw_bytes(8)
        .with_varlen_values(2)
        .with_owned_interior_ptrs(vec![offset + 16]);
    let child = TypeInfo::for_header(H::SIZE)
        .with_type_id(1)
        .with_raw_bytes(8);
    let heap = if generational {
        Heap::new_generational::<H>(4096, 65536, vec![parent.clone(), child.clone()])
    } else {
        Heap::new::<H>(65536, vec![parent.clone(), child.clone()])
    };
    heap.set_type_meta(vec![
        TypeMeta {
            type_id: 0,
            name: "Parent".into(),
            kind: TypeKind::Struct {
                fields: vec![
                    FieldMeta {
                        name: "child".into(),
                        offset,
                        ty: FieldTy::Ref(1),
                    },
                    FieldMeta {
                        name: "empty".into(),
                        offset: offset + 8,
                        ty: FieldTy::Ref(1),
                    },
                    FieldMeta {
                        name: "inline".into(),
                        offset: offset + 16,
                        ty: FieldTy::Value(0),
                    },
                ],
            },
        },
        TypeMeta {
            type_id: 1,
            name: "Child".into(),
            kind: TypeKind::Opaque,
        },
    ]);
    heap.set_value_meta(vec![ValueMeta {
        value_id: 0,
        name: "Link".into(),
        kind: TypeKind::Struct {
            fields: vec![FieldMeta {
                name: "child".into(),
                offset: 0,
                ty: FieldTy::Ref(1),
            }],
        },
    }]);
    let (state, _) = heap.register_thread();
    unsafe {
        let allocate = |info: &TypeInfo, len| {
            if generational {
                heap.alloc_nursery_obj::<H>(info, len)
            } else {
                heap.alloc_obj::<H>(info, len)
            }
        };
        let leaf = allocate(&child, 0);
        let owner = allocate(&parent, 1);
        assert!(!leaf.is_null() && !owner.is_null());
        owner
            .add(parent.value_field_offset(0))
            .cast::<u64>()
            .write(P::encode_ptr(leaf));
        owner
            .add(parent.value_field_offset(1))
            .cast::<u64>()
            .write(P::encode_ptr(std::ptr::null_mut()));
        owner
            .add(parent.raw_data_offset())
            .cast::<u64>()
            .write(P::encode_ptr(leaf));
        owner
            .add(parent.varlen_element_offset(0))
            .cast::<u64>()
            .write(P::encode_ptr(leaf));
        let root = heap.globals.add(P::encode_ptr(owner));
        // Encoded null must never become a synthetic root or edge.
        heap.globals.add(P::encode_ptr(std::ptr::null_mut()));
        for iteration in 0..3 {
            if iteration > 0 {
                if generational {
                    heap.mutator_triggered_minor_gc::<P>(&state);
                }
                heap.mutator_triggered_gc::<P>(&state);
            }
            let expected_owner = P::try_decode_ptr(heap.globals.get(root)).unwrap();
            let pause = heap.pause_world();
            let mut roots = Vec::new();
            heap.visit_roots_with_policy::<P>(&mut |p| roots.push(p));
            assert_eq!(roots, vec![expected_owner]);
            let nodes = collect_graph::<P>(&heap);
            let parent_id = nodes.iter().position(|n| n.obj == expected_owner).unwrap();
            let child_id = 1 - parent_id;
            assert_eq!(nodes.len(), 2);
            assert_eq!(nodes[parent_id].refs, vec![child_id; 3]);
            drop(pause);
            let text = dump_heap_text_with_policy::<P>(&heap);
            assert!(text.contains(&format!("Parent {{ child: #{child_id}, empty: null, inline: Link {{ child: #{child_id} }} }}")), "{text}");
            let json = dump_heap_json_with_policy::<P>(&heap);
            assert!(json.contains("\"reachable_objects\": 2"), "{json}");
            assert!(
                json.contains(&format!("\"roots\": [{{\"id\": {parent_id},")),
                "{json}"
            );
            assert!(
                json.contains(&format!("\"refs\": [{child_id}, {child_id}, {child_id}]")),
                "{json}"
            );
        }
        heap.safe_deregister_thread(&state);
    }
}

#[test]
fn collector_inspection_decodes_root_graph_and_inline_fields_with_pointer_policy() {
    for generational in [false, true] {
        inspect::<Full, IdentityPtrPolicy>(generational);
        inspect::<Compact, IdentityPtrPolicy>(generational);
        inspect::<Full, LowTag>(generational);
        inspect::<Compact, LowTag>(generational);
        inspect::<Full, HighTag>(generational);
        inspect::<Compact, HighTag>(generational);
    }
}
