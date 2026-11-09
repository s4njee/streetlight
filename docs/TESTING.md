# Verification

P0 verification on **2026-11-08** on the development desktop (KWin/Wayland,
RX 9070 XT). The P0 results apply to this machine. Pi P1 results follow below; iPad remains unverified.

## Host web API

`cargo test -p streetlight-host --test web_api` serves the actual router on an ephemeral
loopback TCP port and makes HTTP requests. Its Host allowlist stays fixed at port 47990
so a test port does not widen the production policy.

Verified:

- Invalid/missing-port Host values return 403, including `GET /api/status` with
  `Host: evil.example:47990`.
- All three allowed authorities (`127.0.0.1:47990`, `localhost:47990`, `[::1]:47990`)
  accept settings updates with the correct token. A missing/wrong/duplicate token fails.
- Foreign, `null`, lookalike and HTTPS Origins fail for non-GET requests;
  an absent Origin is permitted with a valid token (CLI requests).
- App command changes require `"confirm": true` (409 otherwise), including additions,
  removals, changes by app ID, and duplicate-ID shadowing/reordering. Successful
  command changes are logged at warn level.
- Both `/` and `/index.html` contain the process token, with `Cache-Control: no-store`.
- Token files are atomically replaced with mode 0600 in a private, user-owned 0700
  directory. Token symlinks are replaced without modifying their targets; unsafe
  directory permissions are rejected.

The host requires `XDG_RUNTIME_DIR`; its token is written to
`$XDG_RUNTIME_DIR/streetlight/token`. The `pair` CLI reads that file and sends the
correct Host and `X-Streetlight-Token`. A fresh random 256-bit token is generated per
host run, after binding HTTP so a second host cannot overwrite a running host's token.

Live verification: the host UI rendered in the browser pane, populated status/devices/
settings, and tracked a running stream. `streetlight-host pair <PIN>` successfully
approved a real client using the isolated test token/config. UI mutation handling is
covered by HTTP tests and the shared fetch helper; app command confirmation was not
clicked in the browser.

## Loss/reordering matrix

Run from an idle host desktop:

```bash
scripts/loss-matrix.sh
```

The script refuses non-Wayland/non-KWin sessions and occupied host ports, builds release
binaries/examples, creates scratch config and runtime/token directories, then pairs a
headless dump client. For each case it plays `testsrc2` at 1920×1080@60 on the streamed
virtual output for a 15-second, 30,000 kbit/s HEVC session with default 20% FEC. It never
sends input or plays decoded audio. Every background process is bounded by `timeout`
and cleaned up by PID in an EXIT trap. Original Wayland/PipeWire sockets are used via
explicit runtime paths; real Streetlight settings, certificates and pairings are untouched.
Logs/dumps remain in the printed `/tmp/streetlight-loss.*` directory for inspection.
`SL_SKIP_BUILD=1` skips only the build when freshly built binaries already exist.

The test-only impairment variables apply to **both video and audio datagrams**, before
decryption/reassembly. They are off by default, validated at stream start, and log a
warning when enabled:

| Variable | Meaning |
|---|---|
| `SL_TEST_DROP=0.05` | Independently drop each packet with probability 0.05 |
| `SL_TEST_BURST=20:0.01` | When not already in a burst, start a 20-packet drop burst with probability 0.01 |
| `SL_TEST_REORDER=0.01` | Hold a packet, pass the next accepted packet first, then release it (1 ms idle fallback) |
| `SL_TEST_SEED=7` | Seed for reproducible decisions on the same packet arrival sequence |

Only the media client receives impairment variables; QUIC control remains unimpaired.
The reassembler permits 2 ms for a missing older frame when a newer frame completes,
then reports `Lost` and releases newer frames. Socket-timeout polling releases held
packets and completed frames even if traffic stops. Reordering, trailing gaps and
u32 sequence rollover have deterministic unit tests.

`--dump file.hevc --report json` emits one Stats JSON line per second, alongside normal
connection/log output. Totals include reassembled video frames, frames actually delivered
past keyframe gating, lost/recovered frames, IDR requests successfully sent, keyframes,
missing audio packets, injected drops, and maximum delivery gap/recovery time. `fps`
counts delivered frames; `frames` counts complete frames even if skipped while waiting
for a keyframe. Packet counters exclude injected drops. Maxima include an ongoing stall;
startup before the first keyframe is excluded from the delivery gap. Recovery time is
measured from the client's first outstanding Lost event to a delivered keyframe.
Host media logs include IDR-request and keyframe-send counts in each 5-second interval.

Measured final run (seed 7; artifacts `/tmp/streetlight-loss.Q8bzN1QW`):

| Impairment | Frames | Lost | FEC recovered | IDR requests | Keyframes | Dropped packets | Max gap ms | Max recovery ms | Decode errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| none | 894 | 0 | 0 | 1 | 1 | 0 | 83.30 | 0.00 | 0 |
| random2 | 896 | 0 | 529 | 1 | 1 | 1051 | 67.50 | 0.00 | 0 |
| random10 | 870 | 23 | 862 | 24 | 24 | 5423 | 67.20 | 15.07 | 0 |
| burst | 526 | 363 | 47 | 236 | 108 | 9220 | 349.98 | 313.67 | 0 |
| reorder | 892 | 0 | 4 | 1 | 1 | 0 | 84.33 | 0.00 | 0 |

Baseline averaged **59.6 fps** including startup. Random10's maximum recovery was
**15.07 ms**, below two 60 Hz intervals plus measured RTT (33.33 + 0.083 ms).
Burst loss's largest delivery gap was **349.98 ms**, below 500 ms. Random10 lost
33 audio packets; burst loss lost 428. Those audio counts come from the headless
receiver; PLC itself is verified separately below.

`ffmpeg -v error -threads 1 -i <dump>.hevc -f null -` produced no errors in any case,
including no RPS errors after recovery. `scripts/report-loss.py <artifact-directory>`
checks all five reports, decode logs, FEC recovery, no spurious baseline/reorder loss,
≥55 average baseline fps, ≤500 ms gaps, and ≤2 frame intervals plus RTT for random10
recovery. Startup is included in the 15-second frame totals. Reorder-only FEC recovery
can be nonzero when a parity packet arrives before a delayed original shard.

## Audio loss

The UDP receiver test drops an entire audio sequence number, verifies a single loss
callback between the surrounding packets, and verifies that later audio/video continues.
Linux playback and the diagnostic self-test use Opus PLC for that callback:
`decode_float(&[], <240 stereo samples>, true)`. A real Opus encoder/decoder test verifies
240 samples/channel from concealment and successful subsequent decoding, without
opening an audio device. At most 100 missing packets are concealed after a long outage
to avoid building a playback backlog. The default platform loss callback leaves a short
gap; C ABI/iPad playback does not yet implement PLC and remains untested on hardware.

## Build and static checks

```bash
cargo build --release --workspace --bins --examples
cargo test --workspace
cargo build --release -p streetlight-client --features webview --bins --examples
cargo clippy --workspace
```

The P0 release workspace/webview builds and their 13 tests passed. At that checkpoint, `client-core/src/ffi.rs` had 20
existing `clippy::not_unsafe_ptr_arg_deref` errors, so the plain workspace clippy command
failed. With only that lint allowed:

```bash
cargo clippy --workspace -- -A clippy::not_unsafe_ptr_arg_deref
```

the workspace check passes with existing warnings (generated VA bindings, unused decoder/
settings/modifier fields, and existing audio/capture/input/session-start style warnings).
No new warnings were introduced by P0. P1 status follows below.


## P1 / Pi verification (2026-11-09)

Native Pi build: Rust 1.99.1, GTK 4.18.6, WebKitGTK 2.54.0, FFmpeg 7.1.5 (Raspberry Pi
request-decoder/SAND patches), 4 GB Pi 5 at `neo.local` on Gigabit Ethernet. The same
`ffmpeg-sys-next` 9 crate builds against those installed FFmpeg 7 headers. See [PI.md](PI.md)
for setup, exact scripts, install/launch steps, and outstanding physical-device checks.

The decoder discovers vc4-drm `/dev/dri/card1` and logs hardware HEVC via `/dev/media0`
and `/dev/video19`. Cage advertises NV12/SAND128. GTK offload logs show the dmabuf
subsurface attached, and `kmsprint` confirms NV12 directly on a 3840×2160 HDMI plane.
No separate KMS implementation or CPU pixel-copy fallback was needed on this machine.

The host now requests `spa_meta_header` metadata and sends its per-buffer `pts`, which
was verified in the host log. The former `pw_stream_get_time().now` fallback was stale
for this video source and produced a growing, invalid latency; it is no longer used.
An explicit receive-time approximation is logged if a source supplies no metadata.
Only samples after clock synchronization contribute to capture-to-complete percentiles.
Actual GDK presentation feedback is associated with the frame clock's painted frame;
zero/absent feedback is not replaced with a predicted time. Decode work is measured
separately from decode-complete → presentation queue time.

Each smoke mode ran for approximately 15 seconds. Counts include startup; the GUI
one-second timer can extend the first interval. Values below are the largest p95 of
the five-second windows from the final timestamp/color-corrected run (BT.601/full). All modes had **zero
lost frames, bad packets, and decode errors**.

| Mode | Decoded frames | Presented frames | Capture→complete p95 ms | Decode p95 ms | Queue→present p95 ms | Capture→present p95 ms |
|---|---:|---:|---:|---:|---:|---:|
| 1080p60 decode only, 30 Mbit/s | 882 | — | 4.416 | 3.112 | — | — |
| 4K60 decode only, 80 Mbit/s | 890 | — | 12.764 | 10.490 | — | — |
| 1080p60 GTK | 883 | 879 | 4.403 | 3.079 | 16.244 | 23.368 |
| 4K60 GTK | 887 | 886 | 12.727 | 10.990 | 16.617 | 39.505 |

Artifacts: host `/tmp/streetlight-pi.z50Gf2Cb`, Pi `/tmp/streetlight-test.XhZWGqlV`.
Both dump modes passed color metadata and six-bar checks: maximum channel error
4/255 at 1080p and 2/255 at 4K. Both presentation modes report an active NV12 plane
with BT.601/full, matching the negotiated stream.
The **4K ≤8 ms decode target is not met**. `scripts/pi-smoke.sh` exits nonzero for
those two latency checks; all functional/loss/presentation checks pass. 1080p meets
the Pi's decode target and the 5 ms capture→complete target over the tested LAN.

The Pi's actual WebKit UI connected using a bootstrap pairing, cancelled an
in-flight Start and restarted Desktop through the asynchronous bridge. The test waits
for all three host sessions (bootstrap, cancelled start, restarted stream), ensuring
it cannot pass on the first start alone. After six seconds the final feedback was
`last_frame=344`, `frames_lost=0`, `idr_requests=1`, `decode_us_avg=2942`.
Normal UI exit stopped the session and closed QUIC without an abort. Artifacts:
`/tmp/streetlight-pi-ui.d55iqhcy`, Pi `/tmp/streetlight-ui.FPVUSK2M`.

Audio initially failed because ALSA default playback bypassed PipeWire; it entered a
`POLLERR` loop after opening its fixed buffer. The failed 30-second check was stopped,
and its repeated-error log was compacted. `pipewire-alsa` is now installed/documented.
Playback appears as `PipeWire ALSA [streetlight-client]` linked to both channels of the
HDMI sink. Backend errors are counted, only the first is logged, and a failed stream
is closed promptly. A fresh 30-second 4K60/audio check passed with zero underruns/errors
at fixed 240 frames (5 ms). Physical tone audibility is pending user confirmation.

The **30-minute 4K60 / 80 Mbit/s audio-enabled soak passed** (1800 Stats reports):

| Metric | Measured |
|---|---:|
| Decoded / presented frames | 107,610 / 106,752 |
| Lost frames / decode errors / bad packets | 0 / 0 / 0 |
| Audio underruns / backend errors / lost packets | 0 / 0 / 0 |
| Client CPU, mean / max (100% = one core) | 9.43% / 9.76% |
| Temperature maximum / throttle flags | 51.25°C / 0 |
| RSS, first → last | 155.97 → 156.97 MiB |
| Largest five-second decode p95 | 10.978 ms |
| Final delivered fps | 59.985 |

Artifacts: host `/tmp/streetlight-soak.FGQfYJom`, Pi `/tmp/streetlight-soak.Lb7wG05K`;
`summary.json`, per-minute `metrics.jsonl`, client/host logs, and KMS properties are retained.
This run validates sustained transport, decode, presentation and HDMI audio routing.
It used the original BT.709 limited stream. Its active NV12 plane reported **BT.601/full**,
exposing a color mismatch; it does **not** establish correct color presentation.

The subsequent compatibility fix negotiates BT.601/full on Pi/GTK <4.20 and explicitly
converts/encodes it at the host. See [PI.md](PI.md) for the GTK limitation and override.
The broad VA BT.601 preset initially altered primaries as well as the matrix; flat
samples exposed that error. Explicit VA properties retain sRGB primaries/TRC and
select only the BT.601 matrix. `scripts/check-color.py` verifies full-range/BT.470BG
matrix/sRGB transfer/BT.709 primaries metadata and median RGB values in six flat bars
against the tagged source. A 1080p check gave maximum channel error **3/255**
(tolerance 6); artifacts `/tmp/streetlight-pi.A7okFAmx/decode1080.color-check.json`.
The source is explicitly tagged to prevent mpv guessing a different HD matrix.

The user confirmed the corrected 4K60 TV picture looked good during a separate
preview (`/tmp/streetlight-soak.3yoO8wt2`, Pi `/tmp/streetlight-soak.Za8z16V9`). That
preview was stopped after confirmation and is not counted as another full soak.
The final corrected-color smoke table is above; the 30-minute run was not repeated
merely to attach its duration to the color fix.

Additional P1 regression coverage:

- Actual QUIC tests reject a concurrent pairing, retain the first pending request,
  clear it on disconnect, and enforce one accepted attempt per IP per five seconds.
  A competing paired client's Start returns `host busy` before capture is touched.
- Input and feedback are accepted only from the session owner; ending a session drops
  its virtual input devices. `--no-input` also disables gamepad forwarding.
- Feedback sends the actual delivered frame number plus IDR requests. The ALPN was
  changed to `streetlight/2`; both clients and host must be rebuilt.
- Virtual output names include the certificate fingerprint prefix and display name.
  `/api/status.status.output_name` lets scripts target the exact monitor.
- `node scripts/check-bridge.mjs` verifies one-shot PIN delivery and 500 cancelled
  pairing subscriptions without stale callbacks.
- The input probe writes a heartbeat only while fullscreen/focused on the target output;
  selftest stops new keyboard/mouse events if that heartbeat becomes stale. Release
  events remain permitted. A unit test rejects a stale heartbeat.
- The unimplemented Text message and unused `sl_input_text` entry point were removed.
  Hardware key scancodes remain supported; Unicode text composition remains unavailable.
- Raw-pointer C exports have explicit unsafe Rust contracts and safety documentation.
  Their C calling conventions are unchanged. Plain workspace clippy now completes;
  only existing warnings remain.

`scripts/e2e.sh` passed on the final source (artifacts `/tmp/streetlight-e2e.Kwqp3aA7`):
471 frames in eight seconds, zero lost frames and an error-free FFmpeg decode; exact
absolute/relative pointer coordinates, safe key down/up pairs, scroll/button/touch
sequences; virtual gamepad press/release/stick/removal; 440 Hz at −31.0 dBFS; actual
WebKit stream with zero-loss frame feedback. Only the focused virtual-output probe
received keyboard/mouse events.

The script exposed two lifecycle issues now corrected: fire-and-forget Stop followed
by immediate runtime shutdown left the host busy, and lazily created uinput devices
could lose the first event before KWin discovered them. Stop now awaits host release
and flushes QUIC close; the injector waits for compositor discovery. GUI shutdown
explicitly releases transport before main-thread Tokio TLS teardown, avoiding an
exit-time abort. In-flight UI cancellation queues Stop without blocking GTK.

Final release workspace and webview builds, **18 Rust tests**, plain workspace clippy,
`node scripts/check-bridge.mjs`, shell syntax and Python compilation pass. Existing
clippy warnings remain; the former raw-pointer C ABI lint failures are fixed.

Still pending beyond this Pi-focused pass: automatic host GPU selection on multi-GPU
systems, measured 120/144 Hz virtual refresh selection, encoder quality/QVBR/VMAF tuning,
physical Pi keyboard/mouse/Xbox/DualSense checks and tone audibility confirmation,
and the Mac/iPad build and hardware matrix.


## P2 verification (2026-11-09)

[P2.md](P2.md) records the changing-loss bitrate/codec matrix, focused Wayland lock
and cursor checks, native Pi Auto/HEVC negotiation, app process lifecycle tests, and
negative HDR negotiation evidence. The current ALPN is `streetlight/3`; earlier P1
protocol/version references above are historical. The P2 source passes 27 Rust tests,
plain clippy with existing warnings and release workspace/webview builds. Earlier
normal and locked E2E runs passed; the latest rerun skipped keyboard/mouse after
probe focus changed and correctly failed its strict input assertions. Details and
final input-free Pi NV12/color checks are in P2.md.
HDR acceptance remains incomplete because the available KWin source rejects the
required 10-bit capture format. The previous 30-minute Pi soak was a P1 measurement;
it has not been relabeled as a full P2 soak.
