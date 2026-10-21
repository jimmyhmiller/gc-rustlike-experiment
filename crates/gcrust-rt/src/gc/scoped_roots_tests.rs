use super::*;
struct Roots(UnsafeCell<[u64; 2]>);
unsafe impl RootSource for Roots {
    unsafe fn scan_roots(&self, visit: &mut dyn FnMut(*mut u64)) {
        let base = self.0.get().cast::<u64>();
        visit(base);
        visit(unsafe { base.add(1) });
    }
}
fn values(registry: &Registry) -> Vec<u64> {
    let mut values = Vec::new();
    unsafe {
        registry.scan(&mut |p| values.push(p.read()));
    }
    values
}
#[test]
fn collector_scoped_registry_reference_arbitrary_unlink_and_reuse() {
    let registry = Registry::default();
    let a = Roots(UnsafeCell::new([1, 2]));
    let b = Roots(UnsafeCell::new([3, 4]));
    let c = Roots(UnsafeCell::new([5, 6]));
    let aa: &[&dyn RootSource] = &[&a];
    let bb: &[&dyn RootSource] = &[&b];
    let cc: &[&dyn RootSource] = &[&c];
    let a = std::pin::pin!(Source::new(aa));
    let b = std::pin::pin!(Source::new(bb));
    let c = std::pin::pin!(Source::new(cc));
    let ga = unsafe { registry.register(a.as_ref()).unwrap() };
    let gb = unsafe { registry.register(b.as_ref()).unwrap() };
    let gc = unsafe { registry.register(c.as_ref()).unwrap() };
    assert_eq!(values(&registry), [5, 6, 3, 4, 1, 2]);
    drop(gb); // middle
    assert_eq!(values(&registry), [5, 6, 1, 2]);
    drop(ga); // tail
    assert_eq!(values(&registry), [5, 6]);
    drop(gc); // head
    assert!(registry.is_empty());
    let _reused = unsafe { registry.register(b.as_ref()).unwrap() };
    assert_eq!(values(&registry), [3, 4]);
}
#[test]
fn collector_scoped_registry_reference_empty_and_unwind() {
    let registry = Registry::default();
    let empty = std::pin::pin!(Source::new(&[]));
    assert!(unsafe { registry.register(empty.as_ref()) }.is_none());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let roots = Roots(UnsafeCell::new([42, 43]));
        let sources: &[&dyn RootSource] = &[&roots];
        let node = std::pin::pin!(Source::new(sources));
        let _guard = unsafe { registry.register(node.as_ref()).unwrap() };
        assert_eq!(values(&registry), [42, 43]);
        panic!("exercise scoped cleanup");
    }));
    assert!(result.is_err());
    assert!(registry.is_empty());
}
#[test]
fn collector_scoped_registry_reference_forwarding_updates_borrowed_slots() {
    let registry = Registry::default();
    let roots = Roots(UnsafeCell::new([41, 42]));
    let sources: &[&dyn RootSource] = &[&roots];
    let node = std::pin::pin!(Source::new(sources));
    let guard = unsafe { registry.register(node.as_ref()).unwrap() };
    unsafe {
        registry.scan(&mut |p| p.write(p.read() + 1));
    }
    assert_eq!(values(&registry), [42, 43]);
    drop(guard);
    assert_eq!(unsafe { *roots.0.get() }, [42, 43]);
}

#[test]
fn collector_scoped_registry_reference_cross_thread_publication_and_updates() {
    let registry = std::sync::Arc::new(Registry::default());
    let mut workers = Vec::new();
    let mut releases = Vec::new();
    for offset in [0, 10] {
        let (ready, waiting) = std::sync::mpsc::channel();
        let (release, go) = std::sync::mpsc::channel();
        let registry = registry.clone();
        workers.push(std::thread::spawn(move || {
            let roots = Roots(UnsafeCell::new([41 + offset, 42 + offset]));
            let sources: &[&dyn RootSource] = &[&roots];
            let node = std::pin::pin!(Source::new(sources));
            let guard = unsafe { registry.register(node.as_ref()).unwrap() };
            ready.send(()).unwrap();
            // Publication and this wait make the borrowed slots quiescent.
            go.recv().unwrap();
            drop(guard);
            assert_eq!(unsafe { *roots.0.get() }, [42 + offset, 43 + offset]);
        }));
        waiting.recv().unwrap();
        releases.push(release);
    }
    unsafe {
        registry.scan(&mut |p| p.write(p.read() + 1));
    }
    let mut seen = values(&registry);
    seen.sort_unstable();
    assert_eq!(seen, [42, 43, 52, 53]);
    for release in releases {
        release.send(()).unwrap();
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert!(registry.is_empty());
}
