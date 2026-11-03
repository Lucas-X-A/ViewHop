use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use wireless_display_core::protocol::wfd::quality::{AdaptiveQuality, QualityTarget};
use wireless_display_core::protocol::wfd::source::{self, MediaRequest, QualityRequest, WfdSource};
use wireless_display_core::protocol::wfd::video::{P2pNetwork, StreamingInfo, VideoMode};
use wireless_display_core::traits::{CaptureLease, MediaPipeline, P2pConnector};
use wireless_display_core::{
    CaptureInfo, CaptureRequest, CaptureSource, MediaEvent, VideoStatistics, WirelessDisplayDevice,
};
use wireless_display_media as media;
use wireless_display_media::{
    EncoderChoice, GstMediaPipeline, MediaError, PreparedFormat, RtpSockets,
};
use wireless_display_platform::capture::{self, PortalSession, SelectedCapture};
use wireless_display_platform::environment::DesktopEnvironment;
use wireless_display_platform::network::{NetworkManagerService, P2pConnection};

pub(crate) enum Event {
    SourceSelected(CaptureInfo),
    Connected,
    Negotiated(StreamingInfo),
    Playing(CaptureInfo),
    Paused,
    Media(MediaEvent),
}

async fn emit(tx: &async_channel::Sender<Event>, event: Event, cancel: &CancellationToken) {
    tokio::select! { _ = cancel.cancelled() => {}, _ = tx.send(event) => {} }
}

pub(crate) async fn run(
    peer: WirelessDisplayDevice,
    environment: DesktopEnvironment,
    preferred: VideoMode,
    events: async_channel::Sender<Event>,
    cancel: CancellationToken,
) -> Result<(), String> {
    tracing::info!(target: "wireless_display::miracast", "Testing H.264 encoders before activating Wi-Fi Direct");
    let mut lease = capture::select_capture(&environment, CaptureRequest, cancel.clone())
        .await
        .map_err(|e| e.to_string())?;
    emit(&events, Event::SourceSelected(lease.info()), &cancel).await;
    let probe_token = cancel.clone();
    let source = lease.source();
    let prepared = tokio::task::spawn_blocking(move || {
        let capture = media::capture_format(source, &probe_token)?;
        let selected = if preferred == VideoMode::Hd60 && !capture.supports_60fps {
            VideoMode::Hd
        } else {
            preferred
        };
        media::probe_encoders(selected, capture.pixels, &probe_token)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|result| result.map_err(|e| e.to_string()));
    let choices = match prepared {
        Ok(choices) => choices,
        Err(error) => {
            let _ = lease.close().await;
            return if cancel.is_cancelled() {
                Ok(())
            } else {
                Err(error)
            };
        }
    };
    if let Err(error) = lease.reopen_remote(&cancel).await {
        let _ = lease.close().await;
        return if cancel.is_cancelled() {
            Ok(())
        } else {
            Err(error.to_string())
        };
    }
    tracing::debug!(target: "wireless_display::miracast", ?choices, "Validated H.264 encoders");
    if choices
        .first()
        .is_some_and(|choice| choice.offer.preferred.mode < preferred)
    {
        emit(
            &events,
            Event::Media(MediaEvent::Warning(
                "The selected display or encoder limits the requested resolution or frame rate."
                    .into(),
            )),
            &cancel,
        )
        .await;
    }
    let connected = async {
        let service = NetworkManagerService::try_new()
            .await
            .map_err(|e| e.to_string())?;
        service
            .connect(&peer, cancel.clone())
            .await
            .map_err(|e| e.to_string())
    }
    .await;
    let connection = match connected {
        Ok(connection) => connection,
        Err(error) => {
            let _ = lease.close().await;
            return if cancel.is_cancelled() {
                Ok(())
            } else {
                Err(error)
            };
        }
    };
    tracing::info!(target: "wireless_display::miracast", "Wi-Fi Direct active; waiting for the P2P group IPv4 address");
    let result = connected_session(&connection, &lease, &choices, events, cancel.clone()).await;
    // Session owns protocol/media; they have stopped before the portal and P2P.
    let closed = lease.close().await.map_err(|e| e.to_string());
    let disconnected = connection.disconnect().await.map_err(|e| e.to_string());
    if cancel.is_cancelled() {
        return Ok(());
    }
    result.and(closed).and(disconnected)
}

async fn connected_session(
    connection: &P2pConnection,
    lease: &SelectedCapture,
    choices: &[EncoderChoice],
    events: async_channel::Sender<Event>,
    cancel: CancellationToken,
) -> Result<(), String> {
    let network = connection
        .network(&cancel)
        .await
        .map_err(|e| e.to_string())?;
    let listener = source::listen(network)
        .await
        .map_err(|e| format!("Could not open RTSP on the P2P interface: {e}"))?;
    tracing::info!(target: "wireless_display::miracast", port = source::RTSP_PORT, "Waiting for RTSP control on the P2P interface");
    emit(&events, Event::Connected, &cancel).await;
    let token = cancel.child_token();
    let work = async {
        let stream = source::accept(&listener, network, &token)
            .await
            .map_err(|e| {
                format!(
                    "The TV did not open the RTSP control connection: {e}. Check the firewall on the P2P interface."
                )
            })?;
        tracing::info!(target: "wireless_display::miracast", "TV connected to RTSP control; starting WFD negotiation");
        session(
            stream,
            CaptureInput {
                source: lease.source(),
                info: lease.info(),
                portal: lease.portal_session(),
            },
            network,
            choices,
            events,
            &token,
        )
        .await
    };
    tokio::pin!(work);
    let monitor = connection.monitor(network, cancel.clone());
    tokio::pin!(monitor);
    tokio::select! {
        result = &mut work => result,
        result = &mut monitor => {
            token.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(4),&mut work).await;
            result.map_err(|e| e.to_string())
        },
    }
}

async fn media_result(
    future: &mut Option<BoxFuture<'static, Result<(), MediaError>>>,
) -> Result<(), MediaError> {
    match future {
        Some(future) => future.await,
        None => std::future::pending().await,
    }
}

async fn quality_result(
    reply: &mut Option<tokio::sync::oneshot::Receiver<Result<(), String>>>,
) -> Result<(), String> {
    match reply {
        Some(reply) => reply
            .await
            .map_err(|_| "Quality control channel closed".to_owned())?,
        None => std::future::pending().await,
    }
}

struct CaptureInput {
    source: CaptureSource,
    info: CaptureInfo,
    portal: Option<Arc<PortalSession>>,
}

async fn session(
    stream: TcpStream,
    capture: CaptureInput,
    network: P2pNetwork,
    choices: &[EncoderChoice],
    events: async_channel::Sender<Event>,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let CaptureInput {
        source,
        info: capture_info,
        portal,
    } = capture;
    let peer = match stream.peer_addr().map_err(|e| e.to_string())?.ip() {
        IpAddr::V4(ip) => ip,
        _ => return Err("IPv6 WFD is not supported".into()),
    };
    let (requests, receive) = async_channel::bounded(8);
    let offers: Vec<_> = choices.iter().map(|choice| choice.offer).collect();
    let (quality_tx, quality_rx) = async_channel::bounded(1);
    let protocol_cancel = cancel.child_token();
    let protocol = WfdSource::try_new(stream, requests)
        .map_err(|e| e.to_string())?
        .run(&offers, peer, quality_rx, protocol_cancel.clone());
    tokio::pin!(protocol);
    let closed = async {
        if let Some(portal) = portal.as_ref() {
            let mut changes = portal.receive_closed().await.map_err(|e| e.to_string())?;
            changes.next().await;
            Err::<(), String>("The desktop closed the capture session.".into())
        } else {
            std::future::pending().await
        }
    };
    tokio::pin!(closed);
    let (media_tx, media_rx) = async_channel::bounded(32);
    let media_cancel = cancel.child_token();
    let mut media_future = None;
    let mut pipeline: Option<Arc<GstMediaPipeline>> = None;
    let mut protocol_finished = false;
    let mut play_requested = false;
    let mut announced = false;
    let mut prior_packets = 0;
    let mut adaptive: Option<AdaptiveQuality> = None;
    let mut stream_info: Option<StreamingInfo> = None;
    let mut prepared_format: Option<PreparedFormat> = None;
    let mut quality_reply = None;
    let mut quality_target: Option<QualityTarget> = None;
    let mut last_statistics = VideoStatistics::default();
    let result = loop {
        tokio::select! {
            _ = cancel.cancelled() => break Ok(()),
            result = &mut protocol => { protocol_finished = true; break result.map_err(|e| e.to_string()); },
            result = &mut closed => break result,
            result = quality_result(&mut quality_reply), if quality_target.is_some() => {
                quality_reply = None;
                if let (Some(target), Some(controller)) = (quality_target.take(), adaptive.as_mut()) {
                    match result {
                        Ok(()) => {
                            controller.applied(target);
                            if let Some(info) = stream_info.as_mut() {
                                info.video = target.video; info.bitrate_kbps = target.bitrate_kbps;
                                tracing::info!(target:"wireless_display::miracast",mode=?target.video.mode,bitrate_kbps=target.bitrate_kbps,"Video quality adjusted");
                                emit(&events, Event::Negotiated(info.clone()), cancel).await;
                            }
                        },
                        Err(error) => {
                            controller.rejected(/* format_change= */ true);
                            emit(&events, Event::Media(MediaEvent::Warning(format!("Automatic resolution switching unavailable: {error}"))), cancel).await;
                        }
                    }
                }
            },
            result = media_result(&mut media_future) => { media_future = None; break match result { Ok(()) => Err("Pipeline stopped before TEARDOWN.".into()), Err(error) => Err(error.to_string()) }; },
            request = receive.recv() => {
                let Ok(request) = request else { break Err("RTSP media channel closed".into()); };
                match request {
                    MediaRequest::Configure { stream,reply } => {
                        let configured = (|| {
                            let choice = choices.iter().find(|c| c.offer.preferred.profile == stream.video.profile && c.offer.preferred.mode == stream.video.mode && c.offer.preferred.level == stream.video.level).ok_or(MediaError::NoEncoder)?;
                            let sockets = RtpSockets::bind(network.local)?;
                            let ports = sockets.ports(stream.client_ports.rtcp.is_some());
                            let new = GstMediaPipeline::try_new_streaming(source.clone(),&stream,choice,sockets)?;
                            Ok::<_,MediaError>((Arc::new(new),ports,StreamingInfo { video:stream.video,encoder:choice.name.clone(),bitrate_kbps:stream.video.mode.bitrate_range().0,peer }))
                        })();
                        match configured {
                            Ok((new,ports,info)) => {
                                adaptive = Some(AdaptiveQuality::new(stream.video, stream.fallback));
                                tracing::info!(target:"wireless_display::miracast",encoder=%info.encoder,mode=?info.video.mode,fps=info.video.mode.fps(),bitrate_kbps=info.bitrate_kbps,"Video negotiated");
                                stream_info = Some(info.clone()); pipeline = Some(new);
                                emit(&events,Event::Negotiated(info),cancel).await;
                                if stream.video.mode.size().1 > 720 && stream.fallback.is_none() {
                                    emit(&events,Event::Media(MediaEvent::Warning("The TV did not advertise 720p compatible with the current profile; resolution fallback is unavailable for this session.".into())),cancel).await;
                                }
                                if stream.client_ports.rtcp.is_none() {
                                    emit(&events,Event::Media(MediaEvent::Warning("The TV did not negotiate RTCP feedback. Bitrate remains conservative; downscaling due to local overload remains active.".into())),cancel).await;
                                }
                                let _ = reply.send(Ok(ports));
                            },
                            Err(error) => { let _ = reply.send(Err(error.to_string())); }
                        }
                    },
                    MediaRequest::Play(reply) => {
                        let result = match pipeline.as_ref() { Some(p) => p.start().map_err(|e| e.to_string()), None => Err("Pipeline not configured".into()) };
                        if result.is_ok() && let Some(pipeline) = pipeline.as_ref() {
                            prior_packets = pipeline.frames(); play_requested = true; announced = false;
                            pipeline.request_keyframe();
                            if media_future.is_none() {
                                let pipeline = pipeline.clone(); let tx = media_tx.clone(); let token = media_cancel.clone();
                                media_future = Some(Box::pin(async move { pipeline.run(tx,token).await }) as BoxFuture<'static,Result<(),MediaError>>);
                            }
                        }
                        let _ = reply.send(result);
                    },
                    MediaRequest::Pause(reply) => {
                        let result = match pipeline.as_ref() { Some(p) => p.pause().map_err(|e| e.to_string()), None => Err("Pipeline not configured".into()) };
                        if result.is_ok() { play_requested = false; if let Some(controller) = adaptive.as_mut() { controller.reset(); } emit(&events,Event::Paused,cancel).await; }
                        let _ = reply.send(result);
                    },
                    MediaRequest::PrepareFormat { target, reply } => {
                        let result = match pipeline.as_ref() {
                            Some(pipeline) if prepared_format.is_none() => match pipeline.prepare_format(target,cancel).await {
                                Ok(prepared) => { let pts = prepared.pts90k; prepared_format = Some(prepared); Ok(pts) },
                                Err(error) => Err(error.to_string()),
                            },
                            _ => Err("Pipeline busy or unavailable for a resolution change".into()),
                        };
                        let _ = reply.send(result);
                    },
                    MediaRequest::FinishFormat { commit, reply } => {
                        let result = match (pipeline.as_ref(),prepared_format.take()) {
                            (Some(pipeline),Some(prepared)) if commit => pipeline.commit_format(prepared).map_err(|error| error.to_string()),
                            (_,Some(prepared)) => { drop(prepared); Ok(()) },
                            _ => Err("Resolution change not prepared".into()),
                        };
                        let _ = reply.send(result);
                    },
                    MediaRequest::Keyframe => if let Some(pipeline) = pipeline.as_ref() { pipeline.request_keyframe(); },
                }
            },
            event = media_rx.recv() => if let Ok(event) = event {
                if play_requested && !announced && matches!(event,MediaEvent::Packets(n) if n > prior_packets) {
                    announced = true; emit(&events,Event::Playing(capture_info.clone()),cancel).await;
                }
                if let MediaEvent::VideoStatistics(statistics) = event {
                    last_statistics = statistics;
                }
                if let MediaEvent::Quality(sample) = event {
                    if play_requested {
                        tracing::debug!(target:"wireless_display::miracast",input_fps=last_statistics.input_fps,encoded_fps=last_statistics.encoded_fps,dropped_frames=last_statistics.dropped_frames,loss_percent=?sample.loss.map(|loss|f64::from(loss)*100.0/256.0),"Frame rates measured on the laptop");
                    }
                    if play_requested && quality_target.is_none() && let Some(controller) = adaptive.as_mut()
                        && let Some(target) = controller.observe(sample) && let Some(pipeline) = pipeline.as_ref() {
                        if target.video == controller.current().video {
                            match pipeline.set_bitrate(target.bitrate_kbps) {
                                Ok(()) => {
                                    controller.applied(target);
                                    if let Some(info) = stream_info.as_mut() {
                                        info.bitrate_kbps = target.bitrate_kbps;
                                        emit(&events,Event::Negotiated(info.clone()),cancel).await;
                                    }
                                },
                                Err(error) => {
                                    controller.rejected(/* format_change= */ false);
                                    emit(&events,Event::Media(MediaEvent::Warning(error.to_string())),cancel).await;
                                }
                            }
                        } else {
                            tracing::info!(target:"wireless_display::miracast",overloaded=sample.overloaded,loss_fraction=?sample.loss,input_fps=last_statistics.input_fps,encoded_fps=last_statistics.encoded_fps,dropped_frames=last_statistics.dropped_frames,next_mode=?target.video.mode,"Requesting video quality adjustment");
                            let (reply,response) = tokio::sync::oneshot::channel();
                            if quality_tx.try_send(QualityRequest { target,reply }).is_ok() {
                                quality_reply = Some(response); quality_target = Some(target);
                            }
                        }
                    }
                } else { emit(&events,Event::Media(event),cancel).await; }
            }
        }
    };
    // Stop video immediately; teardown must never delay the user's privacy control.
    media_cancel.cancel();
    drop(prepared_format);
    if let Some(pipeline) = pipeline.as_ref() {
        let _ = pipeline.stop();
    }
    // Keep polling protocol cancellation so its bounded TEARDOWN write executes.
    protocol_cancel.cancel();
    if !protocol_finished {
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut protocol).await;
    }
    if let Some(future) = media_future {
        let _ = future.await;
    }
    // Cancellation can make the media future complete before the select branch
    // that observes the token. The enclosing network monitor retains its cause.
    if cancel.is_cancelled() {
        Ok(())
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use tokio::net::{TcpListener, UdpSocket};
    use wireless_display_core::protocol::wfd::capabilities::WfdParameters;
    use wireless_display_core::protocol::wfd::rtsp::transport::RtspTransport;
    use wireless_display_core::protocol::wfd::rtsp::{Message, StartLine};
    use wireless_display_core::{CaptureKind, protocol};

    struct Sink {
        control: RtspTransport,
        udp: UdpSocket,
        rtcp: Option<UdpSocket>,
        server_ports: Option<protocol::wfd::video::RtpPorts>,
        next: u32,
        session: String,
        url: String,
        keepalives: usize,
    }

    impl Sink {
        async fn receive(&mut self) -> Message {
            self.control
                .receive(&CancellationToken::new())
                .await
                .unwrap()
        }
        async fn reply(&mut self, request: &Message, body: &[u8], public: bool) {
            let mut headers = vec![
                ("CSeq".into(), request.header("CSeq").unwrap().into()),
                ("Content-Type".into(), "text/parameters".into()),
            ];
            if public {
                headers.push((
                    "Public".into(),
                    "org.wfa.wfd1.0, GET_PARAMETER, SET_PARAMETER".into(),
                ));
            }
            self.control
                .send(
                    &Message {
                        start: StartLine::Response {
                            status: 200,
                            reason: "OK".into(),
                        },
                        headers,
                        body: body.to_vec(),
                    },
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
        }
        async fn command(&mut self, method: &str, setup: bool) -> Message {
            self.next += 1;
            let mut headers = vec![("CSeq".into(), self.next.to_string())];
            if method == "OPTIONS" {
                headers.push(("Require".into(), "org.wfa.wfd1.0".into()));
            }
            if setup {
                headers.push((
                    "Transport".into(),
                    format!(
                        "RTP/AVP/UDP;unicast;client_port={}",
                        match self.rtcp.as_ref() {
                            Some(rtcp) => format!(
                                "{}-{}",
                                self.udp.local_addr().unwrap().port(),
                                rtcp.local_addr().unwrap().port()
                            ),
                            None => self.udp.local_addr().unwrap().port().to_string(),
                        }
                    ),
                ));
            } else if !self.session.is_empty() {
                headers.push(("Session".into(), self.session.clone()));
            }
            let uri = if method == "OPTIONS" { "*" } else { &self.url };
            self.control
                .send(
                    &Message {
                        start: StartLine::Request {
                            method: method.into(),
                            uri: uri.into(),
                        },
                        headers,
                        body: Vec::new(),
                    },
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            loop {
                let message = self.receive().await;
                if matches!(&message.start,StartLine::Request { method,.. } if method=="GET_PARAMETER")
                {
                    self.keepalives += 1;
                    self.reply(&message, &[], false).await;
                    continue;
                }
                assert!(
                    matches!(message.start, StartLine::Response { status: 200, .. }),
                    "{:?}",
                    message
                );
                assert_eq!(message.header("CSeq"), Some(self.next.to_string().as_str()));
                return message;
            }
        }
        async fn handshake(&mut self) {
            let m1 = self.receive().await;
            assert!(matches!(&m1.start,StartLine::Request { method,.. } if method=="OPTIONS"));
            self.reply(&m1, &[], true).await;
            self.command("OPTIONS", false).await;
            let m3 = self.receive().await;
            assert!(
                matches!(&m3.start,StartLine::Request { method,.. } if method=="GET_PARAMETER")
            );
            let caps = format!(
                "wfd_video_formats: 00 00 03 1f 000000a1 00000000 00000000 00 0000 0000 00 none none\r\nwfd_client_rtp_ports: RTP/AVP/UDP;unicast {} 0 mode=play\r\nwfd_content_protection: HDCP2.1 port=1024\r\n",
                self.udp.local_addr().unwrap().port()
            );
            self.reply(&m3, caps.as_bytes(), false).await;
            let m4 = self.receive().await;
            let parameters = WfdParameters::parse(&m4.body).unwrap();
            assert!(parameters.get("wfd_audio_codecs").is_none());
            assert_eq!(parameters.get("wfd_content_protection"), Some("none"));
            self.url = parameters
                .get("wfd_presentation_URL")
                .unwrap()
                .strip_suffix(" none")
                .unwrap()
                .into();
            self.reply(&m4, &[], false).await;
            let m5 = self.receive().await;
            assert_eq!(m5.body, b"wfd_trigger_method: SETUP\r\n");
            self.reply(&m5, &[], false).await;
            let setup = self.command("SETUP", true).await;
            self.session = setup
                .header("Session")
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .into();
            let server = setup
                .header("Transport")
                .unwrap()
                .split(';')
                .find_map(|part| part.strip_prefix("server_port="))
                .unwrap();
            let (rtp, rtcp) = match server.split_once('-') {
                Some((rtp, rtcp)) => (rtp.parse().unwrap(), Some(rtcp.parse().unwrap())),
                None => (server.parse().unwrap(), None),
            };
            self.server_ports = Some(protocol::wfd::video::RtpPorts { rtp, rtcp });
            self.command("PLAY", false).await;
        }
        async fn video_packet(&self) -> u32 {
            let mut bytes = [0u8; 2048];
            let (size, _) =
                tokio::time::timeout(Duration::from_secs(5), self.udp.recv_from(&mut bytes))
                    .await
                    .unwrap()
                    .unwrap();
            assert!(size >= 200);
            assert_eq!(bytes[0] >> 6, 2);
            assert_eq!(bytes[1] & 0x7f, 33);
            assert_eq!(bytes[12], 0x47);
            u32::from_be_bytes(bytes[8..12].try_into().unwrap())
        }
    }

    async fn pair() -> (TcpStream, Sink) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let sink = Sink {
            control: RtspTransport::from_stream(client),
            udp: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
            rtcp: None,
            server_ports: None,
            next: 0,
            session: String::new(),
            url: String::new(),
            keepalives: 0,
        };
        (server, sink)
    }
    fn input() -> CaptureInput {
        CaptureInput {
            source: CaptureSource::Test,
            info: CaptureInfo {
                backend: CaptureKind::Dummy,
                logical_size: Some((1280, 720)),
            },
            portal: None,
        }
    }
    fn network() -> P2pNetwork {
        P2pNetwork {
            local: Ipv4Addr::LOCALHOST,
            prefix: 8,
            gateway: None,
        }
    }

    #[tokio::test]
    async fn real_video_m1_m7_keepalive_pause_resume_and_teardown() {
        let cancel = CancellationToken::new();
        let choices =
            wireless_display_media::probe_encoder_candidates(&["openh264enc", "x264enc"], &cancel)
                .unwrap();
        let (server, mut sink) = pair().await;
        let (tx, rx) = async_channel::bounded(256);
        let token = cancel.clone();
        let task =
            tokio::spawn(
                async move { session(server, input(), network(), &choices, tx, &token).await },
            );
        sink.handshake().await;
        sink.video_packet().await;
        let keepalive = tokio::time::timeout(Duration::from_secs(8), sink.receive())
            .await
            .unwrap();
        assert!(
            matches!(&keepalive.start,StartLine::Request { method,.. } if method=="GET_PARAMETER")
        );
        sink.reply(&keepalive, &[], false).await;
        sink.command("PAUSE", false).await;
        sink.command("PLAY", false).await;
        sink.video_packet().await;
        sink.command("TEARDOWN", false).await;
        task.await.unwrap().unwrap();
        let mut playing = false;
        let mut paused = false;
        let mut negotiated = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                Event::Playing(_) => playing = true,
                Event::Paused => paused = true,
                Event::Negotiated(_) => negotiated = true,
                _ => {}
            }
        }
        assert!(playing && paused && negotiated);
    }

    #[tokio::test]
    async fn cancellation_sends_teardown_and_releases_control() {
        let cancel = CancellationToken::new();
        let choices =
            wireless_display_media::probe_encoder_candidates(&["openh264enc", "x264enc"], &cancel)
                .unwrap();
        let (server, mut sink) = pair().await;
        let (tx, _rx) = async_channel::bounded(256);
        let token = cancel.clone();
        let task =
            tokio::spawn(
                async move { session(server, input(), network(), &choices, tx, &token).await },
            );
        sink.handshake().await;
        sink.video_packet().await;
        cancel.cancel();
        let teardown = sink.receive().await;
        assert!(matches!(&teardown.start,StartLine::Request { method,.. } if method=="TEARDOWN"));
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn receiver_loss_automatically_renegotiates_full_hd_to_720p() {
        let cancel = CancellationToken::new();
        let choices = wireless_display_media::probe_modes(
            &["x264enc"],
            VideoMode::FullHd,
            VideoMode::FullHd.size(),
            &cancel,
        )
        .unwrap();
        let (server, mut sink) = pair().await;
        sink.rtcp = Some(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let (tx, rx) = async_channel::bounded(128);
        let token = cancel.clone();
        let task =
            tokio::spawn(
                async move { session(server, input(), network(), &choices, tx, &token).await },
            );
        sink.handshake().await;
        let ssrc = sink.video_packet().await;
        let rtcp_port = sink.server_ports.unwrap().rtcp.unwrap();
        for n in 1u32..=3 {
            let mut rr = vec![0x81, 201, 0, 7, 0, 0, 0, 99];
            rr.extend(ssrc.to_be_bytes());
            rr.extend([64, 0, 0, 1]);
            rr.extend((n * 6000).to_be_bytes());
            rr.extend([0u8; 12]);
            sink.rtcp
                .as_ref()
                .unwrap()
                .send_to(&rr, (Ipv4Addr::LOCALHOST, rtcp_port))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(2100)).await;
        }
        let m4 = tokio::time::timeout(Duration::from_secs(10),async {
            loop {
                let message = sink.receive().await;
                if matches!(&message.start,StartLine::Request { method,.. } if method=="GET_PARAMETER") { sink.reply(&message,&[],false).await; continue; }
                break message;
            }
        }).await.unwrap();
        let parameters = WfdParameters::parse(&m4.body).unwrap();
        assert!(parameters.get("wfd_av_format_change_timing").is_some());
        assert!(
            parameters
                .get("wfd_video_formats")
                .unwrap()
                .contains("00000020")
        );
        sink.reply(&m4, &[], false).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Event::Negotiated(info) = rx.recv().await.unwrap()
                    && info.video.mode == VideoMode::Hd
                {
                    assert_eq!(info.bitrate_kbps, 4000);
                    break;
                }
            }
        })
        .await
        .unwrap();
        sink.video_packet().await;
        sink.command("TEARDOWN", false).await;
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn renegotiation_waits_for_ack_and_rolls_back_on_rejection() {
        use wireless_display_core::protocol::wfd::video::{
            H264Profile, RtpPorts, VideoFormat, VideoOffer,
        };
        let preferred = VideoFormat {
            mode: VideoMode::FullHd,
            profile: H264Profile::High,
            level: 4,
            r2: false,
        };
        let fallback = VideoFormat {
            mode: VideoMode::Hd,
            ..preferred
        };
        let offers = [VideoOffer {
            preferred,
            fallback: Some(fallback),
        }];
        let (server, mut sink) = pair().await;
        let (media_tx, media_rx) = async_channel::bounded(8);
        let (finished_tx, finished_rx) = async_channel::bounded(8);
        let media = tokio::spawn(async move {
            while let Ok(request) = media_rx.recv().await {
                match request {
                    MediaRequest::Configure { reply, .. } => {
                        let _ = reply.send(Ok(RtpPorts {
                            rtp: 18000,
                            rtcp: None,
                        }));
                    }
                    MediaRequest::Play(reply) | MediaRequest::Pause(reply) => {
                        let _ = reply.send(Ok(()));
                    }
                    MediaRequest::PrepareFormat { reply, .. } => {
                        let _ = reply.send(Ok(324_090_000));
                    }
                    MediaRequest::FinishFormat { commit, reply } => {
                        finished_tx.send(commit).await.unwrap();
                        let _ = reply.send(Ok(()));
                    }
                    MediaRequest::Keyframe => {}
                }
            }
        });
        let (quality_tx, quality_rx) = async_channel::bounded(1);
        let protocol = tokio::spawn(async move {
            WfdSource::try_new(server, media_tx)
                .unwrap()
                .run(
                    &offers,
                    Ipv4Addr::LOCALHOST,
                    quality_rx,
                    CancellationToken::new(),
                )
                .await
        });
        sink.handshake().await;
        for accepted in [false, true] {
            let (reply, response) = tokio::sync::oneshot::channel();
            quality_tx
                .send(QualityRequest {
                    target: QualityTarget {
                        video: fallback,
                        bitrate_kbps: 4000,
                    },
                    reply,
                })
                .await
                .unwrap();
            let m4 = sink.receive().await;
            assert!(
                matches!(&m4.start,StartLine::Request { method,.. } if method == "SET_PARAMETER")
            );
            let fields = WfdParameters::parse(&m4.body).unwrap();
            assert_eq!(
                fields.get("wfd_video_formats"),
                Some(fallback.parameter().as_str())
            );
            assert_eq!(
                fields.get("wfd_av_format_change_timing"),
                Some(format!("{:010x} {:010x}", 324_090_000u64 << 7, 324_090_000u64 << 7).as_str())
            );
            assert!(finished_rx.is_empty(), "media cannot switch before RTSP OK");
            if accepted {
                sink.reply(&m4, &[], false).await;
            } else {
                sink.control
                    .send(
                        &Message {
                            start: StartLine::Response {
                                status: 451,
                                reason: "Parameter Not Understood".into(),
                            },
                            headers: vec![("CSeq".into(), m4.header("CSeq").unwrap().into())],
                            body: Vec::new(),
                        },
                        &CancellationToken::new(),
                    )
                    .await
                    .unwrap();
            }
            assert_eq!(response.await.unwrap().is_ok(), accepted);
            assert_eq!(finished_rx.recv().await.unwrap(), accepted);
        }
        sink.command("TEARDOWN", false).await;
        protocol.await.unwrap().unwrap();
        media.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_mismatched_response_cseq() {
        let (server, mut sink) = pair().await;
        let (tx, _rx) = async_channel::bounded(32);
        let task = tokio::spawn(async move {
            WfdSource::try_new(server, tx)
                .unwrap()
                .run(
                    &[],
                    Ipv4Addr::LOCALHOST,
                    async_channel::bounded(1).1,
                    CancellationToken::new(),
                )
                .await
        });
        let mut request = sink.receive().await;
        request.headers = vec![("CSeq".into(), "999".into())];
        sink.reply(&request, &[], true).await;
        assert!(task.await.unwrap().is_err());
    }
}
