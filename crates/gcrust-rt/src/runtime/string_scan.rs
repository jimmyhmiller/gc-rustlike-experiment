//! Bounded-work scans over immutable managed String bytes.

use super::{
    STR_DATA_OFF, Thread, str_bytes,
    string_copy::{COPY_BYTES, poll},
};

fn hash_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    hash
}

/// The owning RUNNING mutator and valid immutable String arguments are required.
/// Small scans return without parking. Large scans root before the first poll;
/// no managed slice survives a poll or escapes a chunk.
pub(super) unsafe fn hash(thread: *mut Thread, source: *const u8) -> i64 {
    unsafe {
        let len = str_bytes(source).len();
        let mut value = 0xcbf29ce484222325;
        if len <= COPY_BYTES {
            value = hash_bytes(value, str_bytes(source));
        } else {
            let roots = (&*(*thread).dyna_thread).scratch_scope();
            let source_root = roots.push(source);
            for offset in (0..len).step_by(COPY_BYTES) {
                #[cfg(test)]
                if offset == COPY_BYTES {
                    tests::checkpoint(thread);
                }
                poll(thread);
                let bytes = std::slice::from_raw_parts(
                    roots.get(source_root).add(STR_DATA_OFF + offset),
                    (len - offset).min(COPY_BYTES),
                );
                value = hash_bytes(value, bytes);
            }
        }
        (value & 0x7fff_ffff_ffff_ffff) as i64
    }
}

/// As hash, with two independently rooted inputs. Unequal lengths and identity
/// are constant-work leaf paths; a mismatch stops at the first unequal chunk.
pub(super) unsafe fn equal(thread: *mut Thread, a: *const u8, b: *const u8) -> i64 {
    unsafe {
        let len = str_bytes(a).len();
        if len != str_bytes(b).len() {
            return 0;
        }
        if a == b {
            return 1;
        }
        if len <= COPY_BYTES {
            return (str_bytes(a) == str_bytes(b)) as i64;
        }
        let roots = (&*(*thread).dyna_thread).scratch_scope();
        let a_root = roots.push(a);
        let b_root = roots.push(b);
        for offset in (0..len).step_by(COPY_BYTES) {
            #[cfg(test)]
            if offset == COPY_BYTES {
                tests::checkpoint(thread);
            }
            poll(thread);
            let end = len.min(offset + COPY_BYTES);
            if std::slice::from_raw_parts(
                roots.get(a_root).add(STR_DATA_OFF + offset),
                end - offset,
            ) != std::slice::from_raw_parts(
                roots.get(b_root).add(STR_DATA_OFF + offset),
                end - offset,
            ) {
                return 0;
            }
        }
        1
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo};
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    static READY: std::sync::OnceLock<std::sync::mpsc::Sender<()>> = std::sync::OnceLock::new();
    static HEAP: std::sync::atomic::AtomicPtr<crate::gc::Heap> =
        std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
    pub(super) unsafe fn checkpoint(thread: *mut Thread) {
        let heap = HEAP.load(std::sync::atomic::Ordering::Acquire);
        if !heap.is_null() && heap == unsafe { (*thread).heap } {
            READY.get().unwrap().send(()).unwrap();
            while !unsafe { &*heap }.gc_requested() {
                std::thread::yield_now();
            }
        }
    }

    fn child(name: &str) -> bool {
        if std::env::var_os("GCR_STRING_SCAN_CHILD").is_some() {
            return true;
        }
        for (generational, late) in [("0", "0"), ("1", "0"), ("0", "1"), ("1", "1")] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name])
                .env("GCR_STRING_SCAN_CHILD", generational)
                .env("GCR_STRING_SCAN_LATE", late)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    panic!(
                        "string scan did not finish: {:?}",
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
        if std::env::var("GCR_STRING_SCAN_CHILD").as_deref() == Ok("1") {
            RuntimeContext::new_generational(32768, 2 << 20, types)
        } else {
            RuntimeContext::new(2 << 20, types)
        }
    }
    fn reference_hash(bytes: &[u8]) -> i64 {
        let mut value = 14695981039346656037u128;
        for byte in bytes {
            value = ((value ^ *byte as u128) * 1099511628211) % (1u128 << 64);
        }
        (value & ((1u128 << 63) - 1)) as i64
    }
    fn moving_scan(hash: bool, mismatch: bool) {
        let mut rt = runtime();
        let heap = rt.heap().clone();
        let text = "λ中🦀\0".repeat(12000);
        rt.enter(|thread| unsafe {
            let roots = (&*(*thread).dyna_thread).scratch_scope();
            let a = roots.push(alloc_string_from_text(thread, 0, &text));
            let mut second = text.clone();
            if mismatch {
                second.pop();
                second.push('!');
            }
            let b = roots.push(alloc_string_from_text(thread, 0, &second));
            let a = roots.get(a);
            let b = roots.get(b);
            drop(roots);
            // No global/caller root saves these arguments: the callee must pin
            // them before its first poll and reload after relocation.
            let before = heap.collections();
            let other = heap.clone();
            let late = std::env::var("GCR_STRING_SCAN_LATE").as_deref() == Ok("1");
            let (ready, waiting) = std::sync::mpsc::channel();
            if late {
                READY.set(ready).unwrap();
                HEAP.store(
                    std::sync::Arc::as_ptr(&heap).cast_mut(),
                    std::sync::atomic::Ordering::Release,
                );
            }
            let collector = std::thread::spawn(move || {
                if late {
                    waiting.recv().unwrap();
                }
                other.stw_collect::<IdentityPtrPolicy>()
            });
            if !late {
                while !heap.gc_requested() {
                    std::thread::yield_now();
                }
            }
            let result = if hash {
                ai_str_hash_v2(thread, a)
            } else {
                ai_str_eq_v2(thread, a, b)
            };
            assert!(
                heap.collections() > before,
                "string scan did not service requested collection"
            );
            assert_eq!(
                result,
                if hash {
                    reference_hash(text.as_bytes())
                } else {
                    (!mismatch) as i64
                }
            );
            blocking_region(thread, || collector.join().unwrap());
            HEAP.store(std::ptr::null_mut(), std::sync::atomic::Ordering::Release);
            assert_eq!((&*(*thread).dyna_thread).scratch_mark(), 0);
        });
    }
    #[test]
    fn collector_string_scan_hash_services_collection_and_reloads() {
        if child(
            "runtime::string_scan::tests::collector_string_scan_hash_services_collection_and_reloads",
        ) {
            moving_scan(true, false);
        }
    }
    #[test]
    fn collector_string_scan_equality_services_collection_and_reloads() {
        if child(
            "runtime::string_scan::tests::collector_string_scan_equality_services_collection_and_reloads",
        ) {
            moving_scan(false, false);
        }
    }
    #[test]
    fn collector_string_scan_late_mismatch_survives_collection() {
        if child(
            "runtime::string_scan::tests::collector_string_scan_late_mismatch_survives_collection",
        ) {
            moving_scan(false, true);
        }
    }
    #[test]
    fn collector_string_scan_legacy_symbols_remain_noncollecting() {
        if !child(
            "runtime::string_scan::tests::collector_string_scan_legacy_symbols_remain_noncollecting",
        ) {
            return;
        }
        let mut rt = runtime();
        let heap = rt.heap().clone();
        let text = "λ".repeat(40000);
        rt.enter(|thread| unsafe {
            let roots = (&*(*thread).dyna_thread).scratch_scope();
            let a = roots.push(alloc_string_from_text(thread, 0, &text));
            let b = roots.push(alloc_string_from_text(thread, 0, &text));
            let before = heap.collections();
            let other = heap.clone();
            let collector = std::thread::spawn(move || other.stw_collect::<IdentityPtrPolicy>());
            while !heap.gc_requested() {
                std::thread::yield_now();
            }
            assert_eq!(
                ai_str_hash(thread, roots.get(a)),
                reference_hash(text.as_bytes())
            );
            assert_eq!(ai_str_eq(thread, roots.get(a), roots.get(b)), 1);
            assert_eq!(
                heap.collections(),
                before,
                "legacy ABI must not relocate caller roots"
            );
            string_copy::poll(thread);
            blocking_region(thread, || collector.join().unwrap());
            assert!(heap.collections() > before);
            assert!(str_bytes(roots.get(a)) == text.as_bytes());
        });
    }
    #[test]
    fn collector_string_scan_matches_reference_across_chunk_edges() {
        let mut rt = runtime();
        rt.enter(|thread| unsafe {
            for len in [0usize, 1, 32767, 32768, 32769, 65536, 65537] {
                let original: String = (0..len)
                    .map(|i| (32 + (i * 17 % 95)) as u8 as char)
                    .collect();
                let roots = (&*(*thread).dyna_thread).scratch_scope();
                let a = roots.push(alloc_string_from_text(thread, 0, &original));
                assert_eq!(
                    ai_str_hash_v2(thread, roots.get(a)),
                    reference_hash(original.as_bytes())
                );
                assert_eq!(ai_str_eq_v2(thread, roots.get(a), roots.get(a)), 1);
                for position in [0, len / 2, len.saturating_sub(1)] {
                    let mut changed = original.as_bytes().to_vec();
                    if len > 0 {
                        changed[position] ^= 1;
                    }
                    let b = roots.push(alloc_string_from_text(
                        thread,
                        0,
                        std::str::from_utf8(&changed).unwrap(),
                    ));
                    assert_eq!(
                        ai_str_eq_v2(thread, roots.get(a), roots.get(b)),
                        (len == 0) as i64
                    );
                }
                let b = roots.push(alloc_string_from_text(thread, 0, &(original.clone() + "x")));
                assert_eq!(ai_str_eq_v2(thread, roots.get(a), roots.get(b)), 0);
            }
        });
    }
}
