#!/usr/bin/env python3
"""Run native Linux validation locally or on Jimmy's existing SSH Linux host."""
import argparse
from pathlib import Path
import os
import platform
import re
import shlex
import subprocess
import tempfile

import check_gc_release as gate

ROOT = Path(__file__).resolve().parents[1]
HOST = 'computer.jimmyhmiller.com'
SSH = ('ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=15', HOST)

REMOTE_SOURCE_CHECK = """\
import hashlib, json, pathlib, stat, sys
root = pathlib.Path(sys.argv[1])
expected = json.load(sys.stdin)
for name, entry in expected.items():
    path = root / name
    if (hashlib.sha256(path.read_bytes()).hexdigest() != entry['sha256']
            or stat.S_IMODE(path.stat().st_mode) != entry['mode']):
        raise SystemExit('Remote source bytes/mode mismatch: ' + name)
print('Remote source bytes/modes verified:', len(expected), flush=True)
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group()
    group.add_argument('--miri', action='store_true', help='run focused Linux Miri runtime checks')
    group.add_argument('--compiler-tests', action='store_true', help='validate the Linux compiler/JIT workspace')
    group.add_argument('--generated-sanitizer', action='store_true', help='validate generated Linux sanitizer programs')
    group.add_argument('--sanitizer', action='store_true', help='run the Linux ThreadSanitizer suite')
    args = parser.parse_args()
    script = 'scripts/check_linux_thread_sanitizer.sh' if args.sanitizer else 'scripts/check_linux_runtime.sh'
    if args.generated_sanitizer:
        script = 'scripts/check_linux_generated_sanitizer.sh'
    if args.compiler_tests:
        script = 'scripts/check_linux_compiler.sh'
    if args.miri:
        script = 'scripts/check_linux_miri.sh'
    environment = {k: v for k, v in os.environ.items() if not k.startswith(('GCR_', 'GCRUST_'))}
    if platform.system() == 'Linux':
        subprocess.run(['bash', script], cwd=ROOT,
                       env=environment, stderr=subprocess.STDOUT, check=True, timeout=1800)
        return
    with tempfile.TemporaryDirectory(prefix='gcrust-linux-snapshot-') as temporary:
        output = Path(temporary)
        sources = gate.snapshot(ROOT, output)
        remote = subprocess.check_output([*SSH, 'mktemp -d /tmp/gcrust-linux-XXXXXXXXXX'],
                                         text=True, timeout=30).strip()
        if not re.fullmatch(r'/tmp/gcrust-linux-[A-Za-z0-9]+', remote):
            raise gate.GateFailure('unexpected remote staging directory')
        print(f'Linux host: {HOST}; retained staging: {remote}', flush=True)
        with (output / 'sources.tar.gz').open('rb') as archive:
            subprocess.run([*SSH, shlex.join(['tar', '-xzf', '-', '-C', remote])],
                           stdin=archive, check=True, timeout=120)
        def verify_remote():
            with (output / 'sources.json').open('rb') as manifest:
                subprocess.run([*SSH, shlex.join(['python3', '-c', REMOTE_SOURCE_CHECK, remote])],
                               stdin=manifest, check=True, timeout=60)
        verify_remote()
        gate.unchanged(ROOT, sources)
        # The isolated directory is retained for investigation. No existing
        # checkout or host configuration is changed. Native commands have their
        # own deadlines in addition to this outer bound.
        program = ("import os,subprocess,sys; "
                   "env={k:v for k,v in os.environ.items() if not k.startswith(('GCR_','GCRUST_'))}; "
                   "sys.exit(subprocess.run(['timeout','1800s','bash',sys.argv[2]], "
                   "cwd=sys.argv[1],env=env,stderr=subprocess.STDOUT).returncode)")
        subprocess.run([*SSH, shlex.join(['python3', '-c', program, remote, script])],
                       check=True, timeout=1830)
        verify_remote()
        gate.unchanged(ROOT, sources)


if __name__ == '__main__':
    main()
