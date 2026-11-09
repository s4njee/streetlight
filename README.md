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
| Wire protocol: FEC, encryption, GSO send, pairing | working, unit-tested |
| End to end over loopback | 1440p60 @ 80 Mbit/s, 0 loss, ~2–6 ms capture→received |
| Linux client: VA-API decode → dmabuf → GTK4 | working on the dev PC (decode ≈ 2 ms) |
| Linux client webview UI (`--features webview`) | written; needs `webkitgtk-6.0` to build |
| Pi 5 V4L2-request decode path | written; untested on hardware |
| iPad app | written; needs a Mac to build (untested) |
| Input (kbd/mouse/touch/gamepad), audio | written; host side runs, not yet verified end to end |

Not done yet: HDR (Main10/PQ), AV1 on iPad, adaptive bitrate, client-side cursor, relative-mouse
pointer lock on Linux, and choosing the refresh rate of the virtual display (KWin's protocol only takes a size).

## Host (this PC)

```bash
cargo install --path crates/host            # → ~/.cargo/bin/streetlight-host
streetlight-host                            # or: systemctl --user enable --now streetlight-host
```

* Open <http://127.0.0.1:47990> to approve pairings, unpair devices, edit apps and limits.
* `streetlight-host pair 1234` approves a pending pairing from a terminal.
* `streetlight-host capture-test 5 out.hevc 2560x1440` checks capture and encode with no client.
* Needs write access to `/dev/uinput` (you already have it via an ACL; otherwise install
  `packaging/60-streetlight-uinput.rules`).
* Firewall: allow UDP 47800–47801 and mDNS (UDP 5353) from your LAN.
* Settings live in `~/.config/streetlight/` (`settings.json`, `clients.json`, host cert).

## Raspberry Pi 5 client

Raspberry Pi OS Trixie (64-bit), desktop image:

```bash
sudo apt install build-essential clang pkg-config libgtk-4-dev libwebkitgtk-6.0-dev \
    libavcodec-dev libavutil-dev libopus-dev libasound2-dev libudev-dev
curl https://sh.rustup.rs -sSf | sh
cargo install --path crates/client-linux --features webview
streetlight-client                 # webview UI: discover → pair → pick an app
streetlight-client 192.168.1.20    # or connect straight away (Ctrl+Alt+Shift+Q to leave)
```

The Pi's only hardware video decoder is HEVC. Raspberry Pi's FFmpeg exposes it as a DRM-PRIME
hwaccel, so frames stay in GPU memory all the way to GTK's offloaded subsurface. If GTK can't
import the Pi's SAND128 buffers, the client logs `dmabuf texture import failed`; the fallback
for that is a direct KMS plane mode, which is next on the roadmap.

`ffmpeg-sys-next` 9 is built against FFmpeg 9 headers. If the Pi's FFmpeg is older, pin
`ffmpeg-sys-next` to the matching major version in `crates/client-linux/Cargo.toml`.

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
