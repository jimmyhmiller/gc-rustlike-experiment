import json
from pathlib import Path
import subprocess
import sys
import unittest

import check_compiler_resources as checker


class CompilerResourceTests(unittest.TestCase):
    def run_checker(self, code, budget=1024 ** 3, seconds=5):
        result = subprocess.run([sys.executable, str(Path(checker.__file__)),
                                 '--max-rss-bytes', str(budget),
                                 '--timeout-seconds', str(seconds), '--',
                                 sys.executable, '-c', code],
                                capture_output=True, text=True, timeout=10)
        return result.returncode, json.loads(result.stdout)

    def test_native_rss_units_are_explicit(self):
        self.assertEqual(checker.rss_bytes(2, 'darwin'), 2)
        self.assertEqual(checker.rss_bytes(2, 'linux'), 2048)
        with self.assertRaises(ValueError):
            checker.rss_bytes(2, 'unsupported')

    def test_success_preserves_output_and_reports_memory(self):
        status, report = self.run_checker('print(42)')
        self.assertEqual(status, 0)
        self.assertEqual(report['stdout'], '42\n')
        self.assertEqual(report['returncode'], 0)
        self.assertGreater(report['max_rss_bytes'], 0)
        self.assertFalse(report['rss_budget_exceeded'])

    def test_child_failure_cannot_be_reported_as_success(self):
        status, report = self.run_checker('import sys; print("failed", file=sys.stderr); sys.exit(7)')
        self.assertNotEqual(status, 0)
        self.assertEqual(report['returncode'], 7)
        self.assertEqual(report['stderr'], 'failed\n')

    def test_memory_budget_is_enforced_for_successful_child(self):
        status, report = self.run_checker('print(42)', budget=1)
        self.assertNotEqual(status, 0)
        self.assertEqual(report['returncode'], 0)
        self.assertTrue(report['rss_budget_exceeded'])

    def test_deadline_is_enforced(self):
        status, report = self.run_checker('import time; time.sleep(30)', seconds=0.1)
        self.assertNotEqual(status, 0)
        self.assertTrue(report['timeout'])
        self.assertIsNone(report['returncode'])


if __name__ == '__main__':
    unittest.main()
