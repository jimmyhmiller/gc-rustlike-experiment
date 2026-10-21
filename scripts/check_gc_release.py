#!/usr/bin/env python3
"""Run named GC release gates and retain source-identified evidence (Unix hosts)."""
from __future__ import annotations

import argparse
import contextlib
import dataclasses
import datetime
import fcntl
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import sys
import tarfile
import time

ROOT = Path(__file__).resolve().parents[1]
INPUT_DIRS = ('src', 'crates', 'tests', 'scripts', 'tools', 'examples', 'bench', 'apps', '.cargo', '.github')
INPUT_FILES = ('Cargo.toml', 'Cargo.lock', 'build.rs', 'README.md', '.gitignore',
               'rust-toolchain', 'rust-toolchain.toml')
SKIP_DIRS = {'target', '.git', '.worktrees', 'node_modules', '__pycache__'}
DETECTOR = 'gc::tests::semi_space_detector_panics_on_bad_header_in_traced_slot'
PRESSURE_TEST = 'runtime::concurrent_allocation_tests::collector_concurrent_allocation_pressure_preserves_slot_ledger'
SOAK_SEEDS = (1, 0x9e3779b97f4a7c15, 0xd1b54a32d192ed03)
SOAK_WORKERS = (1, 2, 4, 8)
SOAK_TRIALS = 3


class GateFailure(Exception):
    pass


class Cancelled(Exception):
    def __init__(self, signum):
        self.signum = signum
        super().__init__(f'interrupted by signal {signum}')


@dataclasses.dataclass(frozen=True)
class Step:
    name: str
    command: tuple[str, ...]
    kind: str = 'tool'
    environment: dict[str, str] = dataclasses.field(default_factory=dict)


def utc():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def save(path, value):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')
    temporary.replace(path)


def input_paths(root):
    paths = {root / name for name in INPUT_FILES if (root / name).exists()}
    for name in INPUT_DIRS:
        base = root / name
        if base.is_symlink():
            raise GateFailure(f'cannot snapshot source directory symlink: {base}')
        if not base.exists():
            continue
        for directory, folders, files in os.walk(base, followlinks=False):
            folders[:] = sorted(folder for folder in folders if folder not in SKIP_DIRS)
            for folder in folders:
                if (Path(directory) / folder).is_symlink():
                    raise GateFailure(f'cannot snapshot source directory symlink: {directory}/{folder}')
            for filename in files:
                if not filename.endswith('.pyc') and filename != '.DS_Store':
                    paths.add(Path(directory) / filename)
    for path in sorted(paths):
        if path.is_symlink() or not path.is_file():
            raise GateFailure(f'cannot snapshot nonregular source input: {path}')
        yield path


def manifest(root):
    return {str(path.relative_to(root)): dict(sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
            mode=path.stat().st_mode & 0o777) for path in input_paths(root)}


def snapshot(root, output):
    entries = {}
    with tarfile.open(output / 'sources.tar.gz', 'w:gz') as archive:
        for path in input_paths(root):
            data = path.read_bytes()
            name = str(path.relative_to(root))
            mode = path.stat().st_mode & 0o777
            entries[name] = dict(sha256=hashlib.sha256(data).hexdigest(), mode=mode)
            entry = tarfile.TarInfo(name)
            entry.size, entry.mode = len(data), mode
            entry.mtime = 0
            archive.addfile(entry, io.BytesIO(data))
    save(output / 'sources.json', entries)
    unchanged(root, entries)
    return entries


def unchanged(root, expected):
    current = manifest(root)
    differences = sorted(p for p in current.keys() | expected.keys() if current.get(p) != expected.get(p))
    if differences:
        raise GateFailure('source inputs changed: ' + ', '.join(differences))


@contextlib.contextmanager
def exclusive(root):
    directory = root / 'target'
    directory.mkdir(exist_ok=True)
    with (directory / 'gc-release-validation.lock').open('a+') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise GateFailure('another release gate holds this checkout lock') from error
        try:
            yield
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)


def plan(suite, toolchain):
    steps = [Step('rustc', ('rustc', '-Vv')), Step('cargo', ('cargo', '-V'))]
    if suite in ('core', 'all'):
        steps += [
            Step('python-tests', (sys.executable, '-m', 'unittest', 'discover', '-s', 'scripts', '-p', 'test_*.py'), 'python'),
            Step('workspace-debug', ('cargo', 'test', '--locked', '--workspace'), 'cargo'),
            Step('workspace-release', ('cargo', 'test', '--locked', '--workspace', '--release'), 'cargo'),
            Step('armed-detector', ('cargo', 'test', '--locked', '-p', 'gcrust-rt', '--release', DETECTOR,
                                   '--', '--ignored', '--exact'), 'detector', {'GCR_GC_VERIFY': '1'}),
        ]
    if suite in ('tsan', 'all'):
        steps += [Step('tsan-rustc', ('rustc', '+' + toolchain, '-Vv')),
                  Step('thread-sanitizer', ('bash', 'scripts/check_thread_sanitizer.sh'), 'cargo',
                       {'GCR_TSAN_TOOLCHAIN': toolchain})]
    if suite in ('big-endian', 'all'):
        steps += [Step('docker', ('docker', 'version')),
                  Step('fetch-dependencies', ('cargo', 'fetch', '--locked')),
                  Step('big-endian', ('bash', 'scripts/check_big_endian_runtime.sh'), 'cargo')]
    if suite in ('linux', 'all'):
        steps += [Step('linux-runtime', (sys.executable, 'scripts/check_linux_runtime.py'), 'linux')]
    if suite in ('linux-tsan', 'all'):
        steps += [Step('linux-thread-sanitizer',
                       (sys.executable, 'scripts/check_linux_runtime.py', '--sanitizer'), 'linux-tsan')]
    if suite in ('linux-generated-tsan', 'all'):
        steps += [Step('linux-generated-sanitizer',
                       (sys.executable, 'scripts/check_linux_runtime.py', '--generated-sanitizer'), 'generated-tsan')]
    if suite in ('linux-compiler', 'all'):
        steps += [Step('linux-compiler',
                       (sys.executable, 'scripts/check_linux_runtime.py', '--compiler-tests'), 'linux-compiler')]
    if suite in ('linux-miri', 'all'):
        steps += [Step('linux-miri', (sys.executable, 'scripts/check_linux_runtime.py', '--miri'), 'linux-miri')]
    if suite in ('soak', 'all'):
        for trial in range(SOAK_TRIALS):
            for seed in SOAK_SEEDS:
                for workers in SOAK_WORKERS:
                    for mode in ('semi', 'generational'):
                        steps.append(Step(f'soak-{trial}-{seed}-{workers}-{mode}',
                            ('cargo', 'test', '--locked', '-p', 'gcrust-rt', '--release',
                             PRESSURE_TEST, '--', '--exact', '--nocapture', '--test-threads=1'),
                            'pressure', {'GCR_CONCURRENT_ALLOCATION_CHILD': mode,
                                         'GCR_CONCURRENT_ALLOCATION_SEED': str(seed),
                                         'GCR_GC_WORKERS': str(workers),
                                         'GCR_GC_VERIFY': '1', 'GCRUST_POISON': '1'}))
    return steps


def pressure_counters(text, environment):
    rows = re.findall(r'^(?:test ' + re.escape(PRESSURE_TEST) + r' \.\.\. )?allocation pressure: concurrent_cycles=(\d+) allocations=(\d+) '
                      r'active_iterations=(\d+) total_collections=(\d+) live_bytes=(\d+) '
                      r'mode=(semi|generational) seed=(\d+) minor_collections=(\d+)$', text, re.M)
    if len(rows) != 1:
        raise GateFailure('missing or repeated pressure workload summary')
    values = rows[0]
    counters = dict(zip(('concurrent_cycles', 'allocations', 'active_iterations',
                         'total_collections', 'live_bytes', 'mode', 'seed', 'minor_collections'),
                        (value if index == 5 else int(value) for index, value in enumerate(values))))
    mode = environment['GCR_CONCURRENT_ALLOCATION_CHILD']
    if (counters['mode'] != mode or counters['seed'] != int(environment['GCR_CONCURRENT_ALLOCATION_SEED'])
            or counters['concurrent_cycles'] != 256 or counters['allocations'] < 8192
            or counters['active_iterations'] <= 0 or counters['total_collections'] < 257
            or counters['live_bytes'] <= 0
            or (mode == 'generational' and counters['minor_collections'] <= 0)
            or (mode == 'semi' and counters['minor_collections'] != 0)):
        raise GateFailure('pressure workload did not satisfy the requested coverage')
    return counters


def linux_coverage(text):
    markers = list(re.finditer(r'^Linux (runtime|detector|doctests): profile=(debug|release)'
                              r'(?: workers=(1|4))?$', text, re.M))
    expected = [(kind, profile, str(workers))
                for profile in ('debug', 'release')
                for workers in (1, 4)
                for kind in ('runtime', 'detector')]
    expected.insert(4, ('doctests', 'debug', None))
    expected.append(('doctests', 'release', None))
    if [match.groups() for match in markers] != expected:
        raise GateFailure('missing, repeated, or reordered Linux matrix sections')
    coverage = []
    for index, marker in enumerate(markers):
        end = markers[index + 1].start() if index + 1 < len(markers) else len(text)
        kind, profile, workers = marker.groups()
        counts = test_counts('detector' if kind == 'detector' else 'cargo', text[marker.end():end])
        coverage.append(dict(kind=kind, profile=profile, workers=workers, tests=counts))
    return coverage


def linux_tsan_coverage(text):
    markers = list(re.finditer(r'^Linux TSAN (runtime|detector): workers=(1|4)$', text, re.M))
    expected = [(kind, str(workers)) for workers in (1, 4) for kind in ('runtime', 'detector')]
    if [match.groups() for match in markers] != expected:
        raise GateFailure('missing, repeated, or reordered Linux sanitizer sections')
    coverage = []
    for index, marker in enumerate(markers):
        end = markers[index + 1].start() if index + 1 < len(markers) else len(text)
        kind, workers = marker.groups()
        counts = test_counts('detector' if kind == 'detector' else 'cargo', text[marker.end():end])
        coverage.append(dict(kind=kind, workers=workers, tests=counts))
    return coverage


def concurrent_jit_coverage(text):
    rows = re.findall(r'^generated concurrent: case=(single|shared) mode=(semispace|generational) '
                      r'optimized=(false|true) cycles=(\d+) active_writes=(\d+) '
                      r'old_alias_writes=(\d+) active_reader_snapshots=(\d+) young_edges=(\d+) '
                      r'minors=(\d+) first_young_minor=(\d+)$', text, re.M)
    expected = {(case, mode, optimized) for case in ('single', 'shared')
                for mode in ('semispace', 'generational') for optimized in ('false', 'true')}
    if len(rows) != 8 or {row[:3] for row in rows} != expected:
        raise GateFailure('missing or repeated concurrent JIT modes')
    result = []
    for case, mode, optimized, *values in rows:
        counters = dict(zip(('cycles', 'active_writes', 'old_alias_writes', 'active_reader_snapshots',
                             'young_edges', 'minors', 'first_young_minor'), map(int, values)))
        if (counters['cycles'] < 8 or counters['active_writes'] <= 0
                or (mode == 'semispace' and counters['old_alias_writes'] <= 0)
                or (case == 'shared' and counters['active_reader_snapshots'] <= 0)
                or (mode == 'generational' and (counters['young_edges'] <= 0
                    or counters['minors'] <= counters['first_young_minor']))):
            raise GateFailure('incomplete concurrent JIT mutation/relocation coverage')
        result.append(dict(case=case, mode=mode, optimized=optimized, **counters))
    return result


def linux_compiler_coverage(text):
    markers = list(re.finditer(r'^Linux (workspace|JIT concurrent|workspace detector): profile=(debug|release)$', text, re.M))
    expected = [('workspace', 'debug'), ('JIT concurrent', 'debug'),
                ('workspace', 'release'), ('JIT concurrent', 'release'), ('workspace detector', 'release')]
    if [match.groups() for match in markers] != expected:
        raise GateFailure('missing, repeated, or reordered Linux compiler sections')
    coverage = []
    for index, marker in enumerate(markers):
        end = markers[index + 1].start() if index + 1 < len(markers) else len(text)
        section = text[marker.end():end]
        kind, profile = marker.groups()
        counts = test_counts('detector' if kind != 'workspace' else 'cargo', section)
        row = dict(kind=kind, profile=profile, tests=counts)
        if kind == 'JIT concurrent':
            row['modes'] = concurrent_jit_coverage(section)
        coverage.append(row)
    return coverage


def linux_miri_coverage(text):
    cases = ('storage-moves', 'allocator-window', 'aggregate-reload', 'poison')
    expected = [(case, str(seed)) for seed in (0, 1, 17) for case in cases]
    markers = list(re.finditer(r'^Linux Miri: case=(\S+) seed=(\d+)$', text, re.M))
    if [marker.groups() for marker in markers] != expected:
        raise GateFailure('missing, repeated, or reordered Miri seed/case sections')
    if re.findall(r'^Linux Miri probes: (\d+) passed$', text, re.M) != ['12']:
        raise GateFailure('missing or repeated Miri summary')
    coverage = []
    for index, marker in enumerate(markers):
        end = markers[index + 1].start() if index + 1 < len(markers) else len(text)
        section = text[marker.end():end]
        case, seed = marker.groups()
        flags = '-Zmiri-seed=' + seed
        if case == 'aggregate-reload':
            flags += ' -Zmiri-env-set=GCR_AGGREGATE_RELOAD_CHILD=1'
        if re.findall(r'^Miri flags: (.*)$', section, re.M) != [flags]:
            raise GateFailure('unexpected Miri interpreter flags')
        if 'error: Undefined Behavior' in section or 'error: unsupported operation' in section:
            raise GateFailure('Miri interpreter rejected the workload')
        coverage.append(dict(case=case, seed=int(seed), flags=flags,
                             tests=test_counts('detector', section)))
    return coverage


def stop_group(process):
    if process.returncode is not None:
        return
    # Do not reap the leader before the final kill: its reserved PID prevents
    # this process-group ID from being reused during cleanup.
    with contextlib.suppress(ProcessLookupError):
        os.killpg(process.pid, signal.SIGTERM)
    time.sleep(0.2)
    with contextlib.suppress(ProcessLookupError):
        os.killpg(process.pid, signal.SIGKILL)
    process.wait()


def run_command(step, root, log, environment, timeout):
    started = time.monotonic()
    with log.open('wb') as stream:
        process = subprocess.Popen(step.command, cwd=root, env=environment | step.environment,
                                   stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            stop_group(process)
            raise GateFailure(f'{step.name} timed out; command group terminated')
        except BaseException:
            stop_group(process)
            raise
    return dict(exit_code=code, elapsed_seconds=time.monotonic() - started)


def test_counts(kind, text):
    if kind == 'generated-tsan':
        from check_generated_sanitizer import PROGRAMS
        rows = re.findall(r'^Instrumented native (\S+); GC stress=([01])$', text, re.M)
        expected = {(name, stress) for name in PROGRAMS for stress in ('0', '1')}
        if (len(rows) != len(expected) or set(rows) != expected
                or re.findall(r'^Sanitized programs: (\d+) passed$', text, re.M) != [str(len(expected))]):
            raise GateFailure('missing, repeated, or incomplete generated sanitizer runs')
        return dict(passed=len(expected), failed=0, ignored=0)
    if kind in ('cargo', 'detector', 'pressure', 'linux', 'linux-tsan', 'linux-compiler', 'linux-miri'):
        rows = [tuple(map(int, match)) for match in re.findall(
            r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored', text)]
        if not rows or 'test result: FAILED' in text:
            raise GateFailure('missing or failed Rust test results')
        passed, failed, ignored = map(sum, zip(*rows))
        if passed == 0 or failed or (kind in ('detector', 'pressure') and (passed != 1 or ignored != 0)):
            raise GateFailure('empty, failed, or incorrectly armed test gate')
        return dict(passed=passed, failed=failed, ignored=ignored)
    if kind == 'python':
        match = re.search(r'Ran (\d+) tests? in ', text)
        if not match or int(match[1]) == 0 or not re.search(r'^OK(?:\s|$)', text, re.M):
            raise GateFailure('missing or failed Python test results')
        return dict(passed=int(match[1]))
    return None


def validate(root, output, suite, toolchain, timeout, steps=None):
    root, output = root.resolve(), output.resolve()
    for name in INPUT_DIRS:
        if output == root / name or (root / name) in output.parents:
            raise GateFailure('evidence output must be outside source input directories')
    if output == root:
        raise GateFailure('evidence output cannot be the checkout root')
    output.mkdir(parents=True, exist_ok=False)
    report = dict(schema='gc-release-gate/1', status='running', suite=suite, started_utc=utc(),
                  platform=platform.platform(), python=sys.version, checks=[],
                  source_scope=dict(directories=INPUT_DIRS, root_files=INPUT_FILES, excluded_directories=sorted(SKIP_DIRS)),
                  tsan_toolchain=toolchain, timeout_seconds=timeout,
                  limitations=['Passing this named suite is not proof of overall production readiness.',
                               'Do not edit build/test inputs while validation runs; hashes are checked at step boundaries.',
                               'Source scope excludes readiness/history documents under docs/; no performance budget is tested.'])
    report_path = output / 'report.json'
    save(report_path, report)
    environment = {k: v for k, v in os.environ.items() if not k.startswith(('GCR_', 'GCRUST_'))}
    report['build_environment'] = {k: environment[k] for k in (
        'LLVM_SYS_211_PREFIX', 'RUSTUP_TOOLCHAIN', 'RUSTFLAGS', 'RUSTDOCFLAGS', 'CARGO_BUILD_TARGET',
        'CARGO_TARGET_DIR', 'CC', 'CXX', 'AR') if k in environment}
    try:
        with exclusive(root):
            sources = snapshot(root, output)
            report['source_count'] = len(sources)
            report['source_archive_sha256'] = hashlib.sha256((output / 'sources.tar.gz').read_bytes()).hexdigest()
            for step in steps if steps is not None else plan(suite, toolchain):
                unchanged(root, sources)
                check = dict(name=step.name, command=step.command, environment=step.environment,
                             kind=step.kind, status='running', started_utc=utc(),
                             timeout_seconds=min(timeout, 180) if step.kind == 'pressure' else timeout)
                report['checks'].append(check)
                save(report_path, report)
                print(f'{step.name}: running', flush=True)
                try:
                    log = output / (step.name + '.log')
                    check.update(run_command(step, root, log, environment, check['timeout_seconds']))
                    if check['exit_code'] != 0:
                        raise GateFailure(f'{step.name} exited {check["exit_code"]}')
                    text = log.read_text(errors='replace')
                    counts = test_counts(step.kind, text)
                    if step.kind == 'pressure':
                        check['workload'] = pressure_counters(text, step.environment)
                    if step.kind == 'linux':
                        check['matrix'] = linux_coverage(text)
                    if step.kind == 'linux-tsan':
                        check['matrix'] = linux_tsan_coverage(text)
                    if step.kind == 'linux-compiler':
                        check['matrix'] = linux_compiler_coverage(text)
                    if step.kind == 'linux-miri':
                        check['matrix'] = linux_miri_coverage(text)
                    if counts is not None:
                        check['tests'] = counts
                    unchanged(root, sources)
                    check['status'] = 'passed'
                except BaseException:
                    check['status'] = 'failed'
                    raise
                finally:
                    check['finished_utc'] = utc()
                    save(report_path, report)
                print(f'{step.name}: passed', flush=True)
            unchanged(root, sources)
            report['status'] = 'passed'
    except (Exception, KeyboardInterrupt) as error:
        report['status'] = 'interrupted' if isinstance(error, (Cancelled, KeyboardInterrupt)) else 'failed'
        report['error'] = str(error)
    finally:
        report['finished_utc'] = utc()
        save(report_path, report)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--suite', choices=['core', 'tsan', 'big-endian', 'linux', 'linux-tsan', 'linux-generated-tsan', 'linux-compiler', 'linux-miri', 'soak', 'all'], default='core')
    parser.add_argument('--output', type=Path, required=True, help='new evidence directory, normally below target/gc-validation')
    parser.add_argument('--timeout', type=float, default=1800, help='per-command timeout in seconds')
    parser.add_argument('--tsan-toolchain', default=os.environ.get('GCR_TSAN_TOOLCHAIN', 'nightly-2026-03-12'))
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error('--timeout must be positive')
    def interrupted(signum, _frame):
        raise Cancelled(signum)
    for signum in (signal.SIGTERM, signal.SIGINT):
        signal.signal(signum, interrupted)
    try:
        report = validate(ROOT, args.output, args.suite, args.tsan_toolchain, args.timeout)
    except (OSError, GateFailure) as error:
        parser.exit(1, f'{error}\n')
    if report['status'] != 'passed':
        parser.exit(1, report.get('error', report['status']) + '\n')
    print(f'passed {args.suite}: {args.output / "report.json"}')


if __name__ == '__main__':
    main()
