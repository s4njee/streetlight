# Streetlight: implementation and test plan

This is the brief for an agent continuing work on Streetlight, a low-latency game and desktop
streaming system: a Linux host streams to a Raspberry Pi 5 or an iPad. It covers what exists,
what is verified, what's left, the order to do it in, and how to prove each piece works.

Read first: [README.md](README.md) (setup and status) and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
(design, wire format, ports). Baseline commit: `fd962b4`.

## P0 checkpoint (2026-11-08; P1 updates below)

- **§2.1 implemented:** Host/Origin middleware, process token in HTML and private runtime
  file, CLI token support, confirmed/warn-logged app command changes, real HTTP integration tests.
  Browser-pane rendering and actual CLI pairing verified with scratch config/runtime.
- **§2.2 implemented and measured on the dev PC:** test-only packet loss/burst/reorder,
  JSON receiver stats and host IDR/keyframe counters, 2 ms reordering tolerance, Linux Opus PLC,
  and `scripts/loss-matrix.sh`. All five 15-second cases passed with zero decode errors.
  2% random: 0 lost / 529 FEC recovered; 10%: 23 lost / ≤15.07 ms recovery;
  burst: ≤349.98 ms delivery gap; reorder: 0 lost.
- Release workspace binaries/examples build and 13 tests pass. Plain workspace clippy remains
  blocked by 20 pre-existing raw-pointer lint errors in unchanged `client-core/src/ffi.rs`;
  allowing only that lint passes without new warnings.
- Exact measurements, reproduction, definitions and limitations: [docs/TESTING.md](docs/TESTING.md).
  Pi/iPad remain unverified; C ABI/iPad audio currently leaves a gap rather than doing PLC.
  This is the historical P0 checkpoint; current P1 results are below.

## P1 progress (2026-11-09; Pi-focused at user's request)

- **§2.3 implemented and passed:** `scripts/e2e.sh` builds release bins/examples and
  webview, isolates configuration, verifies video plus focused/heartbeat-gated input,
  gamepad creation/removal, 440 Hz audio, and the real WebKit UI. It also exposed and
  fixed graceful session teardown and first-event loss during KWin input discovery.
- **§2.4 items 1–4 and 6–11 implemented/verified:** concurrent pairing rejection/IP
  rate limit with real QUIC tests; competing sessions receive `host busy`; owner-only
  input/feedback; actual frame number/IDR feedback; real GDK presentation timing;
  fingerprint output names; one-shot pairing listeners; asynchronous/cancellable UI
  startup; PipeWire header timestamps; removed/documented unsupported Text; explicit
  audio buffering, backend errors and underrun counters. Item 5's client discovery
  is verified on Pi, but automatic host multi-GPU selection remains pending.
- **§2.5 partial:** `--latency` prints bounded p50/p95/p99 samples every five seconds;
  real Pi timings are in `docs/TESTING.md`. 120/144 Hz delivery/refresh selection and
  encoder quality/QVBR/VMAF comparisons remain pending.
- **§3 native bring-up verified:** Rust/webview builds on the 4 GB Trixie Pi; vc4-drm
  discovery selects card1; HEVC request decode and NV12/SAND128 offload work at
  1080p60/4K60. Correct legacy color conversion was checked separately from the soak.
  Packaging is installed on `neo.local` under `~/.local`, with a desktop entry and
  optional disabled service; the existing kiosk is preserved.
- **§3 audio/performance:** 30 minutes at 4K60/80 Mbit/s with 107,610 decoded frames,
  zero lost frames/decode errors/audio underruns/backend errors; CPU mean 9.43% of
  one core, maximum 51.25°C, no throttling. Decode p95 reaches 10.978 ms: the ≤8 ms
  target is explicitly **not met**. The soak exposed an NV12 matrix/range mismatch;
  corrected color negotiation/conversion is validated in separate smoke checks.
- Physical Pi keyboard/mouse/Xbox/DualSense checks and tone audibility
  remain pending; the user confirmed the corrected TV picture. Mac/iPad work (§4) is deferred; the Mac was not accessed.
- Release workspace/webview builds, 18 Rust tests, bridge subscription check and plain
  workspace clippy pass (existing warnings only). The old P0 FFI lint blockers are fixed.
  Rebuild both ends for the `streetlight/2` protocol. Exact evidence and remaining
  checklist: [docs/TESTING.md](docs/TESTING.md), [docs/PI.md](docs/PI.md).

## P2 progress (2026-11-09)

- **§5.1 implemented/verified:** feedback-driven bitrate control, 15% reductions after
  sustained loss/RTT growth, 5% clean recovery, request ceiling and bounded floor;
  codec-only reopen retains capture/VPP/imports and starts a fresh reference chain.
  The scripted changing-loss run measured 30,000 → 15,659 → 18,126 kbit/s with clean
  decoded output and a single capture instance.
- **§5.2 Linux implementation verified:** checked SPA cursor bitmap/position parsing,
  reliable shape transport plus independent QUIC position datagrams, epoch/serial
  filtering and refresh, GTK overlay with hotspot/letterbox scaling and local motion.
  Normal selftest received nine positions and a valid bitmap. Pi overlay verification
  uses a synthetic cursor fixture with no input injection. On Cage/GTK 4.18 it forces
  RGB composition, so the Pi retains embedded cursors by default (`SL_CLIENT_CURSOR=1`
  opts in). iPad CALayer work is deferred.
- **§5.3 manual Linux pointer lock implemented/verified:** GTK-surface Wayland constraints
  and unaccelerated relative motion, Ctrl+Alt+Shift+M toggle, focus/end/disabled-input
  release and absolute fallback. The focused virtual probe measured exact +30,+20
  relative motion and lock/release events. Automatic host-game grab detection remains pending.
- **§5.4 guarded implementation; acceptance blocked:** 10-bit RGB BT.2020/PQ capture
  negotiation, explicit P010 conversion, HEVC Main10, configured mastering/content-light
  SEI and GTK PQ state. The available KWin source rejects the 10-bit input formats.
  HDR is off by default; unsupported/unverified capture is rejected. Actual Main10/SEI
  stream output, physical HDR presentation and iPad EDR remain unverified.
- **§5.5 implemented/verified on Linux:** Auto selects AV1 → HEVC → H.264 from real VA
  profile/FFmpeg support; explicit unsupported codecs are rejected. Fixed accidental
  software AV1 selection and added raw-OBU temporal delimiters. AV1 and H.264 hardware
  decode/dump checks passed; Pi Auto selects HEVC and explicitly rejects AV1 before capture.
- **§5.6 implemented:** tracked per-app process groups, configurable stop/leave policy,
  reserved group leader IDs, reaping, status/UI controls and Steam/KWin placement recipe.
  Process lifecycle tests pass; the user's Steam and KWin rules were not changed.
- Both ends require **`streetlight/3`**. Full evidence, commands and remaining limits:
  [docs/P2.md](docs/P2.md), [docs/APPS.md](docs/APPS.md). Release workspace/webview builds,
  27 Rust tests, bridge subscription tests and plain workspace clippy pass (existing
  warnings only). Earlier normal and locked-pointer E2E checks passed; a final input
  rerun safely skipped injection after probe focus changed and failed its strict
  assertions. Input-free Pi checks retain direct NV12; 4K decode p95 11.152 ms still
  misses the P1 8 ms target.

---

## 0. Ground rules for the implementing agent

### 0.1 This machine is the user's live desktop

The dev PC (`freya`: KDE Plasma 6.8 on Wayland, RX 9070 XT, CachyOS) is the user's
everyday machine. The host injects real input into it through `uinput`. Earlier testing showed
how easily that leaks into the user's session. Follow these rules:

1. **Never run the client fullscreen with input enabled on this machine.** Always pass
   `--windowed --no-input` unless the test is the gated input self-test (rule 2).
2. **Keyboard/mouse injection tests must be gated.** Use `--selftest` with `SL_SELFTEST_GATE`
   and the `input_probe` example (see §2.3). The client sends keyboard and mouse events only after
   the probe confirms it is fullscreen and focused on the streamed virtual display.
3. **Only use keys with no shortcuts.** F13–F24 map to XF86Tools/XF86Launch*, and KDE binds
   those to apps. Media keys, Meta and Print are also off limits. Modifier taps plus `a` are safe.
4. **Isolate config.** Point `XDG_CONFIG_HOME` at a scratch directory for every test so the user's
   `~/.config/streetlight*` (pairings, certificates) is never touched.
5. **Give every background process a `timeout N` and stop it by PID.** Never use
   `pkill -f <pattern>`: the pattern matches the shell running it, which kills your own command
   (this happened twice). `kill $PID` is fine.
6. **Guard `rm` targets:** write `rm -f "${S:?}"/file`, never `rm -f $S/file`.
7. **Build with `--bins --examples`.** `cargo build --examples` alone leaves the old binary in
   place. That once caused a stale client to ignore `--selftest` and open fullscreen with input
   live. (Unknown flags are rejected now, but rebuild properly anyway.)
8. **Audio tests:** play test tones at `pw-play --volume 0.08` or quieter, for ≤4 s. Never play the
   client's decoded audio on this machine: it would feed back into the host's capture.
9. **Don't screenshot the user's desktop to "see" a window.** Focus moves unpredictably (mpv took
   focus twice), and full-desktop captures show the user's private windows. Prefer logs, stats and
   the probe window. If you need a picture of the stream, decode a `--dump` file with ffmpeg.
10. **Commits:** work on a branch, use the repo-local git identity that's already set, and end
    each commit message with the attribution trailer the harness gives you. Don't push without being asked.

### 0.2 Available hardware

The user authorized SSH to the Pi at `neo.local` and asked to work on the Pi for now.
It is a 4 GB Pi 5, Trixie 13.7, Gigabit Ethernet, Cage/Chromium and a 4K60 TV.
Preserve its kiosk/startup configuration. The Mac at `192.168.1.251` and iPad work are
deferred; the Mac has not been accessed. Never mark a hardware task done without evidence.
The user confirmed the TV picture; tone audibility and USB/controller observations remain outstanding.

### 0.3 Build and run reference

```bash
cargo build --release --workspace --bins --examples          # host + Linux client (no webview)
cargo build --release -p streetlight-client --features webview --bins --examples
cargo test --workspace                                       # proto FEC/pairing + host layout tests
S=$(mktemp -d); export XDG_CONFIG_HOME=$S/cfg                # isolated config for a test run

timeout 60 ./target/release/streetlight-host serve > $S/host.log 2>&1 & HP=$!
./target/release/streetlight-client 127.0.0.1 --dump $S/out.hevc --seconds 8   # headless
./target/release/streetlight-client 127.0.0.1 --windowed --no-input            # GUI, direct
SL_UI_EVAL='…js…' ./target/release/streetlight-client --windowed --no-input     # webview UI, scripted
./target/release/streetlight-host pair <PIN>                 # approve a pairing from a shell
./target/release/streetlight-host capture-test 5 $S/c.hevc 2560x1440   # capture+encode only
```

There's no motion on the virtual display by default. Obtain the exact output name
(fingerprint prefix + client display name) from the running host, then verify the
`Virtual-` prefixed monitor exists before playing a pattern:

```bash
output=$(python3 -c 'import json,urllib.request; print(json.load(urllib.request.urlopen("http://127.0.0.1:47990/api/status"))["status"]["output_name"])')
wayland-info | rg "Virtual-$output"  # required before fullscreen playback
timeout 6 mpv --really-quiet --no-audio --fs --fs-screen-name="$output" \
  --screen-name="$output" "av://lavfi:testsrc2=size=1920x1080:rate=60"
```

Decode check for a dump: `ffmpeg -v error -threads 1 -i out.hevc -f null - 2>&1 | grep -c 'Could not find'` must be `0`.

---

## 1. Current state (baseline `fd962b4`, updated with P0/P1 evidence)

| Area | State | Evidence |
|---|---|---|
| Capture: KWin `zkde_screencast` virtual output → PipeWire DMA-BUF (explicit modifiers) | ✅ | tiled modifier `0x200000000082305` negotiated; frames are correct |
| VA-API import + VPP (BGRx→NV12 BT.709 limited) + `hevc_vaapi` | ✅ | ~3.4 ms per 1080p frame; VBR default (CBR wasted ~25 % on filler) |
| Wire: per-frame RS FEC, AES-128-GCM, GSO send (≤64 KiB per send) | ✅ | unit tests; 1440p60 @ 80 Mbit/s, 0 loss, 0 decode errors |
| QUIC control, SPAKE2 PIN pairing, cert pinning | ✅ | pair via CLI and via web UI |
| Host web UI (127.0.0.1:47990) | ✅ hardened after baseline | Host/Origin/token checks + command confirmation; HTTP tests and browser-pane rendering |
| Linux client: VA-API decode → dmabuf → GTK4 `GraphicsOffload` | ✅ on dev PC | ~2 ms decode; no import errors |
| Linux webview UI (WebKitGTK 6) + bridge | ✅ | discover → connect → apps → stream 2560×1440@144 from the UI |
| Input: keys, abs + relative mouse, buttons, scroll, touch, gamepad | ✅ | gated self-test + `input_probe`; python-evdev on the virtual pad |
| Audio: PipeWire monitor → Opus 5 ms → client | ✅ | 440 Hz tone decoded at the expected RMS, continuous |
| Pi 5 decode path (V4L2-request DRM-PRIME, SAND128) | ✅ hardware verified | 1080p60/4K60, 30-minute soak; 4K decode p95 misses 8 ms target |
| iPad app (Swift, `AVSampleBufferDisplayLayer`, WKWebView, GameController) | ❓ written, never compiled | Mac available; work deferred at user request |
| Loss recovery (FEC + IDR on loss) | ✅ injected UDP loss on dev PC | five-case matrix in `docs/TESTING.md`; physical network/device loss remains unverified |
| Virtual display refresh > 60 Hz actually delivered | ❓ unknown | 144 fps was negotiated, but delivery wasn't measured |

Key source files: `crates/host/src/{server,video,encode,media,input,capture/*}.rs`,
`crates/proto/src/{fec,control,net,pair}.rs`, `crates/client-core/src/{lib,ffi}.rs`,
`crates/client-linux/src/{main,view,decode,session,bridge,selftest}.rs`,
`clients/ios/Streetlight/*.swift`, `web/{client,host}/index.html`.

---

## 2. Priority order

| # | Workstream | Needs hardware? | Priority |
|---|---|---|---|
| 2.1 | Lock down the host web API (DNS rebinding → command execution) | no | **P0** |
| 2.2 | Loss/jitter test harness + FEC/IDR recovery verification | no | P0 |
| 2.3 | Turn the self-tests into a repeatable script (`scripts/e2e.sh`) | no | P1 |
| 2.4 | Correctness fixes from code review (§2.4 list) | no | P1 |
| 2.5 | Latency instrumentation + virtual-display refresh rate | no | P1 |
| 3 | Raspberry Pi 5 bring-up | Pi 5 | P1 |
| 4 | iPad bring-up | Mac + iPad | P1 |
| 5 | Features: adaptive bitrate, client cursor, Linux pointer lock, HDR, AV1 | partly | P2 |

Do 2.1 → 2.2 → 2.3 first. Those protect the user and give a regression net for everything after.

---

### 2.1 P0: Lock down the host web API

**Problem.** `crates/host/src/web.rs` serves a JSON API on `127.0.0.1:47990` and checks
neither `Host` nor `Origin`. Through DNS rebinding, a web page can make the browser treat
`127.0.0.1` as the attacker's own origin. Then it can:
- `PUT /api/settings` with an app whose `command` is arbitrary shell; that command runs through
  `sh -c` when any paired client launches the app (`server.rs`, `start_session`);
- `POST /api/pair` to approve a pending attacker pairing;
- `DELETE /api/clients/*`.

**Implement:**
1. An axum middleware that rejects (403) any request whose `Host` isn't exactly `127.0.0.1:47990`,
   `localhost:47990` or `[::1]:47990`.
2. For non-GET methods, require `Origin` to be absent or one of those same origins. Also require a
   per-process random token, generated at startup, served inside `index.html` (templated in) and
   sent back as `X-Streetlight-Token`. Update `web/host/index.html` to send it, and have the
   `streetlight-host pair` CLI read the token from a `0600` file at `$XDG_RUNTIME_DIR/streetlight/token`.
3. Defence in depth: app `command` changes over the API should need an extra confirmation
   (`"confirm": true`), and every command change gets logged at `warn`.

**Acceptance tests** (add as Rust integration tests that start the axum router in-process, on an ephemeral port):
- `GET /api/status` with `Host: evil.example:47990` → 403.
- `PUT /api/settings` with the right Host but no token → 403; with the token → 204.
- `POST /api/pair` with `Origin: http://evil.example` → 403.
- Manual: the web UI still works in the browser pane; `streetlight-host pair 1234` still works.

### 2.2 P0: Loss and jitter harness; verify recovery

No real loss has been tested yet. `tc netem` needs root, so build loss injection into the code:

1. In `client-core` `MediaReceiver`, add env-controlled impairment for tests only:
   `SL_TEST_DROP=0.05` (independent loss), `SL_TEST_BURST=20:0.01` (burst of N packets
   with probability p per packet), `SL_TEST_REORDER=0.01`. Apply these before `Reassembler::push`.
   Document that it's for tests only.
2. Extend the headless `--dump` mode's stats output (or add `--report json`) so every second it
   prints frames, lost, recovered, IDR requests and keyframes seen.
3. Count IDR requests and keyframes sent on the host; log them with the 5-second media stats.

**Test matrix** (1080p60, 30 Mbit/s, mpv test pattern running, 15 s each):

| Impairment | FEC | Expect |
|---|---|---|
| none | 20 % | 0 lost, 0 recovered |
| 2 % random | 20 % | lost ≈ 0, recovered > 0, 0 decode errors in the dump |
| 10 % random | 20 % | some lost frames; each loss is followed by a keyframe within ≤ 2 frame intervals plus RTT; dump decodes with 0 RPS errors after the first keyframe following each loss |
| burst 20 pkts @1 % | 20 % | recovery or a clean IDR; never a frozen stream lasting > 500 ms |
| 1 % reorder | 20 % | no spurious `Lost` events (if there are, make the reassembler tolerate small reordering, e.g. wait until frame N+2 completes before declaring N lost, or a 2 ms deadline) |

Also check: a lost **audio** packet gives a short gap, not a stall; the Opus decoder's PLC
(`decode_float(&[], …, true)`) should be used for a missing sequence number. Implement that.

**Pass:** a table like the one above, filled with measured numbers, in the PR description and in `docs/TESTING.md`.

### 2.3 P1: Repeatable end-to-end script

Create `scripts/e2e.sh`. It wraps everything in §0 and the procedures that worked in this session:

- builds `--release --bins --examples` (webview feature on if `pkg-config webkitgtk-6.0` succeeds);
- makes a temp `XDG_CONFIG_HOME`; starts the host with `timeout`; pairs automatically
  (parse `Pairing PIN: (\d+)` from the client log and call `streetlight-host pair`);
- **video:** `--dump` for 8 s while mpv plays a pattern on the virtual output; asserts ≥ 55 fps
  average, 0 lost and 0 decode errors;
- **input:** `--selftest --clicks` + `input_probe streetlight` with `SL_SELFTEST_GATE`; then
  asserts on the probe log:
  - `ENTER 480 270`, `MOTION 960 540`, `MOTION 1440 810` (absolute mapping on a 1920×1080 output);
  - relative moves +10/+10/+10/+20 exactly;
  - `KEY down evdev=54/97/30` with matching ups;
  - two `SCROLL` lines, +1 then −1;
  - `BUTTON 8` then `BUTTON 1`, both at `960 540`;
  - `TOUCH begin 576 324` and `TOUCH end Δ 192 108`;
- **gamepad:** python-evdev watcher (see the session's `padwatch.py`, reproduce it under `scripts/`)
  asserts `BTN_SOUTH` down/up and `ABS_X`, then the device is removed;
- **audio:** 440 Hz tone at volume 0.08; asserts the self-test reports 430–450 Hz and a level
  between −40 and −28 dBFS;
- **webview UI:** the `SL_UI_EVAL` script connects and starts "Desktop"; asserts via `/api/status`
  that `streaming: true` and that client feedback shows `frames_lost == 0`;
- prints a pass/fail summary; exits non-zero on failure; kills everything by PID in a `trap`.

The script must refuse to run if `XDG_SESSION_TYPE != wayland` or if KWin's `zkde_screencast`
is missing (check with `wayland-info`).

### 2.4 P1: Correctness fixes found in review

Fix each one with a test or a logged verification:

1. **Concurrent pairing.** A second `PairStart` overwrites `AppState.pending`, silently failing the
   first. Reject the second with `HostMsg::Error("pairing in progress")`. Also rate-limit
   `PairStart` per IP (e.g. 1 per 5 s).
2. **Session preemption.** Any paired client's `Start` kills another client's stream
   (`stop_session(None)`). Make it explicit: either refuse with "host busy" or notify the evicted
   client with `StreamStopped { reason: "taken over by <name>" }`. The second option is preferred.
3. **`Feedback.last_frame`** is filled with the frame *count*. Rename the field or send the real
   last frame number. Add `idr_requests`.
4. **The `present_ms` stat** in the Linux client is measured from receive to texture creation, so it
   includes decode. Split it into `decode_ms` and `queue_to_present_ms`. Get the real present time from
   `GdkFrameClock` (`frame_clock.frame_time()` / `get_timings` of the frame that showed the texture).
5. **Hardcoded render nodes.** The client's VA-API uses `/dev/dri/renderD128` and the Pi DRM path uses
   `/dev/dri/card0`; the host default is `renderD128`. Enumerate `/dev/dri/by-path`, pick the
   node whose driver matches (host: whichever GPU KWin renders on, found by DMA-BUF import
   success; Pi: the `vc4`/`v3d` card that supports KMS), and make it overridable.
6. **Virtual output name collisions.** It's derived from the client hostname, and on iPadOS 16+
   `UIDevice.current.name` is just "iPad". Use the client certificate fingerprint prefix plus the
   display name.
7. **Repeated `pairPin` listeners.** `web/client/index.html` registers a new `pairPin` listener on
   every pairing attempt. Support `off()` or a one-shot listener.
8. **Stream start blocks the GTK main thread.** `bridge.rs` `start_stream` blocks while the host
   creates the output (~0.5 s). Move the network part to a worker thread, keeping the `ThreadGuard`
   closure on the main thread.
9. **`capture_us` is the host time at the encoder minus encode duration**, an approximation. Carry
   the PipeWire buffer timestamp (`spa_meta_header.pts`, or `pw_stream` time) through `EncodedFrame`
   so latency starts at the real compositor commit time.
10. **Text input.** `InputEvent::Text` is unimplemented on the host. Implement Unicode entry via
    a temporary keymap or IBus/`wtype`-style approach, or drop the variant and document that.
11. **cpal fixed buffer of 240 frames** may fail on some devices. The fallback path exists, but log
    which one was chosen; on the Pi, check underruns.

### 2.5 P1: Latency instrumentation and virtual-display refresh rate

1. **Refresh rate.** Measure what KWin actually delivers for a virtual output when 120/144 fps is
   requested. Use the mpv pattern at 144 fps and the host's media fps log. If it's capped at 60:
   - investigate `kde_output_management_v2` (KWin advertises v22) to set a custom mode or refresh
     on the virtual output after creation, or `kscreen-doctor output.<name>.mode.<WxH@R>`;
   - acceptance: `media: N fps` ≥ 0.95 × requested for 120 and 144 with a 144 fps source.
2. **End-to-end latency report.** Add a `--latency` option to the client that combines host capture
   time (fixed per item 9 above), clock offset from pings, frame complete, decode done and present
   time. It prints p50/p95/p99 of each stage every 5 s. Target on loopback 1080p60:
   capture→complete ≤ 5 ms p95, decode ≤ 3 ms p95.
3. **Encoder tuning.** Try the VA `quality` level (radeonsi speed vs quality presets) and
   QVBR. Measure encode ms and VMAF against a lossless capture of the same test-pattern scene:
   - record with the CPU encoder at high bitrate as the reference, or compare against `testsrc2`
     directly;
   - use `ffmpeg -lavfi libvmaf` (FFmpeg here is built with `--enable-libvmaf`).
   Pick defaults that keep encode ≤ 4 ms at 1440p60 and maximise VMAF. Record the numbers in
   `docs/TESTING.md`.

---

## 3. Raspberry Pi 5 bring-up (needs the Pi)

Ask the user for: the Pi's OS (`cat /etc/os-release`; Trixie is expected), whether it runs a
desktop (labwc) or Lite, its IP, SSH permission, and whether it's on wired Ethernet.

1. **Build on the Pi** (README has the apt list). Verified native release/webview build
   against Raspberry Pi's patched FFmpeg 7.1.5 with `ffmpeg-sys-next` 9. The bindings are
   generated against system headers; the crate major does not require that FFmpeg major.
   Keep the request-decoder/SAND patches; install CMake/Ninja for bundled Opus and use `-j 1`
   on the 4 GB Pi. Source and detailed evidence: [docs/PI.md](docs/PI.md).
2. **Decoder bring-up.** Run `streetlight-client <host> --windowed --no-input` and check the log for
   `decoder: hevc via Drm`. If `avcodec_open2` fails, check `ffmpeg -hwaccels` on the Pi and the
   needed `-hwaccel drm` device (on the Pi 5 the KMS card may be `card1`; see §2.4 item 5).
   Acceptance: 60 fps decoded at 1080p and 4K, `decode_ms` p95 ≤ 8 ms.
3. **Presentation.** If the log shows `dmabuf texture import failed`, GTK/Mesa can't import
   `DRM_FORMAT_MOD_BROADCOM_SAND128`. Options, in order:
   a. check whether the Pi's FFmpeg can output NV12 linear (`-hwaccel_output_format`) or whether
      the kernel decoder supports a linear NV12 capture format; accept the bandwidth cost if so;
   b. implement **KMS direct mode**: `crates/client-linux/src/kms.rs`, which opens the KMS card,
      finds an overlay plane supporting NV12 + SAND128, and atomically commits each decoded
      framebuffer (`drmModeAddFB2WithModifiers`). Use this when there's no compositor (Pi OS Lite
      or a VT), and make the UI either WPE WebKit on a plane above, or a "UI on the desktop,
      stream on KMS after a VT switch" mode. Document the trade-off.
   Acceptance: a visible, correctly coloured picture (have the user photograph the TV or run
   `kmsprint`), and `GraphicsOffload` confirmed with `GDK_DEBUG=offload` showing the subsurface is
   used.
4. **Input on the Pi.** USB keyboard/mouse via GTK in pointer-abs mode works. Add relative-mouse
   capture (see §5.3), and test gamepads through gilrs with an Xbox and a DualSense controller.
5. **Audio on the Pi.** HDMI audio through PipeWire; measure underruns over 10 minutes.
6. **Performance soak.** 30-minute stream at 4K60 HEVC 80 Mbit/s over Ethernet. Record CPU
   (expect < 30 % of one core), temperature, frames lost and decode p95. Run
   `vcgencmd measure_temp` every minute.
7. **Packaging.** A `.desktop` entry and a systemd user unit so the Pi boots into the client's UI
   (kiosk). Document it in the README.

Hand the user a numbered checklist for every step you can't run, with the exact commands and
which output to paste back.

## 4. iPad bring-up (needs the Mac + iPad)

1. **First build on the Mac:** `clients/ios/build-rust.sh`, then `xcodegen`, then build. Expect
   compile errors: the Swift code was written without a compiler. Likely spots:
   - `VideoRenderer.swift` `withPointers` generic closure types;
   - `Input.swift` `GCControllerButtonValueChangedHandler` closure factory;
   - `AudioPlayer.swift` `AVAudioCompressedBuffer` packet descriptions;
   - `project.yml` `info.properties` vs `INFOPLIST_FILE` duplication. Let XcodeGen generate the
     plist, and remove `INFOPLIST_FILE` if it conflicts.
   Also link-time issues for the Rust static lib: you may need `-framework Security`,
   `-framework SystemConfiguration` or `-lresolv`; check `nm` for undefined symbols.
   Acceptance: builds for a device and the simulator with no warnings in Streetlight sources.
2. **Simulator smoke test:** the UI loads, discovery runs (the simulator shares the Mac's network),
   connecting to the host works, and pairing works. Video won't decode in the simulator reliably;
   that's fine.
3. **On-device tests**, using the same matrix as the Linux client:
   - native 2732×2048 at 2× scale: the host log shows the virtual output's logical size 1366×1024;
   - 120 Hz ProMotion: `CADisplayLink` reports 120 and stats show ~120 fps (depends on §2.5 item 1);
   - Wi-Fi: default FEC 20 % and pacing 600 Mbit/s; run the §2.2 matrix with real Wi-Fi and record loss;
   - touch modes, a hardware keyboard (Magic Keyboard), mouse pointer lock and relative motion,
     an Xbox/PS controller, audio sync.
4. **Latency on iPad.** `AVSampleBufferDisplayLayer` hides decode timing. Add an optional
   `VTDecompressionSession` path that renders to `CAMetalLayer` and measures decode and
   present precisely. Compare its latency with the display-layer path, then keep the better default.
5. **AV1 on M-series / A17 Pro iPads:** build the `av1C` atom from the sequence header OBU for
   `CMVideoFormatDescriptionCreate`, gated on `VTIsHardwareDecodeSupported(kCMVideoCodecType_AV1)`.

## 5. Features (P2)

1. **Adaptive bitrate.** The host uses `Feedback` (loss, recovered, `idr_requests`, plus a new RTT
   trend) to step the bitrate down 15 % on sustained loss and up 5 % per 5 s when clean, bounded by
   the client's request. Needs encoder reconfiguration without a full restart: try changing
   `bit_rate` via `AVCodecContext` and setting `force_idr`; if VA-API ignores it, reopen only the
   encoder, keeping the PipeWire stream. Test with the §2.2 harness at varying `SL_TEST_DROP`.
2. **Client-side cursor.** Use `CursorMode::Metadata`: read `spa_meta_cursor` (position + bitmap)
   in `capture/pipewire.rs` and send it on a new QUIC datagram type. Draw it in the clients (a GTK
   overlay widget; a `CALayer` on iPad) so the pointer moves at display rate without waiting for
   encode.
3. **Linux pointer lock / relative mouse.** GTK4 has no pointer-lock API. Implement it with
   `zwp_pointer_constraints_v1` and `zwp_relative_pointer_v1` on the GTK window's `wl_surface`
   (`gdk4-wayland`), toggled by Ctrl+Alt+Shift+M and automatically while a game has grabbed the
   host cursor. Fall back to absolute.
4. **HDR10.** Capture 10-bit (`xRGB_210LE`) with KWin's HDR output, VPP to P010 BT.2020, encode
   HEVC Main10 with mastering metadata SEI. Signal it in `StreamInfo` and present via the PQ colour
   state on GTK (`GdkColorState` CICP 9/16/9) and on iPad (EDR layer). Gate it on client caps.
5. **AV1 on the host** is already selectable (`av1_vaapi`). Add the decode paths and a codec
   negotiation rule: choose the best codec both sides support.
6. **Multiple apps and app lifecycle.** Track the launched process; stop it or leave it on stream
   end (per-app setting). Add a "Steam Big Picture on the virtual display" recipe: KWin window rules
   to move the window to the virtual output.

---

## 6. Definition of done for each change

- `cargo build --release --workspace --bins --examples` and `cargo test --workspace` pass;
  `cargo clippy --workspace` adds no new warnings in touched files.
- `scripts/e2e.sh` passes (once §2.3 exists) on this machine.
- Any new behaviour has a test, or a scripted verification in `scripts/`, with measured numbers
  in `docs/TESTING.md`.
- README "Status" table updated: honest ✅/❓/❌ with evidence.
- No test touched the user's real config, typed into their windows, or left processes running
  (`pgrep -a streetlight` is empty afterwards).
- Report back to the user: what changed, what was verified with which numbers, what's still
  unverified, and any hardware checklist they need to run.
