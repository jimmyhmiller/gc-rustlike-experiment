//! Requested native allocation bytes, retained until physical deallocation.
use std::sync::atomic::{AtomicUsize, Ordering};
#[derive(Default)]
pub(crate) struct AllocationCounter {
    live: AtomicUsize,
    peak: AtomicUsize,
}
impl AllocationCounter {
    pub(crate) const fn new() -> Self {
        Self { live: AtomicUsize::new(0), peak: AtomicUsize::new(0) }
    }
    pub(crate) fn bytes(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }
    pub(crate) fn peak(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }
    pub(crate) fn try_charge(&self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        let Ok(old) = self
            .live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(bytes)
            })
        else {
            return false;
        };
        let live = old + bytes;
        if live > self.peak.load(Ordering::Relaxed) {
            self.peak.fetch_max(live, Ordering::Relaxed);
        }
        true
    }
    pub(crate) fn release(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_sub(bytes)
            })
            .expect("native allocation accounting underflow");
    }
}
