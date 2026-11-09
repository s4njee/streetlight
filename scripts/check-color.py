#!/usr/bin/env python3
"""Check tagged testsrc2's six flat bars in a legacy BT.601/full diagnostic dump."""
import json, pathlib, statistics, subprocess, sys
path=pathlib.Path(sys.argv[1])
meta=json.loads(subprocess.check_output(['ffprobe','-v','error','-show_entries',
    'stream=width,height,color_range,color_space,color_transfer,color_primaries','-of','json',str(path)],timeout=20))['streams'][0]
expected_meta={'color_range':'pc','color_space':'bt470bg','color_transfer':'iec61966-2-1','color_primaries':'bt709'}
if any(meta.get(k)!=v for k,v in expected_meta.items()): raise SystemExit('FAIL: unexpected color metadata: '+json.dumps(meta))
w,h=meta['width'],meta['height']
raw=subprocess.check_output(['ffmpeg','-v','error','-threads','1','-i',str(path),'-vf',r'select=eq(n\,120)',
    '-frames:v','1','-pix_fmt','rgb24','-f','rawvideo','-'],timeout=25)
if len(raw)!=w*h*3: raise SystemExit('FAIL: incomplete RGB frame')
reference=[(255,0,0),(0,255,0),(255,255,0),(0,0,255),(255,0,255),(0,255,255)]
samples=[]
for i in range(6):
    x=w*(2*i+1)//12; y=h*100//1080
    samples.append(tuple(int(statistics.median(raw[((y+dy)*w+x+dx)*3+c]
        for dx in range(-4,5) for dy in range(-4,5))) for c in range(3)))
error=max(abs(a-b) for expected,actual in zip(reference,samples) for a,b in zip(expected,actual))
report={'metadata':meta,'reference_rgb':reference,'sample_rgb':samples,'max_channel_error':error,'tolerance':6}
path.with_suffix('.color-check.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps(report))
if error>6: raise SystemExit('FAIL: flat colors exceed six RGB levels; check source tags and VPP properties')
print('PASS: BT.601/full metadata and six flat-color samples')
