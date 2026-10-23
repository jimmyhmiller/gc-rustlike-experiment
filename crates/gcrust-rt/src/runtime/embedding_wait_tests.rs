use super::*;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bounded(name: &str) -> bool {
    if std::env::var_os("GCR_EMBED_WAIT_CHILD").is_some() {
        return true;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("runtime::embedding_wait_tests::{name}"),
            "--nocapture",
        ])
        .env("GCR_EMBED_WAIT_CHILD", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    fn read(mut p: impl std::io::Read) -> Vec<u8> {
        let mut data = Vec::new();
        p.read_to_end(&mut data).unwrap();
        data
    }
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || read(out));
    let err = std::thread::spawn(move || read(err));
    let deadline = Instant::now() + Duration::from_secs(8);
    let timeout = loop {
        if child.try_wait().unwrap().is_some() {
            break false;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break true;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let status = child.wait().unwrap();
    let out = out.join().unwrap();
    let err = err.join().unwrap();
    assert!(
        !timeout && status.success(),
        "timeout={timeout}, {status}: {} {}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    false
}
#[test]
fn collector_embedding_idle_context_does_not_block_another_contexts_shutdown() {
    if !bounded("collector_embedding_idle_context_does_not_block_another_contexts_shutdown") {
        return;
    }
    let outer = RuntimeContext::new(8192, vec![]);
    let inner = RuntimeContext::new(8192, vec![]);
    let heap = outer.heap().clone();
    let native = std::thread::spawn(move || {
        unsafe {
            heap.stw_collect::<IdentityPtrPolicy>();
        }
        42
    });
    inner
        .heap()
        .thread_completions
        .lock()
        .unwrap()
        .register(Arc::new(JoinHandle {
            completion_notice: false,
            daemon: std::sync::atomic::AtomicBool::new(false),
            native_joined: std::sync::atomic::AtomicBool::new(false),
            pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
            reaper_registration: std::sync::OnceLock::new(),
            retirement: std::sync::OnceLock::new(),
            state: Mutex::new(JoinState::Running(native)),
            owner: std::sync::atomic::AtomicUsize::new(1),
        }), &inner.heap().native_retirement);
    drop(inner);
    assert_eq!(outer.heap().collections(), 1);
    drop(outer);
}
#[test]
fn collector_embedding_unbound_idle_context_does_not_block_selected_execution() {
    if !bounded("collector_embedding_unbound_idle_context_does_not_block_selected_execution") {
        return;
    }
    let outer = RuntimeContext::new(8192, vec![]);
    let mut inner = RuntimeContext::new(8192, vec![]);
    let thread = inner.thread_ptr();
    unsafe {
        with_current_thread(thread, || {
            let heap = outer.heap().clone();
            std::thread::spawn(move || heap.stw_collect::<IdentityPtrPolicy>())
                .join()
                .unwrap();
        });
    }
    assert_eq!(outer.heap().collections(), 1);
}

#[test]
fn collector_embedding_roots_move_while_context_is_idle_between_entries() {
    if !bounded("collector_embedding_roots_move_while_context_is_idle_between_entries") {
        return;
    }
    let info = TypeInfo::for_header(16).with_raw_bytes(8);
    let mut context = RuntimeContext::new(8192, vec![info]);
    let heap = context.heap().clone();
    assert!(context.thread.registration.dyna.is_blocked());
    let (root, old) = context.enter(|thread| {
        assert!(unsafe { &*(*thread).dyna_thread }.is_running());
        assert_eq!(unsafe { ai_current_thread() }, thread);
        let object = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
        unsafe {
            object.add(16).cast::<u64>().write(42);
            (heap.globals.add(object as u64), object as usize)
        }
    });
    assert!(context.thread.registration.dyna.is_blocked());
    assert!(unsafe { ai_current_thread() }.is_null());
    let target = heap.clone();
    std::thread::spawn(move || unsafe {
        target.concurrent_collect::<IdentityPtrPolicy>();
    })
    .join()
    .unwrap();
    let moved = heap.globals.get(root) as usize;
    assert_ne!(old, moved);
    context.enter(|thread| {
        let object = heap.globals.get(root) as *mut u8;
        assert_eq!(
            unsafe { slots::ai_slot_bits64_load(thread, object, 16) },
            42
        );
        // A pre-entry collection invalidated the constructor's reservation.
        let fresh = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
        assert!(heap.from_space_contains(fresh));
    });
    assert!(context.thread.registration.dyna.is_blocked());
}

#[test]
fn collector_embedding_nested_entries_preserve_moving_roots_and_foreign_scopes() {
    if !bounded("collector_embedding_nested_entries_preserve_moving_roots_and_foreign_scopes") {
        return;
    }
    for foreign in [false, true] {
        let info = TypeInfo::for_header(16).with_raw_bytes(8);
        let mut outer = RuntimeContext::new(8192, vec![info]);
        let mut inner = RuntimeContext::new(8192, vec![]);
        let heap = outer.heap().clone();
        outer.enter(|thread| {
            let state = unsafe { &*(*thread).dyna_thread };
            let roots = unsafe { state.scratch_scope() };
            let object = unsafe { ai_gc_alloc_fixed(thread, 0, 0) };
            unsafe {
                object.add(16).cast::<u64>().write(42);
            }
            let slot = unsafe { roots.push(object) };
            if foreign {
                unsafe {
                    ai_ffi_enter(thread);
                }
            }
            inner.enter(|selected| {
                assert_eq!(unsafe { ai_current_thread() }, selected);
                assert!(state.is_blocked());
                let target = heap.clone();
                std::thread::spawn(move || unsafe {
                    target.stw_collect::<IdentityPtrPolicy>();
                })
                .join()
                .unwrap();
            });
            assert_eq!(unsafe { ai_current_thread() }, thread);
            if foreign {
                assert!(state.is_blocked());
                unsafe {
                    ai_ffi_leave(thread);
                }
            } else {
                assert!(state.is_running());
            }
            assert_ne!(roots.get(slot), object);
            assert_eq!(
                unsafe { slots::ai_slot_bits64_load(thread, roots.get(slot), 16) },
                42
            );
        });
        assert!(
            outer.thread.registration.dyna.is_blocked()
                && inner.thread.registration.dyna.is_blocked()
        );
    }
}

#[test]
fn collector_embedding_entry_unwind_restores_outer_then_parks_both_contexts() {
    if !bounded("collector_embedding_entry_unwind_restores_outer_then_parks_both_contexts") {
        return;
    }
    let mut outer = RuntimeContext::new(8192, vec![]);
    let mut inner = RuntimeContext::new(8192, vec![]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        outer.enter(|thread| {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                inner.enter(|_| panic!("inner entry unwind"))
            }));
            assert!(caught.is_err());
            assert_eq!(unsafe { ai_current_thread() }, thread);
            assert!(unsafe { &*(*thread).dyna_thread }.is_running());
            assert!(inner.thread.registration.dyna.is_blocked());
            panic!("outer entry unwind");
        })
    }));
    assert!(result.is_err());
    assert!(
        outer.thread.registration.dyna.is_blocked() && inner.thread.registration.dyna.is_blocked()
    );
    assert!(unsafe { ai_current_thread() }.is_null());
    let a = outer.heap().clone();
    let b = inner.heap().clone();
    std::thread::spawn(move || unsafe {
        a.stw_collect::<IdentityPtrPolicy>();
        b.stw_collect::<IdentityPtrPolicy>();
    })
    .join()
    .unwrap();
}

#[test]
fn collector_embedding_inner_shutdown_suspends_an_active_outer_entry() {
    if !bounded("collector_embedding_inner_shutdown_suspends_an_active_outer_entry") {
        return;
    }
    let mut outer = RuntimeContext::new(8192, vec![]);
    let heap = outer.heap().clone();
    outer.enter(|thread| {
        let inner = RuntimeContext::new(8192, vec![]);
        let target = heap.clone();
        let native = std::thread::spawn(move || {
            unsafe {
                target.stw_collect::<IdentityPtrPolicy>();
            }
            42
        });
        inner
            .heap()
            .thread_completions
            .lock()
            .unwrap()
            .register(Arc::new(JoinHandle {
                completion_notice: false,
                daemon: std::sync::atomic::AtomicBool::new(false),
                native_joined: std::sync::atomic::AtomicBool::new(false),
                pending_index: std::sync::atomic::AtomicUsize::new(usize::MAX),
                reaper_registration: std::sync::OnceLock::new(),
                retirement: std::sync::OnceLock::new(),
                state: Mutex::new(JoinState::Running(native)),
                owner: std::sync::atomic::AtomicUsize::new(1),
            }), &inner.heap().native_retirement);
        drop(inner);
        assert!(unsafe { &*(*thread).dyna_thread }.is_running());
        assert_eq!(unsafe { ai_current_thread() }, thread);
    });
    assert_eq!(heap.collections(), 1);
    assert!(outer.thread.registration.dyna.is_blocked());
}
