# Architecture

ViewHop is an experimental Linux Miracast video source; its current code and
runtime identifiers still use WirelessDisplay/wireless-display. Rust coordinates
GTK, capture, Wi-Fi Direct, RTSP, and native GStreamer pipelines. Audio streaming
is not implemented; GPU processing is optional and selected through preflight.

```text
GTK Main Thread / GLib Main Loop
    ▼ commands: tokio::sync::mpsc<AppCommand> (capacity 32)
    ▲ events: async_channel<AppEvent> (capacity 256)
    │
Backend Coordinator / two-worker Tokio runtime on a separate bootstrap thread
    ├── Discovery job → NetworkManagerService / zbus / system bus
    ├── Local capture-test job → capture lease + GStreamer fakesink
    ├── Diagnostics job → platform/media capabilities
    └── Connection job / app::mirroring
        ├── SelectedCapture / ashpd portal or X11 source
        ├── P2pConnection / NetworkManager activation and monitoring
        ├── WfdSource / core RTSP M1–M7 and session control
        └── GstMediaPipeline / GStreamer's internal media threads
```

## Workspace boundaries

| Crate | Responsibility |
| --- | --- |
| [core](crates/core/src/lib.rs) | Domain/state types, capture/network traits, RTSP/WFD parsing and negotiation, adaptive quality policy, and mocks. |
| [platform](crates/platform/src/lib.rs) | Desktop detection, ScreenCast Portal/session ownership, X11 fallback, and NetworkManager discovery/activation/monitoring. |
| [media](crates/media/src/lib.rs) | Capture consumption, encoder settings, CPU/GPU processing, preflight, RTP/RTCP sockets, telemetry, and pipeline lifecycle. |
| [app](crates/app/src/lib.rs) | GTK presentation, backend jobs, diagnostics, and coordination between protocol, platform, and media. |

## Ownership and invariants

GTK exists only in `app::ui` and the entry point. `Backend::spawn` creates a
bootstrap thread that builds the Tokio runtime. No widget enters a Tokio task,
message, or `Arc<Mutex<_>>`. GTK consumes events with
`glib::MainContext::spawn_local`; callbacks send commands without blocking the loop.

Coordinator owns SessionMachine, the discovered-peer map, and job lifecycles.
The real connection job in `app::mirroring` owns the capture lease and P2P
connection; its session future owns protocol/media control and native pipelines.
The UI renders `AppState` snapshots and does not perform connection transitions
on its own. Local capture and discovery are explicitly separate from connection
state. Dummy mode uses `simulated: true`. A real session enters Streaming only
after PLAY and actual RTP packet transmission; PAUSE produces Paused.
Negotiation failures release resources and produce Error, without simulating success.

Each long operation is a Job with a CancellationToken and a tracked JoinHandle.
Discovery uses JoinSet internally for multiple adapters. Discovery, capture,
and connection events carry a generation; stale results cannot resurrect a session.
Backpressure uses bounded channels. There are no Rust frame queues.

`core` defines DeviceDiscovery, CaptureBackend/CaptureLease, P2pConnector,
MediaPipeline, and AudioCapture. Asynchronous traits return Send futures;
media lifecycle and audio capability queries are synchronous. None depend on
GTK, zbus, ashpd, or GStreamer. The project does not require async-trait. Dummy
discovery/connections use mocks; local dummy capture uses videotestsrc. Separate
protocol/transport integration tests use real loopback sockets and synthetic video.
Workspace lints forbid project-owned unsafe Rust; native libraries are used
through their bindings.

## Capture

GDK provides an active-backend hint; XDG variables and DISPLAY/WAYLAND_DISPLAY
provide fallback/diagnostic information. The portal takes priority in every
session. Only absent ScreenCast support allows X11 fallback. Denying/cancelling
selection does not allow fallback. A Wayland session never uses `ximagesrc`,
even if XWayland provides DISPLAY.

An X11 GDK hint cannot override a known Wayland session. GPU-processing fallback
reuses the selected capture backend; it does not bypass portal denial or cancellation.

PortalCaptureBackend creates a dedicated D-Bus connection, ashpd session,
monitor selection, Start, and OpenPipeWireRemote. The Start response uses a small
zbus bridge because ashpd 0.13 does not expose `pipewire-serial`, added in portal
v6. The bridge subscribes to the exact Request path before the call. It does not
collect other applications' signals. The portal's opaque `id` field is not used as a serial.

SelectedCapture owns the session and remote FD; CaptureSource shares FD ownership
with the pipeline through Arc<OwnedFd>. `pipewiresrc` receives that FD and the
serial or node ID, without access to the user's full set of nodes. `target-object`
is required when the portal supplies a serial. The pipeline is set to NULL before
the session is closed and the FD released. Cancellation closes sessions even during selection.

The local capture test consumes the selected source through conversion/scaling
into fakesink, without encoding or network transport. Its CaptureStarted event
requires a consumed frame. Streaming instead announces CaptureStarted after
PLAY and an increase in transmitted RTP packets. GstBus uses an asynchronous
stream without blocking GTK. The running media pipeline has a 15-second startup
timeout; once started, sparse frames are not interpreted as a failure. Geometry
and encoder/import preflight have their own shorter deadlines.

## Preparation and media processing

`app::mirroring::prepare_encoders` completes preparation before P2P activation:

1. Consume the authorized source to obtain physical pixel geometry and supported
   frame-rate caps. A source without p60/59.94 support limits a 60 fps request to
   720p/30 before negotiation.
2. Use cancellable spawn_blocking work to test encoder/processing candidates with
   synthetic RGB input at that geometry. Test the preferred mode and compatible
   720p fallback/recovery with the same H.264 profile/level. Full HD also exercises
   live 8/16 Mbit/s targets; 720p/60 requires at least 120 frames and 57 encoded fps.
3. For GPU candidates, encode one frame from the authorized capture into fakesink.
   PipeWire first tries explicit DMABuf caps, then system-memory capture/upload.
   X11 uses system-memory capture. Import validation is reused for matching
   encoder/processing pairs; a sparse desktop needs only one real encoded frame.
4. Exclude any rejected encoder/GPU-path pair for this preparation and retest
   candidates. Other GPU paths, CPU processing with hardware encoding, and
   x264/OpenH264 remain available. Renew the restricted portal remote after every
   consumed capture probe and once more for streaming, in the same authorized
   session and D-Bus owner connection.

The encoder module owns per-backend low-latency settings, GOP/B-frame controls,
Restricted High CABAC constraints, and bitrate units/PLAYING mutability. Input
selection prefers a supported NV12 format for hardware and I420 for software.
Both are 8-bit 4:2:0; the chosen format is tested rather than inferred from the
presence of a factory. The processing module owns conversion/scaling and pacing:

| Processing path | Elements | Required encoder-input memory |
| --- | --- | --- |
| CPU | videoconvert/videoscale; RGB scaling first at 720p/60 | SystemMemory |
| VA, including compatible QSV encoders | vapostproc | VAMemory |
| Legacy VA-API | vaapipostproc | VASurface |
| CUDA/NVENC | cudaupload + cudaconvertscale | CUDAMemory |

Optional plugins, caps, driver behavior, and successful probes determine the
path. No CPU vendor/model or CPU-specific compiler flag is required. GPU output
caps require GPU surfaces at encoder input; the graph does not insert a download
between GPU processing and encoding. Aspect ratio is preserved in every path.

The streaming graph is assembled from element factories, never by interpolating
network strings into a pipeline description:

```text
authorized source
  → videorate → capture-memory/frame-rate caps → leaky queue (two frames)
  → selected CPU/GPU conversion and scaling
  → tested pixel-format/geometry/encoder-memory caps → H.264 encoder → h264parse
  → mpegtsmux → rtpmp2tpay → optional rtpbin → udpsink
```

Rust does not map or copy video pixels. Native GPU drivers can still allocate
additional surfaces or perform internal copies; successful DMA-BUF import does
not guarantee dedicated VRAM, lower whole-system RAM, or a specific energy gain.
INFO logs identify the configured path, validated capture-memory contract, and
negotiated capture/encoder caps. These logs contain metadata, not frame contents.
The selected path remains fixed during a session. Runtime pipeline failure ends
sharing; preflight fallback does not imply live encoder replacement or automatic
reconnection. Hardware-specific validation is described in [TESTING.md](docs/TESTING.md).

## Networking and protocol

NetworkManagerService uses typed D-Bus proxies. Discovery subscribes to PeerAdded,
PeerRemoved, and PropertiesChanged before listing peers/StartFind. WFD, name,
signal-strength, and availability changes update existing peers; property signals
are restricted to NetworkManager's unique owner and the observed adapter's peers.
GTK updates existing rows without recreating buttons or moving them on each
advertisement. WfdIEs uses subelements with validated big-endian lengths. PeerType
separates unknown P2P peers from advertised WFD sinks. Normal logs omit MAC
addresses; names are length-limited and stripped of control characters.

Each adapter's discovery loop is bounded to 120 seconds; cleanup awaits StopFind.
Ending or restarting discovery invalidates the unmonitored device list. Connecting retains
a selected-peer snapshot, stops discovery, then revalidates identity, presence,
and current WFD availability through NetworkManager before activation.

Activation uses `wifi-p2p`, the peer address, the WFD source IE, and
AddAndActivateConnection2 with `persist=volatile` and `bind-activation=dbus-client`.
A dedicated D-Bus connection guarantees release if cancellation races with the
activation response. Normal disconnection also calls DeactivateConnection.
Privileged execution is not required; NetworkManager/Polkit decide authorization.

RTSP has an incremental parser, bounded headers/messages/buffers, validated CSeq,
overflow-safe decimal Content-Length, and rejection of duplicates and invalid
header UTF-8. Bodies remain bytes. TCP transport has total timeouts and cancellation.
`core::protocol::wfd::source` executes M1–M7, keepalive, and teardown. MediaRequest
with oneshot acknowledgments confirms media configuration/control without depending
on GStreamer. `app::mirroring` coordinates this protocol, capture, pipeline, and
NM monitor as tracked futures within the Coordinator's connection job. See
[MIRACAST.md](docs/MIRACAST.md).

After capture/encoder preparation and P2P activation, the job queries the group's
IPv4/prefix/gateway, opens RTSP 7236 only on that IPv4 address, and filters peers
by group. It never guesses an IP from a MAC. The TV opens TCP; the source starts OPTIONS and capability
exchange. NM's active state does not guarantee immediate AddressData publication.
Preparation waits for Ip4Config/address D-Bus signals with a total eight-second
deadline and cancellation. After the session starts, the network monitor cancels
sharing on loss or change of IPv4, without reusing the initial waiting period.
RTP/RTCP sockets are reserved and passed to GStreamer as GSocket through OwnedFd
ownership, without project-owned unsafe code or a close/reopen port race.
RTCP source metadata is checked against the RTSP peer's IP before telemetry and
rtpbin. The negotiated receive port remains the outgoing destination; the TV may
send feedback from a different port on that IP. Missing sender metadata and
unrelated IPs are rejected. Compound RTCP bounds, the transmitted SSRC, and fresh
report sequence numbers are checked before loss reports feed adaptive quality.
The app does not open firewall ports or equate RTCP negotiation with received feedback.

The selected pipeline uses H.264, MPEG-TS, and RTP payload 33, with optional RTCP.
Local capture frames, encoded access-unit FPS, queue drops, and sent RTP packets
have separate events/counters. They do not measure frames displayed on the TV.
The backend observes NM State, IP4Config, AddressData, and Gateway through
signals. Network loss, IP changes, portal closure, or GstBus errors stop sharing.
Cancellation stops video
immediately before waiting for bounded TEARDOWN.

## Video preference and adaptation

The GUI sends `Connect(DeviceId, VideoMode)` through the existing channel. GDK
only limits visible choices; media verifies real pixels in the authorized stream.
`EncoderChoice` retains the encoder name, tested input format, processing path,
capture-memory contract, and VideoOffer. Higher 30 fps modes can offer 720p/30
fallback with the same profile/level; the 60 fps preference is negotiated at
startup and does not authorize live FPS changes. Core intersects offers with
the TV's legacy or R2 CEA/VESA capabilities. A resolution is not advertised after
only a 720p test.

AdaptiveQuality consumes two-second observations. Full HD starts at 8 Mbit/s and
can rise to 16 Mbit/s in 1 Mbit/s steps after fresh favorable RTCP reports and
sufficient local frame headroom. Sustained loss or processing pressure can
reduce bitrate and request the negotiated resolution fallback. Local overload
can trigger a reduction without RTCP; missing feedback cannot justify an
increase. These targets concern the Wi-Fi Direct link, not internet speed.

`core::protocol::wfd::quality::AdaptiveQuality` chooses targets from typed
observations without depending on GStreamer or GTK. `media::quality` measures
queues and RTCP and controls bitrate/caps. App retains session ownership and
sends protocol requests through a bounded channel. `WfdSource` remains the sole
RTSP writer and validates every resolution change against the negotiated offer.
Media prepares a frame; RTSP M4 acknowledges the change; media applies caps/IDR and
releases the frame. Rejections preserve the previous stream. Blocking uses RAII
and is also released by `stop()`, even if cancellation races with renegotiation.
The prepared frame's MPEG-TS timestamp is included in the resolution-change
request. StreamingControl retains the pixel format and processing memory features
when applying new geometry. A rejected resolution change preserves the current
stream and disables further automatic resolution attempts for that connection.
Bitrate-only changes apply through the live encoder setter without RTSP format
renegotiation; the controller commits them only after the setter succeeds.
Core's WFD offer and quality policy remain independent of GStreamer/raw pixel types.

`StreamNegotiated` updates the GUI after initial negotiation, successful bitrate
updates, and acknowledged resolution changes. The displayed bitrate is a target,
not measured total network traffic. Missing RTCP does not mean zero loss.
Hysteresis, limits, and behavior without feedback are detailed in
[docs/MIRACAST.md](docs/MIRACAST.md).

## Shutdown

Closing the window keeps the GTK loop running until ShutdownComplete. The backend:

1. Cancels discovery and waits for the adapters' StopFind.
2. Cancels the ongoing connection/sharing job.
3. The job sets the pipeline to NULL, performs bounded TEARDOWN, closes Portal/FD,
   and deactivates P2P.
4. Stops any local capture test, independently of dummy mode.
5. Cancels/awaits diagnostics and every tracked task.
6. Emits ShutdownComplete and exits the runtime; main joins the thread after GTK exits.

Jobs have a 20-second cleanup deadline; as a last resort, their asynchronous tasks
are aborted and awaited. RAII releases resources owned by dropped futures.
Blocking probes check cancellation cooperatively and are normally awaited;
aborting an async task cannot forcibly stop an already running native call or
spawn_blocking closure. Dropping a live JoinHandle is not a normal shutdown mechanism.

## Identity and native execution

APP_NAME and APP_ID are in [core/src/lib.rs](crates/core/src/lib.rs).
Renaming also requires synchronizing
names/content in data/ and metadata tests. Services do not
hard-code a distribution or compositor name. A future D-Bus API can translate
methods into AppCommand and observe AppEvent without coupling widgets.

The README presents the project as ViewHop, while APP_NAME is still
`WirelessDisplay`, APP_ID is `org.example.WirelessDisplay`, and the executable is
`wireless-display`. These are the implemented identifiers, not a completed rename.
The `--diagnostics` entry point creates its own Tokio runtime and reports
capabilities without constructing a GTK application.

The application builds and runs natively against the host's GTK and GStreamer
libraries. Cargo.lock pins Rust dependencies. NetworkManager/Polkit control
network authorization, and the desktop portal controls capture authorization.
Native GPU availability depends on installed plugins and compatible drivers;
there is no mandatory VA/CUDA Rust dependency.

AudioCapture currently reports capabilities only. Audio streaming, HDCP, UIBC,
IPv6 WFD, TCP media transport, automatic reconnection, and an exported application
D-Bus API are not implemented. Implemented R2 video modes do not imply complete
Miracast R2 support. Physical GPU/DMA-BUF interoperability and resource savings
require target-hardware validation; see [TESTING.md](docs/TESTING.md),
[MIRACAST.md](docs/MIRACAST.md), and [VALIDATION.md](docs/VALIDATION.md).
