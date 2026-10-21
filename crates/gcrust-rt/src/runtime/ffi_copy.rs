//! Bounded managed-to-native FFI copying.
use super::{STR_DATA_OFF, Thread, managed, string_copy};

// Bulk immutable copies amortize native memcpy setup over a larger fixed piece.
// Atomic scalar loops retain the smaller work bound in string_copy::COPY_BYTES.
const STRING_COPY_BYTES: usize = 256 * 1024;

/// The native destination includes one byte for the terminating NUL.
#[inline]
pub(super) unsafe fn string(thread: *mut Thread, source: *const u8, native: *mut u8) -> i64 {
    let bytes = unsafe { super::str_bytes(source) }.len();
    if bytes <= string_copy::COPY_BYTES {
        return unsafe { super::ai_str_copy_to_buf(thread, source, native) };
    }
    unsafe { string_large(thread, source, native, bytes) }
}

#[inline(never)]
unsafe fn string_large(
    thread: *mut Thread,
    source: *const u8,
    native: *mut u8,
    bytes: usize,
) -> i64 {
    let roots = unsafe { (&*(*thread).dyna_thread).scratch_scope() };
    let owner_root = unsafe { roots.push(source) };
    for offset in (0..bytes).step_by(STRING_COPY_BYTES) {
        #[cfg(test)]
        tests::checkpoint(thread, offset);
        unsafe { string_copy::poll(thread) };
        let take = (bytes - offset).min(STRING_COPY_BYTES);
        // Immutable String data follows the rooted scan/copy access contract.
        // No managed address survives the next poll.
        unsafe {
            std::ptr::copy_nonoverlapping(
                roots.get(owner_root).add(STR_DATA_OFF + offset),
                native.add(offset),
                take,
            );
        }
    }
    unsafe { *native.add(bytes) = 0 };
    bytes as i64
}

/// Each bounded piece preserves the legacy matching-width SC element accesses.
/// Native storage is private, initialized and disjoint from managed storage.
#[inline]
pub(super) unsafe fn scalar(
    thread: *mut Thread,
    owner: *const u8,
    native: *mut u8,
    bytes: i64,
    bits: i64,
    copy_out: bool,
) {
    if bytes <= string_copy::COPY_BYTES as i64 {
        unsafe {
            if copy_out {
                super::ai_buf_copy_out(thread, owner.cast_mut(), native, bytes, bits)
            } else {
                super::ai_buf_copy_in(thread, owner, native, bytes, bits)
            }
        }
        return;
    }
    unsafe { scalar_large(thread, owner, native, bytes, bits, copy_out) }
}

#[inline(never)]
unsafe fn scalar_large(
    thread: *mut Thread,
    owner: *const u8,
    native: *mut u8,
    bytes: i64,
    bits: i64,
    copy_out: bool,
) {
    let stride = match bits {
        1 | 8 => 1,
        16 => 2,
        32 => 4,
        64 => 8,
        _ => panic!("invalid scalar buffer width"),
    };
    assert!(
        bytes >= 0 && bytes % stride == 0,
        "invalid scalar buffer size"
    );
    let bytes = usize::try_from(bytes).expect("scalar buffer exceeds address space");
    let roots = unsafe { (&*(*thread).dyna_thread).scratch_scope() };
    let owner_root = unsafe { roots.push(owner) };
    for offset in (0..bytes).step_by(string_copy::COPY_BYTES) {
        #[cfg(test)]
        tests::checkpoint(thread, offset);
        unsafe { string_copy::poll(thread) };
        let take = (bytes - offset).min(string_copy::COPY_BYTES);
        let access = unsafe { (&*(*thread).heap).object_access(roots.get(owner_root)) };
        unsafe {
            managed::copy_scalar_buffer(
                access.as_ptr().add(STR_DATA_OFF + offset),
                native.add(offset),
                take as i64,
                bits,
                copy_out,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{RuntimeContext, ai_gc_alloc_varlen, blocking_region};
    use super::*;
    use crate::gc::{Full, IdentityPtrPolicy, ObjHeader, TypeInfo};
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    fn child(name: &str) -> bool {
        if std::env::var_os("GCR_FFI_COPY_CHILD").is_some() {
            return true;
        }
        for generational in ["0", "1"] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .env("GCR_FFI_COPY_CHILD", generational)
                .env("GCR_GC_VERIFY", "1")
                .env("GCRUST_POISON", "1")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    panic!(
                        "FFI copy did not make progress: {:?}",
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
    fn runtime(generational: bool) -> RuntimeContext {
        let types = vec![TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0)];
        if generational {
            RuntimeContext::new_generational(32768, 2 << 20, types)
        } else {
            RuntimeContext::new(2 << 20, types)
        }
    }
    static HOOK: std::sync::Mutex<Option<(usize, std::sync::mpsc::Sender<()>)>> =
        std::sync::Mutex::new(None);
    pub(super) fn checkpoint(thread: *mut Thread, offset: usize) {
        if offset != string_copy::COPY_BYTES && offset != STRING_COPY_BYTES {
            return;
        }
        let heap = unsafe { (*thread).heap };
        let sender = {
            let mut hook = HOOK.lock().unwrap();
            if hook
                .as_ref()
                .is_some_and(|(owner, _)| *owner == heap as usize)
            {
                hook.take().map(|(_, sender)| sender)
            } else {
                None
            }
        };
        if let Some(sender) = sender {
            sender.send(()).unwrap();
            while !unsafe { &*heap }.gc_requested() {
                std::thread::yield_now();
            }
        }
    }
    fn progress(mid_copy: bool) {
        let mut rt = runtime(std::env::var("GCR_FFI_COPY_CHILD").as_deref() == Ok("1"));
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            for (bits, direction) in [1, 8, 16, 32, 64].into_iter()
                .flat_map(|bits| [(bits, 0), (bits, 1)])
                .chain([(8, 2)]) {
                let roots = (&*(*thread).dyna_thread).scratch_scope();
                let piece = if direction == 2 { STRING_COPY_BYTES } else { string_copy::COPY_BYTES };
                let requested_bytes = 3 * piece + (bits as usize / 8).max(1);
                let input: Vec<u8> = if direction == 2 {
                    // Seven-byte UTF-8 periods cross the fixed native-copy
                    // boundary inside a code point; bytes must remain exact.
                    "é🦀a".repeat(requested_bytes / 7 + 1).into_bytes()
                } else {
                    (0..requested_bytes).map(|i| ((i * 17 + 3) % 127) as u8).collect()
                };
                let bytes = input.len();
                let object = ai_gc_alloc_varlen(thread, 0, bytes as u64, 0);
                // Read-only copies have no caller root: the helper itself
                // must retain the owner across its collecting pieces.
                let root = if direction == 1 { Some(roots.push(object)) } else { None };
                let expected: Vec<u8> = input.iter().map(|&b| if bits == 1 { b & 1 } else { b }).collect();
                if direction != 1 {
                    super::super::ai_buf_copy_out(thread, object, input.as_ptr(), bytes as i64, bits);
                }
                let before = heap.collections();
                let other = heap.clone();
                let (ready, waiting) = std::sync::mpsc::channel();
                if mid_copy {
                    *HOOK.lock().unwrap() = Some((std::sync::Arc::as_ptr(&heap) as usize, ready));
                } else { ready.send(()).unwrap(); }
                let collector = std::thread::spawn(move || {
                    waiting.recv().unwrap();
                    other.stw_collect::<IdentityPtrPolicy>();
                });
                if !mid_copy { while !heap.gc_requested() { std::thread::yield_now(); } }
                // Offset by one to exercise unaligned native scalar storage, and
                // retain canaries around the entire destination and NUL byte.
                let mut output = vec![0xa5; bytes + 3];
                let native = output.as_mut_ptr().add(1);
                match direction {
                    0 => scalar(thread, object, native, bytes as i64, bits, false),
                    1 => scalar(thread, object, input.as_ptr().cast_mut(), bytes as i64, bits, true),
                    _ => { assert_eq!(string(thread, object, native), bytes as i64); }
                }
                assert!(heap.collections() > before, "FFI copy direction {direction} width {bits} did not service requested collection");
                blocking_region(thread, || collector.join().unwrap());
                if direction == 1 {
                    super::super::ai_buf_copy_in(thread, roots.get(root.unwrap()), native, bytes as i64, bits);
                }
                assert!(output[1..bytes + 1] == expected, "copy mismatch: width {bits}, bytes {bytes}, first differing byte {:?}", output[1..bytes + 1].iter().zip(&expected).position(|(a,b)| a != b));
                assert_eq!(output[0], 0xa5);
                assert_eq!(output[bytes + 1], if direction == 2 { 0 } else { 0xa5 });
                assert_eq!(output[bytes + 2], 0xa5);
            }
            assert_eq!((&*(*thread).dyna_thread).scratch_mark(), 0);
        });
    }
    #[test]
    fn collector_ffi_copy_services_requested_collection() {
        if child("runtime::ffi_copy::tests::collector_ffi_copy_services_requested_collection") {
            progress(false);
        }
    }
    #[test]
    fn collector_ffi_copy_reloads_between_chunks() {
        if child("runtime::ffi_copy::tests::collector_ffi_copy_reloads_between_chunks") {
            progress(true);
        }
    }
    #[test]
    fn collector_ffi_copy_reference_boundaries() {
        for generational in [false, true] {
            let mut rt = runtime(generational);
            rt.enter(|thread| unsafe {
                for bits in [1, 8, 16, 32, 64] {
                    let stride = (bits as usize / 8).max(1);
                    for bytes in [
                        0,
                        stride,
                        string_copy::COPY_BYTES - stride,
                        string_copy::COPY_BYTES,
                        string_copy::COPY_BYTES + stride,
                        3 * string_copy::COPY_BYTES + stride,
                    ]
                    .into_iter()
                    .chain(if bits == 8 {
                        vec![
                            STRING_COPY_BYTES - 1,
                            STRING_COPY_BYTES,
                            STRING_COPY_BYTES + 1,
                            3 * STRING_COPY_BYTES + 1,
                        ]
                    } else {
                        vec![]
                    }) {
                        let roots = (&*(*thread).dyna_thread).scratch_scope();
                        let owner = roots.push(ai_gc_alloc_varlen(thread, 0, bytes as u64, 0));
                        let input: Vec<u8> = (0..bytes + 1)
                            .map(|i| {
                                if bits == 8 {
                                    ((i * 17 + 3) % 127) as u8
                                } else {
                                    ((i * 17 + 3) % 251) as u8
                                }
                            })
                            .collect();
                        let expected: Vec<u8> = input[1..]
                            .iter()
                            .map(|&b| if bits == 1 { b & 1 } else { b })
                            .collect();
                        let mut output = vec![0xa5; bytes + 2];
                        scalar(
                            thread,
                            roots.get(owner),
                            input.as_ptr().add(1).cast_mut(),
                            bytes as i64,
                            bits,
                            true,
                        );
                        scalar(
                            thread,
                            roots.get(owner),
                            output.as_mut_ptr().add(1),
                            bytes as i64,
                            bits,
                            false,
                        );
                        assert!(
                            output[1..bytes + 1] == expected,
                            "copy mismatch: width {bits}, bytes {bytes}, first differing byte {:?}",
                            output[1..bytes + 1]
                                .iter()
                                .zip(&expected)
                                .position(|(a, b)| a != b)
                        );
                        assert_eq!(output[0], 0xa5);
                        assert_eq!(output[bytes + 1], 0xa5);
                        if bits == 8 {
                            let mut text = vec![0xa5; bytes + 3];
                            assert_eq!(
                                string(thread, roots.get(owner), text.as_mut_ptr().add(1)),
                                bytes as i64
                            );
                            assert!(
                                text[1..bytes + 1] == expected,
                                "String copy mismatch at length {bytes}"
                            );
                            assert_eq!(text[bytes + 1], 0);
                            assert_eq!(text[0], 0xa5);
                            assert_eq!(text[bytes + 2], 0xa5);
                        }
                    }
                }
                assert_eq!((&*(*thread).dyna_thread).scratch_mark(), 0);
            });
        }
    }
    #[test]
    fn collector_ffi_copy_legacy_and_small_remain_noncollecting() {
        if !child(
            "runtime::ffi_copy::tests::collector_ffi_copy_legacy_and_small_remain_noncollecting",
        ) {
            return;
        }
        let mut rt = runtime(std::env::var("GCR_FFI_COPY_CHILD").as_deref() == Ok("1"));
        let heap = rt.heap().clone();
        rt.enter(|thread| unsafe {
            {
                let roots = (&*(*thread).dyna_thread).scratch_scope();
                let bytes = 3 * string_copy::COPY_BYTES;
                let large = roots.push(ai_gc_alloc_varlen(thread, 0, bytes as u64, 0));
                let small = roots.push(ai_gc_alloc_varlen(
                    thread,
                    0,
                    string_copy::COPY_BYTES as u64,
                    0,
                ));
                let mut native = vec![0; bytes + 1];
                let before = heap.collections();
                let other = heap.clone();
                let collector =
                    std::thread::spawn(move || other.stw_collect::<IdentityPtrPolicy>());
                while !heap.gc_requested() {
                    std::thread::yield_now();
                }
                super::super::ai_str_copy_to_buf(thread, roots.get(large), native.as_mut_ptr());
                super::super::ai_buf_copy_in(
                    thread,
                    roots.get(large),
                    native.as_mut_ptr(),
                    bytes as i64,
                    8,
                );
                super::super::ai_buf_copy_out(
                    thread,
                    roots.get(large),
                    native.as_ptr(),
                    bytes as i64,
                    8,
                );
                string(thread, roots.get(small), native.as_mut_ptr());
                scalar(
                    thread,
                    roots.get(small),
                    native.as_mut_ptr(),
                    string_copy::COPY_BYTES as i64,
                    8,
                    false,
                );
                scalar(
                    thread,
                    roots.get(small),
                    native.as_mut_ptr(),
                    string_copy::COPY_BYTES as i64,
                    8,
                    true,
                );
                assert_eq!(
                    heap.collections(),
                    before,
                    "legacy or bounded leaf copy collected"
                );
                string_copy::poll(thread);
                blocking_region(thread, || collector.join().unwrap());
                assert!(heap.collections() > before);
            }
            assert_eq!((&*(*thread).dyna_thread).scratch_mark(), 0);
        });
    }
}
