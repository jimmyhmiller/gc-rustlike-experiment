import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

import benchmark_apps as bench


class ApplicationMeasurements(unittest.TestCase):
    def test_rss_units_are_platform_specific(self):
        self.assertEqual(bench.rss_bytes('  123456 maximum resident set size\n', 'Darwin'), 123456)
        self.assertEqual(bench.rss_bytes('Maximum resident set size (kbytes): 123\n', 'Linux'), 123 * 1024)
        with self.assertRaises(ValueError):
            bench.rss_bytes('missing', 'Darwin')

    def test_models_include_zero_duration_dispatch_rounds(self):
        tasks = {'alpha': (0, []), 'beta': (2, ['alpha']), 'gamma': (1, [])}
        self.assertEqual(bench.plan_model(tasks, 2), {
            'workers': 2, 'makespan': 2, 'critical_path': 2,
            'tasks': [dict(task='alpha', start=0, finish=0, worker=1),
                      dict(task='beta', start=0, finish=2, worker=1),
                      dict(task='gamma', start=0, finish=1, worker=2)]})
        with self.assertRaises(ValueError):
            bench.plan_model({'a': (1, ['b']), 'b': (1, ['a'])}, 2)

    def test_fixture_models_and_unicode_byte_count(self):
        with tempfile.TemporaryDirectory() as directory:
            cases = bench.fixtures(Path(directory) / 'inputs', 2)
            report = next(c for c in cases if c['name'] == 'csv-report-1')['expected']
            self.assertEqual(report['rows'], 16)
            self.assertEqual(report['total'], -712)
            self.assertEqual(report['bytes'], sum(p.stat().st_size for p in (Path(directory) / 'inputs').glob('part-*.csv')))
            self.assertEqual(len(report['groups']), 16)
            route = next(case for case in cases if case['name'] == 'routes')
            self.assertEqual(route['expected']['from'], route['args'][1])
            self.assertEqual(route['expected']['to'], route['args'][2])
            self.assertEqual(route['expected']['path'][-1], route['args'][2])
            self.assertEqual(route['expected']['distance'], len(route['expected']['path']) - 1)

    def test_bad_results_and_malformed_telemetry_are_rejected(self):
        with self.assertRaises(ValueError):
            bench.validate_result(b'{"rows":0}', {'rows': 1})
        valid = {key: 0 for key in bench.METRICS}
        bench.validate_metrics(valid)
        for key, bad in [('gc_minor', True), ('gc_minor', 0.5), ('alloc_bytes', -1),
                         ('gc_pause_max_ms', float('nan')), ('gc_pause_quantile_error_ratio', 2)]:
            with self.subTest(key=key, bad=bad), self.assertRaises(ValueError):
                bench.validate_metrics(valid | {key: bad})
        with self.assertRaises(ValueError):
            bench.validate_metrics({})

    def test_variant_order_alternates_per_configuration_while_rotating(self):
        trials = [list(bench.trial_order(list(range(6)), ['current', 'baseline'], trial)) for trial in range(3)]
        self.assertEqual([trial[0][0] for trial in trials], [0, 1, 2])
        self.assertEqual([dict(trial)[0] for trial in trials],
                         [['current', 'baseline'], ['baseline', 'current'], ['current', 'baseline']])
        for trial in trials:
            self.assertEqual(sorted(config for config, _ in trial), list(range(6)))

    def test_automatic_worker_policy_and_explicit_overrides(self):
        self.assertEqual(bench.worker_policy('auto'), 'auto')
        self.assertEqual(bench.worker_policy('4'), 4)
        for value in ('0', '-1', 'many'):
            with self.assertRaises(argparse.ArgumentTypeError):
                bench.worker_policy(value)

    def test_nonzero_application_exit_is_failure(self):
        with self.assertRaises(RuntimeError):
            bench.execute([sys.executable, '-c', 'raise SystemExit(2)'], os.environ, 2)

    def test_timeout_stops_descendant_processes(self):
        with tempfile.TemporaryDirectory() as directory:
            ready, marker = Path(directory) / 'ready', Path(directory) / 'orphan'
            child = ('from pathlib import Path; import time; '
                     f'Path({str(ready)!r}).write_text("ready"); time.sleep(2); '
                     f'Path({str(marker)!r}).write_text("survived")')
            parent = f'import subprocess,sys,time; subprocess.Popen([sys.executable,"-c",{child!r}]); time.sleep(10)'
            with self.assertRaises(subprocess.TimeoutExpired):
                bench.execute([sys.executable, '-c', parent], os.environ, 1)
            self.assertTrue(ready.exists(), 'descendant must start before the timeout')
            time.sleep(1.3)
            self.assertFalse(marker.exists(), 'timed-out descendant remained alive')


if __name__ == '__main__':
    unittest.main()
