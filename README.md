# Streetlight

Low-latency game and desktop streaming from a Linux PC running **KDE Plasma on
Wayland** with an **AMD GPU**, to a **Raspberry Pi 5** or an **iPad**. Think
Moonlight/Sunshine, rebuilt around what KWin, PipeWire, VA-API and each client's
hardware decoder can do natively, with zero copies on the host and direct scan-out
on the client.

```
host   KWin virtual monitor (the client's exact size) → PipeWire DMA-BUF → VA-API VPP
       → HEVC/AV1 on VCN → one Reed-Solomon block per frame → AES-GCM → GSO UDP
       uinput ← QUIC (input, control, PIN pairing) · Opus 5 ms audio · web UI on 127.0.0.1:47990
pi 5   UDP → client-core → V4L2 request HEVC → DRM-PRIME dmabuf → GTK4 GraphicsOffload (scan-out)
ipad   UDP → client-core (Rust, C ABI) → AVSampleBufferDisplayLayer, shown on arrival
```

Every stream gets **its own virtual monitor** at the client's native resolution,
refresh rate and scale, so nothing is ever scaled, letterboxed by the host or
mirrored from a physical screen: an iPad gets a real 2732×2048 desktop at 2×, a 4K
TV gets 3840×2160, and the PC's own monitors are left alone.

---

## Contents

- [What it does](#what-it-does)
- [Measured results](#measured-results)
- [Tech stack](#tech-stack)
- [How Streetlight uses KWin](#how-streetlight-uses-kwin)
- [The pipeline, and how each stage is kept fast](#the-pipeline-and-how-each-stage-is-kept-fast)
- [Wire protocol](#wire-protocol)
- [Security](#security)
- [Host setup (the PC)](#host-setup-the-pc)
- [Raspberry Pi 5 client](#raspberry-pi-5-client)
- [iPad client](#ipad-client)
- [Apps, games and Steam](#apps-games-and-steam)
- [Testing](#testing)
- [Status and limitations](#status-and-limitations)
- [Repository layout](#repository-layout)
- [License](#license)

---

## What it does

- **Desktop and games.** The host's app list has a **Desktop** entry (your whole
  Plasma session on the new monitor) and any number of commands, such as Steam Big
  Picture. Launches are tracked per stream, with a stop-or-leave policy for when the
  client disconnects.
- **Native-size virtual monitors.** KWin creates a real output at exactly the
  client's resolution, refresh and scale for the length of the stream; windows can be
  placed on it like on any monitor.
- **Input as if you were there.** Keyboard, mouse (absolute, or 1:1 relative with
  pointer lock), scroll, multitouch and game controllers arrive as real Linux input
  devices (`uinput`), so games and Steam see a real gamepad.
- **Audio.** The desktop's audio, captured from PipeWire and sent as 5 ms Opus.
- **Pairing.** A 4-digit PIN shown on the client and approved on the host (web UI
  or CLI), turned into pinned certificates on both sides with SPAKE2.
- **Discovery.** mDNS (`_streetlight._udp`), or an address typed in.
- **Recovery.** Forward error correction rebuilds frames from partial packets;
  anything worse costs one IDR, and adaptive bitrate backs off under sustained loss.
- **A webview UI on every client**, with native video underneath: one HTML/JS bundle
  serves WebKitGTK on the Pi and WKWebView on the iPad.

## Measured results

All on the dev PC (KDE Plasma 6.8, Wayland, Radeon RX 9070 XT, CachyOS) unless
noted. Methods and raw numbers: [docs/TESTING.md](docs/TESTING.md),
[docs/PI.md](docs/PI.md), [docs/P2.md](docs/P2.md).

| What | Result |
|---|---|
| Host capture → GPU import → colour convert → HEVC encode, 1080p60 | **~3.4 ms** per frame |
| End to end over loopback, 1440p60 at 80 Mbit/s | 0 loss, **~2–6 ms** capture → received |
| 2 % random packet loss, 1080p60 at 30 Mbit/s | **0 frames lost**, 529 rebuilt by FEC, 0 decode errors |
| 10 % random loss | 23 of 870 frames lost, recovered by IDR in **≤ 15.07 ms** |
| Burst loss (20-packet bursts) | longest delivery gap 349.98 ms, 0 decode errors |
| Reordering | 0 lost (2 ms reordering tolerance) |
| Pi 5, 1080p60 HEVC decode | p95 **3.1 ms**; capture → on screen p95 23.4 ms |
| Pi 5, 4K60 at 80 Mbit/s, 30-minute soak with audio | 107,610 frames decoded, **0 lost, 0 decode errors, 0 audio underruns**; **9.43 %** of one CPU core; 51.25 °C, no throttling |
| iPad Air 13-inch (M2), 2732×2048 HEVC at 60 fps over Wi-Fi | ~5–7 ms RTT; 16,543 frames with 0 frames lost and 0 decode errors |

Not met yet: the Pi's 4K decode p95 is 10.5–11 ms against an 8 ms target.

## Tech stack

| Layer | Technology |
|---|---|
| Language | **Rust** (edition 2024) for the host, the protocol and both clients' cores; **Swift** for the iPad's native layer; one HTML/JS file per UI |
| Compositor | **KWin** (KDE Plasma 6, Wayland): `zkde_screencast_unstable_v1` virtual outputs, `xdg-output` layout, `kscreen-doctor` placement, window rules |
| Capture | **PipeWire** screencast node, DMA-BUF buffers with explicit DRM format modifiers, `SPA_META_Header` timestamps, `SPA_META_Cursor` |
| GPU | **VA-API** (Mesa radeonsi): DMA-BUF import as `DRM_PRIME_2` surfaces, VPP for BGRx → NV12/P010 and BT.709/BT.2020 |
| Encode | **FFmpeg** `hevc_vaapi` / `av1_vaapi` / `h264_vaapi` on AMD **VCN**, via `ffmpeg-sys-next` |
| Audio | PipeWire sink monitor → **Opus** (48 kHz stereo, 5 ms frames, low-delay mode); PLC on the Linux client |
| FEC | **`reed-solomon-simd`** (O(n log n) Reed-Solomon, SIMD), one block per frame |
| Encryption | **AES-128-GCM** per packet (`ring`), the header as associated data |
| Media transport | Raw UDP with **GSO** batching (`quinn-udp`) and optional pacing |
| Control | **QUIC** (`quinn`, `rustls` with `ring`), mutual TLS with pinned self-signed certificates (`rcgen`); `postcard` messages |
| Pairing | **SPAKE2** (`spake2`) over the PIN, binding both certificate fingerprints |
| Input | **`uinput`** virtual keyboard, mouse, absolute pointer, multitouch and gamepads (`evdev`) |
| Host web UI | `axum` on `127.0.0.1:47990`, Host/Origin checks and a process token |
| Discovery | `mdns-sd`; NWBrowser (Bonjour) on iOS |
| Pi client | FFmpeg **V4L2 request** HEVC hwaccel → DRM-PRIME (NV12 SAND128) → **GTK 4** `GdkDmabufTexture` under **`GtkGraphicsOffload`** (direct scan-out); **WebKitGTK 6** UI; ALSA/PipeWire audio |
| Desktop Linux client | VA-API decode → `av_hwframe_map` → DRM-PRIME, same GTK path; Wayland pointer constraints for pointer lock |
| iPad client | `client-core` through its C ABI; **`AVSampleBufferDisplayLayer`** (hardware HEVC/H.264, display-immediately); `AVAudioEngine` Opus; **GameController** for pads and mice; **WKWebView** UI |

## How Streetlight uses KWin

KWin is not just "the thing being captured"; Streetlight leans on several of its
interfaces to make a stream feel like a monitor plugged into the PC.

### A real monitor per stream

The host asks KWin for a **virtual output** through KDE's private screencast protocol,
`zkde_screencast_unstable_v1.stream_virtual_output_with_description(stream, name,
description, width, height, scale, pointer)` (the XML is in
[`crates/host/protocols`](crates/host/protocols)). KWin creates a real output,
`Virtual-<name>`, at the requested logical size and scale, adds it to the desktop
like a new monitor, and hands back a **PipeWire node** that streams exactly that
output. When the stream ends, the output goes away.

- The size is the client's own: `width × height` physical pixels at `scale`
  (an iPad asks for 2732×2048 at 2.0, so the desktop lays out at 1366×1024 and
  renders at full resolution). The host never scales video, and the client shows it
  pixel for pixel.
- The name is derived from the client certificate's fingerprint plus its display
  name (`streetlight-921c132be80b-mpimconiPad`), so it's stable per device. Window
  rules and KScreen's saved layout can refer to it, and two iPads don't collide.
- Without a virtual output (`virtual_display: false`), the host mirrors an existing
  output with `stream_output` instead.

### Keeping the monitor where it belongs

A new output isn't fully under the host's control after it appears:

- **Placement.** KWin may place the new output on top of an existing one. On the
  dev PC it landed at 2560,0, exactly over HDMI-A-1, so a game full screen on HDMI
  filled only part of the stream.
- **Scale.** KScreen applies its own configuration to an output it hasn't seen
  before; it reset the iPad's scale-2 output to scale 1 a moment after it appeared.

So once the output is up, the host reads every output's logical position and size
through **`xdg-output`**, and if the stream's output overlaps another or isn't at the
requested logical size, it runs **`kscreen-doctor
output.<name>.position.X,Y output.<name>.scale.S`**, putting it to the right of all
the others. KScreen then remembers that for the output's name, so the next stream from
the same device starts right. ([`capture/layout.rs`](crates/host/src/capture/layout.rs):
`arrange`, retried up to five times per stream.)

### Absolute input across the whole desktop

KWin maps an absolute pointer device (the host's `uinput` tablet and touchscreen)
across the **bounding box of all outputs**, not onto one monitor. A client's touch at
(x, y) on its screen is therefore first placed on the stream's output and then
re-expressed over the whole desktop's bounding box, from the same `xdg-output` layout
([`AbsMap`](crates/host/src/capture/layout.rs)). Because the layout can change after
the stream starts (the KScreen reset above, or you rearranging monitors), the host
**re-reads it every second for as long as the stream lasts** and remaps when the
output moves or resizes (`follow_layout` in [`server.rs`](crates/host/src/server.rs)).
Relative mouse motion bypasses all of this and goes to KWin 1:1, unaccelerated.

### Apps on the stream's monitor

An app launched for a stream gets `STREETLIGHT_OUTPUT` (the exact KWin connector
name, `Virtual-…`), `STREETLIGHT_WIDTH` and `STREETLIGHT_HEIGHT` in its environment.
KWin opens a new window on the active screen, which at the start of a stream is the
PC's own, so Steam Big Picture appeared on the main monitor. While a stream runs an
app, the host loads a small **KWin script** (through KWin's scripting D-Bus API) that
sends every new normal window to the stream's output, keeps it full screen if it was,
and brings it back if a game goes full screen on another monitor
([`placement.rs`](crates/host/src/placement.rs)). Only windows that open during the
stream are moved; the script is unloaded when the stream ends. Desktop streams load
nothing, and the host never edits your Plasma window rules.

### The cursor

PipeWire's `SPA_META_Cursor` metadata gives the cursor's bitmap and position
separately from the frame. Clients that can draw their own cursor get it as a
reliable QUIC stream (shapes) plus small datagrams (positions), so the pointer moves
at display rate without waiting for a video frame; others get the cursor embedded in
the picture.

### HDR

A guarded 10-bit path exists (BT.2020 PQ capture negotiation, P010 conversion, HEVC
Main10 with mastering-display SEI). The KWin this was built against rejects the
10-bit capture formats, so HDR is off by default and refused when unsupported.

## The pipeline, and how each stage is kept fast

The principle throughout: **no frame touches the CPU on either end**, every stage
works on one frame at a time with nothing queued behind it, and recovery from loss
is cheap enough that the stream never has to wait for a retransmission.

### 1. Capture: KWin → PipeWire, zero copy

- The PipeWire stream negotiates **DMA-BUF** buffers only (`SPA_DATA_DmaBuf`), with
  the **DRM format modifiers** the GPU can import (tiling and compression layouts),
  so KWin hands over the GPU buffer it composited into. There is no `glReadPixels`,
  no shared-memory fallback and no CPU copy.
- Frames carry **PipeWire header timestamps**, which the latency stats use instead of
  "when the host noticed".
- Capture is paced by the virtual output's own refresh at the client's rate, so the
  host encodes exactly the frames the client can show.

### 2. GPU colour conversion: VA-API VPP

- Each DMA-BUF is **imported into VA-API** as a `VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2`
  surface with its modifier: still the same memory, now usable by the encoder.
- **VPP** (the GPU's video processor) converts BGRx to NV12 (or P010 for HDR) with the
  right matrix and range in one GPU pass. Imports are cached per buffer, so a
  steady stream re-imports nothing.

### 3. Encode: VCN with nothing in the way

FFmpeg's VA-API encoders drive the GPU's fixed-function **VCN** encoder, configured for
latency rather than compression:

| Setting | Why |
|---|---|
| `max_b_frames = 0`, `b_depth = 1` | B-frames need future frames, which means waiting |
| `AV_CODEC_FLAG_LOW_DELAY`, closed GOP | the encoder emits each frame as soon as it's coded |
| `gop_size` effectively infinite | no periodic keyframes: an IDR only when a client asks (startup or unrecoverable loss), so bitrate never spikes on a timer |
| `async_depth = 1` | one frame in flight in the encoder, not a queue of them |
| CBR/VBR rate control, AUDs off | steady packet sizes and no wasted bytes |

**Adaptive bitrate** watches the client's feedback and treats its two signals
differently. **Loss** (lost frames, IDR requests, more than 2 % of frames rebuilt by
FEC) for two seconds cuts 15 %, down to a fifth of the client's request. **Queueing
delay** (RTT at least double the last 10–20 s's lowest *and* 15 ms above it) for three
seconds cuts only 10 %, never below half the request, because Wi-Fi RTT wanders
between 5 and 15 ms with no congestion at all. Three clean seconds raise it 10 % at a
time, up to the request. Changing bitrate **reopens only the codec context**; capture, the
VPP pipeline and the cached imports stay up, so an adjustment costs one fresh
reference chain, not a pipeline restart. **Auto codec** picks AV1 → HEVC → H.264 from
what the host's VA driver and the client's decoder both support (the Pi says HEVC).

### 4. Transport: one Reed-Solomon block per frame

Video goes over **raw UDP**, not TCP or QUIC streams, because waiting for a lost
packet stalls every frame behind it (head-of-line blocking) and congestion windows
slow down exactly when you need throughput.

- Each encoded frame is cut into equal **shards**, and `fec_percent` **parity shards**
  are added with `reed-solomon-simd` (SIMD, O(n log n), fast enough for 4K I-frames).
  The client rebuilds the frame from **any** `data_shards` of them, so a few lost
  packets cost nothing.
- Each shard is encrypted with **AES-128-GCM**, with the packet header as associated
  data and a nonce made of stream, frame and shard numbers, so tampering or replay
  is rejected per packet.
- Packets are sent with **UDP GSO**: up to 64 equal-sized datagrams handed to the
  kernel in one `sendmsg`, which segments them, so a 4K frame is a handful of
  syscalls instead of hundreds.
- **Pacing** spreads a frame's packets over part of the frame interval. On Wi-Fi a
  burst of a large I-frame overflows the access point's queues; pacing at ~2–4× the
  bitrate (e.g. 600 Mbit/s on Wi-Fi 6) avoids that, while wired clients can turn it off.
- If a frame can't be rebuilt, the client keeps showing the last good picture and
  asks for an IDR over QUIC; a 2 ms reordering window avoids treating late packets as
  lost.

### 5. Control, feedback and input: QUIC

- **QUIC** (`quinn`) carries pairing, the app list, start/stop and reliable ordered
  input on a dedicated stream, plus **datagrams** for frequent, loss-tolerant
  traffic (feedback, cursor positions). QUIC's own RTT estimate feeds adaptive bitrate.
- Input is accepted only from the stream's owner, and injected through **`uinput`**:
  separate virtual devices for keyboard, relative mouse, absolute pointer, multitouch
  (`INPUT_PROP_DIRECT`) and one gamepad per controller, created and removed as pads
  connect.

### 6. Audio: 5 ms Opus

The PipeWire sink monitor is encoded as **Opus at 48 kHz with 5 ms frames** in
low-delay mode (~5 ms algorithmic delay), sent over the same media socket. The Linux
client conceals a missing packet with Opus PLC; queues on both clients are bounded and
drop rather than build latency.

### 7. Decode and present: straight to the screen

- **Pi 5.** Its only video decoder is the HEVC block, used through FFmpeg's **V4L2
  request** hwaccel. Decoded frames come out as **DRM-PRIME** buffers (NV12 in
  Broadcom's SAND128 column layout) and are wrapped as a `GdkDmabufTexture` under a
  **`GtkGraphicsOffload`**, so the compositor (Cage) puts them on a hardware plane and
  **scans them out directly**: no GPU composition, no copy. That's why a 4K60 stream
  costs under a tenth of one CPU core.
- **Desktop Linux.** VA-API decode, `av_hwframe_map` to DRM-PRIME, the same GTK path.
- **iPad.** Annex-B access units become `CMSampleBuffer`s on an
  **`AVSampleBufferDisplayLayer`**, which decodes on the hardware HEVC/H.264 decoder,
  with `kCMSampleAttachmentKey_DisplayImmediately`, so frames are shown on arrival
  instead of being scheduled by timestamp.
- The webview UI sits on top only when it's needed; during a stream it's hidden, and
  the native video layer is the only thing on screen.

### Latency budget (4K60 on a LAN)

| Stage | Target | Measured |
|---|---|---|
| Capture (KWin → PipeWire) | ≤ 1 ms after compositor commit | |
| VPP + VCN encode | 2–4 ms | ~3.4 ms at 1080p60, import + convert + encode |
| Packetise + FEC + encrypt | < 0.5 ms | |
| Network | < 1 ms wired (+ pacing) | 2–6 ms capture → received, loopback 1440p60 |
| Decode | 3–6 ms | Pi 1080p 3.1 ms p95; Pi 4K 10.5–11 ms p95 |
| Present | ≤ 1 vblank | direct scan-out on the Pi |

## Wire protocol

QUIC ALPN `streetlight/3`; both ends must run the same protocol version.

| Port | Protocol | Use |
|---|---|---|
| 47800 | UDP (QUIC) | control, input, feedback, cursor shapes and positions |
| 47801 | UDP | video and audio media |
| 47990 | TCP (HTTP, loopback only) | host web UI |
| 5353 | mDNS | `_streetlight._udp.local` |

Media packet (UDP, little endian):

```
 0  u8   magic/version (0x5A)
 1  u8   stream        0 = video, 1 = audio, 2 = probe
 2  u16  shard_index
 4  u32  frame_number  (video frame / audio packet sequence)
 8  u16  data_shards
10  u16  parity_shards
12  u32  frame_bytes   (payload length before padding)
16  u32  capture_us    (host monotonic µs, low 32 bits)
20  u16  shard_bytes
22  u8   flags         bit 0 keyframe, bit 1 end of stream
23  u8   reserved
24  ..   AES-128-GCM(shard) || 16-byte tag   AAD = header, nonce = stream|frame|shard
```

Session: discover → QUIC with the client certificate → (first time) SPAKE2 pairing on
the PIN → `Hello{caps}` → `Start{display, codec, bitrate, fec, pacing, app}` → the host
creates the virtual output and replies with the codec, size, key and token → the
client sends a UDP probe with the token so the host learns its media address → media
flows; feedback and IDR requests go back over QUIC. Details:
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Security

- **Mutual TLS** on every connection with self-signed certificates, **pinned** on
  both sides after pairing; an unpaired client can only pair.
- **Pairing** is SPAKE2 over a 4-digit PIN shown on the client and typed on the host;
  it binds both certificate fingerprints, so a man in the middle can't pair. Concurrent
  pairings are refused and attempts are rate-limited per IP.
- **Media** is encrypted and authenticated per packet with a key sent over the QUIC
  channel for each stream; a media address is only learned from a probe carrying the
  stream's token.
- **The host web UI** listens on loopback only, checks `Host` and `Origin`, and needs
  a per-process token (in the page, and in `$XDG_RUNTIME_DIR/streetlight/token`,
  mode 0600); changes to app commands need confirmation.
- Only the stream's owner can send input; a second client gets `host busy`.

## Host setup (the PC)

Requirements: KDE Plasma 6 on Wayland, an AMD GPU with VA-API (Mesa radeonsi),
PipeWire, FFmpeg development headers, Rust.

```bash
cargo install --path crates/host            # → ~/.cargo/bin/streetlight-host
streetlight-host                            # run in a terminal, or as a user service:

cp packaging/streetlight-host.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now streetlight-host
```

- Open <http://127.0.0.1:47990> to approve pairings, unpair devices, and edit apps and
  limits. `streetlight-host pair 1234` approves a pending pairing from a terminal in the
  same session (both need `XDG_RUNTIME_DIR`).
- `streetlight-host capture-test 5 out.hevc 2560x1440` checks capture and encode with no
  client.
- The host needs write access to `/dev/uinput`: an ACL, or install
  `packaging/60-streetlight-uinput.rules`.
- Firewall: allow UDP 47800–47801 and mDNS (UDP 5353) from your LAN.
- Settings live in `~/.config/streetlight/` (`settings.json`, `clients.json`, the host
  certificate).

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

- The Pi's only hardware video decoder is HEVC, exposed by Raspberry Pi's FFmpeg as a
  DRM-PRIME hwaccel; keep its `--enable-v4l2-request` build. Frames stay in GPU memory
  all the way to GTK's offloaded subsurface. If GTK can't import the SAND128 buffers, the
  client logs `dmabuf texture import failed`; the tested Cage stack scans NV12 out
  directly, other compositors still need checking.
- DRM devices are found through `/dev/dri/by-path` and sysfs, preferring the `vc4-drm`
  display card (often `card1`); override with `SL_DRM_DEVICE` or `SL_VAAPI_DEVICE`.
- `--decode-only --seconds 15 --latency --report json` checks decoding without a window
  or audio; `--no-audio` turns audio off in GUI diagnostics.
- On GTK < 4.20 the client negotiates an explicit BT.601/full-range stream matching the
  tested Cage NV12 plane. See [docs/PI.md](docs/PI.md).
- `scripts/install-client.sh` installs the client and a desktop launcher under
  `~/.local`, plus an optional (not enabled) systemd user service. For a Pi OS Lite
  kiosk, run `cage -s -- ~/.local/bin/streetlight-client` on the graphical seat.

## iPad client

Two ways to use an iPad:

- **mpimc's Stream tab** (a media center app for iOS and Android) links `client-core`
  and ships a native player ported from `clients/ios`. This is the path that has been
  run on an iPad Air 13-inch (M2): pairing, Desktop at 2732×2048/60, touch, and
  full-screen games.
- **The standalone app in `clients/ios`** (needs a Mac; not yet built):

  ```bash
  rustup target add aarch64-apple-ios aarch64-apple-ios-sim
  brew install xcodegen
  cd clients/ios
  ./build-rust.sh          # builds client-core → StreetlightCore.xcframework
  xcodegen                 # generates Streetlight.xcodeproj from project.yml
  open Streetlight.xcodeproj   # set your signing team, run on the iPad
  ```

On the iPad:

- Video goes through `AVSampleBufferDisplayLayer` (hardware HEVC, shown immediately).
  At native resolution the host creates a 2732×2048 monitor at 2× scale at the
  iPad's refresh rate.
- Touch: tap or drag to click, two-finger tap to right-click, two-finger drag to
  scroll, three fingers to leave the stream; a touchscreen-passthrough mode sends raw
  multitouch instead.
- A hardware keyboard, a mouse (pointer locked while streaming) and MFi, Xbox and
  PlayStation controllers are passed through. Ctrl+Alt+Shift+Q leaves the stream.
- The defaults assume Wi-Fi: 20 % FEC and 600 Mbit/s pacing.

## Apps, games and Steam

The host's app list (web UI) has entries with an id, a label, a shell command and a
`stop_on_disconnect` policy; Desktop has no command. Each launch gets its own process
group: with `stop_on_disconnect: true` it's sent TERM when the stream ends, then KILL
after 500 ms; with `false` (the default) it's left running. Details:
[docs/APPS.md](docs/APPS.md).

**Steam Big Picture:** pick it from the client's app list (`steam
steam://open/bigpicture`, `stop_on_disconnect: false` for a shared Steam instance). The
host's placement script puts the Big Picture window, and the games launched from it,
on the stream's monitor. A Big Picture window that was already open before the stream
stays where it is; close it first. If you'd rather pin placement yourself, a KWin
window rule on the stream's output still works (the output's name, in `/api/status` as
`output_name`, is stable per device).

## Testing

```bash
scripts/e2e.sh                               # local KWin: video, gated input, gamepad, audio, the UI
scripts/loss-matrix.sh                       # packet loss, bursts and reordering (headless, scratch config)
scripts/pi-smoke.sh <pi-host> <host-ip>      # Pi: native decode and presentation
python3 scripts/pi-ui-smoke.py <pi-host> <host-ip>
SL_SOAK_SECONDS=1800 scripts/pi-soak.sh <pi-host> <host-ip>
```

- The scripts use isolated configuration and bounded processes. Input tests are
  **gated**: the local input probe must be full screen and focused on the streamed
  virtual output, and its heartbeat gates every keyboard and mouse event, because the
  host injects real input into the session it runs in.
- Test-only impairments (`SL_TEST_DROP`, `SL_TEST_BURST`, `SL_TEST_REORDER`,
  `SL_TEST_SEED`) and JSON stats (`--dump out.hevc --report json`) are documented with
  results in [docs/TESTING.md](docs/TESTING.md).

## Status and limitations

| Piece | State |
|---|---|
| Host: virtual monitor → zero-copy capture → HEVC/AV1/H.264 | working |
| Transport: FEC, encryption, GSO, pacing, pairing | working, unit- and integration-tested |
| Loss and reordering recovery | 1080p60 matrix passed (results above) |
| Linux client (desktop) and its webview UI | working |
| Pi 5 client | verified at 1080p60 and 4K60, 30-minute soak |
| iPad through mpimc's Stream tab | Desktop and games streamed, touch verified |
| Standalone `clients/ios` app | written, not yet built |
| Input: keyboard, mouse (absolute and relative), scroll, touch, gamepads | verified end to end on Linux; touch on the iPad |
| Adaptive bitrate, Auto codec / AV1 | verified on Linux |
| Client-drawn cursor and pointer lock | Linux overlay and manual Wayland lock (Ctrl+Alt+Shift+M) |
| App lifecycle | tracked process groups, stop/leave policy |
| HDR10 | guarded and off: this KWin rejects 10-bit capture |

Still open: the Pi's 4K decode p95 (10.5–11 ms vs 8 ms), HDR capture and display,
AV1 and an EDR/cursor layer on the iPad, automatic pointer-grab detection for games,
high-refresh virtual mode selection, and wider device and network testing.

## Repository layout

```
crates/proto         wire format, FEC, crypto, control messages, QUIC/TLS helpers
crates/host          streetlight-host: capture (KWin, PipeWire), GPU (VA-API), encode,
                     audio, input (uinput), media sender, QUIC server, web UI
crates/client-core   client transport and reassembly; Rust API + C ABI (for Swift)
crates/client-linux  Pi / Linux client (GTK 4 + WebKitGTK, dmabuf video, ALSA audio)
clients/ios          iPad app (Swift, AVSampleBufferDisplayLayer, WKWebView)
web/client           the client UI shared by the Pi and the iPad (one HTML file)
web/host             the host web UI
packaging            systemd units, a desktop entry, the uinput udev rule
scripts              end-to-end, loss, Pi smoke/soak and report scripts
docs                 architecture, testing evidence, Pi notes, apps
```

`plan.md` is the working plan and progress log that development follows.

## License

MIT or Apache-2.0, at your option ([LICENSE-MIT](LICENSE-MIT),
[LICENSE-APACHE](LICENSE-APACHE)).
