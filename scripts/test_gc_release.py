import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest

import check_gc_release as gate
import check_linux_runtime as linux


class ReleaseGateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'src').mkdir()
        self.source = self.root / 'src/main.rs'
        self.source.write_text('original')

    def test_miri_matrix_requires_each_case_seed_and_full_checks(self):
        sections = []
        for seed in (0, 1, 17):
            for case in ('storage-moves', 'allocator-window', 'aggregate-reload', 'poison'):
                flags = f'-Zmiri-seed={seed}'
                if case == 'aggregate-reload':
                    flags += ' -Zmiri-env-set=GCR_AGGREGATE_RELOAD_CHILD=1'
                sections.append(f'Linux Miri: case={case} seed={seed}\nMiri flags: {flags}\n'
                                'test result: ok. 1 passed; 0 failed; 0 ignored\n')
        summary = 'Linux Miri probes: 12 passed\n'
        valid = ''.join(sections) + summary
        self.assertEqual(len(gate.linux_miri_coverage(valid)), 12)
        invalid = [
            ''.join(sections[1:]) + summary,
            ''.join(sections + sections[:1]) + summary,
            ''.join(reversed(sections)) + summary,
            valid.replace('1 passed; 0 failed; 0 ignored', '0 passed; 0 failed; 1 ignored', 1),
            valid.replace('-Zmiri-seed=0', '-Zmiri-seed=0 -Zmiri-ignore-leaks', 1),
            valid.replace(summary, ''),
            valid + summary,
            valid.replace('Miri flags:', 'error: Undefined Behavior\nMiri flags:', 1),
        ]
        for text in invalid:
            with self.subTest(text=text), self.assertRaises(gate.GateFailure):
                gate.linux_miri_coverage(text)

    def test_miri_plan_runs_through_linux_transport_and_all(self):
        for suite in ('linux-miri', 'all'):
            steps = [step for step in gate.plan(suite, 'nightly') if step.kind == 'linux-miri']
            self.assertEqual(len(steps), 1)
            self.assertEqual(steps[0].command[-2:], ('scripts/check_linux_runtime.py', '--miri'))

    def test_remote_source_check_survives_python_optimization(self):
        self.source.chmod(0o644)
        expected = gate.manifest(self.root)
        for flags in ((), ('-O',), ('-OO',)):
            for change in ('none', 'contents', 'mode', 'missing'):
                with self.subTest(flags=flags, change=change):
                    self.source.write_text('original')
                    self.source.chmod(0o644)
                    if change == 'contents':
                        self.source.write_text('changed')
                    elif change == 'mode':
                        self.source.chmod(0o755)
                    elif change == 'missing':
                        self.source.unlink()
                    result = subprocess.run(
                        [sys.executable, *flags, '-c', linux.REMOTE_SOURCE_CHECK, str(self.root)],
                        input=json.dumps(expected), text=True, capture_output=True, timeout=10)
                    if change == 'none':
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertIn('Remote source bytes/modes verified: 1', result.stdout)
                    else:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertNotIn('Remote source bytes/modes verified:', result.stdout)

    def test_archive_matches_hashed_bytes_and_modes(self):
        output = self.root / 'target/evidence'
        output.mkdir(parents=True)
        entries = gate.snapshot(self.root, output)
        with tarfile.open(output / 'sources.tar.gz') as archive:
            for item in archive:
                self.assertEqual(hashlib.sha256(archive.extractfile(item).read()).hexdigest(),
                                 entries[item.name]['sha256'])
                self.assertEqual(item.mode, entries[item.name]['mode'])
        (output / 'generated').write_text('ignored build output')
        gate.unchanged(self.root, entries)

    def test_debugger_tools_are_source_inputs_and_archive_members(self):
        tools = self.root / 'tools'
        tools.mkdir()
        formatter = tools / 'gcr_lldb.py'
        formatter.write_text('# debugger formatter\n')
        output = self.root / 'target/evidence'
        output.mkdir(parents=True)
        entries = gate.snapshot(self.root, output)
        name = 'tools/gcr_lldb.py'
        self.assertEqual(entries[name]['sha256'], hashlib.sha256(formatter.read_bytes()).hexdigest())
        with tarfile.open(output / 'sources.tar.gz') as archive:
            self.assertEqual(archive.extractfile(name).read(), formatter.read_bytes())
        # Importing the formatter may create bytecode; that is generated output.
        cache = tools / '__pycache__'
        cache.mkdir()
        (cache / 'gcr_lldb.cpython-313.pyc').write_bytes(b'generated')
        gate.unchanged(self.root, entries)
        formatter.write_text('# modified formatter\n')
        with self.assertRaises(gate.GateFailure):
            gate.unchanged(self.root, entries)

    def test_source_changes_fail(self):
        for change in ('edit', 'delete', 'add', 'mode'):
            with self.subTest(change=change):
                self.source.write_text('original')
                self.source.chmod(0o644)
                extra = self.root / 'src/extra.rs'
                extra.unlink(missing_ok=True)
                entries = gate.manifest(self.root)
                if change == 'edit':
                    self.source.write_text('changed')
                elif change == 'delete':
                    self.source.unlink()
                elif change == 'add':
                    extra.write_text('new')
                else:
                    self.source.chmod(0o755)
                with self.assertRaises(gate.GateFailure):
                    gate.unchanged(self.root, entries)

    def test_top_level_source_symlink_rejected(self):
        (self.root / 'apps').symlink_to(self.root / 'src', target_is_directory=True)
        with self.assertRaises(gate.GateFailure):
            gate.manifest(self.root)

    def test_lock_contention_and_release(self):
        with gate.exclusive(self.root):
            with self.assertRaises(gate.GateFailure):
                with gate.exclusive(self.root):
                    self.fail('acquired held lock')
        with gate.exclusive(self.root):
            pass

    def test_zero_and_unarmed_tests_rejected(self):
        for kind, text in [('cargo', 'test result: ok. 0 passed; 0 failed; 0 ignored'),
                           ('detector', 'test result: ok. 0 passed; 0 failed; 1 ignored'),
                           ('detector', 'test result: ok. 2 passed; 0 failed; 0 ignored'),
                           ('python', 'Ran 0 tests in 0.1s\nOK')]:
            with self.subTest(kind=kind, text=text), self.assertRaises(gate.GateFailure):
                gate.test_counts(kind, text)
        self.assertEqual(gate.test_counts('detector',
            'test result: ok. 1 passed; 0 failed; 0 ignored')['passed'], 1)

    def test_failure_is_retained_and_stops_later_commands(self):
        output = self.root / 'target/evidence'
        steps = [gate.Step('failure', (sys.executable, '-c', 'print("failure detail");exit(7)')),
                 gate.Step('never', (sys.executable, '-c', 'raise Exception("must not run")'))]
        report = gate.validate(self.root, output, 'core', 'nightly', 10, steps)
        self.assertEqual(report['status'], 'failed')
        self.assertEqual(len(report['checks']), 1)
        self.assertEqual(json.loads((output / 'report.json').read_text())['checks'][0]['exit_code'], 7)
        self.assertIn('failure detail', (output / 'failure.log').read_text())
        with self.assertRaises(FileExistsError):
            gate.validate(self.root, output, 'core', 'nightly', 10, [])

    def test_soak_plan_covers_repeated_seed_worker_heap_matrix(self):
        steps = [step for step in gate.plan('soak', 'nightly') if step.kind == 'pressure']
        self.assertEqual(len(steps), 72)
        self.assertEqual(len({step.name for step in steps}), 72)
        for seed in gate.SOAK_SEEDS:
            for workers in gate.SOAK_WORKERS:
                for mode in ('semi', 'generational'):
                    matches = [step for step in steps
                               if step.environment['GCR_CONCURRENT_ALLOCATION_SEED'] == str(seed)
                               and step.environment['GCR_GC_WORKERS'] == str(workers)
                               and step.environment['GCR_CONCURRENT_ALLOCATION_CHILD'] == mode]
                    self.assertEqual(len(matches), 3)
                    for step in matches:
                        self.assertIn(gate.PRESSURE_TEST, step.command)
                        self.assertIn('--exact', step.command)
                        self.assertEqual(step.environment['GCR_GC_VERIFY'], '1')
                        self.assertEqual(step.environment['GCRUST_POISON'], '1')
        self.assertEqual(sum(step.kind == 'pressure' for step in gate.plan('all', 'nightly')), 72)

    def test_pressure_summary_rejects_missing_or_wrong_coverage(self):
        environment = {'GCR_CONCURRENT_ALLOCATION_CHILD': 'generational',
                       'GCR_CONCURRENT_ALLOCATION_SEED': '1'}
        valid = ('allocation pressure: concurrent_cycles=256 allocations=8192 '
                 'active_iterations=9 total_collections=300 live_bytes=64 '
                 'mode=generational seed=1 minor_collections=2')
        self.assertEqual(gate.pressure_counters(valid, environment)['allocations'], 8192)
        prefixed = 'test ' + gate.PRESSURE_TEST + ' ... ' + valid
        self.assertEqual(gate.pressure_counters(prefixed, environment)['mode'], 'generational')
        invalid = ['', valid + '\n' + valid,
                   valid.replace('mode=generational', 'mode=semi'),
                   valid.replace('seed=1', 'seed=2'),
                   valid.replace('concurrent_cycles=256', 'concurrent_cycles=255'),
                   valid.replace('allocations=8192', 'allocations=8191'),
                   valid.replace('active_iterations=9', 'active_iterations=0'),
                   valid.replace('total_collections=300', 'total_collections=256'),
                   valid.replace('live_bytes=64', 'live_bytes=0'),
                   valid.replace('minor_collections=2', 'minor_collections=0')]
        for text in invalid:
            with self.subTest(text=text), self.assertRaises(gate.GateFailure):
                gate.pressure_counters(text, environment)
        for text in ['test result: ok. 0 passed; 0 failed; 0 ignored',
                     'test result: ok. 2 passed; 0 failed; 0 ignored',
                     'test result: ok. 1 passed; 0 failed; 1 ignored']:
            with self.assertRaises(gate.GateFailure):
                gate.test_counts('pressure', text)

    def test_successful_test_cannot_hide_missing_soak_workload(self):
        output = self.root / 'target/soak-evidence'
        step = gate.Step('missing-workload', (sys.executable, '-c',
                         'print("test result: ok. 1 passed; 0 failed; 0 ignored")'),
                         'pressure', {'GCR_CONCURRENT_ALLOCATION_CHILD': 'semi',
                                      'GCR_CONCURRENT_ALLOCATION_SEED': '1'})
        report = gate.validate(self.root, output, 'soak', 'nightly', 10, [step])
        self.assertEqual(report['status'], 'failed')
        self.assertEqual(report['checks'][0]['exit_code'], 0)
        self.assertIn('pressure workload summary', report['error'])

    def test_linux_matrix_requires_each_profile_worker_and_armed_detector(self):
        sections = []
        for profile in ('debug', 'release'):
            for workers in (1, 4):
                sections.extend([
                    f'Linux runtime: profile={profile} workers={workers}\n'
                    'test result: ok. 528 passed; 0 failed; 5 ignored\n',
                    f'Linux detector: profile={profile} workers={workers}\n'
                    'test result: ok. 1 passed; 0 failed; 0 ignored\n'])
            sections.append(f'Linux doctests: profile={profile}\n'
                            'test result: ok. 9 passed; 0 failed; 5 ignored\n')
        self.assertEqual(len(gate.linux_coverage(''.join(sections))), 10)
        for text in [''.join(sections[1:]), ''.join(sections + sections[-1:]),
                     ''.join(sections).replace('1 passed; 0 failed; 0 ignored',
                                              '0 passed; 0 failed; 0 ignored', 1),
                     ''.join(sections).replace('528 passed', '0 passed', 1)]:
            with self.subTest(text=text), self.assertRaises(gate.GateFailure):
                gate.linux_coverage(text)

    def test_linux_sanitizer_requires_both_workers_and_armed_detectors(self):
        sections = []
        for workers in (1, 4):
            sections.extend([
                f'Linux TSAN runtime: workers={workers}\n'
                'test result: ok. 527 passed; 0 failed; 6 ignored\n',
                f'Linux TSAN detector: workers={workers}\n'
                'test result: ok. 1 passed; 0 failed; 0 ignored\n'])
        self.assertEqual(len(gate.linux_tsan_coverage(''.join(sections))), 4)
        for text in [''.join(sections[:-1]), ''.join(sections + sections[-1:]),
                     ''.join(sections).replace('1 passed; 0 failed; 0 ignored',
                                              '0 passed; 0 failed; 1 ignored', 1),
                     ''.join(sections).replace('527 passed', '0 passed', 1)]:
            with self.subTest(text=text), self.assertRaises(gate.GateFailure):
                gate.linux_tsan_coverage(text)
        step = next(step for step in gate.plan('linux-tsan', 'nightly') if step.kind == 'linux-tsan')
        self.assertIn('--sanitizer', step.command)

    def test_generated_sanitizer_requires_every_program_and_stress_mode(self):
        from check_generated_sanitizer import PROGRAMS
        rows = [f'Instrumented native {name}; GC stress={stress}'
                for name in PROGRAMS for stress in ('0', '1')]
        summary = f'Sanitized programs: {len(rows)} passed'
        valid = '\n'.join([*rows, summary])
        self.assertEqual(gate.test_counts('generated-tsan', valid)['passed'], 50)
        for text in ['\n'.join(rows), '\n'.join([*rows[1:], summary]),
                     '\n'.join([*rows, rows[0], summary]), valid.replace('GC stress=1', 'GC stress=0', 1)]:
            with self.subTest(text=text), self.assertRaises(gate.GateFailure):
                gate.test_counts('generated-tsan', text)

    @staticmethod
    def concurrent_jit_rows():
        return [f'generated concurrent: case={case} mode={mode} optimized={optimized} '
                'cycles=8 active_writes=1 old_alias_writes=1 active_reader_snapshots=1 '
                'young_edges=1 minors=2 first_young_minor=1'
                for case in ('single', 'shared') for mode in ('semispace', 'generational')
                for optimized in ('false', 'true')]

    def test_concurrent_jit_requires_all_modes_and_actual_mutation_coverage(self):
        rows = self.concurrent_jit_rows()
        valid = '\n'.join(rows)
        self.assertEqual(len(gate.concurrent_jit_coverage(valid)), 8)
        invalid = ['\n'.join(rows[1:]), '\n'.join(rows + rows[:1]),
                   valid.replace('active_writes=1', 'active_writes=0', 1),
                   valid.replace('old_alias_writes=1', 'old_alias_writes=0', 1),
                   valid.replace('active_reader_snapshots=1', 'active_reader_snapshots=0'),
                   valid.replace('young_edges=1', 'young_edges=0'),
                   valid.replace('minors=2', 'minors=1')]
        for text in invalid:
            with self.subTest(text=text), self.assertRaises(gate.GateFailure):
                gate.concurrent_jit_coverage(text)

    def test_linux_compiler_requires_workspace_jit_and_detector_sections(self):
        sections = []
        for profile in ('debug', 'release'):
            sections.extend([f'Linux workspace: profile={profile}\n'
                             'test result: ok. 1000 passed; 0 failed; 10 ignored\n',
                             f'Linux JIT concurrent: profile={profile}\n'
                             + '\n'.join(self.concurrent_jit_rows())
                             + '\ntest result: ok. 1 passed; 0 failed; 0 ignored\n'])
        sections.append('Linux workspace detector: profile=release\n'
                        'test result: ok. 1 passed; 0 failed; 0 ignored\n')
        self.assertEqual(len(gate.linux_compiler_coverage(''.join(sections))), 5)
        for text in [''.join(sections[:-1]), ''.join(sections + sections[-1:]),
                     ''.join(sections).replace('1000 passed', '0 passed', 1)]:
            with self.assertRaises(gate.GateFailure):
                gate.linux_compiler_coverage(text)

    def test_timeout_kills_term_ignoring_descendant(self):
        marker = self.root / 'escaped'
        child = ('import signal,time;from pathlib import Path;'
                 'signal.signal(signal.SIGTERM, signal.SIG_IGN);'
                 f'time.sleep(1);Path({str(marker)!r}).write_text("escaped")')
        parent = ('import subprocess,sys,time;'
                  f'subprocess.Popen([sys.executable,"-c",{child!r}]);time.sleep(30)')
        step = gate.Step('timeout', (sys.executable, '-c', parent))
        with self.assertRaises(gate.GateFailure):
            gate.run_command(step, self.root, self.root / 'timeout.log', dict(os.environ), .3)
        time.sleep(1)
        self.assertFalse(marker.exists(), 'timed-out descendant survived command-group cleanup')


if __name__ == '__main__':
    unittest.main()
