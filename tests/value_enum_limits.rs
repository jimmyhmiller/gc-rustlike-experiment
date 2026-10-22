use std::process::Command;
#[allow(dead_code)]
mod support;

#[test]
fn large_value_enum_reference_counts_report_layout_errors_without_panicking_or_wrapping() {
    let root = std::env::temp_dir().join(format!("gcr-value-enum-limits-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    for count in [8192usize, 8193, 65536] {
        let source = root.join(format!("{count}.gcr"));
        let types = vec!["Node"; count].join(",");
        let arguments = vec!["node"; count].join(",");
        std::fs::write(&source, format!("#[heap] struct Node {{ n:i64 }} #[value] enum Huge {{ All({types}) }} fn main()->i64 {{ let node=Node {{n:1}}; let value=Huge::All({arguments}); 0 }}")).unwrap();
        let output = support::run_with_timeout(
            Command::new(env!("CARGO_BIN_EXE_gcr"))
                .arg("emit")
                .arg("layout")
                .arg(&source),
            30,
        );
        let error = String::from_utf8_lossy(&output.stderr);
        if count == 8192 {
            assert!(output.status.success(), "count={count}: {error}");
            let layout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let value = layout["values"]
                .as_array()
                .unwrap()
                .iter()
                .find(|value| value["name"] == "Huge")
                .unwrap();
            assert_eq!(value["size"], 65544, "{value}");
            assert_eq!(
                value["variants"][0]["fields"].as_array().unwrap().len(),
                count
            );
            continue;
        }
        assert_eq!(output.status.code(), Some(1), "count={count}: {error}");
        assert!(
            error.contains("layout exceeds representable offset or size limit"),
            "{error}"
        );
        assert!(!error.contains("panicked"), "{error}");
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn nested_value_reference_offsets_are_checked_before_codegen() {
    let root = std::env::temp_dir().join(format!("gcr-nested-value-limits-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let types = vec!["Node"; 8191].join(",");
    let arguments = vec!["node"; 8191].join(",");
    for (name, definitions, expression, accepted) in [
        ("boundary", "#[value] struct Outer { first:i64, huge:Huge }", "Outer {first:1,huge:huge}", true),
        ("struct", "#[value] struct Outer { first:i64, second:i64, huge:Huge }", "Outer {first:1,second:2,huge:huge}", false),
        ("tuple", "", "(1,2,huge)", false),
        ("recursive", "#[value] struct Inner { first:i64, huge:Huge } #[value] struct Outer { first:i64, inner:Inner }", "Outer {first:1,inner:Inner {first:2,huge:huge}}", false),
    ] {
        let source = root.join(format!("{name}.gcr"));
        std::fs::write(&source, format!("#[heap] struct Node {{ n:i64 }} #[value] enum Huge {{ All({types}) }} {definitions} fn main()->i64 {{ let node=Node {{n:1}}; let huge=Huge::All({arguments}); let outer={expression}; 0 }}")).unwrap();
        for stage in if accepted { vec!["layout"] } else { vec!["layout", "llvm"] } {
            let output = support::run_with_timeout(Command::new(env!("CARGO_BIN_EXE_gcr")).arg("emit").arg(stage).arg(&source), 30);
            let error = String::from_utf8_lossy(&output.stderr);
            if accepted {
                assert!(output.status.success(), "{name}/{stage}: {error}");
            } else {
                assert_eq!(output.status.code(), Some(1), "{name}/{stage}: {error}");
                assert!(error.contains("layout exceeds representable offset or size limit"), "{name}/{stage}: {error}");
                assert!(!error.contains("panicked"), "{error}");
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn scalar_only_nested_offsets_do_not_overflow_reference_traversal() {
    let root =
        std::env::temp_dir().join(format!("gcr-scalar-value-offsets-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("main.gcr");
    let mut definitions = vec!["#[value] struct S0 { n:i64 }".to_string()];
    let mut initializers = vec!["let s0=S0 {n:0};".to_string()];
    for i in 1..13 {
        definitions.push(format!(
            "#[value] struct S{i} {{left:S{},right:S{}}}",
            i - 1,
            i - 1
        ));
        initializers.push(format!(
            "let s{i}=S{i} {{left:s{},right:s{}}};",
            i - 1,
            i - 1
        ));
    }
    // 8 + 16 + ... + 32768 = 65528 bytes, using only thirteen fields.
    // The enclosing reference moves Inner.leaf to offset 65536. That leaf
    // contains no references and must not consume a GC descriptor offset.
    let fields = (0..13)
        .map(|i| format!("f{i}:S{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let arguments = (0..13)
        .map(|i| format!("f{i}:s{i}"))
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(&source, format!("#[heap] struct Node {{ n:i64 }} {} #[value] struct Big {{ {fields} }} #[value] struct Leaf {{ n:i64 }} #[value] struct Inner {{ big:Big, leaf:Leaf }} #[value] struct Outer {{ node:Node, inner:Inner }} fn main()->i64 {{ {} let outer=Outer {{node:Node {{n:1}},inner:Inner {{big:Big {{{arguments}}},leaf:Leaf {{n:42}}}}}}; outer.inner.leaf.n }}", definitions.join(" "), initializers.join(" "))).unwrap();
    for stage in ["layout", "llvm"] {
        let output = support::run_with_timeout(
            Command::new(env!("CARGO_BIN_EXE_gcr"))
                .arg("emit")
                .arg(stage)
                .arg(&source)
                .arg("--no-opt"),
            30,
        );
        assert!(
            output.status.success(),
            "{stage}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}
