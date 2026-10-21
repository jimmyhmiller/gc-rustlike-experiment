use super::*;
use crate::gc::{MutatorThread, ObjHeader};
use std::{sync::mpsc, time::{Duration, Instant}};

fn wait_case(name: &str, send: bool) {
    const CHILD: &str = "GCR_CHANNEL_SIBLING_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"]).env(CHILD, "1")
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("channel sibling wait timed out: {:?}", child.wait_with_output().unwrap());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("test {name} ... ok")), "{output:?}");
        return;
    }
    let (ready, states) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let types = vec![
            TypeInfo::for_header(Full::SIZE).with_type_id(0).with_varlen_values(0),
            TypeInfo::for_header(Full::SIZE).with_type_id(1).with_raw_bytes(8),
        ];
        let mut runtime = RuntimeContext::new(4096, types);
        let heap = runtime.heap().clone();
        runtime.enter(|thread| unsafe {
            let sibling = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
            let blocked = MutatorThread::<IdentityPtrPolicy>::register(heap.clone());
            blocked.state().enter_blocked();
            let caller = &*(*thread).dyna_thread;
            let roots = crate::gc::roots::RootScope::new(caller.frame_chain(), 2);
            let witness = roots.root::<u8>(ai_gc_alloc_varlen(thread, 0, 0, 0) as u64);
            let message = ai_gc_alloc_fixed(thread, 1, 0);
            message.add(Full::SIZE).cast::<u64>().write(42);
            let message = roots.root::<u8>(message as u64);
            let handle = ai_chan_new(thread, 1, witness.get() as *const u8, 1);
            if send {
                assert_eq!(ai_chan_send(thread, witness.get() as *mut u8, handle, message.get() as *mut u8), 0);
            }
            let control = heap.channels.lock().unwrap().records[&(handle as usize)].0.clone();
            ready.send((heap.clone(), control, caller as *const ThreadState as usize,
                crate::gc::ThreadStateHandle::as_ptr(sibling.state()) as usize, crate::gc::ThreadStateHandle::as_ptr(blocked.state()) as usize)).unwrap();
            if send {
                assert_eq!(ai_chan_send(thread, witness.get() as *mut u8, handle, message.get() as *mut u8), 1);
            } else {
                assert!(ai_chan_recv(thread, witness.get() as *mut u8, handle).is_null());
            }
            assert!(caller.is_running() && sibling.state().is_running());
            assert!(blocked.state().is_blocked());
            let moved = message.get() as *const u8;
            assert!(heap.from_space_contains(moved));
            assert_eq!(moved.add(Full::SIZE).cast::<u64>().read(), 42);
            blocked.state().exit_blocked(&heap);
        });
    });
    let (heap, control, caller, sibling, blocked) = states.recv_timeout(Duration::from_secs(5)).unwrap();
    // Only synchronized native/atomic observations cross OS threads; the owner
    // remains alive until the channel is released and the worker is joined.
    let (caller, sibling, blocked) = unsafe {
        (&*(caller as *const ThreadState), &*(sibling as *const ThreadState), &*(blocked as *const ThreadState))
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut state = loop {
        let state = control.state.lock().unwrap();
        let waiting = if send { state.waiting_senders } else { state.waiting_receivers };
        if waiting != 0 { break state; }
        drop(state);
        assert!(Instant::now() < deadline, "channel did not enter its condition-variable wait");
        std::thread::yield_now();
    };
    // The wait counter plus state mutex identifies the real condvar wait,
    // rather than the preceding brief mutex-acquisition blocking region.
    let all_parked = caller.is_blocked() && sibling.is_blocked() && blocked.is_blocked();
    if all_parked {
        // Collector queue scanning intentionally does not acquire state.
        // Original code skips this collection so it can release/join cleanly.
        unsafe { heap.stw_collect::<IdentityPtrPolicy>(); }
    }
    state.closed = true;
    control.not_empty.notify_all();
    control.not_full.notify_all();
    drop(state);
    worker.join().unwrap();
    assert!(all_parked, "channel condition-variable wait left an embedding sibling RUNNING");
}

#[test]
fn collector_channel_full_send_parks_embedding_siblings() {
    wait_case("runtime::channel_sibling_tests::collector_channel_full_send_parks_embedding_siblings", true);
}

#[test]
fn collector_channel_empty_recv_parks_embedding_siblings() {
    wait_case("runtime::channel_sibling_tests::collector_channel_empty_recv_parks_embedding_siblings", false);
}
