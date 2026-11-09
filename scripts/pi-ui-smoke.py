#!/usr/bin/env python3
"""Exercise the Pi's real WebKit bridge and asynchronous stream start, without input/audio."""
import json, os, pathlib, re, shlex, subprocess, sys, tempfile, time, urllib.request

pi = sys.argv[1] if len(sys.argv)>1 else 'neo.local'
host = sys.argv[2] if len(sys.argv)>2 else '192.168.1.99'
if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.@-]*',pi) or not re.fullmatch(r'[A-Za-z0-9_.:-]+',host): raise SystemExit('Invalid SSH target/host')
root = pathlib.Path(__file__).resolve().parent.parent
os.chdir(root)
if os.environ.get('XDG_SESSION_TYPE') != 'wayland': raise SystemExit('Requires host Wayland session')
import socket
for kind,port in [(socket.SOCK_STREAM,47990),(socket.SOCK_DGRAM,47800),(socket.SOCK_DGRAM,47801)]:
    with socket.socket(socket.AF_INET,kind) as sock:
        if kind == socket.SOCK_STREAM: sock.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
        sock.bind(('127.0.0.1',port))
scratch=pathlib.Path(tempfile.mkdtemp(prefix='streetlight-pi-ui.'))
env=os.environ.copy()
env['XDG_CONFIG_HOME']=str(scratch/'config')
wayland=env.get('WAYLAND_DISPLAY','wayland-0')
env['WAYLAND_DISPLAY']=wayland if wayland.startswith('/') else env['XDG_RUNTIME_DIR']+'/'+wayland
env['PIPEWIRE_RUNTIME_DIR']=env.get('PIPEWIRE_RUNTIME_DIR',env['XDG_RUNTIME_DIR'])
env['XDG_RUNTIME_DIR']=str(scratch/'runtime'); (scratch/'runtime').mkdir(mode=0o700)
env['NO_COLOR']='1'; env['RUST_LOG']='info'
for key in ['SL_TEST_DROP','SL_TEST_BURST','SL_TEST_REORDER','SL_TEST_DROP_SCHEDULE']: env.pop(key,None)
remote=subprocess.check_output(['ssh','-o','BatchMode=yes',pi,'mktemp -d /tmp/streetlight-ui.XXXXXXXX'],text=True).strip()
if not re.fullmatch(r'/tmp/streetlight-ui\.[A-Za-z0-9]+',remote): raise SystemExit('Invalid scratch directory')
processes=[]; files=[]

def background(command, name):
    f=(scratch/name).open('w'); files.append(f)
    p=subprocess.Popen(command,env=env,stdout=f,stderr=subprocess.STDOUT); processes.append(p); return p

def status():
    with urllib.request.urlopen('http://127.0.0.1:47990/api/status',timeout=2) as r: return json.load(r)['status']

try:
    print('Artifacts:',scratch, 'Pi:',remote,flush=True)
    server=background(['timeout','90','./target/release/streetlight-host','serve'],'host.log')
    for _ in range(100):
        if (scratch/'runtime/streetlight/token').exists(): break
        if server.poll() is not None: raise RuntimeError('Host failed')
        time.sleep(.1)
    prefix=f'export XDG_CONFIG_HOME={shlex.quote(remote+"/config")} XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus NO_COLOR=1; cd "$HOME/streetlight-dev"; '
    if os.environ.get('SL_TEST_CURSOR')=='1': prefix='export SL_CLIENT_CURSOR=1; '+prefix
    bootstrap=prefix+f'timeout 20 ./target/release/streetlight-client {shlex.quote(host)} --windowed --no-input --no-audio --dump {shlex.quote(remote+"/bootstrap.hevc")} --seconds 1'
    prep=background(['timeout','25','ssh','-o','BatchMode=yes',pi,bootstrap],'bootstrap.log')
    for _ in range(100):
        text=(scratch/'bootstrap.log').read_text()
        match=re.search(r'Pairing PIN: (\d{4})',text)
        if match:
            result=subprocess.run(['./target/release/streetlight-host','pair',match[1]],env=env,capture_output=True,text=True,timeout=3)
            if 'PIN accepted' in result.stdout: break
        if prep.poll() is not None: raise RuntimeError('Bootstrap exited before pairing: '+text[-2000:])
        time.sleep(.1)
    if prep.wait(timeout=22): raise RuntimeError('Bootstrap failed')
    for _ in range(100):
        if not status()['streaming']: break
        time.sleep(.05)
    else: raise RuntimeError('Bootstrap stream did not stop')
    unsupported=prefix+f'timeout 10 ./target/release/streetlight-client {shlex.quote(host)} --windowed --no-input --no-audio --decode-only --codec av1 --seconds 1'
    rejected=subprocess.run(['ssh','-o','BatchMode=yes',pi,unsupported],env=env,capture_output=True,text=True,timeout=15)
    (scratch/'unsupported-av1.log').write_text(rejected.stdout+rejected.stderr)
    if rejected.returncode==0 or 'no shared hardware codec' not in rejected.stdout+rejected.stderr:
        raise RuntimeError('Pi must reject unsupported AV1 before capture')
    script='''(async () => {
      setTimeout(() => window.streetlight.call("quit"), 15000);
      const sl=window.streetlight;
      await sl.call("setSettings", {resolution:"1920x1080",fps:60,bitrate_kbps:30000,codec:"Auto",fec_percent:20,pace_mbps:0,virtual_display:true});
      const welcome=await sl.call("connect", {host:__HOST__});
      if (!welcome.paired) throw new Error("bootstrap pairing missing");
      const first=sl.call("start", {app:"desktop"});
      setTimeout(()=>sl.call("stop"),0);
      let cancelled=false;
      try { await first; } catch(e) { cancelled=true; }
      if (!cancelled) throw new Error("in-flight start was not cancelled");
      await sl.call("start", {app:"desktop"});
    })();'''.replace('__HOST__',json.dumps(host))
    command=prefix+f'SL_UI_EVAL={shlex.quote(script)} timeout 25 ./target/release/streetlight-client --windowed --no-input --no-audio > {shlex.quote(remote+"/ui.log")} 2>&1 & p=$!; echo $p > {shlex.quote(remote+"/ui.pid")}; wait $p'
    ui=background(['timeout','30','ssh','-o','BatchMode=yes',pi,command],'ui.ssh.log')
    for _ in range(120):
        s=status()
        if s['streaming'] and s['width']==1920 and (scratch/'host.log').read_text().count('streaming 1920x1080@60') >= 3: break
        if ui.poll() is not None: raise RuntimeError('Webview exited before starting stream')
        time.sleep(.1)
    else: raise RuntimeError('Webview did not start a stream')
    outputs=subprocess.check_output(['wayland-info'],env=env,stderr=subprocess.DEVNULL,text=True)
    output=s['output_name']
    if 'Virtual-'+output not in outputs: raise RuntimeError('Virtual target not found')
    pattern=background(['timeout','16','mpv','--focus-on=never','--really-quiet','--no-audio','--fs','--fs-screen-name='+output,'--screen-name='+output,'av://lavfi:testsrc2=size=1920x1080:rate=60'],'pattern.log')
    time.sleep(6)
    s=status(); (scratch/'status.json').write_text(json.dumps(s,indent=2)+'\n')
    feedback=s.get('last_feedback') or {}
    if not s['streaming'] or s.get('codec')!='Hevc' or feedback.get('frames_lost') != 0 or feedback.get('last_frame',0)<250:
        raise RuntimeError('Streaming/feedback verification failed: '+json.dumps(s))
    if os.environ.get('SL_TEST_CURSOR')=='1':
        ui_log=subprocess.check_output(['ssh','-o','BatchMode=yes',pi,f'cat {shlex.quote(remote+"/ui.log")}'],text=True)
        if 'cursor overlay: shape' not in ui_log: raise RuntimeError('Cursor texture was not created')
        (scratch/'ui.log').write_text(ui_log)
        with (scratch/'kms.log').open('w') as kms:
            subprocess.run(['ssh','-o','BatchMode=yes',pi,'timeout 5 kmsprint -p'],stdout=kms,check=True,timeout=8)
    if ui.wait(timeout=25): raise RuntimeError('Webview exited with error')
    print('PASS: Pi WebKit connect → paired → cancel in-flight start → restart Desktop; feedback:',json.dumps(feedback),flush=True)
finally:
    for p in reversed(processes):
        if p.poll() is None:
            p.terminate()
            try: p.wait(timeout=5)
            except subprocess.TimeoutExpired: p.kill(); p.wait()
    subprocess.run(['ssh','-o','BatchMode=yes','-o','ConnectTimeout=5',pi,
        f'if test -f {shlex.quote(remote+"/ui.pid")}; then kill $(cat {shlex.quote(remote+"/ui.pid")}) 2>/dev/null || true; fi'],timeout=8,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    for f in files: f.close()
