# Testing

## Without hardware or a display

```sh
cargo fmt --all --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Tests cover valid/invalid transitions and device identity, MAC/WFD parsing,
NetworkManager types, P2P settings, mocks, cancellation, dummy connections,
fragmented/pipelined and hostile RTSP input, WFD parameters, modern PipeWire
serials and legacy node IDs, X11 rejection on Wayland, and encoder selection.

The media test uses real videotestsrc/fakesink and confirms frames before
cancellation. It does not depend on a TV, Wi-Fi, PipeWire, or a display. Local
base/good plugins are required.

## GTK and virtual X11

Run on a dedicated Xvfb display with a private session bus:

```sh
GTK_A11Y=none xvfb-run -a -s '-screen 0 1920x1080x24' dbus-run-session --config-file=crates/app/tests/session-bus.conf -- cargo test -p wireless-display-app --test gtk_smoke -- --ignored --test-threads=1
GTK_A11Y=none xvfb-run -a -s '-screen 0 1920x1080x24' dbus-run-session --config-file=crates/app/tests/session-bus.conf -- cargo test -p wireless-display-app --test gtk_presentation -- --ignored --test-threads=1
xvfb-run -a dbus-run-session --config-file=crates/app/tests/session-bus.conf -- cargo test -p wireless-display-app --test x11_fallback -- --ignored
xvfb-run -a cargo test -p wireless-display-media --test x11_capture -- --ignored
dbus-run-session --config-file=crates/app/tests/session-bus.conf -- cargo test -p wireless-display-platform --lib on_dbus -- --ignored --test-threads=1
```

The GTK test opens a window on the main thread, receives backend events from
another thread, discovers the dummy TV, connects, captures test frames, stops
capture, disconnects, simulates an error, verifies Search for devices is enabled,
searches again, and shuts down. Availability updates preserve the existing
button/row and update sensitivity. The X11 test confirms ximagesrc frames without
accessing the user's actual screen. These tests are ignored in the default suite
because they require a display. The private bus has no service directories and
does not start desktop daemons.
The presentation test injects backend events without using Wi-Fi or capture. It
checks progressive disclosure, receiver availability, plain-text device names,
the active receiver after discovery cleanup, cancel/stop/recovery commands,
negotiated quality through pause, local-test controls, and cooperative shutdown.
The x11_fallback test exercises the real backend selector, verifies that Wayland
rejects a missing portal, and that X11 produces frames through fallback.
D-Bus tests use a fake NetworkManager on the private bus. They confirm rejection
of an expired peer and reading/monitoring an active group's IPv4, including a
missing gateway. They do not query the system bus or connect to a TV. They also
reproduce activation with empty AddressData, later publication through signals,
initially absent Ip4Config, wait cancellation, and address/link loss. They cover
WfdIEs updates without a new PeerAdded, availability changes, rejection of another
peer's updates and late signals after removal, and StopFind on cancellation.
Ip4Config removal before a state change is identified as configuration loss,
without misreporting it as an IP change.

Real discovery, only with hardware and authorization to start a search:

```sh
cargo test -p wireless-display-platform --test p2p_discovery -- --ignored
```

This test calls StartFind, waits three seconds, and cancels/awaits StopFind.
It does not connect to peers or require finding a TV to validate the lifecycle.

## WFD video without a physical TV

The default suite also opens loopback sockets. It uses a synthetic receiver for
M1–M7, capabilities, SETUP/PLAY, keepalive, PAUSE/PLAY, cancellation, and TEARDOWN.
It validates incorrect CSeq and rejects disallowed ports/destinations. Requires
H.264, h264parse, mpegtsmux, rtpmp2tpay, rtpbin, UDP, and a test H.264 decoder.

```sh
cargo test -p wireless-display-app --lib mirroring::tests
cargo test -p wireless-display-media --lib streaming::tests
```

The media test sends real RTP, confirms MPEG-TS and RTCP, and uses another
pipeline to depacketize, demultiplex, and decode H.264 frames. It does not access
a display or Wi-Fi. If the sandbox blocks sockets, allow loopback for these tests.
This does not authorize a TV connection or capture of the user's screen.

## Real integration (manual)

1. Run `wireless-display --diagnostics` and record host capabilities.
2. On X11 and Wayland, select a monitor through the portal; verify frames > 0.
3. Stop and restart; cancel selection and verify capture does not start.
4. Close the session from the desktop, disconnect the monitor, and test source removal.
5. On X11 without ScreenCast, confirm ximagesrc fallback. On Wayland without
   ScreenCast, confirm an explanatory error, including with XWayland DISPLAY set.
6. With P2P hardware, search while the TV is in Screen Share and verify the
   distinction between unknown peers and WFD advertisements. Test
   StartFind/StopFind/the 120-second timeout.
7. Click Share screen. Authorize capture before P2P; check the negotiated
   mode/encoder and RTP packets. Verify the TV picture and record interoperability
   capabilities without personal data. Audio is not expected in this version.
8. Stop, disconnect, close during activation/negotiation, and stop Screen Share
   on the TV. Check cleanup of protocol, pipeline, portal, and temporary profile.
9. Repeat inside Flatpak and on GNOME/KDE/Cinnamon as backends are available.

Do not automate portal-selection approval or change host network settings in CI.
Real integration steps can only be considered passed with appropriate
hardware/desktop evidence. See VALIDATION.md for what was run here.

## Resolution and video adaptation

Before connecting, choose **Preferred resolution**. 720p prioritizes performance;
1080p starts at an 8 Mbit/s target and adapts up to 16 Mbit/s. Higher modes appear
only when they fit a current monitor. The display selected through the portal,
the encoder, and the TV can impose a lower limit. The session card shows the actual
mode and bitrate **target**, excluding transport overhead/peaks.

For a physical test, close the old app and open the rebuilt binary:

```sh
RUST_LOG=wireless_display=debug ./target/debug/wireless-display
```

Choose 1080p, connect to the TV, and compare small text, scrolling, and motion.
Record the displayed mode/encoder/bitrate. Bitrate increases require valid RTCP
reports from the TV; the GUI warns if SETUP did not negotiate them. In that case,
1080p stays at the conservative 8 Mbit/s target, and only local overload can
request 720p. There is no need to test broadband speed: Miracast uses the Wi-Fi
Direct link. TV rejection of M4 is reported without restarting the connection.

Local tests use synthetic video and loopback sockets. They cover selection
limited by capture/encoder/receiver, R2 formats, RTCP, automatic fallback, M4
rejection, and 1080p/720p/1080p switching with decoding, IDR, actual PES PTS, and
cancellation during a blocked frame. They do not capture the user's screen or
connect to a TV. Modes above Full HD still need physical validation.

## Smoothness test: 720p/60 and 1080p/30

Choose **720p at 60 fps — smoother motion** or **1080p — Full HD** before
connecting. The p60 mode appears for monitors around 60 Hz or higher (also when
GDK does not report a refresh rate). Without common TV/encoder support, the
actual mode may be 720p/30; check the session card, not only the selector.
60 fps is not a promise of sustained performance under every load.

The **Encoding** line under **Tools & diagnostics** shows measured FPS separately from RTP
packets. Compare scrolling/moving text and record that value. If the GUI reports
about 30 encoded fps in Full HD while the TV still stutters, the next diagnostic
step must examine the link/receiver. If encoding drops and local frames are
discarded, the pipeline/CPU is under pressure. Logs identify the selected encoder
and fallback cause; DEBUG includes statistics.

x264 is the first software alternative, with hardware still preferred. The Full
HD target uses 8–16 Mbit/s; 720p/60 uses 8–12 Mbit/s. The preset change reduces CPU
cost and can reduce compression efficiency. Reopen the rebuilt binary with
`RUST_LOG=wireless_display=debug` for retesting.

### Adaptive 1080p/30 bitrate

Full HD starts at 8 Mbit/s and increases in 1 Mbit/s steps up to 16 Mbit/s.
Alongside five fresh, favorable RTCP reports, it requires input/encoding of at
least 29 fps and at most one drop per two-second window. Without fresh RTCP or
sufficient statistics, the target does not increase. A static picture or slow
capture alone is not treated as a processing failure.

Compare the same text, scrolling, and motion; record bitrate, encoded FPS, and
drops. Two local-pressure windows reduce the target by 2 Mbit/s before attempting
to lower resolution. The minimum is 8 Mbit/s; sustained pressure at that minimum
can request 720p. Adjustments have a minimum ten-second wait. Worsening RTCP loss
also reduces bitrate. The relevant capacity is the Wi-Fi Direct link, not internet
speed. The superfast preset and 30 fps remain; the higher ceiling does not
guarantee a visible improvement or smoother TV playback.

### 720p/60 sharpness

The latest revision uses superfast and up to four x264 frame threads, and scales
the image before color conversion. The initial target increased from 6 to
8 Mbit/s to preserve at least the 720p/30 per-frame bit budget. Increasing to
12 Mbit/s still requires favorable RTCP feedback. Preflight now uses at least
120 frames and requires 57 fps; a machine without headroom may start at 720p/30.
This does not guarantee 60 fps under any later load.

Compare small text and blocks in the same content, and record GUI FPS/drops.
The configuration can add about 50 ms of encoding delay. Frame-rate logs include
`loss_percent`; `None` means no new report in that window. Traffic of 1 MiB/s
in the monitor is about 8.4 Mbit/s, but does not measure the Wi-Fi Direct link's
maximum capacity.
