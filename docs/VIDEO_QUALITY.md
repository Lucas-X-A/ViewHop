# Video quality investigation — 2026-10-23

> Update, 2026-10-24: resolution selection, adaptive bitrate, and resolution
> renegotiation were implemented after this investigation. See
> [MIRACAST.md](MIRACAST.md) for current behavior and limits. The results below
> remain a record of the original experiment.

## Result

The user confirmed video transmission to the TV and reported blurry text/images.
The main improvement to try is preserving native resolution: negotiate
**1920×1080 at 30 fps**, when supported by the TV and encoder, with an initial
bitrate of **8–12 Mbit/s**. Increasing only the bitrate of a 720p stream cannot
restore details removed by downscaling.

This investigation did not change the app pipeline. Experiments used local
synthetic video without capturing the user's screen, connecting to the TV, or
changing the firewall. The configuration actually used by the TV session was
not recorded; the user did not note the UI's resolution/profile/encoder line.

## Evidence in the code and host

1. The laptop panel runs at **1920×1080 at 60.05 Hz**, confirmed with
   `xrandr --current`, querying display metadata only.
2. `core::protocol::wfd::video::negotiate_video` prefers 1280×720p30 over
   1920×1080p30 even when both are advertised by the receiver. The existing test
   with CEA bitmap `000000a0` confirms this preference. Detail is therefore lost
   whenever this path selects 720p for Full HD capture.
3. `media::streaming::configured_encoder` configures x264 at 4000 kbit/s and
   OpenH264 at 4,000,000 bit/s. The value does not scale with resolution.
   OpenH264 uses the default `rate-control=quality`, not a strict traffic limit.
4. The pipeline uses `videoscale` with its default bilinear method. Reducing
   Full HD to 720p and upscaling at the receiver harms letters and fine edges.
   Increasing bitrate after downscaling does not undo that loss.
5. Encoder selection prioritizes hardware, then OpenH264 and x264. An accepted
   OpenH264 Baseline candidate can precede x264 High in the profile list. This
   preference is not an encoder-quality comparison.
6. The probe validates three frames at 1280×720p30/level 3.1. This does not prove
   the same encoder can sustain 1080p30/level 4. Before changing mode preference,
   the full intended format must also be validated.

Responsible files:

- [Resolution/profile/level negotiation](../crates/core/src/protocol/wfd/video.rs).
- [Bitrate, parameters, probes, and scaling](../crates/media/src/streaming.rs).
- [Encoder order](../crates/media/src/encoder.rs).
- [Session composition](../crates/app/src/mirroring.rs).
- [UI mode and encoder line](../crates/app/src/ui.rs).

## Local experiment

Environment: Core i7-8565U, Linux Mint, GStreamer 1.24.2. The test created a
90-frame I420 Full HD reference at 30 fps using `videotestsrc pattern=smpte` with
`Sans 16` text overlaid. Every case encoded the same frames. The benchmark
included no X11/PipeWire capture, RTP transport, Wi-Fi, or TV processing.

x264 used the app's options: zerolatency, no B-frames, a keyframe every 30 frames,
byte-stream, AUD, and High. Level 3.1 was enforced for 720p and level 4 for 1080p.
OpenH264 used the app's current parameters and Constrained Baseline. After
encoding, another pipeline decoded the files to I420 Full HD. All 90 frames
were confirmed in each case.

Image metrics use luma PSNR with fixed samples and a separate text region.
Higher values mean less error for this synthetic pattern. They are not TV
perceptual-quality scores. The 720p comparison includes upscaling to Full HD,
since the reference matches the panel's native resolution.

| Configuration | Measured H.264 bitrate | Text PSNR-Y | Encoding/scaling speed |
| --- | ---: | ---: | ---: |
| 720p, OpenH264, 4 Mbit/s target | 4.09 Mbit/s | 24.16 dB | 84 fps |
| 720p, x264 veryfast, 4 Mbit/s target | 4.32 Mbit/s | 24.00 dB | 82 fps |
| 720p, x264 veryfast, 8 Mbit/s target | 8.19 Mbit/s | 24.24 dB | 72 fps |
| 1080p, x264 veryfast, 8 Mbit/s target | 8.64 Mbit/s | 35.07 dB | 49 fps |
| 1080p, x264 veryfast, 12 Mbit/s target | 13.02 Mbit/s | 39.94 dB | 51 fps |
| 1080p, x264 fast, 12 Mbit/s target | 13.05 Mbit/s | 39.99 dB | 23 fps |

Doubling the 720p bitrate barely changed text-region error. Preserving resolution
substantially improved that metric. At 1080p, 12 Mbit/s also improved the pattern
compared with 8 Mbit/s. This supports prioritizing resolution and then adjusting
compression, rather than compensating for 720p with bitrate alone.

The `fast` preset cost more than twice the CPU and fell below the intended 30 fps,
with minimal text-region improvement in this test. It is not the first recommended
improvement for this laptop. `veryfast` was a more viable baseline.

Timings include reference-file reading and scaling; they are not end-to-end
latency. Each case was measured once, without statistical guarantees of speed.
Real-capture conversion, concurrent load, and networking can reduce the observed
headroom. Actual traffic also includes MPEG-TS/RTP/UDP and can exceed the configured
bitrate; the three-second files already show variation from the target.

Local investigation artifacts: `/tmp/wireless-display-quality/compare.py`,
`results.json`, and synthetic H.264 files. Raw reference/decoded files were removed
by the experiment. No dependencies were installed.

## Proposed improvements, in order

1. **Preserve Full HD when commonly supported.** Prefer 1080p30 for a Full HD
   source, retaining 720p30 fallback. Respect receiver bitmaps, profile, level,
   and limits; do not assume this TV supports 1080p without its actual capabilities.
2. **Validate the encoder in the negotiated mode.** Run preflight before P2P
   with the full resolution, FPS, profile, and level. Passing only at 720p must
   not by itself justify 1080p negotiation.
3. **Match bitrate to resolution.** Start real Full HD comparisons at 8 Mbit/s
   and 12 Mbit/s with the same source/content. Choose the higher value that
   preserves stability and acceptable latency. These are test candidates, not
   measurements of Wi-Fi link capacity.
4. **Keep low latency without prematurely increasing preset cost.** Retain
   `veryfast` for the first x264 experiment. Measurements did not favor switching
   straight to `fast` on this hardware.
5. **Evaluate hardware and per-encoder parameters.** The host has Intel UHD 620
   and NVIDIA MX130. The legacy VA-API H.264 plugin appears in the registry outside
   the sandbox, but `vainfo --display drm --device /dev/dri/renderD128` failed to
   initialize. This proves neither working acceleration nor its absence on the
   app's X11 path. Validate with actual encoding before recommending driver
   changes. Hardware can free CPU; it does not guarantee sharper images at the same bitrate.
6. **Review discarded capture work if performance is insufficient.** Currently,
   conversion/scaling precedes videorate. Investigate reducing FPS before these
   operations when converting a 60 Hz source to 30 fps. Use frame and QoS
   measurements; the current RTP counter counts packets, not video FPS.

For sources that still need downscaling, comparing a scaling filter better
suited to fine edges is a secondary improvement. I420/H.264 4:2:0 output also
limits color detail in small text; increasing bitrate is not equivalent to
providing 4:4:4 chroma. Do not switch to a format unsupported by WFD negotiation.

On the receiver, check for scaling without overscan and 1:1 mapping, if available
in Screen Share mode. This avoids additional TV upscaling. The TV menu and its
panel's native resolution were not inspected.

## Next-test criteria

Compare the current configuration against 1080p30 at 8 and 12 Mbit/s using the
same text, reading distance, and a moving/scrolling segment. Record mode, encoder,
sharpness, stability, CPU load, dropped frames, and latency. Verify that each
configuration still shuts down correctly through both the app and the TV.

Selection remains in core, GStreamer remains in media, and the app coordinates
the session through messages. Improving quality does not require changing the
GTK/Tokio separation.

## Smoothness and 60 fps — 2026-10-24

The user confirmed stable 720p video, reported perceived 1080p playback around
15–20 fps, and supplied a log showing fallback to `Hd` at 4000 kbit/s. The encoder
was not noted; the old log lacked measured FPS and fallback causes. The
`peer-not-found` error belonged to a separate discovery/connection attempt.

Five-second synthetic test: BGRx desktop at 1920×1080/60 Hz, smpte pattern,
rate reduction before the bounded/leaky queue, I420 conversion, scaling, H.264,
and fakesink. No personal screen, TV, or network interface participated. Results
are local FPS, not TV display rates; each case was measured once.

| Configuration | Encoded FPS | Local drops |
| --- | ---: | ---: |
| Current OpenH264, screen/bitrate, Full HD/30 | 22.87 | 34 |
| OpenH264 low/4 threads, Full HD/30 | 20.39 | 48 |
| x264 veryfast, Full HD/30 | 29.41 | 4 |
| x264 superfast, Full HD/30 | 30.26 | 0 |
| Current OpenH264, 720p/60 | 33.43 | 133 |
| x264 superfast, 720p/60 | 50.97 | 46 |

A second round, using two conversion/scaling threads and explicit profile/level
caps, achieved 30.11 fps without drops for Full HD/30 in x264 superfast High,
and 60.24 fps without drops for 720p/60 in x264 ultrafast High. Baseline ultrafast
ranged from 50.21 to 54.18 fps for this pattern, reinforcing the need for preflight
and actual statistics instead of promising smoothness from plugin presence.
Test pattern/order, load, and temperature can affect the numbers.

Changes: hardware remains first, x264 moves ahead of OpenH264; superfast/ultrafast
presets for 30/60 fps; two conversion/scaling threads; CEA 720p60 offer with level
3.2 and initial 720p30 fallback; p60 preflight with 30 frames and at least 55 fps
at the actual capture size; H.264 FPS/drops in the GUI and fallback causes in logs.
There is no automatic FPS renegotiation at this stage. Full HD resolution fallback
remains 720p30.

On an isolated Xvfb display, `ximagesrc` was verified to negotiate actual 30/60 fps
capture as requested. The p60 path does not duplicate frames and accepts 59.94 Hz.
Sources without 60/59.94 Hz caps are limited to 30 fps before connection.
Temporary artifacts: `/tmp/wireless-display-fps/{measure.py,before.json,
candidates.py,candidates.json}`. The new version still needs a physical retest.

## 720p/60 sharpness after the TV retest — 2026-10-24

The user confirmed improved Full HD and described 720p/60 degradation as blurry
text/blocks. The log identified x264, Hd60, and an initial 6000 kbit/s target.
Recovered log windows show input close to 60 fps, encoding around 38–57.5 fps,
and up to 44 local drops per window. This proves pipeline pressure; it does not
alone separate CPU pressure from output/network backpressure. The 1 MiB/s traffic
graph corresponds to about 8.4 Mbit/s, not maximum bandwidth.

The previous mode combined ultrafast with only 100 kbit per nominal frame,
compared with 133 kbit for 720p/30 at 4000 kbit/s. The revision increases p60's
initial target to 8000 kbit/s, adaptable to 12000, uses superfast and up to four
frame threads without B-frames/lookahead, and scales RGB before converting to I420.
720p/30 and Full HD/30 retain their parameters. Frame parallelism can add about
50 ms of encoding delay.

Synthetic experiment: BGRx Full HD/60 desktop with smpte pattern and Sans 24 text,
two-frame leaky queue, encoding, Annex B file, and real decoding. PSNR-Y measures
a fixed text region against an I420 reference before compression; higher values
mean less error for this pattern, not a TV perceptual-quality score. Each row
was measured once in the final round; FPS varied significantly between rounds,
so sustained 60 fps on this host is not promised.

| Configuration | Encoded FPS | Drops | Text PSNR-Y |
| --- | ---: | ---: | ---: |
| 720p/30, superfast, 4 Mbit/s (existing reference) | 30.36 | 0 | 32.96 dB |
| Previous 720p/60, ultrafast, 6 Mbit/s | 40.40 | 100 | 31.94 dB |
| Revised 720p/60, superfast, 8 Mbit/s, scaling first, four threads | 48.65 | 42 | 33.31 dB |

Combined conversion/scaling and explicit AQ did not provide enough headroom in
these experiments and were not incorporated. p60 preflight expanded from about
half a second to at least 120 frames, requiring 57 fps. It can select 720p/30 at
initial negotiation, preserving sharpness/stability when headroom is insufficient;
FPS does not change mid-session without appropriate negotiation.

The app's immediate VA-API rejection comes from the bitrate property's missing
PLAYING-mutability flag. The error now describes this reason rather than just
NoEncoder. A separate synthetic VA-API High/level 3.2 test outside the sandbox
also failed with not-negotiated. This does not prove a driver defect in every
format. No driver was installed or changed.

DEBUG frame-rate logs now include RTCP loss percentage when a fresh report is
available, to distinguish future blocking/blur cases. `None` does not mean zero
loss. Tests did not capture personal content or connect to the TV.
Local artifacts: `/tmp/wireless-display-720-quality/quality-comparison.json`,
`results.json`, `thread-results.json`, `final-results.json`, and the corresponding
synthetic scripts/patterns. The new behavior still needs a physical retest.

## Full HD sharpness and receiver stutters — 2026-11-03

The first supplied screenshot/log showed 1280×720 at 30 fps and 4 Mbit/s, with
only Hd/Vga offers. It did not record the requested preference or capture size,
so the reason Full HD was absent cannot be established retrospectively.
Added diagnostics now record the selected preference, capture geometry, tested
offer, and RTCP negotiation without recording screen content or RTSP bodies.

The user's next attempt confirmed FullHd selection, 1920×1080 capture, an x264
Full HD offer, and negotiation at 30 fps with an 8 Mbit/s target and RTCP enabled.
The supplied measurement windows showed approximately 30 input/encoded fps,
zero local drops, and no fresh RTCP reports. The user confirmed good sharpness
but reported stutters on the TV. This evidence shifts the remaining investigation
to transport/receiver behavior; it does not measure the TV's displayed FPS.

A read-only socket/kernel-journal observer correlated a UFW UDP block with the
app's RTCP socket on the P2P interface. The TV's source port differed from the
negotiated receive port. No packet payload was captured, so this observation
alone does not prove that the blocked datagram contained a valid Receiver Report.
The app also connected its UDP socket to the advertised port, which would discard
feedback from another source port even if UFW allowed it. RTSP Transport ports
describe receiving ports, not mandatory feedback source ports.

The media fix leaves the RTCP socket unconnected and checks udpsrc sender metadata
against the RTSP peer's IP before telemetry and rtpbin. Outgoing RTCP still goes
to the negotiated port. Missing metadata and other IPs are rejected; compound
packet bounds, SSRC validation, and fresh-sequence checks remain. Safe bindings
from gstreamer-net expose the metadata without project-owned unsafe code.

The loopback regression timed out before the fix when a valid report came from
a separate sending port. After the fix, all ten media unit tests passed, including
that exchange and rejection of another source IP. Metadata tests also reject
missing addresses and the wrong address family. No queues, Rust video-frame copies,
encoder settings, or adaptive bitrate thresholds were added or changed. RAM use
was not benchmarked comparatively.
The default workspace suite passed 53 tests, with ten environment-dependent tests
ignored. Workspace check, Clippy with warnings denied, formatting, and the native
app build passed. Flatpak sources match the lockfile; only the two new binding
crates were added, with no existing package version changes.

UFW remains unchanged. A physical retest with a narrowly allowed RTCP return path
must verify valid reports and whether the TV's stutters improve. Missing feedback
keeps the current controller conservative at 8 Mbit/s; it does not prove that
raising bitrate will improve smoothness or that this fix alone resolves stutters.

## Encoder-specific inputs and latency settings — 2026-11-04

The previous pipeline forced I420 for every encoder, although modern VA/NV/QSV
encoders can require NV12. Media now selects a supported 8-bit 4:2:0 input from
the encoder's sink caps, preferring NV12 for hardware and retaining I420 for the
software encoders. Preflight validates that selection, including live resolution
fallback/recovery. The selected input is retained for subsequent resolution changes.

Encoder configuration and bitrate units/mutability now belong to the encoder
module. VA, legacy VA-API, NVENC, and QSV use their own GOP, no-B-frame, rate-control,
and low-latency controls. QSV's CABAC enum is handled explicitly. Unsupported
required settings reject the candidate rather than silently doing nothing;
software alternatives remain available. The adaptive bitrate policy is unchanged.

INFO diagnostics log negotiated input format, geometry, frame rate, and caps
memory features at capture and encoder input. They do not log frame contents.
Conversion/scaling still uses system memory; DMA-BUF/GPU processing is a separate
step. No raw frame queues, receive threads, or Rust frame copies were added.
Comparative RAM use, GPU performance, and physical TV smoothness were not measured.

The default workspace suite passed 56 tests, with ten environment-dependent tests
ignored. New coverage includes NV12-only input selection, software preference,
invalid controls, rejection of GPU-only caps, and software fallback after an
unusable candidate. A real x264/NV12 pipeline encoded, transported, and decoded
video while changing bitrate to 16 Mbit/s and switching 1080p/720p/1080p with
the same pixel format, exact PES timing, and cancellable frame gating.

## Optional GPU processing and capture import — 2026-11-04

Media now offers optional VA/VAMemory, legacy VA-API/VASurface, and CUDA/CUDAMemory
conversion/scaling paths matched to the encoder's caps. CPU conversion/scaling
remains available, including hardware encoding from system memory and complete
x264/OpenH264 alternatives. Selection does not depend on CPU vendor or model.
The same tested path and memory contract remain in use during resolution changes.

Synthetic preflight now uses the streaming pacing/queue stages, validates GPU
input caps, exercises the Full HD target at 8 and 16 Mbit/s, and tests resolution
fallback/recovery. Before P2P, app coordination also validates an encoded frame
from the already authorized capture. PipeWire tries explicit DMA-BUF import,
then system-memory capture/upload. Each attempt renews the restricted remote in
the same portal session. Rejected encoder/path pairs are excluded only during
that preparation; other GPU paths and every CPU path remain eligible. A sparse
static portal stream needs only one encoded frame for import validation, while
cadence is checked by the synthetic test and runtime telemetry.

Default tests cover plugin-dependent selection, memory-feature isolation, DRM
modifier preservation, bounded queues, cancellation, and CPU operation after
exclusions, alongside the existing real encode/transport/decode tests. The
workspace suite passed 60 tests; eleven environment-dependent tests were ignored,
including the new synthetic GPU test. Formatting, workspace check, Clippy with
warnings denied, and the native binary build passed.

GPU devices are visible outside the sandbox, but usable VA/CUDA postprocessor
factories were not exposed by the installed plugins in this environment. No
physical GPU/DMA-BUF success, battery savings, or RAM reduction is claimed.
No screen capture or TV connection was initiated by these tests. No driver,
firewall, scheduler, or CPU-specific compiler setting was changed. Optional
hardware validation and power/memory measurements remain manual target-host work.
