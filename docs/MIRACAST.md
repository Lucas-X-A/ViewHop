# Wi-Fi Display — experimental video source

## Flow implemented in 0.2

1. Before P2P, obtain capture authorization, read the restricted stream's pixel
   geometry, and test encoders in the preferred mode and fallback. Obtain a new
   restricted PipeWire remote within the same session for streaming; do not
   reinitialize the protocol on a socket already consumed by the probe.
2. Activate the temporary connection and query the NM ActiveConnection's IP4Config.
   Wait for Ip4Config/AddressData publication through D-Bus signals for up to eight
   seconds, with cancellation. Activation can be announced before the address.
3. Open TCP 7236 only on the P2P group's IPv4 address. The TV (sink) connects to
   the laptop (source), as specified by WFD. Control never binds to 0.0.0.0 or the LAN address.
4. Send M1 OPTIONS, answer M2 OPTIONS, and query capabilities through M3
   GET_PARAMETER. Select a common profile, level, CEA/VESA mode, and UDP transport.
5. M4 SET_PARAMETER configures video, RTP, and the actual presentation URL.
   Omitting wfd_audio_codecs is intentional: this is a video-only session.
6. M5 requests SETUP. When M6 arrives, validate the URL/ports and create the
   pipeline/sockets. Respond with Session and the server's actual reserved ports.
7. On M7 PLAY, respond before starting UDP. The UI enters Streaming only once
   the pipeline has actually sent packets.
8. An empty GET_PARAMETER every five seconds keeps the session alive. Sink
   PAUSE/PLAY, IDR requests, and TEARDOWN are handled. CSeq and Session are validated.
9. On cancellation, network loss, IP changes, capture end, EOS, or error, stop
   video, send TEARDOWN when possible, close capture, and release P2P.

## Video and transport

The interface offers 720p/30 fps (the performance default), 720p/60 fps,
1080p/30 fps, and larger standard modes that fit current monitors: 1920×1200,
2560×1440, 2560×1600, 3840×2160, and 4096×2160. The choice applies to the next
connection. Authorized capture pixel geometry, encoder tests, and TV capabilities
limit the final mode; portal logical size is not treated as physical size.
Modes above Full HD query `wfd2_video_formats`, with 48-bit bitmaps and H.264 levels
up to 5.2. Legacy receivers continue using `wfd_video_formats`. This implements
R2 video negotiation, not all R2 features/certification. Arbitrary ultrawide
monitor modes outside this table are not offered.

Each encoder offer proves resolution, profile, level, and 720p fallback at the
same profile/level. Negotiation chooses the highest offer allowed by the preference.
Automatic fallback from higher-resolution modes preserves 30 fps, H.264 profile,
level, and the RTP session. The 720p/60 fps option is negotiated only at startup;
FPS does not change during a session. If the TV does not advertise CEA p60 or
level 3.2, negotiation uses 720p/30 fps. Preflight includes synthetic BGRx capture
at the actual desktop size, conversion/scaling, and at least 120 frames for p60;
encoders below 57 fps in that test are not offered at 60 fps. For sources whose
producer does not advertise 60/59.94 Hz, the preference also drops to 30. The
query uses supported caps, not the current cadence as a maximum. The p60 path
accepts 60/1 and 60000/1001 and does not duplicate frames from a slow source.
640×480p60 remains a last-resort compatibility mode for receivers that do not
advertise the main modes; it is not shown as a GUI preference.

The pipeline reduces frame rate before queuing/conversion/scaling, preserves
aspect ratio, and uses H.264, h264parse with SPS/PPS on IDRs, mpegtsmux, and
rtpmp2tpay. Frames are not copied into Rust. 720p/30 starts at 4 Mbit/s; 720p/60
uses 8–12 Mbit/s. Full HD starts at 8 Mbit/s and adjusts the target between 8 and
16 Mbit/s, increasing in 1 Mbit/s steps. These are encoder targets: peaks and
MPEG-TS/RTP/UDP overhead can exceed them.
Hardware retains priority. Among software encoders, x264 precedes OpenH264 and
uses superfast at 30 fps and at 720p/60, without B-frames. At 720p/60 it scales RGB
before color conversion and uses up to four frame threads without lookahead.
This can add about three frames (50 ms) of encoding delay in exchange for more
efficient compression than sliced-threads/ultrafast. Other modes retain their
previous processing order and configuration. Conversion/scaling uses two threads.
OpenH264 retains bitrate control and screen-content settings as an alternative.
The lighter preset prioritizes smoothness and can reduce compression efficiency
at the same bitrate. The advertised High profile is Restricted High (RHP), so
CABAC is disabled as required by WFD. Encoders without known bitrate control
that is mutable in PLAYING are not advertised by this path.

## In-session adaptation

The relevant bandwidth is the local Wi-Fi Direct link, not internet speed.
Every two seconds, media reports queue overload after videorate and loss from
fresh RTCP reports about the transmitted SSRC. The RTCP socket accepts only the
negotiated IP and port. Malformed packets, other SSRCs, and repeated/out-of-order
reports do not feed the controller. The GUI's RTP packet counter remains a
packet counter, not an FPS or bandwidth measurement. The GUI separately shows
H.264 access-unit FPS encoded on the laptop and local queue drops. This does
not measure FPS actually displayed on the TV.
INFO logs identify encoder/mode/target FPS and the cause of each fallback
request; DEBUG logs record input/encoded FPS, drops, and the fresh RTCP report's
loss percentage. `None` means no new report in that window, not zero loss.

Core reduces bitrate after two reports with at least 3.125% loss. In Full HD,
two consecutive windows of local pressure first reduce the target by 2 Mbit/s,
down to a minimum of 8 Mbit/s, even without RTCP. Pressure includes queue
overload or input of at least 29 fps with encoding below 28 fps. Three bad
reports already at minimum bitrate, or three local-overload windows at that
minimum, request 720p. In other modes, the three-window overload rule remains
independent of bitrate.

Five good reports (loss up to 0.78125%) allow a bitrate increase. In Full HD,
windows must also show input and encoding of at least 29 fps, with at most one
local drop per window. Static/slow capture or missing statistics do not prove
headroom for an increase and discard recovery history. Fifteen good reports
allow an attempt to recover the preferred mode. Each adjustment has a minimum
ten-second wait; missing feedback never triggers recovery. Ten seconds without
a fresh report discard loss history. Reaction times depend on the receiver's
RTCP interval.

A change prepares/blocks the first frame for the new format and computes its
MPEG-TS PTS/DTS. Core sends M4 with `wfd_av_format_change_timing` (33 bits in the
high bits of the 40-bit field) and waits for RTSP 200. Only then does media change
caps and request an IDR. The same capture, encoder, sockets, SSRC, and timeline
remain in use. TV rejection releases the frame without changing the format and
suspends further automatic resolution attempts for the session. Stopping sharing
releases any blocked frame before setting the pipeline to NULL.

If SETUP does not negotiate RTCP, the GUI reports the limitation: bitrate stays
conservative and downscaling under local overload remains active. A sent UDP
packet is not assumed to have reached the TV. In this case, receiver-only loss
cannot trigger network adaptation. TV rejection of renegotiation also limits
fallback; there is no silent reconnection or transmission of an unaccepted format.

MPEG-TS uses video PID 0x1011, PMT 0x1000, and video PCR. RTP uses payload type 33,
a 90 kHz clock, and MTU 1400. RTCP uses rtpbin when SETUP requests a port pair.
The second wfd_client_rtp_ports number represents another RTP stream, not RTCP.
Multiple streams are unsupported; RTCP is negotiated through client_port in SETUP.

## Security and limits

Headers are limited to 16 KiB and messages/buffers to 64 KiB. Transactions have
timeouts and validate CSeq, Session, URI, and Transport. No multicast, TCP
interleaving, privileged ports, or arbitrary destination. Video is sent to the TCP peer.

The listener accepts only the P2P gateway when NM supplies it; if the laptop is
the GO, it accepts peers from the group subnet. It does not infer IPs from MACs.
Link identity follows NM's authenticated group formation; WFD/RTSP adds no TLS.
Session identifies only the current TCP connection and does not authorize another one.

No audio, HDCP, UIBC, MPEG-TS/TCP, EDID/preferred-display-mode, other R2 features,
or automatic reconnection. Advertising HDCP support does not require using it:
the source selects unprotected video. If the TV requires protection, the session
fails. There is no attempt to bypass content protection.

## Verification and interoperability

A synthetic receiver covers M1–M7, SETUP/PLAY, RTP, keepalive, PAUSE/PLAY, and
TEARDOWN. Other tests cover cancellation, mismatched CSeq, and redirection
rejection. A separate GStreamer receiver depacketizes RTP, demultiplexes MPEG-TS,
and decodes real H.264 frames. Tests verify Sender/Receiver Reports, automatic
RTCP-triggered fallback, M4 rejection/acceptance, 1080p/720p/1080p with both
software encoders, the first IDR, and exact PES PTS. This does not replace physical
TV testing; modes above Full HD still need compatible-hardware validation.

References: [Miracast Specification v2.3](https://www.wi-fi.org/system/files/Miracast_Specification_v2.3.pdf)
(Wi-Fi Alliance, 2024), §§4.10.3.2, 5.4.1, 6.1.13, and 6.1.22; NetworkManager
D-Bus IP4Config/ActiveConnection, GStreamer mpegtsmux/rtpbin, and gstreamer-rs 0.25 /
gio 0.22. No GPL code from another Miracast source was copied.
