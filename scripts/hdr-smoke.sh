#!/usr/bin/env bash
# HDR negotiation check on a temporary virtual output. Never capture a physical output.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ ${XDG_SESSION_TYPE:-} == wayland ]] || exit 1
S=$(mktemp -d /tmp/streetlight-hdr.XXXXXXXX)
export XDG_CONFIG_HOME="$S/config"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-0}"
[[ $WAYLAND_DISPLAY == /* ]] || export WAYLAND_DISPLAY="${XDG_RUNTIME_DIR:?}/$WAYLAND_DISPLAY"
export PIPEWIRE_RUNTIME_DIR="${PIPEWIRE_RUNTIME_DIR:-${XDG_RUNTIME_DIR:?}}"
export XDG_RUNTIME_DIR="$S/runtime";mkdir -m700 "$XDG_RUNTIME_DIR"
mkdir -p "$XDG_CONFIG_HOME/streetlight"
# Test-only mastering fixture, not a claim about this TV's calibration or arbitrary content.
cat > "$XDG_CONFIG_HOME/streetlight/settings.json" <<'JSON'
{"hdr_metadata":{"primaries":[[35400,14600],[8500,39850],[6550,2300]],"white_point":[15635,16450],"min_luminance":50,"max_luminance":10000000,"max_cll":1000,"max_fall":400}}
JSON
HP= MP=
cleanup(){ for pid in "$MP" "$HP"; do if [[ -n $pid ]];then kill "$pid" 2>/dev/null || true;wait "$pid" 2>/dev/null || true;fi;done; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
printf 'Artifacts: %s\n' "$S"
export RUST_LOG=info NO_COLOR=1
timeout 20 ./target/release/streetlight-host capture-test 8 "$S/hdr.hevc" 1920x1080 --hdr > "$S/capture.log" 2>&1 & HP=$!
for _ in {1..100};do
    if rg -q 'HDR capture verified' "$S/capture.log";then break;fi
    kill -0 "$HP" 2>/dev/null || { cat "$S/capture.log";wait "$HP";exit 1; }
    sleep .1
done
rg -q 'HDR capture verified' "$S/capture.log"
monitors=$(wayland-info 2>/dev/null)
[[ $monitors == *Virtual-streetlight-test* ]] || exit 1
timeout 10 mpv --focus-on=never --really-quiet --no-audio --fs --fs-screen-name=streetlight-test --screen-name=streetlight-test \
    'av://lavfi:testsrc2=size=1920x1080:rate=60,setparams=colorspace=bt470bg:range=limited:color_primaries=bt709:color_trc=iec61966-2-1' > "$S/pattern.log" 2>&1 & MP=$!
wait "$HP";HP=
ffprobe -v error -select_streams v:0 -read_intervals '%+#1' -show_streams -show_frames -of json "$S/hdr.hevc" > "$S/metadata.json"
python3 - "$S/metadata.json" <<'PY'
import json,sys
s=json.load(open(sys.argv[1]));v=s['streams'][0]
assert v['profile']=='Main 10' and v['pix_fmt']=='yuv420p10le',v
assert (v['color_space'],v['color_transfer'],v['color_primaries'])==('bt2020nc','smpte2084','bt2020'),v
side=s['frames'][0].get('side_data_list',[])
assert any(d['side_data_type']=='Mastering display metadata' for d in side),side
assert any(d['side_data_type']=='Content light level metadata' for d in side),side
print('PASS: 10-bit BT.2020/PQ capture, HEVC Main10 and mastering/content-light SEI (test fixture)')
PY
timeout 20 ffmpeg -v error -threads 1 -i "$S/hdr.hevc" -f null - > "$S/decode.log" 2>&1
[[ ! -s $S/decode.log ]]
