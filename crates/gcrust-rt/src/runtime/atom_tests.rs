use super::*;
use crate::gc::ObjHeader;
use std::sync::atomic::{AtomicBool, AtomicUsize};

#[test]
fn collector_atom_reference_abi_survives_concurrent_cycles() {
    const HELPER: &str = "GCR_ATOM_REFERENCE_ABI_HELPER";
    if std::env::var_os(HELPER).is_none() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::atom_tests::collector_atom_reference_abi_survives_concurrent_cycles",
                "--nocapture",
            ])
            .env(HELPER, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!(
                    "Atom reference ABI hung: {:?}",
                    child.wait_with_output().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    // Rendezvous at pointer decoding during the concurrent scan, after its
    // owner stripe has been released. This keeps the phase open until the
    // RUNNING worker completes an iteration; tiny heaps must not accidentally
    // validate only idle accesses between the two pauses.
    use crate::gc::PtrPolicy;
    use std::sync::atomic::AtomicPtr;
    use std::sync::{Mutex, OnceLock};
    static HEAP: AtomicPtr<Heap> = AtomicPtr::new(std::ptr::null_mut());
    static JOIN_TEXT: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
    static VISITED: AtomicBool = AtomicBool::new(false);
    static SCAN: OnceLock<(
        std::sync::mpsc::Sender<()>,
        Mutex<std::sync::mpsc::Receiver<()>>,
    )> = OnceLock::new();
    struct ScheduledIdentity;
    impl PtrPolicy for ScheduledIdentity {
        fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
            let pointer = HEAP.load(Ordering::Acquire);
            if !pointer.is_null() {
                let heap = unsafe { &*pointer };
                if bits == JOIN_TEXT.load(Ordering::Acquire) as u64
                    && heap.barriers_active()
                    && !heap.gc_requested()
                    && !VISITED.swap(true, Ordering::AcqRel)
                {
                    let (ready, resume) = SCAN.get().unwrap();
                    ready.send(()).unwrap();
                    resume.lock().unwrap().recv().unwrap();
                }
            }
            IdentityPtrPolicy::try_decode_ptr(bits)
        }
        fn encode_ptr(pointer: *mut u8) -> u64 {
            IdentityPtrPolicy::encode_ptr(pointer)
        }
    }
    let (scan_ready, scan_waiting) = std::sync::mpsc::channel();
    let (scan_resume, scan_continue) = std::sync::mpsc::channel();
    SCAN.set((scan_ready, Mutex::new(scan_continue))).unwrap();
    let (ready, receive) = std::sync::mpsc::channel();
    let (cycle_ready, cycle_waiting) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let operations = Arc::new(AtomicUsize::new(0));
    let worker_stop = stop.clone();
    let worker_operations = operations.clone();
    let observed_peak = Arc::new(AtomicUsize::new(0));
    let worker_peak = observed_peak.clone();
    let worker = std::thread::spawn(move || {
        let leaf = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
        let atom_info = TypeInfo::for_header(Full::SIZE)
            .with_type_id(1)
            .with_fields(5);
        use crate::gc::{FieldMeta, FieldTy, ScalarKind, TypeKind, TypeMeta, VariantMeta};
        let scalar_infos: Vec<_> = (2..6)
            .map(|id| {
                TypeInfo::for_header(Full::SIZE)
                    .with_type_id(id)
                    .with_raw_bytes(24)
            })
            .collect();
        let mut types = vec![leaf.clone(), atom_info.clone()];
        types.extend_from_slice(&scalar_infos);
        let buffer_info = TypeInfo::for_header(Full::SIZE)
            .with_type_id(6)
            .with_varlen_bytes(0);
        types.push(buffer_info.clone());
        let closure_info = TypeInfo::for_header(Full::SIZE)
            .with_type_id(7)
            .with_fields(2)
            .with_raw_bytes(8);
        let string_info = TypeInfo::for_header(Full::SIZE)
            .with_type_id(8)
            .with_varlen_bytes(0);
        let witness_info = TypeInfo::for_header(Full::SIZE)
            .with_type_id(9)
            .with_varlen_values(0);
        let join_info = TypeInfo::for_header(Full::SIZE)
            .with_type_id(10)
            .with_varlen_values(0);
        types.extend([closure_info.clone(), string_info.clone(), witness_info.clone(), join_info.clone()]);
        let mut runtime = RuntimeContext::new(1 << 18, types);
        let heap = runtime.heap().clone();
        let mut metadata = vec![
            TypeMeta {
                type_id: 0,
                name: "Leaf".into(),
                kind: TypeKind::Enum {
                    tag_offset: Full::ENUM_TAG_OFFSET as u16,
                    variants: vec![VariantMeta {
                        name: "Value".into(),
                        tag: 1,
                        fields: vec![FieldMeta {
                            name: "payload".into(),
                            offset: Full::SIZE as u16,
                            ty: FieldTy::Scalar(ScalarKind::U64),
                        }],
                    }],
                },
            },
            TypeMeta {
                type_id: 1,
                name: "Atom".into(),
                kind: TypeKind::Struct {
                    fields: vec![FieldMeta {
                        name: "value".into(),
                        offset: Full::SIZE as u16,
                        ty: FieldTy::Ref(0),
                    }],
                },
            },
        ];
        for (index, kind) in [
            ScalarKind::U8,
            ScalarKind::U16,
            ScalarKind::U32,
            ScalarKind::U64,
        ]
        .into_iter()
        .enumerate()
        {
            metadata.push(TypeMeta {
                type_id: (index + 2) as u16,
                name: format!("Scalar{index}"),
                kind: TypeKind::Struct {
                    fields: vec![
                        FieldMeta {
                            name: "initial".into(),
                            offset: Full::SIZE as u16,
                            ty: FieldTy::Scalar(kind),
                        },
                        FieldMeta {
                            name: "offset".into(),
                            offset: (Full::SIZE + 8) as u16,
                            ty: FieldTy::Scalar(kind),
                        },
                        FieldMeta {
                            name: "sentinel".into(),
                            offset: (Full::SIZE + 16) as u16,
                            ty: FieldTy::Scalar(ScalarKind::U64),
                        },
                    ],
                },
            });
        }
        for (type_id, name) in [
            (6, "Buffer"),
            (7, "ClosureProbe"),
            (8, "String"),
            (9, "Witness"),
            (10, "JoinArray"),
        ] {
            metadata.push(TypeMeta {
                type_id,
                name: name.into(),
                kind: TypeKind::Opaque,
            });
        }
        heap.set_type_meta(metadata);
        let thread = runtime.thread_ptr();
        let execute = || {
            let first = unsafe { heap.alloc_obj::<Full>(&leaf, 0) };
            let second = unsafe { heap.alloc_obj::<Full>(&leaf, 0) };
            let atom = unsafe { heap.alloc_obj::<Full>(&atom_info, 0) };
            let buffer = unsafe { heap.alloc_obj::<Full>(&buffer_info, 32) };
            unsafe extern "C" fn code_marker(_thread: *mut Thread, _env: *mut u8) -> u64 {
                42
            }
            let closure = unsafe { heap.alloc_obj::<Full>(&closure_info, 0) };
            let witness = unsafe { heap.alloc_obj::<Full>(&witness_info, 1) };
            unsafe {
                closure.add(Full::SIZE).cast::<*mut u8>().write(first);
                closure.add(Full::SIZE + 8).cast::<*mut u8>().write(buffer);
                closure
                    .add(Full::SIZE + 16)
                    .cast::<*const u8>()
                    .write(code_marker as *const () as *const u8);
                witness.add(STR_DATA_OFF).cast::<*mut u8>().write(first);
            }
            let join_array = unsafe { heap.alloc_obj::<Full>(&join_info, 2) };
            let left = unsafe { heap.alloc_obj::<Full>(&string_info, 4) };
            let right = unsafe { heap.alloc_obj::<Full>(&string_info, 5) };
            unsafe {
                left.add(STR_DATA_OFF)
                    .copy_from_nonoverlapping(b"left".as_ptr(), 4);
                right
                    .add(STR_DATA_OFF)
                    .copy_from_nonoverlapping(b"right".as_ptr(), 5);
                join_array.add(STR_DATA_OFF).cast::<*mut u8>().write(left);
                join_array
                    .add(STR_DATA_OFF + 8)
                    .cast::<*mut u8>()
                    .write(left);
                atom.add(Full::SIZE + 32)
                    .cast::<*mut u8>()
                    .write(join_array);
            }
            let left_root = unsafe { heap.globals.add(left as u64) };
            let right_root = unsafe { heap.globals.add(right as u64) };
            let channel = unsafe { ai_chan_new(thread, 1, witness, 0) };
            unsafe { buffer.add(STR_DATA_OFF).write_bytes(0xcc, 32) };
            unsafe {
                atom.add(Full::SIZE).cast::<u64>().write(first as u64);
                atom.add(Full::SIZE + 8).cast::<u64>().write(buffer as u64);
                atom.add(Full::SIZE + 16).cast::<*mut u8>().write(closure);
                atom.add(Full::SIZE + 24).cast::<*mut u8>().write(witness);
                for (pointer, value) in [(first, 17u64), (second, 23u64)] {
                    pointer.add(Full::ENUM_TAG_OFFSET).cast::<u32>().write(1);
                    pointer.add(Full::SIZE).cast::<u64>().write(value);
                }
            }
            let first_root = unsafe { heap.globals.add(first as u64) };
            let second_root = unsafe { heap.globals.add(second as u64) };
            let atom_root = unsafe { heap.globals.add(atom as u64) };
            let buffer_root = unsafe { heap.globals.add(buffer as u64) };
            unsafe {
                heap.globals.add(closure as u64);
                heap.globals.add(witness as u64);
            }
            let scalar_roots: Vec<_> = (0..4)
                .map(|index| {
                    let scalar = unsafe { heap.alloc_obj::<Full>(&scalar_infos[index], 0) };
                    unsafe {
                        scalar
                            .add(Full::SIZE)
                            .cast::<u64>()
                            .write(0xa5a5a5a5a5a5a5a5)
                    };
                    unsafe {
                        scalar
                            .add(Full::SIZE + 8)
                            .cast::<u64>()
                            .write(0xa5a5a5a5a5a5a5a5);
                        scalar
                            .add(Full::SIZE + 16)
                            .cast::<u64>()
                            .write(0xcccccccccccccccc);
                        heap.globals.add(scalar as u64)
                    }
                })
                .collect();
            ready
                .send((heap.clone(), scalar_roots.clone(), buffer_root, left_root))
                .unwrap();
            cycle_ready.send(()).unwrap();
            let mut choose_second = true;
            let mut buffer_sequence = 0u8;
            while !worker_stop.load(Ordering::Acquire) {
                // All local aliases die before the next poll. Roots are reloaded
                // after either pause, just as generated shadow-stack locals are.
                if heap.gc_requested() {
                    unsafe { ai_gc_pollcheck_slow(thread) };
                }
                let atom = heap.globals.get(atom_root) as *mut u8;
                let new = heap.globals.get(if choose_second {
                    second_root
                } else {
                    first_root
                }) as *mut u8;
                let old = unsafe { ai_atom_ref_load(thread, atom) };
                assert_eq!(unsafe { ai_reflect_field_count(thread, old) }, 1);
                assert_eq!(unsafe { ai_reflect_field_kind(thread, old, 0) }, 1);
                assert!(matches!(
                    unsafe { ai_reflect_field_i64(thread, old, 0) },
                    17 | 23
                ));
                let payload = unsafe { ai_reflect_field_i64(thread, old, 0) };
                let changed = if payload == 17 { 23 } else { 17 };
                unsafe { ai_slot_bits64_store(thread, old, Full::SIZE, changed) };
                assert_eq!(
                    unsafe { ai_reflect_field_i64(thread, old, 0) },
                    changed as i64
                );
                assert_eq!(unsafe { ai_atom_ref_cas(thread, atom, old, new) }, 1);
                assert_eq!(unsafe { ai_atom_ref_cas(thread, atom, atom, old) }, 0);
                let observed = unsafe { ai_atom_ref_load(thread, atom) };
                assert!(unsafe { heap.reference_eq(observed, new) });
                unsafe { ai_slot_ref_store(thread, atom, Full::SIZE, old) };
                assert!(unsafe {
                    heap.reference_eq(
                        ai_slot_bits64_load(thread, atom, Full::SIZE) as *mut u8,
                        old,
                    )
                });
                unsafe { ai_slot_ref_store(thread, atom, Full::SIZE, new) };
                macro_rules! exercise_scalar {
                    ($index:expr, $bits:ty, $load:ident, $cas:ident, $slot_load:ident, $slot_store:ident) => {{
                        let scalar = heap.globals.get(scalar_roots[$index]) as *mut u8;
                        let before = if choose_second {
                            0xa5a5a5a5a5a5a5a5u64 as $bits
                        } else {
                            0x5a5a5a5a5a5a5a5au64 as $bits
                        };
                        let after = !before;
                        assert_eq!(unsafe { $load(thread, scalar) }, before);
                        assert_eq!(unsafe { $cas(thread, scalar, before, after) }, 1);
                        assert_eq!(unsafe { $cas(thread, scalar, before, after) }, 0);
                        assert_eq!(unsafe { $load(thread, scalar) }, after);
                        assert_eq!(
                            unsafe { $slot_load(thread, scalar, Full::SIZE + 8) },
                            before
                        );
                        unsafe { $slot_store(thread, scalar, Full::SIZE + 8, after) };
                        assert_eq!(unsafe { $slot_load(thread, scalar, Full::SIZE + 8) }, after);
                        assert_eq!(unsafe { ai_reflect_field_count(thread, scalar) }, 3);
                        assert_eq!(unsafe { ai_reflect_field_kind(thread, scalar, 1) }, 1);
                        assert_eq!(
                            unsafe { ai_reflect_field_i64(thread, scalar, 1) },
                            after as i64
                        );
                    }};
                }
                exercise_scalar!(
                    0,
                    u8,
                    ai_atom_bits8_load,
                    ai_atom_bits8_cas,
                    ai_slot_bits8_load,
                    ai_slot_bits8_store
                );
                exercise_scalar!(
                    1,
                    u16,
                    ai_atom_bits16_load,
                    ai_atom_bits16_cas,
                    ai_slot_bits16_load,
                    ai_slot_bits16_store
                );
                exercise_scalar!(
                    2,
                    u32,
                    ai_atom_bits32_load,
                    ai_atom_bits32_cas,
                    ai_slot_bits32_load,
                    ai_slot_bits32_store
                );
                exercise_scalar!(
                    3,
                    u64,
                    ai_atom_bits64_load,
                    ai_atom_bits64_cas,
                    ai_slot_bits64_load,
                    ai_slot_bits64_store
                );
                // The probe field retains its source alias during copying; only
                // the stopped-world closure rewrites it. Native buffers are private.
                let buffer_alias =
                    unsafe { ai_slot_bits64_load(thread, atom, Full::SIZE + 8) } as *mut u8;
                buffer_sequence = buffer_sequence.wrapping_add(1);
                for bits in [1, 8, 16, 32, 64] {
                    let input: [u8; 24] = std::array::from_fn(|index| {
                        (index as u8)
                            .wrapping_mul(11)
                            .wrapping_add(buffer_sequence)
                            .wrapping_add(bits as u8)
                    });
                    let mut output = [0u8; 24];
                    unsafe {
                        ai_buf_copy_out(thread, buffer_alias, input.as_ptr(), 24, bits);
                        ai_buf_copy_in(thread, buffer_alias, output.as_mut_ptr(), 24, bits);
                    }
                    let expected = input.map(|byte| if bits == 1 { byte & 1 } else { byte });
                    assert_eq!(output, expected);
                    // Resolve independently so copying to and from the same stale
                    // source cannot make a broken pair of helpers appear correct.
                    let access = unsafe { heap.object_access(buffer_alias) };
                    let data =
                        unsafe { std::slice::from_raw_parts(access.as_ptr().add(STR_DATA_OFF), 32) };
                    assert_eq!(&data[..24], &expected);
                    assert!(data[24..].iter().all(|&byte| byte == 0xcc));
                }
                unsafe {
                    ai_buf_copy_in(thread, buffer_alias, std::ptr::null_mut(), 0, 8);
                    ai_buf_copy_out(thread, buffer_alias, std::ptr::null(), 0, 8);
                }
                let closure_alias =
                    unsafe { ai_slot_bits64_load(thread, atom, Full::SIZE + 16) } as *mut u8;
                let witness_alias =
                    unsafe { ai_slot_bits64_load(thread, atom, Full::SIZE + 24) } as *mut u8;
                assert_eq!(unsafe { ai_object_type_id(thread, closure_alias) }, 7);
                assert_eq!(
                    unsafe { ai_closure_code_ptr(thread, closure_alias) },
                    code_marker as *const () as *const u8
                );
                unsafe { (&*channel).validate(thread, witness_alias) };
                choose_second = !choose_second;
                worker_operations.fetch_add(1, Ordering::Release);
                if scan_waiting.try_recv().is_ok() {
                    assert!(heap.barriers_active());
                    // Overflow several local SATB chunks while copying is held
                    // open. Both ordinary stores and reference CAS must flush;
                    // the collector drains their published chunks at its final pause.
                    for _ in 0..1025 {
                        unsafe {
                            ai_slot_ref_store(thread, atom, Full::SIZE, old);
                        }
                        assert_eq!(unsafe { ai_atom_ref_cas(thread, atom, old, new) }, 1);
                        assert!(unsafe { (&*(*thread).dyna_thread).satb_buffer().len() } < 256);
                    }
                    assert!(!heap.satb_queue.is_empty());
                    // Exercise witness decoding while copying is explicitly
                    // held open. Native construction may block, so it belongs
                    // after the collector rendezvous is released.
                    unsafe { (&*channel).validate(thread, witness_alias) };
                    // No managed alias from this iteration is used after allocation.
                    let name = unsafe { ai_type_name(thread, 8, closure_alias) };
                    assert_eq!(unsafe { str_bytes(name) }, b"ClosureProbe");
                    // The scheduler stops on a String edge decoded while scanning
                    // the already-published join array. Its probe field still has
                    // the source alias. Replace both canonical slots, then join via
                    // that alias: reading source bytes must not hide the mutation.
                    let atom = heap.globals.get(atom_root) as *mut u8;
                    let array =
                        unsafe { ai_slot_bits64_load(thread, atom, Full::SIZE + 32) } as *mut u8;
                    let right = heap.globals.get(right_root) as *mut u8;
                    let separator = heap.globals.get(left_root) as *mut u8;
                    unsafe {
                        ai_slot_ref_store(thread, array, STR_DATA_OFF, right);
                        ai_slot_ref_store(thread, array, STR_DATA_OFF + 8, right);
                    }
                    let joined = unsafe { ai_str_join(thread, 8, array, 2, separator) };
                    assert_eq!(unsafe { str_bytes(joined) }, b"rightleftright");
                    // Restore an edge to the watched text for the next cycle.
                    let atom = heap.globals.get(atom_root) as *mut u8;
                    let array =
                        unsafe { ai_slot_bits64_load(thread, atom, Full::SIZE + 32) } as *mut u8;
                    let left = heap.globals.get(left_root) as *mut u8;
                    unsafe {
                        ai_slot_ref_store(thread, array, STR_DATA_OFF, left);
                    }
                    // This private allocation becomes reachable only through a
                    // queued SATB entry after its slot is restored. The final live
                    // byte check requires the collector to consume that chunk.
                    let transient = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
                    unsafe {
                        transient.add(Full::SIZE).cast::<u64>().write(0x5151);
                    }
                    let atom = heap.globals.get(atom_root) as *mut u8;
                    let original = unsafe { ai_atom_ref_load(thread, atom) };
                    unsafe {
                        ai_slot_ref_store(thread, atom, Full::SIZE, transient);
                        ai_slot_ref_store(thread, atom, Full::SIZE, original);
                    }
                    for _ in 0..1025 {
                        unsafe {
                            ai_slot_ref_store(thread, atom, Full::SIZE, original);
                        }
                    }
                    worker_peak.fetch_max(heap.from_used() + heap.nursery_used(), Ordering::Relaxed);
                    scan_resume.send(()).unwrap();
                    // Reload every managed argument before a potentially
                    // collecting constructor. Do not make the concurrent scan
                    // depend on this call returning: exit_blocked may await it.
                    let atom = heap.globals.get(atom_root) as *mut u8;
                    let witness = unsafe { ai_slot_bits64_load(thread, atom, Full::SIZE + 24) } as *mut u8;
                    let fresh = unsafe { ai_chan_new(thread, 1, witness, 0) };
                    assert_eq!(unsafe { (*fresh).array_type }, 9);
                    // The next collector cycle may require RUNNING progress.
                    // Never start it while this constructor is still BLOCKED.
                    cycle_ready.send(()).unwrap();
                }
            }
        };
        unsafe { crate::runtime::with_current_thread(thread, execute) }
    });
    let (heap, scalar_roots, buffer_root, left_root) = receive.recv().unwrap();
    while operations.load(Ordering::Acquire) == 0 {
        std::thread::yield_now();
    }
    HEAP.store(Arc::as_ptr(&heap) as *mut Heap, Ordering::Release);
    for _ in 0..8 {
        cycle_waiting.recv().unwrap();
        JOIN_TEXT.store(heap.globals.get(left_root) as *mut u8, Ordering::Release);
        VISITED.store(false, Ordering::Release);
        unsafe { heap.concurrent_collect::<ScheduledIdentity>() };
        assert!(VISITED.load(Ordering::Acquire));
    }
    HEAP.store(std::ptr::null_mut(), Ordering::Release);
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
    assert_eq!(heap.collections(), 8);
    assert!(observed_peak.load(Ordering::Relaxed) > heap.from_used());
    assert!(
        heap.sampled_heap_peak_bytes() >= observed_peak.load(Ordering::Relaxed),
        "final concurrent pause missed allocation growth"
    );
    assert!(operations.load(Ordering::Acquire) > 0);
    assert_eq!(
        heap.from_used(),
        3 * (Full::SIZE + 8) // includes the last cycle's SATB-only transient
            + (Full::SIZE + 40)
            + 4 * (Full::SIZE + 24)
            + (Full::SIZE + 8 + 32)
            + (Full::SIZE + 16 + 8)
            + (Full::SIZE + 8 + 8)
            + (Full::SIZE + 8 + 16)
            + 2 * (Full::SIZE + 8 + 8)
    );
    let buffer = heap.globals.get(buffer_root) as *mut u8;
    assert_eq!(unsafe { buffer.add(Full::SIZE).cast::<u64>().read() }, 32);
    assert!(
        unsafe { std::slice::from_raw_parts(buffer.add(STR_DATA_OFF + 24), 8) }
            .iter()
            .all(|&byte| byte == 0xcc)
    );
    // Narrow operations must preserve the surrounding bytes through copying.
    for (index, root) in scalar_roots.into_iter().enumerate() {
        let scalar = heap.globals.get(root) as *mut u8;
        for offset in [Full::SIZE, Full::SIZE + 8] {
            let word = unsafe { scalar.add(offset).cast::<u64>().read() };
            let bytes = word.to_ne_bytes();
            assert!(bytes[1 << index..].iter().all(|&byte| byte == 0xa5));
        }
        assert_eq!(
            unsafe { scalar.add(Full::SIZE + 16).cast::<u64>().read() },
            0xcccccccccccccccc
        );
    }
}

fn reference_card_boundary(use_cas: bool) {
    let leaf = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
    let owner_info = TypeInfo::for_header(Full::SIZE)
        .with_type_id(1)
        .with_fields(100);
    let mut runtime = RuntimeContext::new_generational(1 << 16, 1 << 16, vec![leaf.clone(), owner_info.clone()]);
    let heap = runtime.heap().clone();
    let thread = runtime.thread_ptr();
    let execute = || {
        let owner = unsafe { heap.alloc_obj::<Full>(&owner_info, 0) };
        let young = unsafe { heap.alloc_nursery_obj::<Full>(&leaf, 0) };
        unsafe { young.add(Full::SIZE).cast::<u64>().write(777) };
        let offset = owner_info.value_field_offset(99);
        let root = unsafe { heap.globals.add(owner as u64) };
        if use_cas {
            assert_eq!(
                unsafe {
                    heap.compare_exchange_reference(
                        &runtime.thread.registration.dyna,
                        owner,
                        offset,
                        std::ptr::null_mut(),
                        young,
                    )
                },
                Ok(std::ptr::null_mut())
            );
        } else {
            unsafe { ai_slot_ref_store(thread, owner, offset, young) };
        }
        // The only path to young is a field in a later card of this tenured object.
        unsafe { heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&runtime.thread.registration.dyna) };
        let owner = heap.globals.get(root) as *mut u8;
        let promoted = unsafe { owner.add(offset).cast::<*mut u8>().read() };
        assert!(
            heap.from_space_contains(promoted),
            "remembered edge was not promoted"
        );
        assert_ne!(promoted, young);
        assert_eq!(
            unsafe { promoted.add(Full::SIZE).cast::<u64>().read() },
            777
        );
        assert_eq!(heap.nursery_used(), 0);
        assert_eq!(
            heap.from_used(),
            owner_info.allocation_size(0) + leaf.allocation_size(0)
        );
        assert_eq!(heap.minor_collections(), 1);
    };
    unsafe { crate::runtime::with_current_thread(thread, execute) }
}

#[test]
fn collector_reference_cas_preserves_late_card_edge() {
    reference_card_boundary(true);
}

#[test]
fn collector_reference_store_preserves_late_card_edge() {
    reference_card_boundary(false);
}
