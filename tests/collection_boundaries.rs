//! Defined boundary failures protect safe library calls even when metadata and
//! backing arrays disagree. Each process is bounded because failures abort.
use std::process::Command;
mod support;

#[test]
fn backing_array_bounds_and_allocation_sizes_fail_before_memory_access() {
    let dir =
        std::env::temp_dir().join(format!("gcr-collection-boundaries-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cases = [
        ("zero-channel-capacity", "let ch: Channel<i64> = Channel::new(0); 0", "channel capacity must be positive"),
        ("negative-channel-capacity", "let ch: Channel<i64> = Channel::new(0 - 1); 0", "channel capacity must be positive"),
        ("channel-type-mismatch", "let mut ints: Channel<i64> = Channel::new(1); let strings: Channel<String> = Channel::new(1); ints.ctrl = strings.ctrl; ints.send(42); 0", "channel handle/message type mismatch"),
        ("negative-index", "let a: Array<i64> = array_new(1); array_get_unchecked(a, 0 - 1)", "array index out of bounds"),
        ("past-end", "let a: Array<i64> = array_new(1); array_get_unchecked(a, 1)", "array index out of bounds"),
        ("metadata-mismatch", "let mut v: Vec<i64> = vec_new(); v.len = 10; match vec_get(v, 9) { Option::Some(n) => n, Option::None => 0 }", "array index out of bounds"),
        ("negative-length", "let a: Array<i64> = array_new(0 - 1); array_len(a)", "invalid array length"),
        ("wrapped-byte-length", "let a: Array<i64> = array_new(2305843009213693952); array_len(a)", "invalid array length"),
        ("traced-overflow", "let a: Array<String> = array_new(2305843009213693952); array_len(a)", "invalid array length"),
        ("header-overflow", "let a: Array<i8> = array_new(9223372036854775807); array_len(a)", "allocation size overflow"),
        ("unset-reference", "let a: Array<String> = array_new(1); str_len(array_get_unchecked(a, 0))", "uninitialized array element"),
        ("unset-optional-reference", "let a: Array<String> = array_new(1); match array_get(a, 0) { Option::Some(s) => str_len(s), Option::None => 0 }", "uninitialized array element"),
        ("unset-value", "let a: Array<Option<String>> = array_new(1); match a[0] { Option::Some(s) => str_len(s), Option::None => 0 }", "uninitialized array element"),
        ("unset-native-join", "let a: Array<String> = array_new(1); str_len(str_join_array(a, 1, \"|\"))", "uninitialized array element"),
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
    std::fs::remove_dir_all(dir).unwrap();
}
