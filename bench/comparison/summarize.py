#!/usr/bin/env python3
"""Render recorded measurements without hardcoded host or timing metadata."""
import json, pathlib, statistics
HERE = pathlib.Path(__file__).resolve().parent
result = json.loads((HERE/'results.json').read_text())
env = result['environment']
o3 = json.loads((HERE/'rust-o3.json').read_text())
lines = ['# Measured performance gaps', '',
         f"Host: {env['platform']}, {env['machine']}. Revision: `{env['git']}`.",
         f"Rust: {env['rust']}. JVM: {env['java'].splitlines()[0]}.",
         f"Three process forks; {env['warmup']} warmups and {env['samples']} measured iterations per fork.",
         'See [methodology and representation differences](README.md); timings are milliseconds.', '',
         '| Workload | gc-rust | Rust O2 | Rust O3 | JVM G1 | gc-rust / Rust O3 | gc-rust / JVM |',
         '|---|---:|---:|---:|---:|---:|---:|']
for name, case in result['benchmarks'].items():
    m = case['median_ms']
    rust3 = o3['benchmarks'][name]['median_ms']['rust']
    lines.append(f"| {name} | {m['gcr']:.3f} | {m['rust']:.3f} | {rust3:.3f} | {m['java']:.3f} | {m['gcr']/rust3:.2f}× | {m['gcr']/m['java']:.2f}× |")
lines += ['', '## Variation and memory', '',
          'Ranges below are medians of individual process forks, not confidence intervals. RSS includes warmup.', '',
          '| Workload | Language | Fork median range (ms) | Median peak RSS (MiB) |',
          '|---|---|---:|---:|']
for name, case in result['benchmarks'].items():
    for lang in ['gcr','rust','java']:
        runs = [r for r in case['runs'] if r['language'] == lang]
        medians = [statistics.median(x['ns']/1e6 for x in r['records'][env['warmup']:]) for r in runs]
        rss = statistics.median(r['peak_rss_bytes']/1048576 for r in runs)
        lines.append(f'| {name} | {lang} | {min(medians):.3f}–{max(medians):.3f} | {rss:.1f} |')
lines += ['', '## Collector contribution', '',
          'Full-process counters include all warmup and measured iterations. Pause share divides total pauses by summed workload intervals; it is not a measured-iteration-only fraction.', '',
          '| Workload | Minor / major collections (first fork) | Allocated bytes per iteration | Pause share across forks | Maximum pause across forks (ms) |',
          '|---|---:|---:|---:|---:|']
for name, case in result['benchmarks'].items():
    runs = [r for r in case['runs'] if r['language']=='gcr']
    gc = runs[0]['gc']
    allocated = statistics.median(r['gc']['alloc_bytes']/len(r['records']) for r in runs)
    shares = [r['gc']['gc_pause_total_ms'] / sum(x['ns']/1e6 for x in r['records']) * 100 for r in runs]
    lines.append(f"| {name} | {gc['gc_minor']} / {gc['gc_major']} | {allocated:,.0f} | {min(shares):.1f}–{max(shares):.1f}% | {max(r['gc']['gc_pause_max_ms'] for r in runs):.3f} |")
(HERE/'MEASUREMENTS.md').write_text('\n'.join(lines)+'\n')
print('\n'.join(lines))
