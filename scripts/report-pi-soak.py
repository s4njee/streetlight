#!/usr/bin/env python3
import json,pathlib,sys
root=pathlib.Path(sys.argv[1]); seconds=int(sys.argv[2]); stats=[]; latency=[]
for line in (root/'client.log').read_text().splitlines():
    if line.startswith('{'):
        try: stats.append(json.loads(line))
        except ValueError: pass
    if line.startswith('LATENCY '): latency.append(json.loads(line[8:]))
if not stats: raise SystemExit('FAIL: no client stats')
s=stats[-1]; metrics=[json.loads(line) for line in (root/'metrics.jsonl').read_text().splitlines() if line.startswith('{')]
cpu=[m['cpu_percent'] for m in metrics if m['cpu_percent'] is not None]
summary=dict(duration_reports=len(stats),final=s,
    decode_p95_ms_max=max((r['decode']['p95_ms'] for r in latency if r['decode']['samples']),default=None),
    cpu_percent_average=sum(cpu)/len(cpu) if cpu else None,
    cpu_percent_max=max(cpu,default=None), temperature_c_max=max((m['temperature_c'] for m in metrics),default=None))
print(json.dumps(summary,indent=2)); (root/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
fail=[]
if len(stats)<seconds: fail.append('short run')
if s['frames_decoded']<seconds*55 or s['frames_presented']<seconds*55: fail.append('decode/presentation below 55 fps average')
for key in ['frames_lost','decode_errors','bad_packets','audio_underruns','audio_output_errors']:
    if s[key]: fail.append(f'{key}={s[key]}')
text=(root/'client.log').read_text()
if 'audio output:' not in text or 'audio disabled:' in text or 'audio output failed:' in text: fail.append('audio output not verified')
if not metrics: fail.append('no thermal/process metrics')
elif metrics[-1]['elapsed_s']<max(0,seconds-65): fail.append('metrics ended early')
if seconds>=120 and not cpu: fail.append('no CPU interval samples')
if cpu and sum(cpu)/len(cpu)>30: fail.append('average CPU exceeded 30% of one core')
if any(m['throttled']!='throttled=0x0' for m in metrics): fail.append('thermal/power throttling reported')
if fail: raise SystemExit('FAIL: '+', '.join(fail))
print('PASS: sustained 4K60 presentation and audio output (8 ms decode target evaluated separately)')
