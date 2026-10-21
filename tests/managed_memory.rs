//! Managed SC slot and aggregate snapshot conformance. Explicit Sync assertions
//! let these fixtures test backend races before the remaining library/API audit
//! permits removing the transitional capture restrictions project-wide.
use std::process::Command;
mod support;

const SOURCE: &str = include_str!("fixtures/managed_memory.gcr");

#[test]
fn mixed_sc_and_racing_snapshots_survive_gc_in_jit_and_aot() {
    let dir = std::env::temp_dir().join(format!("gcr-managed-memory-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("gcr.toml"),
        "[package]\nname = \"managed-memory\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/main.gcr"), SOURCE).unwrap();
    let executable = dir.join("managed-memory");
    let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
    build
        .arg("build")
        .arg(&dir)
        .arg("-o")
        .arg(&executable)
        .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
    let out = support::run_with_timeout(&mut build, 60);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for jit in [false, true] {
        for stress in [false, true] {
            let mut cmd = if jit {
                let mut cmd = Command::new(env!("CARGO_BIN_EXE_gcr"));
                cmd.arg("run").arg(&dir).arg("--jit");
                cmd
            } else {
                Command::new(&executable)
            };
            cmd.env("GCR_GC_STRESS", if stress { "1" } else { "0" })
                .env("GCR_GC_VERIFY", "1")
                .env("GCR_GC_WORKERS", "4")
                .env("GCR_NURSERY_MB", "1")
                .env("GCR_TENURED_MB", "8");
            let out = support::run_with_timeout(&mut cmd, 60);
            assert_eq!(
                out.status.code(),
                Some(0),
                "jit={jit} stress={stress}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn generated_accesses_are_atomic_or_proven_private_and_snapshots_cannot_collect() {
    use gcrust::compile::parse_with_prelude;
    use gcrust::lower::lower_program;
    use gcrust::resolve::resolve_module;
    let (module, _) = parse_with_prelude(SOURCE).unwrap();
    let resolved = resolve_module(module).unwrap();
    let program = lower_program(&resolved.globals).unwrap();
    for optimize in [false, true] {
        let ir = gcrust::codegen::emit_llvm_ir(&program, optimize).unwrap();
        assert!(ir.contains("load atomic i64") && ir.contains("store atomic i64"));
        assert!(ir.contains("load atomic i8") && ir.contains("store atomic i8"));
        assert!(ir.contains("load atomic double") && ir.contains("store atomic double"));
        assert!(check_snapshot_paths(&ir) > 0, "missing aggregate regions");
        assert!(ir.lines().any(|line| line.contains("call i8 @ai_managed_try_write(")), "missing nonparking aggregate attempt: {ir}");
        assert!(ir.contains("aggregate.contended"), "missing rooted contention fallback: {ir}");
        for line in ir.lines() {
            if line.contains(" = load ") && line.contains("%fld") && !line.contains("%fld.snapshot")
            {
                assert!(
                    line.contains("load atomic") || line.contains("!alias.scope"),
                    "mutable field has neither SC ordering nor private provenance: {line}"
                );
            }
        }
    }
    // Full debug permits edited references and disables the closed-world
    // privacy proof, so every scalar/reference field must stay atomic.
    let context = inkwell::context::Context::create();
    let compiled = gcrust::codegen::codegen_with_debug(
        &context, &program, gcrust::codegen::DebugLevel::Full,
    ).unwrap();
    let ir = compiled.module.print_to_string().to_string();
    for line in ir.lines() {
        if line.contains(" = load ") && line.contains("%fld") && !line.contains("%fld.snapshot") {
            assert!(line.contains("load atomic"), "non-atomic full-debug field: {line}");
        }
    }
}

// Follow control flow rather than textual block order. LLVM may lay a poll
// block between acquisition and release in the printed IR without putting it
// on any path that holds the snapshot stripe.
fn llvm_label(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix('"') {
        // LLVM quoted identifiers encode embedded quotes as hexadecimal
        // escapes, so the next literal quote terminates the identifier.
        let end = rest.find('"').expect("unterminated LLVM label");
        &text[..end + 2]
    } else {
        let end = text.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$' | '-')))
            .unwrap_or(text.len());
        &text[..end]
    }
}

fn check_snapshot_paths(ir: &str) -> usize {
    use std::collections::{HashMap, HashSet};
    let mut functions = Vec::new();
    let mut current = Vec::new();
    let mut inside = false;
    for line in ir.lines() {
        if line.starts_with("define ") {
            inside = true;
            current.clear();
        } else if inside && line == "}" {
            functions.push(std::mem::take(&mut current));
            inside = false;
        } else if inside {
            current.push(line);
        }
    }
    let mut snapshots = 0;
    for lines in functions {
        let mut blocks: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut entry = None;
        let mut label = "";
        for line in lines {
            let name = llvm_label(line);
            if !name.is_empty() && line[name.len()..].starts_with(':') {
                label = name;
                entry.get_or_insert(name);
                blocks.entry(name).or_default();
            } else if !label.is_empty() {
                blocks.get_mut(label).unwrap().push(line);
            }
        }
        let Some(entry) = entry else { continue };
        let mut pending = vec![(entry.to_string(), false)];
        let mut visited = HashSet::new();
        let mut locks = HashSet::new();
        while let Some((label, mut held)) = pending.pop() {
            if !visited.insert((label.clone(), held)) {
                continue;
            }
            let lines = blocks
                .get(label.as_str())
                .expect("missing control-flow block");
            for (index, line) in lines.iter().enumerate() {
                if line.contains("call ptr @ai_managed_lock(") {
                    assert!(!held, "nested aggregate region in {label}");
                    held = true;
                    locks.insert((label.clone(), index));
                } else if line.contains("call void @ai_managed_unlock(") {
                    assert!(held, "unpaired release in {label}");
                    held = false;
                } else if held && line.contains("call ") {
                    assert!(
                        line.contains("@ai_gc_write_barrier("),
                        "unexpected call in snapshot region: {line}"
                    );
                }
                if line.trim_start().starts_with("ret ") || line.trim() == "unreachable" {
                    assert!(!held, "snapshot stripe held at exit in {label}");
                }
            }
            for line in lines {
                for target in line.split("label %").skip(1) {
                    let name = llvm_label(target);
                    assert!(!name.is_empty(), "missing LLVM branch target: {line}");
                    pending.push((name.to_owned(), held));
                }
            }
        }
        snapshots += locks.len();
    }
    snapshots
}

#[test]
fn snapshot_path_check_accepts_reordered_blocks_and_rejects_collecting_paths() {
    let good = "define void @test() {\nentry:\n  call ptr @ai_managed_lock(ptr %t, ptr %o)\n  br label %release\nunrelated:\n  call void @ai_gc_pollcheck_slow(ptr %t)\n  ret void\nrelease:\n  call void @ai_managed_unlock(ptr %t)\n  ret void\n}\n";
    assert_eq!(check_snapshot_paths(good), 1);
    let quoted = good.replace("release", "\"Atom.RefCounter.new$RefCounter:exit-i\"");
    assert_eq!(check_snapshot_paths(&quoted), 1);
    let bad = good.replace("br label %release", "br label %unrelated");
    assert!(std::panic::catch_unwind(|| check_snapshot_paths(&bad)).is_err());
}

#[test]
fn scalar_and_reference_slots_keep_phase_checked_scoped_fallbacks() {
    use gcrust::compile::parse_with_prelude;
    use gcrust::lower::lower_program;
    use gcrust::resolve::resolve_module;
    let (module, _) = parse_with_prelude(r#"
        struct Item { byte: i8, word: i16, wide: i32, number: i64, text: String }
        fn main() -> i64 {
            let mut item = Item { byte: 1 as i8, word: 2 as i16, wide: 3 as i32, number: 4, text: "a" };
            item.byte = item.byte + 1 as i8;
            item.word = item.word + 1 as i16;
            item.wide = item.wide + 1 as i32;
            item.number = item.number + 1;
            item.text = "b";
            let mut refs: Array<Item> = Array::new(1);
            refs[0] = item;
            let mut bytes: Array<i8> = Array::new(1);
            bytes[0] = refs[0].byte;
            bytes[0] as i64
        }
    "#).unwrap();
    let resolved = resolve_module(module).unwrap();
    let program = lower_program(&resolved.globals).unwrap();
    // Unoptimized emission retains both paths even for closed-world private
    // objects. Optimization may fold accesses when their results are known.
    let ir = gcrust::codegen::emit_llvm_ir(&program, false).unwrap();
    assert!(ir.contains("load atomic i8") && ir.contains("copy.phase"), "{ir}");
    for width in [8, 16, 32, 64] {
        for operation in ["load", "store"] {
            let symbol = format!("@ai_slot_bits{width}_{operation}(");
            assert!(ir.lines().any(|line| line.contains("call ") && line.contains(&symbol)), "{ir}");
        }
    }
    assert!(ir.lines().any(|line| line.contains("call void @ai_slot_ref_store(")), "{ir}");
    assert!(ir.lines().any(|line| line.contains(" = load ") && line.contains("!alias.scope")), "private idle path lost its provenance: {ir}");
}

#[test]
fn native_aggregate_snapshot_uses_target_layout_before_optimization() {
    use gcrust::{compile::parse_with_prelude, lower::lower_program, resolve::resolve_module};
    let (module, _) = parse_with_prelude(r#"
        struct Leaf { n: i64 }
        #[value] struct Mixed { tag: i32, wide: i64, leaf: Leaf }
        struct Holder { mixed: Mixed }
        fn main() -> i64 {
            let holder = Holder { mixed: Mixed { tag: 7 as i32, wide: 123456789, leaf: Leaf { n: 42 } } };
            let observed = holder.mixed;
            if observed.tag != 7 as i32 || observed.wide != 123456789 || observed.leaf.n != 42 { 81 } else { 0 }
        }
    "#).unwrap();
    let program = lower_program(&resolve_module(module).unwrap().globals).unwrap();
    for stress in [false, true] {
        assert_eq!(unsafe { gcrust::codegen::jit_run_i64_gc(&program, stress) }.unwrap(), 0);
    }
    for optimize in [false, true] {
        let ir = gcrust::codegen::emit_llvm_ir(&program, optimize).unwrap();
        assert!(ir.contains("target datalayout =") && ir.contains("target triple ="), "{ir}");
        assert!(ir.lines().any(|line| line.contains("call void @ai_managed_read(")), "{ir}");
    }
}

#[test]
fn aggregate_write_reuses_rooted_value_local_storage() {
    use gcrust::{compile::parse_with_prelude, lower::lower_program, resolve::resolve_module};
    let (module, _) = parse_with_prelude(r#"
        struct Leaf { n: i64 }
        #[value] struct Pair { left: Leaf, right: Leaf }
        struct Holder { pair: Pair }
        fn replace(mut holder: Holder, pair: Pair) { holder.pair = pair; }
        fn main() -> i64 {
            let holder = Holder { pair: Pair { left: Leaf { n: 1 }, right: Leaf { n: 2 } } };
            replace(holder, Pair { left: Leaf { n: 42 }, right: Leaf { n: 43 } });
            if holder.pair.left.n == 42 && holder.pair.right.n == 43 { 0 } else { 81 }
        }
    "#).unwrap();
    let program = lower_program(&resolve_module(module).unwrap().globals).unwrap();
    let ir = gcrust::codegen::emit_llvm_ir(&program, false).unwrap();
    let function = ir.split("define ").find(|body| body.lines().next()
        .is_some_and(|line| line.contains("@replace(") || line.contains("@\"replace\"("))).unwrap();
    let body = function.split("\n}").next().unwrap();
    assert!(body.contains("call i8 @ai_managed_try_write("), "{body}");
    assert!(body.contains("call ptr @ai_managed_lock("), "{body}");
    assert!(!body.contains("aggregate.input"), "rooted RHS was duplicated: {body}");
    for stress in [false, true] {
        assert_eq!(unsafe { gcrust::codegen::jit_run_i64_gc(&program, stress) }.unwrap(), 0);
    }
}
