"""Compare automatic GC worker policy using already-built tree wrappers.

Run after run.py has recorded baseline-control.json and tree-confirmation.json;
do not run alongside timings, builds or other CPU-intensive work.
"""
import importlib.util,json,os,re,statistics,time
from pathlib import Path
root=Path(__file__).resolve().parents[2];folder=root/'bench/comparison/optimization-2026-10-06'
spec=importlib.util.spec_from_file_location('comparison',root/'bench/comparison/run.py');mod=importlib.util.module_from_spec(spec);spec.loader.exec_module(mod)
a=json.loads((folder/'baseline-control.json').read_text());z=json.loads((folder/'tree-confirmation.json').read_text())
assert a['benchmarks']['binarytrees']['source_sha256']==z['benchmarks']['binarytrees']['source_sha256']
reference=a['benchmarks']['binarytrees']['runs'][0]['records'][0]['signature'];result={'workers':'automatic (GCR_GC_WORKERS unset)','nursery_mb':16,'tenured_mb':256,'warmup':20,'samples':10,'forks':3,'baseline_revision':a['environment']['git'],'optimized_implementation_sha256':z['environment']['implementation_sha256'],'runs':[]}
for fork in range(3):
 for label,data in ([('baseline',a),('optimized',z)] if fork%2==0 else [('optimized',z),('baseline',a)]):
  command=next(r['command'] for r in data['benchmarks']['binarytrees']['runs'] if r['language']=='gcr')
  metrics=root/f'target/default-workers-{label}-{fork}.json'
  env={k:v for k,v in os.environ.items() if not k.startswith('GCR_') and k!='GCRUST_RUNTIME_LIB'};env.update(GCR_NURSERY_MB='16',GCR_TENURED_MB='256',GCR_METRICS_FILE=str(metrics))
  start=time.perf_counter();proc=mod.run(['/usr/bin/time','-l']+command,env=env);wall=time.perf_counter()-start
  chunks=re.split(r'@sample\s+(\d+)\s+(\d+)\n',proc.stdout);records=[]
  for i in range(0,len(chunks)-1,3):
   sig=mod.signature(chunks[i]);index=int(chunks[i+1]);ns=int(chunks[i+2]);assert index==len(records) and ns>0 and mod.matches(reference,sig)
   records.append({'index':index,'ns':ns,'signature':sig})
  assert len(records)==30 and not chunks[-1].strip()
  gc=json.loads(metrics.read_text());assert gc['alloc_objects']==96665730 and gc['alloc_bytes']==3866628960
  run={'stage':label,'fork':fork,'command':command,'wall_s':wall,'records':records,'gc':gc,'stderr':proc.stderr};result['runs'].append(run)
  print(label,fork,statistics.median(x['ns'] for x in records[20:])/1e6,flush=True)
result['median_ms']={label:statistics.median(x['ns']/1e6 for r in result['runs'] if r['stage']==label for x in r['records'][20:]) for label in ('baseline','optimized')}
(folder/'default-workers.json').write_text(json.dumps(result,indent=2)+'\n');print(result['median_ms'])
