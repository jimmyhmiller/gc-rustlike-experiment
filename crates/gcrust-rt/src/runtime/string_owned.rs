//! Native ownership boundaries for String input to allocating/blocking calls.

use super::{
    Thread, blocking_region, str_bytes,
    string_copy::{Buffer, COPY_BYTES},
    string_range::{self, RangeError},
};

/// The small branch never parks. Outline the rooted large-copy setup so the
/// bounded Vec copy can remain visible to callers without large-path cleanup.
#[inline]
unsafe fn copy_range(
    thread: *mut Thread,
    source: *const u8,
    range: std::ops::Range<usize>,
) -> Vec<u8> {
    if range.len() <= COPY_BYTES {
        let bytes = (unsafe { str_bytes(source) })[range].to_vec();
        #[cfg(test)]
        unsafe {
            tests::input_finished(thread, &bytes);
        }
        bytes
    } else {
        unsafe { copy_large(thread, source, range) }
    }
}

/// Root before native growth/polls; no source borrow reaches validation.
unsafe fn copy_large(
    thread: *mut Thread,
    source: *const u8,
    range: std::ops::Range<usize>,
) -> Vec<u8> {
    let mut buffer = Buffer::new(range.len());
    unsafe {
        buffer.append_range(thread, source, range);
    }
    #[cfg(test)]
    unsafe {
        tests::input_finished(thread, &buffer.bytes);
    }
    buffer.bytes
}

/// The source is a valid immutable String owned by this RUNNING mutator.
#[inline]
pub(super) unsafe fn copy(thread: *mut Thread, source: *const u8) -> Vec<u8> {
    #[cfg(test)]
    unsafe {
        tests::input_started(thread, source);
    }
    let len = unsafe { str_bytes(source) }.len();
    unsafe { copy_range(thread, source, 0..len) }
}

unsafe fn validate(
    thread: *mut Thread,
    bytes: Vec<u8>,
) -> Result<String, std::string::FromUtf8Error> {
    if bytes.len() <= COPY_BYTES {
        String::from_utf8(bytes)
    } else {
        unsafe {
            blocking_region(thread, || {
                #[cfg(test)]
                tests::validation_checkpoint();
                String::from_utf8(bytes)
            })
        }
    }
}

/// As copy; large validation borrows native storage only and runs BLOCKED.
pub(super) unsafe fn text(
    thread: *mut Thread,
    source: *const u8,
) -> Result<String, std::string::FromUtf8Error> {
    let bytes = unsafe { copy(thread, source) };
    unsafe { validate(thread, bytes) }
}

/// Preserve byte clamping, boundary and selected-byte UTF-8 validation rules.
/// Work scales with output bytes. The native copy finishes before validation
/// parks, and before managed output allocation can publish its own safepoints.
pub(super) unsafe fn range(
    thread: *mut Thread,
    source: *const u8,
    start: i64,
    end: i64,
) -> Result<String, RangeError> {
    let range = string_range::checked_range(unsafe { str_bytes(source) }, start, end)?;
    let bytes = unsafe { copy_range(thread, source, range) };
    unsafe { validate(thread, bytes) }.map_err(|_| RangeError::InvalidUtf8)
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::*;
    use crate::gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo};
    use std::{
        process::{Command, Stdio},
        sync::{
            Arc, OnceLock,
            atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };
    static HEAP: AtomicPtr<crate::gc::Heap> = AtomicPtr::new(std::ptr::null_mut());
    static COLLECTED: AtomicBool = AtomicBool::new(false);
    static BEFORE: AtomicUsize = AtomicUsize::new(0);
    static FINISHED: AtomicUsize = AtomicUsize::new(0);
    static READY: OnceLock<std::sync::mpsc::Sender<()>> = OnceLock::new();
    fn text() -> String {
        "λ中🦀".repeat(12000)
    }
    pub(crate) unsafe fn input_started(thread: *mut Thread, source: *const u8) {
        let heap = HEAP.load(Ordering::Acquire);
        if !heap.is_null() && heap == unsafe { (*thread).heap } {
            assert!(!source.is_null(), "missing input root");
        }
    }
    pub(crate) unsafe fn input_finished(thread: *mut Thread, bytes: &[u8]) {
        let heap = HEAP.load(Ordering::Acquire);
        if heap.is_null() || heap != unsafe { (*thread).heap } {
            return;
        }
        let count = FINISHED.fetch_add(1, Ordering::AcqRel);
        let reversed = std::env::var("GCR_STRING_OWNED_OP").as_deref() == Ok("host-text");
        let expected = if (count == 0) == reversed {
            "secondλ".to_owned()
        } else {
            text()
        };
        assert!(
            bytes == expected.as_bytes(),
            "owned source bytes differ from reference"
        );
        if bytes.len() > string_copy::COPY_BYTES
            && std::env::var("GCR_STRING_OWNED_OP").as_deref() != Ok("validation")
        {
            assert!(
                unsafe { &*heap }.collections() > BEFORE.load(Ordering::Acquire),
                "input copy did not service collection before output/validation"
            );
        }
    }
    pub(crate) fn validation_checkpoint() {
        let heap = HEAP.load(Ordering::Acquire);
        if !heap.is_null() && std::env::var("GCR_STRING_OWNED_OP").as_deref() == Ok("validation") {
            READY.get().unwrap().send(()).unwrap();
            // This body owns native bytes only. A collector must finish even
            // while validation has not returned to its managed caller.
            while !COLLECTED.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
    }
    pub(crate) unsafe fn checkpoint(thread: *mut Thread) {
        let heap = HEAP.load(Ordering::Acquire);
        if !heap.is_null()
            && heap == unsafe { (*thread).heap }
            && std::env::var("GCR_STRING_OWNED_LATE").as_deref() == Ok("1")
            && std::env::var("GCR_STRING_OWNED_OP").as_deref() != Ok("validation")
        {
            READY.get().unwrap().send(()).unwrap();
            while !unsafe { &*heap }.gc_requested() {
                std::thread::yield_now();
            }
        }
    }
    fn child(name: &str, ops: &[&str]) -> bool {
        if std::env::var_os("GCR_STRING_OWNED_OP").is_some() {
            return true;
        }
        for &op in ops {
            for (generational, late) in [("0", "0"), ("1", "0"), ("0", "1"), ("1", "1")] {
                let mut child = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", name, "--nocapture"])
                    .env("GCR_STRING_OWNED_OP", op)
                    .env("GCR_STRING_OWNED_GEN", generational)
                    .env("GCR_STRING_OWNED_LATE", late)
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(20);
                while child.try_wait().unwrap().is_none() {
                    if Instant::now() >= deadline {
                        child.kill().unwrap();
                        panic!(
                            "input preparation timed out: {:?}",
                            child.wait_with_output().unwrap()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let output = child.wait_with_output().unwrap();
                assert!(output.status.success(), "{output:?}");
            }
        }
        false
    }
    fn moving() {
        let types = vec![
            TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0),
            TypeInfo::for_header(Full::SIZE)
                .with_type_id(1)
                .with_varlen_values(0),
            TypeInfo::for_header(Full::SIZE)
                .with_type_id(2)
                .with_fields(2)
                .with_raw_bytes(16),
        ];
        let mut rt = if std::env::var("GCR_STRING_OWNED_GEN").as_deref() == Ok("1") {
            RuntimeContext::new_generational(32768, 2 << 20, types)
        } else {
            RuntimeContext::new(2 << 20, types)
        };
        let heap = rt.heap().clone();
        let text = text();
        let op = std::env::var("GCR_STRING_OWNED_OP").unwrap();
        let source_text = if op == "substring" {
            format!("prefixλ/{text}/end")
        } else {
            text.clone()
        };
        rt.enter(|thread| unsafe {
            let roots = (&*(*thread).dyna_thread).scratch_scope();
            let source = roots.push(alloc_string_from_text(thread, 0, &source_text));
            let second = roots.push(alloc_string_from_text(thread, 0, "secondλ"));
            let source = roots.get(source);
            let second = roots.get(second);
            drop(roots);
            BEFORE.store(heap.collections(), Ordering::Release);
            HEAP.store(Arc::as_ptr(&heap).cast_mut(), Ordering::Release);
            let other = heap.clone();
            let late =
                std::env::var("GCR_STRING_OWNED_LATE").as_deref() == Ok("1") || op == "validation";
            let (ready, waiting) = std::sync::mpsc::channel();
            READY.set(ready).unwrap();
            let collector = std::thread::spawn(move || {
                if late {
                    waiting.recv().unwrap();
                }
                other.stw_collect::<IdentityPtrPolicy>();
                COLLECTED.store(true, Ordering::Release);
            });
            if !late {
                while !heap.gc_requested() {
                    std::thread::yield_now();
                }
            }
            let result = match std::env::var("GCR_STRING_OWNED_OP").unwrap().as_str() {
                "substring" => ai_str_substring(
                    thread,
                    0,
                    source,
                    "prefixλ/".len() as i64,
                    ("prefixλ/".len() + text.len()) as i64,
                ),
                "validation" => {
                    assert!(super::text(thread, source).unwrap() == text);
                    std::ptr::null_mut()
                }
                "raw" => {
                    ai_print_str_raw(thread, source);
                    std::ptr::null_mut()
                }
                "line" => {
                    ai_print_str(thread, source);
                    std::ptr::null_mut()
                }
                "read" => {
                    let value = ai_read_file(thread, 0, source);
                    assert!(str_bytes(value).is_empty());
                    value
                }
                "host-text" => {
                    let response = io::ai_host_call(thread, io::STDOUT, 2, 0, 1, second, source, 0);
                    assert_eq!(slots::ai_slot_bits64_load(thread, response, 32), 0);
                    response
                }
                "host" => {
                    let response = io::ai_host_call(thread, io::STDOUT, 2, 0, 1, source, second, 0);
                    assert_eq!(slots::ai_slot_bits64_load(thread, response, 32), 0);
                    response
                }
                _ => unreachable!(),
            };
            let result_root = heap.globals.add(result as u64);
            blocking_region(thread, || collector.join().unwrap());
            HEAP.store(std::ptr::null_mut(), Ordering::Release);
            assert!(FINISHED.load(Ordering::Acquire) > 0);
            if std::env::var("GCR_STRING_OWNED_OP").as_deref() == Ok("substring") {
                assert!(str_bytes(heap.globals.get(result_root) as *const u8) == text.as_bytes());
            }
            assert_eq!((&*(*thread).dyna_thread).scratch_mark(), 0);
        });
    }
    #[test]
    fn collector_string_owned_substring_preparation_polls() {
        if child(
            "runtime::string_owned::tests::collector_string_owned_substring_preparation_polls",
            &["substring"],
        ) {
            moving();
        }
    }
    #[test]
    fn collector_string_owned_print_preparation_polls() {
        if child(
            "runtime::string_owned::tests::collector_string_owned_print_preparation_polls",
            &["raw", "line"],
        ) {
            moving();
        }
    }
    #[test]
    fn collector_string_owned_read_path_preparation_polls() {
        if child(
            "runtime::string_owned::tests::collector_string_owned_read_path_preparation_polls",
            &["read"],
        ) {
            moving();
        }
    }
    #[test]
    fn collector_string_owned_host_roots_both_arguments() {
        if child(
            "runtime::string_owned::tests::collector_string_owned_host_roots_both_arguments",
            &["host", "host-text"],
        ) {
            moving();
        }
    }
    #[test]
    fn collector_string_owned_validation_allows_collection() {
        if child(
            "runtime::string_owned::tests::collector_string_owned_validation_allows_collection",
            &["validation"],
        ) {
            moving();
        }
    }
    #[test]
    fn collector_string_owned_range_and_error_semantics() {
        for generational in [false, true] {
            let types = vec![
                TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0),
                TypeInfo::for_header(Full::SIZE)
                    .with_type_id(1)
                    .with_varlen_values(0),
                TypeInfo::for_header(Full::SIZE)
                    .with_type_id(2)
                    .with_fields(2)
                    .with_raw_bytes(16),
            ];
            let mut rt = if generational {
                RuntimeContext::new_generational(32768, 2 << 20, types)
            } else {
                RuntimeContext::new(2 << 20, types)
            };
            let original = text();
            rt.enter(|thread| unsafe {
                let roots = (&*(*thread).dyna_thread).scratch_scope();
                let source = roots.push(alloc_string_from_text(thread, 0, &original));
                for (start, end) in [
                    (i64::MIN, i64::MAX),
                    (1, 1),
                    (0, 2),
                    (2, 5),
                    (5, 9),
                    (9, original.len() as i64 - 9),
                    (i64::MAX, i64::MIN),
                    (32760, 65529),
                    (32768, 65536),
                ] {
                    let lo = (start as i128).max(0).min(original.len() as i128) as usize;
                    let hi = (end as i128).max(lo as i128).min(original.len() as i128) as usize;
                    let expected = original
                        .get(lo..hi)
                        .map(str::to_owned)
                        .ok_or(super::RangeError::Boundary);
                    assert_eq!(
                        super::range(thread, roots.get(source), start, end),
                        expected
                    );
                }
                for len in [1usize, 32768, 32769, 70000] {
                    let mut malformed = vec![b'a'; len];
                    malformed[len / 2] = 0xff;
                    let source = ai_gc_alloc_varlen(thread, 0, len as u64, 0);
                    std::ptr::copy_nonoverlapping(
                        malformed.as_ptr(),
                        source.add(STR_DATA_OFF),
                        len,
                    );
                    let source = roots.push(source);
                    assert!(super::copy(thread, roots.get(source)) == malformed);
                    assert!(super::text(thread, roots.get(source)).is_err());
                    assert_eq!(
                        super::range(thread, roots.get(source), 0, len as i64),
                        Err(super::RangeError::InvalidUtf8)
                    );
                    if len > 1 {
                        assert_eq!(
                            super::range(thread, roots.get(source), 0, 1),
                            Ok("a".into())
                        );
                    }
                    let empty = roots.push(alloc_string_from_text(thread, 0, ""));
                    let response = io::ai_host_call(
                        thread,
                        io::STDOUT,
                        2,
                        0,
                        1,
                        roots.get(source),
                        roots.get(empty),
                        0,
                    );
                    assert_eq!(slots::ai_slot_bits64_load(thread, response, 32), 3);
                }
            });
        }
    }
}
