//! Compiled and runtime allocation share one cursor and publish initialized
//! extents correctly across nursery resets, worker allocation, and GC stress.
use std::process::Command;
mod support;

#[test]
fn inline_and_runtime_allocations_preserve_roots_and_exact_counters() {
    let dir = std::env::temp_dir().join(format!("gcr-inline-allocation-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/inline_allocation.gcr");
    let binary = dir.join("inline-allocation");
    let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
    build
        .arg("build")
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
    let compiled = support::run_with_timeout(&mut build, 60);
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let mut allocation_counts = Vec::new();
    for jit in [false, true] {
        for stress in [false, true] {
            for workers in [1, 4] {
                let metrics = dir.join(format!("metrics-{jit}-{stress}-{workers}.json"));
                let mut command = if jit {
                    let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
                    command.arg("run").arg(&source).arg("--jit");
                    command
                } else {
                    Command::new(&binary)
                };
                command
                    .env("GCR_GC_STRESS", if stress { "1" } else { "0" })
                    .env("GCR_GC_VERIFY", "1")
                    .env("GCR_NURSERY_MB", "1")
                    .env("GCR_TENURED_MB", "16")
                    .env("GCR_GC_WORKERS", workers.to_string())
                    .env("GCR_METRICS_FILE", &metrics);
                let output = support::run_with_timeout(&mut command, 60);
                assert!(
                    output.status.success(),
                    "jit={jit}, stress={stress}, workers={workers}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    String::from_utf8_lossy(&output.stdout).trim(),
                    if jit { "277276\n0" } else { "277276" }
                );
                let data: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(metrics).unwrap()).unwrap();
                if !stress {
                    assert!(
                        data["gc_minor"].as_u64().unwrap() > 0,
                        "must exercise nursery reset"
                    );
                }
                allocation_counts.push((
                    data["alloc_objects"].as_u64().unwrap(),
                    data["alloc_bytes"].as_u64().unwrap(),
                ));
            }
        }
    }
    // Mode and scheduling affect when collection runs, never allocated bytes.
    // 400 Item allocations and 386 arrays must all be counted.
    assert!(allocation_counts[0].0 >= 786);
    assert!(
        allocation_counts
            .iter()
            .all(|count| count == &allocation_counts[0]),
        "{allocation_counts:?}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn collecting_foreign_callback_refreshes_caller_roots_and_copy_out() {
    let dir = std::env::temp_dir().join(format!("gcr-root-mirror-callback-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/root_mirror_callback.gcr");
    for mode in ["jit", "native", "debug"] {
        let binary = dir.join(mode);
        if mode != "jit" {
            let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
            build
                .arg("build")
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
            if mode == "debug" {
                build.arg("--debug");
            }
            let output = support::run_with_timeout(&mut build, 60);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        for stress in [false, true] {
            let metrics = dir.join(format!("metrics-{mode}-{stress}.json"));
            let mut command = if mode == "jit" {
                let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
                command.arg("run").arg(&source).arg("--jit");
                command
            } else {
                Command::new(&binary)
            };
            command
                .env("GCR_GC_STRESS", if stress { "1" } else { "0" })
                .env("GCR_GC_VERIFY", "1")
                .env("GCR_NURSERY_MB", "1")
                .env("GCR_TENURED_MB", "16")
                .env("GCR_GC_WORKERS", "4")
                .env("GCR_METRICS_FILE", &metrics);
            let output = support::run_with_timeout(&mut command, 60);
            assert!(
                output.status.success(),
                "{mode} stress={stress}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                if mode == "jit" { "1050\n0" } else { "1050" }
            );
            let data: serde_json::Value =
                serde_json::from_slice(&std::fs::read(metrics).unwrap()).unwrap();
            assert!(
                data["gc_minor"].as_u64().unwrap() + data["gc_major"].as_u64().unwrap() > 0,
                "callback must actually collect"
            );
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn emitted_inline_headers_keep_numeric_words_on_both_byte_orders() {
    use gcrust::{
        codegen::codegen, compile::parse_with_prelude, lower::lower_program,
        resolve::resolve_module,
    };
    use inkwell::{
        context::Context,
        targets::TargetData,
        values::{InstructionOpcode, StructValue},
    };
    let source = "struct First { n: i64 } struct Second { first: First, n: i64 } fn make() -> Second { Second { first: First { n: 17 }, n: 25 } } fn main() -> i64 { let b = make(); b.first.n + b.n }";
    let (module, _) = parse_with_prelude(source).unwrap();
    let resolved = resolve_module(module).unwrap();
    let program = lower_program(&resolved.globals).unwrap();
    let context = Context::create();
    let compiled = codegen(&context, &program).unwrap();
    let little = TargetData::create("e-p:64:64-i64:64-n8:16:32:64-S128");
    let big = TargetData::create("E-p:64:64-i64:64-n32:64-S128");
    // Serialize actual emitted constants using LLVM's independent field layout,
    // accepting any integer-field decomposition that preserves the word ABI.
    fn bytes(value: StructValue<'_>, layout: &TargetData, big: bool) -> Vec<u8> {
        let ty = value.get_type();
        let mut bytes = vec![0; layout.get_store_size(&ty) as usize];
        for index in 0..value.count_fields() {
            let field = value.get_field_at_index(index).unwrap().into_int_value();
            let number = field.get_zero_extended_constant().unwrap();
            let size = layout.get_store_size(&field.get_type()) as usize;
            let offset = layout.offset_of_element(&ty, index).unwrap() as usize;
            let encoded = if big {
                number.to_be_bytes()
            } else {
                number.to_le_bytes()
            };
            let encoded = if big {
                &encoded[8 - size..]
            } else {
                &encoded[..size]
            };
            bytes[offset..offset + size].copy_from_slice(encoded);
        }
        bytes
    }
    let mut found = 0;
    let mut nonzero_id = false;
    for function in compiled.module.get_functions() {
        for block in function.get_basic_blocks() {
            for instruction in block.get_instructions() {
                if instruction.get_opcode() != InstructionOpcode::Store {
                    continue;
                }
                let destination = instruction
                    .get_operand(1)
                    .unwrap()
                    .value()
                    .unwrap()
                    .into_pointer_value();
                if !destination
                    .get_name()
                    .to_bytes()
                    .starts_with(b"tlab.object")
                {
                    continue;
                }
                let Some(inkwell::values::BasicValueEnum::StructValue(header)) =
                    instruction.get_operand(0).unwrap().value()
                else {
                    continue;
                };
                let le = bytes(header, &little, false);
                let be = bytes(header, &big, true);
                assert_eq!(le.len(), 16);
                assert_eq!(be.len(), 16);
                for offset in [0, 8] {
                    let expected = u64::from_le_bytes(le[offset..offset + 8].try_into().unwrap());
                    let actual = u64::from_be_bytes(be[offset..offset + 8].try_into().unwrap());
                    assert_eq!(
                        actual, expected,
                        "header word {offset} changes meaning with byte order"
                    );
                    assert_eq!(actual & gcrust::gc::FORWARDING_BIT, 0);
                    if offset == 8 {
                        assert!(actual <= u16::MAX as u64);
                        nonzero_id |= actual != 0;
                    }
                }
                found += 1;
            }
        }
    }
    assert!(
        found >= 2 && nonzero_id,
        "fixture did not emit distinct heap headers"
    );
}
