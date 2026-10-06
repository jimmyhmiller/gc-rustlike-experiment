#!/usr/bin/env python3
"""Sample the built gc-rust tree workload; do not run alongside timing tests."""
import os, pathlib, subprocess
ROOT = pathlib.Path(__file__).resolve().parents[2]
HERE = pathlib.Path(__file__).resolve().parent
env = {k:v for k,v in os.environ.items() if not k.startswith('GCR_')}
env.update(GCR_GC_WORKERS='1', GCR_NURSERY_MB='16', GCR_TENURED_MB='256',
           GCR_GC_LOG=str(HERE/'tree-gc-log.jsonl'))
with (ROOT/'target/performance-comparison/tree-profile-output.txt').open('w') as output:
    process = subprocess.Popen([str(ROOT/'target/performance-comparison/binarytrees/binarytrees')], env=env, stdout=output, stderr=output)
    try:
        subprocess.run(['/usr/bin/sample', str(process.pid), '3', '1', '-file', str(HERE/'tree-profile.txt')], check=True, timeout=30)
    finally:
        code = process.wait(timeout=120)
        if code: raise RuntimeError(f'profiled workload failed: {code}')
