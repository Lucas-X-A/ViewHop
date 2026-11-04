# ViewHop

ViewHop is an open-source Linux desktop application for sharing a laptop
or desktop screen with Miracast/Wi-Fi Display TVs and receivers over Wi-Fi Direct.
Its primary goal is to make wireless screen sharing practical across Linux
desktops, with screen and audio sharing as the long-term goal.

**The current MVP, version 0.2.0, streams video only.** It negotiates WFD/RTSP and
sends H.264 over MPEG-TS/RTP UDP through a local Wi-Fi Direct connection. Video
streaming has been confirmed on a physical TV, and automated tests exercise a
synthetic receiver with real H.264 decoding. Receiver compatibility, quality,
and performance remain experimental. The project name and application ID are
still provisional.

## What works today

- Discover nearby Wi-Fi Direct devices and distinguish advertised Miracast
  receivers from unconfirmed P2P peers. Availability changes update the list.
- Connect to a receiver through a temporary NetworkManager P2P connection.
- Select a monitor through ScreenCast Portal/PipeWire. On X11, capture can fall
  back to `ximagesrc` when ScreenCast support is absent. Wayland requires a
  compatible ScreenCast Portal backend.
- Choose 720p/30 fps, 720p/60 fps, 1080p/30 fps, or higher standard modes that fit
  an attached monitor. Capture geometry, encoder tests, and receiver capabilities
  determine the actual negotiated mode. Modes above Full HD still need physical
  validation; 60 fps requires support and sufficient performance throughout the path.
- Adapt the Full HD bitrate target from 8 to 16 Mbit/s, with resolution fallback
  to 720p and recovery when conditions allow. Bitrate increases depend on fresh
  RTCP feedback and sufficient encoding headroom; they are not based on internet speed.
- Prefer usable hardware encoders, with x264/OpenH264 software alternatives.
- Use GPU conversion/scaling and PipeWire DMA-BUF import when the complete path
  passes preflight. VA-API/CUDA plugins are optional; compatible CPU paths remain
  available. Actual power and memory savings depend on hardware and drivers.
- Inspect system capabilities, test local capture, or run a simulated connection
  without a TV through dummy mode.

**Audio capture/streaming, HDCP, UIBC, TCP media transport, and automatic
reconnection are not implemented.** A receiver that requires unsupported
features may reject the session. See [Miracast behavior and limits](docs/MIRACAST.md)
and the [validation history](docs/VALIDATION.md) for details.

## Languages and architecture

Both the backend and the graphical frontend are written in Rust.

| Layer | Languages | Main technologies |
| --- | --- | --- |
| Backend | Rust, Edition 2024 | Tokio, NetworkManager/D-Bus through zbus, ScreenCast Portal through ashpd, GStreamer |
| Frontend | Rust and GTK CSS | GTK4 through gtk-rs, GLib main loop, native widgets, theme-aware styling |
| Packaging helper | Python 3.11+ | Generates pinned Cargo sources for local Flatpak builds |

GTK runs on the main thread; the Tokio backend runs separately and communicates
with the UI through typed messages. GStreamer handles video processing and
encoding. The workspace has four crates: `core` for domain/state/protocol,
`platform` for Linux integrations, `media` for GStreamer, and `app` for the GUI
and coordination. See [ARCHITECTURE.md](ARCHITECTURE.md).

## Get the app

The MVP is currently available as **source code**. There is no official prebuilt
installer or published Flathub package yet. To use it now, download the source
and follow the native build instructions below. A local experimental Flatpak
build is also available as a packaging workflow.

On this project's GitHub page, choose **Code → Download ZIP** and extract the
archive. For development, copy the clone URL from **Code**, replace
`REPOSITORY_URL` below with that URL, and run:

```sh
git clone REPOSITORY_URL wireless-display
cd wireless-display
```

If you downloaded a ZIP instead, open a terminal in the extracted directory
containing `Cargo.toml`. All build commands below run from that project root.

## Requirements

For real sharing, you need Linux, a Wi-Fi adapter/driver with Wi-Fi P2P support,
NetworkManager managing that adapter, and a Miracast-compatible TV or receiver.
Keep the TV's Screen Share/Miracast mode open while searching and connecting.

Native builds require:

- Stable Rust **1.92 or newer**, with Cargo. Development checks also use rustfmt
  and Clippy.
- A C compiler/linker and pkg-config.
- GTK **4.8+**, GLib **2.66+**, and GStreamer **1.22+**, including development files.
- GStreamer capture, H.264, MPEG-TS, RTP/UDP, and decoder plugins. The packages
  below provide the usual native setup.
- PipeWire and an appropriate XDG Desktop Portal backend for portal capture.
  Wayland requires a backend implementing ScreenCast; installing
  `xdg-desktop-portal-gtk` alone does not guarantee that interface.

On Debian, Ubuntu, or Linux Mint, install the native dependencies:

```sh
sudo apt install build-essential pkg-config libgtk-4-dev libgstreamer1.0-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly \
  gstreamer1.0-libav gstreamer1.0-pipewire \
  network-manager pipewire xdg-desktop-portal
```

Install Rust separately and select the portal backend appropriate for your
desktop/compositor. On X11 without ScreenCast, the fallback captures the entire
X11 desktop rather than an individually selected monitor.

On Arch-based distributions, the corresponding packages are:

```sh
sudo pacman -S --needed base-devel pkgconf gtk4 gstreamer gst-plugins-base \
  gst-plugins-good gst-plugins-bad gst-plugins-ugly gst-libav \
  gst-plugin-pipewire networkmanager pipewire xdg-desktop-portal
```

Add the appropriate portal backend for your desktop. See the
[dependency matrix](docs/DEPENDENCIES.md) for Rust crate versions and native-library
requirements. libadwaita is not required.

## Build and run for use

After installing the requirements and obtaining the source, build an optimized
native executable:

```sh
cargo build --release --locked -p wireless-display-app
./target/release/wireless-display
```

The executable is `target/release/wireless-display`; no system-wide installation
is required. Run the app as your normal user. NetworkManager/Polkit control
authorization for network operations.

To share your screen:

1. Enable Screen Share/Miracast on the TV.
2. Open ViewHop and choose **Search for devices**.
3. Set **Preferred resolution** before connecting; 720p/30 fps is the default.
4. Choose **Share screen** for an available receiver and authorize capture if
   the portal asks. The app prepares capture and tests encoders before activating P2P.
5. Check the session card for connection status, negotiated resolution, and
   bitrate target. Use **Cancel connection** during setup or **Stop sharing**
   to end the session.

Expand **Tools & diagnostics** for encoder details, frame/packet counters,
encoding statistics, system diagnostics, and **Test local capture**. Local
capture uses `fakesink`: it consumes frames without a preview or recording file.
An active local test keeps its status and **Stop capture** visible even when the
tools section is collapsed. The interface follows the desktop's GTK theme,
including dark mode.

The firewall must allow RTSP TCP 7236 and negotiated RTCP traffic on the P2P
interface. The app does not change firewall rules. If connection fails, follow
[TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) before adjusting your setup.

## Build for development and tests

Build the workspace in debug mode:

```sh
cargo build --workspace --locked
```

Run the GUI with a simulated TV and synthetic video, without real Wi-Fi P2P or
portal access:

```sh
cargo run --locked -p wireless-display-app -- --dummy
```

Query actual host capabilities without opening the GUI:

```sh
cargo run --locked -p wireless-display-app -- --diagnostics
```

Run real sharing with debug logs:

```sh
RUST_LOG=wireless_display=debug cargo run --locked -p wireless-display-app
```

`--diagnostics` still queries actual capabilities when combined with `--dummy`.
The binary also accepts `--help` and `--version`. Normal application logs omit
peer MAC addresses, tokens, and RTSP bodies; review local paths and library error
details before sharing logs.

Run the development checks:

```sh
cargo fmt --all --check
cargo check --workspace --all-targets --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

The default suite uses synthetic media and local loopback sockets; it does not
require a physical TV or capture your screen. GTK/X11, private D-Bus, and real
hardware tests have separate environment requirements. See
[TESTING.md](docs/TESTING.md) for how to run them and distinguish virtual tests
from physical validation.

## Local Flatpak build

Flatpak packaging is experimental. Install flatpak-builder, configure Flathub,
and install GNOME Platform/SDK 51 and the matching Rust stable SDK extension as
described in [flatpak/README.md](flatpak/README.md). The source-generation helper
requires Python 3.11 or newer. Then run:

```sh
python3 scripts/generate-cargo-sources.py
flatpak-builder --user --install --force-clean build-dir flatpak/org.example.ViewHop.yml
flatpak run org.example.ViewHop
```

The manifest builds pinned Cargo sources offline after downloading them. This
is a local build/install workflow, not installation from a published Flathub
listing. See [sandbox permissions](docs/FLATPAK.md) and the
[validation report](docs/VALIDATION.md) for packaging status and tested paths.

## Next steps

- Broaden interoperability testing across TVs, adapters, drivers, and desktops.
- Validate real Wayland Portal/PipeWire capture, hardware encoders, higher
  resolutions, and Flatpak builds on supported environments.
- Improve latency, link stability, recovery, and adaptive video quality.
- Add negotiated audio capture/streaming and A/V synchronization.
- Provide a dedicated D-Bus API for optional desktop integrations.

## License

Original project code is licensed under [MIT](LICENSE). Native libraries,
plugins, and Rust dependencies retain their own licenses; see [LICENSES.md](LICENSES.md).

## Acknowledgments

ViewHop was built with assistance from OpenAI Codex.
