# Sandboxing and distribution

GNOME Platform/SDK 51 uses the Freedesktop 26.08 base and provides GTK4,
GStreamer, and pipewiresrc. The gst-ximagesrc module builds only the required X11
fallback, from official sources with a pinned checksum. Rust comes from the
SDK's rust-stable extension.

Permissions:

| Permission | Purpose |
| --- | --- |
| share=network | System D-Bus and P2P/RTSP/RTP/RTCP transport |
| share=ipc | X11/MIT-SHM for the ximagesrc fallback |
| socket=wayland | Native Wayland GTK window |
| socket=fallback-x11 | GTK/capture in X11 sessions; no X11 access on Wayland |
| device=dri | GTK rendering and hardware encoding when usable |
| system-talk-name=org.freedesktop.NetworkManager | Enumeration, discovery, and P2P connection |

There is no filesystem=host, device=all, unrestricted system-bus, audio socket,
direct PipeWire socket, or access to Mutter/Cinnamon. The portal is accessed
through Flatpak's default filtered session bus and returns only the authorized
remote FD.

The NetworkManager D-Bus permission is restricted to one service, but that
service exposes network management: host Polkit authorization remains essential.
The package adds no Polkit rules or privilege escalation.

On X11, capture fallback has the same isolation limitations as the X11 protocol.
On Wayland, only ScreenCast Portal is allowed. Do not add socket=x11 to work
around a missing portal.

`cargo-sources.json` contains .crate archives pinned by version/checksum and
vendor metadata. The network is used only to prepare sources; Cargo build-commands
do not use the network. The manifest is not a ready-to-submit Flathub package:
the project name/ID and URLs must be defined before publication.

Video requires a genuinely usable H.264 encoder. Depending on the runtime's
codec distribution, the org.freedesktop.Platform.codecs-extra//26.08-extra
extension may be needed for software fallback. The app tests the encoder before
P2P and reports missing support rather than claiming it is available.
Permissions were not broadened for WFD; RTSP binds only to the P2P IP address.
