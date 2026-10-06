// Matched single-threaded integer kernel, signed 64-bit throughout.
fn main() { let mut x: i64 = 7; for i in 0..10000000i64 { x = (x * 17 + i) % 1000000007; } println!("{}", x); }
