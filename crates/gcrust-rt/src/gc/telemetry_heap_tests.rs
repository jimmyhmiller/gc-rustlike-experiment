use super::*;
use crate::gc::{Full, GC_EVENT_HISTORY_LIMIT};
use std::io::{self, Write};
use std::time::Duration;

#[derive(Clone)]
struct Captured(Arc<Mutex<Vec<u8>>>);
impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn collector_telemetry_stream_preserves_every_event_across_rollovers_and_flushes() {
    let heap = Arc::new(Heap::new::<Full>(4096, vec![]));
    let captured = Captured(Arc::new(Mutex::new(Vec::new())));
    heap.set_gc_log_writer(captured.clone()).unwrap();
    for batch in 0..2 {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let heap = heap.clone();
                std::thread::spawn(move || {
                    for _ in 0..2000 {
                        heap.record_gc_duration(
                            GcKind::Major,
                            Duration::from_nanos(100),
                            100,
                            40,
                            0,
                        );
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        heap.flush_gc_log().unwrap();
        let bytes = captured.0.lock().unwrap().clone();
        let log = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(log.lines().count(), (batch + 1) * 8000);
        for (seq, line) in log.lines().enumerate() {
            assert!(
                line.starts_with(&format!("{{\"seq\":{seq},\"kind\":\"major\",")),
                "{line}"
            );
        }
        let metrics = heap.metrics_json();
        assert!(
            metrics.contains(&format!("\"gc_major\":{}", (batch + 1) * 8000)),
            "{metrics}"
        );
        assert!(metrics.contains("\"gc_events_retained\":1024"), "{metrics}");
        assert!(
            metrics.contains("\"gc_pause_quantile_error_ratio\":0.015625"),
            "{metrics}"
        );
        assert!(
            heap.gc_stats_summary()
                .contains("quantiles upper estimates")
        );
        let recent = heap.gc_events();
        assert_eq!(recent.len(), GC_EVENT_HISTORY_LIMIT);
        assert_eq!(recent.last().unwrap().seq, ((batch + 1) * 8000 - 1) as u64);
        assert_eq!(heap.gc_log_jsonl().lines().count(), GC_EVENT_HISTORY_LIMIT);
    }
}

struct Failing {
    writes: Arc<AtomicUsize>,
    flush_only: bool,
}
impl Write for Failing {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        if self.flush_only {
            Ok(bytes.len())
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "trace write failed",
            ))
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "trace flush failed",
        ))
    }
}

#[test]
fn collector_telemetry_stream_errors_preserve_lifetime_accounting_and_first_error() {
    for flush_only in [false, true] {
        let heap = Heap::new::<Full>(4096, vec![]);
        let writes = Arc::new(AtomicUsize::new(0));
        heap.set_gc_log_writer(Failing {
            writes: writes.clone(),
            flush_only,
        })
        .unwrap();
        for _ in 0..4000 {
            heap.record_gc_duration(GcKind::Minor, Duration::from_nanos(100), 100, 40, 0);
        }
        let first = heap.flush_gc_log().unwrap_err();
        assert_eq!(first.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            first.to_string(),
            if flush_only {
                "trace flush failed"
            } else {
                "trace write failed"
            }
        );
        let count = writes.load(Ordering::Relaxed);
        assert!(count > 0);
        for _ in 0..100 {
            heap.record_gc_duration(GcKind::Minor, Duration::ZERO, 0, 0, 0);
        }
        assert_eq!(
            heap.flush_gc_log().unwrap_err().to_string(),
            first.to_string()
        );
        assert_eq!(
            writes.load(Ordering::Relaxed),
            count,
            "do not retry a failed trace during GC"
        );
        assert!(heap.metrics_json().contains("\"gc_minor\":4100"));
        assert_eq!(heap.gc_events().len(), GC_EVENT_HISTORY_LIMIT);
        drop(heap);
        assert_eq!(
            writes.load(Ordering::Relaxed),
            count,
            "heap teardown must not retry a failed buffered write"
        );
    }
}

#[test]
fn collector_telemetry_trace_configuration_cannot_truncate_or_claim_missing_history() {
    let directory =
        std::env::temp_dir().join(format!("gcr-telemetry-config-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("preserved.jsonl");
    std::fs::write(&path, b"preserved").unwrap();
    let heap = Heap::new::<Full>(4096, vec![]);
    heap.set_gc_log_writer(io::sink()).unwrap();
    assert_eq!(
        heap.start_gc_log(&path).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"preserved");
    let late = Heap::new::<Full>(4096, vec![]);
    late.record_gc_duration(GcKind::Major, Duration::ZERO, 0, 0, 0);
    assert_eq!(
        late.start_gc_log(&path).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"preserved");
    let unopened = Heap::new::<Full>(4096, vec![]);
    assert!(
        unopened
            .start_gc_log(directory.join("missing/file"))
            .is_err()
    );
    unopened.set_gc_log_writer(io::sink()).unwrap(); // Failed open does not install a sink.
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn collector_telemetry_finalization_attempts_metrics_and_trace_independently() {
    let directory =
        std::env::temp_dir().join(format!("gcr-telemetry-finalize-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let metrics = directory.join("metrics.json");
    let invalid = directory.join("missing/metrics.json");

    let broken_trace = Heap::new::<Full>(4096, vec![]);
    broken_trace
        .set_gc_log_writer(Failing {
            writes: Arc::new(AtomicUsize::new(0)),
            flush_only: true,
        })
        .unwrap();
    broken_trace.record_gc_duration(GcKind::Major, Duration::ZERO, 0, 0, 0);
    let error = crate::runtime::finish_gc_telemetry(&broken_trace, Some(&metrics)).unwrap_err();
    assert!(error.contains("cannot write GC log"));
    assert!(
        std::fs::read_to_string(&metrics)
            .unwrap()
            .contains("\"gc_major\":1"),
        "a trace failure must not suppress metrics"
    );

    let captured = Captured(Arc::new(Mutex::new(Vec::new())));
    let broken_metrics = Heap::new::<Full>(4096, vec![]);
    broken_metrics.set_gc_log_writer(captured.clone()).unwrap();
    broken_metrics.record_gc_duration(GcKind::Major, Duration::ZERO, 0, 0, 0);
    assert!(captured.0.lock().unwrap().is_empty()); // Still buffered.
    let error = crate::runtime::finish_gc_telemetry(&broken_metrics, Some(&invalid)).unwrap_err();
    assert!(error.contains("cannot write GC metrics"));
    assert_eq!(
        captured
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count(),
        1,
        "a metrics failure must not skip the trace flush"
    );

    let error = crate::runtime::finish_gc_telemetry(&broken_trace, Some(&invalid)).unwrap_err();
    assert!(error.contains("cannot write GC metrics"));
    assert!(
        error.contains("cannot write GC log"),
        "report both sink errors"
    );
    std::fs::remove_dir_all(directory).unwrap();
}


#[test]
fn collector_telemetry_flush_allows_world_pause_while_sink_blocks() {
    use crate::gc::{IdentityPtrPolicy, MutatorThread};
    use std::sync::mpsc;
    struct BlockedFlush {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Write for BlockedFlush {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> { Ok(bytes.len()) }
        fn flush(&mut self) -> io::Result<()> {
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            Ok(())
        }
    }
    let heap = Arc::new(Heap::new::<Full>(4096, vec![]));
    let (entered, entered_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    heap.set_gc_log_writer(BlockedFlush { entered, release: release_rx }).unwrap();
    let worker_heap = heap.clone();
    let worker = std::thread::spawn(move || {
        let mutator = MutatorThread::<IdentityPtrPolicy>::register(worker_heap.clone());
        let sibling = MutatorThread::<IdentityPtrPolicy>::register(worker_heap.clone());
        worker_heap.flush_gc_log().unwrap();
        assert!(mutator.state().is_running() && sibling.state().is_running());
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (paused, paused_rx) = mpsc::channel();
    let observer_heap = heap.clone();
    let observer = std::thread::spawn(move || {
        let pause = observer_heap.pause_world();
        paused.send(()).unwrap();
        drop(pause);
    });
    let progressed = paused_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    // Always unblock and join before asserting, including on the old path.
    // A failure must not leave a registered mutator or collector behind.
    release.send(()).unwrap();
    worker.join().unwrap();
    observer.join().unwrap();
    assert!(progressed, "blocking telemetry flush pinned a RUNNING mutator and prevented world pause");
}


#[test]
fn collector_telemetry_waiters_release_metadata_before_resuming() {
    use crate::gc::{IdentityPtrPolicy, MutatorThread};
    use std::sync::mpsc;
    use std::time::Instant;
    for case in 0..7 {
        let heap = Arc::new(Heap::new::<Full>(4096, vec![]));
        let held = heap.gc_events.lock().unwrap();
        let (ready, ready_rx) = mpsc::channel();
        let (finished, finished_rx) = mpsc::channel();
        let worker_heap = heap.clone();
        let path = std::env::temp_dir().join(format!("gcr-telemetry-wait-{}-{case}.jsonl", std::process::id()));
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            let mutator = MutatorThread::<IdentityPtrPolicy>::register(worker_heap.clone());
            ready.send(std::thread::current().id()).unwrap();
            FINISHED_PROBE.with(|probe| *probe.borrow_mut() = Some(finished));
            match case {
                0 => { worker_heap.gc_events(); }
                1 => { worker_heap.gc_stats_summary(); }
                2 => { worker_heap.metrics_json(); }
                3 => { worker_heap.flush_gc_log().unwrap(); }
                4 => { worker_heap.set_gc_log_writer(io::sink()).unwrap(); }
                5 => { worker_heap.start_gc_log(worker_path).unwrap(); }
                6 => { worker_heap.gc_log_jsonl(); }
                _ => unreachable!(),
            }
            assert!(mutator.state().is_running());
        });
        let owner = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let is_blocked = || {
            let threads = heap.threads.lock().unwrap();
            threads.iter().find(|state| state.os_thread() == owner).unwrap().is_blocked()
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while !is_blocked() && Instant::now() < deadline { std::thread::yield_now(); }
        let blocked = is_blocked();
        if !blocked {
            drop(held);
            worker.join().unwrap();
            panic!("telemetry case {case} waited while RUNNING");
        }
        let pause = heap.pause_world();
        drop(held);
        // Once the native operation finishes, it must release telemetry before
        // waiting to resume from this pause. A scanner can then acquire it.
        let completed = finished_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        let deadline = Instant::now() + Duration::from_secs(2);
        let released = completed && loop {
            if let Ok(guard) = heap.gc_events.try_lock() { drop(guard); break true; }
            if Instant::now() >= deadline { break false; }
            std::thread::yield_now();
        };
        drop(pause);
        worker.join().unwrap();
        if case == 5 { std::fs::remove_file(path).unwrap(); }
        assert!(released, "telemetry case {case} retained its native lock while resuming");
    }
}


thread_local! {
    static FINISHED_PROBE: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
}
pub(super) fn operation_finished() {
    FINISHED_PROBE.with(|probe| {
        if let Some(sender) = probe.borrow_mut().take() { sender.send(()).unwrap(); }
    });
}


#[test]
fn collector_telemetry_sink_unwind_restores_all_current_registrations() {
    use crate::gc::{IdentityPtrPolicy, MutatorThread};
    struct PanickingFlush;
    impl Write for PanickingFlush {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> { Ok(bytes.len()) }
        fn flush(&mut self) -> io::Result<()> { panic!("embedding sink flush panic") }
    }
    let heap = Arc::new(Heap::new::<Full>(4096, vec![]));
    heap.set_gc_log_writer(PanickingFlush).unwrap();
    let first = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
    let second = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| heap.flush_gc_log()));
    assert!(result.is_err());
    assert!(first.state().is_running() && second.state().is_running());
    // Poison remains observable, but no poisoned guard may pin either caller
    // BLOCKED, and cleanup/inspection must still complete after native unwind.
    assert!(heap.gc_events.is_poisoned());
    drop(heap.pause_world());
}
