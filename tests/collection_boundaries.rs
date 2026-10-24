//! Defined boundary failures protect safe library calls even when metadata and
//! backing arrays disagree. Each process is bounded because failures abort.
use std::process::Command;
mod support;

#[test]
fn backing_array_bounds_and_allocation_sizes_fail_before_memory_access() {
    let dir =
        std::env::temp_dir().join(format!("gcr-collection-boundaries-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // The raw-handle mismatch case keeps the original wrapper live through
    // the call: a borrowed native control does not retain its managed owner.
    let cases = [
        ("zero-channel-capacity", "let ch: Channel<i64> = Channel::new(0); 0", "channel capacity must be positive"),
        ("negative-channel-capacity", "let ch: Channel<i64> = Channel::new(0 - 1); 0", "channel capacity must be positive"),
        ("channel-type-mismatch", "let mut ints: Channel<i64> = Channel::new(1); let strings: Channel<String> = Channel::new(1); unsafe { ints.ctrl = strings.ctrl; } ints.send(42); strings.close(); 0", "channel handle/message type mismatch"),
        ("unit-negative-index", "let a: Array<()> = Array::new(1); a[0 - 1]; 42", "array index out of bounds"),
        ("unit-past-end-write", "let mut a: Array<()> = Array::new(1); a[1] = (); 42", "array index out of bounds"),
        ("negative-index", "let a: Array<i64> = Array::new(1); a[0 - 1]", "array index out of bounds"),
        ("past-end", "let a: Array<i64> = Array::new(1); a[1]", "array index out of bounds"),
        ("negative-length", "let a: Array<i64> = Array::new(0 - 1); a.len()", "invalid array length"),
        ("wrapped-byte-length", "let a: Array<i64> = Array::new(2305843009213693952); a.len()", "invalid array length"),
        ("traced-overflow", "let a: Array<String> = Array::new(2305843009213693952); a.len()", "invalid array length"),
        ("header-overflow", "let a: Array<i8> = Array::new(9223372036854775807); a.len()", "allocation size overflow"),
        ("unset-reference", "let a: Array<String> = Array::new(1); a[0].len()", "uninitialized array element"),
        ("unset-optional-reference", "let a: Array<String> = Array::new(1); match a.get(0) { Option::Some(s) => s.len(), Option::None => 0 }", "uninitialized array element"),
        ("unset-value", "let a: Array<(String, i64)> = Array::new(1); a[0].0.len()", "uninitialized array element"),
        ("unset-native-join", "let a: Array<String> = Array::new(1); str_join_array(a, 1, \"|\").len()", "uninitialized array element"),
    ];
    for (name, body, diagnostic) in cases {
        let source = dir.join(format!("{name}.gcr"));
        let binary = dir.join(name);
        std::fs::write(&source, format!("fn main() -> i64 {{ {body} }}")).unwrap();
        let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
        build
            .arg("build")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
        let output = support::run_with_timeout(&mut build, 60);
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        for jit in [false, true] {
            for stress in [false, true] {
                let mut command = if jit {
                    let mut command = Command::new(env!("CARGO_BIN_EXE_gcr"));
                    command.arg("run").arg(&source).arg("--jit");
                    command
                } else {
                    Command::new(&binary)
                };
                command
                    .env("GCR_GC_STRESS", if stress { "1" } else { "0" })
                    .env("GCR_GC_VERIFY", "1");
                let output = support::run_with_timeout(&mut command, 30);
                assert!(!output.status.success(), "{name} jit={jit} stress={stress}");
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains(diagnostic),
                    "{name} jit={jit} stress={stress}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
    // A `Vec`'s length and storage cannot be made to disagree: its fields
    // cannot be written or built outside the standard library.
    for (name, body) in [
        ("metadata-mismatch", "let mut v: Vec<i64> = Vec::new(); v.len = 10; match v.get(9) { Option::Some(n) => n, Option::None => 0 }"),
        ("metadata-literal", "let v: Vec<i64> = Vec { data: Array::new(1), len: 10, cap: 1 }; v[9]"),
    ] {
        let source = dir.join(format!("{name}.gcr"));
        std::fs::write(&source, format!("fn main() -> i64 {{ {body} }}")).unwrap();
        let mut build = Command::new(env!("CARGO_BIN_EXE_gcr"));
        build.arg("build").arg(&source).arg("-o").arg(dir.join(name))
            .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
        let output = support::run_with_timeout(&mut build, 60);
        assert!(!output.status.success(), "{name}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("of `Vec` is private"), "{name}: {}", String::from_utf8_lossy(&output.stderr));
    }
    std::fs::remove_dir_all(dir).unwrap();
}
