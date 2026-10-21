//! Prepared, typed ownership chains for native-only record destruction.
use super::{Monitor, Retirement, State};
use std::{cell::UnsafeCell, sync::Arc};

pub(super) struct Source {
    monitor: Arc<Monitor>,
}
impl Source {
    pub(super) fn new(monitor: Arc<Monitor>) -> Arc<Self> {
        {
            let mut state = monitor.state.lock().unwrap();
            assert!(
                state.owner,
                "record source after native retirement owner ended"
            );
            state.producers = state
                .producers
                .checked_add(1)
                .expect("native producer overflow");
        }
        Arc::new(Self { monitor })
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.monitor.release_producer();
    }
}
/// # Safety
/// The implementation returns this record's unique link and its corresponding
/// typed mailbox. Observers never access links. Registry retirement transfers
/// a record only once; afterwards only its producer/monitor/detached consumer
/// may access links. No managed reference may be dereferenced during destruction.
pub(in crate::runtime) unsafe trait Record: Send + Sync + Sized {
    fn retirement(&self) -> &Link<Self>;
    fn mailbox(state: &mut State) -> &mut Queue<Self>;
}
enum Entry<T> {
    Reserved(Arc<Source>),
    Queued {
        next: Option<Arc<T>>,
        source: Arc<Source>,
    },
    Drained,
}
pub(in crate::runtime) struct Link<T> {
    entry: UnsafeCell<Entry<T>>,
}
// SAFETY: observers cannot access entry. A registry's once-only transfer is
// followed by exclusive producer ownership, mutex publication, and exclusive
// detached consumption. Retained Arcs prevent destruction during link access.
unsafe impl<T: Send + Sync> Send for Link<T> {}
unsafe impl<T: Send + Sync> Sync for Link<T> {}
impl<T> Link<T> {
    pub(in crate::runtime) fn new(retirement: &Retirement) -> Self {
        Self::prepared(retirement.reserve_record_source())
    }
    pub(super) fn prepared(source: Arc<Source>) -> Self {
        Self {
            entry: UnsafeCell::new(Entry::Reserved(source)),
        }
    }
    #[cfg(test)]
    pub(super) fn is_drained(&self) -> bool {
        matches!(unsafe { &*self.entry.get() }, Entry::Drained)
    }
}
impl<T: Record> Link<T> {
    /// # Safety
    /// Exclusively removed from the owning registry, once only. No observer or
    /// other producer accesses this link while the batch accepts ownership.
    pub(in crate::runtime) unsafe fn retire(record: Arc<T>, batch: &mut Batch<T>) {
        let entry = unsafe { &mut *record.retirement().entry.get() };
        let Entry::Reserved(source) = std::mem::replace(entry, Entry::Drained) else {
            panic!("native record retired twice");
        };
        *entry = Entry::Queued { next: None, source };
        let Entry::Queued { source, .. } = entry else {
            unreachable!()
        };
        batch.push(&source.monitor, record);
    }
    unsafe fn set_next(&self, next: Option<Arc<T>>) {
        let Entry::Queued { next: slot, .. } = (unsafe { &mut *self.entry.get() }) else {
            panic!("unprepared native retirement link");
        };
        assert!(slot.is_none(), "native retirement link already occupied");
        *slot = next;
    }
    unsafe fn take_next(&self) -> Option<Arc<T>> {
        let Entry::Queued { next, source } =
            std::mem::replace(unsafe { &mut *self.entry.get() }, Entry::Drained)
        else {
            panic!("native retirement link not queued");
        };
        // Closed epochs must not free producer metadata on the collector.
        drop(source);
        next
    }
}
impl<T> Drop for Link<T> {
    fn drop(&mut self) {
        match std::mem::replace(self.entry.get_mut(), Entry::Drained) {
            Entry::Reserved(source) => drop(source),
            Entry::Drained => {}
            Entry::Queued { .. } => panic!("native record dropped before retirement unlink"),
        }
    }
}
pub(in crate::runtime) struct Queue<T: Record> {
    head: Option<Arc<T>>,
    tail: Option<Arc<T>>,
    len: usize,
}
impl<T: Record> Default for Queue<T> {
    fn default() -> Self {
        Self {
            head: None,
            tail: None,
            len: 0,
        }
    }
}
impl<T: Record> Queue<T> {
    pub(super) fn is_empty(&self) -> bool {
        self.head.is_none()
    }
    pub(super) fn len(&self) -> usize {
        self.len
    }
    fn push(&mut self, record: Arc<T>) {
        if self.tail.is_none() {
            self.tail = Some(record.clone());
        }
        unsafe {
            record.retirement().set_next(self.head.take());
        }
        self.head = Some(record);
        self.len = self
            .len
            .checked_add(1)
            .expect("native record batch overflow");
    }
    pub(super) fn append(&mut self, mut other: Self) {
        if other.is_empty() {
            return;
        }
        if self.is_empty() {
            std::mem::swap(self, &mut other);
            return;
        }
        unsafe {
            self.tail
                .as_ref()
                .unwrap()
                .retirement()
                .set_next(other.head.take());
        }
        self.tail = other.tail.take();
        self.len = self
            .len
            .checked_add(std::mem::take(&mut other.len))
            .expect("native record queue overflow");
    }
    pub(super) fn pop(&mut self) -> Option<Arc<T>> {
        let record = self.head.take()?;
        self.head = unsafe { record.retirement().take_next() };
        self.len -= 1;
        if self.head.is_none() {
            self.tail = None;
        }
        Some(record)
    }
}
impl<T: Record> Drop for Queue<T> {
    fn drop(&mut self) {
        while let Some(record) = self.pop() {
            drop(record);
        }
    }
}
pub(in crate::runtime) struct Batch<T: Record> {
    monitor: Option<Arc<Monitor>>,
    records: Queue<T>,
}
impl<T: Record> Default for Batch<T> {
    fn default() -> Self {
        Self {
            monitor: None,
            records: Queue::default(),
        }
    }
}
impl<T: Record> Batch<T> {
    fn push(&mut self, monitor: &Arc<Monitor>, record: Arc<T>) {
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
        self.records.push(record);
    }
    fn flush(&mut self) {
        if let Some(monitor) = self.monitor.take() {
            monitor.enqueue_records(std::mem::take(&mut self.records));
        }
    }
}
impl<T: Record> Drop for Batch<T> {
    fn drop(&mut self) {
        self.flush();
    }
}
