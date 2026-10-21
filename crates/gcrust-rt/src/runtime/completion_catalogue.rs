//! Prepared weak nodes. Published links are accessed only under their owning
//! monitor mutex. Admission initializes privately; shutdown closes the monitor
//! before detaching the catalogue and clearing links without that mutex.
use super::{Arc, Batch, JoinHandle, Weak};
use std::cell::UnsafeCell;

pub(super) struct Node {
    record: Weak<JoinHandle>,
    links: UnsafeCell<Links>,
}
struct Links {
    previous: Weak<Node>,
    next: Option<Arc<Node>>,
    linked: bool,
}
// SAFETY: Links are private. Only Catalogue's unsafe operations access them;
// those operations require exclusive access through the owning monitor, or an
// exclusively detached closed catalogue. Record/registration access never reads
// links. The immutable weak record itself is Send + Sync.
unsafe impl Send for Node {}
unsafe impl Sync for Node {}
impl Node {
    pub(super) fn new(record: &Arc<JoinHandle>) -> Arc<Self> {
        Arc::new(Self {
            record: Arc::downgrade(record),
            links: UnsafeCell::new(Links {
                previous: Weak::new(),
                next: None,
                linked: false,
            }),
        })
    }
}
#[derive(Default)]
pub(super) struct Catalogue {
    head: Option<Arc<Node>>,
    tail: Option<Arc<Node>>,
    cursor: Option<Arc<Node>>,
    len: usize,
    // None starts a fresh pass; removals do not extend its visit budget.
    remaining: Option<usize>,
}
impl Catalogue {
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.len
    }
    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub(super) fn rewind(&mut self) {
        self.cursor = self.head.clone();
        self.remaining = Some(self.len);
    }
    /// # Safety
    /// Each node must belong to one monitor for its entire lifetime. The caller
    /// holds that monitor's state mutex, serializing all link access. It must
    /// close the monitor before detaching/dropping the catalogue off-lock.
    pub(super) unsafe fn insert(&mut self, node: &Arc<Node>) -> bool {
        let links = unsafe { &mut *node.links.get() };
        if links.linked {
            return false;
        }
        assert!(links.next.is_none() && links.previous.upgrade().is_none());
        links.previous = self.tail.as_ref().map(Arc::downgrade).unwrap_or_default();
        links.linked = true;
        if let Some(tail) = &self.tail {
            assert!(!Arc::ptr_eq(tail, node), "catalogue self-link");
            unsafe {
                (*tail.links.get()).next = Some(node.clone());
            }
        } else {
            self.head = Some(node.clone());
            self.cursor = Some(node.clone());
            self.remaining = None;
        }
        self.tail = Some(node.clone());
        self.len += 1;
        true
    }
    /// # Safety
    /// Same ownership/exclusion contract as insert. The caller retains node
    /// until after releasing the monitor, so unlinking cannot destroy it here.
    pub(super) unsafe fn remove(&mut self, node: &Arc<Node>) {
        let (previous, next) = {
            let links = unsafe { &mut *node.links.get() };
            if !links.linked {
                return;
            }
            let previous = links.previous.upgrade();
            let next = links.next.take();
            links.previous = Weak::new();
            links.linked = false;
            (previous, next)
        };
        assert!(!previous.as_ref().is_some_and(|p| Arc::ptr_eq(p, node)));
        assert!(!next.as_ref().is_some_and(|p| Arc::ptr_eq(p, node)));
        if self
            .cursor
            .as_ref()
            .is_some_and(|cursor| Arc::ptr_eq(cursor, node))
        {
            self.cursor = next.clone();
        }
        if let Some(previous) = &previous {
            unsafe {
                (*previous.links.get()).next = next.clone();
            }
        } else {
            self.head = next.clone();
        }
        if let Some(next) = &next {
            unsafe {
                (*next.links.get()).previous =
                    previous.as_ref().map(Arc::downgrade).unwrap_or_default();
            }
        } else {
            self.tail = previous.clone();
        }
        self.len -= 1;
        // Every neighbour remains in the chain; these borrows cannot destroy
        // another node while the caller holds the monitor.
    }
    /// # Safety
    /// Same ownership/exclusion contract as insert. The chain's strong links
    /// keep every borrowed node alive until the final cursor clone is stored.
    pub(super) unsafe fn batch(&mut self) -> Batch {
        let mut batch = Batch {
            entries: std::array::from_fn(|_| None),
            len: 0,
        };
        let mut current = self.cursor.as_ref();
        let mut remaining = self.remaining.unwrap_or(self.len);
        for slot in &mut batch.entries {
            if remaining == 0 {
                break;
            }
            let Some(node) = current else {
                break;
            };
            let links = unsafe { &*node.links.get() };
            assert!(links.linked);
            *slot = Some((node.record.as_ptr() as usize, node.record.clone()));
            batch.len += 1;
            remaining -= 1;
            current = links.next.as_ref();
        }
        self.remaining = Some(remaining);
        let next = current.cloned();
        self.cursor = next;
        batch
    }
}

impl Drop for Catalogue {
    fn drop(&mut self) {
        // Shutdown detaches this entire owner outside the monitor mutex.
        // Break links iteratively: destruction must not recurse with backlog.
        self.tail = None;
        self.cursor = None;
        while let Some(node) = self.head.take() {
            // Closed/detached ownership excludes every published link access.
            let links = unsafe { &mut *node.links.get() };
            self.head = links.next.take();
            links.previous = Weak::new();
            links.linked = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        JoinState, allocation_probe,
        completion_reaper::{Monitor, State},
    };
    use std::sync::Mutex;
    use std::sync::{Condvar, OnceLock, atomic::AtomicUsize};
    fn record(i: usize) -> Arc<JoinHandle> {
        Arc::new(JoinHandle {
            completion_notice: false,
            native_joined: std::sync::atomic::AtomicBool::new(false),
            pending_index: AtomicUsize::new(usize::MAX),
            reaper_registration: OnceLock::new(),
            retirement: OnceLock::new(),
            state: Mutex::new(JoinState::Complete(i as u64)),
            owner: AtomicUsize::new(1),
        })
    }
    fn monitor() -> Arc<Monitor> {
        let monitor = Arc::new(Monitor {
            state: Mutex::new(State {
                catalogue: Catalogue::default(),
                closed: false,
                max_batch: 0,
            }),
            changed: Condvar::new(),
            active: AtomicUsize::new(0),
            joining: AtomicUsize::new(0),
            deferred: AtomicUsize::new(0),
        });
        drop(monitor.state.lock().unwrap());
        monitor.changed.notify_all();
        monitor
    }
    #[test]
    fn collector_reaper_batch_reference_order_bounds_and_weak_lifetimes() {
        assert_eq!(super::super::BATCH_SIZE, 64);
        for count in [0, 1, 63, 64, 65, 257] {
            let records: Vec<_> = (0..count).map(record).collect();
            let nodes: Vec<_> = records.iter().map(Node::new).collect();
            let mut catalogue = Catalogue::default();
            allocation_probe::start();
            for node in &nodes {
                assert!(unsafe { catalogue.insert(node) });
                assert!(!unsafe { catalogue.insert(node) });
            }
            let first = unsafe { catalogue.batch() };
            assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            assert_eq!(first.len(), count.min(64));
            let mut observed: Vec<_> = first
                .into_iter()
                .map(|(key, weak)| {
                    assert_eq!(key, weak.as_ptr() as usize);
                    key
                })
                .collect();
            allocation_probe::start();
            for (i, node) in nodes.iter().enumerate() {
                if i % 3 != 0 && i != 64 && i + 1 != count {
                    continue;
                }
                unsafe {
                    catalogue.remove(node);
                }
                unsafe {
                    catalogue.remove(node);
                }
            }
            assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            loop {
                allocation_probe::start();
                let batch = unsafe { catalogue.batch() };
                assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
                if batch.is_empty() {
                    break;
                }
                assert!(batch.len() <= 64);
                observed.extend(batch.into_iter().map(|(key, _)| key));
            }
            let expected: Vec<_> = records
                .iter()
                .enumerate()
                .filter(|(i, _)| *i < 64 || (i % 3 != 0 && *i != 64 && i + 1 != count))
                .map(|(_, r)| Arc::as_ptr(r) as usize)
                .collect();
            assert_eq!(observed, expected);
            assert_eq!(
                catalogue.len(),
                (0..count)
                    .filter(|i| i % 3 != 0 && *i != 64 && i + 1 != count)
                    .count()
            );
            catalogue.rewind();
            drop(records);
            allocation_probe::start();
            let batch = unsafe { catalogue.batch() };
            assert!(batch.iter().all(|(_, weak)| weak.upgrade().is_none()));
            drop(batch);
            assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
        }
    }
    #[test]
    fn collector_reaper_catalogue_reference_prepared_enqueue_forget_and_reentry_do_not_allocate() {
        for count in [0, 1, 63, 64, 65, 257] {
            let monitor = monitor();
            let records: Vec<_> = (0..count).map(record).collect();
            for record in &records {
                allocation_probe::start();
                monitor.prepare(record);
                let preparation = allocation_probe::finish_bytes();
                if count == 1 {
                    eprintln!(
                        "prepared-node traffic={preparation:?}; node={} bytes; record={} bytes",
                        std::mem::size_of::<Node>(),
                        std::mem::size_of::<JoinHandle>()
                    );
                }
                allocation_probe::start();
                monitor.prepare(record);
                assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            }
            allocation_probe::start();
            for record in &records {
                monitor.enqueue(record);
                monitor.enqueue(record);
            }
            for record in records.iter().step_by(3) {
                Monitor::forget(record);
                Monitor::forget(record);
            }
            for record in records.iter().step_by(3) {
                monitor.enqueue(record);
            }
            assert_eq!(allocation_probe::finish_bytes(), (0, 0, 0, 0));
            assert_eq!(monitor.len(), count);
            assert!(records.iter().all(|r| Arc::strong_count(r) == 1));
            let mut expected: Vec<_> = records
                .iter()
                .enumerate()
                .filter(|(i, _)| i % 3 != 0)
                .map(|(_, r)| Arc::as_ptr(r) as usize)
                .collect();
            expected.extend(records.iter().step_by(3).map(|r| Arc::as_ptr(r) as usize));
            let mut observed = Vec::new();
            let mut state = monitor.state.lock().unwrap();
            state.catalogue.rewind();
            loop {
                let batch = unsafe { state.catalogue.batch() };
                if batch.is_empty() {
                    break;
                }
                observed.extend(batch.into_iter().map(|(key, _)| key));
            }
            drop(state);
            assert_eq!(observed, expected);
            drop(records);
            assert!(
                monitor.is_empty(),
                "record destruction must unlink its prepared node"
            );
            assert_eq!(Arc::strong_count(&monitor), 1);
        }
    }
    #[test]
    fn collector_reaper_catalogue_reference_detached_shutdown_is_iterative() {
        let records: Vec<_> = (0..4096).map(record).collect();
        let nodes: Vec<_> = records.iter().map(Node::new).collect();
        let witnesses: Vec<_> = nodes.iter().map(Arc::downgrade).collect();
        let mut catalogue = Catalogue::default();
        for node in &nodes {
            unsafe { catalogue.insert(node) };
        }
        drop(nodes);
        drop(records);
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || drop(catalogue))
            .unwrap()
            .join()
            .unwrap();
        assert!(witnesses.iter().all(|node| node.upgrade().is_none()));
    }
    #[test]
    fn collector_reaper_catalogue_reference_continuous_arrivals_bound_each_probe_cycle() {
        let records: Vec<_> = (0..513).map(record).collect();
        let nodes: Vec<_> = records.iter().map(Node::new).collect();
        let mut catalogue = Catalogue::default();
        for node in &nodes[..65] {
            unsafe {
                catalogue.insert(node);
            }
        }
        let first = unsafe { catalogue.batch() };
        assert_eq!(first.len(), 64);
        // Arrivals extend the tail while removal destroys the old boundary.
        // The in-progress cycle must still stop after its original work budget.
        for node in &nodes[65..193] {
            unsafe {
                catalogue.insert(node);
            }
        }
        unsafe {
            catalogue.remove(&nodes[64]);
        }
        let second = unsafe { catalogue.batch() };
        assert_eq!(second.len(), 1, "arrivals extended an in-progress cycle");
        assert_eq!(
            second.iter().next().unwrap().0,
            Arc::as_ptr(&records[65]) as usize
        );
        assert!(unsafe { catalogue.batch() }.is_empty());
        catalogue.rewind();
        let oldest = unsafe { catalogue.batch() };
        assert_eq!(
            oldest.into_iter().map(|(key, _)| key).collect::<Vec<_>>(),
            records[..64]
                .iter()
                .map(|r| Arc::as_ptr(r) as usize)
                .collect::<Vec<_>>()
        );
        for node in &nodes[193..] {
            unsafe {
                catalogue.insert(node);
            }
        }
        let mut visited = 64;
        loop {
            let batch = unsafe { catalogue.batch() };
            if batch.is_empty() {
                break;
            }
            visited += batch.len();
            assert!(visited <= 192, "new arrivals bypassed the cycle budget");
        }
        assert_eq!(visited, 192);
        catalogue.rewind();
        assert_eq!(
            unsafe { catalogue.batch() }.iter().next().unwrap().0,
            Arc::as_ptr(&records[0]) as usize
        );
    }
}
