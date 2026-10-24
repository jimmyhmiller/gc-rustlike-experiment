use gcrust::codegen::{emit_llvm_ir, jit_run_i64_gc};
use gcrust::compile::parse_with_prelude;
use gcrust::core::CoreProgram;
use gcrust::lower::lower_program;
use gcrust::resolve::resolve_module;

fn program(src: &str) -> CoreProgram {
    let (module, _) = parse_with_prelude(src).unwrap();
    let resolved = resolve_module(module).unwrap();
    lower_program(&resolved.globals).unwrap()
}

#[test]
fn scalar_allocation_operands_do_not_leave_stale_heap_addresses() {
    let prog = program(
        r#"
        struct Cell { n: i64 }
        fn allocate(n: i64) -> i64 { let c = Cell { n: n }; c.n }
        fn read(c: Cell, n: i64) -> i64 { c.n + n }
        fn main() -> i64 {
            let mut c = Cell { n: 0 };
            c.n = allocate(42);
            let mut a: Array<i64> = Array::new(2);
            a[allocate(1)] = allocate(7);
            let x = a[allocate(1)];
            read(c, allocate(3)) + x
        }
    "#,
    );
    assert_eq!(unsafe { jit_run_i64_gc(&prog, true) }.unwrap(), 52);
}

#[test]
fn allocating_scalar_arguments_preserve_left_to_right_reads() {
    let prog = program(
        r#"
        struct Cell { n: i64 }
        fn change(mut c: Cell) -> i64 { c.n = 9; 1 }
        fn combine(a: i64, b: i64) -> i64 { a * 10 + b }
        fn main() -> i64 {
            let c = Cell { n: 3 };
            combine(c.n, change(c))
        }
    "#,
    );
    assert_eq!(unsafe { jit_run_i64_gc(&prog, true) }.unwrap(), 31);
}

#[test]
fn scalar_normalization_preserves_short_circuiting() {
    let prog = program(
        r#"
        struct Counter { n: i64 }
        fn bump(mut c: Counter) -> bool { c.n = c.n + 1; true }
        fn main() -> i64 {
            let c = Counter { n: 0 };
            let a = false && bump(c);
            let b = true || bump(c);
            let d = true && bump(c);
            let e = false || bump(c);
            c.n
        }
    "#,
    );
    assert_eq!(unsafe { jit_run_i64_gc(&prog, true) }.unwrap(), 2);
}

#[test]
fn fixed_match_temporaries_allocate_once_per_function_call() {
    let prog = program(
        r#"
        enum HeapChoice { No, Yes(i64) }
        fn main() -> i64 {
            let mut i = 0;
            let mut total = 0;
            while i < 200000 {
                let v = Option::Some(i);
                total = total + match v { Option::Some(x) => x, Option::None => 0 };
                i = i + 1;
            }
            let h = HeapChoice::Yes(1);
            total + match h { HeapChoice::No => 0, HeapChoice::Yes(x) => x }
        }
    "#,
    );
    let ir = emit_llvm_ir(&prog, false).unwrap();
    let mut block = "";
    for line in ir.lines() {
        let line = line.trim();
        if line.starts_with("define ") {
            block = "";
        }
        if !line.starts_with(';') && line.contains(':') && !line.starts_with('%') {
            block = line.split(':').next().unwrap();
        }
        if line.contains(" = alloca ") {
            assert_eq!(
                block, "entry",
                "repeated stack allocation in {block}: {line}"
            );
        }
    }
    assert_eq!(unsafe { jit_run_i64_gc(&prog, true) }.unwrap(), 19_999_900_001);
}

#[test]
fn foreign_buffers_preserve_argument_evaluation_order() {
    let prog = program(
        r#"
        extern "C" fn memcmp(a: RawPtr, b: RawPtr, n: i64) -> i32;
        fn change(mut a: Array<i64>) -> i64 { a[0] = 1; 8 }
        fn main() -> i64 {
            let a: Array<i64> = Array::new(1);
            let b: Array<i64> = Array::new(1);
            (unsafe { memcmp(as_c_bytes(a), as_c_bytes(b), change(a)) }) as i64
        }
    "#,
    );
    assert_eq!(unsafe { jit_run_i64_gc(&prog, true) }.unwrap(), 0);
}

#[test]
fn native_callback_reloads_relocated_copy_out_destination() {
    let prog = program(
        r#"
        struct Allocate { n: i64 }
        extern "C" fn qsort(mut base: RawPtr, n: i64, size: i64, compar: extern fn(RawPtr, RawPtr) -> i32);
        fn cmp(a: RawPtr, b: RawPtr) -> i32 {
            let c = Allocate { n: 1 };
            let x = (unsafe { ptr_read_i64(a) }); let y = (unsafe { ptr_read_i64(b) });
            if x < y { (0 - c.n) as i32 } else if x > y { c.n as i32 } else { 0i32 }
        }
        fn main() -> i64 {
            let mut a: Array<i64> = Array::new(3);
            a[0] = 30; a[1] = 10; a[2] = 20;
            (unsafe { qsort(as_c_bytes(a), 3, 8, cmp) });
            a[0] * 100 + a[2]
        }
    "#,
    );
    assert_eq!(unsafe { jit_run_i64_gc(&prog, true) }.unwrap(), 1030);
}

#[test]
fn native_buffers_release_stack_storage_in_loops() {
    let prog = program(
        r#"
        extern "C" fn strlen(s: RawPtr) -> i64;
        fn main() -> i64 {
            let s = "abcdefghijklmnopqrstuvwxyz";
            let mut i = 0;
            let mut total = 0;
            while i < 200000 {
                total = total + (unsafe { strlen(as_c_bytes(s)) });
                i = i + 1;
            }
            total
        }
    "#,
    );
    assert_eq!(unsafe { jit_run_i64_gc(&prog, true) }.unwrap(), 5200000);
}

#[test]
fn blocking_native_call_allows_other_mutators_to_collect() {
    let prog = program(
        r#"
        struct Cell { n: i64 }
        extern "C" fn usleep(microseconds: u32) -> i32;
        fn work() -> i64 {
            let mut i = 0;
            let mut total = 0;
            while i < 1000 { let c = Cell { n: i }; total = total + c.n; i = i + 1; }
            total
        }
        fn main() -> i64 {
            let t = Thread::spawn(|| work());
            (unsafe { usleep(100000u32) });
            t.join()
        }
    "#,
    );
    assert_eq!(unsafe { jit_run_i64_gc(&prog, true) }.unwrap(), 499500);
}

#[test]
fn later_operands_do_not_change_earlier_local_values() {
    let prog = program(
        r#"
        struct Cell { n: i64 }
        fn combine(a: i64, b: i64) -> i64 { a * 10 + b }
        fn read(c: Cell, n: i64) -> i64 { c.n + n }
        fn main() -> i64 {
            let mut x = 3;
            let a = combine(x, { x = 9; 1 });
            let mut c = Cell { n: 5 };
            let b = read(c, { c = Cell { n: 20 }; 1 });
            a + b
        }
    "#,
    );
    assert_eq!(unsafe { jit_run_i64_gc(&prog, true) }.unwrap(), 37);
}

#[test]
fn foreign_copy_helpers_reload_caller_roots() {
    let prog = program(r#"
        extern "C" fn strlen(text: RawPtr) -> i64;
        extern "C" fn memset(mut buf: RawPtr, c: i32, n: i64) -> RawPtr;
        fn main() -> i64 {
            let keep = "caller root";
            let text = "argument";
            let a: Array<i64> = Array::new(1);
            unsafe { strlen(as_c_bytes(text)); memset(as_c_bytes(a), 0, 8); }
            keep.len() + a[0]
        }
    "#);
    let ir = emit_llvm_ir(&prog, false).unwrap();
    for symbol in ["ai_str_copy_to_buf_v2", "ai_buf_copy_in_v2", "ai_buf_copy_out_v2", "ai_ffi_buffer_alloc_v2", "ai_ffi_buffer_free_v2"] {
        let call = ir.find(&format!("call {} @{symbol}", match symbol { "ai_str_copy_to_buf_v2" => "i64", "ai_ffi_buffer_alloc_v2" => "ptr", _ => "void" })).expect("missing versioned copy call");
        // LLVM block order differs from control-flow order. Follow successors
        // until the next call, so a later unrelated reload cannot satisfy this.
        let function_start = ir[..call].rfind("\ndefine ").unwrap_or(0);
        let function_end = function_start + ir[function_start..].find("\n}").unwrap();
        let mut blocks = std::collections::BTreeMap::<&str, Vec<&str>>::new();
        let mut label = "";
        for line in ir[function_start..function_end].lines() {
            if !line.starts_with(' ') && line.contains(':') {
                label = line.split(':').next().unwrap();
            }
            blocks.entry(label).or_default().push(line);
        }
        let call_line = ir[call..].lines().next().unwrap();
        let (start, index) = blocks.iter().find_map(|(&label, lines)| {
            lines.iter().position(|line| line.contains(call_line)).map(|i| (label, i + 1))
        }).unwrap();
        let mut pending = vec![(start, index)];
        let mut seen = std::collections::BTreeSet::new();
        let mut after = String::new();
        while let Some((label, index)) = pending.pop() {
            if !seen.insert(label) { continue; }
            for line in blocks[label].iter().skip(index) {
                if line.contains("call ") { break; }
                after.push_str(line);
                after.push('\n');
                for (_, suffix) in line.match_indices("label %").map(|(i, _)| (i, &line[i + 7..])) {
                    let target = suffix.split(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '_').next().unwrap();
                    pending.push((target, 0));
                }
            }
        }
        // A few spilled roots are read back unconditionally, more behind the
        // relocation-epoch check; either way each is reloaded.
        assert!(after.contains("root.relocated"), "{symbol}: missing caller reload before next call: {after}");
    }
    assert_eq!(unsafe { jit_run_i64_gc(&prog, true) }.unwrap(), 11);
}

#[test]
fn foreign_copy_out_keeps_original_owner_when_a_later_argument_replaces_field() {
    let prog = program(r#"
        struct Holder { bytes: Array<u8> }
        extern "C" fn memset(mut bytes: RawPtr, value: i32, count: i64) -> RawPtr;
        fn replace(mut holder: Holder) -> i32 {
            holder.bytes = Array::new(1);
            7i32
        }
        fn main() -> i64 {
            let holder = Holder { bytes: Array::new(65537) };
            let original = holder.bytes;
            unsafe { memset(as_c_bytes(holder.bytes), replace(holder), 65537); }
            if original[0] != 7u8 || original[65536] != 7u8 { return 1; }
            if holder.bytes.len() != 1 || holder.bytes[0] != 0u8 { return 2; }
            42
        }
    "#);
    assert_eq!(unsafe { jit_run_i64_gc(&prog, true) }.unwrap(), 42);
}
