#!/usr/bin/env bash
# Public-API-only placement oracle; no source/binary internals are inspected.
set -euo pipefail
if [[ $# -lt 1 || $# -gt 2 ]]; then
  printf 'usage: %s placed.def [receipt-directory]\n' "$0" >&2
  exit 2
fi
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
def=$(realpath -- "$1")
out=$(realpath -m -- "${2:-$repo/.work/openroad-placement-oracle}")
image=f152917ebc458bd015042e44d9dac5af69bcae9fb5657d5121cb4d7226ed0c67
mkdir -p -- "$out"
free -h > "$out/host-memory.txt"
# Max 5000 instances * 256 B ~= 1.3 MB; reference internals are unknown.
# Conservatively allow n*(n-1)/2 * 16 B ~= 200 MB for overlap pairs.
# No timing/cut/path state. Expected <1 GiB startup+fixture RSS; 2 GiB container cap.
if ! command -v podman >/dev/null || ! podman image exists "$image"; then
  printf '{"status":"unverified","reason":"pinned OpenROAD image unavailable"}\n' | tee "$out/result.json"
  exit 77
fi
if [[ -n "${REA_CLI:-}" ]]; then
  rea=(node "$REA_CLI")
elif command -v rea >/dev/null; then
  rea=(rea)
else
  cached=/home/cgao/.npm/_npx/a1722018f28f9df2/node_modules/rea-agents/scripts/rea.mjs
  if [[ ! -f "$cached" ]]; then
    printf '{"status":"unverified","reason":"REA runtime unavailable; set REA_CLI"}\n' | tee "$out/result.json"
    exit 77
  fi
  rea=(node "$cached")
fi
ulimit -v 8388608
if [[ "$(timeout 10s "${rea[@]}" --version)" != 6.7.0 ]]; then
  printf '{"status":"unverified","reason":"REA version differs from pinned 6.7.0"}\n' | tee "$out/result.json"
  exit 77
fi
python3 - "$repo" "$def" "$out" "$image" <<'PY'
import hashlib,json,re,subprocess,sys
from pathlib import Path
repo,source,out=map(Path,sys.argv[1:4]); image=sys.argv[4]
if source.stat().st_size > 16*1024*1024:
    raise SystemExit('fixture exceeds 16 MiB input budget')
raw=source.read_bytes()
if len(raw)>16*1024*1024:
    raise SystemExit('fixture exceeds 16 MiB input budget')
text=raw.decode('ascii')
pat=re.compile(r'(\+ PLACED\s*\(\s*)(-?\d+)(\s+)(-?\d+)(\s*\))')
matches=list(pat.finditer(text))
if not 2 <= len(matches) <= 5000:
    raise SystemExit('oracle requires 2..5000 placed instances')
first,second=matches[:2]
replacement=second.group(1)+first.group(2)+second.group(3)+first.group(4)+second.group(5)
negative=text[:second.start()]+replacement+text[second.end():]
(out/'positive.def').write_bytes(raw)
(out/'overlap.def').write_text(negative)
lef=(repo/'fixtures/placement-oracle.lef').read_bytes()
(out/'oracle.lef').write_bytes(lef)
ident={'input_def_sha256':hashlib.sha256(raw).hexdigest(),'overlap_def_sha256':hashlib.sha256(negative.encode()).hexdigest(),'lef_sha256':hashlib.sha256(lef).hexdigest(),'instances':len(matches),'negative_mutation':'second instance moved to first instance x/y','image_id':image,'image_digests':json.loads(subprocess.check_output(['podman','image','inspect',image]))[0]['RepoDigests'],'analysis_scope':{'corner':None,'wire_model':'none','unconstrained_endpoints':None,'placement_only':True,'drc_status':'unchecked','lvs_status':'unchecked','timing_status':'unchecked'}}
(out/'inputs.json').write_text(json.dumps(ident,indent=2)+'\n')
for name in ['positive','overlap']:
    # Paths are constant inside a single selected writable output mount.
    tcl='''if {[catch {
  read_lef /oracle/oracle.lef
  read_def /oracle/NAME.def
  check_placement -verbose -report_file_name /oracle/NAME.placement.json
} message]} {
  puts "RUSTROAD_PLACEMENT_REJECTED $message"
  exit 42
}
puts {RUSTROAD_PLACEMENT_ACCEPTED}
exit 0
'''.replace('NAME',name)
    (out/f'{name}.tcl').write_text(tcl)
    args=['run','--rm','--pull=never','--network=none','--read-only','--tmpfs=/tmp:rw,size=64m','--memory=2g','--memory-swap=2g','--cpus=2','--pids-limit=128','--volume='+str(out)+':/oracle:rw','--entrypoint=/OpenROAD-flow-scripts/tools/install/OpenROAD/bin/openroad',image,'-no_init','-no_splash','-threads','2','-exit','/oracle/'+name+'.tcl']
    scenario={'executable':'/usr/bin/podman','arguments':args,'working_directory':str(out),'environment':{'OMP_NUM_THREADS':'2'},'filesystem_observation_paths':[str(out/f'{name}.placement.json')],'terminal':{'columns':160,'rows':50,'scrollback':100},'timeout_ms':30000,'idle_timeout_ms':30000,'settle_ms':100,'limits':{'output_bytes':262144,'files':32,'file_bytes':262144,'processes':128,'filesystem_depth':2}}
    (out/f'{name}.scenario.json').write_text(json.dumps(scenario,indent=2)+'\n')
PY
for name in positive overlap; do
  /usr/bin/time -v -o "$out/$name.time.txt" timeout 60s \
    "${rea[@]}" capture-process "$out/$name.scenario.json" --json \
    > "$out/$name.capture.json" 2> "$out/$name.progress.jsonl"
done
python3 - "$out" <<'PY'
import json,re,sys
from pathlib import Path
out=Path(sys.argv[1]); runs={}
for name in ['positive','overlap']:
    record=json.loads((out/f'{name}.capture.json').read_text()); n=record['normalized_result']
    text=''.join(f.get('raw_data',f['data']) for f in n['frames'])
    (out/f'{name}.stdout.txt').write_text(text)
    timing=(out/f'{name}.time.txt').read_text()
    runs[name]={'exit':n['exit'],'truncated':n['truncated'],'cleanup':n['cleanup'],'evidence_id':record['evidence_id'],'outer_max_rss_kib':int(re.search(r'Maximum resident set size \(kbytes\): (\d+)',timing)[1]),'outer_wall_clock':re.search(r'Elapsed \(wall clock\) time \(h:mm:ss or m:ss\): (.+)',timing)[1].strip(),'accepted_marker':'RUSTROAD_PLACEMENT_ACCEPTED' in text,'rejected_marker':'RUSTROAD_PLACEMENT_REJECTED' in text,'overlap_diagnostic':bool(re.search(r'(?im)^.*(?:\[(?:WARNING|ERROR) DPL-[0-9]+\].*overlap|\boverlaps\b|overlap violations)',text))}
p,n=runs['positive'],runs['overlap']
passed=(p['exit']=={'code':0,'signal':0,'reason':'exited'} and p['accepted_marker'] and not p['truncated'] and n['exit']=={'code':42,'signal':0,'reason':'exited'} and n['rejected_marker'] and n['overlap_diagnostic'] and not n['truncated'])
result={'schema':'rustroad.openroad-placement-oracle.v1','status':'pass' if passed else 'fail','claim':'geometry-only fixture accepted and intentional overlap rejected; not PDK or signoff coverage','drc_status':'unchecked','lvs_status':'unchecked','timing_status':'unchecked','runs':runs}
(out/'result.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result,indent=2))
raise SystemExit(0 if passed else 1)
PY
