use super::*;

#[test]
fn collector_statemap_common_events_allocate_nothing_through_rollover() {
    let tracer = StatemapTracer::with_limits(TraceLimits {
        max_events: 8,
        max_entity_bytes: 8,
    });
    crate::runtime::allocation_probe::start();
    for id in 0..4096 {
        tracer.record_thread(id, TraceState::Running);
        tracer.record_gc(TraceState::GcStw);
    }
    let traffic = crate::runtime::allocation_probe::finish_bytes();
    assert_eq!(traffic, (0, 0, 0, 0));
    let stats = tracer.stats();
    assert_eq!(stats.retained, 8);
    assert_eq!(stats.total_recorded, 8192);
    assert_eq!(stats.evicted, 8184);
    assert_eq!(stats.rejected, 0);
}

#[test]
fn collector_statemap_escapes_custom_entities_and_preserves_creation_origin() {
    let mut tracer = StatemapTracer::new();
    tracer.wall_epoch = std::time::UNIX_EPOCH + std::time::Duration::new(123, 456);
    tracer.record("quote\"\\\n\r\t\0\u{1f}λ", TraceState::Running);
    tracer.record("thread0017", TraceState::AtSafepoint);
    let mut output = Vec::new();
    tracer.write_to(&mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert_eq!(output.lines().count(), 3);
    assert!(
        output
            .lines()
            .next()
            .unwrap()
            .contains("\"start\":[123,456]")
    );
    assert!(output.contains("\"entity\":\"quote\\\"\\\\\\n\\r\\t\\u0000\\u001fλ\""));
    assert!(output.contains("\"entity\":\"thread0017\""));
}

#[test]
fn collector_statemap_limits_custom_text_and_reports_eviction_and_rejection() {
    let tracer = StatemapTracer::with_limits(TraceLimits {
        max_events: 4,
        max_entity_bytes: 5,
    });
    tracer.record("abc", TraceState::Running);
    tracer.record("def", TraceState::AtSafepoint);
    tracer.record("oversized", TraceState::Running);
    assert_eq!(
        tracer.stats(),
        TraceStats {
            retained: 1,
            entity_bytes: 3,
            total_recorded: 3,
            evicted: 1,
            rejected: 1
        }
    );
    let mut output = Vec::new();
    tracer.write_to(&mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("1 retained; 1 earlier events evicted; 1 events rejected"));
    assert!(output.contains("\"entity\":\"def\""));
    assert!(!output.contains("\"entity\":\"abc\""));
    assert!(!output.contains("oversized"));
    let disabled = StatemapTracer::with_limits(TraceLimits {
        max_events: 0,
        max_entity_bytes: 0,
    });
    disabled.record_gc(TraceState::GcIdle);
    disabled.record_thread(1, TraceState::Running);
    disabled.record("custom", TraceState::Running);
    assert!(disabled.is_empty());
    assert_eq!(disabled.stats().rejected, 3);
}

#[test]
fn collector_statemap_export_allows_reentrant_recording_and_write_failure() {
    struct Reenter<'a> {
        tracer: &'a StatemapTracer,
        output: Vec<u8>,
        called: bool,
    }
    impl Write for Reenter<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if !self.called {
                self.called = true;
                self.tracer.record_thread(2, TraceState::Running);
            }
            self.output.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let tracer = StatemapTracer::new();
    tracer.record_thread(1, TraceState::Running);
    let mut writer = Reenter {
        tracer: &tracer,
        output: Vec::new(),
        called: false,
    };
    tracer.write_to(&mut writer).unwrap();
    let output = String::from_utf8(writer.output).unwrap();
    assert!(output.contains("thread1"));
    assert!(!output.contains("thread2"));
    assert_eq!(tracer.len(), 2);
    let mut full = &mut [][..];
    assert!(tracer.write_to(&mut full).is_err());
    tracer.record_gc(TraceState::GcIdle);
    assert_eq!(tracer.len(), 3);
}

#[test]
fn collector_statemap_concurrent_recorders_keep_order_and_exact_retention() {
    let tracer = Arc::new(StatemapTracer::with_limits(TraceLimits {
        max_events: 64,
        max_entity_bytes: 0,
    }));
    std::thread::scope(|scope| {
        for id in 0..4 {
            let tracer = &tracer;
            scope.spawn(move || {
                for _ in 0..1000 {
                    tracer.record_thread(id, TraceState::Running);
                }
            });
        }
    });
    let stats = tracer.stats();
    assert_eq!(stats.total_recorded, 4000);
    assert_eq!(stats.retained, 64);
    assert_eq!(stats.evicted, 3936);
    let history = tracer.history.lock().unwrap();
    assert!(
        history
            .events
            .iter()
            .zip(history.events.iter().skip(1))
            .all(|(a, b)| a.time_ns <= b.time_ns)
    );
}

#[test]
fn collector_statemap_pre_epoch_origin_returns_error_without_poisoning_history() {
    let mut tracer = StatemapTracer::new();
    tracer.wall_epoch = std::time::UNIX_EPOCH - std::time::Duration::from_secs(1);
    assert_eq!(
        tracer.write_to(&mut Vec::new()).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    tracer.record_gc(TraceState::GcIdle);
    assert_eq!(tracer.len(), 1);
}
