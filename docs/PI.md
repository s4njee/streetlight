# Raspberry Pi 5 bring-up

The tested Pi is `neo.local` (`192.168.1.219`), a 4 GB Raspberry Pi 5 running
64-bit Trixie with Gigabit Ethernet. It has a Cage/Chromium kiosk on
`/run/user/1000/wayland-0` and a TV at 3840×2160@60, scale 2. Its DRM nodes are
`card0` (v3d), `card1` (vc4-drm/KMS), and `renderD128` (v3d).
The existing kiosk configuration is preserved.

## Build

```bash
sudo apt install --no-install-recommends build-essential clang libclang-dev cmake ninja-build \
  pkg-config libgtk-4-dev libwebkitgtk-6.0-dev libavcodec-dev libavutil-dev \
  libopus-dev libasound2-dev libudev-dev pipewire-alsa
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release -p streetlight-client --features webview --bins --examples -j 1
```

The tested toolchain is Rust 1.99.1, GTK 4.18.6, WebKitGTK 2.54.0, and Raspberry Pi's
patched FFmpeg 7.1.5 (`--enable-v4l2-request --enable-sand`). The source is staged in
`~/streetlight-dev`. `ffmpeg-sys-next` 9 generates bindings against those installed
headers; no crate downgrade or FFmpeg upgrade was necessary. A first build exposed
missing CMake/Ninja for the bundled Opus dependency; those are included above.
A single Cargo job keeps memory use manageable on the 4 GB Pi.

The decoder discovers devices via `/dev/dri/by-path` and sysfs, preferring vc4-drm
for the request decoder. It reports:

```text
decoder: hevc via Drm (/dev/dri/card1)
Hwaccel V4L2 HEVC stateless V4; devices: /dev/media0,/dev/video19
```

Overrides: `SL_DRM_DEVICE=/dev/dri/card1`, `SL_VAAPI_DEVICE=/dev/dri/renderD128`.
The host's GPU selection remains configurable through its settings; automatic host
GPU selection on a multi-GPU desktop is still pending.

## Repeatable checks

On the development host, with no other Streetlight host process running:

```bash
scripts/pi-smoke.sh neo.local 192.168.1.99
python3 scripts/pi-ui-smoke.py neo.local 192.168.1.99
SL_SOAK_SECONDS=1800 scripts/pi-soak.sh neo.local 192.168.1.99
```

Build host/client binaries before running these scripts. The Pi scripts expect the
native client at `~/streetlight-dev/target/release/streetlight-client`. They use
scratch Streetlight configs/certificates and approve only that isolated test pairing.
All remote/local background processes have timeouts and are stopped by PID.
The native app runs `--windowed --no-input`; decoded audio is disabled in the smoke
checks and enabled in the soak. No screenshot of the host desktop is taken.

The smoke script runs 1080p60 and 4K60 at 30/80 Mbit/s, first through the real hardware
decoder without a window (`--decode-only`), then through GTK presentation. It retains
logs and checks loss, decode errors, frame counts, actual presentation feedback, and
the plan's 8 ms decode p95 target. **It currently exits nonzero for the 4K decode
latency budget**, even though all four cases sustain 60 fps with zero loss/decode errors.
Measured results are in [TESTING.md](TESTING.md).

The UI script exercises the actual WebKit bridge's connect → known pairing → cancel
in-flight start → restart Desktop flow, with stream startup on a worker and GTK completion on the main thread. It checks
host status and real frame-number feedback after the stream starts.

The soak records one Stats JSON line per second, latency percentiles every five
seconds, and CPU/RSS/temperature/throttling once per minute. Its CPU percentage uses
one core as 100%. It checks sustained decode/presentation and zero loss/backend errors;
the separate 8 ms decode target remains visible in the summary.

## Presentation and audio

The decoder exports NV12 DRM-PRIME/SAND128 frames. Cage advertises that modifier;
GTK `GraphicsOffload` attaches the textures to a subsurface (see the [GTK offload/debug documentation](https://docs.gtk.org/gtk4/class.GraphicsOffload.html)). `kmsprint` confirmed an
NV12 framebuffer directly on the active 3840×2160 HDMI plane, including 1920×1080
upscaling and native 3840×2160. A separate KMS renderer or CPU-copy fallback was not
needed on this Pi. Other compositor/driver combinations still need checking.

GTK 4.18 rejects YUV matrix/narrow-range CICP color states (see the
[4.18 implementation](https://raw.githubusercontent.com/GNOME/gtk/4.18.6/gdk/gdkcolorstate.c));
[GTK 4.20](https://raw.githubusercontent.com/GNOME/gtk/4.20.0/gdk/gdkcicpparams.c) adds
YUV matrix handling. This Cage session advertises no color-representation protocol.
The original BT.709 limited stream was therefore interpreted incorrectly: the active
NV12 KMS plane reported BT.601/full. The client now requests `Sdr601Full` on the Pi
with GTK <4.20, and the host converts and tags that matrix/range while retaining sRGB primaries/TRC
through explicit VA color properties. The broad BT.601 preset also changed primaries
and was rejected by the numerical color check. Other streams must
have a representable color state; the previous silent sRGB fallback was removed.
`SL_LEGACY_YUV=1` forces compatibility negotiation; `=0` disables it. This is scoped
to the tested unmanaged Pi stack; other compositors need their own color verification.

The Pi initially lacked `pipewire-alsa`. Direct ALSA playback opened with a 240-frame
buffer but subsequently returned `POLLERR` in a busy error loop. Installing the
PipeWire ALSA bridge routes the client to the existing HDMI sink. The client now logs
only the first backend failure, counts errors/underruns, and promptly closes a failed
backend. Fixed 240-frame (5 ms) output is verified through PipeWire; devices refusing
it log the default-buffer fallback. No HDMI volume setting was changed.

The test tone is 440 Hz for three seconds, played on the host with `pw-play --volume
0.08`. Decoded audio is played only on the Pi, never on the capturing host. The user confirmed the corrected TV test picture looked good on 2026-11-09.
Tone audibility has not been confirmed.

## Install and launch

On the Pi after building:

```bash
scripts/install-client.sh
~/.local/bin/streetlight-client
```

Installed and checked on `neo.local` at `/home/sanjee/.local/bin/streetlight-client`;
the installed binary matches the tested release build. The desktop launcher is present
and the optional service remains disabled.

The installer writes the binary and desktop launcher under `~/.local` and an optional
`~/.config/systemd/user/streetlight-client.service`. It does not start/enable the
service or replace Cage/Chromium. In a normal Wayland desktop, import the session's
`WAYLAND_DISPLAY`, then explicitly enable the user service if desired. For a Pi OS
Lite kiosk, use `cage -s -- ~/.local/bin/streetlight-client` from its graphical seat
instead of another kiosk application.

Both ends must use the updated source: the QUIC ALPN is now `streetlight/3`, reflecting
negotiation/cursor capabilities and RTT feedback as well as the P1 control changes. The
unused C `sl_input_text` entry point was removed too. Keyboard scancodes remain
supported; Unicode text composition needs a future implementation.

The test pairings are deliberately isolated. Launch the normal host and client, select
the host in the Pi UI, then approve the new PIN in the host UI to establish a persistent
pairing under the normal user config.

## Remaining hardware checklist

1. Confirm the quiet 440 Hz tone is audible. The TV test picture was confirmed by the
   user; KMS, bitstream metadata and flat-color checks also pass. Audibility remains
   separate from the verified HDMI route and zero-underrun counters.
2. Attach a USB keyboard and mouse. Use the host's `input_probe` on the exact virtual
   output reported by `/api/status`, then verify `a` down/up, absolute pointer movement,
   buttons and scrolling arrive in that probe. Use only the safe keys from `plan.md`.
3. Connect an Xbox and a DualSense controller and exercise buttons/sticks through the
   Pi's gilrs path. No physical USB/gamepad devices were available during SSH bring-up.
4. Tune 4K decoding toward the ≤8 ms p95 target; the measured 10–11 ms hardware decode
   time currently misses it. The stream sustains 4K60.
5. Optional: test 1080p120/144 after host virtual-refresh selection is implemented and
   measured. The current TV mode and verified streaming mode are 4K60.

The Pi keeps embedded cursors by default. `SL_CLIENT_CURSOR=1` opts into the P2 GTK
overlay, which forces RGB composition on this Cage/GTK 4.18 stack and adds delay.
P2 Auto/HEVC negotiation, cursor transport and manual pointer-lock details are in
[P2.md](P2.md). The Mac/iPad work is deferred at the user's request; the Mac has not been accessed.
