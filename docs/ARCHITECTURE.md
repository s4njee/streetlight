# Streetlight — architecture

Low-latency, high-fidelity game/desktop streaming. One **host** (Linux, KDE Plasma
Wayland, AMD VA-API) streams to **clients** (Raspberry Pi 5, iPad). Native media
paths everywhere; HTML/JS in a webview for every menu and settings screen.

```
                    HOST (Linux / KWin / RX 9070 XT)                      CLIENT (Pi 5 / iPad)
 ┌───────────────────────────────────────────────────────────┐   ┌──────────────────────────────────────┐
 │ KWin virtual output (client's exact WxH)                   │   │ webview UI (shared HTML/JS)           │
 │   └─ zkde_screencast → PipeWire node (DMA-BUF, modifiers)  │   │   ↕ window.webkit.messageHandlers     │
 │        └─ VA-API import (zero copy)                        │   │ client-core (Rust, C ABI for Swift)   │
 │             └─ VPP: BGRx → NV12/P010 (BT.709/BT.2020)      │   │   UDP rx → decrypt → RS-FEC → frame   │
 │                  └─ HEVC/AV1 encode (VCN, no B-frames)     │   │   QUIC control / input / feedback     │
 │                       └─ shard → RS-FEC → AES-GCM → UDP ───┼──►│ decoder                               │
 │ PipeWire sink monitor → Opus (5 ms) ───────────────────────┼──►│   Pi: V4L2 HEVC → DRM-PRIME dmabuf     │
 │ uinput (kbd / mouse / pad / touch) ◄── QUIC ◄──────────────┼───│   iPad: VideoToolbox → CVPixelBuffer   │
 │ axum web UI (127.0.0.1) — pairing PIN, settings            │   │ present: zero-copy plane / Metal      │
 └───────────────────────────────────────────────────────────┘   └──────────────────────────────────────┘
```

## Why these choices

| Concern | Choice | Reason |
|---|---|---|
| Codec | **HEVC** (Main / Main10), AV1 optional | Pi 5 has *only* an HEVC hardware decoder. iPad: HEVC everywhere, AV1 on M3/M4/A17 Pro+. |
| Capture | KWin `zkde_screencast_unstable_v1.stream_virtual_output_with_description` | Creates a monitor at the client's native resolution → no scaling, pixel-exact. Falls back to `stream_output` (mirror). |
| Zero copy | PipeWire DMA-BUF w/ explicit modifiers → `vaCreateSurfaces(DRM_PRIME_2)` | Frame never touches the CPU. Colour conversion on GPU (VPP). |
| Encoder | FFmpeg `hevc_vaapi` / `av1_vaapi` via `ffmpeg-sys-next` | Mesa radeonsi VCN. `bf=0`, very long GOP, `async_depth=1`, CBR/VBR, IDR on demand. |
| Video transport | Raw UDP, **one Reed-Solomon block per frame**, per-packet AES-128-GCM | No head-of-line blocking or congestion-window stalls; FEC hides Wi-Fi loss (iPad); `reed-solomon-simd` is O(n log n) and handles 4K I-frames. |
| Control / input | QUIC (`quinn`), mutual TLS with pinned certs | Reliable ordered input, datagrams for feedback, built-in RTT. |
| Pairing | SPAKE2 over a 4-digit PIN, binds both cert fingerprints | PIN shown on client, typed into host web UI (Moonlight-style). |
| Audio | Opus 48 kHz, 5 ms frames, restricted-lowdelay | ~5 ms algorithmic delay. |
| Input | `uinput` virtual devices | Works under any compositor, exposes real gamepads to games/Steam. |
| UI | Webview everywhere (WebKitGTK 6 on Pi, WKWebView on iPad) | Both expose `window.webkit.messageHandlers.<name>.postMessage`, so one HTML/JS bundle serves both clients. |

## Repository layout

```
crates/proto        wire formats: media packet header, FEC, crypto, control messages
crates/host         streetlight-host daemon (capture, encode, audio, input, net, web UI)
crates/client-core  client transport + reassembly; Rust API + C ABI (for Swift)
crates/client-linux Pi 5 / desktop Linux client (GTK4 + WebKitGTK webview + dmabuf video)
clients/ios         iPad app (Swift, VideoToolbox, WKWebView) linking client-core.xcframework
web/host            host web UI (pairing, settings)
web/client          client UI shared by Pi and iPad
```

## Ports

| Port | Proto | Use |
|---|---|---|
| 47800 | UDP (QUIC) | control, input, feedback |
| 47801 | UDP | video + audio media |
| 47990 | TCP (HTTP, loopback only) | host web UI |
| mDNS | `_streetlight._udp.local` | discovery |

## Media packet (UDP, little endian)

```
 0  u8   magic/version (0x5A)
 1  u8   stream        0=video 1=audio 2=probe
 2  u16  shard_index
 4  u32  frame_number  (video frame / audio packet seq)
 8  u16  data_shards
10  u16  parity_shards
12  u32  frame_bytes   (payload length before padding)
16  u32  capture_us    (host monotonic µs, low 32 bits)
20  u16  shard_bytes
22  u8   flags         bit0 keyframe, bit1 end-of-stream
23  u8   reserved
24  ..   AES-128-GCM(shard) || 16-byte tag    AAD = header, nonce = stream|frame|shard
```

Every frame (an HEVC access unit, Annex B) is split into `data_shards` equal shards,
`parity_shards = ceil(data_shards * fec_percent)` are added, every shard is encrypted
and sent paced across a fraction of the frame interval. The client can rebuild the
frame from **any** `data_shards` of the shards it receives. If a frame can't be
rebuilt, the client asks for an IDR over QUIC and holds the last good picture.

## Session flow

1. Client discovers host via mDNS (or manual IP), opens QUIC with its client cert.
2. Unpaired → `PairRequest` → SPAKE2(PIN) → both fingerprints MAC'd → host stores client.
3. `Hello{caps, display}` → host creates a virtual output at `display.width × display.height`.
4. Host replies `StreamStart{codec, size, fps, key, media_port, token}`.
5. Client sends a UDP `probe` with `token` → host learns the client's media address.
6. Media flows; client sends `Feedback` datagrams (loss, decode/present latency), `RequestIdr`.
7. Input events flow over a dedicated QUIC uni-stream.

## Latency budget (target, 4K60 LAN)

| Stage | Target |
|---|---|
| Capture (KWin → PipeWire) | ≤ 1 ms after compositor commit |
| VPP + VCN encode | 2–4 ms |
| Packetise + FEC + encrypt | < 0.5 ms |
| Network (wired GbE) | < 1 ms (+ pacing ≤ 25 % of frame interval) |
| Decode (Pi V4L2 HEVC / VideoToolbox) | 3–6 ms |
| Present (direct plane / Metal) | ≤ 1 vblank |

## Roadmap

1. **Host pipeline**: virtual output → DMA-BUF → VA-API HEVC → file (verify).
2. **Transport**: proto crate, UDP media + QUIC control, loopback client.
3. **Linux client**: decode (VA-API on desktop, V4L2-request on Pi) → GTK4 offload + WebKit UI.
4. **Input + audio**, pairing, web UIs.
5. **iPad client**: Swift app + client-core xcframework.
6. HDR (Main10 / PQ), AV1, adaptive bitrate, client-side cursor, Pi KMS "direct" mode.
