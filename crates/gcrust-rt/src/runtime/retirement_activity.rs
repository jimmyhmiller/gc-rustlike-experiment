//! Publication/completion accounting shared by all worker epochs of a service.
//! A producer enters before queue visibility; native consumers finish only after
//! destruction, including nested publication into an older epoch.
use std::sync::{Condvar, Mutex};
pub(super) struct Activity {
    pending: Mutex<usize>,
    drained: Condvar,
}
impl Activity {
    pub(super) fn new() -> Self {
        let activity = Self {
            pending: Mutex::new(0),
            drained: Condvar::new(),
        };
        // Prepare platform synchronization in native construction, not GC.
        drop(activity.pending.lock().unwrap());
        activity.drained.notify_all();
        activity
    }
    pub(super) fn start(&self, count: usize) {
        let mut pending = self.pending.lock().unwrap();
        *pending = pending
            .checked_add(count)
            .expect("native activity overflow");
    }
    pub(super) fn finish(&self, count: usize) {
        let mut pending = self.pending.lock().unwrap();
        *pending = pending
            .checked_sub(count)
            .expect("native activity underflow");
        if *pending == 0 {
            self.drained.notify_all();
        }
    }
    pub(super) fn drain(&self) {
        let mut pending = self.pending.lock().unwrap();
        while *pending != 0 {
            pending = self.drained.wait(pending).unwrap();
        }
    }
    /// Callback runs only at a globally idle publication boundary. It must not
    /// allocate, wait, publish retirement work, or acquire a monitor lock.
    pub(super) fn try_idle(&self, snapshot: impl FnOnce()) -> bool {
        let pending = self.pending.lock().unwrap();
        if *pending != 0 {
            return false;
        }
        snapshot();
        drop(pending);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    #[test]
    fn collector_native_activity_reference_prepared_accounting_does_not_allocate() {
        let activity = Activity::new();
        crate::runtime::allocation_probe::start();
        activity.start(17);
        activity.finish(17);
        activity.drain();
        assert!(activity.try_idle(|| {}));
        assert_eq!(
            crate::runtime::allocation_probe::finish_bytes(),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn collector_native_activity_reference_snapshot_requires_all_work_finished() {
        let activity = Activity::new();
        let snapshots = AtomicUsize::new(0);
        activity.start(2);
        assert!(!activity.try_idle(|| {
            snapshots.fetch_add(1, Ordering::SeqCst);
        }));
        activity.finish(1);
        assert!(!activity.try_idle(|| {
            snapshots.fetch_add(1, Ordering::SeqCst);
        }));
        activity.start(3); // A native consumer publishes nested/foreign work.
        activity.finish(1);
        assert!(!activity.try_idle(|| {
            snapshots.fetch_add(1, Ordering::SeqCst);
        }));
        activity.finish(3);
        activity.drain();
        assert!(activity.try_idle(|| {
            snapshots.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(snapshots.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn collector_native_activity_reference_snapshot_serializes_new_publication() {
        let activity = Arc::new(Activity::new());
        let (begin, go) = mpsc::channel();
        let (started, ready) = mpsc::channel();
        let (completed, done) = mpsc::channel();
        let other = activity.clone();
        let producer = std::thread::spawn(move || {
            go.recv().unwrap();
            started.send(()).unwrap();
            other.start(1);
            completed.send(()).unwrap();
        });
        assert!(activity.try_idle(|| {
            // The producer is still waiting on begin, so no other thread can
            // account for a failed try_lock here.
            assert!(
                activity.pending.try_lock().is_err(),
                "snapshot released exclusion early"
            );
            begin.send(()).unwrap();
            ready.recv().unwrap();
            assert!(
                activity.pending.try_lock().is_err(),
                "snapshot did not retain exclusion"
            );
            assert!(done.try_recv().is_err());
        }));
        producer.join().unwrap();
        done.recv().unwrap();
        assert!(!activity.try_idle(|| panic!("pending publication accepted")));
        activity.finish(1);
        activity.drain();
    }
}
