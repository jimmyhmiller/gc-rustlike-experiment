//! Small SSA and large memory-backed enums must encode the same payloads.
#[test]
fn nested_packed_and_wide_enum_payloads_survive_gc() {
    let padding = (0..20).map(|i| format!("f{i}: i64")).collect::<Vec<_>>().join(",");
    let safepoints = (0..40).map(|_| "churn = str_concat(churn, \"!\");").collect::<String>();
    let source = format!(r#"
        #[value] enum Tiny {{ Number(i64), Text(String), Empty }}
        #[value] struct Payload {{
            flag: bool, byte: u8, short: u16, wide: i128,
            real: f64, name: String, nested: Tiny
        }}
        #[value] struct Padding {{ {padding} }}
        #[value] enum Small {{ Data(Payload), Empty, Text(String) }}
        #[value] enum Large {{ Data(Payload), Empty, Pad(Padding) }}
        #[inline(never)] fn payload(n: i64) -> Payload {{
            Payload {{ flag: true, byte: 251u8, short: 65001u16,
                wide: -(1i128 << 100) + 123i128, real: -123.125,
                name: str_concat("kept", " alive"),
                nested: if n == 0 {{ Tiny::Text(str_concat("nested", " text")) }}
                    else {{ Tiny::Number(n) }} }}
        }}
        #[inline(never)] fn check(p: Payload, n: i64) -> i64 {{
            let nested = match p.nested {{
                Tiny::Text(s) => if s == "nested text" && n == 0 {{ 1 }} else {{ 0 }},
                Tiny::Number(v) => if v == n {{ 1 }} else {{ 0 }},
                Tiny::Empty => 0
            }};
            if p.flag && p.byte == 251u8 && p.short == 65001u16
                && p.wide == -(1i128 << 100) + 123i128
                && p.real == -123.125 && p.name == "kept alive" {{ nested }} else {{ 0 }}
        }}
        #[inline(never)] fn small(e: Small, n: i64) -> i64 {{
            match e {{ Small::Data(p) => check(p, n), Small::Empty => 0, Small::Text(_) => 0 }}
        }}
        #[inline(never)] fn large(e: Large, n: i64) -> i64 {{
            match e {{ Large::Data(p) => check(p, n), Large::Empty => 0, Large::Pad(_) => 0 }}
        }}
        fn main() -> i64 {{
            let a = Small::Data(payload(0));
            let b = Large::Data(payload(17));
            let mut i = 0;
            while i < 100 {{ let s = str_concat("churn", " allocation"); i = i + str_len(s); }}
            let mut churn = "";
            {safepoints}
            if str_len(churn) != 40 {{ return 0; }}
            small(a, 0) + large(b, 17)
        }}
    "#);
    let path = std::env::temp_dir().join(format!("gcr-enum-storage-{}.gcr", std::process::id()));
    std::fs::write(&path, source).unwrap();
    for optimized in [true, false] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_gcr"));
        command.arg("run").arg(&path).arg("--jit");
        if !optimized { command.arg("--no-opt"); }
        let output = command.env("GCR_GC_STRESS", "1").env("GCR_GC_VERIFY", "1").output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "2");
    }
    std::fs::remove_file(path).unwrap();
}
