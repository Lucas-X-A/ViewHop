Act as a senior software engineer specializing in Rust, Linux desktop, GTK,
GStreamer, PipeWire, D-Bus, NetworkManager, Flatpak, Wi-Fi Direct, and streaming protocols.

Your task is to **implement, not merely plan**, the first executable architectural
version of a Linux application for wireless screen mirroring over Miracast/Wi-Fi Display.

The project's future goal is to let a Linux laptop transmit its screen and audio
to Miracast-compatible TVs and receivers, including LG webOS TVs, similarly to
Windows 11's “Wireless Display / Cast” feature.

The application must work across Linux distributions and must not depend
exclusively on GNOME, KDE, or Cinnamon.

The main target environments are:

- Linux Mint/Cinnamon with X11 and Wayland sessions (mature Wayland support from
  Linux Mint 27 — Adrien, scheduled for late 2026);
- Ubuntu and GNOME on Wayland;
- KDE Plasma/Wayland;
- Arch/CachyOS distributions;
- other distributions with X11 or Wayland;
- Flatpak as the primary installation method.

The project name is not yet defined. Temporarily use `WirelessDisplay` in the
code and keep the name, desktop ID, and application ID easy to change later.

Do not use Axum. This is a desktop application, not a web server.

# 1. Goal of this first implementation

Deliver a **technically sound, executable vertical MVP**.

Do not pretend Miracast support is complete if some stages are not implemented.

In this first implementation, make the following functional wherever possible:

1. Rust GTK4 application;
2. well-defined asynchronous architecture;
3. Tokio runtime separate from the GTK thread;
4. message-based UI/backend communication;
5. X11/Wayland environment detection;
6. XDG Desktop Portal integration;
7. screen capture through ScreenCast Portal + PipeWire;
8. X11 capture fallback when the screencast portal is unavailable;
9. GStreamer pipeline initialization and management;
10. NetworkManager D-Bus integration;
11. Wi-Fi P2P interface detection;
12. Wi-Fi Direct peer discovery;
13. identification of possible Wireless Display receivers;
14. GTK interface showing discovered devices;
15. session state machine;
16. diagnostics mode;
17. test mode without real hardware;
18. initial Miracast/Wi-Fi Display protocol structure;
19. Flatpak manifest;
20. architecture documentation and build instructions.

Then leave the foundation ready for complete WFD/RTSP negotiation and H.264/AAC
streaming to the TV.

Do not deliver only empty files or functions with `TODO`. Implement an executable vertical slice.

# 2. Required technologies

Use stable Rust with Edition 2024.

Use the latest compatible stable versions available at implementation time.
Before adding versions to `Cargo.toml`, consult current documentation and verify compatibility.

For reference, the current ecosystem includes approximately these families:

- `gtk4` / gtk-rs;
- `glib`;
- `gio`;
- `gstreamer`;
- `gstreamer-video`;
- `gstreamer-app`, if needed;
- `gstreamer-rtsp`;
- `gstreamer-rtsp-server`, only if appropriate for WFD;
- `zbus`;
- `ashpd`;
- `tokio`;
- `tokio-util`;
- `futures-util`;
- `async-channel`;
- `tracing`;
- `tracing-subscriber`;
- `thiserror`;
- `serde`, only where genuinely needed.

Avoid unnecessary dependencies.

Use `anyhow` only at the application boundary if useful. Internal libraries
should prefer typed errors through `thiserror`.

Do not implement critical components in C/C++.

Native libraries such as GTK, GLib, GStreamer, PipeWire, and NetworkManager may
be consumed through the appropriate Rust bindings.

# 3. Fundamental threading rule

GTK4 MUST NOT run inside the Tokio runtime.

GTK must remain exclusively on the main thread with the GLib Main Loop.

The architecture should look approximately like this:

    ┌─────────────────────────────┐
    │       GTK MAIN THREAD       │
    │                             │
    │ gtk4 / glib / UI widgets    │
    │                             │
    └──────────────┬──────────────┘
                   │
             commands/events
                   │
    ┌──────────────▼──────────────┐
    │      BACKEND COORDINATOR    │
    │                             │
    │    Tokio multithread RT     │
    │                             │
    ├─────────────────────────────┤
    │ NetworkManager / zbus       │
    │ Portal / ashpd              │
    │ Miracast protocol           │
    │ Network operations          │
    │ Session state machine       │
    └──────────────┬──────────────┘
                   │
                   ▼
            GStreamer pipelines
                   │
             internal threads

Never send GTK objects across threads.

GTK objects must not appear inside `Send`/`Sync` structures.

Do not use `Arc<Mutex<gtk::Widget>>`.

Use a message-oriented architecture.

Suggested channels:

UI -> backend:

`tokio::sync::mpsc`

backend -> UI:

`async_channel`

On the GTK side, use `glib::MainContext` to consume events and update widgets
only on the main thread.

If current gtk-rs/GLib versions offer a more idiomatic solution, use it while
preserving this separation.

# 4. Project architecture

Create a Cargo Workspace.

Suggested initial structure:

    wireless-display/
    ├── Cargo.toml
    ├── Cargo.lock
    ├── README.md
    ├── ARCHITECTURE.md
    ├── LICENSES.md
    ├── rustfmt.toml
    ├── flatpak/
    │   ├── <application-id>.yml
    │   └── README.md
    ├── data/
    │   ├── <application-id>.desktop
    │   ├── <application-id>.metainfo.xml
    │   └── icons/
    ├── docs/
    │   ├── MIRACAST.md
    │   ├── FLATPAK.md
    │   ├── TESTING.md
    │   └── TROUBLESHOOTING.md
    └── crates/
        ├── core/
        ├── platform/
        ├── media/
        └── app/

Names may be refined if needed.

Responsibilities:

`core`

- domain types;
- state machine;
- commands;
- events;
- receiver/device model;
- trait-based abstractions;
- future Miracast protocol implementation;
- no GTK dependency.

`platform`

- D-Bus;
- NetworkManager;
- Wi-Fi Direct;
- XDG Desktop Portal;
- PipeWire;
- X11/Wayland identification;
- capture backends.

`media`

- GStreamer;
- pipeline construction;
- encoder selection;
- lifecycle;
- bus messages;
- error handling;
- statistics.

`app`

- GTK4;
- view models;
- main thread;
- Coordinator communication;
- startup/shutdown.

Avoid circular dependencies.

# 5. Event model

Create types similar to:

    enum AppCommand {
        StartDiscovery,
        StopDiscovery,
        RefreshDevices,
        Connect(DeviceId),
        Disconnect,
        StartCapture(CaptureRequest),
        StopCapture,
        RunDiagnostics,
        Shutdown,
    }

and:

    enum AppEvent {
        StateChanged(AppState),
        DeviceAdded(WirelessDisplayDevice),
        DeviceUpdated(WirelessDisplayDevice),
        DeviceRemoved(DeviceId),
        DiscoveryStarted,
        DiscoveryStopped,
        CaptureSourceSelected(...),
        CaptureStarted(...),
        CaptureStopped,
        MediaPipelineStarted,
        MediaPipelineStopped,
        DiagnosticResult(...),
        Warning(...),
        Error(...),
    }

Adapt the types as needed.

Do not use unstructured strings to represent internal states.

Use enums and specific types.

# 6. State machine

Implement an explicit state machine.

Suggested initial states:

    Idle
      ↓
    Discovering
      ↓
    DeviceSelected
      ↓
    Connecting
      ↓
    Negotiating
      ↓
    Streaming
      ↓
    Disconnecting
      ↓
    Idle

And:

    Error

The backend must control transitions.

Avoid a second, independent version of connection state in the graphical interface.

The UI must reflect backend-produced state.

Include unit tests for valid and invalid transitions.

# 7. Cancellation and shutdown

Use structured cancellation.

Consider:

- `tokio_util::sync::CancellationToken`;
- `JoinSet`;
- explicit network-operation timeouts;
- cooperative shutdown.

When closing the application:

1. stop Wi-Fi P2P discovery;
2. cancel connection attempts;
3. stop the GStreamer pipeline;
4. close the ScreenCast Portal session;
5. disconnect any P2P session;
6. await relevant tasks;
7. shut down the runtime.

Do not leave orphan tasks.

# 8. X11 and Wayland support

Do not assume “Linux = X11” in the architecture.

Do not assume GNOME/Mutter either.

Implement:

    trait CaptureBackend {
        ...
    }

with at least:

    PortalCaptureBackend
    X11CaptureBackend

Selection must follow this order:

    any session
          │
          ▼
    XDG ScreenCast Portal available?
          │
       yes│       no
          ▼        │
      Portal       │
                   ▼
              X11 session?
                │    │
              yes    no
                │    │
                ▼    ▼
             ximagesrc
                 explanatory
                   error

In other words:

## Wayland

On Wayland, screen capture must occur exclusively through XDG Desktop Portal /
ScreenCast / PipeWire.

Never capture an arbitrary Wayland screen by bypassing the compositor.

Do not make Mutter-specific APIs an architectural requirement.

## X11

On X11:

1. try ScreenCast Portal first;
2. if the desktop backend does not offer ScreenCast, use GStreamer `ximagesrc` fallback;
3. encapsulate fallback entirely inside `X11CaptureBackend`.

The rest of the application must not need to know whether frames came from
Portal/PipeWire or X11.

Use `XDG_SESSION_TYPE` for diagnostics/backend selection, not as the sole source
of truth when GDK or the portal provides better information.

# 9. XDG ScreenCast Portal

Use `ashpd`.

Implement the correct portal flow:

    CreateSession
          ↓
    SelectSources
          ↓
        Start
          ↓
    OpenPipeWireRemote
          ↓
       PipeWire
          ↓
      GStreamer

Initially allow sharing:

- an entire monitor;
- optionally a window, if supported by the portal implementation.

Prioritize MONITOR for the first MVP.

Use an embedded cursor when supported.

Handle:

- the user cancelling selection;
- a missing portal;
- a backend without ScreenCast support;
- session closure;
- source removal;
- monitor disconnection;
- unavailable PipeWire.

For modern ScreenCast Portal versions, prefer the persistent/serial PipeWire
identifier when available, retaining compatibility fallback for older implementations.

Do not directly access all of the user's PipeWire nodes when the portal supplies
a restricted remote FD.

# 10. PipeWire + GStreamer integration

Avoid frame-by-frame video processing in Rust.

Rust coordinates; GStreamer executes the media pipeline.

The architecture should be:

    Portal
       │
       ▼
    PipeWire
       │
       ▼
    pipewiresrc
       │
       ▼
    videoconvert
       │
       ▼
    videoscale
       │
       ▼
    H.264 encoder
       │
       ▼
    RTP / Miracast

Initially, before Miracast negotiation works, provide a test mode:

    PipeWire
       │
       ▼
    GStreamer
       │
       ├── local preview OR
       └── fakesink

The goal is to prove that:

- the portal works;
- the PipeWire stream was obtained;
- GStreamer can consume the stream;
- start/stop are reliable.

Do not copy frames unnecessarily into Rust buffers.

# 11. GStreamer

Create a `MediaPipeline` abstraction.

It must have at least:

    start()
    pause()
    stop()

and report:

- Error;
- EOS;
- StateChanged;
- Warning.

Monitor GstBus correctly.

Never block the GTK main thread waiting for GStreamer messages.

Dynamically detect available plugins.

For H.264, prefer hardware acceleration in the future when available.

Check possible encoders dynamically; do not assume any specific one is installed.

Examples that may exist depending on the system:

- VA-API;
- NVENC;
- V4L2;
- OpenH264;
- x264.

Create something like:

    EncoderSelector

that produces a list of available candidates through GstRegistry.

Priority order:

1. suitable hardware encoder;
2. efficient open-source encoder;
3. software fallback.

Do not fail the entire application startup because a particular encoder is missing.

Show this information in diagnostics.

# 12. NetworkManager and Wi-Fi Direct

Do not invoke `nmcli` as the primary mechanism.

Use NetworkManager's D-Bus API directly through `zbus`.

The app must not run as root.

Use the system bus only for strictly necessary services.

Implement a module:

    NetworkManagerService

Responsible for:

- checking NetworkManager availability;
- enumerating devices;
- finding Wi-Fi devices;
- finding Wi-Fi P2P devices;
- checking feature support;
- starting P2P discovery;
- stopping P2P discovery;
- consuming peer-added/removed events;
- transforming D-Bus objects into internal models.

Use NetworkManager's current `WifiP2P` D-Bus interface.

Avoid aggressive polling when D-Bus signals exist.

Implement signal-based discovery.

The `core` layer must not depend directly on `zbus`.

# 13. Wireless Display / WFD

Wi-Fi Direct and Miracast are not exactly the same thing.

An arbitrary Wi-Fi P2P peer must not automatically appear to the user as a
confirmed Miracast TV.

Where possible, the model must distinguish:

    PeerType::UnknownP2P
    PeerType::WirelessDisplayReceiver

Inspect Wi-Fi Display Information Elements available through NetworkManager.

Create internal types representing:

- MAC;
- peer name;
- WFD capability;
- status;
- IP address when connected;
- future codec/capability information.

If information cannot yet be obtained at this stage, use `Option<T>` and document
its absence.

Do not invent values.

# 14. Wi-Fi P2P connection

Create the initial abstraction needed to connect to a receiver.

NetworkManager supports `wifi-p2p` connections and Wireless Display-specific properties.

Implement the integration layer while clearly separating:

    discovery
    connection
    WFD negotiation
    streaming

Do not create one giant `connect_to_tv()` function containing the entire protocol.

Use components.

Conceptual example:

    P2pDiscovery
    P2pConnection
    WfdSession
    RtspSession
    MediaSession

# 15. Miracast / RTSP

Create the initial module structure:

    protocol/
        wfd/
            mod.rs
            capabilities.rs
            session.rs
            rtsp/
                mod.rs
                message.rs
                parser.rs
                transport.rs

Miracast does not have to be 100% complete in this first milestone.

However:

- model components correctly;
- prepare a state machine;
- implement a testable RTSP parser/serializer if this stage is reached;
- add tests with synthetic messages;
- never use fragile `split()` parsing without validation.

Use `tokio::net` for asynchronous TCP connections if implementing RTSP directly.

Do not use Axum.

Do not treat Miracast as merely “a regular RTSP server.” Wi-Fi Display has its
own RTSP-based negotiation and capability exchange.

If `gstreamer-rtsp-server` does not correctly represent that negotiation, do not force its use.

# 16. Audio

Prepare the architecture for audio streaming, but do not let audio complexity
block the video MVP.

Create an abstraction:

    AudioCapture

Plan modern support through PipeWire.

Miracast will later use a compatible codec negotiated with the receiver.

Initially:

- implement detection;
- model capabilities;
- prepare the pipeline to incorporate audio.

The UI must be able to report:

    Video: available
    Audio: not enabled yet

when that reflects the actual state.

Do not pretend support exists.

# 17. GTK4 interface

Use GTK4 through gtk-rs.

Avoid a mandatory libadwaita dependency initially to keep the app desktop-neutral.

Create a simple, modern interface.

Conceptual initial screen:

    ┌─────────────────────────────────────────┐
    │ Wireless Display                       │
    ├─────────────────────────────────────────┤
    │                                         │
    │ Available displays                      │
    │                                         │
    │ ┌─────────────────────────────────────┐ │
    │ │ 📺 LG webOS TV                     │ │
    │ │ Wireless Display                   │ │
    │ │                         Connect     │ │
    │ └─────────────────────────────────────┘ │
    │                                         │
    │ ┌─────────────────────────────────────┐ │
    │ │ ? Unknown Wi-Fi Direct device      │ │
    │ │ P2P peer                           │ │
    │ └─────────────────────────────────────┘ │
    │                                         │
    │ [ Scan again ]                          │
    │                                         │
    │ Status: Ready                           │
    └─────────────────────────────────────────┘

When connected:

    Connected to LG webOS TV

    Screen:
    [ Entire monitor ]

    Resolution:
    1920x1080

    Codec:
    H.264

    [ Stop sharing ]

Do not hard-code this information if it is not actually available.

The UI must reflect backend events.

# 18. Diagnostics

Implement a “Diagnostics” screen or dialog.

Show:

- session type: X11/Wayland;
- desktop environment;
- XDG current desktop;
- portal availability;
- ScreenCast Portal version;
- supported source types;
- PipeWire availability;
- GStreamer availability;
- GStreamer version;
- required plugins;
- detected H.264 encoders;
- NetworkManager availability;
- NetworkManager version;
- Wi-Fi device;
- Wi-Fi P2P device;
- discovery availability;
- any limitations found.

Never expose unnecessary sensitive data.

MAC addresses may be partially masked in normal logs.

Allow detailed logging only through a debug option.

# 19. Logging

Use:

    tracing
    tracing-subscriber

Support:

    RUST_LOG=wireless_display=debug

Separate targets:

    wireless_display::ui
    wireless_display::network
    wireless_display::portal
    wireless_display::media
    wireless_display::miracast

Avoid `println!` for normal logging.

Never log secrets or tokens.

# 20. Error handling

Create specific errors, for example:

    PortalError
    NetworkError
    MediaError
    ProtocolError
    FlatpakEnvironmentError

Errors must include:

- a technical cause;
- a user-friendly UI message where applicable;
- a source error where applicable.

Do not use `unwrap()` or `expect()` in normal runtime paths.

They may be used in tests when failure necessarily means a test bug.

# 21. Flatpak

Design the app for Flatpak from the start.

Create a functional manifest.

Choose a current, supported GNOME/FreeDesktop runtime providing the appropriate
GTK/GStreamer dependencies.

Use stable Rust for the build.

Commit `Cargo.lock`.

Make the project compatible with offline Flatpak/Flathub builds, including
Cargo source generation/vendoring where needed.

Follow least privilege in the manifest.

Likely permissions include equivalents of:

    --share=network
    --socket=wayland
    --socket=fallback-x11
    --device=dri

and D-Bus access to NetworkManager.

Add an audio socket only when necessary.

DO NOT automatically use:

    --filesystem=host
    --device=all

Do not grant unrestricted system-bus access.

Allow only D-Bus services proven necessary.

If Avahi or firewalld is needed later, add it only with a documented technical reason.

ScreenCast access must use the portal, not permissions that break the sandbox.

# 22. Security

Treat network input as untrusted.

In particular:

- device names;
- WFD information elements;
- RTSP messages;
- capability strings;
- headers;
- payload sizes.

The future RTSP implementation must have:

- a maximum header size;
- a maximum message size;
- timeouts;
- Content-Length validation;
- rejection of invalid UTF-8 where applicable;
- protection against unbounded buffer growth.

Avoid `unsafe`.

If a dependency requires `unsafe`, keep that code outside our project whenever possible.

If project-owned `unsafe` is absolutely necessary:

1. isolate it in a small module;
2. document invariants;
3. add a `SAFETY` comment;
4. create tests.

Do not run the app as root.

Do not call `sudo`.

# 23. Testable architecture

Hide the main external integrations behind traits.

For example:

    trait DeviceDiscovery
    trait CaptureBackend
    trait MediaPipeline
    trait P2pConnector

This allows:

    MockDeviceDiscovery
    MockCaptureBackend
    MockMediaPipeline

`core` must be testable without:

- a TV;
- Wi-Fi;
- PipeWire;
- a display server.

# 24. Dummy mode

Implement a development mode:

    wireless-display --dummy

In this mode:

- real Wi-Fi P2P is not required;
- at least one fictional receiver is created;
- the UI can be tested;
- the state machine can be tested;
- connect/disconnect can be simulated;
- errors can be simulated.

If possible, also provide:

    wireless-display --diagnostics

and:

    wireless-display --version

Dummy mode will be used in CI.

# 25. Capability detection

Do not assume everything exists at startup.

Perform capability discovery.

Example:

    SystemCapabilities {
        session_type,
        portal_available,
        screencast_available,
        pipewire_available,
        network_manager_available,
        wifi_available,
        wifi_p2p_available,
        h264_encoders,
        audio_encoders,
    }

The app must open even when Miracast is unavailable on that computer.

In that case, show something like:

    Wireless Display unavailable

    Wi-Fi Direct was not detected on this system.

instead of crashing.

# 26. Performance

The video hot path must not run through frame-by-frame Rust loops.

Use:

    screen
      ↓
    PipeWire / X11 source
      ↓
    GStreamer
      ↓
    conversion
      ↓
    hardware/software encoder
      ↓
    RTP

Rust coordinates the pipeline.

Avoid copying large buffers between threads.

# 27. Architectural reference

You may study GNOME Network Displays to understand:

- overall flow;
- NetworkManager interaction;
- Wi-Fi P2P;
- Wireless Display;
- the GStreamer pipeline;
- LG webOS TV behavior.

However:

DO NOT copy GPL code into this project without an explicit compatible-licensing decision.

Use it as a behavioral and architectural reference.

Prefer:

- official NetworkManager documentation;
- official Wi-Fi Display documentation where available;
- GStreamer documentation;
- XDG Desktop Portal documentation;
- PipeWire documentation;
- gtk-rs;
- gstreamer-rs;
- zbus;
- ashpd.

# 28. Do not couple the app to Linux Mint

Although the first test computer may run Linux Mint/Cinnamon/X11, core must not
depend on Cinnamon.

Do not import Cinnamon APIs.

There may later be:

    Cinnamon Applet
          │
          │ D-Bus
          ▼
    WirelessDisplay

but it will be a separate component.

The main application must work independently.

# 29. Future application D-Bus API

This need not be a priority in the first milestone, but prepare the architecture
to eventually expose an equivalent of:

    org.example.WirelessDisplay

Methods:

    Scan()
    Connect(device_id)
    Disconnect()
    StartSharing()
    StopSharing()

Signals:

    DeviceAdded
    DeviceRemoved
    StateChanged

This will enable future development of:

- a Cinnamon applet;
- a GNOME extension;
- a KDE widget;
- a CLI.

Do not implement an HTTP API for this.

# 30. Tests

Write tests from the start.

Required:

- state-machine tests;
- NetworkManager type parsing;
- device-model validation;
- EncoderSelector;
- dummy backend;
- connection transitions;
- cancellation;
- RTSP parser if implemented.

Do not depend on hardware in unit tests.

Mark Linux-dependent integration tests appropriately.

# 31. Rust quality

Run at completion:

    cargo fmt --all --check

    cargo check --workspace

    cargo test --workspace

    cargo clippy --workspace --all-targets --all-features -- -D warnings

Fix all errors before finishing.

Do not suppress lints without justification.

Use:

- clear ownership;
- enums;
- RAII;
- strong types;
- zero-copy where reasonable;
- async only where it benefits I/O;
- messages instead of shared mutable state.

# 32. README

Write a README covering:

- purpose;
- experimental status;
- architecture overview;
- dependencies;
- native build;
- Flatpak build;
- running the app;
- dummy mode;
- diagnostics;
- current limitations;
- roadmap.

Make it very clear what works and what does not yet work.

# 33. ARCHITECTURE.md

Document:

    GTK Main Thread
          │
          │ commands/events
          ▼
    Backend Coordinator
          │
          ├── Portal Service
          ├── NetworkManager Service
          ├── Capture Service
          ├── Media Service
          └── Miracast Service
                     │
                     ▼
                  receiver

Explain ownership, threads, and lifecycle.

# 34. Documented roadmap

Divide future development into milestones.

Suggestion:

Milestone 1:

- GTK;
- Tokio architecture;
- diagnostics;
- P2P discovery;
- ScreenCast Portal;
- GStreamer test capture;
- Flatpak.

Milestone 2:

- P2P connection;
- WFD IEs;
- receiver filtering;
- networking tests.

Milestone 3:

- RTSP/WFD capability negotiation;
- H.264;
- RTP;
- first real video on a TV.

Milestone 4:

- audio;
- AAC/LPCM according to capabilities;
- A/V synchronization.

Milestone 5:

- hardware encoding;
- latency tuning;
- bitrate adaptation;
- reconnect.

Milestone 6:

- app D-Bus API;
- Cinnamon applet;
- optional KDE/GNOME integration.

# 35. Acceptance criteria

Consider the first implementation complete only if:

1. the workspace builds;
2. GTK opens;
3. dummy mode works;
4. the state machine works;
5. the Tokio backend runs separately from the GTK main thread;
6. UI/backend messaging works;
7. diagnostics work;
8. NetworkManager is queried through zbus;
9. Wi-Fi P2P can be detected;
10. P2P discovery has a real implementation where hardware supports it;
11. ScreenCast Portal has a real implementation;
12. users can select a display when the portal supports it;
13. the PipeWire FD/stream is obtained;
14. GStreamer can start a test capture pipeline;
15. X11 fallback is implemented or clearly isolated with a working implementation;
16. Wayland does not use unsafe capture/compositor bypass;
17. Flatpak has a manifest;
18. the README is accurate;
19. tests pass;
20. Clippy produces no warnings treated as errors.

The first milestone does NOT have to transmit video to a real TV.

However, the architecture must be ready for that next step without rewriting
the UI, threading, capture, or discovery.

# 36. Working method

Before changing code:

1. inspect the repository;
2. identify existing work;
3. preserve valid code;
4. create a brief implementation plan.

Then EXECUTE the plan.

Do not stop after planning.

Create files. Write code. Build. Run tests. Fix errors. Run Clippy.
Validate Flatpak if the tools are available.

If an API has changed since your knowledge base:

- consult current documentation;
- adapt to the actual API;
- do not invent methods.

If GTK/GLib/GStreamer crate versions are incompatible:

- resolve them using compatible version families;
- do not force conflicting versions.

If a system library is unavailable and blocks a specific test:

- continue implementing everything possible;
- document exactly which test could not run;
- do not replace the real implementation with fake code.

# 37. Recommended implementation order

Follow approximately this order:

    1. Cargo workspace
            ↓
    2. Core types + state machine
            ↓
    3. GTK shell
            ↓
    4. message bridge GTK ↔ Tokio
            ↓
    5. dummy backend
            ↓
    6. diagnostics
            ↓
    7. NetworkManager/zbus
            ↓
    8. Wi-Fi P2P discovery
            ↓
    9. ashpd ScreenCast
            ↓
    10. PipeWire → GStreamer test pipeline
            ↓
    11. X11 fallback
            ↓
    12. Flatpak
            ↓
    13. protocol/WFD scaffolding
            ↓
    14. tests/documentation
            ↓
    15. fmt/check/test/clippy

# 38. Final delivery

At completion, provide a report covering:

## Implemented

List the features actually implemented.

## Architecture

Briefly explain threads, runtime, GTK, NetworkManager, Portal, and GStreamer.

## Main files

Show the locations of:

- coordinator;
- state machine;
- GTK app;
- NetworkManager service;
- capture backends;
- GStreamer pipeline;
- Flatpak manifest.

## Tests run

Report the exact commands executed and their results.

## Actual functionality

Explicitly state:

- whether real discovery works;
- whether real screen capture works;
- whether PipeWire works;
- whether X11 fallback works;
- whether Flatpak was actually built or only its manifest was validated;
- whether real Miracast is implemented yet.

## Next milestone

Define concrete steps needed to achieve:

    Linux laptop
          ↓
    Wi-Fi Direct
          ↓
    WFD / RTSP
          ↓
    H.264 RTP
          ↓
    LG webOS TV

Do not claim completion without testing.

Prioritize architectural correctness, security, portability, and idiomatic Rust.
