#!/usr/bin/env bash
# Host regression. Keyboard/mouse input is only sent while the virtual-output probe is focused.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ ${XDG_SESSION_TYPE:-} == wayland ]] || { echo 'Requires Wayland' >&2; exit 1; }
for c in wayland-info mpv ffmpeg pw-play python3 timeout rg; do command -v "$c" >/dev/null; done
python3 -c 'import evdev'
interfaces=$(wayland-info 2>/dev/null)
[[ $interfaces == *zkde_screencast_unstable_v1* ]] || { echo 'Requires KWin screencast' >&2; exit 1; }
python3 - <<'PY'
import socket
for kind,port in [(socket.SOCK_STREAM,47990),(socket.SOCK_DGRAM,47800),(socket.SOCK_DGRAM,47801)]:
    with socket.socket(socket.AF_INET,kind) as s:
        if kind == socket.SOCK_STREAM: s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
        s.bind(('127.0.0.1',port))
PY
WEBVIEW=0
if pkg-config --exists webkitgtk-6.0; then WEBVIEW=1; fi
cargo build --release --workspace --bins --examples
if [[ $WEBVIEW == 1 ]]; then cargo build --release -p streetlight-client --features webview --bins --examples; fi
S=$(mktemp -d /tmp/streetlight-e2e.XXXXXXXX)
export XDG_CONFIG_HOME="$S/config"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-0}"
[[ $WAYLAND_DISPLAY == /* ]] || export WAYLAND_DISPLAY="${XDG_RUNTIME_DIR:?}/$WAYLAND_DISPLAY"
export PIPEWIRE_RUNTIME_DIR="${PIPEWIRE_RUNTIME_DIR:-${XDG_RUNTIME_DIR:?}}"
export XDG_RUNTIME_DIR="$S/runtime"
mkdir -m700 "$XDG_RUNTIME_DIR"
export RUST_LOG=info NO_COLOR=1
unset SL_TEST_DROP SL_TEST_BURST SL_TEST_REORDER SL_TEST_DROP_SCHEDULE
HP= CP= MP= PP= GP= AP=
cleanup() {
    for p in "$AP" "$PP" "$GP" "$MP" "$CP" "$HP"; do
        if [[ -n $p ]]; then kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; fi
    done
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
printf 'Artifacts: %s\n' "$S"
timeout 150 ./target/release/streetlight-host serve > "$S/host.log" 2>&1 & HP=$!
for _ in {1..100}; do [[ -f "$XDG_RUNTIME_DIR/streetlight/token" ]] && break; kill -0 "$HP"; sleep .1; done
output_name() {
    python3 -c 'import json,urllib.request; s=json.load(urllib.request.urlopen("http://127.0.0.1:47990/api/status"))["status"]; print(s["output_name"] if s["streaming"] else "")'
}
wait_stream() {
    local log=$1 pattern=$2
    for _ in {1..120}; do
        if rg -q "$pattern" "$log"; then return; fi
        kill -0 "$CP" 2>/dev/null || { cat "$log"; return 1; }
        sleep .1
    done
    cat "$log"; return 1
}
pattern() {
    local output=$1
    local monitors
    monitors=$(wayland-info 2>/dev/null)
    [[ $monitors == *"Virtual-$output"* ]] || { echo 'Virtual target missing' >&2; return 1; }
    timeout 12 mpv --focus-on=never --really-quiet --no-audio --fs --fs-screen-name="$output" --screen-name="$output" 'av://lavfi:testsrc2=size=1920x1080:rate=60' > "$S/pattern.log" 2>&1 & MP=$!
}
timeout 30 ./target/release/streetlight-client 127.0.0.1 --windowed --no-input --no-audio --dump "$S/video.hevc" --seconds 8 --size 1920x1080 --fps 60 --bitrate 30000 --report json > "$S/video.log" 2>&1 & CP=$!
for _ in {1..100}; do
    pin=$(sed -n 's/.*Pairing PIN: \([0-9]\{4\}\).*/\1/p' "$S/video.log" | head -1)
    if [[ -n $pin ]]; then
        if ./target/release/streetlight-host pair "$pin" | tee "$S/pair.log" | rg -q 'PIN accepted'; then break; fi
    fi
    kill -0 "$CP"; sleep .1
done
wait_stream "$S/video.log" '^stream:'
pattern "$(output_name)"
wait "$CP"; CP=
kill "$MP" 2>/dev/null || true; wait "$MP" 2>/dev/null || true; MP=
timeout 20 ffmpeg -v error -threads 1 -i "$S/video.hevc" -f null - > "$S/video.decode.log" 2>&1
sleep .5
export SL_SELFTEST_GATE="$S/probe.gate"
timeout 40 python3 scripts/padwatch.py > "$S/pad.log" 2>&1 & GP=$!
timeout 40 ./target/release/streetlight-client 127.0.0.1 --selftest --clicks > "$S/selftest.log" 2>&1 & CP=$!
wait_stream "$S/selftest.log" 'SELFTEST stream'
output=$(output_name)
timeout 35 ./target/release/examples/input_probe "$output" > "$S/probe.log" 2>&1 & PP=$!
# The probe owns focus before selftest can send any keyboard/mouse events.
for _ in {1..200}; do
    if rg -q 'SELFTEST collecting audio' "$S/selftest.log"; then break; fi
    kill -0 "$CP" 2>/dev/null || { cat "$S/selftest.log"; exit 1; }
    sleep .1
done
python3 - "$S/tone.wav" <<'PY'
import math,struct,sys,wave
with wave.open(sys.argv[1],'wb') as f:
    f.setnchannels(2); f.setsampwidth(2); f.setframerate(48000)
    f.writeframes(b''.join(struct.pack('<hh',int(math.sin(i*math.tau*440/48000)*16383),int(math.sin(i*math.tau*440/48000)*16383)) for i in range(48000*3)))
PY
timeout 4 pw-play --volume 0.08 "$S/tone.wav" > "$S/tone.log" 2>&1 & AP=$!
wait "$CP"; CP=
wait "$GP"; GP=
wait "$AP"; AP=
kill "$PP" 2>/dev/null || true; wait "$PP" 2>/dev/null || true; PP=
unset SL_SELFTEST_GATE
sleep .5
if [[ $WEBVIEW == 1 ]]; then
    export SL_UI_EVAL='(async()=>{setTimeout(()=>streetlight.call("quit"),12000);await streetlight.call("setSettings",{resolution:"1920x1080",fps:60,bitrate_kbps:30000,codec:"Hevc",fec_percent:20,pace_mbps:0,virtual_display:true});const w=await streetlight.call("connect",{host:"127.0.0.1"});if(!w.paired)throw Error("pairing missing");await streetlight.call("start",{app:"desktop"});})();'
    timeout 20 ./target/release/streetlight-client --windowed --no-input --no-audio > "$S/ui.log" 2>&1 & CP=$!
    for _ in {1..100}; do
        output=$(output_name)
        [[ -n $output ]] && break
        kill -0 "$CP"; sleep .1
    done
    [[ -n $output ]]
    pattern "$output"
    sleep 5
    python3 - "$S/ui-status.json" <<'PY'
import pathlib,sys,urllib.request
pathlib.Path(sys.argv[1]).write_bytes(urllib.request.urlopen('http://127.0.0.1:47990/api/status').read())
PY
    wait "$CP"; CP=
    kill "$MP" 2>/dev/null || true; wait "$MP" 2>/dev/null || true; MP=
fi
python3 scripts/check-e2e.py "$S"
