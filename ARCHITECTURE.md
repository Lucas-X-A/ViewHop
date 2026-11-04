# Architecture

```text
GTK Main Thread / GLib Main Loop
    │ tokio::mpsc<AppCommand> (bounded)
    │ async_channel<AppEvent> (bounded)
    ▼
Backend Coordinator / Tokio multithread on a separate bootstrap thread
    ├── NetworkManagerService / zbus / system bus
    ├── PortalCaptureBackend / ashpd / session bus
    ├── X11CaptureBackend / only when ScreenCast is unavailable
    ├── GstMediaPipeline / GStreamer's internal threads
    └── WFD / RTSP / M1–M7 source and session control
```

## Ownership and invariants

GTK exists only in `app::ui` and the entry point. `Backend::spawn` creates a
bootstrap thread that builds the Tokio runtime. No widget enters a Tokio task,
message, or `Arc<Mutex<_>>`. GTK consumes events with
`glib::MainContext::spawn_local`; callbacks send commands without blocking the loop.

Coordinator is the sole owner of SessionMachine, peers, jobs, and the P2P session.
The UI renders `AppState` snapshots and does not perform connection transitions
on its own. Local capture and discovery are explicitly separate from connection
state. Dummy mode uses `simulated: true`. A real session enters Streaming only
after PLAY and actual RTP packet transmission; PAUSE produces Paused.
Negotiation failures release resources and produce Error, without simulating success.

Each long operation is a Job with a CancellationToken and a tracked JoinHandle.
Discovery uses JoinSet internally for multiple adapters. Events carry a
generation; results from cancelled operations cannot resurrect a session.
Backpressure uses bounded channels. There are no Rust frame queues.

`core` defines DeviceDiscovery, CaptureBackend/CaptureLease, P2pConnector,
MediaPipeline, and AudioCapture. These traits use ownership and Send futures;
none depend on GTK, zbus, ashpd, or GStreamer. The project does not require
async-trait. Mocks exercise the domain and Coordinator without a display, TV, or network.

## Capture

GDK provides an active-backend hint; XDG variables and DISPLAY/WAYLAND_DISPLAY
provide fallback/diagnostic information. The portal takes priority in every
session. Only absent ScreenCast support allows X11 fallback. Denying/cancelling
selection does not allow fallback. A Wayland session never uses `ximagesrc`,
even if XWayland provides DISPLAY.

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

GstMediaPipeline builds elements through factories; network strings never become
pipeline descriptions. GstBus uses an asynchronous stream without blocking GTK.
Rendered frames come from sink statistics; CaptureStarted requires at least one
consumed frame. If no first frame arrives within 15 seconds, capture fails and
cleans up; once started, sparse frames are not interpreted as a failure.

## Networking and protocol

NetworkManagerService uses typed D-Bus proxies. Discovery subscribes to PeerAdded,
PeerRemoved, and PropertiesChanged before listing peers/StartFind. WFD, name,
signal-strength, and availability changes update existing peers; property signals
are restricted to NetworkManager's unique owner and the observed adapter's peers.
GTK updates existing rows without recreating buttons or moving them on each
advertisement. WfdIEs uses subelements with validated big-endian lengths. PeerType
separates unknown P2P peers from advertised WFD sinks. Normal logs omit MAC
addresses; names are length-limited and stripped of control characters.

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
NM monitor as tracked futures within the Coordinator's connection job. See docs/MIRACAST.md.

Before activating P2P, the job authorizes capture, reads its physical geometry,
and tests encoders in the selected format and fallback (cooperatively cancellable
spawn_blocking). After the geometry probe, the portal supplies a new restricted
remote within the same session/D-Bus connection; a consumed PipeWire FD is not
reused as a new connection. The job then queries the group's IPv4/prefix/gateway,
opens RTSP 7236 only on that IPv4 address, and filters peers by group. It never
guesses an IP from a MAC. The TV opens TCP; the source starts OPTIONS and capability
exchange. NM's active state does not guarantee immediate AddressData publication.
Preparation waits for Ip4Config/address D-Bus signals with a total eight-second
deadline and cancellation. After the session starts, loss or change of IPv4 stops
sharing immediately, without reusing the initial waiting period.
RTP/RTCP sockets are reserved and passed to GStreamer as GSocket through OwnedFd
ownership, without project-owned unsafe code or a close/reopen port race.
RTCP source metadata is checked against the RTSP peer's IP before telemetry and
rtpbin. The negotiated receive port remains the outgoing destination; the TV may
send feedback from a different port on that IP.

The selected pipeline uses H.264, MPEG-TS, and RTP payload 33, with optional RTCP.
Local frames and sent packets have separate events/counters. The backend observes
NM State, IP4Config, AddressData, and Gateway through signals. Network loss, IP
changes, portal closure, or GstBus errors stop sharing. Cancellation stops video
immediately before waiting for bounded TEARDOWN.

## Video preference and adaptation

The GUI sends `Connect(DeviceId, VideoMode)` through the existing channel. GDK
only limits visible choices; media verifies real pixels in the authorized stream.
`EncoderChoice` contains a `VideoOffer` tested in the preferred mode and at 720p,
using the same profile/level. Core intersects these offers with the TV's legacy
or R2 CEA/VESA capabilities. A resolution is not advertised after only a 720p test.

`core::protocol::wfd::quality::AdaptiveQuality` chooses targets from typed
observations without depending on GStreamer or GTK. `media::quality` measures
queues and RTCP and controls bitrate/caps. App retains session ownership and
sends protocol requests through a bounded channel. `WfdSource` remains the sole
RTSP writer and validates every change against the negotiated offer. Media
prepares a frame; RTSP M4 acknowledges the change; media applies caps/IDR and
releases the frame. Rejections preserve the previous stream. Blocking uses RAII
and is also released by `stop()`, even if cancellation races with renegotiation.

Media's encoder module owns per-backend settings, bitrate units/mutability, and
selection of a supported I420/NV12 input. EncoderChoice retains the tested input;
StreamingControl preserves it when changing resolution. Core's WFD offer and
adaptive bitrate policy remain independent of raw pixel formats and GStreamer.

Media's processing module owns optional VA/legacy VA-API/CUDA conversion/scaling,
GPU caps, and capture pacing. EncoderChoice retains the tested processing and
capture-memory contracts; resolution changes preserve GPU memory features.
App renews restricted portal remotes while validating GPU import on authorized
capture before P2P. Failed encoder/path pairs are excluded only for this
preparation, with CPU/software alternatives retained. No CPU vendor is required.

`StreamNegotiated` updates the GUI's resolution and bitrate target after initial
negotiation and acknowledged adjustments. Missing RTCP does not mean zero loss.
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

Jobs have a 20-second cleanup deadline; as a last resort, they are aborted and
awaited, with RAII releasing native objects and D-Bus connections. Dropping a live
JoinHandle is not a normal shutdown mechanism.

## Identity and packaging

APP_NAME and APP_ID are in `core/src/lib.rs`. Renaming also requires synchronizing
names/content in data/, the Flatpak manifest, and metadata tests. Services do not
hard-code a distribution or compositor name. A future D-Bus API can translate
methods into AppCommand and observe AppEvent without coupling widgets.
