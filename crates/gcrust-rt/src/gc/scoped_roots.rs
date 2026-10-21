//! Allocation-free publication of pinned, borrowed per-call root sources.
use super::RootSource;
use std::{cell::UnsafeCell, marker::PhantomPinned, pin::Pin, ptr, sync::Mutex};

pub(super) struct Registry {
    list: Mutex<List>,
}
impl Default for Registry {
    fn default() -> Self {
        let registry = Self {
            list: Mutex::new(List::default()),
        };
        // Prepare any platform mutex state during heap construction, before
        // RUNNING publication can require this registry for the first time.
        drop(registry.list.lock().unwrap());
        registry
    }
}
#[derive(Default)]
struct List {
    head: *const Source<'static>,
}
// SAFETY: All links are accessed under Registry::list. The unsafe registration
// contract retains pinned nodes and quiescent sources across collector threads.
// Removal excludes enumeration; callers separately exclude late root writes.
unsafe impl Send for List {}

pub(super) struct Source<'r> {
    sources: &'r [&'r dyn RootSource],
    next: UnsafeCell<*const Source<'static>>,
    _pinned: PhantomPinned,
}
impl<'r> Source<'r> {
    pub(super) fn new(sources: &'r [&'r dyn RootSource]) -> Self {
        Self {
            sources,
            next: UnsafeCell::new(ptr::null()),
            _pinned: PhantomPinned,
        }
    }
}
pub(super) struct Guard<'h, 'n, 'r> {
    registry: &'h Registry,
    entry: Pin<&'n Source<'r>>,
}
impl Registry {
    /// # Safety
    /// Entry is not already registered. Caller is RUNNING or has exclusive
    /// quiescent access before another collector can reclaim these objects.
    /// Sources expose live slots and stay quiescent until removal. BLOCKED or
    /// unregistered callers must exclude collection through guard removal so
    /// every forwarding write to borrowed slots finishes before their release.
    pub(super) unsafe fn register<'h, 'n, 'r>(
        &'h self,
        entry: Pin<&'n Source<'r>>,
    ) -> Option<Guard<'h, 'n, 'r>> {
        if entry.sources.is_empty() {
            return None;
        }
        let pointer = ptr::from_ref(entry.get_ref()).cast::<Source<'static>>();
        let mut list = self.list.lock().unwrap();
        // Pin and the guard's borrow preserve the node and source lifetimes,
        // despite erasing the private registry pointer's lifetime.
        unsafe {
            *entry.next.get() = list.head;
        }
        list.head = pointer;
        Some(Guard {
            registry: self,
            entry,
        })
    }
    /// # Safety
    /// Caller provides collector exclusion and source quiescence. Returned slot
    /// addresses may only be used while registrations remain published, through
    /// the end of all parallel forwarding writes.
    pub(super) unsafe fn scan(&self, visit: &mut dyn FnMut(*mut u64)) {
        let list = self.list.lock().unwrap();
        let mut current = list.head;
        while !current.is_null() {
            // The lock excludes unlink; guards retain every reachable node.
            let source = unsafe { &*current };
            for root in source.sources {
                unsafe {
                    root.scan_roots(visit);
                }
            }
            current = unsafe { *source.next.get() };
        }
    }
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.list.lock().unwrap().head.is_null()
    }
}
impl Drop for Guard<'_, '_, '_> {
    fn drop(&mut self) {
        let target = ptr::from_ref(self.entry.get_ref()).cast::<Source<'static>>();
        let mut list = self.registry.list.lock().unwrap();
        let mut link = &mut list.head as *mut *const Source<'static>;
        // Every reachable node remains pinned and borrowed by its guard. Unlink
        // before ending that borrow, without allocating or freeing metadata.
        unsafe {
            while !(*link).is_null() && *link != target {
                link = (*(*link)).next.get();
            }
            assert_eq!(*link, target, "scoped root registration missing");
            *link = *self.entry.next.get();
            *self.entry.next.get() = ptr::null();
        }
    }
}

#[cfg(test)]
#[path = "scoped_roots_tests.rs"]
mod tests;
