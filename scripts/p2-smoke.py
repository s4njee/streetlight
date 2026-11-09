#!/usr/bin/env python3
"""Local P2 video tests: changing packet loss, encoder reopen, and negotiated AV1/H.264.
No input, no audio playback, isolated configuration, bounded processes.
"""
import json,os,pathlib,re,socket,subprocess,sys,tempfile,time,urllib.request
root=pathlib.Path(__file__).resolve().parent.parent; os.chdir(root)
if os.environ.get('XDG_SESSION_TYPE')!='wayland': raise SystemExit('Requires Wayland')
if 'zkde_screencast_unstable_v1' not in subprocess.check_output(['wayland-info'],stderr=subprocess.DEVNULL,text=True): raise SystemExit('Requires KWin')
for kind,port in [(socket.SOCK_STREAM,47990),(socket.SOCK_DGRAM,47800),(socket.SOCK_DGRAM,47801)]:
    with socket.socket(socket.AF_INET,kind) as s:
        if kind==socket.SOCK_STREAM: s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
        s.bind(('127.0.0.1',port))
scratch=pathlib.Path(tempfile.mkdtemp(prefix='streetlight-p2.')); env=os.environ.copy()
env['XDG_CONFIG_HOME']=str(scratch/'config'); wayland=env.get('WAYLAND_DISPLAY','wayland-0')
env['WAYLAND_DISPLAY']=wayland if wayland.startswith('/') else env['XDG_RUNTIME_DIR']+'/'+wayland
env['PIPEWIRE_RUNTIME_DIR']=env.get('PIPEWIRE_RUNTIME_DIR',env['XDG_RUNTIME_DIR'])
env['XDG_RUNTIME_DIR']=str(scratch/'runtime'); (scratch/'runtime').mkdir(mode=0o700)
env['RUST_LOG']='info';env['NO_COLOR']='1'
for key in ['SL_TEST_DROP','SL_TEST_BURST','SL_TEST_REORDER','SL_TEST_DROP_SCHEDULE']: env.pop(key,None)
files=[]; procs=[]
def bg(cmd,name,e=env):
    f=(scratch/name).open('w');files.append(f);p=subprocess.Popen(cmd,env=e,stdout=f,stderr=subprocess.STDOUT);procs.append(p);return p
def status():
    return json.load(urllib.request.urlopen('http://127.0.0.1:47990/api/status',timeout=2))['status']
try:
    print('Artifacts:',scratch,flush=True)
    host=bg(['timeout','150','./target/release/streetlight-host','serve'],'host.log')
    for _ in range(100):
        if (scratch/'runtime/streetlight/token').exists():break
        if host.poll() is not None:raise RuntimeError('Host failed')
        time.sleep(.1)
    for mode,codec,seconds in [('adaptive','hevc',35),('auto','auto',8),('h264','h264',8)]:
        ce=env.copy()
        if mode=='adaptive':ce.update(SL_TEST_DROP_SCHEDULE='0:0,3:0.10,13:0',SL_TEST_SEED='7')
        client=bg(['timeout',str(seconds+20),'./target/release/streetlight-client','127.0.0.1','--windowed','--no-input','--no-audio',
            '--decode-only','--dump',str(scratch/(mode+'.bin')),'--codec',codec,'--seconds',str(seconds),'--size','1920x1080','--fps','60','--bitrate','30000','--report','json'],mode+'.log',ce)
        for _ in range(150):
            text=(scratch/(mode+'.log')).read_text(); pin=re.search(r'Pairing PIN: (\d{4})',text)
            if pin: subprocess.run(['./target/release/streetlight-host','pair',pin[1]],env=env,stdout=subprocess.DEVNULL,timeout=3)
            if 'stream:' in text:break
            if client.poll() is not None:raise RuntimeError(text[-2000:])
            time.sleep(.1)
        else: raise RuntimeError('Stream did not start')
        s=status(); output=s['output_name']
        if 'Virtual-'+output not in subprocess.check_output(['wayland-info'],env=env,stderr=subprocess.DEVNULL,text=True):raise RuntimeError('Virtual output missing')
        pattern=bg(['timeout',str(seconds+5),'mpv','--focus-on=never','--really-quiet','--no-audio','--fs','--fs-screen-name='+output,'--screen-name='+output,
            'av://lavfi:testsrc2=size=1920x1080:rate=60,setparams=colorspace=bt470bg:range=limited:color_primaries=bt709:color_trc=iec61966-2-1'],mode+'.pattern.log')
        reports=[]
        while client.poll() is None:
            reports.append(status());time.sleep(.5)
        pattern.terminate();pattern.wait(timeout=5)
        if client.returncode:raise RuntimeError('Client failed: '+(scratch/(mode+'.log')).read_text()[-2000:])
        (scratch/(mode+'.status.json')).write_text(json.dumps(reports,indent=2))
        rows=[json.loads(s) for s in (scratch/(mode+'.log')).read_text().splitlines() if s.startswith('{')]
        last=rows[-1];rates=[s['bitrate_kbps'] for s in reports if s['streaming']]
        if last['decode_errors'] or last['frames_decoded']<seconds*45:raise RuntimeError('Decode/frame count failure: '+str(last))
        if mode=='adaptive':
            trough=min(rates)
            if trough>=30_000 or max(rates[rates.index(trough):])<=trough or max(rates)>30_000:raise RuntimeError('No bounded down/up adaptation: '+str(rates))
            h=(scratch/'host.log').read_text()
            if h.count('screencast node')!=1 or 'codec reopen' not in h:raise RuntimeError('Capture restarted or codec never reopened')
            if last['max_video_gap_ms']>500:raise RuntimeError('Adaptive change stalled video')
        elif last['frames_lost']:raise RuntimeError('Baseline codec test lost frames')
        # Decode raw dumps with the actual negotiated codec, independent of file extension.
        demux={'Hevc':'hevc','Av1':'obu','H264':'h264'}[next(s['codec'] for s in reports if s['streaming'])]
        result=subprocess.run(['timeout','20','ffmpeg','-v','error','-threads','1','-f',demux,'-i',str(scratch/(mode+'.bin')),'-f','null','-'],capture_output=True,text=True)
        (scratch/(mode+'.decode.log')).write_text(result.stderr)
        if result.returncode or result.stderr.strip():raise RuntimeError('Raw dump decode errors: '+result.stderr[-1000:])
        print('PASS:',mode,'codec',demux,'frames',last['frames_decoded'],'lost',last['frames_lost'],'rates',min(rates),max(rates),'last',rates[-1],flush=True)
        time.sleep(.2)
finally:
    for p in reversed(procs):
        if p.poll() is None:
            p.terminate()
            try:p.wait(timeout=5)
            except subprocess.TimeoutExpired:p.kill();p.wait()
    for f in files:f.close()
