//! Native string staging with rooted, bounded-copy safepoints.
use super::{
    STR_DATA_OFF, Thread, abort_with_diagnostic, ai_gc_pollcheck_slow, blocking_region, str_bytes,
};
use crate::gc::Heap;

pub(super) const COPY_BYTES: usize = 32 * 1024;

pub(super) fn output_limit(heap: &Heap) -> Option<usize> {
    (heap.max_object_allocation_bytes() & !7).checked_sub(STR_DATA_OFF)
}
fn checked_end(current: usize, additional: usize, limit: usize) -> Option<usize> {
    current.checked_add(additional).filter(|&end| end <= limit)
}
fn growth_target(capacity: usize, needed: usize, limit: usize) -> usize {
    needed.max(capacity.saturating_mul(2)).min(limit)
}

pub(super) struct Buffer {
    pub(super) bytes: Vec<u8>,
    limit: usize,
}
impl Buffer {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
    pub(super) unsafe fn check(&self, thread: *mut Thread, additional: usize) -> usize {
        checked_end(self.bytes.len(), additional, self.limit).unwrap_or_else(|| unsafe {
            abort_with_diagnostic(
                thread,
                b"gc-rust: string result exceeds maximum object size",
            )
        })
    }
    /// Every managed value the caller will use after this call must be rooted.
    /// Large native growth runs BLOCKED: no managed borrow enters the allocator.
    pub(super) unsafe fn reserve(&mut self, thread: *mut Thread, needed: usize) {
        if needed > self.limit {
            unsafe {
                abort_with_diagnostic(
                    thread,
                    b"gc-rust: string result exceeds maximum object size",
                );
            }
        }
        if needed <= self.bytes.capacity() {
            return;
        }
        let target = growth_target(self.bytes.capacity(), needed, self.limit);
        let additional = target - self.bytes.len();
        let result = if target > COPY_BYTES {
            unsafe { blocking_region(thread, || self.bytes.try_reserve_exact(additional)) }
        } else {
            self.bytes.try_reserve_exact(additional)
        };
        if result.is_err() {
            unsafe {
                abort_with_diagnostic(thread, b"gc-rust: native string buffer allocation failed");
            }
        }
        debug_assert!(self.bytes.capacity() <= self.limit);
    }
    /// Append one complete immutable String. Only a possible polling/growth path
    /// needs a temporary root; small non-growing copies retain the leaf path.
    #[inline]
    pub(super) unsafe fn append(&mut self, thread: *mut Thread, source: *const u8) {
        let len = unsafe { str_bytes(source) }.len();
        unsafe {
            self.append_range(thread, source, 0..len);
        }
    }
    /// Append a checked byte range of an immutable String. The range must fit
    /// its initialized payload; its owner is pinned before any possible park.
    pub(super) unsafe fn append_range(
        &mut self,
        thread: *mut Thread,
        source: *const u8,
        range: std::ops::Range<usize>,
    ) {
        debug_assert!(range.start <= range.end && range.end <= unsafe { str_bytes(source) }.len());
        let len = range.len();
        let end = unsafe { self.check(thread, len) };
        let may_park = len > COPY_BYTES
            || (end > self.bytes.capacity()
                && growth_target(self.bytes.capacity(), end, self.limit) > COPY_BYTES);
        if !may_park {
            unsafe {
                self.reserve(thread, end);
            }
            self.bytes
                .extend_from_slice(&unsafe { str_bytes(source) }[range]);
            return;
        }
        let dyna = unsafe { &*(*thread).dyna_thread };
        let roots = unsafe { dyna.scratch_scope() };
        let source_root = unsafe { roots.push(source) };
        #[cfg(test)]
        unsafe {
            tests::piece_checkpoint(thread);
        }
        unsafe {
            self.reserve(thread, end);
        }
        for offset in (range.start..range.end).step_by(COPY_BYTES) {
            #[cfg(test)]
            if offset - range.start == COPY_BYTES {
                unsafe {
                    super::string_owned::tests::checkpoint(thread);
                }
            }
            unsafe {
                poll(thread);
            }
            let take = (range.end - offset).min(COPY_BYTES);
            // All slices die before the next poll. The String is immutable.
            let bytes = unsafe {
                std::slice::from_raw_parts(roots.get(source_root).add(STR_DATA_OFF + offset), take)
            };
            debug_assert!(self.bytes.capacity() - self.bytes.len() >= take);
            self.bytes.extend_from_slice(bytes);
        }
    }
}

pub(super) unsafe fn poll(thread: *mut Thread) {
    if unsafe { &*(*thread).heap }.gc_requested() {
        unsafe {
            ai_gc_pollcheck_slow(thread);
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::*;
    use super::{Buffer, checked_end, output_limit};
    use crate::gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo};
    use std::sync::{
        Arc, OnceLock,
        atomic::{AtomicPtr, Ordering},
    };
    static HEAP: AtomicPtr<crate::gc::Heap> = AtomicPtr::new(std::ptr::null_mut());
    static READY: OnceLock<std::sync::mpsc::Sender<()>> = OnceLock::new();

    fn armed(thread: *mut Thread, mode: &str) -> bool {
        let heap = HEAP.load(Ordering::Acquire);
        !heap.is_null()
            && heap == unsafe { (*thread).heap }
            && std::env::var("GCR_STRING_COPY_MODE").unwrap_or_else(|_| "input".into()) == mode
    }
    pub(crate) unsafe fn piece_checkpoint(thread: *mut Thread) {
        if armed(thread, "piece") {
            READY.get().unwrap().send(()).unwrap();
            let heap = HEAP.load(Ordering::Acquire);
            while !unsafe { &*heap }.gc_requested() {
                std::thread::yield_now();
            }
        }
    }
    pub(crate) unsafe fn output_checkpoint(thread: *mut Thread) {
        if armed(thread, "output") {
            READY.get().unwrap().send(()).unwrap();
            let heap = HEAP.load(Ordering::Acquire);
            while !unsafe { &*heap }.gc_requested() {
                std::thread::yield_now();
            }
        }
    }
    pub(crate) unsafe fn input_checkpoint(thread: *mut Thread) {
        let heap = HEAP.load(Ordering::Acquire);
        if armed(thread, "input") {
            READY.get().unwrap().send(()).unwrap();
            while !unsafe { &*heap }.gc_requested() {
                std::thread::yield_now();
            }
        }
    }
    pub(crate) unsafe fn input_finished(thread: *mut Thread) {
        let heap = HEAP.load(Ordering::Acquire);
        if armed(thread, "input") {
            assert!(
                unsafe { &*heap }.collections() > 0,
                "join did not service collection during native preparation"
            );
        }
    }
    #[test]
    fn collector_string_copy_join_services_collection_before_allocation() {
        const NAME: &str = "runtime::string_copy::tests::collector_string_copy_join_services_collection_before_allocation";
        if !in_child(NAME, "input") {
            return;
        }
        let mut rt = runtime_for_copy(vec![
            TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0),
            TypeInfo::for_header(Full::SIZE)
                .with_type_id(1)
                .with_varlen_values(0),
        ]);
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            let source = alloc_string_from_text(thread, 0, "x");
            let source_root = heap.globals.add(source as u64);
            let separator = alloc_string_from_text(thread, 0, "λ");
            let separator_root = heap.globals.add(separator as u64);
            let array = ai_gc_alloc_varlen(thread, 1, 1024, 0);
            let array_root = heap.globals.add(array as u64);
            for index in 0..1024 {
                slots::ai_slot_ref_store(
                    thread,
                    array,
                    24 + index * 8,
                    heap.globals.get(source_root) as *mut u8,
                );
            }
            let (ready, waiting) = std::sync::mpsc::channel();
            READY.set(ready).unwrap();
            HEAP.store(Arc::as_ptr(&heap).cast_mut(), Ordering::Release);
            let other = heap.clone();
            let collector = std::thread::spawn(move || {
                waiting.recv().unwrap();
                other.stw_collect::<IdentityPtrPolicy>();
            });
            let result = ai_str_join(
                thread,
                0,
                heap.globals.get(array_root) as *const u8,
                1024,
                heap.globals.get(separator_root) as *const u8,
            );
            let result_root = heap.globals.add(result as u64);
            blocking_region(thread, || collector.join().unwrap());
            HEAP.store(std::ptr::null_mut(), Ordering::Release);
            assert_eq!(
                str_bytes(heap.globals.get(result_root) as *const u8),
                format!("{}x", "xλ".repeat(1023)).as_bytes()
            );
        });
    }
    fn runtime_for_copy(types: Vec<TypeInfo>) -> RuntimeContext {
        if std::env::var("GCR_STRING_COPY_GENERATIONAL").as_deref() == Ok("1") {
            RuntimeContext::new_generational(32768, 2 << 20, types)
        } else {
            RuntimeContext::new(2 << 20, types)
        }
    }
    fn in_child(name: &str, mode: &str) -> bool {
        if std::env::var_os("GCR_STRING_COPY_EXTRA_CHILD").is_some() {
            return true;
        }
        use std::{
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        for generational in ["0", "1"] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name])
                .env("GCR_STRING_COPY_EXTRA_CHILD", "1")
                .env("GCR_STRING_COPY_MODE", mode)
                .env("GCR_STRING_COPY_GENERATIONAL", generational)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    panic!(
                        "copy phase did not poll: {:?}",
                        child.wait_with_output().unwrap()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        false
    }
    #[test]
    fn collector_string_copy_output_root_reloads_after_collection() {
        if !in_child(
            "runtime::string_copy::tests::collector_string_copy_output_root_reloads_after_collection",
            "output",
        ) {
            return;
        }
        let text = "λ中🦀".repeat(8192);
        let mut rt = runtime_for_copy(vec![TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0)]);
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            let source = alloc_string_from_text(thread, 0, &text);
            let source_root = heap.globals.add(source as u64);
            let (ready, waiting) = std::sync::mpsc::channel();
            READY.set(ready).unwrap();
            HEAP.store(Arc::as_ptr(&heap).cast_mut(), Ordering::Release);
            let other = heap.clone();
            let collector = std::thread::spawn(move || {
                waiting.recv().unwrap();
                other.stw_collect::<IdentityPtrPolicy>();
            });
            let p = heap.globals.get(source_root) as *const u8;
            let output = ai_str_concat(thread, 0, p, p);
            let root = heap.globals.add(output as u64);
            blocking_region(thread, || collector.join().unwrap());
            HEAP.store(std::ptr::null_mut(), Ordering::Release);
            assert!(
                str_bytes(heap.globals.get(root) as *const u8) == text.repeat(2).as_bytes(),
                "output bytes changed after collection"
            );
            assert_eq!((&*(*thread).dyna_thread).scratch_mark(), 0);
        });
    }
    #[test]
    fn collector_string_copy_pins_observed_slot_while_another_writer_replaces_it() {
        if !in_child(
            "runtime::string_copy::tests::collector_string_copy_pins_observed_slot_while_another_writer_replaces_it",
            "piece",
        ) {
            return;
        }
        let text = "original-λ".repeat(8192);
        let mut rt = runtime_for_copy(vec![
            TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0),
            TypeInfo::for_header(Full::SIZE)
                .with_type_id(1)
                .with_varlen_values(0),
        ]);
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            let source = alloc_string_from_text(thread, 0, &text);
            let source_root = heap.globals.add(source as u64);
            let replacement = alloc_string_from_text(thread, 0, "replacement");
            let replacement_root = heap.globals.add(replacement as u64);
            let separator = alloc_string_from_text(thread, 0, "");
            let separator_root = heap.globals.add(separator as u64);
            let array = ai_gc_alloc_varlen(thread, 1, 1, 0);
            let array_root = heap.globals.add(array as u64);
            slots::ai_slot_ref_store(thread, array, 24, heap.globals.get(source_root) as *mut u8);
            heap.globals.set(source_root, 0);
            let (ready, waiting) = std::sync::mpsc::channel();
            READY.set(ready).unwrap();
            HEAP.store(Arc::as_ptr(&heap).cast_mut(), Ordering::Release);
            let (registered, registration) = std::sync::mpsc::channel();
            let other = heap.clone();
            let collector = std::thread::spawn(move || {
                let mt = crate::gc::MutatorThread::<IdentityPtrPolicy>::register(other.clone());
                registered.send(()).unwrap();
                waiting.recv().unwrap();
                {
                    let access = other.object_access(other.globals.get(array_root) as *mut u8);
                    (&*access
                        .as_ptr()
                        .add(24)
                        .cast::<std::sync::atomic::AtomicU64>())
                        .store(other.globals.get(replacement_root), Ordering::SeqCst);
                }
                mt.state().enter_blocked();
                other.stw_collect::<IdentityPtrPolicy>();
                mt.state().exit_blocked(&other);
            });
            blocking_region(thread, || registration.recv().unwrap());
            let output = ai_str_join(
                thread,
                0,
                heap.globals.get(array_root) as *const u8,
                1,
                heap.globals.get(separator_root) as *const u8,
            );
            let root = heap.globals.add(output as u64);
            blocking_region(thread, || collector.join().unwrap());
            HEAP.store(std::ptr::null_mut(), Ordering::Release);
            assert!(
                str_bytes(heap.globals.get(root) as *const u8) == text.as_bytes(),
                "observed string changed after slot replacement"
            );
            assert_eq!(
                slots::ai_slot_bits64_load(thread, heap.globals.get(array_root) as *mut u8, 24),
                heap.globals.get(replacement_root)
            );
            assert_eq!((&*(*thread).dyna_thread).scratch_mark(), 0);
        });
    }
    #[test]
    fn collector_string_copy_bounds_match_arena_and_native_capacity() {
        for nursery in [0, 1024, 8192] {
            let info = TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0);
            let mut rt = if nursery == 0 {
                RuntimeContext::new(4096, vec![info])
            } else {
                RuntimeContext::new_generational(nursery, 4096, vec![info])
            };
            let heap = rt.heap().clone();
            let limit = output_limit(&heap).unwrap();
            assert_eq!(limit, nursery.max(4096) - 24);
            for current in [0, 1, limit - 1, limit] {
                for additional in [0, 1, limit, usize::MAX] {
                    let wide = current as u128 + additional as u128;
                    assert_eq!(
                        checked_end(current, additional, limit),
                        (wide <= limit as u128).then_some(wide as usize)
                    );
                }
            }
            rt.enter(|thread| unsafe {
                let mut buffer = Buffer::new(limit);
                for needed in [1, 7, 100, limit] {
                    buffer.reserve(thread, needed);
                    assert!(buffer.bytes.capacity() >= needed && buffer.bytes.capacity() <= limit);
                }
                assert!(buffer.bytes.is_empty());
            });
        }
    }
    #[test]
    fn collector_string_copy_rejects_alias_output_beyond_arena() {
        const NAME: &str =
            "runtime::string_copy::tests::collector_string_copy_rejects_alias_output_beyond_arena";
        if let Ok(mode) = std::env::var("GCR_STRING_COPY_LIMIT_CHILD") {
            let mut rt = RuntimeContext::new(
                1024,
                vec![
                    TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0),
                    TypeInfo::for_header(Full::SIZE)
                        .with_type_id(1)
                        .with_varlen_values(0),
                ],
            );
            rt.enter(|thread| unsafe {
                let heap = &*(*thread).heap;
                let text = "x".repeat(if mode == "concat" { 600 } else { 400 });
                let source = alloc_string_from_text(thread, 0, &text);
                let root = heap.globals.add(source as u64);
                if mode == "concat" {
                    ai_str_concat(thread, 0, source, source);
                } else {
                    let separator = alloc_string_from_text(thread, 0, "");
                    let sep_root = heap.globals.add(separator as u64);
                    let array = ai_gc_alloc_varlen(thread, 1, 3, 0);
                    heap.globals.add(array as u64);
                    for index in 0..3 {
                        slots::ai_slot_ref_store(
                            thread,
                            array,
                            24 + index * 8,
                            heap.globals.get(root) as *mut u8,
                        );
                    }
                    ai_str_join(thread, 0, array, 3, heap.globals.get(sep_root) as *const u8);
                }
            });
            panic!("oversized result was accepted");
        }
        for mode in ["concat", "join"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME])
                .env("GCR_STRING_COPY_LIMIT_CHILD", mode)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while child.try_wait().unwrap().is_none() {
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    panic!(
                        "size rejection hung: {:?}",
                        child.wait_with_output().unwrap()
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("string result exceeds maximum object size"),
                "{output:?}"
            );
        }
    }
}
