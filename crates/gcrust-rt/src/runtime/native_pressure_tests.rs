use super::*;
use crate::gc::ObjHeader;

#[test]
fn collector_native_pressure_reclaims_old_channel_buffers_before_tenured_exhaustion() {
    let array = TypeInfo::for_header(Full::SIZE).with_varlen_values(0);
    let owner = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_fields(1);
    let mut runtime = RuntimeContext::new_generational(65536, 1 << 20, vec![array, owner]);
    let heap = runtime.heap().clone();
    runtime.enter(|thread| unsafe {
        let state = &*(*thread).dyna_thread;
        let witness = ai_gc_alloc_varlen(thread, 0, 0, 0);
        let root = heap.globals.add(witness as u64);
        for _ in 0..64 {
            let control = ai_chan_new(thread, 8192, heap.globals.get(root) as *const u8, 1);
            let wrapper = ai_gc_alloc_fixed(thread, 1, 0);
            wrapper
                .add(Full::SIZE)
                .cast::<u64>()
                .write(heap.globals.get(root));
            ai_chan_bind_owner(thread, control, wrapper);
            let roots = state.scratch_scope();
            roots.push(wrapper);
            state.set_parked_jit_fp((*thread).top_frame as *const u8);
            heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(state);
            state.clear_parked_jit_fp();
            drop(roots);
        }
        let retained: usize = heap
            .channels
            .lock()
            .unwrap()
            .records
            .values()
            .map(|(c, _)| c.slots.read().unwrap().as_ref().map_or(0, |s| s.len() * 8))
            .sum();
        assert!(
            heap.collections() > 0,
            "native pressure never scheduled a major collection; retained={retained}"
        );
        assert!(
            retained <= (1 << 20) + 65536,
            "native buffers retained {retained} bytes"
        );
        heap.globals.set(root, 0);
    });
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

fn full_closure<H: ObjHeader, P: crate::gc::PtrPolicy>(generational: bool) {
    let owner_info = TypeInfo::for_header(H::SIZE).with_raw_bytes(8);
    let message_info = TypeInfo::for_header(H::SIZE)
        .with_type_id(1)
        .with_fields(1)
        .with_raw_bytes(8);
    let table = vec![owner_info.clone(), message_info.clone()];
    let heap = if generational {
        Heap::new_generational::<H>(8192, 65536, table)
    } else {
        Heap::new::<H>(65536, table)
    };
    let (state, _) = heap.register_thread();
    unsafe {
        let live_owner = heap.alloc_obj::<H>(&owner_info, 0);
        let dead_owner = heap.alloc_obj::<H>(&owner_info, 0);
        let allocate = || {
            if generational {
                heap.alloc_nursery_obj::<H>(&message_info, 0)
            } else {
                heap.alloc_obj::<H>(&message_info, 0)
            }
        };
        let message = allocate();
        message
            .add(H::SIZE)
            .cast::<u64>()
            .write(P::encode_ptr(live_owner));
        message.add(H::SIZE + 8).cast::<u64>().write(42);
        // This dead young reference must not keep an old native owner alive.
        let dead_young = allocate();
        dead_young
            .add(H::SIZE)
            .cast::<u64>()
            .write(P::encode_ptr(dead_owner));
        let live = super::channel_owner_tests::queue(P::encode_ptr(message), &heap.native_retirement);
        let weak = Arc::downgrade(&live);
        let live_handle = heap.channels.lock().unwrap().register(live, &heap.native_memory);
        let dead = super::channel_owner_tests::queue(P::encode_ptr(dead_owner), &heap.native_retirement);
        let dead_weak = Arc::downgrade(&dead);
        let dead_handle = heap.channels.lock().unwrap().register(dead, &heap.native_memory);
        {
            let mut channels = heap.channels.lock().unwrap();
            channels.bind(
                live_handle,
                ChannelOwner {
                    address: live_owner as usize,
                    type_id: 0,
                },
            );
            channels.bind(
                dead_handle,
                ChannelOwner {
                    address: dead_owner as usize,
                    type_id: 0,
                },
            );
        }
        let root = heap.globals.add(P::encode_ptr(live_owner));
        assert_eq!(heap.native_buffer_bytes(), 16);
        heap.mutator_gc_for_native_allocations::<P>(&state, 2 << 20);
        assert_eq!(heap.native_pressure_collections(), 1);
        assert_eq!(heap.collections(), 1);
        assert_eq!(heap.native_buffer_bytes(), 8);
        assert!(
            dead_weak.upgrade().is_none(),
            "dead nursery storage retained old owner"
        );
        let native = weak.upgrade().unwrap();
        let value = native.slots.read().unwrap().as_ref().unwrap()[0].load(Ordering::SeqCst);
        let moved = P::try_decode_ptr(value).unwrap();
        assert!(heap.from_space_contains(moved));
        assert_eq!(moved.add(H::SIZE + 8).cast::<u64>().read(), 42);
        assert_eq!(
            native.owner.load(Ordering::Acquire),
            P::try_decode_ptr(heap.globals.get(root)).unwrap() as usize
        );
        heap.globals.set(root, 0);
        heap.mutator_gc_for_native_allocations::<P>(&state, 8 << 20);
        assert!(heap.channels.lock().unwrap().is_empty());
        assert_eq!(
            heap.native_buffer_bytes(),
            8,
            "native borrow lost its buffer charge"
        );
        drop(native);
        assert_eq!(heap.native_buffer_bytes(), 0);
        assert_eq!(heap.native_buffer_peak_bytes(), 16);
        heap.mutator_gc_for_native_allocations::<P>(&state, 8 << 20);
        assert_eq!(
            heap.native_pressure_collections(),
            2,
            "empty external storage retriggered collection"
        );
        heap.safe_deregister_thread(&state);
    }
}

#[test]
fn collector_native_pressure_preserves_complete_graphs_and_actual_buffer_lifetimes() {
    for generational in [false, true] {
        full_closure::<Full, IdentityPtrPolicy>(generational);
        full_closure::<Full, Tagged>(generational);
        full_closure::<crate::gc::Compact, IdentityPtrPolicy>(generational);
        full_closure::<crate::gc::Compact, Tagged>(generational);
    }
}

#[test]
fn collector_native_pressure_adapts_to_live_buffers_and_close_releases_storage() {
    let array = TypeInfo::for_header(Full::SIZE).with_varlen_values(0);
    let mut runtime = RuntimeContext::new_generational(8192, 65536, vec![array]);
    let heap = runtime.heap().clone();
    runtime.enter(|thread| unsafe {
        let witness = ai_gc_alloc_varlen(thread, 0, 0, 0);
        let root = heap.globals.add(witness as u64);
        // Raw handles retain execution lifetime. Pressure cannot reclaim them,
        // so the threshold must adapt instead of collecting every allocation.
        let big = ai_chan_new(thread, (2 << 20) / 8, witness, 0);
        assert_eq!(heap.native_buffer_bytes(), 2 << 20);
        for _ in 0..64 {
            ai_chan_new(thread, 1, heap.globals.get(root) as *const u8, 0);
        }
        assert_eq!(heap.native_pressure_collections(), 1);
        ai_chan_close(thread, big);
        assert_eq!(heap.native_buffer_bytes(), 64 * 8);
        ai_chan_close(thread, big);
        assert_eq!(heap.native_buffer_bytes(), 64 * 8);
        heap.globals.set(root, 0);
    });
    let memory = heap.native_memory.clone();
    drop(runtime);
    assert_eq!(memory.bytes(), 0);
}

#[test]
fn collector_native_pressure_tracks_closed_queued_storage_and_outliving_native_borrows() {
    let array = TypeInfo::for_header(Full::SIZE).with_varlen_values(0);
    let message = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_raw_bytes(8);
    let mut runtime = RuntimeContext::new(65536, vec![array, message]);
    let weak_heap = Arc::downgrade(runtime.heap());
    let (memory, native) = runtime.enter(|thread| unsafe {
        let witness = ai_gc_alloc_varlen(thread, 0, 0, 0);
        let channel = ai_chan_new(thread, 1, witness, 1);
        let message = ai_gc_alloc_fixed(thread, 1, 0);
        message.add(Full::SIZE).cast::<u64>().write(42);
        assert_eq!(ai_chan_send(thread, witness, channel, message), 0);
        ai_chan_close(thread, channel);
        let heap = &*(*thread).heap;
        assert_eq!(
            heap.native_buffer_bytes(),
            8,
            "close discarded queued buffer charge"
        );
        let received = ai_chan_recv(thread, witness, channel);
        assert_eq!(received.add(Full::SIZE).cast::<u64>().read(), 42);
        assert_eq!(heap.native_buffer_bytes(), 0);
        let other = ai_chan_new(thread, 1, witness, 1);
        let channels = heap.channels.lock().unwrap();
        (
            heap.native_memory.clone(),
            channels.records[&(other as usize)].0.clone(),
        )
    });
    drop(runtime);
    assert!(
        weak_heap.upgrade().is_none(),
        "buffer accounting retained the heap"
    );
    assert_eq!(memory.bytes(), 8);
    drop(native);
    assert_eq!(memory.bytes(), 0);
}

unsafe extern "C" fn native_buffers(thread: *mut Thread, env: *mut u8) -> u64 {
    unsafe {
        let state = &*(*thread).dyna_thread;
        let roots = state.scratch_scope();
        let witness = roots.push(env.add(Full::SIZE).cast::<*mut u8>().read());
        for _ in 0..8 {
            ai_chan_new(thread, 32768, roots.get(witness), 0);
        }
        42
    }
}

#[test]
fn collector_native_pressure_concurrent_requests_preserve_roots_and_coalesce() {
    if std::env::var_os("GCR_NATIVE_PRESSURE_CHILD").is_none() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runtime::native_pressure_tests::collector_native_pressure_concurrent_requests_preserve_roots_and_coalesce", "--nocapture"])
            .env("GCR_NATIVE_PRESSURE_CHILD", "1").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        fn read(mut pipe: impl std::io::Read) -> Vec<u8> {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).unwrap();
            bytes
        }
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let stdout = std::thread::spawn(move || read(stdout));
        let stderr = std::thread::spawn(move || read(stderr));
        let deadline = Instant::now() + Duration::from_secs(20);
        let timeout = loop {
            if child.try_wait().unwrap().is_some() {
                break false;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                break true;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let status = child.wait().unwrap();
        let stdout = stdout.join().unwrap();
        let stderr = stderr.join().unwrap();
        assert!(
            !timeout && status.success(),
            "timeout={timeout}: {status}, {} {}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        return;
    }
    let array = TypeInfo::for_header(Full::SIZE).with_varlen_values(0);
    let env_info = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_fields(1);
    let mut runtime = RuntimeContext::new_generational(8192, 65536, vec![array, env_info]);
    let heap = runtime.heap().clone();
    runtime.enter(|thread| unsafe {
        let state = &*(*thread).dyna_thread;
        let roots = state.scratch_scope();
        let witness = ai_gc_alloc_varlen(thread, 0, 0, 0);
        let witness = roots.push(witness);
        let env = ai_gc_alloc_fixed(thread, 1, 0);
        env.add(Full::SIZE)
            .cast::<*mut u8>()
            .write(roots.get(witness));
        let env = roots.push(env);
        let mut handles = Vec::new();
        for _ in 0..4 {
            handles.push(ai_thread_spawn(
                thread,
                roots.get(env),
                native_buffers as *const () as *const u8,
            ));
        }
        for handle in handles {
            assert_eq!(ai_thread_join(thread, handle), 42);
        }
        assert_eq!(heap.native_buffer_bytes(), 8 << 20);
        assert!(heap.native_pressure_collections() > 0);
        assert!(
            heap.native_pressure_collections() <= 8,
            "pressure requests did not coalesce/adapt"
        );
        assert!(heap.from_space_contains(roots.get(witness)));
    });
    drop(runtime);
    assert_eq!(heap.native_buffer_bytes(), 0);
}

#[test]
fn collector_native_pressure_registry_only_churn_reclaims_dead_atomic_owners() {
    for nursery in [false, true] {
        let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let mut runtime = if nursery {
            RuntimeContext::new_generational(1 << 20, 8 << 20, vec![info])
        } else {
            RuntimeContext::new(8 << 20, vec![info])
        };
        let heap = runtime.heap().clone();
        runtime.enter(|thread| unsafe {
            let raw = ai_atomic_i64_new(thread, 41);
            let kept = ai_atomic_i64_new(thread, 42);
            let owner = ai_gc_alloc_fixed(thread, 0, 0);
            let root = heap.globals.add(owner as u64);
            ai_atomic_i64_bind_owner(thread, kept, owner);
            // Managed allocation remains below the arena limit. Native table
            // demand, with no channel/FFI buffers, must initiate collection.
            for _ in 0..40000 {
                let cell = ai_atomic_i64_new(thread, 0);
                let owner = ai_gc_alloc_fixed(thread, 0, 0);
                ai_atomic_i64_bind_owner(thread, cell, owner);
            }
            assert_eq!(heap.native_buffer_bytes(), 0);
            assert!(heap.native_pressure_collections() > 0);
            assert!(heap.native_pressure_collections() < 8);
            assert_eq!(ai_atomic_i64_load(thread, raw), 41);
            assert_eq!(ai_atomic_i64_load(thread, kept), 42);
            assert!(heap.from_space_contains(heap.globals.get(root) as *mut u8));
            assert!(heap.atomic_cells.lock().unwrap().records.len() < 40000);
            heap.globals.set(root, 0);
        });
    }
}

#[test]
fn collector_native_pressure_spawn_roots_environment_before_registry_collection() {
    unsafe extern "C" fn body(_thread: *mut Thread, env: *mut u8) -> u64 {
        unsafe { env.add(Full::SIZE).cast::<u64>().read() }
    }
    let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let mut runtime = RuntimeContext::new_generational(65536, 1 << 20, vec![info]);
    let heap = runtime.heap().clone();
    runtime.enter(|thread| unsafe {
        // Retained backing models another registrant's in-flight stage. It is
        // real accounted allocation and remains live through pressure retuning.
        let backing = native_metadata::vector::<u64>(
            (1 << 20) / 8 + 1,
            native_metadata::MetadataAllocator::new(&heap.native_memory.registry),
        );
        let env = ai_gc_alloc_fixed(thread, 0, 0);
        env.add(Full::SIZE).cast::<u64>().write(42);
        let before = heap.native_pressure_collections();
        let handle = ai_thread_spawn(thread, env, body as *const u8);
        assert_eq!(ai_thread_join(thread, handle), 42);
        assert_eq!(heap.native_pressure_collections(), before + 1);
        // Surviving registry demand doubles the trigger, preventing a GC on
        // every subsequent native constructor.
        for _ in 0..64 { ai_atomic_i64_new(thread, 0); }
        assert_eq!(heap.native_pressure_collections(), before + 1);
        assert_eq!(heap.native_buffer_bytes(), 0);
        drop(backing);
    });
}

#[test]
fn collector_native_pressure_execution_restart_forgets_released_peak() {
    for generational in [false, true] {
        let info = TypeInfo::for_header(Full::SIZE).with_varlen_values(0);
        let mut runtime = if generational {
            RuntimeContext::new_generational(65536, 4 << 20, vec![info])
        } else { RuntimeContext::new(4 << 20, vec![info]) };
        let heap = runtime.heap().clone();
        let root = runtime.enter(|thread| unsafe {
            let witness = ai_gc_alloc_varlen(thread, 0, 0, 0);
            let root = heap.globals.add(witness as u64);
            let large = ai_ffi_buffer_alloc_v2(thread, 8 << 20);
            let small = ai_ffi_buffer_alloc_v2(thread, 1);
            assert!(heap.native_pressure_collections() > 0);
            ai_ffi_buffer_free_v2(thread, large);
            ai_ffi_buffer_free_v2(thread, small);
            root
        });
        let before = heap.native_pressure_collections();
        let peak = heap.native_buffer_peak_bytes();
        unsafe { runtime.finish_threads(); }
        assert_eq!(heap.native_buffer_bytes(), 0);
        assert_eq!(heap.native_pressure_collections(), before);
        assert_eq!(heap.native_buffer_peak_bytes(), peak);
        runtime.enter(|thread| unsafe {
            for _ in 0..20 {
                ai_chan_new(thread, 8192, heap.globals.get(root) as *const u8, 0);
            }
            assert!(heap.native_pressure_collections() > before,
                "new execution inherited the released 8 MiB footprint");
            assert!(heap.native_pressure_collections() <= before + 2);
            heap.globals.set(root, 0);
        });
    }
}

#[test]
fn collector_native_pressure_long_execution_forgets_released_spike() {
    for generational in [false, true] {
        let info = TypeInfo::for_header(Full::SIZE).with_varlen_values(0);
        let mut runtime = if generational {
            RuntimeContext::new_generational(65536, 4 << 20, vec![info])
        } else { RuntimeContext::new(4 << 20, vec![info]) };
        let heap = runtime.heap().clone();
        runtime.enter(|thread| unsafe {
            let witness = ai_gc_alloc_varlen(thread, 0, 0, 0);
            let root = heap.globals.add(witness as u64);
            let large = ai_ffi_buffer_alloc_v2(thread, 8 << 20);
            let small = ai_ffi_buffer_alloc_v2(thread, 1);
            let before = heap.native_pressure_collections();
            assert!(before > 0);
            ai_ffi_buffer_free_v2(thread, large);
            ai_ffi_buffer_free_v2(thread, small);
            let peak = heap.native_buffer_peak_bytes();
            for _ in 0..20 {
                ai_chan_new(thread, 8192, heap.globals.get(root) as *const u8, 0);
            }
            assert!(heap.native_pressure_collections() > before,
                "long execution inherited released 8 MiB footprint");
            assert!(heap.native_pressure_collections() <= before + 2);
            assert_eq!(heap.native_buffer_peak_bytes(), peak);
            heap.globals.set(root, 0);
        });
    }
}
