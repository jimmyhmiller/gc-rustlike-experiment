//! Native float parsing ownership and collector progress.

use super::{STRING_SCAN_LEAF_BYTES, Thread, blocking_region, str_bytes, string_owned};

/// Native bytes only: malformed text keeps the legacy positive-zero fallback.
#[inline]
fn parse_bytes(bytes: &[u8]) -> f64 {
    match core::str::from_utf8(bytes) {
        Ok(text) => text.trim().parse::<f64>().unwrap_or(0.0),
        Err(_) => 0.0,
    }
}

/// Requires this OS thread's RUNNING mutator and an immutable String argument.
/// Small inputs keep the legacy leaf path. Large copies pin/reload their source;
/// validation, Unicode trimming and parsing then run BLOCKED on native bytes.
pub(super) unsafe fn parse(thread: *mut Thread, source: *const u8) -> f64 {
    if unsafe { str_bytes(source) }.len() <= STRING_SCAN_LEAF_BYTES {
        return unsafe { super::ai_str_to_float(thread, source) };
    }
    let bytes = unsafe { string_owned::copy(thread, source) };
    #[cfg(test)]
    let heap = unsafe { (*thread).heap };
    unsafe {
        blocking_region(thread, || {
            #[cfg(test)]
            tests::parsing_checkpoint(heap);
            parse_bytes(&bytes)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo};
    use std::sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicPtr, Ordering},
    };
    static HEAP: AtomicPtr<crate::gc::Heap> = AtomicPtr::new(std::ptr::null_mut());
    static READY: OnceLock<std::sync::mpsc::Sender<()>> = OnceLock::new();
    static COLLECTED: AtomicBool = AtomicBool::new(false);
    pub(super) fn parsing_checkpoint(heap: *const crate::gc::Heap) {
        if !heap.is_null() && heap == HEAP.load(Ordering::Acquire).cast_const() {
            READY.get().unwrap().send(()).unwrap();
            while !COLLECTED.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
    }
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    fn child(name: &str) -> bool {
        if std::env::var_os("GCR_FLOAT_PARSE_CHILD").is_some() {
            return true;
        }
        for generational in ["0", "1"] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .env("GCR_FLOAT_PARSE_CHILD", generational)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    panic!(
                        "float parse failed to make progress: {:?}",
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
    fn runtime() -> RuntimeContext {
        let types = vec![TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0)];
        if std::env::var("GCR_FLOAT_PARSE_CHILD").as_deref() == Ok("1") {
            RuntimeContext::new_generational(32768, 2 << 20, types)
        } else {
            RuntimeContext::new(2 << 20, types)
        }
    }
    #[test]
    fn collector_float_parse_services_requested_collection() {
        if !child(
            "runtime::float_parse::tests::collector_float_parse_services_requested_collection",
        ) {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        let text = format!("{}3.25{}", "\u{2003}".repeat(12000), " ".repeat(40000));
        rt.enter(|thread| unsafe {
            let source = alloc_string_from_text(thread, 0, &text);
            let before = heap.collections();
            let other = heap.clone();
            let collector = std::thread::spawn(move || other.stw_collect::<IdentityPtrPolicy>());
            while !heap.gc_requested() {
                std::thread::yield_now();
            }
            let value = ai_str_to_float_v2(thread, source);
            assert!(
                heap.collections() > before,
                "float parser did not service requested collection"
            );
            assert_eq!(value.to_bits(), 3.25f64.to_bits());
            blocking_region(thread, || collector.join().unwrap());
            assert_eq!((&*(*thread).dyna_thread).scratch_mark(), 0);
        });
    }
    #[test]
    fn collector_float_parse_native_phase_allows_collection() {
        if !child(
            "runtime::float_parse::tests::collector_float_parse_native_phase_allows_collection",
        ) {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        let text = format!("{}3.25{}", "\u{2003}".repeat(12000), " ".repeat(40000));
        rt.enter(|thread| unsafe {
            let source = alloc_string_from_text(thread, 0, &text);
            let before = heap.collections();
            let (ready, waiting) = std::sync::mpsc::channel();
            READY.set(ready).unwrap();
            HEAP.store(std::sync::Arc::as_ptr(&heap).cast_mut(), Ordering::Release);
            let other = heap.clone();
            let collector = std::thread::spawn(move || {
                waiting.recv().unwrap();
                other.stw_collect::<IdentityPtrPolicy>();
                COLLECTED.store(true, Ordering::Release);
            });
            let value = ai_str_to_float_v2(thread, source);
            assert_eq!(value.to_bits(), 3.25f64.to_bits());
            assert!(COLLECTED.load(Ordering::Acquire) && heap.collections() > before);
            blocking_region(thread, || collector.join().unwrap());
            HEAP.store(std::ptr::null_mut(), Ordering::Release);
            assert_eq!((&*(*thread).dyna_thread).scratch_mark(), 0);
        });
    }
    #[test]
    fn collector_float_parse_legacy_and_small_v2_remain_noncollecting() {
        if !child(
            "runtime::float_parse::tests::collector_float_parse_legacy_and_small_v2_remain_noncollecting",
        ) {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            let roots = (&*(*thread).dyna_thread).scratch_scope();
            let text = format!("{}3.25", " ".repeat(70000));
            let large = roots.push(alloc_string_from_text(thread, 0, &text));
            let small = roots.push(alloc_string_from_text(thread, 0, "3.25"));
            let before = heap.collections();
            let other = heap.clone();
            let collector = std::thread::spawn(move || other.stw_collect::<IdentityPtrPolicy>());
            while !heap.gc_requested() {
                std::thread::yield_now();
            }
            assert_eq!(
                ai_str_to_float(thread, roots.get(large)).to_bits(),
                3.25f64.to_bits()
            );
            assert_eq!(
                ai_str_to_float_v2(thread, roots.get(small)).to_bits(),
                3.25f64.to_bits()
            );
            assert_eq!(
                heap.collections(),
                before,
                "leaf parse unexpectedly collected"
            );
            string_copy::poll(thread);
            blocking_region(thread, || collector.join().unwrap());
            assert!(heap.collections() > before);
        });
    }
    #[test]
    fn collector_float_parse_reference_semantics_and_boundaries() {
        for generational in [false, true] {
            let types = vec![TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0)];
            let mut rt = if generational {
                RuntimeContext::new_generational(32768, 2 << 20, types)
            } else {
                RuntimeContext::new(2 << 20, types)
            };
            rt.enter(|thread| unsafe {
                for (text, expected) in [
                    ("", 0.0),
                    ("3.25", 3.25),
                    ("-0.0", -0.0),
                    ("  +1.5\t", 1.5),
                    ("\u{2003}-0\u{2003}", -0.0),
                    ("1,25", 0.0),
                    ("1\0", 0.0),
                    ("NaN", f64::NAN),
                    ("-NaN", f64::NAN),
                    ("inf", f64::INFINITY),
                    ("-Infinity", f64::NEG_INFINITY),
                    ("1e99999", f64::INFINITY),
                    ("-1e99999", f64::NEG_INFINITY),
                    ("5e-324", f64::from_bits(1)),
                    ("1.7976931348623157e308", f64::MAX),
                    ("-1e-99999", -0.0),
                ] {
                    for padding in [0, 32767, 32768, 32769, 70000] {
                        let padded = format!(
                            "{}{}{}",
                            " ".repeat(padding),
                            text,
                            "\u{2003}".repeat(padding / 3)
                        );
                        // Empty text padded with whitespace keeps the zero fallback.
                        let roots = (&*(*thread).dyna_thread).scratch_scope();
                        let source = roots.push(alloc_string_from_text(thread, 0, &padded));
                        let old = ai_str_to_float(thread, roots.get(source));
                        let new = ai_str_to_float_v2(thread, roots.get(source));
                        assert_eq!(new.to_bits(), old.to_bits(), "{text:?}, padding={padding}");
                        if expected.is_nan() {
                            assert!(new.is_nan());
                        } else {
                            assert_eq!(new.to_bits(), expected.to_bits(), "{text:?}");
                        }
                        drop(roots);
                    }
                }
                for len in [32767usize, 32768, 32769] {
                    let text = " ".repeat(len - 4) + "3.25";
                    assert_eq!(text.len(), len);
                    let roots = (&*(*thread).dyna_thread).scratch_scope();
                    let source = roots.push(alloc_string_from_text(thread, 0, &text));
                    assert_eq!(
                        ai_str_to_float_v2(thread, roots.get(source)).to_bits(),
                        3.25f64.to_bits()
                    );
                }
                for len in [1usize, 32768, 32769, 70000] {
                    let mut bytes = vec![b' '; len];
                    bytes[len - 1] = 0xff;
                    let source = ai_gc_alloc_varlen(thread, 0, len as u64, 0);
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), source.add(STR_DATA_OFF), len);
                    let roots = (&*(*thread).dyna_thread).scratch_scope();
                    let source = roots.push(source);
                    assert_eq!(ai_str_to_float(thread, roots.get(source)).to_bits(), 0);
                    assert_eq!(ai_str_to_float_v2(thread, roots.get(source)).to_bits(), 0);
                }
            });
        }
    }
}
