//! FIFO collector exclusion. A new major cannot barge ahead of an allocation
//! request that already owns a queue ticket. The protected data lives in Heap.
use std::{
    marker::PhantomData,
    sync::{
        Condvar, LockResult, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

pub(super) struct Coordinator {
    next: AtomicU64,
    serving: AtomicU64,
    waiting: Mutex<()>,
    changed: Condvar,
}
pub(super) struct Guard<'a> {
    coordinator: &'a Coordinator,
    ticket: u64,
    // Preserve the previous guard's !Send and Sync behavior and same-thread scope.
    marker: PhantomData<MutexGuard<'a, ()>>,
}
impl Coordinator {
    pub(super) const fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
            serving: AtomicU64::new(0),
            waiting: Mutex::new(()),
            changed: Condvar::new(),
        }
    }
    pub(super) fn lock(&self) -> LockResult<Guard<'_>> {
        // Tickets never repeat, including after very long executions. Exhaustion
        // is fatal rather than granting duplicate ownership or losing a waiter.
        let ticket = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .unwrap_or_else(|_| std::process::abort());
        if self.serving.load(Ordering::Acquire) != ticket {
            let mut waiting = self
                .waiting
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while self.serving.load(Ordering::Acquire) != ticket {
                waiting = self
                    .changed
                    .wait(waiting)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        Ok(Guard {
            coordinator: self,
            ticket,
            marker: PhantomData,
        })
    }
    #[cfg(test)]
    pub(super) fn try_lock(&self) -> std::sync::TryLockResult<Guard<'_>> {
        // Drop publishes serving under this mutex, so it cannot advance between
        // our idle check and ticket claim. Existing queued tickets always win.
        let _waiting = match self.waiting.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err(std::sync::TryLockError::WouldBlock);
            }
        };
        let serving = self.serving.load(Ordering::Acquire);
        let next = serving
            .checked_add(1)
            .unwrap_or_else(|| std::process::abort());
        if self
            .next
            .compare_exchange(serving, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return Err(std::sync::TryLockError::WouldBlock);
        }
        Ok(Guard {
            coordinator: self,
            ticket: serving,
            marker: PhantomData,
        })
    }
}
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        // Pair predicate publication with the wait mutex, including uncontended
        // release. A waiter either sees the new serving value before sleeping,
        // or atomically releases this mutex and is covered by the notification.
        let _waiting = self
            .coordinator
            .waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert_eq!(
            self.coordinator.serving.load(Ordering::Relaxed),
            self.ticket
        );
        let next = self.ticket + 1;
        self.coordinator.serving.store(next, Ordering::Release);
        if self.coordinator.next.load(Ordering::Relaxed) != next {
            // Condvar selection is not FIFO: waking only one arbitrary waiter
            // could leave the actual next ticket asleep indefinitely.
            self.coordinator.changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::UnsafeCell,
        sync::{Arc, atomic::AtomicBool},
        thread,
        time::{Duration, Instant},
    };
    fn wait_until(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "coordinator participant did not arrive"
            );
            thread::yield_now();
        }
    }
    #[test]
    fn collector_coordinator_queued_requests_precede_late_reentry() {
        let coordinator = Arc::new(Coordinator::new());
        let held = coordinator.lock().unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut threads = Vec::new();
        for id in 1..=3 {
            let c = coordinator.clone();
            let order = order.clone();
            threads.push(thread::spawn(move || {
                let _guard = c.lock().unwrap();
                order.lock().unwrap().push(id);
            }));
            wait_until(|| coordinator.next.load(Ordering::Acquire) == id + 1);
        }
        assert!(
            coordinator.try_lock().is_err(),
            "try_lock must not bypass queued requests"
        );
        drop(held);
        let _late = coordinator.lock().unwrap();
        assert_eq!(
            *order.lock().unwrap(),
            [1, 2, 3],
            "late collector must follow every earlier request"
        );
        for worker in threads {
            worker.join().unwrap();
        }
    }
    #[test]
    fn collector_coordinator_unwind_releases_next_request() {
        let coordinator = Arc::new(Coordinator::new());
        let target = coordinator.clone();
        let arrived = Arc::new(AtomicBool::new(false));
        let observed = arrived.clone();
        let mut worker = None;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _held = coordinator.lock().unwrap();
                worker = Some(thread::spawn(move || {
                    let _next = target.lock().unwrap();
                    observed.store(true, Ordering::Release);
                }));
                wait_until(|| coordinator.next.load(Ordering::Acquire) == 2);
                panic!("owner unwinds outside coordinator internals");
            }))
            .is_err()
        );
        worker.unwrap().join().unwrap();
        assert!(arrived.load(Ordering::Acquire));
        assert!(coordinator.try_lock().is_ok());
    }
    #[test]
    fn collector_coordinator_acquire_observes_prior_owner_payload() {
        struct Data {
            coordinator: Coordinator,
            value: UnsafeCell<usize>,
            written: AtomicBool,
            reader_done: AtomicBool,
        }
        // All payload accesses hold the coordinator; the relaxed flag only
        // arranges arrival order and deliberately does not publish the value.
        unsafe impl Sync for Data {}
        let data = Arc::new(Data {
            coordinator: Coordinator::new(),
            value: UnsafeCell::new(0),
            written: AtomicBool::new(false),
            reader_done: AtomicBool::new(false),
        });
        let writer = data.clone();
        let worker = thread::spawn(move || {
            {
                let _guard = writer.coordinator.lock().unwrap();
                unsafe {
                    writer.value.get().write(42);
                }
            }
            // Arrange the uncontended handoff, avoiding the wait mutex as an
            // alternative publication path. This relaxed flag does not publish
            // the payload; only serving's release/acquire may do that.
            writer.written.store(true, Ordering::Relaxed);
            while !writer.reader_done.load(Ordering::Relaxed) {
                thread::yield_now();
            }
        });
        wait_until(|| data.written.load(Ordering::Relaxed));
        {
            let _guard = data.coordinator.lock().unwrap();
            assert_eq!(unsafe { data.value.get().read() }, 42);
        }
        data.reader_done.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        fn assert_sync<T: Sync>() {}
        assert_sync::<Guard<'static>>();
    }
}
