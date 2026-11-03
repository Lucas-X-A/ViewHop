# Dependency matrix checked on 2026-10-17

Stable versions were checked through the crates.io API; APIs and requirements
were verified in the published crate sources before writing Cargo.toml.

| Component | Version | Compatibility |
| --- | --- | --- |
| gtk4 | 0.11.5 | glib/gio 0.22; Rust >= 1.92; GTK 4.8 feature |
| glib / gio | 0.22.10 | Same GTK/GStreamer family; direct gio dependency in media for sockets |
| gstreamer | 0.25.4 | glib 0.22; Rust >= 1.92; Gst 1.22 feature |
| ashpd | 0.13.13 | tokio runtime; zbus ^5.13; Rust >= 1.87 |
| zbus | 5.19.0 | Latest stable 5.x compatible with ashpd at the time; 6.0 exists |
| tokio / tokio-util | 1.53.3 / 0.7.20 | Runtime separate from GTK |
| futures-util / async-channel | 0.3.34 / 2.5.0 | Streams and bounded messaging |
| tracing / tracing-subscriber | 0.1.44 / 0.3.23 | EnvFilter and stderr |
| thiserror | 2.0.21 | Typed errors |

Cargo.lock pins the resolved patch versions. `rust-version=1.92` sets an explicit
minimum; Cargo respects it when resolving transitive dependencies.
gstreamer-video/app/rtsp/server were not added: the current pipeline does not
require those APIs. There are no Rust HTTP dependencies, libadwaita, or mandatory codecs.

In version 0.2, gio is a direct dependency of media, at the version already used by GTK.
Its safe Socket::from_fd API takes already reserved UDP sockets, avoiding project-owned
unsafe code and port allocation races when configuring udpsink/udpsrc. Core uses tracing
to log RTSP methods and sequence numbers without logging bodies, Session, or credentials.

Documentation consulted:

- https://docs.rs/gtk4/0.11.5/
- https://docs.rs/gstreamer/0.25.4/
- https://docs.rs/ashpd/0.13.13/ashpd/desktop/screencast/
- https://docs.rs/zbus/5.19.0/
- https://networkmanager.dev/docs/api/latest/gdbus-org.freedesktop.NetworkManager.Device.WifiP2P.html
- https://networkmanager.dev/docs/api/latest/gdbus-org.freedesktop.NetworkManager.WifiP2PPeer.html
- https://networkmanager.dev/docs/api/latest/settings-wifi-p2p.html
- https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html
- https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/master/src/gst/gstpipewiresrc.c

The installed GNOME 51 runtime was inspected: Freedesktop 26.08 base, GStreamer
1.28.6, and the PipeWire plugin present; ximagesrc is not included. The manifest
provides that plugin through gst-plugins-good.
