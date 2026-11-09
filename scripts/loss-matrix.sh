#!/usr/bin/env bash
# Headless P0 loss/reordering regression on the KWin virtual output. No input/audio playback.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ ${XDG_SESSION_TYPE:-} == wayland ]] || { echo 'Requires a Wayland session' >&2; exit 1; }
for cmd in wayland-info mpv ffmpeg python3 timeout rg; do command -v "$cmd" >/dev/null; done
interfaces=$(wayland-info 2>/dev/null)
[[ $interfaces == *zkde_screencast_unstable_v1* ]] || { echo 'KWin screencast protocol missing' >&2; exit 1; }
# Refuse to touch another running host's runtime token or stream.
python3 - <<'PY'
import socket
for kind, port in [(socket.SOCK_STREAM, 47990), (socket.SOCK_DGRAM, 47800), (socket.SOCK_DGRAM, 47801)]:
    with socket.socket(socket.AF_INET, kind) as s:
        if kind == socket.SOCK_STREAM: s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
        s.bind(('127.0.0.1', port))
PY
if [[ ${SL_SKIP_BUILD:-0} != 1 ]]; then cargo build --release --workspace --bins --examples; fi
S=$(mktemp -d "${TMPDIR:-/tmp}/streetlight-loss.XXXXXXXX")
export XDG_CONFIG_HOME="$S/config"
# Keep Wayland/PipeWire socket discovery in the real runtime; isolate our API token.
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-0}"
[[ $WAYLAND_DISPLAY == /* ]] || export WAYLAND_DISPLAY="${XDG_RUNTIME_DIR:?}/$WAYLAND_DISPLAY"
export PIPEWIRE_RUNTIME_DIR="${PIPEWIRE_RUNTIME_DIR:-${XDG_RUNTIME_DIR:?}}"
export XDG_RUNTIME_DIR="$S/runtime"
mkdir -m 700 "$XDG_RUNTIME_DIR"
HP= CP= MP=
cleanup() {
    for pid in "$MP" "$CP" "$HP"; do
        if [[ -n $pid ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    done
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
echo "Artifacts: $S"
unset SL_TEST_DROP SL_TEST_BURST SL_TEST_REORDER SL_TEST_DROP_SCHEDULE
export SL_TEST_SEED=7
export RUST_LOG=info
export NO_COLOR=1
mkdir -p "$XDG_CONFIG_HOME/streetlight"
printf '%s\n' '{"adaptive_bitrate":false}' > "$XDG_CONFIG_HOME/streetlight/settings.json"
timeout 180 ./target/release/streetlight-host serve > "$S/host.log" 2>&1 & HP=$!
for _ in {1..100}; do
    [[ -f "$XDG_RUNTIME_DIR/streetlight/token" ]] && break
    kill -0 "$HP" 2>/dev/null || { cat "$S/host.log"; exit 1; }
    sleep 0.1
done
for mode in none random2 random10 burst reorder; do
    unset SL_TEST_DROP SL_TEST_BURST SL_TEST_REORDER SL_TEST_DROP_SCHEDULE
    case $mode in
        random2) export SL_TEST_DROP=0.02;;
        random10) export SL_TEST_DROP=0.10;;
        burst) export SL_TEST_BURST=20:0.01;;
        reorder) export SL_TEST_REORDER=0.01;;
    esac
    timeout 35 ./target/release/streetlight-client 127.0.0.1 --windowed --no-input --dump "$S/$mode.hevc" --seconds 15 --size 1920x1080 --fps 60 --bitrate 30000 --report json > "$S/$mode.log" 2>&1 & CP=$!
    for _ in {1..100}; do
        if [[ $mode == none ]]; then
            pin=$(sed -n 's/.*Pairing PIN: \([0-9]\{4\}\).*/\1/p' "$S/$mode.log" | head -1)
            if [[ -n $pin ]]; then
                # PairStart may still be in flight after printing the PIN.
                if ./target/release/streetlight-host pair "$pin" | tee "$S/pair.log" | rg -q 'PIN accepted'; then break; fi
            fi
        else break; fi
        kill -0 "$CP" 2>/dev/null || { cat "$S/$mode.log"; exit 1; }
        sleep 0.1
    done
    for _ in {1..100}; do
        if rg -q '^stream:' "$S/$mode.log"; then break; fi
        kill -0 "$CP" 2>/dev/null || { cat "$S/$mode.log"; cat "$S/host.log"; exit 1; }
        sleep 0.1
    done
    rg -q '^stream:' "$S/$mode.log" || { cat "$S/$mode.log"; exit 1; }
    output=$(python3 -c 'import json,urllib.request; print(json.load(urllib.request.urlopen("http://127.0.0.1:47990/api/status"))["status"]["output_name"])')
    monitors=$(wayland-info 2>/dev/null)
    [[ $monitors == *"Virtual-$output"* ]] || { echo 'Virtual output missing' >&2; exit 1; }
    timeout 17 mpv --focus-on=never --really-quiet --no-audio --fs --fs-screen-name="$output" --screen-name="$output" "av://lavfi:testsrc2=size=1920x1080:rate=60" > "$S/$mode.mpv.log" 2>&1 & MP=$!
    wait "$CP"; CP=
    kill "$MP" 2>/dev/null || true; wait "$MP" 2>/dev/null || true; MP=
    timeout 20 ffmpeg -v error -threads 1 -i "$S/$mode.hevc" -f null - > "$S/$mode.decode.log" 2>&1
    sleep 0.5
done
python3 scripts/report-loss.py "$S"
