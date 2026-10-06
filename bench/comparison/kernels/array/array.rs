// Matched single-threaded integer kernel, signed 64-bit throughout.
fn main() { let mut a = vec![7i64; 1024]; for i in 0..5000000i64 { let k = (i % 1024) as usize; a[k] = (a[k]*17+i)%1000000007; } println!("{}", a.iter().sum::<i64>()); }
