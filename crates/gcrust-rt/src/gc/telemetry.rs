//! Bounded collection history, lifetime aggregates, and opt-in streamed traces.
use super::heap::{GcEvent, GcKind};
use std::collections::VecDeque;
use std::io::{self, BufWriter, Write};

/// Maximum number of recent collection events retained by each heap.
pub const GC_EVENT_HISTORY_LIMIT: usize = 1024;
const SUB_BUCKET_BITS: u32 = 6;
const SUB_BUCKETS: usize = 1 << SUB_BUCKET_BITS;
const HISTOGRAM_BINS: usize = SUB_BUCKETS * (65 - SUB_BUCKET_BITS as usize);
const QUANTILE_ERROR_RATIO: f64 = 1.0 / SUB_BUCKETS as f64;

pub(super) fn kind_index(kind: GcKind) -> usize {
    match kind {
        GcKind::Minor => 0,
        GcKind::Major => 1,
        GcKind::Concurrent => 2,
    }
}

pub(super) fn pause_percentile(sorted: &[u64], percent: usize) -> u64 {
    assert!((1..=100).contains(&percent));
    if sorted.is_empty() {
        return 0;
    }
    sorted[nearest_rank(sorted.len() as u128, percent) as usize - 1]
}

fn nearest_rank(count: u128, percent: usize) -> u128 {
    (count / 100) * percent as u128 + ((count % 100) * percent as u128).div_ceil(100)
}

// Integer logarithmic buckets: values below 128 ns are exact. Each subsequent
// power-of-two interval has 64 equal-width buckets. An upper bucket endpoint
// overestimates any value it represents by strictly less than value / 64.
struct PauseHistogram {
    bins: Vec<u128>,
    count: u128,
    overflowed: bool,
}

impl PauseHistogram {
    fn new() -> Self {
        Self {
            bins: vec![0; HISTOGRAM_BINS],
            count: 0,
            overflowed: false,
        }
    }

    fn record(&mut self, value: u64) {
        let index = if value < SUB_BUCKETS as u64 {
            value as usize
        } else {
            let exponent = 63 - value.leading_zeros();
            let shift = exponent - SUB_BUCKET_BITS;
            SUB_BUCKETS + shift as usize * SUB_BUCKETS + (value >> shift) as usize - SUB_BUCKETS
        };
        self.bins[index] = self.bins[index].saturating_add(1);
        match self.count.checked_add(1) {
            Some(next) => self.count = next,
            None => self.overflowed = true,
        }
    }

    fn percentile_upper(&self, percent: usize, max: u64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        // Once even the 128-bit sample census overflows, max remains an honest
        // upper bound but no relative quantile error is certified.
        if self.overflowed {
            return max;
        }
        let rank = nearest_rank(self.count, percent);
        let mut cumulative = 0u128;
        for (index, count) in self.bins.iter().enumerate() {
            cumulative = cumulative.saturating_add(*count);
            if cumulative >= rank {
                let upper = if index < SUB_BUCKETS {
                    index as u64
                } else {
                    let offset = index - SUB_BUCKETS;
                    let shift = offset / SUB_BUCKETS;
                    let bucket = SUB_BUCKETS + offset % SUB_BUCKETS;
                    ((((bucket + 1) as u128) << shift) - 1) as u64
                };
                return upper.min(max);
            }
        }
        unreachable!("histogram sample census disagrees with buckets")
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct GcStats {
    pub count: u64,
    pub pause_total: u64,
    pub elapsed_total: u64,
    pub pause_max: u64,
    pub reclaimed: u64,
    pub promoted: u64,
    pub p50: u64,
    pub p99: u64,
    // Zero for exact quantiles, 1/64 for histogram upper estimates; -1 only
    // after overflowing the 128-bit histogram sample census (no error bound).
    pub quantile_error_ratio: f64,
}

impl GcStats {
    fn record(&mut self, event: GcEvent) {
        self.count = self.count.saturating_add(1);
        self.pause_total = self.pause_total.saturating_add(event.pause_ns);
        self.elapsed_total = self.elapsed_total.saturating_add(event.elapsed_ns);
        self.pause_max = self.pause_max.max(event.pause_ns);
        self.reclaimed = self.reclaimed.saturating_add(
            event
                .before_bytes
                .saturating_sub(event.after_bytes)
                .saturating_sub(event.promoted_bytes),
        );
        self.promoted = self.promoted.saturating_add(event.promoted_bytes);
    }
}

pub(super) struct TelemetrySnapshot {
    pub total: GcStats,
    pub by_kind: [GcStats; 3],
    pub retained: usize,
}

struct Trace {
    writer: Option<BufWriter<Box<dyn Write + Send>>>,
    error: Option<(io::ErrorKind, String)>,
}

impl Trace {
    fn remember(&mut self, error: io::Error) {
        self.error = Some((error.kind(), error.to_string()));
        // A failed BufWriter otherwise retries its buffered write in Drop.
        // Disassemble it without I/O so a reported failure stays terminal,
        // including during Heap teardown. The embedding owns its sink's Drop.
        if let Some(writer) = self.writer.take() {
            let (sink, _unwritten) = writer.into_parts();
            drop(sink);
        }
    }

    fn record(&mut self, event: GcEvent) {
        if self.error.is_none() {
            if let Err(error) = write_event_jsonl(
                self.writer.as_mut().expect("healthy trace has a writer"),
                event,
            ) {
                self.remember(error);
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.error.is_none() {
            if let Err(error) = self
                .writer
                .as_mut()
                .expect("healthy trace has a writer")
                .flush()
            {
                self.remember(error);
            }
        }
        match &self.error {
            Some((kind, message)) => Err(io::Error::new(*kind, message.clone())),
            None => Ok(()),
        }
    }
}

pub(super) struct GcTelemetry {
    recent: VecDeque<GcEvent>,
    total: GcStats,
    by_kind: [GcStats; 3],
    // Allocated only when the first event would leave the recent window. At
    // that transition all previous events are still available to seed them.
    histograms: Option<[PauseHistogram; 4]>,
    trace: Option<Trace>,
}

impl GcTelemetry {
    pub fn new() -> Self {
        Self {
            recent: VecDeque::new(),
            total: GcStats::default(),
            by_kind: [GcStats::default(); 3],
            histograms: None,
            trace: None,
        }
    }

    pub fn check_trace_start(&self) -> io::Result<()> {
        if self.trace.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "GC trace already configured",
            ));
        }
        if self.total.count != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "GC tracing must be configured before the first collection",
            ));
        }
        Ok(())
    }

    pub fn start_trace(&mut self, writer: Box<dyn Write + Send>) -> io::Result<()> {
        self.check_trace_start()?;
        self.trace = Some(Trace {
            writer: Some(BufWriter::with_capacity(8192, writer)),
            error: None,
        });
        Ok(())
    }

    pub fn flush_trace(&mut self) -> io::Result<()> {
        match &mut self.trace {
            Some(trace) => trace.flush(),
            None => Ok(()),
        }
    }

    pub fn record(&mut self, mut event: GcEvent) {
        event.seq = self.total.count;
        if self.recent.len() == GC_EVENT_HISTORY_LIMIT {
            if self.histograms.is_none() {
                let mut histograms = std::array::from_fn(|_| PauseHistogram::new());
                for old in &self.recent {
                    histograms[0].record(old.pause_ns);
                    histograms[kind_index(old.kind) + 1].record(old.pause_ns);
                }
                self.histograms = Some(histograms);
            }
            self.recent.pop_front();
        }
        if let Some(histograms) = &mut self.histograms {
            histograms[0].record(event.pause_ns);
            histograms[kind_index(event.kind) + 1].record(event.pause_ns);
        }
        self.total.record(event);
        self.by_kind[kind_index(event.kind)].record(event);
        if let Some(trace) = &mut self.trace {
            trace.record(event);
        }
        self.recent.push_back(event);
    }

    pub fn events(&self) -> Vec<GcEvent> {
        self.recent.iter().copied().collect()
    }

    pub fn snapshot(&self) -> TelemetrySnapshot {
        let mut stats = [
            self.total,
            self.by_kind[0],
            self.by_kind[1],
            self.by_kind[2],
        ];
        for (index, stat) in stats.iter_mut().enumerate() {
            if let Some(histograms) = &self.histograms {
                stat.p50 = histograms[index].percentile_upper(50, stat.pause_max);
                stat.p99 = histograms[index].percentile_upper(99, stat.pause_max);
                stat.quantile_error_ratio = if histograms[index].overflowed {
                    -1.0
                } else {
                    QUANTILE_ERROR_RATIO
                };
            } else {
                let mut pauses: Vec<_> = self
                    .recent
                    .iter()
                    .filter(|event| index == 0 || kind_index(event.kind) + 1 == index)
                    .map(|event| event.pause_ns)
                    .collect();
                pauses.sort_unstable();
                stat.p50 = pause_percentile(&pauses, 50);
                stat.p99 = pause_percentile(&pauses, 99);
            }
        }
        TelemetrySnapshot {
            total: stats[0],
            by_kind: [stats[1], stats[2], stats[3]],
            retained: self.recent.len(),
        }
    }
}

pub(super) fn write_event_jsonl(writer: &mut dyn Write, event: GcEvent) -> io::Result<()> {
    writeln!(
        writer,
        "{{\"seq\":{},\"kind\":\"{}\",\"pause_ns\":{},\"elapsed_ns\":{},\"before_bytes\":{},\"after_bytes\":{},\"reclaimed_bytes\":{},\"promoted_bytes\":{}}}",
        event.seq,
        event.kind.as_str(),
        event.pause_ns,
        event.elapsed_ns,
        event.before_bytes,
        event.after_bytes,
        event
            .before_bytes
            .saturating_sub(event.after_bytes)
            .saturating_sub(event.promoted_bytes),
        event.promoted_bytes
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: GcKind, pause_ns: u64) -> GcEvent {
        GcEvent {
            kind,
            seq: 0,
            pause_ns,
            elapsed_ns: pause_ns.saturating_add(10),
            before_bytes: 100,
            after_bytes: 40,
            promoted_bytes: 10,
        }
    }

    #[test]
    fn collector_telemetry_exact_window_transitions_without_losing_samples() {
        let mut telemetry = GcTelemetry::new();
        for pause in 1..=GC_EVENT_HISTORY_LIMIT as u64 {
            telemetry.record(event(GcKind::Minor, pause));
        }
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.total.p50, 512);
        assert_eq!(snapshot.total.p99, 1014);
        assert_eq!(snapshot.total.quantile_error_ratio, 0.0);
        assert!(telemetry.histograms.is_none());
        telemetry.record(event(GcKind::Major, 1_000_000));
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.retained, GC_EVENT_HISTORY_LIMIT);
        assert_eq!(snapshot.total.count, 1025);
        assert_eq!(snapshot.by_kind[0].count, 1024);
        assert_eq!(snapshot.by_kind[1].count, 1);
        assert_eq!(snapshot.by_kind[1].p50, 1_000_000);
        assert_eq!(snapshot.total.quantile_error_ratio, QUANTILE_ERROR_RATIO);
        assert_eq!(telemetry.events()[0].seq, 1);
        assert_eq!(telemetry.events().last().unwrap().seq, 1024);
        assert_eq!(telemetry.histograms.as_ref().unwrap()[0].count, 1025);
    }

    #[test]
    fn collector_telemetry_lifetime_metrics_survive_many_history_rollovers() {
        let mut telemetry = GcTelemetry::new();
        let mut pauses = [Vec::new(), Vec::new(), Vec::new()];
        let mut total_ns = 0u64;
        for index in 0..30_000 {
            let kind = [GcKind::Minor, GcKind::Major, GcKind::Concurrent][index % 3];
            // The recent window's distribution is deliberately unlike the
            // lifetime distribution; deriving stats from the window is wrong.
            let pause = if index < 28_000 {
                1_000_000 + index as u64
            } else {
                1
            };
            telemetry.record(event(kind, pause));
            pauses[index % 3].push(pause);
            total_ns += pause;
        }
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.total.count, 30_000);
        assert_eq!(snapshot.total.pause_total, total_ns);
        assert_eq!(snapshot.total.elapsed_total, total_ns + 300_000);
        assert_eq!(snapshot.total.reclaimed, 1_500_000);
        assert_eq!(snapshot.total.promoted, 300_000);
        assert_eq!(snapshot.total.pause_max, 1_027_999);
        assert_eq!(snapshot.retained, GC_EVENT_HISTORY_LIMIT);
        assert!(telemetry.recent.capacity() <= GC_EVENT_HISTORY_LIMIT);
        for (index, mut samples) in pauses.into_iter().enumerate() {
            samples.sort_unstable();
            let stat = snapshot.by_kind[index];
            assert_eq!(stat.count, 10_000);
            assert_eq!(stat.reclaimed, 500_000);
            assert_eq!(stat.promoted, 100_000);
            for (percent, upper) in [(50, stat.p50), (99, stat.p99)] {
                let exact = pause_percentile(&samples, percent);
                assert!(upper >= exact);
                assert!(upper - exact <= exact / 64);
            }
        }
        let histograms = telemetry.histograms.as_ref().unwrap();
        assert_eq!(histograms[0].count, 30_000);
        for histogram in histograms {
            assert_eq!(histogram.bins.len(), HISTOGRAM_BINS);
            assert_eq!(histogram.bins.capacity(), HISTOGRAM_BINS);
        }
        let events = telemetry.events();
        assert!(events.iter().all(|event| event.pause_ns == 1));
        assert_eq!(events[0].seq, 30_000 - GC_EVENT_HISTORY_LIMIT as u64);
        assert_eq!(events.last().unwrap().seq, 29_999);
    }

    #[test]
    fn collector_telemetry_histogram_bounds_match_independent_sorted_quantiles() {
        let mut samples = vec![0, 1, 63, 64, 127, 128, u64::MAX];
        for bit in 1..64 {
            let boundary = 1u64 << bit;
            samples.extend([boundary - 1, boundary, boundary + 1]);
        }
        let mut seed = 0x29e8_8f22_349b_0761u64;
        for _ in 0..50_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            samples.push(seed >> (seed % 64));
        }
        let mut histogram = PauseHistogram::new();
        for &sample in &samples {
            let mut singleton = PauseHistogram::new();
            singleton.record(sample);
            // Do not clamp to the observed maximum here: exercise the bucket
            // endpoints themselves, including u64::MAX and the zero bucket.
            let upper = singleton.percentile_upper(50, u64::MAX);
            assert!(upper >= sample, "{sample} -> {upper}");
            assert!(upper - sample <= sample / 64, "{sample} -> {upper}");
            histogram.record(sample);
        }
        samples.sort_unstable();
        for percent in 1..=100 {
            let exact = pause_percentile(&samples, percent);
            let upper = histogram.percentile_upper(percent, *samples.last().unwrap());
            assert!(upper >= exact, "p{percent}: {exact} -> {upper}");
            assert!(
                upper - exact <= exact / 64,
                "p{percent}: {exact} -> {upper}"
            );
        }
    }

    #[test]
    fn collector_telemetry_histogram_census_overflow_uses_honest_maximum() {
        let mut histogram = PauseHistogram::new();
        histogram.record(1);
        histogram.count = u128::MAX;
        histogram.record(2);
        assert!(histogram.overflowed);
        assert_eq!(histogram.percentile_upper(50, 2), 2);
        assert_eq!(histogram.percentile_upper(99, 2), 2);
    }
}
