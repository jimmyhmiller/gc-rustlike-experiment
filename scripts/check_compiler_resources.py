#!/usr/bin/env python3
"""Measure one compiler process on macOS/Linux and enforce explicit budgets."""
import argparse
import json
import resource
import subprocess
import sys
import time


def rss_bytes(value, platform):
    if platform == 'darwin':
        return int(value)
    if platform.startswith('linux'):
        return int(value) * 1024
    raise ValueError('compiler resource checks require macOS or Linux')


def text(value):
    return value.decode(errors='replace') if isinstance(value, bytes) else value or ''


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--max-rss-bytes', type=int, required=True)
    parser.add_argument('--timeout-seconds', type=float, default=30)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command or args.max_rss_bytes <= 0 or args.timeout_seconds <= 0:
        parser.error('a command and positive resource budgets are required')
    try:
        rss_bytes(0, sys.platform)
    except ValueError as error:
        parser.error(str(error))
    start = time.perf_counter()
    report = dict(command=command, timeout=False, returncode=None, stdout='', stderr='')
    try:
        result = subprocess.run(command, capture_output=True, text=True,
                                timeout=args.timeout_seconds)
        report.update(returncode=result.returncode, stdout=result.stdout, stderr=result.stderr)
    except subprocess.TimeoutExpired as error:
        report.update(timeout=True, stdout=text(error.stdout), stderr=text(error.stderr))
    except OSError as error:
        report['stderr'] = str(error)
    report.update(elapsed_seconds=time.perf_counter() - start,
                  max_rss_bytes=rss_bytes(resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss,
                                          sys.platform),
                  rss_budget_bytes=args.max_rss_bytes)
    report['rss_budget_exceeded'] = report['max_rss_bytes'] > args.max_rss_bytes
    print(json.dumps(report))
    return 0 if report['returncode'] == 0 and not report['timeout'] and not report['rss_budget_exceeded'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
