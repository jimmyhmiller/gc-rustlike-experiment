#!/usr/bin/env python3
"""Reproducible warm-process comparison of the attributed bench/suite ports.
Generated wrappers retain the original sources and output; see README.md.
"""
import argparse, hashlib, json, math, os, pathlib, platform, random, re, statistics, subprocess, time
ROOT = pathlib.Path(__file__).resolve().parents[2]
HERE = ROOT / 'bench/comparison'
BUILD = ROOT / 'target/performance-comparison'
CASES = {'nbody': (1000000, 'app'), 'spectralnorm': (1000, 'spectralnorm'),
         'fannkuchredux': (9, 'fannkuchredux'), 'binarytrees': (14, 'app'), 'scalar': (0, 'Kernel'), 'array': (0, 'Kernel'), 'nbody_objects': (1000000, 'app')}

def run(cmd, **kw):
    try:
        return subprocess.run([str(x) for x in cmd], check=True, capture_output=True, text=True, timeout=600, **kw)
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f"Command failed: {error.cmd}\n{error.stdout}\n{error.stderr}") from error

def signature(output):
    return re.findall(r'-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?', output)

def matches(a, b):
    if len(a) != len(b): return False
    return all(int(x) == int(y) if '.' not in x and '.' not in y and 'e' not in x.lower() and 'e' not in y.lower()
               else math.isclose(float(x), float(y), rel_tol=1e-7, abs_tol=1e-8) for x, y in zip(a, b))

def build(name, size, main, warm, samples, rust_opt, env):
    d = BUILD / name
    d.mkdir(parents=True, exist_ok=True)
    src = (HERE / 'kernels' if name in ('scalar','array','nbody_objects') else ROOT / 'bench/suite') / name
    g = (src / (name + '.gcr')).read_text().replace('fn main()', 'fn workload()')
    for old in ['let steps = 5000000;', 'let n = 3000;', 'let n = 11;', 'let n = 16;']:
        if old in g: g = g.replace(old, f'let {"steps" if "steps" in old else "n"} = {size};')
    g += f'''\nextern "C" fn benchmark_clock_ns() -> i64;
fn main() -> i64 {{
    let mut i = 0;
    while i < {warm + samples} {{
        let start = benchmark_clock_ns();
        let result = workload();
        let elapsed = benchmark_clock_ns() - start;
        println("@sample");
        print_int(i);
        print_int(elapsed);
        i = i + 1;
    }}
    0
}}\n'''
    (d / (name + '.gcr')).write_text(g)
    r = (src / (name + '.rs')).read_text().replace('fn main()', 'fn workload()')
    r += f'''\nfn main() {{ for i in 0..{warm + samples} {{
        let start = std::time::Instant::now(); workload();
        println!("@sample {{}} {{}}", i, start.elapsed().as_nanos());
    }} }}\n'''
    (d / (name + '.rs')).write_text(r)
    j = (src / (name + '.java')).read_text().replace('void main(String[] args)', 'void workload(String[] args)')
    j += f'''\nclass Runner {{ public static void main(String[] args) {{
        for (int i = 0; i < {warm + samples}; i++) {{
            long start = System.nanoTime(); {main}.workload(args);
            System.out.println("@sample " + i + " " + (System.nanoTime() - start));
        }}
    }} }}\n'''
    (d / (name + '.java')).write_text(j)
    run(['rustc', '-C', f'opt-level={rust_opt}', '-C', 'panic=abort', d/(name+'.rs'), '-o', d/'rust'])
    run(['javac', '-d', d, d/(name+'.java')])
    run([ROOT/'target/release/gcr', 'build', d/(name+'.gcr'), '--link-arg', BUILD/'clock.o'], cwd=d, env=env)
    return {'gcr': [str(d/name)], 'rust': [str(d/'rust'), str(size)],
            'java': ['java', '-Xms16m', '-Xmx512m', '-XX:+UseG1GC', '-Duser.language=en', '-Duser.country=US', '-cp', str(d), 'Runner', str(size)]}

def main():
    p = argparse.ArgumentParser()
    p.add_argument('--warmup', type=int, default=20)
    p.add_argument('--samples', type=int, default=10)
    p.add_argument('--forks', type=int, default=3)
    p.add_argument('--rust-opt-level', type=int, choices=[2,3], default=2)
    p.add_argument('--workers', type=int, default=1)
    p.add_argument('--nursery-mb', type=int, default=16)
    p.add_argument('--languages', nargs='+', choices=['gcr','rust','java'], default=['gcr','rust','java'])
    p.add_argument('--output', type=pathlib.Path, default=HERE/'results.json')
    p.add_argument('--cases', nargs='+', choices=CASES, default=list(CASES))
    args = p.parse_args()
    if min(args.warmup, args.samples, args.forks, args.workers, args.nursery_mb) < 1: p.error('counts must be positive')
    BUILD.mkdir(parents=True, exist_ok=True)
    run(['clang', '-O2', '-c', HERE/'clock.c', '-o', BUILD/'clock.o'])
    env = {k:v for k,v in os.environ.items() if not k.startswith('GCR_') and k != 'GCRUST_RUNTIME_LIB'}
    env['GCR_GC_WORKERS'] = str(args.workers)
    env['GCR_NURSERY_MB'] = str(args.nursery_mb)
    result = {'environment': {'platform': platform.platform(), 'machine': platform.machine(),
        'git': run(['git','rev-parse','HEAD'], cwd=ROOT).stdout.strip(),
        'git_dirty': bool(run(['git', 'status', '--porcelain'], cwd=ROOT).stdout.strip()),
        'implementation_sha256': {str(x.relative_to(ROOT)): hashlib.sha256(x.read_bytes()).hexdigest()
            for directory in (ROOT/'src', ROOT/'crates/gcrust-rt/src')
            for x in sorted(directory.rglob('*.rs'))},
        'rust': run(['rustc','--version']).stdout.strip(), 'java': run(['java','-version']).stderr.strip(),
        'gcr_workers': args.workers, 'gcr_nursery_mb': args.nursery_mb, 'gcr_tenured_mb': 256,
        'rust_opt_level': args.rust_opt_level, 'gcr_opt_level': 3, 'order_seed': 20261006,
        'cpu': run(['sysctl','-n','machdep.cpu.brand_string']).stdout.strip(),
        'memory_bytes': int(run(['sysctl','-n','hw.memsize']).stdout.strip()),
        'warmup': args.warmup, 'samples': args.samples, 'forks': args.forks}, 'benchmarks': {}}
    rng = random.Random(20261006)
    for name in args.cases:
        size, java_main = CASES[name]
        print('Building', name, flush=True)
        cmds = build(name, size, java_main, args.warmup, args.samples, args.rust_opt_level, env)
        sources = (HERE/'kernels' if name in ('scalar','array','nbody_objects') else ROOT/'bench/suite')/name
        entry = {'input': size, 'source_sha256': {x.name:hashlib.sha256(x.read_bytes()).hexdigest() for x in sources.iterdir() if x.suffix in ('.rs','.gcr','.java')}, 'runs': []}
        reference = None
        for fork in range(args.forks):
            langs = list(args.languages); rng.shuffle(langs)
            for lang in langs:
                metrics = BUILD/name/f'metrics-{fork}.json'
                local = dict(env)
                if lang == 'gcr': local['GCR_METRICS_FILE'] = str(metrics)
                start = time.perf_counter()
                proc = run(['/usr/bin/time', '-l'] + cmds[lang], env=local)
                wall = time.perf_counter() - start
                chunks = re.split(r'@sample\s+(\d+)\s+(\d+)\n', proc.stdout)
                records = []
                for i in range(0, len(chunks)-1, 3):
                    sig = signature(chunks[i]); index = int(chunks[i+1]); ns = int(chunks[i+2])
                    if not sig or index != len(records) or ns <= 0:
                        raise RuntimeError(f'invalid sample: {name}/{lang}/{index}')
                    if reference is None: reference = sig
                    if not matches(reference, sig): raise RuntimeError(f'{name}/{lang}/{index}: mismatch {sig} vs {reference}')
                    records.append({'index': index, 'ns': ns, 'signature': sig})
                if len(records) != args.warmup + args.samples or chunks[-1].strip():
                    raise RuntimeError(f'invalid records: {name}/{lang}')
                if lang == 'gcr' and not metrics.exists():
                    raise RuntimeError(f'missing GC metrics: {name}/{fork}')
                rss = re.search(r'(\d+)\s+maximum resident set size', proc.stderr)
                if rss is None: raise RuntimeError('missing macOS RSS measurement')
                data = {'language':lang, 'fork':fork, 'command':cmds[lang], 'wall_s':wall,
                    'peak_rss_bytes':int(rss.group(1)) if rss else None, 'records':records,
                    'stderr':proc.stderr}
                if lang == 'gcr' and metrics.exists(): data['gc'] = json.loads(metrics.read_text())
                entry['runs'].append(data)
                print(name, lang, fork, 'median ms', round(statistics.median(x['ns'] for x in records[args.warmup:])/1e6,3), flush=True)
        entry['median_ms'] = {lang:statistics.median(x['ns']/1e6 for r in entry['runs'] if r['language']==lang for x in r['records'][args.warmup:]) for lang in args.languages}
        result['benchmarks'][name] = entry
        args.output.write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps({k:v['median_ms'] for k,v in result['benchmarks'].items()}, indent=2))

if __name__ == '__main__': main()
