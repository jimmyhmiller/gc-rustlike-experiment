//! Native-only destruction of retired buffers, scalar cells and completion records.
//! Each producer reserves an intrusive node before publication. Enqueue neither
//! allocates nor destroys payloads; the worker detaches batches before freeing.
#[path = "retirement_activity.rs"]
mod activity;
#[path = "record_retirement.rs"]
mod records;
pub(in crate::runtime) use records::{Link as RecordLink, Record, Queue as RecordQueue};
pub(in crate::runtime) type CompletionBatch = records::Batch<super::JoinHandle>;
pub(in crate::runtime) type ChannelBatch = records::Batch<super::ChanCtrl>;
#[path = "atomic_retirement.rs"]
mod atomics;
pub(in crate::runtime) use atomics::{Cell as AtomicCell, Batch as AtomicBatch};
use super::{abort_on_cleanup_panic, native_memory::Charge};
use std::sync::{Arc, Condvar, Mutex, atomic::AtomicU64};

type NodeBox = allocator_api2::boxed::Box<Node, super::native_metadata::MetadataAllocator>;

struct Node {
    values: Box<[AtomicU64]>,
    charge: Option<Charge>,
    next: Option<NodeBox>,
    batch: Option<NodeBox>,
}
impl Node {
    fn allocate(self) -> NodeBox {
        let allocator = self.charge.as_ref().map_or_else(
            super::native_metadata::MetadataAllocator::default,
            Charge::metadata_allocator,
        );
        allocator_api2::boxed::Box::new_in(self, allocator)
    }
}

pub(super) struct State {
    head: Option<NodeBox>,
    cells: atomics::Queue,
    pub(super) completions: records::Queue<super::JoinHandle>,
    pub(super) channels: records::Queue<super::ChanCtrl>,
    pending: usize,
    producers: usize,
    owner: bool,
    #[cfg(test)]
    submissions: usize,
}
pub(super) struct Monitor {
    activity: Arc<activity::Activity>,
    state: Mutex<State>,
    changed: Condvar,
    #[cfg(test)]
    hook: Mutex<Option<tests::Hook>>,
    #[cfg(test)]
    finished: std::sync::atomic::AtomicBool,
}
impl Monitor {
    #[cfg(test)]
    fn new() -> Self { Self::with_activity(Arc::new(activity::Activity::new())) }
    fn with_activity(activity: Arc<activity::Activity>) -> Self {
        let monitor = Self {
            activity,
            state: Mutex::new(State {
                head: None,
                cells: Default::default(),
                channels: Default::default(),
                completions: Default::default(),
                pending: 0,
                producers: 0,
                owner: true,
                #[cfg(test)]
                submissions: 0,
            }),
            changed: Condvar::new(),
            #[cfg(test)]
            hook: Mutex::new(None),
            #[cfg(test)]
            finished: std::sync::atomic::AtomicBool::new(false),
        };
        // pthread-backed Condvar notification can lazily allocate. Prepare it
        // in the native BLOCKED constructor, before collector publication.
        monitor.changed.notify_all();
        monitor
    }

    fn enqueue(&self, mut node: NodeBox, count: usize) {
        let mut state = self.state.lock().unwrap();
        state.pending = state
            .pending
            .checked_add(count)
            .expect("native retirement overflow");
        // Consume the producer in the same transaction as queue publication.
        state.producers = state
            .producers
            .checked_sub(count)
            .expect("native producer underflow");
        self.activity.start(count);
        let wake = state.head.is_none();
        #[cfg(test)]
        {
            state.submissions += 1;
        }
        node.batch = state.head.take();
        state.head = Some(node);
        if wake {
            self.changed.notify_all();
        }
    }
    fn enqueue_records<T: Record>(&self, mut records: records::Queue<T>) {
        let count = records.len();
        assert!(count != 0);
        let mut state = self.state.lock().unwrap();
        state.pending = state.pending.checked_add(count).expect("native retirement overflow");
        // Native records share one prepared source per worker epoch. Their
        // queued links keep that producer alive until native consumption.
        self.activity.start(count);
        let mailbox = T::mailbox(&mut state);
        let wake = mailbox.is_empty();
        #[cfg(test)] { state.submissions += 1; }
        records.append(std::mem::take(T::mailbox(&mut state)));
        *T::mailbox(&mut state) = records;
        if wake { self.changed.notify_all(); }
    }
    fn release_producer(&self) {
        let mut state = self.state.lock().unwrap();
        state.producers = state.producers.checked_sub(1).expect("native producer underflow");
        if !state.owner && state.producers == 0 { self.changed.notify_all(); }
    }
    #[cfg(test)]
    fn drain(&self) {
        let mut state = self.state.lock().unwrap();
        while state.pending != 0 {
            state = self.changed.wait(state).unwrap();
        }
    }
    fn run(self: &Arc<Self>) {
        loop {
            let mut state = self.state.lock().unwrap();
            while state.head.is_none() && state.cells.is_empty() && state.channels.is_empty() && state.completions.is_empty() {
                if !state.owner && state.producers == 0 {
                    return;
                }
                state = self.changed.wait(state).unwrap();
            }
            let mut batches = state.head.take();
            let mut cells = std::mem::take(&mut state.cells);
            let mut channels = std::mem::take(&mut state.channels);
            let mut completions = std::mem::take(&mut state.completions);
            drop(state);
            let mut completed = 0;
            while let Some(mut head) = batches {
                batches = head.batch.take();
                let mut items = Some(head);
                while let Some(mut node) = items {
                    items = node.next.take();
                    // Both dimensions are detached before destruction. No
                    // mailbox/registry/collector lock or recursive linked drop.
                    #[cfg(test)]
                    tests::checkpoint(self);
                    drop(node);
                    completed += 1;
                }
            }
            while let Some(cell) = cells.pop() {
                #[cfg(test)] tests::checkpoint(self);
                drop(cell);
                completed += 1;
            }
            while let Some(completion) = completions.pop() {
                #[cfg(test)] tests::checkpoint(self);
                drop(completion);
                completed += 1;
            }
            // These releases are covered by the pending controller jobs.
            let mut backing = unsafe { Batch::native_release(self) };
            while let Some(control) = channels.pop() {
                #[cfg(test)] tests::checkpoint(self);
                super::ChannelRegistry::destroy_retired_control(control, &mut backing);
                completed += 1;
            }
            drop(backing);
            let mut state = self.state.lock().unwrap();
            state.pending = state
                .pending
                .checked_sub(completed)
                .expect("native retirement underflow");
            self.activity.finish(completed);
            if state.pending == 0 {
                self.changed.notify_all();
            }
        }
    }
}
impl Monitor {
    fn release_batch(&self, head: NodeBox, count: usize) {
        {
            let mut state = self.state.lock().unwrap();
            state.producers = state.producers.checked_sub(count).expect("native producer underflow");
            if !state.owner && state.producers == 0 { self.changed.notify_all(); }
        }
        // The caller's existing pending record jobs cover these frees. No new
        // mailbox job or pending decrement occurs until every backing is gone.
        let mut nodes = Some(head);
        let mut completed = 0;
        while let Some(mut node) = nodes {
            nodes = node.next.take();
            assert!(node.batch.is_none(), "published backing in native release batch");
            #[cfg(test)] tests::checkpoint(self);
            drop(node);
            completed += 1;
        }
        assert_eq!(completed, count, "native release batch count mismatch");
    }
}
struct Worker {
    monitor: Arc<Monitor>,
    worker: Option<std::thread::JoinHandle<()>>,
    record_source: Option<Arc<records::Source>>,
}
impl Worker {
    fn new(activity: Arc<activity::Activity>) -> Self {
        let monitor = Arc::new(Monitor::with_activity(activity));
        let target = monitor.clone();
        let worker = std::thread::Builder::new()
            .name("gcr-native-retirement".into())
            .spawn(move || {
                abort_on_cleanup_panic(|| {
                    target.run();
                    #[cfg(test)]
                    target
                        .finished
                        .store(true, std::sync::atomic::Ordering::Release);
                })
            })
            .expect("cannot start native buffer retirement worker");
        Self {
            monitor,
            worker: Some(worker),
            record_source: None,
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        abort_on_cleanup_panic(|| {
            // Drop the epoch's cache before testing whether native observers or
            // unpublished cells still retain its shared producer.
            drop(self.record_source.take());
            let join = {
                let mut state = self.monitor.state.lock().unwrap();
                state.owner = false;
                self.monitor.changed.notify_all();
                state.producers == 0
            };
            let worker = self.worker.take().unwrap();
            if join {
                assert_ne!(worker.thread().id(), std::thread::current().id());
                worker
                    .join()
                    .expect("native buffer retirement worker failed");
            }
            // A native observer may retain storage after Heap destruction.
            // Detach in that case: its producer keeps this native-only monitor
            // alive, and the worker exits once the last producer and work end.
        });
    }
}

#[derive(Default)]
pub(crate) struct Retirement {
    worker: Mutex<Option<Worker>>,
    activity: std::sync::OnceLock<Arc<activity::Activity>>,
}
impl Retirement {
    /// Construct only in a native BLOCKED body, before publishing backing.
    pub(super) fn reserve(&self, values: Box<[AtomicU64]>, charge: Option<Charge>) -> Reservation {
        let monitor = self.reserve_producer();
        Reservation {
            monitor,
            node: Some(Node { values, charge, next: None, batch: None }.allocate()),
        }
    }
    fn activity(&self) -> &Arc<activity::Activity> {
        self.activity.get_or_init(|| Arc::new(activity::Activity::new()))
    }
    /// Native BLOCKED preparation, before acquiring collector exclusion.
    pub(crate) fn prepare_pressure(&self) { self.activity(); }
    /// Caller holds collector exclusion after prepare_pressure. Callback is
    /// nonallocating and acquires no monitor/retirement lock. Publication remains
    /// excluded through the callback, including first-epoch publication.
    pub(crate) fn try_idle(&self, snapshot: impl FnOnce()) -> bool {
        self.activity.get().expect("native pressure activity not prepared").try_idle(snapshot)
    }
    fn reserve_record_source(&self) -> Arc<records::Source> {
        let mut owner = self.worker.lock().unwrap();
        let worker = owner.get_or_insert_with(|| Worker::new(self.activity().clone()));
        worker.record_source.get_or_insert_with(|| records::Source::new(worker.monitor.clone())).clone()
    }
    fn reserve_producer(&self) -> Arc<Monitor> {
        let mut owner = self.worker.lock().unwrap();
        let worker = owner.get_or_insert_with(|| Worker::new(self.activity().clone()));
        let mut state = worker.monitor.state.lock().unwrap();
        assert!(state.owner, "reservation after native retirement owner ended");
        state.producers = state.producers.checked_add(1).expect("native producer overflow");
        worker.monitor.clone()
    }

    #[cfg(test)]
    fn monitor(&self) -> Option<Arc<Monitor>> {
        self.worker
            .lock()
            .unwrap()
            .as_ref()
            .map(|worker| worker.monitor.clone())
    }
    /// Caller is unregistered or BLOCKED and holds no collector/registry lock.
    pub(crate) fn drain(&self) {
        if let Some(activity) = self.activity.get() { activity.drain(); }
    }
    /// Execution boundary: drain and end the owned worker while still BLOCKED.
    /// A later execution may lazily create a fresh worker. Native observers of
    /// the old execution retain its monitor until their final backing retires.
    pub(crate) fn shutdown(&self) {
        let worker = self.worker.lock().unwrap().take();
        self.drain();
        if let Some(worker) = worker { drop(worker); }
    }
}
/// Stack-local aggregation of already reserved nodes. Separate item and batch
/// links let publication prepend an entire group under one O(1) mailbox lock.
#[derive(Default)]
pub(super) struct Batch {
    monitor: Option<Arc<Monitor>>,
    native_monitor: Option<Arc<Monitor>>,
    head: Option<NodeBox>,
    count: usize,
}
impl Batch {
    /// # Safety
    /// Native-only consumer with no collector/registry lock. Its existing
    /// pending record jobs cover all same-monitor releases through batch drop.
    unsafe fn native_release(monitor: &Arc<Monitor>) -> Self {
        Self { native_monitor: Some(monitor.clone()), monitor: None, head: None, count: 0 }
    }
    fn push(&mut self, monitor: &Arc<Monitor>, mut node: NodeBox) {
        if self
            .monitor
            .as_ref()
            .is_some_and(|old| !Arc::ptr_eq(old, monitor))
        {
            self.flush();
        }
        if self.monitor.is_none() {
            self.monitor = Some(monitor.clone());
        }
        self.count = self.count.checked_add(1).expect("native batch overflow");
        node.next = self.head.take();
        self.head = Some(node);
    }
    fn flush(&mut self) {
        if let Some(head) = self.head.take() {
            let count = std::mem::take(&mut self.count);
            let monitor = self.monitor.take().unwrap();
            if self.native_monitor.as_ref().is_some_and(|current| Arc::ptr_eq(current, &monitor)) {
                monitor.release_batch(head, count);
            } else {
                // A different epoch/observer must publish to its own worker.
                monitor.enqueue(head, count);
            }
        }
    }
}
impl Drop for Batch {
    fn drop(&mut self) {
        self.flush();
    }
}

pub(super) struct Reservation {
    monitor: Arc<Monitor>,
    node: Option<NodeBox>,
}
impl Reservation {
    pub(super) fn values(&self) -> &[AtomicU64] {
        &self.node.as_ref().unwrap().values
    }
    pub(super) fn parts(&mut self) -> (&[AtomicU64], &mut Option<Charge>) {
        let Node { values, charge, .. } = &mut **self.node.as_mut().unwrap();
        (values, charge)
    }
    pub(super) fn submit(mut self) {
        let node = self.node.take().unwrap();
        self.monitor.enqueue(node, 1);
    }
    pub(super) fn into_batch(mut self, batch: &mut Batch) {
        let node = self.node.take().unwrap();
        batch.push(&self.monitor, node);
    }
    pub(super) fn release(self) {
        drop(self);
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        // Submitted nodes already consumed the producer under the mailbox lock.
        let Some(node) = self.node.take() else {
            return;
        };
        // Explicit release stays synchronous and BLOCKED; no mailbox lock is
        // held while its backing and charge are destroyed.
        drop(node);
        let mut state = self.monitor.state.lock().unwrap();
        state.producers = state
            .producers
            .checked_sub(1)
            .expect("native producer underflow");
        if !state.owner && state.producers == 0 {
            self.monitor.changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::allocation_probe;
    use crate::{
        gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo},
        runtime::*,
    };
    use std::{
        sync::{atomic::Ordering, mpsc},
        time::{Duration, Instant},
    };
    #[test]
    fn collector_native_retirement_reference_native_release_keeps_record_job_pending() {
        let monitor = Arc::new(Monitor::new());
        let memory = Arc::new(native_memory::NativeMemory::default());
        monitor.state.lock().unwrap().pending = 1; // Existing consumer job.
        monitor.activity.start(1);
        let mut batch = unsafe { Batch::native_release(&monitor) };
        for _ in 0..17 {
            monitor.state.lock().unwrap().producers += 1;
            Reservation {
                monitor: monitor.clone(),
                node: Some(Node {
                    values: vec![AtomicU64::new(0)].into_boxed_slice(),
                    charge: Some(memory.charge(8)), next: None, batch: None,
                }.allocate()),
            }.into_batch(&mut batch);
        }
        assert_eq!(memory.bytes(), 17 * 8);
        drop(batch);
        assert_eq!(memory.bytes(), 0);
        {
            let state = monitor.state.lock().unwrap();
            assert_eq!((state.pending, state.producers, state.submissions), (1, 0, 0));
            assert!(state.head.is_none());
        }
        monitor.state.lock().unwrap().pending = 0; // Consumer completes last.
        monitor.activity.finish(1);
    }

    #[test]
    fn collector_native_retirement_reference_native_release_preserves_foreign_epoch() {
        let current = Arc::new(Monitor::new());
        let foreign = Arc::new(Monitor::new());
        let memory = Arc::new(native_memory::NativeMemory::default());
        current.state.lock().unwrap().pending = 1;
        current.activity.start(1);
        foreign.state.lock().unwrap().producers = 1;
        let mut batch = unsafe { Batch::native_release(&current) };
        Reservation {
            monitor: foreign.clone(),
            node: Some(Node {
                values: vec![AtomicU64::new(0)].into_boxed_slice(),
                charge: Some(memory.charge(8)), next: None, batch: None,
            }.allocate()),
        }.into_batch(&mut batch);
        drop(batch);
        assert_eq!(memory.bytes(), 8, "foreign epoch released without its pending job");
        {
            let state = foreign.state.lock().unwrap();
            assert_eq!((state.pending, state.producers, state.submissions), (1, 0, 1));
        }
        foreign.state.lock().unwrap().owner = false;
        foreign.run();
        assert_eq!(memory.bytes(), 0);
        assert_eq!(foreign.state.lock().unwrap().pending, 0);
        assert_eq!(current.state.lock().unwrap().pending, 1);
        current.state.lock().unwrap().pending = 0;
        current.activity.finish(1);
    }

    #[test]
    fn collector_native_retirement_reference_foreign_publication_keeps_global_activity_pending() {
        let activity = Arc::new(activity::Activity::new());
        let current = Arc::new(Monitor::with_activity(activity.clone()));
        let old = Arc::new(Monitor::with_activity(activity.clone()));
        let memory = Arc::new(native_memory::NativeMemory::default());
        current.state.lock().unwrap().pending = 1;
        activity.start(1); // Current controller-consumer job.
        old.state.lock().unwrap().producers = 1;
        let mut backing = unsafe { Batch::native_release(&current) };
        Reservation {
            monitor: old.clone(),
            node: Some(Node {
                values: vec![AtomicU64::new(0)].into_boxed_slice(),
                charge: Some(memory.charge(8)), next: None, batch: None,
            }.allocate()),
        }.into_batch(&mut backing);
        drop(backing); // Publishes old-epoch backing before completing current.
        current.state.lock().unwrap().pending = 0;
        activity.finish(1);
        assert!(!activity.try_idle(|| panic!("foreign backing was treated as idle")));
        assert_eq!(memory.bytes(), 8);
        old.state.lock().unwrap().owner = false;
        old.run();
        activity.drain();
        assert!(activity.try_idle(|| {}));
        assert_eq!(memory.bytes(), 0);
    }

    #[test]
    fn collector_native_retirement_reference_metadata_follows_physical_node_lifetime() {
        let service = Retirement::default();
        let memory = Arc::new(native_memory::NativeMemory::default());
        let bytes = std::mem::size_of::<Node>();
        let reservation = service.reserve(
            vec![AtomicU64::new(42)].into_boxed_slice(), Some(memory.charge(8)),
        );
        assert_eq!(memory.registry.bytes(), bytes);
        assert_eq!(memory.pressure_bytes(), 8 + bytes);
        let (_, waiting, release) = arm(&service);
        reservation.submit();
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        // The worker has detached the node but not destroyed it. Queue removal
        // must not erase physical demand while the native consumer owns it.
        assert_eq!(memory.registry.bytes(), bytes);
        assert_eq!(memory.bytes(), 8);
        release.send(()).unwrap();
        service.drain();
        assert_eq!(memory.registry.bytes(), 0);
        assert_eq!(memory.bytes(), 0);
        assert_eq!(memory.registry.peak(), bytes);
        // Explicit release uses the same allocation ledger without enqueueing.
        let reservation = service.reserve(
            vec![AtomicU64::new(43)].into_boxed_slice(), Some(memory.charge(8)),
        );
        assert_eq!(memory.registry.bytes(), bytes);
        reservation.release();
        assert_eq!(memory.registry.bytes(), 0);
        assert_eq!(memory.bytes(), 0);
        service.shutdown();
    }

    #[test]
    fn collector_native_retirement_reference_pressure_prepares_first_epoch_activity() {
        let service = Retirement::default();
        service.prepare_pressure();
        assert!(service.monitor().is_none(), "pressure preparation started a worker");
        assert!(service.try_idle(|| {}));
        let prepared = service.activity.get().unwrap().clone();
        let reserved = service.reserve(vec![AtomicU64::new(0)].into_boxed_slice(), None);
        let monitor = service.monitor().unwrap();
        assert!(Arc::ptr_eq(&prepared, &monitor.activity));
        drop(reserved);
        service.shutdown();
    }

    #[test]
    fn collector_native_retirement_reference_drain_includes_closed_epochs() {
        let service = Arc::new(Retirement::default());
        let memory = Arc::new(native_memory::NativeMemory::default());
        let old = service.reserve(vec![AtomicU64::new(0)].into_boxed_slice(), Some(memory.charge(8)));
        let (old_monitor, waiting, release) = arm(&service);
        service.shutdown(); // Reserved native storage keeps this epoch alive.
        let current = service.reserve(vec![AtomicU64::new(0)].into_boxed_slice(), Some(memory.charge(8)));
        assert!(!Arc::ptr_eq(&old_monitor, &service.monitor().unwrap()));
        old.submit();
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(old_monitor.state.lock().unwrap().pending, 1);
        let (started, ready) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let other = service.clone();
        let drainer = std::thread::spawn(move || {
            started.send(()).unwrap();
            other.drain();
            finished.send(()).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let returned_early = done.recv_timeout(Duration::from_millis(100)).is_ok();
        // Always release the deliberately stopped worker before asserting.
        release.send(()).unwrap();
        old_monitor.drain();
        if !returned_early { done.recv_timeout(Duration::from_secs(5)).unwrap(); }
        drainer.join().unwrap();
        assert_eq!(memory.bytes(), 8);
        drop(current);
        service.shutdown();
        assert_eq!(memory.bytes(), 0);
        assert!(!returned_early, "drain ignored already-published old-epoch work");
    }

    #[test]
    fn collector_native_retirement_reference_publication_has_no_rust_allocations() {
        // No worker exists to initialize the notification backend for us.
        // Construction must reserve all synchronization before publication.
        let monitor = Arc::new(Monitor::new());
        let memory = Arc::new(native_memory::NativeMemory::default());
        let mut nodes = Vec::new();
        for _ in 0..64 {
            nodes.push(Reservation {
                monitor: monitor.clone(),
                node: Some(Node {
                    values: vec![AtomicU64::new(0)].into_boxed_slice(),
                    charge: Some(memory.charge(8)),
                    next: None,
                    batch: None,
                }.allocate()),
            });
        }
        monitor.state.lock().unwrap().producers = 64;
        let mut batch = Batch::default();
        allocation_probe::start();
        while let Some(node) = nodes.pop() {
            node.into_batch(&mut batch);
        }
        drop(batch);
        let counts = allocation_probe::finish();
        assert_eq!(
            counts,
            (0, 0),
            "publication must neither allocate nor free Rust storage"
        );
        assert_eq!(memory.bytes(), 64 * 8);
        monitor.state.lock().unwrap().owner = false;
        monitor.run();
        assert_eq!(memory.bytes(), 0);
    }
    pub(super) struct Hook {
        ready: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    pub(super) fn checkpoint(monitor: &Monitor) {
        let hook = monitor.hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook.ready.send(()).unwrap();
            hook.release.recv().unwrap();
        }
    }
    pub(super) fn arm(retirement: &Retirement) -> (Arc<Monitor>, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let monitor = retirement.monitor().unwrap();
        let (ready, waiting) = mpsc::channel();
        let (release, go) = mpsc::channel();
        *monitor.hook.lock().unwrap() = Some(Hook { ready, release: go });
        (monitor, waiting, release)
    }
    pub(super) fn child(name: &str) -> bool {
        if std::env::var_os("GCR_NATIVE_RETIREMENT_CHILD").is_some() {
            return true;
        }
        use std::process::{Command, Stdio};
        for generational in ["0", "1"] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .env("GCR_NATIVE_RETIREMENT_CHILD", generational)
                .env("GCR_GC_VERIFY", "1")
                .env("GCRUST_POISON", "1")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let until = Instant::now() + Duration::from_secs(30);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= until {
                    child.kill().unwrap();
                    panic!(
                        "native retirement stalled: {:?}",
                        child.wait_with_output().unwrap()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "generational={generational}: {out:?}");
        }
        false
    }
    pub(super) fn runtime() -> RuntimeContext {
        let types = vec![
            TypeInfo::for_header(Full::SIZE).with_varlen_values(0),
            TypeInfo::for_header(Full::SIZE)
                .with_type_id(1)
                .with_raw_bytes(8),
        ];
        if std::env::var("GCR_NATIVE_RETIREMENT_CHILD").as_deref() == Ok("1") {
            RuntimeContext::new_generational(32768, 2 << 20, types)
        } else {
            RuntimeContext::new(2 << 20, types)
        }
    }
    pub(super) unsafe fn queue(thread: *mut Thread, capacity: usize, bound: bool) -> *mut ChanCtrl {
        unsafe { queue_with_admission(thread, capacity, bound, true) }
    }
    // Pressure coordination fixtures need native demand that has not already
    // been serviced by constructor admission. Seed it through the ordinary
    // native control/backing primitives, before observers or hooks are armed.
    // The operation under test still uses the complete admission path.
    unsafe fn unserviced_queue(thread: *mut Thread, capacity: usize, bound: bool) -> *mut ChanCtrl {
        unsafe { queue_with_admission(thread, capacity, bound, false) }
    }
    unsafe fn queue_with_admission(thread: *mut Thread, capacity: usize, bound: bool, admit: bool) -> *mut ChanCtrl {
        unsafe {
            let roots = (&*(*thread).dyna_thread).scratch_scope();
            let witness = roots.push(ai_gc_alloc_varlen(thread, 0, 0, 0));
            let control = if admit {
                ai_chan_new(thread, capacity as i64, roots.get(witness), 1)
            } else {
                let heap = &*(*thread).heap;
                blocking_region(thread, || {
                    let channel = channel_sync::construct(capacity, 0, 1,
                        heap.native_memory.charge(capacity * 8), &heap.native_retirement);
                    heap.channels.lock().unwrap().register(channel, &heap.native_memory)
                })
            };
            let message = ai_gc_alloc_fixed(thread, 1, 0);
            message.add(Full::SIZE).cast::<u64>().write(42);
            assert_eq!(
                ai_chan_send(thread, roots.get(witness), control, message),
                0
            );
            if bound {
                let owner = ai_gc_alloc_fixed(thread, 1, 0);
                ai_chan_bind_owner(thread, control, owner);
                roots.push(owner);
                // A major conservatively retains nursery owners. Promote the
                // live owner first so the subsequent major can decide death.
                (&*(*thread).heap)
                    .mutator_triggered_minor_gc::<IdentityPtrPolicy>(&*(*thread).dyna_thread);
            }
            control
        }
    }
    #[test]
    fn collector_native_retirement_reference_grouped_and_mixed_handoffs() {
        let service_a = Retirement::default();
        let service_b = Retirement::default();
        let memory_a = Arc::new(native_memory::NativeMemory::default());
        let memory_b = Arc::new(native_memory::NativeMemory::default());
        let mut a: Vec<_> = (0..64)
            .map(|_| {
                native_memory::NativeSlots::zeroed_reserved(
                    4097,
                    memory_a.charge(4097 * 8),
                    &service_a,
                )
            })
            .collect();
        let mut b: Vec<_> = (0..16)
            .map(|_| {
                native_memory::NativeSlots::zeroed_reserved(
                    4097,
                    memory_b.charge(4097 * 8),
                    &service_b,
                )
            })
            .collect();
        let (monitor_a, waiting_a, release_a) = arm(&service_a);
        let (monitor_b, waiting_b, release_b) = arm(&service_b);
        let mut first = Batch::default();
        for _ in 0..32 {
            a.pop().unwrap().retire_into(&mut first);
        }
        assert_eq!(monitor_a.state.lock().unwrap().pending, 0);
        drop(first);
        waiting_a.recv_timeout(Duration::from_secs(5)).unwrap();
        // Cross-monitor changes flush only the previous group. A's queued
        // batches must retain their item chains behind a later individual node.
        let mut mixed = Batch::default();
        for _ in 0..16 {
            a.pop().unwrap().retire_into(&mut mixed);
        }
        for _ in 0..8 {
            b.pop().unwrap().retire_into(&mut mixed);
        }
        for _ in 0..15 {
            a.pop().unwrap().retire_into(&mut mixed);
        }
        waiting_b.recv_timeout(Duration::from_secs(5)).unwrap();
        for _ in 0..8 {
            b.pop().unwrap().retire_into(&mut mixed);
        }
        drop(mixed);
        drop(a.pop().unwrap());
        assert!(a.is_empty() && b.is_empty());
        {
            let state = monitor_a.state.lock().unwrap();
            assert_eq!(
                (state.pending, state.producers, state.submissions),
                (64, 0, 4)
            );
        }
        {
            let state = monitor_b.state.lock().unwrap();
            assert_eq!(
                (state.pending, state.producers, state.submissions),
                (16, 0, 2)
            );
        }
        assert_eq!(memory_a.bytes(), 64 * 4097 * 8);
        assert_eq!(memory_b.bytes(), 16 * 4097 * 8);
        release_a.send(()).unwrap();
        release_b.send(()).unwrap();
        service_a.drain();
        service_b.drain();
        assert_eq!(memory_a.bytes(), 0);
        assert_eq!(memory_b.bytes(), 0);
        drop(service_a);
        drop(service_b);
        assert!(monitor_a.finished.load(Ordering::Acquire));
        assert!(monitor_b.finished.load(Ordering::Acquire));
    }
    #[test]
    fn collector_native_retirement_registry_batches_sweep_and_retained_observer() {
        if !child(
            "runtime::native_retirement::tests::collector_native_retirement_registry_batches_sweep_and_retained_observer",
        ) {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            let mut owners = Vec::new();
            let mut weak = None;
            let mut last = std::ptr::null_mut();
            for _ in 0..65 {
                last = queue(thread, 4097, true);
                if weak.is_none() {
                    weak = Some(Arc::downgrade(
                        &heap.channels.lock().unwrap().records[&(last as usize)].0,
                    ));
                }
                owners.push(
                    heap.globals
                        .add((*last).owner.load(Ordering::Acquire) as u64),
                );
            }
            let held = heap.channels.lock().unwrap().records[&(last as usize)]
                .0
                .clone();
            let (monitor, waiting, release) = arm(&heap.native_retirement);
            for owner in owners {
                heap.globals.set(owner, 0);
            }
            let other = heap.clone();
            let collector = std::thread::spawn(move || other.stw_collect::<IdentityPtrPolicy>());
            blocking_region(thread, || {
                collector.join().unwrap();
                waiting.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!(heap.channels.lock().unwrap().is_empty());
                let weak = weak.unwrap();
                assert!(weak.upgrade().is_some(), "worker was stopped before control destruction");
                {
                    let state = monitor.state.lock().unwrap();
                    assert_eq!(
                        (state.pending, state.producers, state.submissions),
                        (65, 66, 1)
                    );
                }
                assert_eq!(
                    held.slots.read().unwrap().as_ref().unwrap().len(),
                    4097,
                    "observer backing detached before final borrow ended"
                );
                assert_eq!(heap.native_buffer_bytes(), 65 * 4097 * 8);
                release.send(()).unwrap();
                heap.native_retirement.drain();
                assert!(weak.upgrade().is_none());
                assert_eq!(heap.native_buffer_bytes(), 4097 * 8);
                assert_eq!(held.slots.read().unwrap().as_ref().unwrap().len(), 4097);
                let (_, waiting_last, release_last) = arm(&heap.native_retirement);
                drop(held);
                waiting_last.recv_timeout(Duration::from_secs(5)).unwrap();
                {
                    let state = monitor.state.lock().unwrap();
                    assert_eq!((state.pending, state.producers, state.submissions), (1, 1, 2));
                }
                release_last.send(()).unwrap();
                heap.native_retirement.drain();
            });
            assert_eq!(heap.native_buffer_bytes(), 0);
        });
    }
    #[test]
    fn collector_native_retirement_registry_batches_multiple_collection_waves() {
        if !child(
            "runtime::native_retirement::tests::collector_native_retirement_registry_batches_multiple_collection_waves",
        ) {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            let mut monitor = None;
            let mut waiting = None;
            let mut release = None;
            for wave in 0..3 {
                let mut owners = Vec::new();
                for _ in 0..4 {
                    let control = queue(thread, 4097, true);
                    owners.push(
                        heap.globals
                            .add((*control).owner.load(Ordering::Acquire) as u64),
                    );
                }
                if wave == 0 {
                    let (seen, ready, go) = arm(&heap.native_retirement);
                    monitor = Some(seen);
                    waiting = Some(ready);
                    release = Some(go);
                }
                for owner in owners {
                    heap.globals.set(owner, 0);
                }
                let other = heap.clone();
                let collector =
                    std::thread::spawn(move || other.stw_collect::<IdentityPtrPolicy>());
                blocking_region(thread, || {
                    collector.join().unwrap();
                    if wave == 0 {
                        waiting
                            .as_ref()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(5))
                            .unwrap();
                    }
                });
                let state = monitor.as_ref().unwrap().state.lock().unwrap();
                assert_eq!(
                    (state.pending, state.producers, state.submissions),
                    ((wave + 1) * 4, (wave + 1) * 4 + 1, wave + 1)
                );
            }
            assert_eq!(heap.native_buffer_bytes(), 12 * 4097 * 8);
            release.unwrap().send(()).unwrap();
            blocking_region(thread, || heap.native_retirement.drain());
            assert_eq!(heap.native_buffer_bytes(), 0);
        });
    }
    #[test]
    fn collector_native_retirement_registry_batches_execution_shutdown() {
        if !child(
            "runtime::native_retirement::tests::collector_native_retirement_registry_batches_execution_shutdown",
        ) {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            for _ in 0..64 {
                queue(thread, 4097, false);
            }
        });
        let (monitor, waiting, release) = arm(&heap.native_retirement);
        let seen = monitor.clone();
        let other = heap.clone();
        let observer = std::thread::spawn(move || {
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
            // Collector completion proves shutdown's caller and registry are
            // quiescent; avoid racing the short clear/publication critical section.
            unsafe {
                other.stw_collect::<IdentityPtrPolicy>();
            }
            assert!(other.channels.lock().unwrap().is_empty());
            {
                let state = seen.state.lock().unwrap();
                assert_eq!(
                    (state.pending, state.producers, state.submissions),
                    (64, 65, 1)
                );
            }
            assert_eq!(other.native_buffer_bytes(), 64 * 4097 * 8);
            release.send(()).unwrap();
        });
        unsafe {
            rt.finish_threads();
        }
        observer.join().unwrap();
        assert_eq!(heap.native_buffer_bytes(), 0);
        assert!(monitor.finished.load(Ordering::Acquire));
    }
    #[test]
    fn collector_native_retirement_reference_batch_release_and_worker_exit() {
        let service = Retirement::default();
        let memory = Arc::new(native_memory::NativeMemory::default());
        let mut buffers = Vec::new();
        for _ in 0..64 {
            buffers.push(native_memory::NativeSlots::zeroed_reserved(
                4097,
                memory.charge(4097 * 8),
                &service,
            ));
        }
        let (monitor, waiting, release) = arm(&service);
        drop(buffers.pop().unwrap());
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(buffers);
        assert_eq!(memory.bytes(), 64 * 4097 * 8);
        {
            let state = monitor.state.lock().unwrap();
            assert_eq!(state.pending, 64);
            assert_eq!(state.producers, 0);
        }
        release.send(()).unwrap();
        service.drain();
        assert_eq!(memory.bytes(), 0);
        assert_eq!(memory.peak(), 64 * 4097 * 8);
        drop(service);
        assert!(monitor.finished.load(Ordering::Acquire));
    }
    #[test]
    fn collector_completion_retirement_reference_sweep_does_not_allocate_or_destroy() {
        let monitor = Arc::new(Monitor::new());
        let source = records::Source::new(monitor.clone());
        let retirement = Retirement::default();
        let mut registry = super::super::CompletionRegistry::default();
        let mut dead = Vec::new();
        for index in 0..288 {
            let record = Arc::new(super::super::JoinHandle {
                completion_notice: false,
                daemon: std::sync::atomic::AtomicBool::new(false),
                native_joined: std::sync::atomic::AtomicBool::new(false),
                pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
                reaper_registration: std::sync::OnceLock::new(),
                retirement: std::sync::OnceLock::new(),
                state: Mutex::new(super::super::JoinState::Complete(42)),
                owner: std::sync::atomic::AtomicUsize::new(if index < 32 { 0 } else { 1 }),
            });
            assert!(record.retirement.set(RecordLink::prepared(source.clone())).is_ok());
            if index < 32 { dead.push(Arc::downgrade(&record)); }
            registry.register(record, &retirement);
        }
        // Retain enough legacy entries to exclude registry compaction, which
        // remains a separate source of allocator traffic during collection.
        super::super::allocation_probe::start();
        registry.sweep(|_| false);
        assert_eq!(super::super::allocation_probe::finish_bytes(), (0, 0, 0, 0));
        assert_eq!(registry.records.len(), 256);
        assert!(dead.iter().all(|weak| weak.upgrade().is_some()));
        assert_eq!(monitor.state.lock().unwrap().pending, 32);
        registry.clear();
        assert_eq!(monitor.state.lock().unwrap().pending, 288);
        drop(source);
        monitor.state.lock().unwrap().owner = false;
        monitor.run();
        monitor.drain();
        assert!(dead.iter().all(|weak| weak.upgrade().is_none()));
        assert!(monitor.activity.try_idle(|| {}));
    }
    #[test]
    fn collector_completion_retirement_allows_another_collection() {
        if !child("runtime::native_retirement::tests::collector_completion_retirement_allows_another_collection") {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            let record = super::super::blocking_region(thread, || {
                let record = Arc::new(super::super::JoinHandle {
                    completion_notice: false,
                    daemon: std::sync::atomic::AtomicBool::new(false),
                    native_joined: std::sync::atomic::AtomicBool::new(false),
                    pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
                    reaper_registration: std::sync::OnceLock::new(),
                    retirement: std::sync::OnceLock::new(),
                    state: Mutex::new(super::super::JoinState::Complete(42)),
                    owner: std::sync::atomic::AtomicUsize::new(0),
                });
                record.retirement.get_or_init(|| RecordLink::new(&heap.native_retirement));
                heap.thread_completions.lock().unwrap().register(record.clone(), &heap.native_retirement);
                record
            });
            let weak = Arc::downgrade(&record);
            let key = Arc::as_ptr(&record) as usize;
            drop(record);
            let (monitor, waiting, release) = arm(&heap.native_retirement);
            blocking_region(thread, || {
                heap.stw_collect::<IdentityPtrPolicy>();
                waiting.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!(!heap.thread_completions.try_lock().unwrap().records.contains_key(&key));
                assert!(monitor.state.try_lock().is_ok());
                assert_eq!(weak.upgrade().unwrap().join(), 42);
                // A stalled native destructor must not retain collector exclusion.
                heap.stw_collect::<IdentityPtrPolicy>();
                release.send(()).unwrap();
                heap.native_retirement.drain();
                assert!(weak.upgrade().is_none());
            });
        });
    }
    #[test]
    fn collector_native_retirement_shutdown_joins_and_supports_new_execution() {
        if !child(
            "runtime::native_retirement::tests::collector_native_retirement_shutdown_joins_and_supports_new_execution",
        ) {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        let mut old = None;
        for _ in 0..3 {
            rt.enter(|thread| unsafe {
                queue(thread, 8193, false);
            });
            let monitor = heap.native_retirement.monitor().unwrap();
            if let Some(previous) = old {
                assert!(!Arc::ptr_eq(&previous, &monitor));
            }
            unsafe {
                rt.finish_threads();
            }
            assert_eq!(heap.native_buffer_bytes(), 0);
            assert!(
                monitor.finished.load(Ordering::Acquire),
                "shutdown returned before worker exit"
            );
            assert!(heap.native_retirement.monitor().is_none());
            old = Some(monitor);
        }
    }
    #[test]
    fn collector_native_retirement_sweep_allows_collection_and_keeps_charge() {
        if !child(
            "runtime::native_retirement::tests::collector_native_retirement_sweep_allows_collection_and_keeps_charge",
        ) {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            queue(thread, 8193, true);
            let (monitor, waiting, release) = arm(&heap.native_retirement);
            let other = heap.clone();
            let collector = std::thread::spawn(move || other.stw_collect::<IdentityPtrPolicy>());
            blocking_region(thread, || {
                collector.join().unwrap();
                waiting.recv_timeout(Duration::from_secs(5)).unwrap();
                assert_eq!(heap.native_buffer_bytes(), 8193 * 8);
                assert!(heap.channels.try_lock().unwrap().is_empty());
                assert!(
                    monitor.state.try_lock().is_ok(),
                    "worker retained mailbox lock during release"
                );
                // A second complete collection cannot depend on the stalled free.
                heap.stw_collect::<IdentityPtrPolicy>();
                release.send(()).unwrap();
                heap.native_retirement.drain();
            });
            assert_eq!(heap.native_buffer_bytes(), 0);
        });
    }
    #[test]
    fn collector_native_retirement_observer_drop_after_heap_shutdown() {
        if !child(
            "runtime::native_retirement::tests::collector_native_retirement_observer_drop_after_heap_shutdown",
        ) {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        let weak = Arc::downgrade(&heap);
        let native = rt.enter(|thread| unsafe {
            let control = queue(thread, 8193, true);
            let native = heap.channels.lock().unwrap().records[&(control as usize)]
                .0
                .clone();
            heap.mutator_triggered_gc::<IdentityPtrPolicy>(&*(*thread).dyna_thread);
            assert!(heap.channels.lock().unwrap().is_empty());
            // Consume the registry's deferred controller reference before the
            // next hook targets the observer's later backing retirement.
            blocking_region(thread, || heap.native_retirement.drain());
            native
        });
        let (monitor, waiting, release) = arm(&heap.native_retirement);
        let memory = heap.native_memory.clone();
        drop(rt);
        drop(heap);
        assert!(weak.upgrade().is_none(), "native observer retained Heap");
        assert_eq!(memory.bytes(), 8193 * 8);
        drop(native);
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            memory.bytes(),
            8193 * 8,
            "pending native backing was uncharged early"
        );
        release.send(()).unwrap();
        monitor.drain();
        assert_eq!(memory.bytes(), 0);
        let until = Instant::now() + Duration::from_secs(5);
        while !monitor.finished.load(Ordering::Acquire) {
            assert!(Instant::now() < until, "detached worker did not exit");
            std::thread::yield_now();
        }
    }
    #[test]
    fn collector_native_retirement_shutdown_drain_is_blocked_and_unlocked() {
        if !child(
            "runtime::native_retirement::tests::collector_native_retirement_shutdown_drain_is_blocked_and_unlocked",
        ) {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            queue(thread, 8193, false);
        });
        let (monitor, waiting, release) = arm(&heap.native_retirement);
        let other = heap.clone();
        let observer = std::thread::spawn(move || {
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
            // A wake can race the short publication/clear critical section.
            // Complete GC first, proving the caller reached its BLOCKED drain.
            unsafe {
                other.stw_collect::<IdentityPtrPolicy>();
            }
            assert!(other.channels.lock().unwrap().is_empty());
            drop(monitor.state.lock().unwrap());
            assert_eq!(other.native_buffer_bytes(), 8193 * 8);
            release.send(()).unwrap();
        });
        unsafe {
            rt.finish_threads();
        }
        observer.join().unwrap();
        assert_eq!(heap.native_buffer_bytes(), 0);
        assert!(heap.native_retirement.monitor().is_none());
    }
    #[test]
    fn collector_native_retirement_pressure_drains_closed_epoch_before_collecting() {
        if !child("runtime::native_retirement::tests::collector_native_retirement_pressure_drains_closed_epoch_before_collecting") { return; }
        let (setup, configured) = mpsc::channel();
        let (begin, go) = mpsc::channel();
        let (started, ready) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let mutator = std::thread::spawn(move || {
            let mut rt = runtime();
            let heap = rt.heap().clone();
            let native = rt.enter(|thread| unsafe {
                let control = unserviced_queue(thread, (2 << 20) / 8, true);
                let native = heap.channels.lock().unwrap().records[&(control as usize)].0.clone();
                heap.mutator_triggered_gc::<IdentityPtrPolicy>(&*(*thread).dyna_thread);
                blocking_region(thread, || heap.native_retirement.drain());
                native
            });
            let (old_monitor, waiting, release) = arm(&heap.native_retirement);
            unsafe { rt.finish_threads(); }
            let state = rt.thread.registration.dyna.clone();
            rt.enter(|thread| unsafe {
                let current = blocking_region(thread, || heap.native_retirement.reserve(
                    vec![AtomicU64::new(0)].into_boxed_slice(), Some(heap.native_memory.charge(8))));
                assert!(!Arc::ptr_eq(&old_monitor, &heap.native_retirement.monitor().unwrap()));
                setup.send((heap.clone(), old_monitor, waiting, release, native)).unwrap();
                blocking_region(thread, || go.recv().unwrap());
                started.send(()).unwrap();
                let before = heap.native_pressure_collections();
                heap.mutator_gc_for_native_allocations::<IdentityPtrPolicy>(&state, 8);
                finished.send(heap.native_pressure_collections() - before).unwrap();
                blocking_region(thread, || drop(current));
            });
        });
        let (heap, old_monitor, waiting, release, native) = configured.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(native);
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(old_monitor.state.lock().unwrap().pending, 1);
        begin.send(()).unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        // Pending old work must not hold the collector while pressure waits.
        unsafe { heap.stw_collect::<IdentityPtrPolicy>(); }
        let early = done.recv_timeout(Duration::from_millis(100)).ok();
        release.send(()).unwrap();
        let collections = early.unwrap_or_else(|| done.recv_timeout(Duration::from_secs(5)).unwrap());
        mutator.join().unwrap();
        assert!(early.is_none(), "pressure returned before older native work completed");
        assert_eq!(collections, 0, "queued garbage triggered a redundant pressure collection");
        assert_eq!(heap.native_buffer_bytes(), 0);
    }

    #[test]
    fn collector_native_pressure_execution_retunes_after_native_drain() {
        for generational in [false, true] {
            let types = vec![
                TypeInfo::for_header(Full::SIZE).with_varlen_values(0),
                TypeInfo::for_header(Full::SIZE).with_type_id(1).with_raw_bytes(8),
            ];
            let mut rt = if generational {
                RuntimeContext::new_generational(32768, 2 << 20, types)
            } else { RuntimeContext::new(2 << 20, types) };
            let heap = rt.heap().clone();
            rt.enter(|thread| unsafe {
                queue(thread, (2 << 20) / 8, false);
                // Establish real pressure headroom for retained backing. The
                // boundary must shrink it only after physical release finishes.
                heap.mutator_gc_for_native_allocations::<IdentityPtrPolicy>(&*(*thread).dyna_thread, 8);
            });
            let (_, waiting, release) = arm(&heap.native_retirement);
            let before = heap.native_pressure_collections();
            std::thread::scope(|scope| {
                let other = heap.clone();
                let observer = scope.spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
                        // shutdown detaches its worker before waiting for drain.
                        // Keep physical release paused until that boundary, so
                        // an incorrectly early retune is deterministically seen.
                        let until = Instant::now() + Duration::from_secs(5);
                        while other.native_retirement.monitor().is_some() {
                            assert!(Instant::now() < until, "shutdown never reached drain");
                            std::thread::yield_now();
                        }
                        assert_eq!(other.native_buffer_bytes(), 2 << 20);
                        // Waiting must leave collector exclusion available.
                        unsafe { other.stw_collect::<IdentityPtrPolicy>(); }
                    }));
                    release.send(()).unwrap();
                    result.unwrap();
                });
                unsafe { rt.finish_threads(); }
                observer.join().unwrap();
            });
            assert_eq!(heap.native_buffer_bytes(), 0);
            assert_eq!(heap.native_pressure_collections(), before);
            rt.enter(|thread| unsafe {
                let roots = (&*(*thread).dyna_thread).scratch_scope();
                let witness = roots.push(ai_gc_alloc_varlen(thread, 0, 0, 0));
                for _ in 0..17 { ai_chan_new(thread, 8192, roots.get(witness), 1); }
                assert!(heap.native_pressure_collections() > before,
                    "next execution retained pending garbage in its trigger");
                assert!(heap.native_pressure_collections() <= before + 2);
            });
        }
    }

    #[test]
    fn collector_native_retirement_pressure_collection_retunes_after_deferred_release() {
        if !child(
            "runtime::native_retirement::tests::collector_native_retirement_pressure_collection_retunes_after_deferred_release",
        ) {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            unserviced_queue(thread, (2 << 20) / 8, true);
            let (_, waiting, release) = arm(&heap.native_retirement);
            let dyna = &*(*thread).dyna_thread;
            let roots = dyna.scratch_scope();
            let witness = roots.push(ai_gc_alloc_varlen(thread, 0, 0, 0));
            std::thread::scope(|scope| {
                let other = heap.clone();
                let observer = scope.spawn(move || {
                    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
                    // Pressure collection must relinquish exclusion during
                    // deferred release, rather than pinning the world here.
                    other.stw_collect::<IdentityPtrPolicy>();
                    release.send(()).unwrap();
                });
                let before = heap.native_pressure_collections();
                let small = ai_chan_new(thread, 1, roots.get(witness), 1);
                assert_eq!(heap.native_pressure_collections(), before + 1);
                assert_eq!(heap.native_buffer_bytes(), 8);
                // Pending garbage must not inflate the next threshold to 4 MiB.
                // This live execution-owned demand must cross the 1 MiB floor.
                for _ in 0..17 {
                    ai_chan_new(thread, 8192, roots.get(witness), 1);
                }
                assert!(
                    heap.native_pressure_collections() > before + 1,
                    "threshold retained the freed native footprint"
                );
                ai_chan_close(thread, small);
                blocking_region(thread, || observer.join().unwrap());
            });
        });
    }
    #[test]
    fn collector_native_retirement_pressure_lock_wait_publishes_roots() {
        if !child(
            "runtime::native_retirement::tests::collector_native_retirement_pressure_lock_wait_publishes_roots",
        ) {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            unserviced_queue(thread, (2 << 20) / 8, true);
            let dyna = &*(*thread).dyna_thread;
            let roots = dyna.scratch_scope();
            let witness = roots.push(ai_gc_alloc_varlen(thread, 0, 0, 0));
            std::thread::scope(|scope| {
                let (ready, waiting) = mpsc::channel();
                let (begin, started) = mpsc::channel();
                let other = heap.clone();
                let observer = scope.spawn(move || {
                    let _pressure = other.native_memory.pressure_service.lock().unwrap();
                    ready.send(()).unwrap();
                    started.recv().unwrap();
                    let until = Instant::now() + Duration::from_secs(5);
                    while !dyna.is_blocked() {
                        assert!(
                            Instant::now() < until,
                            "pressure-lock waiter stayed RUNNING"
                        );
                        std::thread::yield_now();
                    }
                    other.stw_collect::<IdentityPtrPolicy>();
                });
                blocking_region(thread, || waiting.recv().unwrap());
                begin.send(()).unwrap();
                let before = heap.native_pressure_collections();
                let small = ai_chan_new(thread, 1, roots.get(witness), 1);
                assert_eq!(
                    heap.native_pressure_collections(),
                    before,
                    "foreign collection already relieved pressure"
                );
                assert_eq!(heap.native_buffer_bytes(), 8);
                ai_chan_close(thread, small);
                blocking_region(thread, || observer.join().unwrap());
            });
        });
    }
    #[test]
    fn collector_native_retirement_pressure_wait_allows_collection_and_avoids_redundant_gc() {
        if !child(
            "runtime::native_retirement::tests::collector_native_retirement_pressure_wait_allows_collection_and_avoids_redundant_gc",
        ) {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            unserviced_queue(thread, (2 << 20) / 8, true);
            let (_, waiting, release) = arm(&heap.native_retirement);
            let dyna = &*(*thread).dyna_thread;
            heap.mutator_triggered_gc::<IdentityPtrPolicy>(dyna);
            blocking_region(thread, || {
                waiting.recv_timeout(Duration::from_secs(5)).unwrap()
            });
            let roots = dyna.scratch_scope();
            let witness = roots.push(ai_gc_alloc_varlen(thread, 0, 0, 0));
            std::thread::scope(|scope| {
                let state = dyna;
                let other = heap.clone();
                let observer = scope.spawn(move || {
                    let until = Instant::now() + Duration::from_secs(5);
                    while !state.is_blocked() {
                        assert!(Instant::now() < until, "pressure waiter remained RUNNING");
                        std::thread::yield_now();
                    }
                    other.stw_collect::<IdentityPtrPolicy>();
                    release.send(()).unwrap();
                });
                let before = heap.native_pressure_collections();
                let small = ai_chan_new(thread, 1, roots.get(witness), 1);
                assert_eq!(heap.native_pressure_collections(), before);
                assert_eq!(heap.native_buffer_bytes(), 8);
                ai_chan_close(thread, small);
                blocking_region(thread, || observer.join().unwrap());
            });
        });
    }
}

#[cfg(test)]
#[path = "channel_retirement_tests.rs"]
mod channel_records_tests;
