# Streetlight

Low-latency game/desktop streaming from a Linux PC (KDE Plasma, Wayland, AMD) to a
Raspberry Pi 5 or an iPad. Each client renders video natively and shows its menus in a webview.

Design and wire format: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

```
host:  KWin virtual monitor → PipeWire DMA-BUF → VA-API VPP → HEVC (VCN) → FEC + AES-GCM → UDP
       uinput ← QUIC (input, control, PIN pairing) · Opus 5 ms audio · web UI on 127.0.0.1:47990
pi 5:  UDP → client-core → V4L2 HEVC → dmabuf → GTK4 GraphicsOffload (direct scan-out) + WebKitGTK UI
ipad:  UDP → client-core (Rust, C ABI) → AVSampleBufferDisplayLayer + WKWebView UI
```

## Status

| Piece | State |
|---|---|
| Host: virtual display capture (zero copy) → HEVC | working, ~3.4 ms import+convert+encode at 1080p60 |
| Wire protocol: FEC, encryption, GSO send, pairing | working, unit-tested; rebuild both ends for `streetlight/3` |
| Host web API | Host/Origin checks, process token, command confirmation; HTTP integration-tested |
| Loss/reordering recovery | 1080p60/30 Mbit/s matrix passed; 2% loss fully recovered, 10% loss → IDR ≤15.07 ms, burst gap ≤350 ms; [results](docs/TESTING.md) |
| End to end over loopback | 1440p60 @ 80 Mbit/s, 0 loss, ~2–6 ms capture→received |
| Linux client: VA-API decode → dmabuf → GTK4 | working on the dev PC (decode ≈ 2 ms) |
| Linux client webview UI (`--features webview`) | working: discover → connect → apps → stream (1440p144 from the UI) |
| Pi 5 V4L2-request decode/presentation + WebKit UI | verified 1080p60/4K60 on `neo.local`; 30-minute 4K60/audio soak, zero loss/errors; 9.43% of one CPU core; [details](docs/PI.md) |
| iPad app | written; needs a Mac to build (untested) |
| Input: keyboard, mouse (abs + 1:1 relative), scroll, buttons, touch, gamepad | verified end to end (`--selftest --clicks` + `input_probe`) |
| Adaptive bitrate | tested changing-loss down/up control; codec reopen retains capture |
| Auto codec / AV1 | real hardware negotiation; native AV1/H.264 decode and clean dumps verified; Pi chooses HEVC |
| Client cursor / pointer lock | desktop Linux overlay and manual Wayland lock; focused virtual-probe tests pass; Pi retains embedded default to preserve direct scan-out |
| App lifecycle | tracked process groups, stop/leave policy and status; [Steam placement recipe](docs/APPS.md) |
| HDR10 | guarded experimental path; current KWin capture rejects 10-bit formats; physical output unverified |
| Audio: PipeWire → Opus 5 ms → client | verified (440 Hz test tone); Linux PLC on missing packets unit-tested |

Pi limits: 4K decode p95 is 10–11 ms (target ≤8 ms); the user confirmed the TV picture; tone audibility and USB/controller checks remain pending. The Mac/iPad work is deferred.

P2 implementation and measured checks: [docs/P2.md](docs/P2.md).
Remaining: compatible HDR capture/physical presentation, iPad AV1/EDR/cursor rendering,
automatic host-game pointer-grab detection, high-refresh virtual-mode selection and
broader device/network checks. Linux pointer lock is manually toggled with
**Ctrl+Alt+Shift+M**; unsupported compositors retain absolute input.

## Host (this PC)

```bash
cargo install --path crates/host            # → ~/.cargo/bin/streetlight-host
streetlight-host                            # or: systemctl --user enable --now streetlight-host
```

* Open <http://127.0.0.1:47990> to approve pairings, unpair devices, edit apps and limits.
* `streetlight-host pair 1234` approves a pending pairing from a terminal in the same user session.
  Both host and CLI need `XDG_RUNTIME_DIR`; the private API token lives at
  `$XDG_RUNTIME_DIR/streetlight/token` (0600). App command changes in the web UI require confirmation.
* `streetlight-host capture-test 5 out.hevc 2560x1440` checks capture and encode with no client.
* Needs write access to `/dev/uinput` (you already have it via an ACL; otherwise install
  `packaging/60-streetlight-uinput.rules`).
* Firewall: allow UDP 47800–47801 and mDNS (UDP 5353) from your LAN.
* Settings live in `~/.config/streetlight/` (`settings.json`, `clients.json`, host cert).

## Raspberry Pi 5 client

Raspberry Pi OS Trixie (64-bit), desktop image:

```bash
sudo apt install build-essential clang libclang-dev cmake ninja-build pkg-config libgtk-4-dev libwebkitgtk-6.0-dev \
    libavcodec-dev libavutil-dev libopus-dev libasound2-dev libudev-dev pipewire-alsa
curl https://sh.rustup.rs -sSf | sh
cargo install --path crates/client-linux --features webview
streetlight-client                 # webview UI: discover → pair → pick an app
streetlight-client 192.168.1.20    # or connect straight away (Ctrl+Alt+Shift+Q to leave)
```

The Pi's only hardware video decoder is HEVC. Raspberry Pi's FFmpeg exposes it as a DRM-PRIME
hwaccel, so frames stay in GPU memory all the way to GTK's offloaded subsurface. If GTK can't
import the Pi's SAND128 buffers, the client logs `dmabuf texture import failed`. The tested
Cage stack imports them and scans NV12 out directly; other compositors still need checking.

`ffmpeg-sys-next` generates bindings from the installed headers; its crate major does
not require that FFmpeg major. The Pi's patched FFmpeg 7.1.5 builds successfully with the
same bindings crate as the host. Keep Raspberry Pi's `--enable-v4l2-request` build.

DRM devices are discovered through `/dev/dri/by-path` and sysfs, with the `vc4-drm`
display card preferred on the Pi (often `card1`). Override with `SL_DRM_DEVICE` or
`SL_VAAPI_DEVICE`. `--decode-only --seconds 15 --latency --report json` checks decoding
without a window or audio; `--no-audio` disables audio in GUI diagnostics.

On Pi/GTK <4.20, the client negotiates an explicit BT.601/full compatibility stream
matching the tested Cage NV12 plane. Unsupported YUV color states otherwise report an
error. See [Pi setup and color verification](docs/PI.md) for overrides and limitations.
Both ends need the updated source (`streetlight/3`); old binaries cannot connect.

After building, `scripts/install-client.sh` installs the client and desktop launcher
under `~/.local`, plus an optional systemd user service. It does not enable the service
or replace an existing kiosk. In a normal Wayland desktop, import its display environment
and explicitly enable `streetlight-client.service` to launch the UI at login. For a Pi
OS Lite kiosk, run `cage -s -- ~/.local/bin/streetlight-client` from the graphical seat
instead of another kiosk application.

## iPad client (needs a Mac)

```bash
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
brew install xcodegen
cd clients/ios
./build-rust.sh          # builds client-core → StreetlightCore.xcframework
xcodegen                 # generates Streetlight.xcodeproj from project.yml
open Streetlight.xcodeproj   # set your signing team, run on the iPad
```

* Video goes through `AVSampleBufferDisplayLayer`, which uses the hardware HEVC decoder and shows
  each frame immediately. At "native" resolution the host creates a 2732×2048 display at 2× scale,
  running at the iPad's maximum refresh rate.
* Touch: tap or drag to click, two-finger tap to right-click, two-finger drag to scroll, three-finger
  tap to open the menu. A touchscreen-passthrough mode is in Settings.
* A hardware keyboard, a mouse (pointer locked while streaming) and MFi/Xbox/PlayStation
  controllers are passed through.
* The defaults assume Wi-Fi: 20 % FEC and 600 Mbit/s packet pacing.

## Regression and Pi checks

```bash
scripts/e2e.sh                              # local KWin: video, gated input, pad, audio, UI
scripts/pi-smoke.sh neo.local 192.168.1.99   # native hardware decode and presentation
python3 scripts/pi-ui-smoke.py neo.local 192.168.1.99
SL_SOAK_SECONDS=1800 scripts/pi-soak.sh neo.local 192.168.1.99
```

The scripts use isolated configs and bounded processes. The local input probe must remain
fullscreen and focused on the exact streamed virtual output; its heartbeat gates every new
keyboard/mouse event. The Pi smoke/UI checks run `--windowed --no-input --no-audio`; the
soak enables HDMI audio. The smoke script reports failure for the outstanding 4K ≤8 ms
decode target. See [verification results](docs/TESTING.md) and the [physical-device checklist](docs/PI.md).

## Loss/recovery tests

Run `scripts/loss-matrix.sh` on an idle KWin/Wayland host. It uses scratch settings,
headless dumps and a virtual-output test pattern, then checks five 15-second cases with
ffmpeg. Test-only packet impairments (`SL_TEST_DROP`, `SL_TEST_BURST`, `SL_TEST_REORDER`)
and JSON stats (`--dump out.hevc --report json`) are documented with measured results in
[docs/TESTING.md](docs/TESTING.md). Pi/iPad recovery still needs hardware verification.

## Layout

```
crates/proto         wire format, FEC, crypto, control messages, QUIC/TLS helpers
crates/host          streetlight-host
crates/client-core   client transport (Rust API + C ABI for Swift)
crates/client-linux  Pi / Linux client (GTK4 + WebKitGTK)
clients/ios          iPad app (Swift)
web/client           UI shared by both clients (one self-contained HTML file)
web/host             host web UI
```
