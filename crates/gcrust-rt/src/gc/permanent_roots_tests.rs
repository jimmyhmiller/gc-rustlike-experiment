use super::*;
use crate::{
    gc::{AtomicRootSet, Full, IdentityPtrPolicy, ObjHeader, TypeInfo},
    runtime::allocation_probe,
};
use std::{sync::mpsc, time::Duration};
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Allocate,
    Dispose,
}
struct Hook {
    heap: usize,
    phase: Phase,
    ready: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
static HOOK: Mutex<Option<Hook>> = Mutex::new(None);
pub(super) fn checkpoint(heap: &Heap, phase: Phase) {
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
#[test]
fn collector_permanent_registry_reference_commit_preserves_order_without_allocation() {
    let counter = Arc::<AllocationCounter>::default();
    let root = AtomicRootSet::new();
    let pointer = &root as *const dyn RootSource;
    let mut sources = Sources::with_capacity_in(2, MetadataAllocator::new(&counter));
    sources.push(pointer);
    sources.push(pointer);
    let mut staged = Sources::with_capacity_in(request(&sources), sources.allocator().clone());
    allocation_probe::start();
    assert!(commit(&mut sources, &mut staged, pointer));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(sources.len(), 3);
    assert!(sources.iter().all(|p| std::ptr::eq(*p, pointer)));
    assert!(staged.is_empty());
    assert!(staged.capacity() > 0);
    drop(staged);
    assert_eq!(
        counter.bytes(),
        sources.capacity() * std::mem::size_of::<*const dyn RootSource>()
    );
    drop(sources);
    assert_eq!(counter.bytes(), 0);
}
#[test]
fn collector_permanent_registry_reference_stale_and_foreign_stages_do_not_move_sources() {
    let a = Arc::<AllocationCounter>::default();
    let b = Arc::<AllocationCounter>::default();
    let root = AtomicRootSet::new();
    let pointer = &root as *const dyn RootSource;
    let mut sources = Sources::with_capacity_in(2, MetadataAllocator::new(&a));
    sources.push(pointer);
    sources.push(pointer);
    let mut small = Sources::with_capacity_in(1, MetadataAllocator::new(&a));
    let mut foreign = Sources::with_capacity_in(8, MetadataAllocator::new(&b));
    let before = sources.as_ptr();
    allocation_probe::start();
    assert!(!commit(&mut sources, &mut small, pointer));
    assert!(!commit(&mut sources, &mut foreign, pointer));
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert_eq!(sources.as_ptr(), before);
    assert_eq!(sources.len(), 2);
    assert!(small.is_empty() && foreign.is_empty());
    drop((sources, small, foreign));
    assert_eq!((a.bytes(), b.bytes()), (0, 0));
}
#[test]
fn collector_permanent_registry_reference_growth_and_disposal_allow_collection() {
    for generational in [false, true] {
        for phase in [Phase::Allocate, Phase::Dispose] {
            // Pointees are declared before the heap and stay alive until after
            // its final strong reference is dropped, including failure cleanup.
            let source = Arc::new(AtomicRootSet::new());
            let mut existing = Vec::new();
            let info = TypeInfo::for_header(Full::SIZE).with_raw_bytes(8);
            let heap = Arc::new(if generational {
                Heap::new_generational::<Full>(65536, 1 << 20, vec![info.clone()])
            } else {
                Heap::new::<Full>(1 << 20, vec![info.clone()])
            });
            let object = unsafe {
                if generational {
                    heap.alloc_nursery_obj::<Full>(&info, 0)
                } else {
                    heap.alloc_obj::<Full>(&info, 0)
                }
            };
            unsafe {
                object.add(Full::SIZE).cast::<u64>().write(42);
                source.add(object as u64);
            }
            loop {
                existing.push(Arc::new(AtomicRootSet::new()));
                unsafe {
                    heap.register_permanent_extra(&**existing.last().unwrap());
                }
                let registry = heap.permanent_extras.lock().unwrap();
                if registry.len() == registry.capacity() {
                    break;
                }
            }
            let (publish, receive) = mpsc::channel();
            let (ready, waiting) = mpsc::channel();
            let (release, go) = mpsc::channel();
            let worker_heap = heap.clone();
            let worker_source = source.clone();
            let worker = std::thread::spawn(move || {
                let states: Vec<_> = (0..3).map(|_| worker_heap.register_thread().0).collect();
                unsafe {
                    states[1].enter_blocked();
                }
                *HOOK.lock().unwrap() = Some(Hook {
                    heap: Arc::as_ptr(&worker_heap) as usize,
                    phase,
                    ready,
                    release: go,
                });
                publish
                    .send(
                        states
                            .iter()
                            .map(|s| crate::gc::ThreadStateHandle::as_ptr(s) as usize)
                            .collect::<Vec<_>>(),
                    )
                    .unwrap();
                unsafe {
                    worker_heap.register_permanent_extra(&*worker_source);
                }
                assert!(states[0].is_running() && states[1].is_blocked() && states[2].is_running());
                let object = worker_source.get(0) as *mut u8;
                assert!(worker_heap.from_space_contains(object));
                assert_eq!(unsafe { object.add(Full::SIZE).cast::<u64>().read() }, 42);
                assert!(!worker_heap.saw_extra_roots());
                assert!(worker_heap.scoped_extras.is_empty());
                for state in states {
                    unsafe {
                        worker_heap.safe_deregister_thread(&state);
                    }
                }
            });
            let states = receive.recv_timeout(Duration::from_secs(10)).unwrap();
            waiting.recv_timeout(Duration::from_secs(10)).unwrap();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                assert!(
                    states
                        .iter()
                        .all(|s| unsafe { &*(*s as *const crate::gc::ThreadState) }.is_blocked())
                );
                assert!(heap.permanent_extras.try_lock().is_ok());
                assert!(heap.gc_lock.try_lock().is_ok());
                {
                    let _pause = heap.pause_world();
                    let mut visits = 0;
                    unsafe {
                        heap.visit_roots(&mut |_| visits += 1);
                    }
                    assert_eq!(
                        visits, 1,
                        "pending/permanent handoff lost or duplicated roots"
                    );
                }
                if phase == Phase::Dispose {
                    let installed = heap.permanent_extras.lock().unwrap().capacity()
                        * std::mem::size_of::<*const dyn RootSource>();
                    assert!(
                        heap.native_registry_bytes() > installed + heap.collector_object_lock_backing_bytes() + heap.collector_layout_backing_bytes() + heap.collector_abi_backing_bytes(),
                        "old backing charge ended before physical disposal"
                    );
                }
                if generational {
                    let (observer, _) = heap.register_thread();
                    unsafe {
                        heap.mutator_triggered_minor_gc::<IdentityPtrPolicy>(&observer);
                    }
                    unsafe {
                        heap.safe_deregister_thread(&observer);
                    }
                } else {
                    unsafe {
                        heap.stw_collect::<IdentityPtrPolicy>();
                    }
                }
                assert!(heap.from_space_contains(source.get(0) as *mut u8));
            }));
            release.send(()).unwrap();
            worker.join().unwrap();
            result.unwrap();
            let bytes = heap.permanent_extras.lock().unwrap().capacity()
                * std::mem::size_of::<*const dyn RootSource>();
            assert_eq!(heap.native_registry_bytes(), bytes + heap.collector_worker_backing_bytes() + heap.collector_object_lock_backing_bytes() + heap.collector_layout_backing_bytes() + heap.collector_abi_backing_bytes());
            let counter = heap.native_memory.registry.clone();
            let weak = Arc::downgrade(&heap);
            drop(heap);
            assert!(weak.upgrade().is_none());
            assert_eq!(counter.bytes(), 0);
        }
    }
}

#[test]
fn collector_permanent_registry_reference_spare_capacity_stays_running_without_allocation() {
    let first = AtomicRootSet::new();
    let second = AtomicRootSet::new();
    let heap = Heap::new::<Full>(8192, vec![]);
    unsafe {
        heap.register_permanent_extra(&first);
    }
    let (state, _) = heap.register_thread();
    let backing = heap.permanent_extras.lock().unwrap().as_ptr();
    allocation_probe::start();
    unsafe {
        heap.register_permanent_extra(&second);
    }
    assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
    assert!(state.is_running());
    assert_eq!(heap.permanent_extras.lock().unwrap().as_ptr(), backing);
    assert!(!heap.saw_extra_roots());
    unsafe {
        heap.safe_deregister_thread(&state);
    }
}
