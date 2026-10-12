//! Regression tests for the prelude stdlib round-out: generic `HashMap<K, V>`,
//! the `Display` trait + generic print helpers, and the `Vec`/string API
//! additions. Each program ends in an `i64` that encodes its checks, so a wrong
//! answer (not just a crash) fails the test. Run under GC-stress so the new
//! generic code is also exercised by the collector.

use gcrust::codegen::jit_run_i64_gc;
use gcrust::compile::parse_with_prelude;
use gcrust::lower::lower_program;
use gcrust::resolve::resolve_module;

/// Compile `src` (with the prelude) and JIT-run it, returning `main`'s i64.
fn run(src: &str) -> i64 {
    let (module, _) = parse_with_prelude(src).expect("parse");
    let resolved = resolve_module(module).expect("resolve");
    let prog = lower_program(&resolved.globals).expect("lower");
    // `true` = collect on every allocation, so the new generic heap shapes
    // (HashMap key/val arrays, etc.) are traced under stress.
    jit_run_i64_gc(&prog, true).expect("run")
}

#[test]
fn hashmap_string_keys_insert_get_overwrite() {
    let r = run(r#"
        fn main() -> i64 {
            let mut m: HashMap<String, i64> = HashMap::new();
            m.insert("alpha", 10);
            m.insert("beta", 20);
            m.insert("alpha", 11);            // overwrite, not a new entry
            let a = m.get("alpha").unwrap_or(0 - 1);     // 11
            let b = m.get("beta").unwrap_or(0 - 1);      // 20
            let miss = m.get("gamma").unwrap_or(999);    // 999
            let len = m.len();                      // 2 (overwrite didn't grow it)
            a + b + miss + len                             // 11 + 20 + 999 + 2 = 1032
        }
    "#);
    assert_eq!(r, 1032);
}

#[test]
fn hashmap_int_keys_force_grow() {
    // 50 entries forces several grows past the 0.7 load factor; the int hash
    // must spread sequential keys so probing stays correct across rehashes.
    let r = run(r#"
        fn main() -> i64 {
            let mut n: HashMap<i64, i64> = HashMap::new();
            let mut i = 0;
            while i < 50 { n.insert(i, i * i); i = i + 1; }
            let sq = n.get(7).unwrap_or(0 - 1);          // 49
            let len = n.len();                       // 50
            let has = if n.contains_key(49) { 1 } else { 0 };   // 1
            let nope = if n.contains_key(50) { 1 } else { 0 };  // 0
            sq + len + has + nope                           // 49 + 50 + 1 + 0 = 100
        }
    "#);
    assert_eq!(r, 100);
}

#[test]
fn hashmap_keys_vals_roundtrip() {
    let r = run(r#"
        fn main() -> i64 {
            let mut m: HashMap<i64, i64> = HashMap::new();
            m.insert(1, 100);
            m.insert(2, 200);
            m.insert(3, 300);
            let ks = m.keys();   // {1,2,3} in some order
            let vs = m.values();   // {100,200,300}
            ks.sum() + vs.sum()   // 6 + 600 = 606
        }
    "#);
    assert_eq!(r, 606);
}

#[test]
fn display_and_str_of() {
    // `bool` renders as true/false (length 4/5); ints/floats via to_string.
    let r = run(r#"
        fn main() -> i64 {
            let t = true.to_string();       // "true"  len 4
            let f = false.to_string();      // "false" len 5
            let n = 42.to_string();         // "42"    len 2
            t.len() + f.len() + n.len()   // 4 + 5 + 2 = 11
        }
    "#);
    assert_eq!(r, 11);
}

#[test]
fn string_formatting_helpers() {
    let r = run(r#"
        fn main() -> i64 {
            let rep = "ab".repeat(3);            // "ababab" len 6
            let pl = "42".pad_left(5, "0");      // "00042" len 5
            let pr = "x".pad_right(4, ".");      // "x..." len 4
            let mut cs = "abcdef".chars();
            cs.reverse();
            let rev = String::from_chars(cs);         // "fedcba"
            let rev_ok = if rev == "fedcba" { 1 } else { 0 };
            rep.len() + pl.len() + pr.len() + rev_ok   // 6 + 5 + 4 + 1 = 16
        }
    "#);
    assert_eq!(r, 16);
}

#[test]
fn vec_api_roundout() {
    let r = run(r#"
        fn main() -> i64 {
            let mut a: Vec<i64> = Vec::new();
            a.push(10); a.push(20); a.push(30); a.push(40);
            let mut b: Vec<i64> = Vec::new();
            b.push(50); b.push(60);

            let f = a.first().unwrap_or(0 - 1);        // 10
            let oob = a.get(9).unwrap_or(0 - 1); // -1
            let mut c = a.clone();
            c.extend(b);                                       // len 6
            let sl = c[1..4].sum();          // 20+30+40 = 90
            let tk = c[0..2].sum();              // 10+20 = 30
            let dr = c[4..].sum();                     // 50+60 = 110
            let mut swapped = a.clone();
            swapped.swap(0, 3);
            let sw = swapped.first().unwrap_or(0); // 40
            let pos = a.position(|x: i64| x - (x / 2) * 2 == 0).unwrap();          // 0
            let found = a.find(|x: i64| x > 25).unwrap_or(0 - 1);    // 30

            f + oob + c.len() + sl + tk + dr + sw + pos + found
            // 10 + (-1) + 6 + 90 + 30 + 110 + 40 + 0 + 30 = 315
        }
    "#);
    assert_eq!(r, 315);
}

#[test]
fn unicode_codepoint_api() {
    // "café" is 5 bytes but 4 code points (é is 2 bytes). The code-point API
    // must see 4 chars, round-trip losslessly, slice without splitting é, and
    // index code points (233 = U+00E9) distinctly from bytes (195 = é's lead byte).
    let r = run(r#"
        fn main() -> i64 {
            let s = "café";
            let bytes = s.len();                                            // 5
            let chars = s.chars().len();                                      // 4
            let rt = if String::from_chars(s.chars()) == s { 1 } else { 0 };  // 1
            let cp = s.chars()[3] as i64;                                     // 233
            let byte = s.byte_at(3);                                          // 195
            let sub_ok = if String::from_chars(s.chars()[0..4]) == s { 1 } else { 0 };    // 1 (é intact)
            let emoji_bytes = (128512 as char).to_string().len();          // 4 (😀)
            bytes + chars + rt + cp + byte + sub_ok + emoji_bytes
            // 5 + 4 + 1 + 233 + 195 + 1 + 4 = 443
        }
    "#);
    assert_eq!(r, 443);
}

#[test]
fn unicode_case_folding_and_invalid_scalar() {
    let r = run(r#"
        fn main() -> i64 {
            // ASCII folds; é (non-ASCII) passes through unchanged either way.
            let up_ok = if ("Café!".to_uppercase()) == "CAFé!" { 1 } else { 0 };
            let lo_ok = if ("HeLLo".to_lowercase()) == "hello" { 1 } else { 0 };
            let inv = char_to_str(0 - 5).len();   // invalid scalar -> U+FFFD = 3 bytes
            up_ok + lo_ok + inv                      // 1 + 1 + 3 = 5
        }
    "#);
    assert_eq!(r, 5);
}

#[test]
fn string_split_and_index_of() {
    let r = run(r#"
        fn main() -> i64 {
            let idx = "hello world".find("world").unwrap();   // 6
            let miss = if "abc".find("z").is_none() { 0 - 1 } else { 0 };  // -1
            let parts = "a,bb,ccc".split(",");           // 3 parts
            let joined = parts.join("|");                // "a|bb|ccc" len 8
            idx + miss + parts.len() + joined.len()     // 6 + (-1) + 3 + 8 = 16
        }
    "#);
    assert_eq!(r, 16);
}
