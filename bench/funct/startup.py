#!/usr/bin/env python3
"""Measure fresh-process funct JIT startup, including the first script."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runs', type=int, default=5)
    parser.add_argument('--output', type=Path, default=Path(__file__).with_name('startup-results.json'))
    args = parser.parse_args()
    if args.runs < 1:
        parser.error('--runs must be positive')
    binary = ROOT / 'target/release/gcr'
    command = [str(binary), 'run', 'ports/funct', '--jit', '--',
               'run', 'bench/funct/workloads/startup.ft']
    environment = {k: v for k, v in os.environ.items() if not k.startswith(('GCR_', 'GCRUST_'))}
    records = []
    for _ in range(args.runs):
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        start = time.perf_counter()
        result = subprocess.run(command, cwd=ROOT, env=environment, capture_output=True,
                                text=True, check=True, timeout=120)
        wall = time.perf_counter() - start
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        if result.stdout != '1\n':
            raise RuntimeError(f'Unexpected script output: {result.stdout!r}; {result.stderr}')
        record = {'wall_s': wall,
                  'cpu_s': after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime,
                  'stdout': result.stdout, 'stderr': result.stderr}
        records.append(record)
        print(f"wall {wall:.3f}s; CPU {record['cpu_s']:.3f}s", flush=True)
    data = {'command': command, 'platform': platform.platform(),
            'scope': 'Fresh processes; no compiler cache. Includes source reading, checking, '
                     'lowering, full eager O3 native code generation, runtime setup, first script and shutdown. '
                     'All reachable native functions compile before user entry.',
            'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
            'git': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
            'git_dirty': bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=ROOT)),
            'runs': records, 'median_wall_s': statistics.median(r['wall_s'] for r in records),
            'max_wall_s': max(r['wall_s'] for r in records)}
    args.output.write_text(json.dumps(data, indent=2) + '\n')
    print(f"Median {data['median_wall_s']:.3f}s; maximum {data['max_wall_s']:.3f}s")


if __name__ == '__main__':
    main()
