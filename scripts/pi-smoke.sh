#!/usr/bin/env bash
# Run after building ~/streetlight-dev on the Pi. No input, no decoded audio playback.
set -euo pipefail
cd "$(dirname "$0")/.."
PI=${1:-neo.local}
HOST_ADDR=${2:-192.168.1.99}
[[ $PI =~ ^[a-zA-Z0-9_.@-]+$ && $HOST_ADDR =~ ^[a-zA-Z0-9_.:-]+$ ]] || { echo 'Invalid SSH target/address' >&2; exit 2; }
[[ ${XDG_SESSION_TYPE:-} == wayland ]] || { echo 'Requires host Wayland session' >&2; exit 1; }
command -v mpv >/dev/null
interfaces=$(wayland-info 2>/dev/null)
[[ $interfaces == *zkde_screencast_unstable_v1* ]] || { echo 'Requires host KWin screencast' >&2; exit 1; }
python3 - <<'PY'
import socket
for kind, port in [(socket.SOCK_STREAM,47990),(socket.SOCK_DGRAM,47800),(socket.SOCK_DGRAM,47801)]:
    with socket.socket(socket.AF_INET,kind) as s:
        if kind == socket.SOCK_STREAM: s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
        s.bind(('127.0.0.1',port))
PY
S=$(mktemp -d /tmp/streetlight-pi.XXXXXXXX)
export XDG_CONFIG_HOME="$S/config"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-0}"
[[ $WAYLAND_DISPLAY == /* ]] || export WAYLAND_DISPLAY="${XDG_RUNTIME_DIR:?}/$WAYLAND_DISPLAY"
export PIPEWIRE_RUNTIME_DIR="${PIPEWIRE_RUNTIME_DIR:-${XDG_RUNTIME_DIR:?}}"
export XDG_RUNTIME_DIR="$S/runtime"
mkdir -m 700 "$XDG_RUNTIME_DIR"
R=$(ssh -o BatchMode=yes "$PI" 'mktemp -d /tmp/streetlight-test.XXXXXXXX')
[[ $R == /tmp/streetlight-test.* && $R != *[!a-zA-Z0-9./_-]* ]] || exit 1
HP= CP= MP=
cleanup() {
    for pid in "$MP" "$CP" "$HP"; do
        if [[ -n $pid ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    done
    ssh -o BatchMode=yes -o ConnectTimeout=5 "$PI" "if test -f '$R/pid'; then kill \$(cat '$R/pid') 2>/dev/null || true; fi" || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
echo "Host artifacts: $S"
echo "Pi artifacts: $PI:$R"
export RUST_LOG=info
unset SL_TEST_DROP SL_TEST_BURST SL_TEST_REORDER SL_TEST_DROP_SCHEDULE
timeout 240 ./target/release/streetlight-host serve > "$S/host.log" 2>&1 & HP=$!
for _ in {1..100}; do [[ -f "$XDG_RUNTIME_DIR/streetlight/token" ]] && break; kill -0 "$HP"; sleep .1; done
read -r -a MODES <<< "${SL_SMOKE_MODES:-decode1080 decode4k present1080 present4k}"
for mode in "${MODES[@]}"; do
    [[ $mode =~ ^(decode1080|decode4k|present1080|present4k)$ ]] || exit 2
done
for mode in "${MODES[@]}"; do
    size=1920x1080 bitrate=30000 display="--decode-only --dump '$R/$mode.hevc'"
    [[ $mode == *4k ]] && size=3840x2160 bitrate=80000
    [[ $mode == present* ]] && display=''
    remote="export XDG_CONFIG_HOME='$R/config' XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus GDK_DEBUG=offload RUST_LOG=info; cd \"\$HOME/streetlight-dev\"; timeout 35 ./target/release/streetlight-client '$HOST_ADDR' --windowed --no-input --no-audio --seconds 15 --size '$size' --fps 60 --bitrate '$bitrate' --latency --report json $display > '$R/$mode.log' 2>&1 & p=\$!; echo \$p > '$R/pid'; wait \$p"
    timeout 40 ssh -o BatchMode=yes "$PI" "$remote" > "$S/$mode.ssh.log" 2>&1 & CP=$!
    for _ in {1..120}; do
        ssh -o BatchMode=yes "$PI" "cat '$R/$mode.log' 2>/dev/null" > "$S/$mode.log" || true
        if [[ $mode == decode1080 ]]; then
            pin=$(sed -n 's/.*Pairing PIN: \([0-9]\{4\}\).*/\1/p' "$S/$mode.log" | head -1)
            if [[ -n $pin ]]; then ./target/release/streetlight-host pair "$pin" >> "$S/pair.log" || true; fi
        fi
        if rg -q '^stream:' "$S/$mode.log"; then break; fi
        kill -0 "$CP" 2>/dev/null || { cat "$S/$mode.log"; cat "$S/$mode.ssh.log"; exit 1; }
        sleep .1
    done
    rg -q '^stream:' "$S/$mode.log" || { cat "$S/$mode.log"; exit 1; }
    # Verify the virtual target exists before allowing mpv's fullscreen fallback.
    output=$(python3 -c 'import json,urllib.request; print(json.load(urllib.request.urlopen("http://127.0.0.1:47990/api/status"))["status"]["output_name"])')
    monitors=$(wayland-info 2>/dev/null)
    [[ $monitors == *"Virtual-$output"* ]] || { echo 'Virtual output missing' >&2; exit 1; }
    timeout 17 mpv --focus-on=never --really-quiet --no-audio --fs --fs-screen-name="$output" --screen-name="$output" "av://lavfi:testsrc2=size=$size:rate=60,setparams=colorspace=bt470bg:range=limited:color_primaries=bt709:color_trc=iec61966-2-1" > "$S/$mode.mpv.log" 2>&1 & MP=$!
    sleep 5
    if [[ $mode == present* ]]; then
        ssh -o BatchMode=yes "$PI" "timeout 5 kmsprint -p > '$R/$mode.kms.log'"
        scp -q "$PI:$R/$mode.kms.log" "$S/$mode.kms.log"
    fi
    wait "$CP"; CP=
    kill "$MP" 2>/dev/null || true; wait "$MP" 2>/dev/null || true; MP=
    scp -q "$PI:$R/$mode.log" "$S/$mode.log"
    if [[ $mode == decode* ]]; then
        scp -q "$PI:$R/$mode.hevc" "$S/$mode.hevc"
        ffprobe -v error -show_entries stream=color_range,color_space,color_transfer,color_primaries -of json "$S/$mode.hevc" > "$S/$mode.color.json"
        python3 scripts/check-color.py "$S/$mode.hevc" > "$S/$mode.color-check.log"
    fi
    echo "$mode complete"
    sleep .5
done
python3 - "$S" "${MODES[@]}" <<'PY'
import json,pathlib,sys
root=pathlib.Path(sys.argv[1]); failed=[]
for mode in sys.argv[2:]:
    stats=[]; latency=[]
    for line in (root/f'{mode}.log').read_text().splitlines():
        if line.startswith('{'):
            try: stats.append(json.loads(line))
            except ValueError: pass
        if line.startswith('LATENCY '): latency.append(json.loads(line[8:]))
    if not stats or not latency: failed.append(f'{mode}: missing stats'); continue
    s=stats[-1]; d=[r['decode']['p95_ms'] for r in latency if r['decode']['samples']]
    print(mode, json.dumps(s), 'decode p95 intervals ms:',d)
    if s['frames_decoded']<750 or s['decode_errors'] or s['frames_lost'] or not d: failed.append(f'{mode}: decode/loss/fps failed')
    if d and max(d)>8: failed.append(f'{mode}: decode p95 >8 ms')
    if mode.startswith('present') and not s['frames_presented']: failed.append(f'{mode}: no presentation feedback')
if failed: raise SystemExit('\n'.join(failed))
print('PASS: Pi smoke modes:', ', '.join(sys.argv[2:]))
PY
