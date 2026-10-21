//! Execution-owned native cleanup. Body-exit notices and GC-discovered orphans
//! index weak records; one worker probes bounded batches and joins outside
//! collector/registry locks. Active bodies need no notification-driven polling.
use super::{Arc, Condvar, JoinHandle, Mutex, abort_on_cleanup_panic};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{sync::Weak, time::Duration};
#[path = "completion_catalogue.rs"]
mod catalogue;
use catalogue::{Catalogue, Node};

const BATCH_SIZE: usize = 64;
struct State {
    catalogue: Catalogue,
    closed: bool,
    #[cfg(test)]
    max_batch: usize,
}
/// Weak borrows only: probing cannot prolong the native completion lifetime.
/// Fixed storage keeps allocation out of the collector-visible monitor lock.
struct Batch {
    entries: [Option<(usize, Weak<JoinHandle>)>; BATCH_SIZE],
    len: usize,
}
impl Batch {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.len
    }
    fn is_empty(&self) -> bool {
        self.len == 0
    }
    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = &(usize, Weak<JoinHandle>)> {
        self.entries.iter().flatten()
    }
}
impl IntoIterator for Batch {
    type Item = (usize, Weak<JoinHandle>);
    type IntoIter = std::iter::Flatten<std::array::IntoIter<Option<Self::Item>, BATCH_SIZE>>;
    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter().flatten()
    }
}
/// One prepared node per completion; neither enqueue nor GC retirement allocates.
pub(super) struct Registration {
    monitor: Arc<Monitor>,
    node: Arc<Node>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        abort_on_cleanup_panic(|| self.monitor.remove_node(&self.node));
    }
}
pub(super) struct Monitor {
    state: Mutex<State>,
    changed: Condvar,
    #[cfg(test)]
    active: AtomicUsize,
    #[cfg(test)]
    joining: AtomicUsize,
    #[cfg(test)]
    deferred: AtomicUsize,
}
impl Monitor {
    pub fn prepare(self: &Arc<Self>, handle: &Arc<JoinHandle>) {
        let registration = handle.reaper_registration.get_or_init(|| Registration {
            monitor: self.clone(),
            node: Node::new(handle),
        });
        assert!(
            Arc::ptr_eq(self, &registration.monitor),
            "completion belongs to another reaper"
        );
    }
    pub fn enqueue(&self, handle: &Arc<JoinHandle>) {
        let registration = handle
            .reaper_registration
            .get()
            .expect("completion node not prepared");
        assert!(std::ptr::eq(self, Arc::as_ptr(&registration.monitor)));
        let mut state = self.state.lock().unwrap();
        assert!(
            !state.closed,
            "completion admitted to a closed execution reaper"
        );
        // Registration identity above proves ownership; state holds its monitor.
        if unsafe { state.catalogue.insert(&registration.node) } {
            self.changed.notify_one();
        }
    }
    fn remove_node(&self, node: &Arc<Node>) {
        let mut state = self.state.lock().unwrap();
        if !state.closed {
            // Registration owns node through this call and unlocks before drop.
            unsafe {
                state.catalogue.remove(node);
            }
        }
    }
    fn forget(handle: &JoinHandle) {
        // Worker TLS can admit descendants into a new reaper during shutdown.
        // GC may still retire older records: dispatch to their actual owner.
        if let Some(registration) = handle.reaper_registration.get() {
            registration.monitor.remove_node(&registration.node);
        }
    }
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.state.lock().unwrap().catalogue.is_empty() && self.active.load(Ordering::Acquire) == 0
    }
    #[cfg(test)]
    pub fn is_joining(&self, key: usize) -> bool {
        self.joining.load(Ordering::Acquire) == key
    }
    #[cfg(test)]
    pub fn is_deferred(&self, key: usize) -> bool {
        self.deferred.load(Ordering::Acquire) == key
    }
    #[cfg(test)]
    pub fn max_batch(&self) -> usize {
        self.state.lock().unwrap().max_batch
    }
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.state.lock().unwrap().catalogue.len()
    }
    fn run(&self) {
        let mut progress = false;
        let mut delay = Duration::from_millis(1);
        loop {
            let mut state = self.state.lock().unwrap();
            while state.catalogue.is_empty() && !state.closed {
                state = self.changed.wait(state).unwrap();
                state.catalogue.rewind();
                progress = false;
                delay = Duration::from_millis(1);
            }
            if state.closed {
                break;
            }
            let batch = unsafe { state.catalogue.batch() };
            #[cfg(test)]
            {
                state.max_batch = state.max_batch.max(batch.len());
            }
            if batch.is_empty() {
                state.catalogue.rewind();
                if !progress {
                    let (_state, timeout) = self.changed.wait_timeout(state, delay).unwrap();
                    delay = if timeout.timed_out() {
                        (delay * 2).min(Duration::from_millis(100))
                    } else {
                        Duration::from_millis(1)
                    };
                }
                progress = false;
                continue;
            }
            drop(state);
            for (_key, weak) in batch {
                #[cfg(test)]
                self.active.store(_key, Ordering::Release);
                // No catalogue lock is held through OS join, native panic-payload
                // destruction, or any callback/reentry those might perform.
                let handle = weak.upgrade();
                let complete = handle.as_ref().is_none_or(|handle| {
                    handle.try_join_finished(|| {
                        #[cfg(test)]
                        self.joining.store(_key, Ordering::Release);
                    })
                });
                #[cfg(test)]
                if !complete {
                    self.deferred.store(_key, Ordering::Release);
                }
                if complete {
                    if let Some(handle) = &handle {
                        Self::forget(handle);
                    }
                    progress = true;
                    delay = Duration::from_millis(1);
                }
                drop(handle);
                #[cfg(test)]
                {
                    self.joining.store(0, Ordering::Release);
                    self.active.store(0, Ordering::Release);
                }
            }
        }
    }
}
pub(super) struct Reaper {
    monitor: Arc<Monitor>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Reaper {
    pub fn new() -> Self {
        let monitor = Arc::new(Monitor {
            state: Mutex::new(State {
                catalogue: Catalogue::default(),
                closed: false,
                #[cfg(test)]
                max_batch: 0,
            }),
            changed: Condvar::new(),
            #[cfg(test)]
            active: AtomicUsize::new(0),
            #[cfg(test)]
            joining: AtomicUsize::new(0),
            #[cfg(test)]
            deferred: AtomicUsize::new(0),
        });
        drop(monitor.state.lock().unwrap());
        monitor.changed.notify_all();
        let target = monitor.clone();
        let worker = std::thread::Builder::new()
            .name("gcr-native-reaper".into())
            .spawn(move || abort_on_cleanup_panic(|| target.run()))
            .expect("cannot start native completion reaper");
        Self {
            monitor,
            worker: Some(worker),
        }
    }
    pub fn enqueue(&self, handle: &Arc<JoinHandle>) {
        self.monitor.enqueue(handle);
    }
    pub fn forget(&self, handle: &JoinHandle) {
        Monitor::forget(handle);
    }
    pub fn monitor(&self) -> Arc<Monitor> {
        self.monitor.clone()
    }
}
impl Drop for Reaper {
    fn drop(&mut self) {
        abort_on_cleanup_panic(|| {
            let detached = {
                let mut state = self.monitor.state.lock().unwrap();
                state.closed = true;
                self.monitor.changed.notify_all();
                std::mem::take(&mut state.catalogue)
            };
            drop(detached);
            if let Some(worker) = self.worker.take() {
                assert_ne!(
                    worker.thread().id(),
                    std::thread::current().id(),
                    "native reaper cannot own its execution"
                );
                worker.join().expect("native completion reaper failed");
            }
        });
    }
}
