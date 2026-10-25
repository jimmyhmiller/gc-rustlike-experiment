//! Same modes and output as ports/funct/src/main.gcr, over the real funct.
use funct::lexer::lex;
use funct::parser::parse;
use std::time::Instant;
use funct::{Funct, Value};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 4 && args[1] == "bench" {
        benchmark_batch(args);
        return;
    }
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
        "newbench" => {
            let iters: u64 = args[3].parse().unwrap();
            let start = Instant::now();
            for _ in 0..iters { std::hint::black_box(funct::Funct::new()); }
            let elapsed = start.elapsed().as_nanos() as u64;
            println!("ns_per_pass={}", elapsed / iters);
        }
        other => println!("unknown mode {}", other),
    }
}

// Run several workloads after one compiler startup. Each eval gets a fresh VM.
// bench <warmup> <samples> (<run|lex|parse|test> <path>)+
fn benchmark_batch(args: Vec<String>) {
    let warmup: i64 = args[2].parse().expect("warmup");
    let samples: i64 = args[3].parse().expect("samples");
    assert!(warmup >= 0 && samples > 0 && args.len() > 4 && args.len() % 2 == 0);
    let mut k: usize = 4;
    while k < args.len() {
        let mode = args[k].as_str();
        let path = std::path::Path::new(&args[k + 1]);
        for i in 0..warmup + samples {
            let start = Instant::now();
            match mode {
                "run" => {
                    let src = std::fs::read_to_string(path).expect("read workload");
                    let mut vm = Funct::new();
                    if let Some(dir) = path.parent() { vm.set_module_root(dir); }
                    match vm.eval(&src).expect("eval workload") {
                        Value::Unit => {},
                        v => println!("{}", v),
                    }
                },
                "lex" => {
                    let src = std::fs::read_to_string(path).expect("read workload");
                    println!("tokens={}", lex(&src).expect("lex workload").len());
                },
                "parse" => {
                    let src = std::fs::read_to_string(path).expect("read workload");
                    println!("items={}", parse(&src).expect("parse workload").items.len());
                },
                "test" => assert!(funct::testing::run_and_print(path)),
                _ => panic!("unknown benchmark mode"),
            }
            let elapsed = start.elapsed().as_nanos() as i64;
            println!("@sample {} {} {}", (k - 4) / 2, i, elapsed);
        }
        k += 2;
    }
}
