#!/usr/bin/env python3
import json,pathlib,re,sys,os
root=pathlib.Path(sys.argv[1]); failed=[]
video=[json.loads(line) for line in (root/'video.log').read_text().splitlines() if line.startswith('{')]
if not video or video[-1]['frames']<440 or video[-1]['frames_lost'] or (root/'video.decode.log').read_text().strip():
    failed.append('video: expected >=55 fps average, zero loss/decode errors')
probe=(root/'probe.log').read_text()
required=['GATE open','ENTER 480 270','MOTION 960 540','MOTION 1440 810','MOTION 970 540','MOTION 980 540','MOTION 990 540','MOTION 990 560',
          'SCROLL 0.00 1.00','SCROLL 0.00 -1.00','BUTTON 8 down at 960 540','BUTTON 1 down at 960 540','TOUCH begin 576 324','TOUCH end Δ 192 108']
required += [f'KEY {direction} evdev={code} ' for code in [54,97,30] for direction in ['down','up']]
if os.environ.get('SL_PROBE_POINTER_LOCK'):
    required=[s for s in required if not s.startswith(('ENTER','MOTION','BUTTON'))]
    relative=[tuple(map(int,p)) for p in re.findall(r'^RELATIVE (-?\d+) (-?\d+)$',probe,re.M)]
    if 'POINTER_LOCK active=true' not in probe or sum(x for x,y in relative)!=30 or sum(y for x,y in relative)!=20:
        failed.append('pointer lock: expected active lock and relative totals +30,+20: '+str(relative))
else:
    positions=[probe.find(text) for text in ['MOTION 970 540','MOTION 980 540','MOTION 990 540','MOTION 990 560']]
    if positions!=sorted(positions): failed.append('relative motion order')
    motions=[tuple(map(int,m)) for m in re.findall(r'MOTION ([-\d]+) ([-\d]+)',probe)]
    expected=[(970,540),(980,540),(990,540),(990,560)]
    if expected[0] in motions:
        start=motions.index(expected[0]); end=next((i for i in range(start,len(motions)) if motions[i]==(960,540)),len(motions))
        if motions[start:end]!=expected: failed.append('relative moves were not exactly +10,+10,+10,+20')
for text in required:
    if text not in probe: failed.append('input: missing '+text)
scrolls=re.findall(r'SCROLL ([-\d.]+) ([-\d.]+)',probe)
if scrolls!=[('0.00','1.00'),('0.00','-1.00')]: failed.append('scroll sequence was not exactly down/up')
if probe.find('BUTTON 8 down at 960 540')>probe.find('BUTTON 1 down at 960 540'): failed.append('button order')
if 'PASS: pad press/release, stick, removal' not in (root/'pad.log').read_text(): failed.append('gamepad')
audio=(root/'selftest.log').read_text()
cursor=re.search(r'cursor positions (\d+) bitmaps (\d+) valid_bitmap true',audio)
if not cursor or int(cursor[1])<1 or int(cursor[2])<1: failed.append('cursor metadata/bitmap did not reach client')
m=re.search(r'audio loudest 1 s: rms [\d.]+ \(([-\d.]+) dBFS\), ~(\d+) Hz',audio)
if not m or not (-40 <= float(m[1]) <= -28 and 430 <= int(m[2]) <= 450): failed.append('audio: expected 440 Hz at -40..-28 dBFS')
ui=root/'ui-status.json'
if ui.exists():
    s=json.loads(ui.read_text())['status']; fb=s.get('last_feedback') or {}
    if not s['streaming'] or fb.get('frames_lost')!=0 or fb.get('last_frame',0)<120: failed.append('webview UI stream/feedback')
else: print('SKIP: webview UI (WebKitGTK development package unavailable)')
print('Video:',video[-1] if video else 'missing')
print('Audio:',m[0] if m else 'missing')
if failed: raise SystemExit('FAIL:\n'+'\n'.join(failed))
print('PASS: video, gated input, gamepad, audio'+(', webview UI' if ui.exists() else ''))
