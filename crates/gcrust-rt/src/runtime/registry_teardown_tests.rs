use super::*;
use crate::{
    gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo},
    runtime::*,
};
use std::{
    sync::{Arc, mpsc},
    time::Duration,
};
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    BeforeDetach,
    AfterDetach,
}
struct Hook {
    phase: Phase,
    heap: usize,
    ready: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
static SERIAL: Mutex<()> = Mutex::new(());
static HOOK: Mutex<Option<Hook>> = Mutex::new(None);
static DETACHED: Mutex<Option<(usize, mpsc::Sender<()>)>> = Mutex::new(None);
pub(super) fn checkpoint(heap: &Heap, phase: Phase) {
    if phase == Phase::AfterDetach {
        let mut probe = DETACHED.lock().unwrap();
        if probe
            .as_ref()
            .is_some_and(|(id, _)| *id == heap as *const Heap as usize)
        {
            let (_, sender) = probe.take().unwrap();
            sender.send(()).unwrap();
        }
    }
    let hook = {
        let mut hook = HOOK.lock().unwrap();
        if hook
            .as_ref()
            .is_some_and(|h| h.heap == heap as *const Heap as usize && h.phase == phase)
        {
            hook.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        hook.ready.send(()).unwrap();
        hook.release.recv_timeout(Duration::from_secs(10)).unwrap();
    }
}
// This fixture retains heap-owned globals, handoff and arena registry backing;
// execution tables must account for no bytes after shutdown.
fn retained_metadata_storage(heap: &Heap) -> usize {
    let globals = heap.globals.backing_bytes();
    let handoff = heap.thread_handoff_slots.lock().unwrap().backing_bytes();
    // Fixture checkpoints own the registrations or observe them BLOCKED;
    // their backing cannot mutate while the census mutex is held.
    let registrations = {
        let census = heap.threads.lock().unwrap();
        census.capacity() * std::mem::size_of::<crate::gc::ThreadStateHandle>()
            + census.iter().map(|state| unsafe { state.native_backing_bytes() } + crate::gc::ThreadStateHandle::allocation_bytes()).sum::<usize>()
    };
    // Every checkpoint retains one root RuntimeContext, including during finish.
    heap.collector_object_lock_backing_bytes() + heap.collector_layout_backing_bytes() + heap.collector_abi_backing_bytes() + globals + crate::abi_storage::AbiStorage::<std::cell::UnsafeCell<Thread>>::allocation_bytes() + handoff + registrations + heap.retired_profile_backing_bytes() + heap.collector_worker_backing_bytes() + unsafe { heap.collector_tlab_metadata_bytes() }
}
#[test]
fn collector_registry_teardown_reference_detach_allocates_and_frees_nothing() {
    let registry = Mutex::new(vec![1, 2, 3]);
    drop(registry.lock().unwrap());
    let replacement = Vec::new();
    allocation_probe::start();
    let old = detach(&registry, replacement);
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(old, [1, 2, 3]);
    assert!(registry.lock().unwrap().is_empty());
    allocation_probe::start();
    drop(old);
    assert!(allocation_probe::finish_bytes().3 > 0);
}
#[test]
fn collector_registry_teardown_reference_detached_disposal_allows_collection() {
    let _serial = SERIAL.lock().unwrap();
    unsafe extern "C" fn body(_: *mut Thread, _: *mut u8) -> u64 {
        42
    }
    for generational in [false, true] {
        let (publish, receive) = mpsc::channel();
        let (ready, waiting) = mpsc::channel();
        let (release, go) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let info = TypeInfo::for_header(Full::SIZE).with_varlen_values(0);
            let mut runtime = if generational {
                RuntimeContext::new_generational(65536, 1 << 20, vec![info])
            } else {
                RuntimeContext::new(1 << 20, vec![info])
            };
            let heap = runtime.heap().clone();
            let (root, cell, channel, completion) = runtime.enter(|thread| unsafe {
                let witness = ai_gc_alloc_varlen(thread, 0, 0, 0);
                let root = heap.globals.add(witness as u64);
                let cell = ai_atomic_i64_new(thread, 42);
                let channel = ai_chan_new(thread, 64, heap.globals.get(root) as *const u8, 0);
                let completion =
                    ai_thread_spawn(thread, heap.globals.get(root) as *mut u8, body as *const u8);
                assert_eq!(ai_thread_join(thread, completion), 42);
                if generational {
                    // The observer performs a tenured STW collection. Promote
                    // the witness first so both modes test a moving root.
                    heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&*(*thread).dyna_thread);
                }
                let cell =
                    Arc::downgrade(&heap.atomic_cells.lock().unwrap().records[&(cell as usize)].0);
                let channel =
                    Arc::downgrade(&heap.channels.lock().unwrap().records[&(channel as usize)].0);
                let completion = Arc::downgrade(
                    &heap.thread_completions.lock().unwrap().records[&(completion as usize)],
                );
                (root, cell, channel, completion)
            });
            *HOOK.lock().unwrap() = Some(Hook {
                phase: Phase::AfterDetach,
                heap: Arc::as_ptr(&heap) as usize,
                ready,
                release: go,
            });
            let state = unsafe { &*(*runtime.thread_ptr()).dyna_thread }
                as *const crate::gc::ThreadState as usize;
            publish.send((heap.clone(), state, root)).unwrap();
            unsafe {
                runtime.finish_threads();
            }
            assert_eq!(heap.native_registry_bytes(), retained_metadata_storage(&heap));
            assert_eq!(heap.native_buffer_bytes(), 0);
            assert!(cell.upgrade().is_none());
            assert!(channel.upgrade().is_none());
            assert!(completion.upgrade().is_none());
            // Explicit finish may repeat, and a later entry must retain this
            // heap's accounting owner and prepare a fresh retirement epoch.
            unsafe {
                runtime.finish_threads();
            }
            runtime.enter(|thread| unsafe {
                let cell = ai_atomic_i64_new(thread, 43);
                let channel = ai_chan_new(thread, 1, heap.globals.get(root) as *const u8, 0);
                assert_eq!(ai_atomic_i64_load(thread, cell), 43);
                assert!(heap.native_registry_bytes() > retained_metadata_storage(&heap));
                ai_chan_close(thread, channel);
            });
            unsafe {
                runtime.finish_threads();
            }
            assert_eq!(heap.native_registry_bytes(), retained_metadata_storage(&heap));
            assert_eq!(heap.native_buffer_bytes(), 0);
            unsafe {
                heap.globals.set(root, 0);
            }
        });
        let (heap, state, root) = receive.recv_timeout(Duration::from_secs(10)).unwrap();
        waiting.recv_timeout(Duration::from_secs(10)).unwrap();
        let observed = || {
            assert!(unsafe { &*(state as *const crate::gc::ThreadState) }.is_blocked());
            assert!(heap.atomic_cells.try_lock().unwrap().records.is_empty());
            assert!(heap.channels.try_lock().unwrap().records.is_empty());
            assert!(
                heap.thread_completions
                    .try_lock()
                    .unwrap()
                    .records
                    .is_empty()
            );
            assert!(
                heap.native_registry_bytes() > heap.collector_object_lock_backing_bytes() + heap.collector_layout_backing_bytes() + heap.collector_abi_backing_bytes(),
                "backing was freed before native disposal"
            );
            unsafe {
                heap.stw_collect::<IdentityPtrPolicy>();
            }
            assert!(heap.from_space_contains(heap.globals.get(root) as *mut u8));
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(observed));
        release.send(()).unwrap();
        worker.join().unwrap();
        result.unwrap();
    }
}

#[test]
fn collector_registry_teardown_reference_detachment_waits_for_collector_exclusion() {
    let _serial = SERIAL.lock().unwrap();
    let (publish, receive) = mpsc::channel();
    let (ready, waiting) = mpsc::channel();
    let (release, go) = mpsc::channel();
    let (detached, observed) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut runtime = RuntimeContext::new(8192, vec![]);
        let heap = runtime.heap().clone();
        runtime.enter(|thread| unsafe {
            ai_atomic_i64_new(thread, 42);
        });
        *HOOK.lock().unwrap() = Some(Hook {
            phase: Phase::BeforeDetach,
            heap: Arc::as_ptr(&heap) as usize,
            ready,
            release: go,
        });
        *DETACHED.lock().unwrap() = Some((Arc::as_ptr(&heap) as usize, detached));
        publish.send(heap.clone()).unwrap();
        unsafe {
            runtime.finish_threads();
        }
        assert_eq!(heap.native_registry_bytes(), retained_metadata_storage(&heap));
    });
    let heap = receive.recv_timeout(Duration::from_secs(10)).unwrap();
    waiting.recv_timeout(Duration::from_secs(10)).unwrap();
    let pause = heap.pause_world();
    release.send(()).unwrap();
    let detached_early = observed.recv_timeout(Duration::from_millis(100)).is_ok();
    let retained = !heap.atomic_cells.lock().unwrap().records.is_empty();
    drop(pause);
    if !detached_early {
        observed.recv_timeout(Duration::from_secs(10)).unwrap();
    }
    worker.join().unwrap();
    assert!(
        !detached_early,
        "registry slots detached before collector exclusion ended"
    );
    assert!(
        retained,
        "registry ownership disappeared while collector held borrowed slots"
    );
}
