#!/usr/bin/env python3
"""Build and correctness-check the shared generated native sanitizer corpus."""
import argparse
import json
import os
from pathlib import Path
import subprocess

FIXTURES = ('compound_places', 'unit_values', 'managed_memory', 'channel_contract', 'app_payloads', 'csv_limits', 'inline_allocation', 'root_mirror_callback', 'private_arrays', 'compact_enums', 'recursive_poll', 'private_heap', 'atomic_ownership', 'channel_ownership', 'orphan_threads', 'native_pressure', 'string_scans', 'float_parse', 'ffi_copy', 'ffi_buffers', 'ffi_buffer_cleanup', 'channel_buffers')
APPS = [('gcr-csvreport', ['4', 'category', 'cents', '-', 'apps/gcr-csvreport/examples/purchases.csv', 'apps/gcr-csvreport/examples/purchases.csv'], {'files': 2, 'rows': 6, 'total': 3000}), ('gcr-buildplan', ['apps/gcr-buildplan/examples/release.csv', '2', '-'], {'workers': 2, 'makespan': 9, 'critical_path': 9}), ('gcr-routes', ['apps/gcr-routes/examples/commute.csv', 'home', 'work', '-'], {'distance': 7, 'path': ['home', 'park', 'cafe', 'work']})]
PROGRAMS = FIXTURES + tuple(case[0] for case in APPS)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--compiler', type=Path, required=True)
    parser.add_argument('--runtime', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    compiler, runtime, output = args.compiler.resolve(), args.runtime.resolve(), args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    build_env = dict(os.environ, GCR_CODEGEN_TSAN='1', GCRUST_RUNTIME_LIB=str(runtime))
    for name in PROGRAMS:
        source = Path('tests/fixtures') / (name + '.gcr') if name in FIXTURES else Path('apps') / name
        subprocess.run([str(compiler), 'build', str(source), '-o', str(output / name)],
                       env=build_env, check=True, timeout=120)
    for fixture in FIXTURES:
        for stress in ('0', '1'):
            env = dict(os.environ, GCR_GC_STRESS=stress, GCR_GC_VERIFY='1', GCR_GC_WORKERS='4',
                       GCR_NURSERY_MB='1', GCR_TENURED_MB='64', TSAN_OPTIONS='halt_on_error=1',
                       GCR_STRESS_HEAP_MB='64' if fixture == 'csv_limits' else '8')
            print('Instrumented native ' + fixture + '; GC stress=' + stress, flush=True)
            result = subprocess.run([str(output / fixture)], env=env, timeout=90)
            expected = 42 if fixture in ('compound_places', 'unit_values') else 0
            if result.returncode != expected:
                raise subprocess.CalledProcessError(result.returncode, result.args)
    for app, arguments, expected in APPS:
        for stress in ('0', '1'):
            env = dict(os.environ, GCR_GC_STRESS=stress, GCR_GC_VERIFY='1', GCR_GC_WORKERS='4',
                       GCR_NURSERY_MB='1', GCR_TENURED_MB='16', TSAN_OPTIONS='halt_on_error=1')
            print('Instrumented native ' + app + '; GC stress=' + stress, flush=True)
            result = subprocess.run([str(output / app), *arguments], env=env,
                                    check=True, capture_output=True, text=True, timeout=90)
            data = json.loads(result.stdout)
            if not all(data[key] == value for key, value in expected.items()):
                raise RuntimeError(f'{app}: unexpected result {data!r}')
    print(f'Sanitized programs: {len(PROGRAMS) * 2} passed', flush=True)


if __name__ == '__main__':
    main()
