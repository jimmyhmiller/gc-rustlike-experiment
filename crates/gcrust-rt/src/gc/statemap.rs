//! Statemap trace instrumentation for GC safepoint visualization.
//!
//! Emits [statemap](https://github.com/bcantrill/statemap)-compatible
//! trace data capturing thread states and GC phases. Tracing is opt-in by
//! attaching a tracer to a heap.
//!
//! Usage:
//! ```rust,ignore
//! let tracer = StatemapTracer::new();
//! // ... pass to Heap, run workload ...
//! tracer.write_to_file("gc_trace.out").unwrap();
//! // Then: statemap gc_trace.out > gc_trace.svg
//! ```

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

// ─── States ──────────────────────────────────────────────────────

/// Thread/GC entity states for the statemap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TraceState {
    /// Mutator thread running normally.
    Running = 0,
    /// Mutator thread suspended at a safepoint.
    AtSafepoint = 1,
    /// Mutator thread blocked waiting for GC lock (in trigger_gc).
    WaitingForGc = 2,
    /// GC thread idle (between collections).
    GcIdle = 3,
    /// GC thread in STW pause (scanning roots, etc.).
    GcStw = 4,
    /// GC thread in concurrent copy phase.
    GcConcurrent = 5,
    /// GC thread waiting for threads to reach safepoints.
    GcWaitingForSafepoints = 6,
    /// GC thread resuming threads.
    GcResuming = 7,
}

impl TraceState {
    fn name(self) -> &'static str {
        match self {
            TraceState::Running => "running",
            TraceState::AtSafepoint => "at-safepoint",
            TraceState::WaitingForGc => "waiting-for-gc",
            TraceState::GcIdle => "gc-idle",
            TraceState::GcStw => "gc-stw",
            TraceState::GcConcurrent => "gc-concurrent",
            TraceState::GcWaitingForSafepoints => "gc-waiting-for-safepoints",
            TraceState::GcResuming => "gc-resuming",
        }
    }

    fn color(self) -> &'static str {
        match self {
            TraceState::Running => "#57ab5a",                // green
            TraceState::AtSafepoint => "#e5534b",            // red
            TraceState::WaitingForGc => "#c69026",           // yellow/orange
            TraceState::GcIdle => "#444c56",                 // dark grey
            TraceState::GcStw => "#e5534b",                  // red
            TraceState::GcConcurrent => "#6cb6ff",           // blue
            TraceState::GcWaitingForSafepoints => "#c69026", // orange
            TraceState::GcResuming => "#986ee2",             // purple
        }
    }

    fn value(self) -> u8 {
        self as u8
    }
}

const ALL_STATES: [TraceState; 8] = [
    TraceState::Running,
    TraceState::AtSafepoint,
    TraceState::WaitingForGc,
    TraceState::GcIdle,
    TraceState::GcStw,
    TraceState::GcConcurrent,
    TraceState::GcWaitingForSafepoints,
    TraceState::GcResuming,
];

/// Retention budgets apply to committed history. Each export owns an additional
/// bounded snapshot; caller-owned input strings and concurrent calls are separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceLimits {
    pub max_events: usize,
    pub max_entity_bytes: usize,
}
impl Default for TraceLimits {
    fn default() -> Self {
        Self {
            max_events: 64 * 1024,
            max_entity_bytes: 1024 * 1024,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceStats {
    pub retained: usize,
    pub entity_bytes: usize,
    pub total_recorded: u64,
    pub evicted: u64,
    pub rejected: u64,
}
#[derive(Clone)]
enum Entity {
    Gc,
    Thread(usize),
    Custom(Arc<str>),
}
impl Entity {
    fn bytes(&self) -> usize {
        match self {
            Self::Custom(name) => name.len(),
            _ => 0,
        }
    }
}
#[derive(Clone)]
struct TraceEvent {
    entity: Entity,
    time_ns: u64,
    state: TraceState,
}
struct History {
    events: VecDeque<TraceEvent>,
    entity_bytes: usize,
    total_recorded: u64,
    evicted: u64,
    rejected: u64,
}
impl History {
    fn stats(&self) -> TraceStats {
        TraceStats {
            retained: self.events.len(),
            entity_bytes: self.entity_bytes,
            total_recorded: self.total_recorded,
            evicted: self.evicted,
            rejected: self.rejected,
        }
    }
}

/// Thread-safe recent trace history with explicit retention and loss statistics.
/// GC/thread recording allocates no event names. Export performs writer calls
/// after releasing the history mutex; custom-name disposal also runs outside it.
pub struct StatemapTracer {
    epoch: Instant,
    wall_epoch: SystemTime,
    limits: TraceLimits,
    history: Mutex<History>,
}
impl Default for StatemapTracer {
    fn default() -> Self {
        Self::new()
    }
}
impl StatemapTracer {
    pub fn new() -> Self {
        Self::with_limits(TraceLimits::default())
    }
    pub fn with_limits(limits: TraceLimits) -> Self {
        let wall_epoch = SystemTime::now();
        let epoch = Instant::now();
        let history = Mutex::new(History {
            events: VecDeque::with_capacity(limits.max_events),
            entity_bytes: 0,
            total_recorded: 0,
            evicted: 0,
            rejected: 0,
        });
        // Some native mutex backends allocate on first use. Prepare before a
        // tracer can be attached to a running heap or collector.
        drop(history.lock().unwrap());
        Self {
            epoch,
            wall_epoch,
            limits,
            history,
        }
    }
    fn reject(&self) {
        let mut history = self.history.lock().unwrap();
        history.total_recorded = history.total_recorded.saturating_add(1);
        history.rejected = history.rejected.saturating_add(1);
    }
    pub fn record(&self, entity: &str, state: TraceState) {
        if entity == "gc" {
            self.record_gc(state);
            return;
        }
        if let Some(digits) = entity.strip_prefix("thread") {
            if !digits.is_empty()
                && (digits == "0" || !digits.starts_with('0'))
                && digits.bytes().all(|byte| byte.is_ascii_digit())
            {
                if let Ok(id) = digits.parse() {
                    self.record_thread(id, state);
                    return;
                }
            }
        }
        if self.limits.max_events == 0 || entity.len() > self.limits.max_entity_bytes {
            self.reject();
            return;
        }
        // Allocation runs outside the mutex. Retention budgets do not include
        // concurrent callers' unpublished names or caller-owned input strings.
        self.push(Entity::Custom(Arc::from(entity)), state);
    }
    pub fn record_thread(&self, thread_id: usize, state: TraceState) {
        self.push(Entity::Thread(thread_id), state);
    }
    pub fn record_gc(&self, state: TraceState) {
        self.push(Entity::Gc, state);
    }
    fn push(&self, entity: Entity, state: TraceState) {
        if self.limits.max_events == 0 {
            self.reject();
            return;
        }
        let bytes = entity.bytes();
        loop {
            let mut history = self.history.lock().unwrap();
            if history.events.len() < self.limits.max_events
                && bytes <= self.limits.max_entity_bytes - history.entity_bytes
            {
                // Timestamp at commit gives ordered records, including across
                // concurrent recorders. Equal timestamps retain commit order.
                let time_ns = self
                    .epoch
                    .elapsed()
                    .as_nanos()
                    .try_into()
                    .unwrap_or(u64::MAX);
                history.events.push_back(TraceEvent {
                    entity,
                    time_ns,
                    state,
                });
                history.entity_bytes += bytes;
                history.total_recorded = history.total_recorded.saturating_add(1);
                return;
            }
            let removed = history
                .events
                .pop_front()
                .expect("retention budget must permit one event");
            history.entity_bytes -= removed.entity.bytes();
            history.evicted = history.evicted.saturating_add(1);
            drop(history);
            drop(removed);
        }
    }
    pub fn stats(&self) -> TraceStats {
        self.history.lock().unwrap().stats()
    }
    /// Number of events currently retained, rather than lifetime attempts.
    pub fn len(&self) -> usize {
        self.stats().retained
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn write_to_file(&self, path: &str) -> std::io::Result<()> {
        self.write_to(&mut std::fs::File::create(path)?)
    }
    pub fn write_to<W: Write>(&self, w: &mut W) -> std::io::Result<()> {
        // Reserve outside the mutex. Cloning bounded events only increments
        // custom-name Arc counts, with no per-name allocation under the lock.
        let mut events = Vec::with_capacity(self.limits.max_events);
        let stats = {
            let history = self.history.lock().unwrap();
            events.extend(history.events.iter().cloned());
            history.stats()
        };
        let start = self
            .wall_epoch
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "trace origin predates Unix epoch",
                )
            })?;
        write!(w, "{{\"title\":\"dynalloc GC trace")?;
        if stats.evicted != 0 || stats.rejected != 0 {
            write!(
                w,
                " ({} retained; {} earlier events evicted; {} events rejected)",
                stats.retained, stats.evicted, stats.rejected
            )?;
        }
        write!(
            w,
            "\",\"start\":[{},{}],\"states\":{{",
            start.as_secs(),
            start.subsec_nanos()
        )?;
        for (i, state) in ALL_STATES.iter().enumerate() {
            if i != 0 {
                write!(w, ",")?;
            }
            write!(
                w,
                "\"{}\":{{\"value\":{},\"color\":\"{}\"}}",
                state.name(),
                state.value(),
                state.color()
            )?;
        }
        writeln!(w, "}}}}")?;
        for event in events {
            write!(w, "{{\"time\":\"{}\",\"entity\":", event.time_ns)?;
            match &event.entity {
                Entity::Gc => w.write_all(b"\"gc\"")?,
                Entity::Thread(id) => write!(w, "\"thread{}\"", id)?,
                Entity::Custom(name) => write_json_string(w, name)?,
            }
            writeln!(w, ",\"state\":{}}}", event.state.value())?;
        }
        Ok(())
    }
}
fn write_json_string<W: Write>(writer: &mut W, value: &str) -> std::io::Result<()> {
    writer.write_all(b"\"")?;
    let mut start = 0;
    let bytes = value.as_bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'"' || byte == b'\\' || byte < 0x20 {
            writer.write_all(&bytes[start..index])?;
            match byte {
                b'"' => writer.write_all(b"\\\"")?,
                b'\\' => writer.write_all(b"\\\\")?,
                b'\n' => writer.write_all(b"\\n")?,
                b'\r' => writer.write_all(b"\\r")?,
                b'\t' => writer.write_all(b"\\t")?,
                _ => {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    writer.write_all(&[
                        b'\\',
                        b'u',
                        b'0',
                        b'0',
                        HEX[(byte >> 4) as usize],
                        HEX[(byte & 15) as usize],
                    ])?;
                }
            }
            start = index + 1;
        }
    }
    writer.write_all(&bytes[start..])?;
    writer.write_all(b"\"")
}

#[cfg(test)]
#[path = "statemap_tests.rs"]
mod tests;
