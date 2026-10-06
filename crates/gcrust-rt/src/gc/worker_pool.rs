//! Persistent helpers with synchronous, borrowed job dispatch.
use std::any::Any;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

type Panic = Box<dyn Any + Send + 'static>;
#[derive(Clone, Copy)]
struct Job {
    context: usize,
    invoke: unsafe fn(usize, usize),
}
struct State {
    generation: u64,
    job: Option<Job>,
    participants: usize,
    remaining: usize,
    shutdown: bool,
    failure: Option<Panic>,
}
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
    done: Condvar,
}

pub(in crate::gc::heap) struct WorkerPool {
    shared: Arc<Shared>,
    // Serializes dispatch and owns all helpers. Heap collection already holds
    // gc_lock, but this also makes the borrowed-job lifetime invariant local.
    helpers: Mutex<Vec<JoinHandle<()>>>,
}
impl WorkerPool {
    pub(super) fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    generation: 0,
                    job: None,
                    participants: 0,
                    remaining: 0,
                    shutdown: false,
                    failure: None,
                }),
                ready: Condvar::new(),
                done: Condvar::new(),
            }),
            helpers: Mutex::new(Vec::new()),
        }
    }

    /// Run a borrowed job on `count` helpers and on the calling coordinator.
    /// Does not return or propagate a job panic until all helpers acknowledge
    /// completion. F need not be 'static; only the helpers themselves are.
    pub(super) fn run<F: Fn(usize) + Sync>(&self, count: usize, job: &F) {
        if count == 0 {
            job(0);
            return;
        }
        let mut helpers = self.helpers.lock().unwrap();
        while helpers.len() < count {
            let id = helpers.len() + 1;
            let shared = self.shared.clone();
            let helper = std::thread::Builder::new()
                .name(format!("gcr-gc-{id}"))
                .spawn(move || helper(shared, id));
            match helper {
                Ok(handle) => helpers.push(handle),
                Err(error) => {
                    // No job has been published. Release the dispatch lock so
                    // already-created idle helpers can be shut down normally.
                    drop(helpers);
                    panic!("could not start collector worker: {error}");
                }
            }
        }
        unsafe fn invoke<F: Fn(usize) + Sync>(context: usize, id: usize) {
            // run keeps F alive, holds exclusive dispatch and waits for every
            // participant before clearing this pointer or propagating a panic.
            unsafe {
                (&*(context as *const F))(id);
            }
        }
        {
            let mut state = self.shared.state.lock().unwrap();
            let Some(next) = state.generation.checked_add(1) else {
                drop(state);
                drop(helpers);
                panic!("collector generation overflow");
            };
            state.generation = next;
            state.participants = count;
            state.remaining = count;
            state.failure = None;
            state.job = Some(Job {
                context: job as *const F as usize,
                invoke: invoke::<F>,
            });
            self.shared.ready.notify_all();
        }
        let coordinator = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(0)));
        let mut state = self.shared.state.lock().unwrap();
        while state.remaining != 0 {
            state = self.shared.done.wait(state).unwrap();
        }
        state.job = None;
        let failure = coordinator.err().or(state.failure.take());
        drop(state);
        // Propagate outside the dispatch lock, keeping the pool reusable after
        // a caught panic and preventing lock poison from hiding the real error.
        drop(helpers);
        if let Some(payload) = failure {
            std::panic::resume_unwind(payload);
        }
    }
}
fn helper(shared: Arc<Shared>, id: usize) {
    let mut seen = 0;
    loop {
        let mut state = shared.state.lock().unwrap();
        while !state.shutdown && (state.job.is_none() || state.generation == seen) {
            state = shared.ready.wait(state).unwrap();
        }
        if state.shutdown {
            return;
        }
        seen = state.generation;
        if id > state.participants {
            continue;
        }
        let job = state.job.unwrap();
        drop(state);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            (job.invoke)(job.context, id);
        }));
        let mut state = shared.state.lock().unwrap();
        if let Err(payload) = result {
            state.failure = Some(payload);
        }
        state.remaining -= 1;
        if state.remaining == 0 {
            shared.done.notify_one();
        }
    }
}
impl Drop for WorkerPool {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap();
            debug_assert!(state.job.is_none());
            state.shutdown = true;
            self.shared.ready.notify_all();
        }
        for handle in self.helpers.get_mut().unwrap().drain(..) {
            handle
                .join()
                .expect("collector helper failed outside its job");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn reuses_helpers_and_respects_changing_participation() {
        let pool = WorkerPool::new();
        let ids = Mutex::new(std::collections::HashMap::new());
        for helpers in [3, 1, 5, 2, 5, 0, 3] {
            let visits = AtomicUsize::new(0);
            pool.run(helpers, &|id| {
                assert!(id <= helpers);
                let tid = std::thread::current().id();
                let mut ids = ids.lock().unwrap();
                if let Some(previous) = ids.insert(id, tid) {
                    assert_eq!(previous, tid);
                }
                visits.fetch_add(1, Ordering::Relaxed);
            });
            assert_eq!(visits.load(Ordering::Relaxed), helpers + 1);
        }
        assert_eq!(pool.helpers.lock().unwrap().len(), 5);
    }

    #[test]
    fn waits_for_borrowed_jobs_before_propagating_panics_and_remains_reusable() {
        let pool = WorkerPool::new();
        for failing in [0, 2] {
            let entered = Barrier::new(4);
            let finished = AtomicUsize::new(0);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pool.run(3, &|id| {
                    entered.wait();
                    if id == failing {
                        panic!("intentional collector job failure");
                    }
                    finished.fetch_add(1, Ordering::Release);
                });
            }));
            assert!(result.is_err());
            assert_eq!(finished.load(Ordering::Acquire), 3);
            pool.run(3, &|_| {
                finished.fetch_add(1, Ordering::Relaxed);
            });
            assert_eq!(finished.load(Ordering::Relaxed), 7);
        }
    }
    #[test]
    fn drop_joins_all_idle_helpers() {
        struct Exit(Arc<AtomicUsize>);
        impl Drop for Exit {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Release);
            }
        }
        thread_local! { static EXIT: std::cell::RefCell<Option<Exit>> = const { std::cell::RefCell::new(None) }; }
        let alive = Arc::new(AtomicUsize::new(0));
        let pool = WorkerPool::new();
        pool.run(4, &|id| {
            if id != 0 {
                EXIT.with(|slot| {
                    alive.fetch_add(1, Ordering::Relaxed);
                    *slot.borrow_mut() = Some(Exit(alive.clone()));
                });
            }
        });
        assert_eq!(alive.load(Ordering::Acquire), 4);
        drop(pool);
        assert_eq!(alive.load(Ordering::Acquire), 0);
    }
}
