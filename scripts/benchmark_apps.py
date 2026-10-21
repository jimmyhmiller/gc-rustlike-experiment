#!/usr/bin/env python3
"""Correctness-checked native application measurements; see docs/GC_PERFORMANCE.md.

Builds three applications once, then runs fresh processes with telemetry enabled.
OS filesystem caches are warmed; this does not measure cold-machine startup.
Reports raw observations, not universal budgets or pooled pause quantiles.
"""
import argparse
import csv
import datetime
import hashlib
import io
import json
import math
import os
from pathlib import Path
import platform
import signal
import statistics
import subprocess
import tarfile
import time

from benchmark_gc import host_state, save

ROOT = Path(__file__).resolve().parents[1]
APPS = ('gcr-csvreport', 'gcr-buildplan', 'gcr-routes')
METRICS = ('gc_minor', 'gc_major', 'gc_pause_max_ms', 'gc_pause_total_ms',
           'gc_elapsed_total_ms', 'gc_pause_p50_ms', 'gc_pause_p99_ms',
           'gc_pause_quantile_error_ratio', 'alloc_objects', 'alloc_bytes',
           'peak_heap_bytes', 'native_buffer_bytes', 'native_buffer_peak_bytes',
           'native_pressure_collections')


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def sources():
    paths = []
    for directory in ('src', 'crates/gcrust-rt', 'apps'):
        paths.extend(p for p in (ROOT / directory).rglob('*')
                     if p.is_file() and p.suffix in ('.rs', '.gcr', '.toml'))
    paths += [ROOT / p for p in ('Cargo.toml', 'Cargo.lock',
                                 'scripts/benchmark_apps.py', 'scripts/test_benchmark_apps.py', 'scripts/benchmark_gc.py')]
    paths.extend(ROOT / name for name in ('build.rs', 'rust-toolchain.toml') if (ROOT / name).is_file())
    return {str(p.relative_to(ROOT)): digest(p) for p in sorted(paths)}


def execute(command, env, timeout):
    """A timeout terminates this invocation's entire process group and reaps it."""
    process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except BaseException:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.communicate()
        raise
    if process.returncode != 0:
        raise RuntimeError(f'command failed ({process.returncode}): {command!r}\n'
                           + stderr.decode(errors='replace'))
    return stdout, stderr


def csv_text(rows):
    stream = io.StringIO(newline='')
    writer = csv.writer(stream, lineterminator='\n')
    writer.writerows(rows)
    return stream.getvalue()


def plan_model(tasks, workers):
    done, assigned = set(), set()
    busy = {}
    result, critical = {}, {}
    now = 0
    while len(done) < len(tasks):
        ready = sorted(name for name, (_, deps) in tasks.items()
                       if name not in assigned and all(d in done for d in deps))
        for name in ready:
            free = next((w for w in range(1, workers + 1) if w not in busy), None)
            if free is None:
                break
            duration, _ = tasks[name]
            result[name] = dict(task=name, start=now, finish=now + duration, worker=free)
            busy[free] = name
            assigned.add(name)
        if not busy:
            raise ValueError('model input contains a cycle')
        now = min(result[name]['finish'] for name in busy.values())
        for worker, name in list(busy.items()):
            if result[name]['finish'] == now:
                duration, deps = tasks[name]
                critical[name] = duration + max((critical[d] for d in deps), default=0)
                done.add(name)
                del busy[worker]
    return dict(workers=workers, makespan=now, critical_path=max(critical.values(), default=0),
                tasks=[result[name] for name in sorted(result)])


def fixtures(directory, rows_per_file):
    directory.mkdir()
    def write(name, text):
        path = directory / name
        path.write_bytes(text.encode('utf-8'))
        return str(path)
    empty = write('empty.csv', 'group,cents\n')
    cases = [dict(name='csv-startup', app=APPS[0], args=['1', 'group', 'cents', '-', empty],
                  expected=dict(files=1, rows=0, bytes=12, total=0, groups=[]))]
    groups, paths, total, size = {}, [], 0, 0
    for part in range(8):
        rows = [('group', 'cents')]
        for index in range(rows_per_file):
            key, amount = f'group-{(index * 37 + part) % 173:03}-λ', index % 91 - 45
            rows.append((key, amount))
            group = groups.setdefault(key, dict(key=key, count=0, sum=0, min=amount, max=amount))
            group.update(count=group['count'] + 1, sum=group['sum'] + amount,
                         min=min(group['min'], amount), max=max(group['max'], amount))
            total += amount
        text = csv_text(rows)
        if len(text.encode()) > 512 * 1024:
            raise ValueError('generated CSV exceeds application byte limit')
        paths.append(write(f'part-{part}.csv', text))
        size += len(text.encode())
    expected = dict(files=8, rows=8 * rows_per_file, bytes=size, total=total,
                    groups=[groups[key] for key in sorted(groups)])
    for workers in (1, 4):
        cases.append(dict(name=f'csv-report-{workers}', app=APPS[0],
                          args=[str(workers), 'group', 'cents', '-', *paths], expected=expected))
    tasks = {f'job-{i:04}': (i % 13, [] if i < 8 else [f'job-{i - 8:04}']) for i in range(1600)}
    text = csv_text([('task', 'duration', 'depends')]
                    + [(name, duration, ';'.join(deps)) for name, (duration, deps) in reversed(tasks.items())])
    path = write('pipeline.csv', text)
    cases.append(dict(name='buildplan', app=APPS[1], args=[path, '4', '-'], expected=plan_model(tasks, 4)))
    node_count = 512
    source, target = 'n0', f'n{node_count - 1}'
    # Every extra edge costs more than the entire unit-cost chain, so its
    # unique cheapest path is known independently of the app's Dijkstra code.
    edges = [(f'n{i}', f'n{i+1}', 1) for i in range(node_count - 1)]
    edges += [(f'n{(i * 37) % node_count}', f'n{(i * 61 + 9) % node_count}', node_count + 1000)
              for i in range(3000)]
    path = write('network.csv', csv_text([('from', 'to', 'cost'), *edges]))
    cases.append(dict(name='routes', app=APPS[2], args=[path, source, target, '-'],
                      expected={'from': source, 'to': target, 'reachable': True, 'distance': node_count - 1,
                                'path': [f'n{i}' for i in range(node_count)],
                                'nodes': node_count, 'edges': len(edges)}))

    return cases


def rss_bytes(text, system):
    if system == 'Darwin':
        for line in text.splitlines():
            if line.strip().endswith('maximum resident set size'):
                return int(line.split()[0])
    elif system == 'Linux':
        for line in text.splitlines():
            if 'Maximum resident set size (kbytes):' in line:
                return int(line.rsplit(':', 1)[1]) * 1024
    raise ValueError('missing per-process peak RSS from external time')


def validate_result(stdout, expected):
    if json.loads(stdout) != expected:
        raise ValueError('application result differs from independent model')


def validate_metrics(value):
    for key in METRICS:
        number = value.get(key)
        if type(number) not in (int, float) or not math.isfinite(number) or number < 0:
            raise ValueError(f'invalid or missing telemetry field {key}')
    for key in ('gc_minor', 'gc_major', 'alloc_objects', 'alloc_bytes', 'peak_heap_bytes',
                'native_buffer_bytes', 'native_buffer_peak_bytes', 'native_pressure_collections'):
        if type(value[key]) is not int:
            raise ValueError(f'noninteger counter {key}')
    if value['gc_pause_quantile_error_ratio'] > 1:
        raise ValueError('invalid quantile error bound')
    if value['gc_pause_p50_ms'] > value['gc_pause_p99_ms']:
        raise ValueError('pause quantiles are out of order')
    if value['native_buffer_bytes'] > value['native_buffer_peak_bytes']:
        raise ValueError('retained native buffers exceed recorded peak')


def trial_order(configs, variants, trial):
    # Keep original configuration indices: using rotated positions here would
    # cancel trial parity and consistently favor one variant for even counts.
    indexed = list(enumerate(configs))
    rotate = trial % len(indexed)
    for index, config in indexed[rotate:] + indexed[:rotate]:
        order = list(variants)
        if (trial + index) % 2:
            order.reverse()
        yield config, order


def worker_policy(value):
    if value == 'auto':
        return value
    try:
        number = int(value)
        if number > 0:
            return number
    except ValueError:
        pass
    raise argparse.ArgumentTypeError('worker policy must be auto or a positive integer')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--gcr', type=Path, required=True)
    parser.add_argument('--runtime-lib', type=Path, required=True)
    parser.add_argument('--baseline', type=Path, help='completed report whose saved binaries are compared on the same inputs')
    parser.add_argument('--output', type=Path, required=True, help='new evidence directory')
    parser.add_argument('--runs', type=int, default=7)
    parser.add_argument('--rows-per-file', type=int, default=6000)
    parser.add_argument('--gc-workers', type=worker_policy, nargs='+', default=['auto', 1, 4])
    parser.add_argument('--nursery-mb', type=int, nargs='+', default=[1, 16])
    parser.add_argument('--timeout', type=float, default=90)
    args = parser.parse_args()
    if args.runs < 1 or not 1 <= args.rows_per_file <= 20000 or not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error('runs/timeout must be positive; rows-per-file must be 1..20000')
    if any(n < 1 for n in args.nursery_mb):
        parser.error('worker counts and nursery sizes must be positive')
    system = platform.system()
    if system not in ('Darwin', 'Linux') or not Path('/usr/bin/time').is_file():
        parser.error('per-process RSS currently requires macOS or Linux /usr/bin/time')
    args.gcr, args.runtime_lib, destination = args.gcr.resolve(), args.runtime_lib.resolve(), args.output.resolve()
    if not args.gcr.is_file() or not args.runtime_lib.is_file():
        parser.error('compiler and runtime archive must exist')
    destination.mkdir(parents=True, exist_ok=False)
    report_path = destination / 'report.json'
    report = dict(schema='gcr-app-bench/1', status='running', started_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  platform=platform.platform(), logical_cpus=os.cpu_count(), configuration=vars(args) | {'output': str(destination)},
                  source_sha256=sources(), compiler_sha256=digest(args.gcr), runtime_sha256=digest(args.runtime_lib),
                  builds=[], runs=[], scope='Fresh-process wall time with telemetry and external time wrapper; warm filesystem caches. RSS is per-process OS peak. Pause quantiles are per-run runtime upper estimates, not pooled quantiles.')
    report['configuration'] = {k: str(v) if isinstance(v, Path) else v for k, v in report['configuration'].items()}
    save(report_path, report)
    try:
        archive = destination / 'sources.tar.gz'
        with tarfile.open(archive, 'w:gz') as snapshot:
            for relative in report['source_sha256']:
                snapshot.add(ROOT / relative, arcname=relative, recursive=False)
        report['source_archive_sha256'] = digest(archive)
        binaries = destination / 'binaries'
        binaries.mkdir()
        env = {k: v for k, v in os.environ.items() if not k.startswith(('GCR_', 'GCRUST_'))}
        env['LC_ALL'] = 'C'
        for app in APPS:
            binary = binaries / app
            started = time.perf_counter_ns()
            stdout, stderr = execute([str(args.gcr), 'build', str(ROOT / 'apps' / app), '-o', str(binary)],
                                     env | {'GCRUST_RUNTIME_LIB': str(args.runtime_lib)}, args.timeout)
            (destination / f'{app}-build.log').write_bytes(stdout + stderr)
            report['builds'].append(dict(app=app, milliseconds=(time.perf_counter_ns() - started) / 1e6,
                                         binary=str(binary), sha256=digest(binary), bytes=binary.stat().st_size))
            save(report_path, report)
        variants = {'current': {item['app']: item for item in report['builds']}}
        if args.baseline:
            baseline_path = args.baseline.resolve()
            baseline = json.loads(baseline_path.read_text())
            if baseline.get('schema') != 'gcr-app-bench/1' or baseline.get('status') != 'complete':
                raise ValueError('baseline must be a completed application report')
            if baseline['compiler_sha256'] != report['compiler_sha256']:
                raise ValueError('GC comparison requires the identical compiler binary')
            app_sources = {k: v for k, v in report['source_sha256'].items() if k.startswith('apps/')}
            if app_sources != {k: v for k, v in baseline['source_sha256'].items() if k.startswith('apps/')}:
                raise ValueError('baseline application sources differ')
            variants['baseline'] = {item['app']: item for item in baseline['builds']}
            if set(variants['baseline']) != set(APPS):
                raise ValueError('baseline does not contain all applications')
            for item in variants['baseline'].values():
                if digest(item['binary']) != item['sha256']:
                    raise ValueError('baseline application binary changed')
            report['baseline'] = dict(report=str(baseline_path), sha256=digest(baseline_path),
                                      builds=baseline['builds'], runtime_sha256=baseline['runtime_sha256'])
        cases = fixtures(destination / 'fixtures', args.rows_per_file)
        report['fixtures'] = {p.name: digest(p) for p in sorted((destination / 'fixtures').iterdir())}
        report['expected'] = {case['name']: case['expected'] for case in cases}
        logs = destination / 'runs'
        logs.mkdir()
        for trial in range(args.runs + 1):
            # Rotate case/configuration order across trials; all runs are serial.
            configs = [(c, w, n) for c in cases for w in args.gc_workers for n in args.nursery_mb]
            for (case, workers, nursery), order in trial_order(configs, variants, trial):
                for variant in order:
                    key = f'{variant}-{case["name"]}-gc{workers}-nursery{nursery}-trial{trial}'
                    metrics, usage = logs / (key + '.metrics.json'), logs / (key + '.time.txt')
                    command = ['/usr/bin/time', '-l' if system == 'Darwin' else '-v', '-o', str(usage),
                               str(Path(variants[variant][case['app']]['binary'])), *case['args']]
                    context = host_state()
                    report['active_run'] = dict(variant=variant, case=case['name'], gc_workers=workers,
                        nursery_mb=nursery, trial=trial, command=command, host_before=context)
                    save(report_path, report)
                    started = time.perf_counter_ns()
                    run_env = env | {'GCR_NURSERY_MB': str(nursery), 'GCR_TENURED_MB': '256', 'GCR_METRICS_FILE': str(metrics)}
                    if workers != 'auto':
                        run_env['GCR_GC_WORKERS'] = str(workers)
                    stdout, stderr = execute(command, run_env, args.timeout)
                    wall_ms = (time.perf_counter_ns() - started) / 1e6
                    (logs / (key + '.stdout.json')).write_bytes(stdout)
                    (logs / (key + '.stderr.txt')).write_bytes(stderr)
                    validate_result(stdout, case['expected'])
                    telemetry = json.loads(metrics.read_text())
                    validate_metrics(telemetry)
                    if (nursery == 1 and case['name'] != 'csv-startup'
                            and (not case['name'].startswith('csv-report-') or args.rows_per_file >= 1000)
                            and telemetry['gc_minor'] + telemetry['gc_major'] == 0):
                        raise ValueError(f'pressure workload did not exercise collection: {key}')
                    built = variants[variant][case['app']]
                    if digest(built['binary']) != built['sha256']:
                        raise RuntimeError('application binary changed during measurement')
                    if sources() != report['source_sha256'] or digest(args.gcr) != report['compiler_sha256'] or digest(args.runtime_lib) != report['runtime_sha256']:
                        raise RuntimeError('sources or build inputs changed during measurement')
                    row = dict(variant=variant, case=case['name'], gc_workers=workers, nursery_mb=nursery, trial=trial,
                               warmup=trial == 0, wall_ms=wall_ms, peak_rss_bytes=rss_bytes(usage.read_text(), system),
                               metrics=telemetry, host_before=context, host_after=host_state(), command=command)
                    if row['peak_rss_bytes'] <= 0:
                        raise ValueError('OS peak RSS must be positive')
                    report['runs'].append(row)
                    report.pop('active_run', None)
                    save(report_path, report)
                    print(f'{key}: {wall_ms:.2f} ms; minor={telemetry["gc_minor"]}', flush=True)
        summary = []
        for case, workers, nursery in [(c, w, n) for c in cases for w in args.gc_workers for n in args.nursery_mb]:
            for variant in variants:
                samples = [r for r in report['runs'] if not r['warmup'] and r['variant'] == variant and r['case'] == case['name'] and r['gc_workers'] == workers and r['nursery_mb'] == nursery]
                summary.append(dict(variant=variant, case=case['name'], gc_workers=workers, nursery_mb=nursery, samples=len(samples),
                    median_wall_ms=statistics.median(r['wall_ms'] for r in samples),
                    min_wall_ms=min(r['wall_ms'] for r in samples), max_wall_ms=max(r['wall_ms'] for r in samples),
                    median_peak_rss_bytes=statistics.median(r['peak_rss_bytes'] for r in samples),
                    max_gc_pause_ms=max(r['metrics']['gc_pause_max_ms'] for r in samples),
                    median_gc_minor=statistics.median(r['metrics']['gc_minor'] for r in samples)))
        report.update(status='complete', summary=summary)
    except BaseException as error:
        report.update(status='failed', error=str(error))
        raise
    finally:
        report['finished_utc'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        save(report_path, report)
    print(report_path)


if __name__ == '__main__':
    main()
