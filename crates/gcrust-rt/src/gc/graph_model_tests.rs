//! Reproducible graph oracle, independent of forwarding and collector traversal.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

struct LowTag;
impl PtrPolicy for LowTag {
    fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
        (bits & 7 == 5).then_some((bits & !7) as *mut u8)
    }
    fn encode_ptr(pointer: *mut u8) -> u64 {
        pointer as u64 | 5
    }
}

// Keep the top bit set in traced values. Forwarding markers belong to object
// headers, never to encoded slots; all traversal must decode before addressing.
struct HighTag;
impl PtrPolicy for HighTag {
    fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
        const MASK: u64 = 0x00ff_ffff_ffff_ffff;
        (bits >> 56 == 0xa5).then_some((bits & MASK) as *mut u8)
    }
    fn encode_ptr(pointer: *mut u8) -> u64 {
        let address = pointer as u64;
        assert_eq!(address >> 56, 0, "fixture pointer exceeds tagged payload");
        address | 0xa500_0000_0000_0000
    }
}

struct Node {
    layout: usize,
    offsets: Vec<usize>,
    edges: Vec<Option<usize>>,
    length: usize,
}

fn random(state: &mut u64, limit: usize) -> usize {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    (*state as usize) % limit
}

fn graph_matrix<H: ObjHeader, P: PtrPolicy>(interior: &'static [u16], generational: bool) {
    let h = H::SIZE;
    // The first layout combines ordinary slots and an embedded raw-region
    // reference; the second exercises empty/small and streaming-wide tails.
    let layouts = [
        TypeInfo::for_header(h)
            .with_fields(2)
            .with_raw_bytes(16)
            .with_interior_ptrs(interior),
        TypeInfo::for_header(h)
            .with_type_id(1)
            .with_raw_bytes(8)
            .with_varlen_values(0),
    ];
    for seed in 1..=16u64 {
        let mut rng = seed;
        let heap = if generational {
            Heap::new_generational::<H>(64 * 1024, 256 * 1024, layouts.to_vec())
        } else {
            Heap::new::<H>(256 * 1024, layouts.to_vec())
        };
        let registration = generational.then(|| heap.register_thread().0);
        let mut nodes = BTreeMap::new();
        let mut pointers = BTreeMap::new();
        let mut next_id = 0;
        let roots = [
            unsafe { heap.globals.add(0) },
            unsafe { heap.globals.add(0) },
            unsafe { heap.globals.add(0) },
            unsafe { heap.globals.add(0) },
        ];
        let immediate = unsafe { heap.globals.add(3) };
        let null = unsafe { heap.globals.add(0) };
        for cycle in 0..6 {
            let first_new = next_id;
            let old_bytes = heap.from_used();
            // New allocations include deliberate garbage; only a rotating subset is
            // eligible for graph edges. Raw addresses never survive a cycle.
            for _ in 0..if cycle == 0 { 96 } else { 24 } {
                let id = next_id;
                next_id += 1;
                let layout = usize::from(id % 2 == 0);
                let length = if id == 0 { 600 } else { random(&mut rng, 8) };
                let offsets = if layout == 0 {
                    vec![h, h + 8, h + 24]
                } else {
                    (0..length)
                        .map(|i| layouts[1].varlen_element_offset(i))
                        .collect()
                };
                let object = unsafe {
                    if generational {
                        heap.alloc_nursery_obj::<H>(&layouts[layout], length)
                    } else {
                        heap.alloc_obj::<H>(&layouts[layout], length)
                    }
                };
                assert!(!object.is_null());
                let id_offset = if layout == 0 { h + 16 } else { h };
                unsafe {
                    object.add(id_offset).cast::<u64>().write(id as u64);
                }
                pointers.insert(id, object);
                nodes.insert(
                    id,
                    Node {
                        layout,
                        edges: vec![None; offsets.len()],
                        offsets,
                        length,
                    },
                );
            }
            let ids: Vec<_> = nodes.keys().copied().collect();
            let eligible: Vec<_> = ids
                .iter()
                .copied()
                .filter(|id| *id == 0 || (id + cycle) % 4 != 3)
                .collect();
            for (&id, node) in &mut nodes {
                for (offset, edge) in node.offsets.iter().zip(&mut node.edges) {
                    *edge = if random(&mut rng, 5) == 0 {
                        None
                    } else {
                        Some(eligible[random(&mut rng, eligible.len())])
                    };
                    unsafe {
                        pointers[&id]
                            .add(*offset)
                            .cast::<u64>()
                            .write(edge.map_or(0, |target| P::encode_ptr(pointers[&target])));
                    }
                    if generational {
                        heap.mark_card_dirty(unsafe { pointers[&id].add(*offset) });
                    }
                }
            }
            // Duplicate roots check identity preservation independently from
            // duplicate edges and cycles; the fourth slot stays null.
            let alias = eligible[random(&mut rng, eligible.len())];
            let root_ids = [Some(0), Some(alias), Some(alias), None];
            for (slot, id) in roots.iter().zip(root_ids) {
                unsafe {
                    heap.globals
                        .set(*slot, id.map_or(0, |id| P::encode_ptr(pointers[&id])));
                }
            }
            let mut expected = BTreeSet::new();
            let mut pending: Vec<_> = root_ids.into_iter().flatten().collect();
            while let Some(id) = pending.pop() {
                if expected.insert(id) {
                    pending.extend(nodes[&id].edges.iter().flatten().copied());
                }
            }
            let bytes: usize = expected
                .iter()
                .map(|id| {
                    let node = &nodes[id];
                    layouts[node.layout].allocation_size(node.length)
                })
                .sum();
            assert!(expected.len() < nodes.len(), "fixture must collect garbage");
            if let Some(thread) = &registration {
                // A minor retains every tenured object, including currently
                // unreachable ones. Their remembered edges can promote nursery
                // objects too. Model that closure separately from major liveness.
                let mut retained = BTreeSet::new();
                let mut pending: Vec<_> = nodes
                    .keys()
                    .copied()
                    .filter(|id| *id < first_new)
                    .chain(root_ids.into_iter().flatten())
                    .collect();
                while let Some(id) = pending.pop() {
                    if retained.insert(id) {
                        pending.extend(nodes[&id].edges.iter().flatten().copied());
                    }
                }
                let promoted: usize = retained
                    .iter()
                    .filter(|id| **id >= first_new)
                    .map(|id| {
                        let node = &nodes[id];
                        layouts[node.layout].allocation_size(node.length)
                    })
                    .sum();
                unsafe {
                    heap.mutator_triggered_minor_gc::<P>(thread);
                }
                assert_eq!(heap.nursery_used(), 0);
                assert_eq!(heap.globals.get(immediate), 3);
                assert_eq!(heap.globals.get(null), 0);
                assert_eq!(
                    heap.from_used(),
                    old_bytes + promoted,
                    "minor seed={seed} cycle={cycle}"
                );
                // Check the relocated graph before a major can hide a minor
                // repair failure or overwrite reset nursery contents.
                let _ = check_graph::<P>(
                    &heap, &nodes, &layouts, &roots, root_ids, &expected, h, seed, cycle,
                );
                // Major collection must now reclaim dead tenured objects and
                // excess promotion reachable only through those objects.
                unsafe {
                    heap.mutator_triggered_gc::<P>(thread);
                }
            } else {
                // No registered mutator or overlapping allocation/access. The
                // same graph goes through both public major collection paths.
                unsafe {
                    if cycle % 2 == 0 {
                        heap.stw_collect::<P>();
                    } else {
                        heap.concurrent_collect::<P>();
                    }
                }
            }
            assert_eq!(heap.globals.get(immediate), 3);
            assert_eq!(heap.globals.get(null), 0);
            assert_eq!(heap.from_used(), bytes, "seed={seed} cycle={cycle}");
            pointers = check_graph::<P>(
                &heap, &nodes, &layouts, &roots, root_ids, &expected, h, seed, cycle,
            );
            nodes.retain(|id, _| expected.contains(id));
        }
        if let Some(thread) = &registration {
            unsafe {
                heap.safe_deregister_thread(thread);
            }
        }
    }
}

#[test]
fn collector_random_graph_oracle_preserves_compact_payloads_and_aliases() {
    bounded_graph_matrix::<Compact, IdentityPtrPolicy>(
        "collector_random_graph_oracle_preserves_compact_payloads_and_aliases",
        &[32],
        false,
    );
}

#[test]
fn collector_random_graph_oracle_preserves_full_payloads_and_aliases() {
    bounded_graph_matrix::<Full, IdentityPtrPolicy>(
        "collector_random_graph_oracle_preserves_full_payloads_and_aliases",
        &[40],
        false,
    );
}

// A queue/termination regression must fail within a deadline even in CI or
// the sanitizer gate, where the outer cargo invocation has no timeout.
fn bounded_graph_matrix<H: ObjHeader, P: PtrPolicy>(
    name: &str,
    interior: &'static [u16],
    generational: bool,
) {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    const CHILD: &str = "GCR_GRAPH_ORACLE_CHILD";
    // Emulated targets cannot re-exec their binary without the target runner.
    // Their gate sets 1 and bounds the entire filtered invocation externally.
    if matches!(std::env::var(CHILD).as_deref(), Ok(value) if value == name || value == "1") {
        graph_matrix::<H, P>(interior, generational);
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("gc::graph_model_tests::{name}"),
            "--nocapture",
        ])
        .env(CHILD, name)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("graph oracle exceeded deadline: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "graph oracle failed: {output:?}");
}

#[test]
fn collector_random_graph_oracle_preserves_compact_promotions_and_remembered_edges() {
    bounded_graph_matrix::<Compact, IdentityPtrPolicy>(
        "collector_random_graph_oracle_preserves_compact_promotions_and_remembered_edges",
        &[32],
        true,
    );
}
#[test]
fn collector_random_graph_oracle_preserves_full_promotions_and_remembered_edges() {
    bounded_graph_matrix::<Full, IdentityPtrPolicy>(
        "collector_random_graph_oracle_preserves_full_promotions_and_remembered_edges",
        &[40],
        true,
    );
}

fn check_graph<P: PtrPolicy>(
    heap: &Heap,
    nodes: &BTreeMap<usize, Node>,
    layouts: &[TypeInfo; 2],
    roots: &[usize; 4],
    root_ids: [Option<usize>; 4],
    expected: &BTreeSet<usize>,
    h: usize,
    seed: u64,
    cycle: usize,
) -> BTreeMap<usize, *mut u8> {
    let mut pointers = BTreeMap::new();
    let mut seen_addresses = BTreeMap::new();
    let mut pending: Vec<_> = roots
        .iter()
        .zip(root_ids)
        .map(|(slot, id)| (heap.globals.get(*slot), id))
        .collect();
    while let Some((bits, id)) = pending.pop() {
        let Some(id) = id else {
            assert_eq!(bits, 0);
            continue;
        };
        let object = P::try_decode_ptr(bits).expect("reference tag was lost during relocation");
        assert_eq!(P::encode_ptr(object), bits);
        assert!(!object.is_null(), "seed={seed} cycle={cycle} id={id}");
        assert!(heap.from_space_contains(object));
        if let Some(previous) = pointers.insert(id, object) {
            assert_eq!(previous, object, "one logical object acquired two copies");
            continue;
        }
        assert!(
            seen_addresses.insert(object as usize, id).is_none(),
            "distinct IDs alias"
        );
        let node = &nodes[&id];
        assert_eq!(unsafe { heap.obj_type_id(object) } as usize, node.layout);
        let id_offset = if node.layout == 0 { h + 16 } else { h };
        assert_eq!(
            unsafe { object.add(id_offset).cast::<u64>().read() },
            id as u64
        );
        if node.layout == 1 {
            assert_eq!(
                unsafe {
                    object
                        .add(layouts[1].varlen_count_offset())
                        .cast::<u64>()
                        .read()
                },
                node.length as u64
            );
        }
        for (offset, target) in node.offsets.iter().zip(&node.edges) {
            pending.push((unsafe { object.add(*offset).cast::<u64>().read() }, *target));
        }
    }
    assert_eq!(&pointers.keys().copied().collect::<BTreeSet<_>>(), expected);
    pointers
}

#[test]
fn collector_tagged_graph_oracle_preserves_compact_major_aliases() {
    bounded_graph_matrix::<Compact, LowTag>(
        "collector_tagged_graph_oracle_preserves_compact_major_aliases",
        &[32],
        false,
    );
}
#[test]
fn collector_tagged_graph_oracle_preserves_full_major_aliases() {
    bounded_graph_matrix::<Full, LowTag>(
        "collector_tagged_graph_oracle_preserves_full_major_aliases",
        &[40],
        false,
    );
}
#[test]
fn collector_tagged_graph_oracle_preserves_compact_promotion_aliases() {
    bounded_graph_matrix::<Compact, LowTag>(
        "collector_tagged_graph_oracle_preserves_compact_promotion_aliases",
        &[32],
        true,
    );
}
#[test]
fn collector_tagged_graph_oracle_preserves_full_promotion_aliases() {
    bounded_graph_matrix::<Full, LowTag>(
        "collector_tagged_graph_oracle_preserves_full_promotion_aliases",
        &[40],
        true,
    );
}

#[test]
fn collector_tagged_graph_oracle_preserves_compact_high_tag_major_aliases() {
    bounded_graph_matrix::<Compact, HighTag>(
        "collector_tagged_graph_oracle_preserves_compact_high_tag_major_aliases",
        &[32],
        false,
    );
}

#[test]
fn collector_tagged_graph_oracle_preserves_compact_high_tag_promotion_aliases() {
    bounded_graph_matrix::<Compact, HighTag>(
        "collector_tagged_graph_oracle_preserves_compact_high_tag_promotion_aliases",
        &[32],
        true,
    );
}

#[test]
fn collector_tagged_graph_oracle_preserves_full_high_tag_major_aliases() {
    bounded_graph_matrix::<Full, HighTag>(
        "collector_tagged_graph_oracle_preserves_full_high_tag_major_aliases",
        &[40],
        false,
    );
}

#[test]
fn collector_tagged_graph_oracle_preserves_full_high_tag_promotion_aliases() {
    bounded_graph_matrix::<Full, HighTag>(
        "collector_tagged_graph_oracle_preserves_full_high_tag_promotion_aliases",
        &[40],
        true,
    );
}
