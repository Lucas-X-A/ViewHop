# Implementation report — 2026-10-17

Historical application messages quoted below are translated into English for
readability. Dates, measurements, error codes, and command arguments retain their
original meaning; these translations are not new observations.

## Implemented

Rust Edition 2024 workspace with four crates, GTK4, a separate Tokio backend,
typed commands/events, connection states, cancellation, and cooperative shutdown.
Includes complete dummy mode, actual system diagnostics, signal-based
NetworkManager discovery, WFD classification, initial P2P activation, ScreenCast
Portal with a restricted FD, PipeWire/GStreamer, X11 fallback, an RTSP
parser/serializer/transport, and the WFD foundation.

A Flatpak manifest, 146 pinned Cargo sources for offline builds, icon, desktop
entry, AppStream metadata, MIT license, documentation, and CI workflow were added.
Miracast transmission and audio are not implemented at this milestone.

## Architecture

GTK and widgets remain on the main thread/GLib. A separate bootstrap thread
creates multithreaded Tokio; Coordinator owns state and resources. The UI sends
AppCommand through mpsc and receives AppEvent through async_channel. Linux
services live in platform; GStreamer processes video on its own threads without
per-frame copies into Rust. Jobs have cancellation/ownership; generations discard
stale results. SIGINT/SIGTERM also use cooperative shutdown.

The portal takes priority. Wayland, including GTK under XWayland, never authorizes
X11 fallback. Portal cancellation/denial does not start another capture. Sparse
sources are allowed; a 15-second timeout bounds the wait for the first frame.

## Main files

| Component | File |
| --- | --- |
| Coordinator and runtime | [coordinator.rs](../crates/app/src/coordinator.rs) |
| State machine | [state.rs](../crates/core/src/state.rs) |
| GTK | [ui.rs](../crates/app/src/ui.rs), [main.rs](../crates/app/src/main.rs) |
| Diagnostics | [diagnostics.rs](../crates/app/src/diagnostics.rs) |
| NetworkManager / P2P | [network.rs](../crates/platform/src/network.rs) |
| Portal / X11 | [capture.rs](../crates/platform/src/capture.rs) |
| Environment detection | [environment.rs](../crates/platform/src/environment.rs) |
| GStreamer pipeline | [pipeline.rs](../crates/media/src/pipeline.rs) |
| Encoders | [encoder.rs](../crates/media/src/encoder.rs) |
| RTSP / WFD | [protocol/wfd](../crates/core/src/protocol/wfd/) |
| Flatpak | [org.example.WirelessDisplay.yml](../flatpak/org.example.WirelessDisplay.yml) |

## Tests run

Environment: Linux Mint 22.3/Cinnamon/X11, Rust 1.99.0, GTK 4.14.5, GLib 2.80.0,
GStreamer 1.24.2, and NetworkManager 1.46.0. Missing development libraries were
downloaded/extracted into /tmp without installing host packages. Cargo commands
used `PKG_CONFIG_PATH=/tmp/wireless-display-deps/pkgconfig`. This is environment
preparation, not a required project path. See README for normal native dependencies.

Latest gate run, all exiting with 0:

```sh
cargo fmt --all --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Result: **21 tests passed**, none failed. Four integration tests are ignored in
the default suite because they depend on a display or hardware. All were run
separately and passed:

```sh
# Xvfb :99, private bus without activatable services; UI and real capture selector
DISPLAY=:99 GDK_BACKEND=x11 GSK_RENDERER=cairo GTK_A11Y=none \
dbus-run-session --config-file=crates/app/tests/session-bus.conf -- \
cargo test -p wireless-display-app --test gtk_smoke --test x11_fallback -- --ignored --test-threads=1

# Real ximagesrc source on the same virtual display
DISPLAY=:99 cargo test -p wireless-display-media --test x11_capture -- --ignored

# Real P2P adapter: three seconds of discovery, cancellation, and StopFind
cargo test -p wireless-display-platform --test p2p_discovery -- --ignored
```

Xvfb/D-Bus sockets and the system bus required execution outside the tool sandbox;
processes still ran as the normal user. Initial attempts inside the sandbox
failed because sockets were blocked. A receive rule on the private test bus was
fixed; the final repeat passed. EGL/DRI3 warnings on the virtual display did not
prevent the UI or tests from working.

Other checks:

- `target/debug/wireless-display --version`: `WirelessDisplay 0.1.0`, exit 0.
- `target/debug/wireless-display --diagnostics`: exit 0; NM, Wi-Fi/P2P interfaces,
  plugins, and encoders detected. ScreenCast absent on this Cinnamon desktop.
- Real `--dummy` binary opened in Xvfb through a Python subprocess; SIGTERM sent
  after two seconds: exit 0 and `Cooperative shutdown complete` confirmed with
  `RUST_LOG=wireless_display=info`.
- `python3 scripts/generate-cargo-sources.py`: 146 crates; coverage checked against
  Cargo.lock. SHA-256 was verified for all 146 cached .crate archives, which were
  extracted according to the manifest into `/tmp/wireless-display-offline`.
- `cargo metadata --locked --offline --config /tmp/wireless-display-offline/cargo/config.toml --format-version 1`:
  exit 0, complete resolution through vendored sources without network access.
- `flatpak-builder --show-manifest flatpak/org.example.WirelessDisplay.yml`:
  exit 0; YAML and Cargo source list read by builder 1.4.2.
- `desktop-file-validate data/org.example.WirelessDisplay.desktop`: exit 0.
- `appstreamcli validate --no-net data/org.example.WirelessDisplay.metainfo.xml`:
  one warning for the undefined homepage. Validation with
  `--override=url-homepage-missing=info` passed with one informational message.
  This exception serves the local project with a provisional identity;
  publication requires a real URL.
- `flatpak-builder --disable-download --force-clean build-dir flatpak/org.example.WirelessDisplay.yml`:
  did not build; `org.gnome.Sdk/x86_64/51 not installed`. The SDK was not installed
  on the host, and no Flatpak package was presented as build-validated.

## Actual functionality

| Feature | Evidence and limits |
| --- | --- |
| GTK / dummy / messaging | Tested: window opens, fictional TV appears, connects, captures, disconnects, simulates an error, and shuts down |
| Real discovery | StartFind/StopFind and cancellation passed on the P2P adapter; finding a particular TV was not proven |
| Real P2P connection | Implemented through NM, temporary and bound to the D-Bus client; activation against a TV not tested |
| ScreenCast Portal | Real implementation; monitor selection and returned FD not tested because the desktop does not provide ScreenCast |
| PipeWire | Socket and pipewiresrc detected; portal-stream consumption not proven in this environment |
| X11 fallback | Tested end to end with no portal, X11 backend, and real Xvfb frames; did not access the user's personal screen |
| Wayland protection | Tests confirm fallback rejection, including a GDK X11/XWayland hint |
| Flatpak | Manifest read by builder and offline sources verified; package not built because the SDK was missing |
| Miracast to TV | Not yet implemented; no WFD M1–M7, H.264/MPEG-TS/RTP, or receiver audio |

The executable foundation was delivered; physical Portal/PipeWire validation and
Flatpak build criteria remain pending. There is no claim that an LG TV or another
receiver received video.

## Next milestone

Validate Portal/PipeWire on a desktop with ScreenCast and build Flatpak with SDK
51. Then validate P2P activation with a TV, obtain actual group IP/routes, observe
connection loss, implement WFD M1–M7 negotiation, and select receiver caps/ports.
Build low-latency H.264 over MPEG-TS/RTP, implement keepalive/teardown, and test
first video on LG webOS. Enable negotiated audio only after that proof.

## Expired discovery results fix — 2026-10-23

The user's manual test showed discovered TVs followed by
`Wi-Fi Direct activation failed (state 4)`. Host logs identified the technical
cause: `peer-not-found`.
Discovery ended at 18:57:59; wpa_supplicant removed the three peers between
18:58:19 and 18:58:59. Activation started at 18:59:02, and NetworkManager ended
the attempt at 18:59:12 because it could not find the peer again.

The prototype stopped D-Bus observers along with the 120-second discovery, but
kept results in the UI. PeerRemoved was therefore no longer observed, leaving
an expired TV displayed as available. ActiveConnection state 4 means deactivated;
it does not by itself explain the failure.

Fixes:

- Clear devices when discovery/observation ends, manually or by timeout. Preserve
  the selected device as an attempt snapshot without presenting others as available.
- Requery Peers, identity, and WFD availability before creating a temporary
  connection. Reject expired peers before activation.
- Observe Device.StateChanged before activation and query StateReason on failure.
  Official reason 67 (PEER_NOT_FOUND) guides the user to repeat discovery rather
  than reporting only state 4.
- Enable Search for devices after errors for direct recovery.

Verification: `cargo fmt --all --check`, `cargo check --workspace`,
`cargo test --workspace`, and
`cargo clippy --workspace --all-targets --all-features -- -D warnings` passed.
The default suite passed with **24 tests**, with five integrations ignored for
environment requirements. Development libraries were prepared in
`/tmp/wireless-display-dev`; commands used
`PKG_CONFIG_PATH=/tmp/wireless-display-dev/pkgconfig` without changing host packages.

Two additional tests, already compiled by the suite, passed in isolation:

```sh
DISPLAY=:98 GDK_BACKEND=x11 GSK_RENDERER=cairo GTK_A11Y=none \
dbus-run-session --config-file=crates/app/tests/session-bus.conf -- \
target/debug/deps/gtk_smoke-b4fa5c2a4042401f --ignored --test-threads=1

dbus-run-session --config-file=crates/app/tests/session-bus.conf -- \
target/debug/deps/wireless_display_platform-883a80e73edd9d3c \
network::tests::expired_discovery_result_is_rejected_on_dbus --exact --ignored
```

The first confirmed UI recovery: Search for devices is enabled after error and
a new search works. The second rejected an expired peer in a fake NetworkManager,
without the system bus or a TV connection. Equivalent Cargo commands without
executable hashes are in TESTING.md and CI.

The native binary was rebuilt. The user's TV connection needs a manual retest
with fresh discovery. No other P2P activation was started on the host, and WFD
negotiation/TV streaming remain unimplemented at this point.

## Video source 0.2.0 — 2026-10-23

This section records the 0.2 state. Earlier sections preserve historical 0.1
milestone results; their WFD/streaming limitations do not describe implementation 0.2.

### Implementation and architecture

The complete video path from P2P group to UDP transport was implemented:

- `platform::network`: obtains ActiveConnection IPv4/prefix/gateway and observes
  connection loss or configuration changes through D-Bus signals.
- `core::protocol::wfd`: RTSP source listening on P2P IPv4, M1–M7 negotiation,
  CSeq/Session/URI/Transport validation, video selection, keepalive, PAUSE/PLAY,
  IDR requests, and TEARDOWN. Core remains independent of GTK, zbus, and GStreamer.
- `media::streaming`: tests encoders, reserves RTP/RTCP sockets, and constructs
  capture, H.264, MPEG-TS, and RTP. A pair requested in SETUP enables RTCP through rtpbin.
- `app::mirroring`: owns session resources and coordinates protocol/media under
  the Coordinator's Job. Authorizes capture and tests encoders before P2P activation.
  Streaming is announced only after PLAY and actual packet transmission.
  Cancellation stops video before closing protocol, portal, and temporary connection.

GTK remains on the main thread with GLib. The backend uses a separate Tokio
runtime and typed commands/events. Local preview and dummy mode remain independent
of the real session. The UI shows negotiated mode, encoder, and RTP packet count.

Files and detailed flow: [ARCHITECTURE.md](../ARCHITECTURE.md),
[MIRACAST.md](MIRACAST.md), [mirroring.rs](../crates/app/src/mirroring.rs),
[source.rs](../crates/core/src/protocol/wfd/source.rs),
[video.rs](../crates/core/src/protocol/wfd/video.rs), and
[streaming.rs](../crates/media/src/streaming.rs).

### Verification performed

Native environment: Linux Mint 22.3/Cinnamon/X11, GStreamer 1.24.2, and GTK 4.14.5.
Development headers extracted into `/tmp/wireless-display-dev` without installing
host packages. Cargo commands used `PKG_CONFIG_PATH=/tmp/wireless-display-dev/pkgconfig`.

All commands below exited with 0:

```sh
cargo fmt --all --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo build --locked -p wireless-display-app
target/debug/wireless-display --version
```

The default suite passed with **35 tests**, none failed; five integrations are
ignored for environment requirements. The rebuilt binary reports
`WirelessDisplay 0.2.0`.

New tests include TCP M1–M7 negotiation, UDP RTP/MPEG-TS reception, keepalive,
PAUSE/PLAY, remote TEARDOWN, cancellation with local TEARDOWN, mismatched CSeq
rejection, and IP/port validation. A second GStreamer pipeline received,
depacketized, and **decoded H.264 frames**. Another test confirmed RTCP Sender
Reports. The seven app unit tests were repeated three times with four threads;
all repetitions passed.

Four additional integrations passed on dedicated Xvfb `:97` and a private session
bus without activatable services. Already compiled test binaries were executed;
the equivalent Cargo commands are:

```sh
DISPLAY=:97 GDK_BACKEND=x11 GSK_RENDERER=cairo GTK_A11Y=none \
dbus-run-session --config-file=crates/app/tests/session-bus.conf -- \
cargo test -p wireless-display-app --test gtk_smoke -- --ignored --test-threads=1

DISPLAY=:97 GDK_BACKEND=x11 GSK_RENDERER=cairo GTK_A11Y=none \
dbus-run-session --config-file=crates/app/tests/session-bus.conf -- \
cargo test -p wireless-display-app --test x11_fallback -- --ignored

dbus-run-session --config-file=crates/app/tests/session-bus.conf -- \
cargo test -p wireless-display-platform --lib expired_discovery_result_is_rejected_on_dbus -- --ignored

DISPLAY=:97 cargo test -p wireless-display-media --test x11_capture -- --ignored
```

These checks confirmed GTK/dummy/shutdown, X11 fallback without a portal, expired
peer rejection on fake D-Bus, and real ximagesrc frames on the virtual display.
They did not capture the user's screen or initiate a TV connection.
Loopback/Xvfb/D-Bus sockets required execution outside the tool sandbox, always
as the normal user. EGL/DRI3 warnings did not prevent integration tests.

The first concurrent test run aborted in native `libgstvaapi`/`libva-x11` code
during simultaneous driver initialization; a serial run passed. GPU-independent
fixtures now explicitly test OpenH264/x264 rather than initializing hardware
drivers. The full suite and three concurrent repetitions passed after this
change. This does not prove driver correctness: the app still tries hardware
before software, and hardware acceleration still needs physical validation.

OpenH264 emitted `cmInitParaError` when finalizing probes on this host. Tests
confirmed encoded buffers and decoded frames; the warning was not hidden.

Other checks passed: valid desktop entry, valid AppStream with the previously
documented informational exception for a provisional homepage, and Flatpak source
regeneration with 146 pinned crates. Flatpak **was not built**: SDK 51 remains absent.

### Actual functionality and next test

Discovery and P2P formation had physical evidence in earlier milestones and the
user's manual test. No new TV activation occurred at this stage. Transport and
decoding worked with a local synthetic receiver; **a physical TV picture has
not yet been validated at this stage**.

X11 was validated on a virtual display. ScreenCast Portal/PipeWire remain
implemented without physical proof on this Cinnamon desktop, which does not
provide ScreenCast. Real Wayland and Flatpak build/execution also remain pending.

For the next test, close the old binary, open Screen Share on the TV, start
`target/debug/wireless-display`, search again, and click Share screen. Confirm
picture, negotiated mode, and RTP packets; then test Stop/close and TV-initiated
shutdown. On failure, preserve the error and logs as described in
[TROUBLESHOOTING.md](TROUBLESHOOTING.md).

At this milestone, the version transmits only video over IPv4/UDP in three
progressive CEA modes. It does not implement audio, HDCP, UIBC, TCP media, 4K,
reconnection, or dynamic renegotiation. First validate video with the real TV;
then implement negotiated audio.

## Immediate-disconnection diagnosis — 2026-10-23

The user reported connection failure and OpenH264 `cmInitParaError` messages.
The NetworkManager journal showed five successful P2P activations followed by
DeactivateConnection requested by the application process about 10 ms later.
This alone does not identify why the app ended the session. No other connection
or personal capture was started during this investigation.

A GStreamer probe with videotestsrc, OpenH264, h264parse, and fakesink reproduced
the message with exit 0. Video tests also continued passing with this warning.
The isolated message therefore does not prove a fatal encoder failure.

A diagnostic gap was fixed: AppEvent::Error and backend warnings reached the
window but were not logged in the terminal. They are now logged alongside
encoder preparation, P2P activation, RTSP waiting, and negotiation startup stages.
Logs omit RTSP bodies and peer addresses. H.264 candidate details use debug
level. The binary was rebuilt.

IPv4 reading/monitoring also passed with a fake NetworkManager, using official
D-Bus property names and an active group without a gateway. This case was added
to integration tests and CI. Seven app tests, nine default platform tests, two
private-bus integrations, Clippy with warnings denied, and formatting passed.
Build/verification commands:

```sh
cargo fmt --all
cargo clippy --locked -p wireless-display-app -p wireless-display-platform --all-targets --all-features -- -D warnings
cargo test --locked -p wireless-display-app -p wireless-display-platform --lib --no-run
cargo build --locked -p wireless-display-app
```

Tests ran through generated binaries with local sockets allowed. Both integrations
use the `on_dbus --ignored --test-threads=1` filter on the private bus documented
in TESTING.md. No TV was used.

The physical failure's cause still depends on another attempt with the rebuilt
binary and `RUST_LOG=wireless_display=debug`. This update improves diagnostics;
it does not prove TV interoperability is fixed.

## Waiting for IPv4 publication — 2026-10-23

New logs located the failure before RTSP: P2P activation completed, followed by
AddressUnavailable. A read-only observer monitored a manual user attempt.
NetworkManager published `State=2` and a valid Ip4Config path, but querying that
object still returned `AddressData=[]` and `Gateway=""`. The app immediately
treated this as failure and requested group deactivation. The observer did not
initiate a connection or capture.

Session preparation now waits for Ip4Config/AddressData publication through D-Bus
signals. Subscriptions precede reads, without polling or an assumed IP. The total
deadline is eight seconds and CancellationToken interrupts the wait. Available
addresses still undergo existing address, prefix, gateway, and ambiguity validation.
During an established session, configuration loss/change still triggers immediate
shutdown; the initial wait is not reused.

The fixture with completed activation and empty AddressData reproduced the
previous failure: an error after about 10 ms, before address publication. After
the fix, the same fixture passed, allowing preparation when the address was
published through a signal.

Checks passed using the previously documented temporary headers:

```sh
cargo fmt --all --check
cargo test --workspace --locked
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo build --locked -p wireless-display-app
```

The default suite passed with **35 tests** and eight integrations ignored for
environment requirements. Four D-Bus tests were run separately on the private
bus and passed, including late address/path publication, wait cancellation,
address loss during monitoring, and link loss. CI already runs these tests with
`on_dbus`.

The native binary was rebuilt. A manual retest confirmed the physical fix for
premature rejection: in the 23:44:16Z and 23:44:39Z attempts, the app obtained
IPv4 in about 140 ms and opened RTSP 7236. The observer recorded populated
AddressData after activation.

The next blocker was identified in the host firewall. The kernel journal logged
`[UFW BLOCK] IN=p2p-wlp0s20-12 ... PROTO=TCP ... DPT=7236 ... SYN` at local 20:44:27,
and the same condition on p2p-wlp0s20-13 at 20:44:49. The TV attempted to open
RTSP control, but UFW dropped requests. About 15 seconds after activation,
wpa_supplicant logged GO_ENDING_SESSION. The subsequent address-change message
occurred during group teardown.

A rule restricted to the observed P2P adapter, TV IP, local group IPv4, and TCP
7236 was prepared. Addition/removal syntax passed the installed UFW parser without
calling its backend. Full simulation required root; `sudo -n` reported that a
password was required. No rule was changed. Commands are in TROUBLESHOOTING.md
and must be run by the host administrator if they choose to allow this traffic.
The TV picture remained unvalidated at this point.

## First video confirmed by the user — 2026-10-23

After UFW configuration, the user confirmed video transmission to the TV. Audio
was not transmitted, consistent with version 0.2's implemented limit. The user
reported blurry text/images and did not record session resolution/profile/encoder.
The resolution, compression, and performance investigation, with six local
synthetic experiments, is in [VIDEO_QUALITY.md](VIDEO_QUALITY.md). These experiments
did not change application configuration and do not validate TV picture quality.

## Resolution preference and adaptation — 2026-10-24

Implemented a GTK resolution selector, offers limited by physical capture
geometry/encoder/TV, R2 video parameters for higher modes, and a bitrate/resolution
controller. Full HD uses an 8–12 Mbit/s target at this stage. 720p fallback
preserves 30 fps, profile, level, sockets, and timeline; recovery uses hysteresis.

Verification:

- `cargo fmt --all --check`, `cargo check --workspace --all-targets --locked`,
  `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`,
  and the native `wireless-display-app` build passed.
- `cargo test --workspace --locked`: 43 tests passed; eight environment-dependent
  integrations were ignored in this default run.
- GTK on Full HD Xvfb: selector sends the preference in `Connect`, prevents
  editing during a session, and preserves dummy/capture/error/shutdown lifecycle; passed.
- X11 capture, including pixel-geometry detection, on Xvfb: passed.
- X11 fallback with private D-Bus and no ScreenCast: passed.
- Four NetworkManager/IP/discovery integrations on private D-Bus: passed.
- A synthetic receiver with RTCP loss triggered automatic Full HD-to-720p
  renegotiation. M4 rejection preserved the previous configuration; acceptance
  committed the change only after the RTSP response. These tests did not reconnect P2P.
- x264 and OpenH264 decoded 1080p/720p/1080p; tests checked the first IDR, announced
  PTS against the access unit and actual MPEG-TS PES, and cancellation while the
  change frame was still blocked.

No personal screen was captured, no TV connection was started, and no firewall
rule was changed in this validation. Graphical integrations used isolated Xvfb
displays. Native headers/metadata were extracted into `/tmp/wireless-display-dev`;
no system package was installed. `target/debug/wireless-display` was rebuilt.

Limits: a physical TV, modes above Full HD, and a Wayland compositor's
Portal/PipeWire were not exercised in this change. After geometry probing, the
portal path requests a new restricted remote in the same session/D-Bus connection.
Loss-based adaptation depends on RTCP negotiated in SETUP; without feedback,
bitrate remains conservative and only local overload can request 720p. The TV
can reject renegotiation; this is reported without changing the format unilaterally.
Flatpak was not rebuilt at this stage.

## Smoothness and the 720p/60 fps option — 2026-10-24

The user reported perceived Full HD at 15–20 fps with 720p fallback in the log,
and did not note the encoder. Local synthetic measurement reproduced a plausible
bottleneck: OpenH264 screen/bitrate at 22.87 fps in Full HD; x264 superfast around
30 fps. With two conversion/scaling threads, x264 ultrafast High reached about
60 fps at 720p. These numbers do not measure TV rendering; details, limits, and
raw results are in VIDEO_QUALITY.md.

Applied: hardware priority, x264 ahead of OpenH264 for software fallback, lighter
presets, two conversion/scaling threads, and an optional 720p60 mode with
CEA/level 3.2 negotiation. 720p30 remains the default; Full HD retains a
30 fps/8–12 Mbit/s target and 720p30 resolution fallback. The p60 path accepts
59.94 Hz and does not duplicate a 30 Hz source's frames. Inspection queries the
producer's supported caps without confusing current cadence with its maximum.
p60 preflight includes conversion/scaling from a BGRx desktop and checks at
least 30 frames and 55 fps. FPS does not switch automatically during a session.

The GUI now shows encoded H.264 access-unit FPS and local drops. INFO logs
identify encoder, mode/target FPS, and fallback cause; DEBUG shows per-window
frame-rate statistics. RTP counters remain separate.

Verification:

- 46 workspace tests passed; eight environment-dependent integrations were
  ignored in the default run.
- Specific tests cover p60/level negotiation and initial p30 fallback, p60
  decoding with PES timestamps and frame statistics, 59.94 Hz compatibility,
  and rejection of frame duplication from a 30 Hz source.
- The GTK test on Full HD Xvfb passed, including sending the selected preference
  to the backend and locking the selector during connection. X11 capture/inspection
  on Xvfb also passed, including the ability to negotiate 60 fps.
- fmt, all-target check, warning-free Clippy, and the native build passed.

No TV connections, personal-screen captures, or firewall/driver changes occurred.
Physical retesting and the Portal/Wayland path remain unvalidated at this stage.
The lighter preset can reduce compression efficiency at the same bitrate.
target/debug/wireless-display was rebuilt; the old process must be closed to test new code.

## 720p/60 sharpness and bit budget — 2026-10-24

The user's physical retest confirmed improved Full HD but blurry text and blocks
at 720p60. The log identifies x264 at 6000 kbit/s and multiple windows with about
60 input fps, only 38–57.5 encoded fps, and up to 44 local drops.

The revision changes only the p60 configuration: an initial 8 Mbit/s target,
adaptive range up to 12 Mbit/s, x264 superfast with up to four frame threads
(without B-frames/lookahead), and scaling before color conversion. Preflight now
uses at least 120 frames with a 57 fps requirement. It can choose 720p30 during
initial negotiation when performance is insufficient. 720p30/1080p30 settings
remain unchanged. Logs now show loss percentages from fresh RTCP reports and
the precise reason for rejecting bitrate properties without PLAYING mutability.

For the synthetic text pattern, the final round measured PSNR-Y of 31.94 dB for
the previous p60 configuration and 33.31 dB for the revision, versus 32.96 dB at
p30. The revision encoded at 48.65 fps/42 drops versus the previous 40.40 fps/100
drops in that round. Results varied between rounds; they do not prove sustained
60 fps or final TV quality. Details and artifact paths are in VIDEO_QUALITY.md.
No dependency was installed and no driver/firewall was changed.

Verification: 47 workspace tests passed, including p60/59.94 Hz, real H.264
decoding, PES timestamps, adaptive 8–12 bitrate, and no unauthorized FPS change.
Eight environment-dependent integrations remained ignored in the default gate.
fmt/check and all-target Clippy passed. target/debug/wireless-display was rebuilt.

Limits: no new physical test, personal-screen capture, or agent-initiated TV
connection. Frame parallelism can add about 50 ms of encoding latency. The spatial
quality of a Full HD source reduced to 720p remains limited by downscaling and
H.264 4:2:0.

## Adaptive 1080p/30 ceiling — 2026-10-24

Full HD keeps its initial 8 Mbit/s target and now allows up to 16 Mbit/s.
1 Mbit/s increases require fresh favorable RTCP reports, input and encoding of
at least 29 fps, and at most one local drop per window. Missing statistics or
slow capture do not prove headroom. The superfast preset, 30 fps, and other modes'
ranges remain unchanged.

Two consecutive local-pressure windows first reduce the target by 2 Mbit/s,
respecting the minimum ten-second wait between adjustments. Encoding below
28 fps with input of at least 29 fps also counts as pressure, even without queue
overload or RTCP. At the 8 Mbit/s minimum, sustained pressure can still request
720p. RTCP loss reductions and TV acknowledgment for resolution changes remain active.

Verification: 49 workspace tests passed; eight environment-dependent integrations
were ignored in the default run. The policy was exercised with missing, nonfinite,
low, stable, and persistently pressured FPS, including bitrate reduction before
resolution reduction without RTCP. The native test changed the live target to
16 Mbit/s in x264/OpenH264, checked the encoder property and continued decoding,
and preserved 1080p/720p/1080p switching with IDR, PES PTS, and blocked-frame cancellation.
fmt and all-target/all-feature Clippy passed without warnings. The native build
passed; `target/debug/wireless-display` was rebuilt.

Tests used synthetic content and local sockets. There was no TV connection,
personal-screen capture, or dependency/firewall/driver change. This proof does
not measure TV quality or guarantee sustained 30 fps at 16 Mbit/s. Sharpness
improvement and link capacity need physical retesting.

## Discovery updates and P2P loss diagnosis — 2026-10-24

The report corresponds to the 2026-10-25 test at 00:16 UTC. The TV opened RTSP and
negotiated Full HD at 8 Mbit/s. Read-only local journals show iwlwifi reporting
beacon loss and AP connection loss at 00:16:57 UTC. wpa_supplicant logged a local
disconnection, reason 4, and removed the group for inactivity at 00:17:07 UTC.
NetworkManager ended the link for supplicant-disconnect. The app's address-change
error followed link loss. These observations do not determine why beacons were lost.

A separate discovery defect was reproduced on private D-Bus: WfdIEs changed on
an existing peer, but the list received no update without a new PeerAdded. The
test timed out before the fix and passed afterward. Discovery now observes
PropertiesChanged from NetworkManager's unique owner, filters adapter peer paths,
and rereads/validates fields. Signals from another peer or late after removal
do not recreate rows. The GUI preserves the existing row and button while
updating name, classification, and availability.

Ip4Config removal during group teardown, before disconnected-state publication,
now reports IPv4 configuration loss. Actual configuration/address changes still
end sharing. No automatic reconnection was added, and network loss is not ignored.

Verification:

- `cargo test --workspace --locked`: 49 tests passed, nine environment-dependent
  tests ignored in the default run.
- Five `on_dbus` tests passed on private D-Bus without desktop services, including
  WFD/availability updates, removal, StopFind on cancellation, and Ip4Config
  removal before state change.
- `gtk_dummy_vertical_slice` passed on Full HD Xvfb with private D-Bus. Updates
  preserved button identity/position and availability, along with the connection,
  synthetic capture, disconnection, error, and shutdown lifecycle.
- fmt, all-target check, all-target/all-feature warning-free Clippy, and native
  build passed. `target/debug/wireless-display` was rebuilt.

The build used PKG_CONFIG_PATH=/tmp/wireless-display-dev/pkgconfig. The graphical
test used the Xvfb already extracted there; no package was installed. The agent
did not capture the user's screen, connect to a TV, or change firewall, drivers,
or networking. Hardware discovery and radio stability still need physical
retesting. Fixing the list does not by itself resolve beacon loss.

## English codebase — 2026-11-03

Reviewed all 35 Rust files and project text. Rust identifiers, type names,
comments, and `.rs` filenames were already English. Translated application UI,
help, diagnostics, logs, errors, and test text; corresponding test expectations
were updated. README, architecture, all guides/reports, the original MVP
specification, and licensing notes are now English. The desktop entry uses its
existing default English comment.

A source comparison confirmed that only string contents and rustfmt layout
changed in Rust: control flow, identifiers, protocol values, and formatting
placeholders were preserved. Cargo manifests, Cargo.lock, dependency versions,
and Flatpak source definitions were unchanged.

Verification:

- `cargo fmt --all --check`, all-target check, all-target/all-feature Clippy with
  `-D warnings`, and native workspace build passed.
- `cargo test --workspace --locked`: 52 tests passed; nine environment-dependent
  tests ignored in the default run.
- Five private D-Bus tests and three dedicated Xvfb tests passed: 60 tests in total.
- `wireless-display --help` displayed English text; `--version` reported 0.2.0.

No physical TV connection or personal-screen capture was needed for this refactoring.
