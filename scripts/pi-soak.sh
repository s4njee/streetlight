#!/usr/bin/env bash
# 4K60 HEVC 80 Mbit/s soak over Ethernet, including actual audio playback on the Pi.
set -euo pipefail
cd "$(dirname "$0")/.."
PI=${1:-neo.local}
HOST_ADDR=${2:-192.168.1.99}
SECONDS_TO_RUN=${SL_SOAK_SECONDS:-1800}
[[ $PI =~ ^[a-zA-Z0-9_.@-]+$ && $HOST_ADDR =~ ^[a-zA-Z0-9_.:-]+$ && $SECONDS_TO_RUN =~ ^[0-9]+$ ]] || exit 2
(( SECONDS_TO_RUN >= 15 && SECONDS_TO_RUN <= 3600 )) || exit 2
[[ ${XDG_SESSION_TYPE:-} == wayland ]] || exit 1
interfaces=$(wayland-info 2>/dev/null)
[[ $interfaces == *zkde_screencast_unstable_v1* ]] || exit 1
python3 - <<'PY'
import socket
for kind,port in [(socket.SOCK_STREAM,47990),(socket.SOCK_DGRAM,47800),(socket.SOCK_DGRAM,47801)]:
    with socket.socket(socket.AF_INET,kind) as s:
        if kind == socket.SOCK_STREAM: s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
        s.bind(('127.0.0.1',port))
PY
S=$(mktemp -d /tmp/streetlight-soak.XXXXXXXX)
export XDG_CONFIG_HOME="$S/config"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-0}"
[[ $WAYLAND_DISPLAY == /* ]] || export WAYLAND_DISPLAY="${XDG_RUNTIME_DIR:?}/$WAYLAND_DISPLAY"
export PIPEWIRE_RUNTIME_DIR="${PIPEWIRE_RUNTIME_DIR:-${XDG_RUNTIME_DIR:?}}"
export XDG_RUNTIME_DIR="$S/runtime"
mkdir -m700 "$XDG_RUNTIME_DIR"
R=$(ssh -o BatchMode=yes "$PI" 'mktemp -d /tmp/streetlight-soak.XXXXXXXX')
[[ $R == /tmp/streetlight-soak.* && $R != *[!a-zA-Z0-9./_-]* ]] || exit 1
HP= CP= MP= TP= MON=
cleanup() {
    for pid in "$TP" "$MON" "$MP" "$CP" "$HP"; do
        if [[ -n $pid ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    done
    ssh -o BatchMode=yes -o ConnectTimeout=5 "$PI" "for file in '$R/client.pid' '$R/monitor.pid'; do if test -f \"\$file\"; then kill \$(cat \"\$file\") 2>/dev/null || true; fi; done" || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
printf 'Host artifacts: %s\nPi artifacts: %s:%s\nDuration: %s seconds\n' "$S" "$PI" "$R" "$SECONDS_TO_RUN"
unset SL_TEST_DROP SL_TEST_BURST SL_TEST_REORDER SL_TEST_DROP_SCHEDULE
export RUST_LOG=info NO_COLOR=1
timeout "$((SECONDS_TO_RUN+120))" ./target/release/streetlight-host serve > "$S/host.log" 2>&1 & HP=$!
for _ in {1..100}; do [[ -f "$XDG_RUNTIME_DIR/streetlight/token" ]] && break; kill -0 "$HP"; sleep .1; done
remote="export XDG_CONFIG_HOME='$R/config' XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus RUST_LOG=info NO_COLOR=1; unset GDK_DEBUG; cd \"\$HOME/streetlight-dev\"; timeout '$((SECONDS_TO_RUN+45))' ./target/release/streetlight-client '$HOST_ADDR' --windowed --no-input --seconds '$SECONDS_TO_RUN' --size 3840x2160 --fps 60 --bitrate 80000 --latency --report json > '$R/client.log' 2>&1 & p=\$!; echo \$p > '$R/client.pid'; wait \$p"
timeout "$((SECONDS_TO_RUN+55))" ssh -o BatchMode=yes "$PI" "$remote" > "$S/client.ssh.log" 2>&1 & CP=$!
for _ in {1..120}; do
    ssh -o BatchMode=yes "$PI" "head -n 30 '$R/client.log' 2>/dev/null" > "$S/startup.log" || true
    pin=$(sed -n 's/.*Pairing PIN: \([0-9]\{4\}\).*/\1/p' "$S/startup.log" | head -1)
    if [[ -n $pin ]]; then ./target/release/streetlight-host pair "$pin" >> "$S/pair.log" || true; fi
    if rg -q '^stream:' "$S/startup.log"; then break; fi
    kill -0 "$CP" 2>/dev/null || { cat "$S/startup.log"; cat "$S/client.ssh.log"; exit 1; }
    sleep .1
done
rg -q '^stream:' "$S/startup.log" || exit 1
output=$(python3 -c 'import json,urllib.request; print(json.load(urllib.request.urlopen("http://127.0.0.1:47990/api/status"))["status"]["output_name"])')
monitors=$(wayland-info 2>/dev/null)
[[ $monitors == *"Virtual-$output"* ]] || { echo 'Virtual output missing' >&2; exit 1; }
timeout "$((SECONDS_TO_RUN+10))" mpv --focus-on=never --really-quiet --no-audio --fs --fs-screen-name="$output" --screen-name="$output" 'av://lavfi:testsrc2=size=3840x2160:rate=60,setparams=colorspace=bt470bg:range=limited:color_primaries=bt709:color_trc=iec61966-2-1' > "$S/pattern.log" 2>&1 & MP=$!
scp -q scripts/pi-monitor.py "$PI:$R/monitor.py"
monitor="timeout '$((SECONDS_TO_RUN+20))' python3 '$R/monitor.py' '$R/client.pid' > '$R/metrics.jsonl' 2>&1 & p=\$!; echo \$p > '$R/monitor.pid'; wait \$p"
timeout "$((SECONDS_TO_RUN+25))" ssh -o BatchMode=yes "$PI" "$monitor" > "$S/monitor.ssh.log" 2>&1 & MON=$!
if [[ ${SL_TEST_TONE:-0} == 1 ]]; then
    python3 - "$S/tone.wav" <<'PY'
import math,struct,sys,wave
with wave.open(sys.argv[1],'wb') as f:
    f.setnchannels(2); f.setsampwidth(2); f.setframerate(48000)
    f.writeframes(b''.join(struct.pack('<hh',int(math.sin(i*math.tau*440/48000)*16383),int(math.sin(i*math.tau*440/48000)*16383)) for i in range(48000*3)))
PY
    timeout 4 pw-play --volume 0.08 "$S/tone.wav" > "$S/tone.log" 2>&1 & TP=$!
fi
wait "$CP"; CP=
kill "$MP" 2>/dev/null || true; wait "$MP" 2>/dev/null || true; MP=
wait "$MON" || true; MON=
scp -q "$PI:$R/client.log" "$S/client.log"
scp -q "$PI:$R/metrics.jsonl" "$S/metrics.jsonl"
python3 scripts/report-pi-soak.py "$S" "$SECONDS_TO_RUN"
