use super::*;
use crate::gc::Full;
use std::collections::BTreeSet;

#[test]
fn collector_thread_identity_is_not_reused_after_retirement() {
    let heap = Heap::new::<Full>(4096, vec![]);
    let (first, first_id) = heap.register_thread();
    let (second, second_id) = heap.register_thread();
    let (third, third_id) = heap.register_thread();
    unsafe {
        heap.safe_deregister_thread(&first);
    }
    let (fourth, fourth_id) = heap.register_thread();
    for state in [&second, &third, &fourth] {
        unsafe {
            heap.safe_deregister_thread(state);
        }
    }
    assert_eq!(
        [first_id, second_id, third_id, fourth_id]
            .into_iter()
            .collect::<BTreeSet<_>>()
            .len(),
        4
    );
}

#[test]
fn collector_thread_identity_is_unique_across_heaps_sharing_a_tracer() {
    let tracer = Arc::new(StatemapTracer::new());
    let first = Heap::new_with_tracer::<Full>(4096, vec![], tracer.clone());
    let second = Heap::new_with_tracer::<Full>(4096, vec![], tracer);
    let (a, a_id) = first.register_thread();
    let (b, b_id) = second.register_thread();
    unsafe {
        first.safe_deregister_thread(&a);
        second.safe_deregister_thread(&b);
    }
    assert_ne!(a_id, b_id);
}

#[test]
fn collector_thread_trace_keeps_registered_identity_after_list_compaction() {
    for concurrent in [false, true] {
        let tracer = Arc::new(StatemapTracer::new());
        let heap = Heap::new_with_tracer::<Full>(4096, vec![], tracer.clone());
        let (first, _) = heap.register_thread();
        let (second, second_id) = heap.register_thread();
        let (third, third_id) = heap.register_thread();
        unsafe {
            heap.safe_deregister_thread(&first);
            second.enter_blocked();
            third.enter_blocked();
            if concurrent {
                heap.concurrent_collect::<IdentityPtrPolicy>();
            } else {
                heap.stw_collect::<IdentityPtrPolicy>();
            }
            second.exit_blocked(&heap);
            third.exit_blocked(&heap);
            heap.safe_deregister_thread(&second);
            heap.safe_deregister_thread(&third);
        }
        let mut output = Vec::new();
        tracer.write_to(&mut output).unwrap();
        let trace = String::from_utf8(output).unwrap();
        let actual: BTreeSet<usize> = trace
            .lines()
            .filter_map(|line| {
                line.split_once("\"entity\":\"thread")
                    .map(|(_, id)| id.split('"').next().unwrap().parse().unwrap())
            })
            .collect();
        assert_eq!(
            actual,
            [second_id, third_id].into(),
            "concurrent={concurrent}: {trace}"
        );
    }
}
