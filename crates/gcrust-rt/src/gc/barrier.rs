use std::sync::Mutex;
use crate::gc::semi_space::FORWARDING_BIT;

// ─── SATB Write Barrier Buffer ──────────────────────────────────────

/// Thread-local buffer for Snapshot-At-The-Beginning (SATB) write barriers.
///
/// When a mutator overwrites a pointer field during concurrent GC, it
/// logs the *old* value here before the store. This ensures the GC
/// sees all objects that were reachable at the start of the cycle —
/// even if the mutator disconnects them.
///
/// Each mutator thread has its own `SATBBuffer`. The GC drains all
/// buffers during or after the concurrent phase to process any
/// objects the mutator "hid" from the collector.
///
/// Uses a simple Vec<u64> with a flush threshold. When the buffer
/// fills up, it should be flushed to a global queue for the GC to
/// process.
pub struct SATBBuffer {
    /// Local log of old pointer values (pre-write snapshots).
    buf: storage::NativeValues,
    /// Max entries before suggesting a flush.
    capacity: usize,
}

impl SATBBuffer {
    /// Create a new SATB buffer with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self::with_allocator(capacity, crate::runtime::metadata_allocator::MetadataAllocator::default())
    }

    pub(crate) fn with_allocator(capacity: usize, allocator: crate::runtime::metadata_allocator::MetadataAllocator) -> Self {
        Self { buf: storage::NativeValues::with_capacity_in(capacity, allocator), capacity }
    }

    #[cfg(test)]
    pub(crate) fn backing_bytes(&self) -> usize {
        self.buf.capacity() * std::mem::size_of::<u64>()
    }

    /// Log an old value before it is overwritten.
    ///
    /// This is the write barrier's hot path — called before every
    /// pointer store during concurrent GC. Must be fast.
    #[inline(always)]
    pub fn log(&mut self, old_value: u64) {
        self.buf.push(old_value);
    }

    /// Check if the buffer is at or over capacity and should be flushed.
    #[inline(always)]
    pub fn should_flush(&self) -> bool {
        self.buf.len() >= self.capacity
    }

    /// Drain all logged values, returning them.
    pub fn drain(&mut self) -> Vec<u64> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        // The public result is a standalone snapshot; a standard Vec cannot
        // retain this allocation's accounting owner. Reset the local reservation
        // as before, releasing oversized backing through its original allocator.
        // Normal collector flushes use drain_values/publication and reuse it.
        let snapshot = self.buf.iter().copied().collect();
        let reserved = storage::NativeValues::with_capacity_in(self.capacity, self.buf.allocator().clone());
        let old = std::mem::replace(&mut self.buf, reserved);
        drop(old);
        snapshot
    }

    /// Consume a stopped mutator's remaining entries without replacing its
    /// reserved storage. The collector must finish before resuming the owner.
    pub(crate) fn drain_values(&mut self) -> allocator_api2::vec::Drain<'_, u64, crate::runtime::metadata_allocator::MetadataAllocator> {
        self.buf.drain(..)
    }

    /// Number of entries currently buffered.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

// ─── Global SATB Queue ──────────────────────────────────────────────

const SATB_CHUNK_VALUES: usize = 256;

/// Snapshot of pending SATB storage and the current/last bounded cycle.
#[derive(Clone, Copy, Debug, Default)]
pub struct SatbQueueStats {
    pub pending_values: usize,
    pub peak_pending_values: usize,
    pub unique_values: usize,
    pub source_slots: usize,
    pub bitmap_bytes: usize,
    pub pending_capacity_bytes: usize,
    pub pending_chunks: usize,
    pub chunk_metadata_bytes: usize,
    pub bounded_cycle_active: bool,
}

#[derive(Clone, Copy)]
struct SatbSource {
    epoch: usize,
    base: usize,
    size: usize,
    decode: fn(u64) -> Option<*mut u8>,
}
struct SatbPending {
    chunks: allocator_api2::vec::Vec<storage::SatbChunk, crate::runtime::metadata_allocator::MetadataAllocator>,
    source: Option<SatbSource>,
    epoch: usize,
    seen: storage::NativeValues,
    pending: usize,
    peak: usize,
    unique: usize,
    source_slots: usize,
}

// Ownership of container backing follows the drained batch, including Heap drop.
fn take_chunks(state: &mut SatbPending) -> allocator_api2::vec::Vec<storage::SatbChunk, crate::runtime::metadata_allocator::MetadataAllocator> {
    let empty = allocator_api2::vec::Vec::new_in(state.chunks.allocator().clone());
    std::mem::replace(&mut state.chunks, empty)
}

/// Global queue that collects flushed SATB buffers from all threads.
///
/// Standalone queues preserve chunks verbatim. Heap concurrent cycles enable
/// source-address deduplication: one retained value per eight-byte source slot,
/// packed into fixed-size chunks. Pointer-policy callbacks run outside the queue
/// mutex; tracing and chunk destruction do too.
pub struct SATBQueue {
    queue: Mutex<SatbPending>,
}

impl SATBQueue {
    pub fn new() -> Self {
        Self::with_allocator(crate::runtime::metadata_allocator::MetadataAllocator::default())
    }

    pub(crate) fn with_allocator(allocator: crate::runtime::metadata_allocator::MetadataAllocator) -> Self {
        Self {
            queue: Mutex::new(SatbPending {
                chunks: allocator_api2::vec::Vec::new_in(allocator.clone()),
                seen: storage::NativeValues::new_in(allocator),
                source: None, epoch: 0, pending: 0, peak: 0, unique: 0, source_slots: 0,
            }),
        }
    }

    /// Configure before enabling barriers. The coordinator excludes another
    /// cycle; registered producers cannot cross cycle changes in a push (no
    /// safepoint). Keep the source region valid until finish_cycle.
    pub(crate) fn begin_cycle<P: crate::gc::PtrPolicy>(&self, base: *const u8, size: usize) {
        let pending = {
            let mut state = self.queue.lock().unwrap();
            assert!(state.source.is_none(), "SATB cycle already active");
            state.epoch = state
                .epoch
                .checked_add(1)
                .expect("SATB cycle epoch exhausted");
            state.source_slots = size.div_ceil(8);
            let words = state.source_slots.div_ceil(64);
            state.seen.resize(words, 0);
            state.seen.fill(0);
            state.source = Some(SatbSource {
                epoch: state.epoch,
                base: base as usize,
                size,
                decode: P::try_decode_ptr,
            });
            state.pending = 0;
            state.peak = 0;
            state.unique = 0;
            take_chunks(&mut state)
        };
        // Preserve any explicitly supplied pre-cycle entries, applying the
        // same policy without invoking frontend code under the queue lock.
        for chunk in pending {
            match chunk {
                storage::SatbChunk::Standalone(mut values) => self.publish_values(&mut values),
                storage::SatbChunk::Accounted(mut values) => self.publish_values(&mut values),
            }
        }
    }

    pub(crate) fn finish_cycle(&self) {
        let mut state = self.queue.lock().unwrap();
        assert!(state.source.is_some(), "no active SATB cycle");
        assert_eq!(state.pending, 0, "SATB cycle ended before draining");
        state.source = None;
        state.epoch = state
            .epoch
            .checked_add(1)
            .expect("SATB cycle epoch exhausted");
    }

    /// Publish a local chunk without a managed safepoint or access-stripe wait.
    pub fn push(&self, mut buf: Vec<u64>) {
        self.publish_values(&mut buf);
        // Owned input drops after all queue guards have been released.
    }

    /// Flush an owning mutator's buffer without replacing its reserved storage
    /// during a bounded cycle. Standalone queues still take ownership verbatim.
    pub(crate) fn flush_buffer(&self, buffer: &mut SATBBuffer) {
        self.publish_values(&mut buffer.buf);
    }

    fn publish_values<B: storage::SatbInput>(&self, buf: &mut B) {
        let mut start = 0;
        while start < buf.values().len() {
            let source = {
                let mut state = self.queue.lock().unwrap();
                match state.source {
                    Some(source) => source,
                    None => {
                        if start != 0 {
                            buf.discard_prefix(start);
                        }
                        state.pending = state
                            .pending
                            .checked_add(buf.values().len())
                            .expect("SATB size overflow");
                        state.peak = state.peak.max(state.pending);
                        state.unique = state.unique.saturating_add(buf.values().len());
                        let chunk = buf.take_owned();
                        let chunk = match chunk {
                            storage::SatbChunk::Standalone(values) => storage::SatbChunk::Accounted(state.chunks.allocator().adopt_vec(values)),
                            accounted => accounted,
                        };
                        state.chunks.push(chunk);
                        return;
                    }
                }
            };
            // Fixed stack scratch bounds temporary decoding storage, including
            // public callers supplying chunks larger than a mutator's buffer.
            let end = start + (buf.values().len() - start).min(SATB_CHUNK_VALUES);
            let mut addresses = [0usize; SATB_CHUNK_VALUES];
            for (address, &bits) in addresses.iter_mut().zip(&buf.values()[start..end]) {
                *address = (source.decode)(bits).map_or(0, |p| p as usize);
            }
            let mut state = self.queue.lock().unwrap();
            if state.epoch != source.epoch {
                continue;
            }
            for (&address, &bits) in addresses.iter().zip(&buf.values()[start..end]) {
                let Some(offset) = address.checked_sub(source.base) else {
                    continue;
                };
                if address == 0 || offset >= source.size || offset % 8 != 0 {
                    continue;
                }
                let slot = offset / 8;
                let mask = 1u64 << (slot % 64);
                if state.seen[slot / 64] & mask != 0 {
                    continue;
                }
                state.seen[slot / 64] |= mask;
                if state
                    .chunks
                    .last()
                    .is_none_or(|chunk| chunk.len() == SATB_CHUNK_VALUES)
                {
                    let values = storage::NativeValues::with_capacity_in(SATB_CHUNK_VALUES, state.chunks.allocator().clone());
                    state.chunks.push(storage::SatbChunk::Accounted(values));
                }
                state.chunks.last_mut().unwrap().push(bits);
                state.pending += 1;
                state.unique += 1;
                state.peak = state.peak.max(state.pending);
            }
            start = end;
        }
        // Active cycles copied unique values into bounded queue chunks. Keep
        // the local allocation for the next flush, including duplicate-only
        // batches. No pointer to the borrowed local buffer escapes this call.
        buf.clear();
    }

    pub fn stats(&self) -> SatbQueueStats {
        let state = self.queue.lock().unwrap();
        SatbQueueStats {
            pending_values: state.pending,
            peak_pending_values: state.peak,
            unique_values: state.unique,
            source_slots: state.source_slots,
            bitmap_bytes: state.seen.capacity() * std::mem::size_of::<u64>(),
            pending_capacity_bytes: state.chunks.iter().map(|c| c.capacity() * 8).sum(),
            pending_chunks: state.chunks.len(),
            chunk_metadata_bytes: state.chunks.capacity() * std::mem::size_of::<storage::SatbChunk>(),
            bounded_cycle_active: state.source.is_some(),
        }
    }

    /// Drain all queued buffers for GC processing.
    pub fn drain_all(&self) -> Vec<u64> {
        self.drain_chunks().into_iter().flatten().collect()
    }

    /// Transfer existing chunks without holding the queue lock during tracing.
    /// Keep seen bits until cycle end: an already-drained referent was traced.
    pub(crate) fn drain_chunks(&self) -> allocator_api2::vec::Vec<storage::SatbChunk, crate::runtime::metadata_allocator::MetadataAllocator> {
        let mut state = self.queue.lock().unwrap();
        state.pending = 0;
        take_chunks(&mut state)
    }

    pub fn is_empty(&self) -> bool {
        self.queue.lock().unwrap().pending == 0
    }
}
// ─── Read Barrier ───────────────────────────────────────────────────

/// Read barrier: follow forwarding pointer if the object has been relocated.
///
/// During concurrent GC, an object may have been copied to to-space
/// and a forwarding pointer installed in the old location. The read
/// barrier resolves an already-published forwarding entry. It does not protect
/// a subsequent memory access from relocation; use Heap::object_access around
/// the complete operation when collection can overlap mutator accesses.
///
/// # Safety
/// - `ptr` must point to a valid heap object (or be null).
/// - `type_info_offset` must identify the aligned eight-byte forwarding word.
/// - No concurrent header writer may overlap this non-atomic read; use
///   read_barrier_atomic when a collector may publish forwarding concurrently.
#[inline(always)]
pub unsafe fn read_barrier(ptr: *mut u8, type_info_offset: usize) -> *mut u8 {
    if ptr.is_null() {
        return ptr;
    }
    let slot = unsafe { ptr.add(type_info_offset) as *const u64 };
    let word = unsafe { *slot };
    if word & FORWARDING_BIT != 0 {
        (word & !FORWARDING_BIT) as *mut u8
    } else {
        // No forwarding — object is in place.
        ptr
    }
}

/// Atomic read barrier using atomic load for concurrent safety.
///
/// Use this when the GC might be concurrently installing forwarding
/// pointers. Uses `Acquire` ordering to ensure we see the complete
/// copied object after following the forwarding pointer.
///
/// # Safety
/// As read_barrier, except atomic concurrent header writes are allowed. The
/// source storage must remain alive until resolution, and a claimed copy must
/// eventually publish a destination or restore its original header.
#[inline(always)]
pub unsafe fn read_barrier_atomic(ptr: *mut u8, type_info_offset: usize) -> *mut u8 {
    if ptr.is_null() {
        return ptr;
    }
    let slot = unsafe { ptr.add(type_info_offset) as *const std::sync::atomic::AtomicU64 };
    follow_atomic_word(ptr, || unsafe { (*slot).load(std::sync::atomic::Ordering::Acquire) })
}

#[inline(always)]
fn follow_atomic_word(ptr: *mut u8, mut load: impl FnMut() -> u64) -> *mut u8 {
    loop {
        let word = load();
        if word == FORWARDING_BIT {
            // Parallel evacuation claims a source before filling/publishing
            // its destination. This marker is not a forwarded null pointer.
            std::hint::spin_loop();
            continue;
        }
        return if word & FORWARDING_BIT != 0 {
            (word & !FORWARDING_BIT) as *mut u8
        } else { ptr };
    }
}

#[cfg(test)]
mod read_barrier_contract_tests {
    use super::*;
    use crate::gc::{Compact, Full, Heap, IdentityPtrPolicy, ObjHeader, RootSet, TypeInfo};

    #[test]
    fn collector_read_barriers_preserve_odd_type_ids() {
        for id in [1u64, 3, 255, 32769, 65535] {
            let mut word = id;
            let pointer = (&mut word as *mut u64).cast::<u8>();
            assert_eq!(unsafe { read_barrier(pointer, 0) }, pointer);
            assert_eq!(unsafe { read_barrier_atomic(pointer, 0) }, pointer);
        }
        assert!(unsafe { read_barrier_atomic(std::ptr::null_mut(), 0) }.is_null());
    }

    #[test]
    fn collector_read_barrier_waits_for_claim_publication_or_rollback() {
        let mut old_word = 1u64;
        let old = (&mut old_word as *mut u64).cast::<u8>();
        let mut new_word = 1u64;
        let new = (&mut new_word as *mut u64).cast::<u8>();
        for final_word in [(new as u64) | FORWARDING_BIT, 1] {
            let mut states = [FORWARDING_BIT, FORWARDING_BIT, final_word].into_iter();
            let result = follow_atomic_word(old, || states.next().expect("decoder read beyond publication"));
            assert_eq!(result, if final_word == 1 { old } else { new });
            assert!(states.next().is_none(), "decoder returned before publication/rollback");
        }
    }

    #[test]
    fn collector_read_barrier_acquires_published_payload() {
        const HELPER: &str = "GCR_READ_BARRIER_PUBLICATION_HELPER";
        if std::env::var_os(HELPER).is_none() {
            use std::process::{Command, Stdio};
            use std::time::{Duration, Instant};
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "gc::barrier::read_barrier_contract_tests::collector_read_barrier_acquires_published_payload"])
                .env(HELPER, "1").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!("read barrier failed to observe publication: {output:?}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        use std::cell::UnsafeCell;
        use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
        struct Publication {
            source: AtomicU64,
            destination: UnsafeCell<[u64; 2]>,
        }
        // One writer initializes the payload before publishing source with
        // Release; the reader accesses it only after the barrier's Acquire.
        unsafe impl Sync for Publication {}
        let publication = Arc::new(Publication {
            source: AtomicU64::new(FORWARDING_BIT),
            destination: UnsafeCell::new([1, 0]),
        });
        let writer = publication.clone();
        let task = std::thread::spawn(move || {
            let destination = writer.destination.get().cast::<u8>();
            unsafe { (*writer.destination.get())[1] = 42; }
            writer.source.store(destination as u64 | FORWARDING_BIT, Ordering::Release);
        });
        let old = publication.source.as_ptr().cast::<u8>();
        let resolved = unsafe { read_barrier_atomic(old, 0) };
        assert_eq!(resolved, publication.destination.get().cast::<u8>());
        assert_eq!(unsafe { resolved.add(8).cast::<u64>().read() }, 42);
        task.join().unwrap();
    }

    fn actual_collection<H: ObjHeader>() {
        let info = TypeInfo::for_header(H::SIZE).with_type_id(1).with_raw_bytes(8);
        let heap = Heap::new::<H>(1 << 16, vec![info.clone().with_type_id(0), info.clone()]);
        let old = unsafe { heap.alloc_obj::<H>(&info, 0) };
        assert!(!old.is_null());
        unsafe { old.add(H::SIZE).cast::<u64>().write(42); }
        let mut roots = RootSet::new();
        roots.add(old as u64);
        assert_eq!(unsafe { read_barrier(old, H::HEAP_FORWARDING_OFFSET) }, old);
        assert_eq!(unsafe { read_barrier_atomic(old, H::HEAP_FORWARDING_OFFSET) }, old);
        for _ in 0..3 {
            let before = roots.get(0) as *mut u8;
            unsafe { heap.collect::<IdentityPtrPolicy>(&[&roots]); }
            // A completed collection reclaims the source arena. Old addresses
            // are no longer valid read-barrier inputs, even if unpoisoned bytes
            // happen to retain forwarding. Reload the collector-updated root.
            let moved = roots.get(0) as *mut u8;
            assert_ne!(before, moved);
            assert!(heap.from_space_contains(moved));
            assert_eq!(unsafe { read_barrier(moved, H::HEAP_FORWARDING_OFFSET) }, moved);
            assert_eq!(unsafe { read_barrier_atomic(moved, H::HEAP_FORWARDING_OFFSET) }, moved);
            assert_eq!(unsafe { moved.add(H::SIZE).cast::<u64>().read() }, 42);
        }
    }

    #[test]
    fn collector_read_barriers_follow_actual_compact_and_full_collection() {
        actual_collection::<Compact>();
        actual_collection::<Full>();
    }
}

#[cfg(test)]
mod satb_capacity_tests {
    use super::*;

    #[test]
    fn collector_satb_drain_preserves_reserved_local_capacity_and_values() {
        let mut buffer = SATBBuffer::new(256);
        let capacity = buffer.buf.capacity();
        for chunk in 0..8 {
            for index in 0..256 {
                buffer.log(chunk * 256 + index);
            }
            assert!(buffer.should_flush());
            assert_eq!(buffer.drain(), (chunk * 256..(chunk + 1) * 256).collect::<Vec<_>>());
            assert!(buffer.is_empty());
            assert_eq!(buffer.buf.capacity(), capacity);
            let address = buffer.buf.as_ptr();
            assert!(buffer.drain().is_empty());
            assert_eq!(buffer.buf.as_ptr(), address, "empty drain replaced the reserved buffer");
        }
    }
}

#[cfg(test)]
mod satb_consumption_tests {
    use super::*;

    #[test]
    fn collector_satb_chunk_transfer_preserves_allocations_and_releases_queue_lock() {
        let queue = SATBQueue::new();
        let first = vec![11, 22, 33];
        let second = vec![44, 55];
        let addresses = [first.as_ptr(), second.as_ptr()];
        queue.push(first);
        queue.push(second);
        let chunks = queue.drain_chunks();
        assert!(queue.is_empty());
        assert_eq!([chunks[0].as_ptr(), chunks[1].as_ptr()], addresses);
        // A new producer may publish while the old chunks are still consumed;
        // ownership transfer must not retain a queue mutex guard.
        queue.push(vec![66]);
        assert_eq!(chunks.into_iter().flatten().collect::<Vec<_>>(), [11, 22, 33, 44, 55]);
        assert_eq!(queue.drain_all(), [66]);
    }

    #[test]
    fn collector_satb_in_place_consumption_reuses_local_storage() {
        let mut buffer = SATBBuffer::new(256);
        let address = buffer.buf.as_ptr();
        let capacity = buffer.buf.capacity();
        for iteration in 0..8 {
            buffer.log(iteration);
            buffer.log(iteration + 1);
            assert_eq!(buffer.drain_values().collect::<Vec<_>>(), [iteration, iteration + 1]);
            assert!(buffer.is_empty());
            assert_eq!(buffer.buf.as_ptr(), address);
            assert_eq!(buffer.buf.capacity(), capacity);
        }
    }
}

#[cfg(test)]
mod bounded_satb_tests {
    use super::*;
    use crate::gc::{IdentityPtrPolicy, PtrPolicy};

    struct Tagged;
    impl PtrPolicy for Tagged {
        fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
            (bits & 7 != 0).then_some((bits & !7) as *mut u8)
        }
        fn encode_ptr(pointer: *mut u8) -> u64 {
            pointer as u64 | 3
        }
    }

    #[test]
    fn collector_satb_buffer_flush_preserves_values_and_local_storage() {
        let queue = SATBQueue::new();
        let base = 0x1000usize;
        let mut buffer = SATBBuffer::new(1024);
        let address = buffer.buf.as_ptr();
        let capacity = buffer.buf.capacity();
        for _ in 0..2 {
            queue.begin_cycle::<Tagged>(base as *const u8, 513 * 8);
            for index in 0..513 {
                buffer.log((base + index * 8) as u64 | 3);
            }
            buffer.log(7); // tagged null
            buffer.log((base + 513 * 8) as u64 | 3); // outside source
            queue.flush_buffer(&mut buffer);
            assert!(buffer.is_empty());
            assert_eq!(buffer.buf.as_ptr(), address);
            assert_eq!(buffer.buf.capacity(), capacity);
            assert_eq!(queue.stats().unique_values, 513);
            let actual = queue.drain_all();
            assert_eq!(actual, (0..513).map(|i| (base + i * 8) as u64 | 3).collect::<Vec<_>>());
            queue.finish_cycle();
        }
        // Without an active cycle, preserve every input bit and transfer the
        // original allocation, exactly as public owned-chunk publication does.
        buffer.log(11);
        buffer.log(22);
        queue.flush_buffer(&mut buffer);
        assert!(buffer.is_empty());
        let chunks = queue.drain_chunks();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].as_ptr(), address);
        assert_eq!(&*chunks[0], [11, 22]);
    }

    #[test]
    fn collector_satb_source_slots_deduplicate_tags_drains_and_reset() {
        let queue = SATBQueue::new();
        let base = 0x1000usize;
        queue.push(vec![(base | 3) as u64]);
        queue.begin_cycle::<Tagged>(base as *const u8, 520);
        for index in (0..65).rev() {
            for tag in 1..8 {
                queue.push(vec![(base + index * 8) as u64 | tag]);
            }
        }
        queue.push(vec![0, 7, (base - 8) as u64 | 3, (base + 520) as u64 | 3]);
        let stats = queue.stats();
        assert_eq!(stats.unique_values, 65);
        assert_eq!(stats.pending_values, 65);
        assert_eq!(stats.source_slots, 65);
        assert!(stats.bounded_cycle_active);
        let mut pointers: Vec<_> = queue
            .drain_all()
            .into_iter()
            .map(|bits| Tagged::try_decode_ptr(bits).unwrap() as usize)
            .collect();
        pointers.sort_unstable();
        assert_eq!(pointers, (0..65).map(|i| base + i * 8).collect::<Vec<_>>());
        queue.push(vec![base as u64 | 5]);
        assert!(
            queue.is_empty(),
            "draining must not reset cycle deduplication"
        );
        queue.finish_cycle();
        assert!(!queue.stats().bounded_cycle_active);
        queue.begin_cycle::<IdentityPtrPolicy>(base as *const u8, 520);
        queue.push(vec![base as u64, (base + 1) as u64, (base + 520) as u64]);
        assert_eq!(queue.drain_all(), [base as u64]);
        queue.finish_cycle();
    }

    #[test]
    fn collector_satb_sparse_concurrent_publications_pack_unique_values() {
        let queue = SATBQueue::new();
        let base = 0x1000usize;
        queue.begin_cycle::<IdentityPtrPolicy>(base as *const u8, 1024 * 8);
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let queue = &queue;
                scope.spawn(move || {
                    for n in 0..1024 {
                        let address = base + ((n + worker * 17) % 1024) * 8;
                        queue.push(vec![address as u64; 256]);
                    }
                });
            }
        });
        let stats = queue.stats();
        assert_eq!(stats.unique_values, 1024);
        assert_eq!(stats.pending_values, 1024);
        assert_eq!(stats.pending_chunks, 4);
        assert_eq!(stats.pending_capacity_bytes, 1024 * 8);
        assert_eq!(stats.bitmap_bytes, 128);
        let mut values = queue.drain_all();
        values.sort_unstable();
        assert_eq!(
            values,
            (0..1024).map(|i| (base + i * 8) as u64).collect::<Vec<_>>()
        );
        queue.finish_cycle();
    }

    #[test]
    fn collector_satb_policy_callbacks_run_outside_queue_mutex() {
        use std::sync::atomic::{AtomicPtr, Ordering};
        static QUEUE: AtomicPtr<SATBQueue> = AtomicPtr::new(std::ptr::null_mut());
        struct Observed;
        impl PtrPolicy for Observed {
            fn try_decode_ptr(bits: u64) -> Option<*mut u8> {
                let queue = unsafe { &*QUEUE.load(Ordering::Acquire) };
                assert!(queue.queue.try_lock().is_ok(), "callback under queue mutex");
                IdentityPtrPolicy::try_decode_ptr(bits)
            }
            fn encode_ptr(pointer: *mut u8) -> u64 {
                pointer as u64
            }
        }
        let queue = SATBQueue::new();
        QUEUE.store((&queue as *const SATBQueue).cast_mut(), Ordering::Release);
        queue.push(vec![0x1000]);
        queue.begin_cycle::<Observed>(0x1000 as *const u8, 512);
        queue.push(vec![0x1008; 1024]);
        assert_eq!(queue.drain_all(), [0x1000, 0x1008]);
        queue.finish_cycle();
        QUEUE.store(std::ptr::null_mut(), Ordering::Release);
    }
}

#[path = "satb_storage.rs"]
mod storage;

#[cfg(test)]
#[path = "satb_storage_tests.rs"]
mod storage_tests;
