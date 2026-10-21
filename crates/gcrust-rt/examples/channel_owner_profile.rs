//! Repeated collection of live managed owners and their one-slot queues.
use gcrust_rt::{
    gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo},
    runtime::{self, RuntimeContext},
};
use std::time::Instant;

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let owners: usize = args[0].parse().unwrap();
    let per_owner: usize = args[1].parse().unwrap();
    let total = owners
        .checked_mul(per_owner)
        .expect("profile capacity overflow");
    assert!(owners > 0 && per_owner > 0 && total <= 8192);
    let minor = match args.get(2).map(String::as_str).unwrap_or("major") {
        "major" => false,
        "minor" => true,
        _ => panic!("collection kind must be major or minor"),
    };
    let bound = match args.get(3).map(String::as_str).unwrap_or("bound") {
        "bound" => true,
        "raw" => false,
        _ => panic!("ownership must be bound or raw"),
    };
    let types = vec![
        TypeInfo::for_header(Full::SIZE).with_varlen_values(0),
        TypeInfo::for_header(Full::SIZE)
            .with_type_id(1)
            .with_raw_bytes(8),
    ];
    let mut context = if minor {
        RuntimeContext::new_generational(1 << 20, 32 << 20, types)
    } else {
        RuntimeContext::new(32 << 20, types)
    };
    let heap = context.heap().clone();
    context.enter(|thread| unsafe {
        let state = &*(*thread).dyna_thread;
        let roots = state.scratch_scope();
        let witness = roots.push(runtime::ai_gc_alloc_varlen(thread, 0, 0, 0));
        let mut queues = Vec::new();
        let mut owner_roots = Vec::new();
        for _ in 0..owners {
            let owner = heap.globals.add(runtime::ai_gc_alloc_fixed(thread, 1, 0) as u64);
            owner_roots.push(owner);
            for _ in 0..per_owner {
                let channel = runtime::ai_chan_new(thread, 1, roots.get(witness), 1);
                if bound { runtime::ai_chan_bind_owner(thread, channel, heap.globals.get(owner) as *mut u8); }
                let message = runtime::ai_gc_alloc_fixed(thread, 1, 0);
                message.add(Full::SIZE).cast::<u64>().write(queues.len() as u64);
                assert_eq!(runtime::ai_chan_send(thread, roots.get(witness), channel, message), 0);
                queues.push(channel);
            }
        }
        let round_root = if minor { Some(heap.globals.add(0)) } else { None };
        let collect = || {
            if minor { heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(state); }
            else { heap.mutator_triggered_gc::<IdentityPtrPolicy>(state); }
        };
        collect();
        if minor {
            assert_eq!(heap.nursery_used(), 0);
            assert!(owner_roots.iter().all(|&root| !heap.is_nursery(heap.globals.get(root) as *const u8)));
        }
        let before = if minor { heap.minor_collections() } else { heap.collections() };
        let rounds = 64;
        let start = Instant::now();
        for index in 0..rounds {
            // Admit one young leaf so the minor is actual collection work,
            // while every channel owner and queued message remains tenured.
            if let Some(root) = round_root {
                let value = runtime::ai_gc_alloc_fixed(thread, 1, 0);
                value.add(Full::SIZE).cast::<u64>().write(index as u64);
                heap.globals.set(root, value as u64);
            }
            collect();
            if let Some(root) = round_root {
                let value = heap.globals.get(root) as *const u8;
                assert!(!heap.is_nursery(value));
                assert_eq!(value.add(Full::SIZE).cast::<u64>().read(), index as u64);
            }
        }
        let nanos = start.elapsed().as_nanos();
        let after = if minor { heap.minor_collections() } else { heap.collections() };
        assert_eq!(after - before, rounds);
        assert_eq!(heap.native_buffer_bytes(), total * 8);
        for (index, channel) in queues.into_iter().enumerate() {
            let message = runtime::ai_chan_recv(thread, roots.get(witness), channel);
            assert_eq!(message.add(Full::SIZE).cast::<u64>().read(), index as u64);
            runtime::ai_chan_close(thread, channel);
        }
        assert_eq!(heap.native_buffer_bytes(), 0);
        let kind = if minor { "minor" } else { "major" };
        let ownership = if bound { "bound" } else { "raw" };
        println!("{{\"owners\":{owners},\"per_owner\":{per_owner},\"kind\":\"{kind}\",\"ownership\":\"{ownership}\",\"rounds\":{rounds},\"nanos\":{nanos}}}");
    });
}
