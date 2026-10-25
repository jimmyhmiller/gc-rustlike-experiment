#!/usr/bin/env python3
"""Compare Rust funct and the gc-rust port in AOT/JIT batch processes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import re
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
BUILD = ROOT / 'target/performance-comparison'
PORT = ROOT / 'ports/funct'
GCR = ROOT / 'target/release/gcr'
RUST = PORT / 'reference/target/release/funct-reference'


def run(cmd, **kwargs):
    return subprocess.run(list(map(str, cmd)), capture_output=True, text=True,
                          check=True, timeout=1800, **kwargs)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--skip-build', action='store_true', help='reuse already built reference and AOT binaries')
    parser.add_argument('--warmup', type=int, default=1)
    parser.add_argument('--samples', type=int, default=5)
    parser.add_argument('--forks', type=int, default=3)
    parser.add_argument('--output', type=Path, default=HERE / 'jit-results.json')
    args = parser.parse_args()
    if args.warmup < 0 or min(args.samples, args.forks) < 1:
        parser.error('warmup must be nonnegative; samples and forks must be positive')
    cases = [(name, 'run', HERE / 'workloads' / (name + '.ft'))
             for name in ('fib', 'loops', 'collatz', 'list', 'startup')]
    cases += [('demo', 'run', PORT / 'examples/demo.ft'),
              ('tests', 'test', PORT / 'examples'),
              ('lex-chess', 'lex', PORT / 'examples/widgets/chess.ft'),
              ('parse-chess', 'parse', PORT / 'examples/widgets/chess.ft')]
    env = {k: v for k, v in os.environ.items() if not k.startswith(('GCR_', 'GCRUST_'))}
    env.update(GCR_GC_WORKERS='1', GCR_NURSERY_MB='16', GCR_TENURED_MB='256')
    BUILD.mkdir(parents=True, exist_ok=True)
    aot = BUILD / 'funct-batch-aot'
    if args.skip_build:
        if not all(path.is_file() for path in (GCR, RUST, aot)):
            parser.error('--skip-build requires existing compiler, reference, and AOT binaries')
    else:
        print('Building release Rust reference and AOT funct port', flush=True)
        run(['cargo', 'build', '--release', '--manifest-path', PORT / 'reference/Cargo.toml'])
        run([GCR, 'build', PORT, '-o', aot], env=env)
    tail = ['bench', str(args.warmup), str(args.samples)]
    for _, mode, path in cases:
        tail += [mode, str(path)]
    commands = {'rust': [str(RUST)] + tail, 'aot': [str(aot)] + tail,
                'jit': [str(GCR), 'run', str(PORT), '--jit']
                       + ['--'] + tail}
    funct = (PORT / 'reference/../../../../../funct').resolve()
    sources = [HERE / 'run.py'] + list(HERE.glob('workloads/*.ft'))
    sources += list((PORT / 'src').glob('*.gcr')) + list((PORT / 'reference/src').glob('*.rs'))
    sources += list((PORT / 'examples').rglob('*.ft')) + list((funct / 'src').rglob('*.rs'))
    data = {'environment': {'platform': platform.platform(),
            'git': run(['git', 'rev-parse', 'HEAD'], cwd=ROOT).stdout.strip(),
            'git_dirty': bool(run(['git', 'status', '--porcelain'], cwd=ROOT).stdout.strip()),
            'rust_version': run(['rustc', '--version']).stdout.strip(),
            'funct_git': run(['git', 'rev-parse', 'HEAD'], cwd=funct).stdout.strip(),
            'funct_dirty': bool(run(['git', 'status', '--porcelain'], cwd=funct).stdout.strip()),
            'sha256': {str(p): digest(p) for p in sorted(set(sources))},
            'binaries_sha256': {str(p): digest(p) for p in (GCR, RUST, aot)},
            'runtime_environment': {k: v for k, v in env.items() if k.startswith('GCR_')},
            'compilation': 'eager O3', 'reused_builds': args.skip_build,
            'warmup': args.warmup, 'samples': args.samples, 'forks': args.forks,
            'order_seed': 20261023,
            'scope': 'Fresh VM per eval; file IO, output and Rust drop included. JIT compilation excluded from workload intervals; included in process wall time.'},
            'cases': [name for name, _, _ in cases], 'processes': []}
    reference = {}
    rng = random.Random(20261023)
    for fork in range(args.forks):
        languages = list(commands)
        rng.shuffle(languages)
        for language in languages:
            print(f'Running {language} fork {fork}', flush=True)
            metrics = BUILD / f'funct-{language}-{fork}-gc.json'
            metrics.unlink(missing_ok=True)
            local = dict(env)
            if language != 'rust':
                local['GCR_METRICS_FILE'] = str(metrics)
            start = time.perf_counter()
            proc = run(['/usr/bin/time', '-l'] + commands[language], env=local)
            wall = time.perf_counter() - start
            chunks = re.split(r'@sample (\d+) (\d+) (\d+)\n', proc.stdout)
            expected = [(c, i) for c in range(len(cases))
                        for i in range(args.warmup + args.samples)]
            if len(chunks) != len(expected) * 4 + 1 or chunks[-1].strip():
                raise RuntimeError(f'{language}: invalid sample count or trailing output: {proc.stdout[-1000:]}')
            records = []
            for offset, (case, index) in enumerate(expected):
                output, actual_case, actual_index, ns = chunks[offset*4:offset*4+4]
                if (int(actual_case), int(actual_index)) != (case, index) or int(ns) <= 0 or not output.strip():
                    raise RuntimeError(f'{language}: invalid sample {case}/{index}')
                if case not in reference:
                    reference[case] = output
                if output != reference[case]:
                    raise RuntimeError(f'{language}/{cases[case][0]} output mismatch:\n{output}\nexpected:\n{reference[case]}')
                records.append({'case': cases[case][0], 'index': index, 'ns': int(ns), 'output': output})
            rss = re.search(r'(\d+)\s+maximum resident set size', proc.stderr)
            if not rss:
                raise RuntimeError('missing macOS RSS')
            record = {'language': language, 'fork': fork, 'command': commands[language],
                      'wall_s': wall, 'outside_workload_s': wall - sum(r['ns'] for r in records)/1e9,
                      'peak_rss_bytes': int(rss[1]), 'records': records, 'stderr': proc.stderr}
            if language != 'rust':
                record['gc'] = json.loads(metrics.read_text())
            data['processes'].append(record)
            args.output.write_text(json.dumps(data, indent=2) + '\n')
            print(f'{language}: process {wall:.3f}s; outside workload {record["outside_workload_s"]:.3f}s', flush=True)
    data['median_ms'] = {case: {language: statistics.median(
        r['ns']/1e6 for process in data['processes'] if process['language'] == language
        for r in process['records'] if r['case'] == case and r['index'] >= args.warmup)
        for language in commands} for case, _, _ in cases}
    args.output.write_text(json.dumps(data, indent=2) + '\n')
    print(json.dumps(data['median_ms'], indent=2))


if __name__ == '__main__':
    main()
