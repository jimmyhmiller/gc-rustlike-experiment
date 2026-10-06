//! Application results are compared with independent scan-based models.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::{Command, Output},
    sync::OnceLock,
};
mod support;
struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("gcr-real-apps-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn write(&self, name: &str, text: &str) -> String {
        let p = self.0.join(name);
        std::fs::write(&p, text).unwrap();
        p.to_str().unwrap().to_owned()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn bins() -> &'static [PathBuf; 3] {
    static BINS: OnceLock<[PathBuf; 3]> = OnceLock::new();
    BINS.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("gcr-real-apps-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lib = support::runtime_staticlib();
        ["gcr-csvreport", "gcr-buildplan", "gcr-routes"].map(|name| {
            let bin = dir.join(name);
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_gcr"));
            cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
                .args(["build", &format!("apps/{name}"), "-o"])
                .arg(&bin)
                .env("GCRUST_RUNTIME_LIB", &lib);
            let out = support::run_with_timeout(&mut cmd, 90);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            bin
        })
    })
}
fn run(app: usize, args: &[&str], jit: bool, stress: bool) -> Output {
    let mut cmd = if jit {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gcr"));
        cmd.current_dir(env!("CARGO_MANIFEST_DIR")).args([
            "run",
            [
                "apps/gcr-csvreport",
                "apps/gcr-buildplan",
                "apps/gcr-routes",
            ][app],
            "--jit",
            "--",
        ]);
        cmd
    } else {
        Command::new(&bins()[app])
    };
    cmd.args(args)
        .env_remove("GCR_GC_STRESS")
        .env("GCR_GC_WORKERS", "4")
        .env("GCR_NURSERY_MB", "1")
        .env("GCR_TENURED_MB", "16");
    if stress {
        cmd.env("GCR_GC_STRESS", "1").env("GCR_GC_VERIFY", "1");
    }
    support::run_with_timeout(&mut cmd, 60)
}
fn value(out: Output) -> Value {
    assert!(
        out.status.success(),
        "status {} stderr {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON: {e}: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}
fn error(out: Output, needle: &str) {
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(needle),
        "expected {needle}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}
struct Rng(u64);
impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 32) as usize) % n
    }
}
#[test]
fn parallel_csv_matches_serial_grouping_with_unicode_multiline_and_many_files() {
    let f = Fixture::new("csv");
    let labels = [
        "",
        "coffee",
        "food,λ",
        "say \"hello\"",
        "line\nbreak",
        "café",
        "🦀",
        "z",
    ];
    let mut rng = Rng(19);
    let mut groups: BTreeMap<String, (i64, i64, i64, i64)> = BTreeMap::new();
    let mut total = 0;
    let mut rows = 0;
    let mut bytes = 0;
    let mut paths = Vec::new();
    for file in 0..12 {
        let mut csv = if file == 0 {
            "\u{feff}group,cents,note\r\n".to_owned()
        } else {
            "group,cents,note\r\n".to_owned()
        };
        for _ in 0..(20 + file * 3) {
            let label = labels[rng.next(labels.len())];
            let amount = rng.next(10001) as i64 - 5000;
            csv += &format!(
                "{}, {},{}\r\n",
                csv_field(label),
                amount,
                csv_field("ignored,\"λ\"")
            );
            let g = groups
                .entry(label.to_owned())
                .or_insert((0, 0, amount, amount));
            g.0 += 1;
            g.1 += amount;
            g.2 = g.2.min(amount);
            g.3 = g.3.max(amount);
            total += amount;
            rows += 1;
        }
        bytes += csv.len();
        paths.push(f.write(&format!("part-{file}.csv"), &csv));
    }
    let expected = json!({"files":paths.len(),"rows":rows,"bytes":bytes,"total":total,"groups":groups.into_iter().map(|(key,(count,sum,min,max))|json!({"key":key,"count":count,"sum":sum,"min":min,"max":max})).collect::<Vec<_>>()});
    for (jit, stress, workers) in [
        (false, false, "1"),
        (false, false, "8"),
        (false, true, "4"),
        (true, false, "3"),
        (true, true, "8"),
    ] {
        let mut args = vec![workers, "group", "cents", "-"];
        args.extend(paths.iter().map(String::as_str));
        assert_eq!(value(run(0, &args, jit, stress)), expected);
    }
}
#[derive(Clone)]
struct Task {
    name: String,
    duration: i64,
    deps: Vec<usize>,
}
fn plan_model(tasks: &[Task], workers: usize) -> Value {
    let n = tasks.len();
    let mut done = vec![false; n];
    let mut assigned = vec![false; n];
    let mut busy = vec![None; workers];
    let mut start = vec![0; n];
    let mut end = vec![0; n];
    let mut worker = vec![0; n];
    let mut critical = vec![0; n];
    let mut now = 0;
    while done.iter().any(|&x| !x) {
        let mut ready = (0..n)
            .filter(|&i| !assigned[i] && tasks[i].deps.iter().all(|&d| done[d]))
            .collect::<Vec<_>>();
        ready.sort_by_key(|&i| &tasks[i].name);
        for job in ready {
            if let Some(w) = busy.iter().position(Option::is_none) {
                assigned[job] = true;
                start[job] = now;
                end[job] = now + tasks[job].duration;
                worker[job] = w + 1;
                busy[w] = Some(job);
            }
        }
        now = busy
            .iter()
            .flatten()
            .map(|&i| end[i])
            .min()
            .expect("acyclic model");
        for slot in &mut busy {
            if let Some(i) = *slot {
                if end[i] == now {
                    done[i] = true;
                    critical[i] = tasks[i].duration
                        + tasks[i]
                            .deps
                            .iter()
                            .map(|&d| critical[d])
                            .max()
                            .unwrap_or(0);
                    *slot = None;
                }
            }
        }
    }
    let mut order = (0..n).collect::<Vec<_>>();
    order.sort_by_key(|&i| &tasks[i].name);
    json!({"workers":workers,"makespan":now,"critical_path":critical.into_iter().max().unwrap_or(0),"tasks":order.into_iter().map(|i|json!({"task":tasks[i].name,"start":start[i],"finish":end[i],"worker":worker[i]})).collect::<Vec<_>>()})
}
#[test]
fn scheduler_matches_scan_model_for_random_dags_zero_durations_and_ties() {
    let f = Fixture::new("plan");
    let mut rng = Rng(77);
    for seed in 0..12 {
        let n = 12 + seed;
        let tasks = (0..n)
            .map(|i| Task {
                name: format!("task-{:02}-λ", n - i),
                duration: rng.next(7) as i64,
                deps: (0..i).filter(|_| rng.next(6) == 0).collect(),
            })
            .collect::<Vec<_>>();
        let mut csv = "task,duration,depends\n".to_owned();
        // Reverse order permits forward references and defeats input-order scheduling.
        for task in tasks.iter().rev() {
            csv += &format!(
                "{},{},{}\n",
                csv_field(&task.name),
                task.duration,
                task.deps
                    .iter()
                    .map(|&i| tasks[i].name.as_str())
                    .collect::<Vec<_>>()
                    .join(";")
            );
        }
        let path = f.write("tasks.csv", &csv);
        let workers = 1 + seed % 5;
        let w = workers.to_string();
        let expected = plan_model(&tasks, workers);
        assert_eq!(value(run(1, &[&path, &w, "-"], false, seed < 2)), expected);
        if seed < 2 {
            assert_eq!(value(run(1, &[&path, &w, "-"], true, true)), expected);
        }
    }
    let empty = f.write("empty.csv", "task,duration,depends\n");
    assert_eq!(
        value(run(1, &[&empty, "4", "-"], false, true)),
        json!({"workers":4,"makespan":0,"critical_path":0,"tasks":[]})
    );
}
#[test]
fn routes_match_bellman_ford_and_paths_have_exact_costs() {
    let f = Fixture::new("routes");
    let mut rng = Rng(31);
    for seed in 0..12 {
        let n = 10 + seed;
        let names = (0..n).map(|i| format!("stop-{i},λ")).collect::<Vec<_>>();
        let mut edges = Vec::new();
        // Self loops declare all nodes; random edges include zero costs and duplicates.
        for i in 0..n {
            edges.push((i, i, 0i64));
        }
        for _ in 0..n * 4 {
            edges.push((rng.next(n), rng.next(n), rng.next(30) as i64));
        }
        let mut csv = "from,to,cost\n".to_owned();
        for &(a, b, c) in &edges {
            csv += &format!("{},{},{c}\n", csv_field(&names[a]), csv_field(&names[b]));
        }
        let path = f.write("edges.csv", &csv);
        let from = rng.next(n);
        let to = rng.next(n);
        let mut dist = vec![None; n];
        dist[from] = Some(0i64);
        for _ in 0..n {
            for &(a, b, c) in &edges {
                if let Some(d) = dist[a] {
                    let nd = d + c;
                    if dist[b].is_none_or(|old| nd < old) {
                        dist[b] = Some(nd);
                    }
                }
            }
        }
        let native = value(run(
            2,
            &[&path, &names[from], &names[to], "-"],
            false,
            seed < 2,
        ));
        assert_eq!(native["distance"], json!(dist[to]));
        assert_eq!(native["reachable"], json!(dist[to].is_some()));
        assert_eq!(native["nodes"], json!(n));
        assert_eq!(native["edges"], json!(edges.len()));
        let route = native["path"].as_array().unwrap();
        if let Some(distance) = dist[to] {
            assert_eq!(route.first().unwrap(), &json!(names[from]));
            assert_eq!(route.last().unwrap(), &json!(names[to]));
            let mut cost = 0;
            for step in route.windows(2) {
                let a = names.iter().position(|s| json!(s) == step[0]).unwrap();
                let b = names.iter().position(|s| json!(s) == step[1]).unwrap();
                cost += edges
                    .iter()
                    .filter(|&&(x, y, _)| x == a && y == b)
                    .map(|&(_, _, c)| c)
                    .min()
                    .unwrap();
            }
            assert_eq!(cost, distance);
        } else {
            assert!(route.is_empty());
        }
        if seed < 2 {
            assert_eq!(
                value(run(2, &[&path, &names[from], &names[to], "-"], true, true)),
                native
            );
        }
    }
}
#[test]
fn apps_reject_invalid_input_and_overflow_without_replacing_existing_output() {
    let f = Fixture::new("errors");
    let output = f.write("output.json", "keep me\n");
    let cases = [
        (
            0,
            "group,cents\na,9223372036854775808\n",
            "invalid signed integer",
        ),
        (0, "group,cents\na,9223372036854775807\na,1\n", "overflow"),
        (0, "group,cents\n\"bad,1\n", "unterminated"),
        (0, "group,cents\na,1,extra\n", "field count"),
        (0, "group,group,cents\na,a,1\n", "unique"),
        (1, "task,duration,depends\na,1,b\nb,1,a\nc,1,a\n", "cycle"),
        (
            1,
            "task,duration,depends\na,1,missing\n",
            "unknown dependency",
        ),
        (
            1,
            "task,duration,depends\na,1,\nb,1,a;a\n",
            "duplicate dependency",
        ),
        (1, "task,duration,depends\na,1,a\n", "self dependency"),
        (1, "task,duration,depends\na,-1,\n", "negative duration"),
        (
            1,
            "task,duration,depends\na,9223372036854775807,\nb,1,a\n",
            "overflow",
        ),
        (2, "from,to,cost\na,b,-1\n", "nonnegative"),
        (2, "from,to,cost\na,b,9223372036854775808\n", "invalid cost"),
        (
            2,
            "from,to,cost\na,c,9223372036854775807\nc,b,1\n",
            "exceeds i64",
        ),
    ];
    for (index, (app, csv, needle)) in cases.into_iter().enumerate() {
        let path = f.write("input.csv", csv);
        let args = match app {
            0 => vec!["3", "group", "cents", &output, &path],
            1 => vec![&path, "2", &output],
            _ => vec![&path, "a", "b", &output],
        };
        error(run(app, &args, false, index % 3 == 0), needle);
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "keep me\n");
        if index == 0 || index == 5 || index == 13 {
            error(run(app, &args, true, true), needle);
        }
    }
    for app in 0..3 {
        let out = run(app, &[], false, false);
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stdout.is_empty());
    }
    let path = f.write(
        "edges.csv",
        "from,to,cost\na,c,9223372036854775807\nc,b,1\na,b,7\n",
    );
    assert_eq!(
        value(run(2, &[&path, "a", "b", "-"], false, true))["distance"],
        json!(7)
    );
    let path = f.write("max.csv", "from,to,cost\na,b,9223372036854775807\n");
    assert_eq!(
        value(run(2, &[&path, "a", "b", "-"], true, true))["distance"],
        json!(i64::MAX)
    );
    let path = f.write("min.csv", "group,cents\na,-9223372036854775808\n");
    assert_eq!(
        value(run(0, &["1", "group", "cents", "-", &path], false, true))["total"],
        json!(i64::MIN)
    );
}
#[test]
fn atomic_output_and_empty_reports_are_usable() {
    let f = Fixture::new("output");
    let csv = f.write("data.csv", "group,cents\n");
    let output = f.write("report.json", "old");
    let out = run(0, &["8", "group", "cents", &output, &csv], false, true);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
    assert_eq!(
        serde_json::from_str::<Value>(&std::fs::read_to_string(&output).unwrap()).unwrap(),
        json!({"files":1,"rows":0,"bytes":12,"total":0,"groups":[]})
    );
    let bad_output = f.0.join("absent/report.json");
    error(
        run(
            0,
            &["1", "group", "cents", bad_output.to_str().unwrap(), &csv],
            false,
            false,
        ),
        "No such",
    );
}
fn exercise_fixture(name: &str, source: &str, stress_mb: &str) {
    let f = Fixture::new(name);
    f.write(
        "gcr.toml",
        "[package]\nname = \"app-regression\"\nversion = \"0.1.0\"\nentry = \"main.gcr\"\n",
    );
    f.write("main.gcr", source);
    let bin = f.0.join("app-regression");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gcr"));
    cmd.args(["build"])
        .arg(&f.0)
        .arg("-o")
        .arg(&bin)
        .env("GCRUST_RUNTIME_LIB", support::runtime_staticlib());
    let out = support::run_with_timeout(&mut cmd, 60);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for jit in [false, true] {
        for stress in [false, true] {
            let mut cmd = if jit {
                let mut c = Command::new(env!("CARGO_BIN_EXE_gcr"));
                c.arg("run").arg(&f.0).arg("--jit");
                c
            } else {
                Command::new(&bin)
            };
            cmd.env_remove("GCR_GC_STRESS")
                .env("GCR_GC_WORKERS", "4")
                .env("GCR_GC_VERIFY", "1")
                .env("GCR_STRESS_HEAP_MB", stress_mb);
            if stress {
                cmd.env("GCR_GC_STRESS", "1");
            }
            let out = support::run_with_timeout(&mut cmd, 60);
            assert!(
                out.status.success(),
                "jit={jit} stress={stress}: {} {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.stdout.is_empty());
        }
    }
}
#[test]
fn nested_payload_regression_runs_native_and_jit_with_moving_gc() {
    exercise_fixture("payloads", include_str!("fixtures/app_payloads.gcr"), "8");
}
#[test]
fn larger_inputs_match_models_with_small_nursery() {
    let f = Fixture::new("larger");
    let mut groups: BTreeMap<String, (i64, i64, i64, i64)> = BTreeMap::new();
    let mut total = 0;
    let mut bytes = 0;
    let mut paths = Vec::new();
    for part in 0..8 {
        let mut csv = "group,cents\n".to_owned();
        for i in 0..600 {
            let key = format!("group-{:03}-λ", (i * 37 + part) % 173);
            let amount = (i % 91) as i64 - 45;
            csv += &format!("{key},{amount}\n");
            let g = groups.entry(key).or_insert((0, 0, amount, amount));
            g.0 += 1;
            g.1 += amount;
            g.2 = g.2.min(amount);
            g.3 = g.3.max(amount);
            total += amount;
        }
        bytes += csv.len();
        paths.push(f.write(&format!("part-{part}.csv"), &csv));
    }
    let expected = json!({"files":8,"rows":4800,"bytes":bytes,"total":total,"groups":groups.into_iter().map(|(key,(count,sum,min,max))|json!({"key":key,"count":count,"sum":sum,"min":min,"max":max})).collect::<Vec<_>>()});
    let mut args = vec!["8", "group", "cents", "-"];
    args.extend(paths.iter().map(String::as_str));
    assert_eq!(value(run(0, &args, false, false)), expected);
    let mut rng = Rng(998);
    let n = 512;
    let tasks = (0..n)
        .map(|i| Task {
            name: format!("job-{:04}", n - i),
            duration: rng.next(20) as i64,
            deps: if i == 0 {
                vec![]
            } else {
                let mut d = vec![i - 1];
                if i > 5 {
                    let other = rng.next(i - 1);
                    d.push(other);
                }
                d
            },
        })
        .collect::<Vec<_>>();
    let mut csv = "task,duration,depends\n".to_owned();
    for task in tasks.iter().rev() {
        csv += &format!(
            "{},{},{}\n",
            task.name,
            task.duration,
            task.deps
                .iter()
                .map(|&i| tasks[i].name.as_str())
                .collect::<Vec<_>>()
                .join(";")
        );
    }
    let path = f.write("pipeline.csv", &csv);
    assert_eq!(
        value(run(1, &[&path, "8", "-"], false, false)),
        plan_model(&tasks, 8)
    );
    let mut csv = "from,to,cost\n".to_owned();
    for i in 0..511 {
        csv += &format!("n{i},n{},1\n", i + 1);
    }
    for _ in 0..3000 {
        let a = rng.next(512);
        let b = rng.next(512);
        csv += &format!("n{a},n{b},1000\n");
    }
    let path = f.write("network.csv", &csv);
    let out = value(run(2, &[&path, "n0", "n511", "-"], false, false));
    assert_eq!(out["distance"], json!(511));
    assert_eq!(
        out["path"],
        json!((0..512).map(|i| format!("n{i}")).collect::<Vec<_>>())
    );
}
#[test]
fn csv_byte_limits_and_error_positions_run_native_and_jit() {
    exercise_fixture("csv-limits", include_str!("fixtures/csv_limits.gcr"), "64");
}
