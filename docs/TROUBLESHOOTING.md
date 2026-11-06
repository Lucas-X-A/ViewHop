# Troubleshooting

Run `wireless-display --diagnostics`. No GTK window is required.

**Wi-Fi P2P not detected:** verify adapter/driver support and whether
NetworkManager manages the device. DeviceType 30 identifies WifiP2P; a regular
Wi-Fi interface does not prove P2P support. Check that NetworkManager is running
and the user is authorized through Polkit. Do not run as root or disable Polkit
to bypass authorization errors.

**Portal without ScreenCast:** install the backend appropriate for the compositor.
Having xdg-desktop-portal or xdg-desktop-portal-gtk does not guarantee the ScreenCast
interface. This prevents capture on Wayland; X11 can use ximagesrc. Cancelling
selection does not activate another backend. An available portal with an operational
failure is reported as an error rather than ignoring the user's choice.

**PipeWire unavailable:** diagnostics check socket visibility, not video consumption.
Portal capture uses a restricted FD. The frame counter after monitor selection
proves consumption.
Install the `pipewiresrc` plugin if missing.

**Source removed/monitor disconnected:** GstBus errors/EOS or Portal Closed end
capture. No first frame within 15 seconds causes a timeout. Select the source
again; this version does not reconnect automatically.

**ximagesrc missing:** install gst-plugins-good. This plugin is never selected on
Wayland, even with XWayland active.

**No H.264 encoder:** this does not prevent the UI, discovery, or fakesink capture.
EncoderSelector enumerates factories. Before connecting, streaming tests encoding
per profile; debug logs identify rejected candidates.

**OpenH264 `cmInitParaError`:** the library can also emit this message when
finalizing a probe that produced valid video. Alone, it does not identify the
connection failure. If `Wi-Fi Direct active` appears afterward, the encoder test
passed. Check the last stage and backend error in the terminal before changing
plugins or versions.

**P2P active without a TV picture (0.2):** check the state and RTP packets sent.
If negotiation persists, the TV may not have opened TCP 7236, may have rejected
an RTSP parameter, or may have requested an incompatible feature. Check the
firewall on the P2P interface; the app does not open firewall ports. In Streaming,
record resolution/profile/encoder and debug logs to investigate interoperability.
Audio is not expected. The app confirms transmission, not that the TV displayed frames.

**P2P active, waiting for IPv4:** NetworkManager can announce activation before
publishing the group address. The app waits for publication through D-Bus signals
for up to eight seconds; Stop sharing also cancels the wait. If the deadline
expires, the error specifically reports that IPv4 was not published. A fixed IP
or regular Wi-Fi network address is not used to bypass missing group configuration.

**UFW blocking RTSP control:** look for a kernel journal entry containing
`[UFW BLOCK]`, the P2P interface, `PROTO=TCP`, `DPT=7236`, and `SYN`. The TV closes
the group when it cannot open control; teardown can also produce an address-change
message in the app. This condition was confirmed on the development host after
delayed IPv4 publication was fixed.

On that host, the TV used 192.168.49.1, the laptop received 192.168.49.10, and group
interfaces had the prefix p2p-wlp0s20-. The administrator can allow only that TV's
RTSP control on that adapter with:

```sh
sudo ufw allow in on p2p-wlp0s20-+ proto tcp from 192.168.49.1 to 192.168.49.10 port 7236 comment 'WirelessDisplay RTSP P2P'
```

The trailing `+` covers the numeric suffix that changes on each group formation.
The rule persists in UFW; to remove it, use:

```sh
sudo ufw delete allow in on p2p-wlp0s20-+ proto tcp from 192.168.49.1 to 192.168.49.10 port 7236 comment 'WirelessDisplay RTSP P2P'
```

These addresses and prefix belong to the observed case. On another host/group,
check the actual interface and IPv4 before adapting the rule. The app does not
run these commands or change the firewall. The firewall remains active.

**TV stutters while encoding stays near 30 fps:** local encoded FPS does not
measure playback on the receiver. Check fresh RTCP loss in DEBUG statistics.
`loss_percent=None` means no new valid report, not zero loss. RTCP negotiation
alone does not prove that feedback reaches the app.

If the kernel journal reports `[UFW BLOCK]` with the TV's IP, `PROTO=UDP`, and
the current local RTCP destination port, feedback is blocked by the firewall.
The TV may send it from a different port than its advertised receive port, so
outbound RTCP does not necessarily create a matching firewall return flow.
An administrator can allow inbound UDP restricted to the current P2P interface,
TV IP, local group IP, and reserved local RTCP port. Do not require the TV's
source port to match its advertised receive port. The local port and group
interface can change each session; remove a temporary rule after the test.
The app does not modify UFW. Allowing RTCP enables measurement and adaptation;
it does not by itself establish or fix the cause of receiver stutters.

**Encoder unavailable:** having a factory in the registry is not enough. Before
P2P, the app tests encoding per profile and can discard unusable hardware encoders.
Install a compatible encoder (for example, OpenH264 or x264) as a fallback.

**Activation failure / peer-not-found:** ActiveConnection state 4 means the
connection was deactivated; that number alone does not identify the cause.
NetworkManager can end activation because the peer is no longer visible.
Prototype discovery lasts 120 seconds. When it ends, results are removed so
expired TVs are not presented as available. Presence, identity, and WFD
availability are revalidated before creating a temporary connection.

Keep Screen Share open on the TV, click **Search for devices**, and connect when
the TV reappears in the current search. If necessary, close and reopen Screen
Share to restart its advertisement. After failure, Search for devices is
available for another attempt. The peer-not-found error occurs before WFD
negotiation and is not fixed by changing encoders or upgrading GStreamer.

**TV availability does not update:** discovery also observes peer property changes.
WfdIEs can arrive after PeerAdded, and opening/closing Screen Share can change
availability without creating another peer. The row and connection button update
in place. Presence in the list does not guarantee the link remains available
throughout connection; pre-activation revalidation remains in place.

**IPv4 removed after WFD negotiation:** IPv4 configuration loss has a different
message from an address change. NetworkManager can remove Ip4Config before
publishing the disconnected state. Check the order in the NetworkManager,
wpa_supplicant, and kernel journals. In the 2026-10-25 test at 00:16 UTC, the TV
opened RTSP and negotiated Full HD at 8 Mbit/s; the iwlwifi driver then logged
`missed beacons exceeds threshold` and `Connection to AP ... lost`. The supplicant
reported a local disconnection, reason 4, and removed the group for inactivity.
This proves link loss before the IPv4 error, without establishing why the beacons
were lost. Refreshing the list or increasing the bitrate ceiling does not fix
this radio failure. The app ends transmission when the group loses its IP;
it does not ignore the loss or change drivers, power saving, or the firewall.

Detailed logs: `RUST_LOG=wireless_display=debug wireless-display`. Before sharing
logs, check local paths and library error details. Backend errors and major
connection stages are also logged at the default level, in addition to window
messages. These logs omit RTSP message bodies and peer addresses.
