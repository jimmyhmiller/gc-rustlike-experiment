#!/usr/bin/env python3
"""Prepare checksum-verified runtime dependencies for the offline s390x gate."""
from pathlib import Path
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]

def main():
    runtime = (ROOT / 'crates/gcrust-rt/Cargo.toml').read_text()
    staging = ROOT / 'target/big-endian-runtime/dependencies'
    (staging / 'src').mkdir(parents=True, exist_ok=True)
    if 'workspace' not in tomllib.loads(runtime):
        runtime += '\n[workspace]\n'
    # The runtime builds against the workspace's patched hashbrown, which is a
    # path source: cargo vendors only its registry dependencies.
    patch = tomllib.loads((ROOT / 'Cargo.toml').read_text())['patch']['crates-io']
    if set(patch) != {'hashbrown'}:
        raise SystemExit(f'unexpected workspace crates-io patches: {sorted(patch)}')
    runtime += f'\n[patch.crates-io]\nhashbrown = {{ path = "{ROOT / patch["hashbrown"]["path"]}" }}\n'
    (staging / 'Cargo.toml').write_text(runtime)
    (staging / 'src/lib.rs').write_text('// Dependency resolution only; never compiled.\n')
    manifest = str(staging / 'Cargo.toml')
    subprocess.run(['cargo', 'generate-lockfile', '--offline', '--manifest-path', manifest], cwd=ROOT, check=True)
    subprocess.run(['cargo', 'vendor', '--offline', '--locked', '--manifest-path', manifest,
                    str(ROOT / 'target/big-endian-runtime/vendor')], cwd=ROOT, check=True)
    (ROOT / 'target/big-endian-runtime/vendor-config.toml').write_text(
        '[source.crates-io]\nreplace-with = "vendored-sources"\n'
        '[source.vendored-sources]\ndirectory = "/vendor"\n'
        '[patch.crates-io]\nhashbrown = { path = "/hashbrown" }\n')

if __name__ == '__main__':
    main()
