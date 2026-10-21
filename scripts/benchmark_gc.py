#!/usr/bin/env python3
"""Run isolated correctness-checked GC microbenchmarks and retain raw evidence.

Each copy run measures nine collections (first two warmup). Allocation runs
measure five samples of 250,000 objects per mutator, with/without TLABs. The
fixed 64 MiB allocation workload supports up to eight mutators. Run without
other builds/tests; these microbenchmarks do not establish application budgets.
"""
import argparse
import ast
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parents[1]
CASES = {
    "copy-small": "gc::heap::parallel::tests::parallel_small_object_throughput",
    "copy-payload": "gc::heap::parallel::tests::parallel_copy_throughput",
    "allocation": "gc::tests::mutator_tlab_allocation_throughput",
    "aggregate-idle": "runtime::managed::tests::aggregate_idle_access_throughput",
}


def output(*command):
    return subprocess.check_output(command, cwd=ROOT, text=True).strip()


def source_hashes():
    paths = list((ROOT / "crates/gcrust-rt").rglob("*.rs"))
    paths += [ROOT / "crates/gcrust-rt/Cargo.toml", ROOT / "Cargo.toml",
              ROOT / "Cargo.lock", Path(__file__).resolve()]
    return {str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(paths)}


def save(path, report):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(report, indent=2) + "\n")
    temporary.replace(path)


def host_state():
    """Observe outside timed workloads; retain contention evidence, not argv."""
    state = {"observed_utc": datetime.datetime.now(datetime.timezone.utc).isoformat()}
    if hasattr(os, "getloadavg"):
        state["load_average"] = list(os.getloadavg())
    try:
        snapshot = output("ps", "-axo", "pid=,ppid=,pcpu=,comm=")
        processes = []
        for line in snapshot.splitlines():
            fields = line.split(None, 3)
            if len(fields) != 4:
                continue
            pid, parent, cpu, command = fields
            if int(pid) != os.getpid():
                processes.append({"pid": int(pid), "parent_pid": int(parent),
                                  "cpu_percent": float(cpu), "command": Path(command).name})
        state["top_cpu_processes"] = sorted(processes, key=lambda p: p["cpu_percent"], reverse=True)[:8]
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        state["sampling_error"] = str(error)
    return state


def measurements(case, workers, text):
    rows = []
    if case == "aggregate-idle":
        pattern = r"aggregate benchmark: workers=(\d+) operations=(\d+) median_ns=(\d+) raw_ns=(\[[^\n]+\])"
    else:
        pattern = (r"copy benchmark: workers=(\d+) live_bytes=(\d+) median_pause_ns=(\d+) raw_pause_ns=(\[[^\n]+\])"
               if case != "allocation" else
               r"allocation benchmark: workers=(\d+) tlab=(true|false) objects=(\d+) median_ns=(\d+) raw_ns=(\[[^\n]+\])")
    for match in re.finditer(pattern, text):
        fields = match.groups()
        if int(fields[0]) != workers:
            raise RuntimeError("benchmark worker count differs from requested count")
        allocation = case == "allocation"
        samples = ast.literal_eval(fields[-1])
        warmup = 0 if allocation else 2
        count = 5 if allocation else 9
        if len(samples) != count or any(type(n) is not int or n <= 0 for n in samples):
            raise RuntimeError("benchmark did not emit the expected positive timing samples")
        median = int(fields[-2])
        if median != statistics.median(samples[warmup:]):
            raise RuntimeError("reported median does not match raw samples")
        row = {"case": case, "workers": workers, "samples_ns": samples,
               "warmup_samples": warmup, "median_ns": median}
        if allocation:
            row.update(tlab=fields[1] == "true", objects=int(fields[2]))
        elif case == "aggregate-idle":
            row["operations"] = int(fields[1])
            if row["operations"] != 250_000:
                raise RuntimeError("aggregate workload operation count changed")
        else:
            row["live_bytes"] = int(fields[1])
        rows.append(row)
    if len(rows) != (2 if case == "allocation" else 1):
        raise RuntimeError("missing or duplicate benchmark measurements; inspect the raw log")
    if case == "allocation" and {row["tlab"] for row in rows} != {False, True}:
        raise RuntimeError("allocation run must include both shared cursor and TLAB")
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--workers", type=int, nargs="+", default=[1, 2, 4, 8])
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--cases", choices=list(CASES), nargs="+",
                        default=["copy-small", "copy-payload", "allocation"])
    args = parser.parse_args()
    if len(set(args.cases)) != len(args.cases):
        parser.error("benchmark cases must be distinct")
    if "aggregate-idle" in args.cases and args.workers != [1]:
        parser.error("aggregate-idle measures one mutator; use --workers 1")
    if args.trials < 1 or len(set(args.workers)) != len(args.workers) or any(n < 1 or n > 8 for n in args.workers):
        parser.error("use positive trials and distinct worker counts between 1 and 8")
    destination = args.output.resolve()
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists():
        parser.error("output already exists; choose a new artifact path")
    logs = destination.parent / (destination.stem + "-logs")
    logs.mkdir(exist_ok=False)
    env = os.environ.copy()
    for key in ["GCR_GC_STRESS", "GCR_GC_VERIFY", "GCR_GC_STATS", "GCR_GC_LOG",
                "GCRUST_TRACEGC", "GCRUST_PREWALK", "GCRUST_POISON", "GCR_CODEGEN_TSAN",
                "GCR_GC_PROFILE_COLLECTIONS"]:
        env.pop(key, None)
    report = {"schema": 2, "started_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "status": "running", "host": platform.platform(), "machine": platform.machine(),
              "logical_cpus": os.cpu_count(),
              "cpu": output("sysctl", "-n", "machdep.cpu.brand_string") if platform.system() == "Darwin" else platform.processor(),
              "rustc": output("rustc", "-Vv"),
              "cargo": output("cargo", "-V"), "revision": output("git", "rev-parse", "HEAD"),
              "worktree_status": output("git", "status", "--short"),
              "tracked_diff_sha256": hashlib.sha256(subprocess.check_output(["git", "diff", "--binary"], cwd=ROOT)).hexdigest(),
              "source_sha256": source_hashes(),
              "build_environment": {key: value for key, value in env.items()
                                    if key.startswith("CARGO_PROFILE_") or key in
                                    ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTUP_TOOLCHAIN", "CARGO_BUILD_TARGET", "CARGO_TARGET_DIR"]},
              "workers": args.workers, "trials": args.trials, "cases": args.cases,
              "scope": "Local microbenchmarks with object-count correctness checks; no application latency or RSS budget claim.",
              "timing_scope": {"copy": "GC event pause only; first two of nine events excluded from median",
                               "allocation": "barrier release through worker joins, including retirement; five samples",
                               "aggregate-idle": "250,000 two-word snapshot updates via runtime ABI; first two of nine samples excluded"},
              "runs": []}
    archive = logs / "sources.tar.gz"
    with tarfile.open(archive, "w:gz") as snapshot:
        for relative in report["source_sha256"]:
            snapshot.add(ROOT / relative, arcname=relative, recursive=False)
    report["source_archive"] = {"path": str(archive), "sha256": hashlib.sha256(archive.read_bytes()).hexdigest()}
    save(destination, report)
    try:
        for trial in range(args.trials):
            # Rotate worker order to reduce consistently favoring one count.
            order = args.workers[trial % len(args.workers):] + args.workers[:trial % len(args.workers)]
            for workers in order:
                for case in args.cases:
                    test = CASES[case]
                    command = ["cargo", "test", "--release", "-p", "gcrust-rt", test, "--",
                               "--exact", "--include-ignored", "--nocapture", "--test-threads=1"]
                    log = logs / f"trial-{trial + 1}-{case}-{workers}.log"
                    run_env = dict(env, GCR_GC_WORKERS=str(workers), GCR_ALLOC_BENCH_WORKERS=str(workers))
                    context_before = host_state()
                    with log.open("w") as stream:
                        result = subprocess.run(command, cwd=ROOT, env=run_env, stdout=stream,
                                                stderr=subprocess.STDOUT, timeout=300)
                    context_after = host_state()
                    if result.returncode:
                        raise RuntimeError(f"benchmark failed ({result.returncode}); see {log}")
                    if source_hashes() != report["source_sha256"] or output("git", "rev-parse", "HEAD") != report["revision"]:
                        raise RuntimeError("sources changed during measurement; results are not a consistent baseline")
                    rows = measurements(case, workers, log.read_text())
                    report["runs"].append({"trial": trial + 1, "case": case, "workers": workers,
                                           "command": command, "log": str(log), "measurements": rows,
                                           "host_before": context_before, "host_after": context_after})
                    save(destination, report)
                    print(f"trial {trial + 1}: {case}, {workers} workers: " +
                          ", ".join(str(row["median_ns"]) + " ns" for row in rows), flush=True)
        report["status"] = "complete"
    except BaseException as error:
        report.update(status="interrupted" if isinstance(error, KeyboardInterrupt) else "failed", error=str(error))
        raise
    finally:
        report["finished_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        save(destination, report)
    print(destination, flush=True)


if __name__ == "__main__":
    main()
