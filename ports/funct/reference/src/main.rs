//! Same modes and output as ports/funct/src/main.gcr, over the real funct.
use funct::lexer::lex;
use funct::parser::parse;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: funct-reference (lex|lexbench|parse|parsebench) <file> [iters]");
        std::process::exit(2);
    }
    let src = std::fs::read_to_string(&args[2]).expect("read source");
    match args[1].as_str() {
        "lex" => match lex(&src) {
            Ok(toks) => { for t in toks { println!("{:?}", t); } }
            Err(e) => println!("error: {}", e),
        },
        "lexbench" => {
            let iters: u64 = args[3].parse().unwrap();
            let mut count = 0;
            let start = Instant::now();
            for _ in 0..iters { count += std::hint::black_box(lex(std::hint::black_box(&src)).unwrap()).len(); }
            let elapsed = start.elapsed().as_nanos() as u64;
            println!("tokens={} ns_per_pass={}", count as u64 / iters, elapsed / iters);
        }
        "parse" => match parse(&src) {
            Ok(program) => println!("{:#?}", program),
            Err(e) => println!("error: {}", e),
        },
        "parsebench" => {
            let iters: u64 = args[3].parse().unwrap();
            let mut count = 0;
            let start = Instant::now();
            for _ in 0..iters { count += std::hint::black_box(parse(std::hint::black_box(&src)).unwrap()).items.len(); }
            let elapsed = start.elapsed().as_nanos() as u64;
            println!("items={} ns_per_pass={}", count as u64 / iters, elapsed / iters);
        }
        other => println!("unknown mode {}", other),
    }
}
