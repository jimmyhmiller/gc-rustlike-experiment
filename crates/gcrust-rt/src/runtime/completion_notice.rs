//! A one-shot handshake between admission and native body exit. Neither side
//! waits for the other: a child can finish before its completion is published.
use super::{Arc, JoinHandle, Mutex, abort_on_cleanup_panic, completion_reaper::Monitor};
use std::sync::Weak;

struct State {
    record: Option<Weak<JoinHandle>>,
    finished: bool,
}
struct Shared {
    monitor: Arc<Monitor>,
    state: Mutex<State>,
}

/// Publication and exit have separate owners. Only one exit guard is created;
/// moving it into the native closure also covers a closure dropped uncalled.
pub(super) struct CompletionNotice(Arc<Shared>);
impl CompletionNotice {
    pub fn new(monitor: Arc<Monitor>) -> (Self, OnExit) {
        let shared = Arc::new(Shared {
            monitor,
            state: Mutex::new(State {
                record: None,
                finished: false,
            }),
        });
        drop(shared.state.lock().unwrap());
        (Self(shared.clone()), OnExit(shared))
    }
    pub fn publish(&self, record: &Arc<JoinHandle>) {
        let ready = {
            let mut state = self.0.state.lock().unwrap();
            assert!(state.record.is_none(), "native completion published twice");
            state.record = Some(Arc::downgrade(record));
            state.finished
        };
        if ready {
            self.0.monitor.enqueue(record);
        }
    }
}
impl Shared {
    fn finished(&self) {
        let record = {
            let mut state = self.state.lock().unwrap();
            assert!(!state.finished, "native body finished twice");
            state.finished = true;
            state.record.clone()
        };
        // Never hold the notice lock through catalogue access or native join.
        if let Some(record) = record.and_then(|record| record.upgrade()) {
            self.monitor.enqueue(&record);
        }
    }
}

pub(super) struct OnExit(Arc<Shared>);
impl Drop for OnExit {
    fn drop(&mut self) {
        abort_on_cleanup_panic(|| self.0.finished());
    }
}
