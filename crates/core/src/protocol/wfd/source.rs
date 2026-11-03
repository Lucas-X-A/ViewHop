use super::capabilities::WfdParameters;
use super::quality::QualityTarget;
use super::rtsp::transport::RtspTransport;
use super::rtsp::{Message, ProtocolError, StartLine};
use super::session::{NegotiationPhase, WfdSession};
use super::video::{self, NegotiatedStream, P2pNetwork, RtpPorts, VideoOffer};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
pub const RTSP_PORT: u16 = 7236;
const PUBLIC: &str =
    "org.wfa.wfd1.0, OPTIONS, GET_PARAMETER, SET_PARAMETER, SETUP, PLAY, PAUSE, TEARDOWN";
const QUERY: &str = "rtsp://localhost/wfd1.0";

pub enum MediaRequest {
    Configure {
        stream: NegotiatedStream,
        reply: oneshot::Sender<Result<RtpPorts, String>>,
    },
    Play(oneshot::Sender<Result<(), String>>),
    Pause(oneshot::Sender<Result<(), String>>),
    Keyframe,
    PrepareFormat {
        target: QualityTarget,
        reply: oneshot::Sender<Result<u64, String>>,
    },
    FinishFormat {
        commit: bool,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

pub struct QualityRequest {
    pub target: QualityTarget,
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// Bind only the P2P local address. Never expose desktop control on the LAN.
pub async fn listen(network: P2pNetwork) -> Result<TcpListener, ProtocolError> {
    Ok(TcpListener::bind(SocketAddr::new(IpAddr::V4(network.local), RTSP_PORT)).await?)
}

pub async fn accept(
    listener: &TcpListener,
    network: P2pNetwork,
    cancel: &CancellationToken,
) -> Result<TcpStream, ProtocolError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(ProtocolError::Cancelled),
        result = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let (stream, peer) = listener.accept().await?;
                if let IpAddr::V4(peer) = peer.ip()
                    && network.permits(peer)
                    && stream.local_addr()?.ip() == IpAddr::V4(network.local)
                { stream.set_nodelay(true)?; return Ok(stream); }
            }
        }) => result.map_err(|_| ProtocolError::Timeout)?,
    }
}

pub struct WfdSource {
    transport: RtspTransport,
    machine: WfdSession,
    media: async_channel::Sender<MediaRequest>,
    cseq: u32,
    remote_cseq: Option<u32>,
    options_received: bool,
    presentation: String,
    session_id: String,
    setup: bool,
    negotiated: Option<NegotiatedStream>,
    offer: Option<VideoOffer>,
}

impl WfdSource {
    /// Compatibility alias for [`Self::try_new`].
    pub fn new(
        stream: TcpStream,
        media: async_channel::Sender<MediaRequest>,
    ) -> Result<Self, ProtocolError> {
        Self::try_new(stream, media)
    }

    /// Takes ownership of the control stream and creates a per-connection session.
    ///
    /// Rejects IPv6 and failures to read the socket's local address. Negotiation
    /// begins only when [`Self::run`] is polled.
    pub fn try_new(
        stream: TcpStream,
        media: async_channel::Sender<MediaRequest>,
    ) -> Result<Self, ProtocolError> {
        let local = match stream.local_addr()?.ip() {
            IpAddr::V4(ip) => ip,
            _ => return Err(ProtocolError::Unsupported("IPv6 WFD")),
        };
        static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
        // Session identifies this one TCP control connection; it is not an
        // authentication credential and is never accepted on another connection.
        let session_id = format!(
            "wd{:x}{:x}",
            std::process::id(),
            NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
        );
        Ok(Self {
            transport: RtspTransport::from_stream(stream),
            machine: WfdSession::default(),
            media,
            cseq: 0,
            remote_cseq: None,
            options_received: false,
            presentation: format!("rtsp://{local}/wfd1.0/streamid=0"),
            session_id,
            setup: false,
            negotiated: None,
            offer: None,
        })
    }

    fn next_cseq(&mut self) -> Result<u32, ProtocolError> {
        self.cseq = self
            .cseq
            .checked_add(1)
            .ok_or(ProtocolError::Malformed("CSeq overflow"))?;
        Ok(self.cseq)
    }

    async fn response(
        &mut self,
        request: &Message,
        status: u16,
        extra: Vec<(&str, String)>,
        cancel: &CancellationToken,
    ) -> Result<(), ProtocolError> {
        let cseq = request
            .header("CSeq")
            .ok_or(ProtocolError::Malformed("missing CSeq"))?;
        let reason = match status {
            200 => "OK",
            405 => "Method Not Allowed",
            451 => "Parameter Not Understood",
            454 => "Session Not Found",
            455 => "Method Not Valid in This State",
            461 => "Unsupported Transport",
            551 => "Option Not Supported",
            _ => "Bad Request",
        };
        let mut headers = vec![
            ("CSeq".into(), cseq.into()),
            (
                "Server".into(),
                format!("WirelessDisplay/{}", env!("CARGO_PKG_VERSION")),
            ),
        ];
        headers.extend(extra.into_iter().map(|(key, value)| (key.into(), value)));
        self.transport
            .send(
                &Message {
                    start: StartLine::Response {
                        status,
                        reason: reason.into(),
                    },
                    headers,
                    body: Vec::new(),
                },
                cancel,
            )
            .await
    }

    async fn request(
        &mut self,
        method: &str,
        uri: &str,
        body: &[u8],
        require: bool,
        cancel: &CancellationToken,
    ) -> Result<Message, ProtocolError> {
        let cseq = self.next_cseq()?;
        tracing::debug!(target:"wireless_display::miracast",method,cseq,"Sending RTSP request");
        let mut headers = vec![("CSeq".into(), cseq.to_string())];
        if require {
            headers.push(("Require".into(), "org.wfa.wfd1.0".into()));
        }
        if !body.is_empty() {
            headers.push(("Content-Type".into(), "text/parameters".into()));
        }
        if self.setup {
            headers.push(("Session".into(), self.session_id.clone()));
        }
        let request = Message {
            start: StartLine::Request {
                method: method.into(),
                uri: uri.into(),
            },
            headers,
            body: body.to_vec(),
        };
        self.transport.send(&request, cancel).await?;
        tokio::time::timeout(CONTROL_TIMEOUT, async {
            // Bound work even if a peer floods requests instead of replying.
            for _ in 0..128 {
                let message = self.transport.receive(cancel).await?;
                match message.start {
                    StartLine::Response { status, .. } => {
                        if message.header("CSeq") != request.header("CSeq") {
                            return Err(ProtocolError::Malformed("response CSeq mismatch"));
                        }
                        if status != 200 {
                            return Err(ProtocolError::Rejected(status));
                        }
                        return Ok(message);
                    }
                    StartLine::Request { .. } => {
                        if self.incoming(&message, cancel).await? {
                            return Err(ProtocolError::Closed);
                        }
                    }
                }
            }
            Err(ProtocolError::TooLarge)
        })
        .await
        .map_err(|_| ProtocolError::Timeout)?
    }

    fn valid_session(&self, message: &Message) -> bool {
        message
            .header("Session")
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim() == self.session_id)
    }

    async fn media_action(
        &self,
        play: bool,
        cancel: &CancellationToken,
    ) -> Result<(), ProtocolError> {
        let (tx, rx) = oneshot::channel();
        let action = if play {
            MediaRequest::Play(tx)
        } else {
            MediaRequest::Pause(tx)
        };
        tokio::select! {
            _ = cancel.cancelled() => Err(ProtocolError::Cancelled),
            result = tokio::time::timeout(CONTROL_TIMEOUT, async {
                self.media.send(action).await.map_err(|_| ProtocolError::Closed)?;
                rx.await.map_err(|_| ProtocolError::Closed)?.map_err(ProtocolError::Media)
            }) => result.map_err(|_| ProtocolError::Timeout)?,
        }
    }

    async fn incoming(
        &mut self,
        message: &Message,
        cancel: &CancellationToken,
    ) -> Result<bool, ProtocolError> {
        let StartLine::Request { method, uri } = &message.start else {
            return Err(ProtocolError::Malformed("unsolicited RTSP response"));
        };
        tracing::debug!(target:"wireless_display::miracast",method,"Received RTSP request");
        let cseq = message
            .header("CSeq")
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or(ProtocolError::Malformed("CSeq"))?;
        if self.remote_cseq.is_some_and(|last| cseq <= last) {
            return Err(ProtocolError::Malformed("non-monotonic CSeq"));
        }
        self.remote_cseq = Some(cseq);
        if message
            .header("Require")
            .is_some_and(|value| value.trim() != "org.wfa.wfd1.0")
        {
            self.response(message, 551, vec![], cancel).await?;
            return Err(ProtocolError::Unsupported("RTSP Require"));
        }
        if method != "OPTIONS" && uri != &self.presentation && uri != QUERY {
            self.response(message, 455, vec![], cancel).await?;
            return Err(ProtocolError::Malformed("presentation URL"));
        }
        match method.as_str() {
            "OPTIONS" => {
                self.options_received = true;
                self.response(message, 200, vec![("Public", PUBLIC.into())], cancel)
                    .await?;
            }
            "GET_PARAMETER"
                if message.body.is_empty() || message.body.iter().all(u8::is_ascii_whitespace) =>
            {
                if self.setup && !self.valid_session(message) {
                    self.response(message, 454, vec![], cancel).await?;
                    return Err(ProtocolError::Malformed("session ID"));
                }
                let extra = if self.setup {
                    vec![("Session", self.session_id.clone())]
                } else {
                    vec![]
                };
                self.response(message, 200, extra, cancel).await?;
            }
            "SETUP" if self.negotiated.is_some() && !self.setup => {
                let mut plan = self
                    .negotiated
                    .clone()
                    .ok_or(ProtocolError::Malformed("SETUP before configuration"))?;
                let ports = match video::setup_ports(
                    message
                        .header("Transport")
                        .ok_or(ProtocolError::Malformed("missing Transport"))?,
                ) {
                    Ok(ports) => ports,
                    Err(error) => {
                        self.response(message, 461, vec![], cancel).await?;
                        return Err(error);
                    }
                };
                if ports.rtp != plan.client_ports.rtp
                    || (plan.client_ports.rtcp.is_some() && ports.rtcp != plan.client_ports.rtcp)
                {
                    self.response(message, 461, vec![], cancel).await?;
                    return Err(ProtocolError::Malformed(
                        "SETUP port differs from WFD negotiation",
                    ));
                }
                plan.client_ports = ports;
                let (reply, rx) = oneshot::channel();
                let server = tokio::select! {
                    _ = cancel.cancelled() => return Err(ProtocolError::Cancelled),
                    result = tokio::time::timeout(CONTROL_TIMEOUT, async {
                        self.media.send(MediaRequest::Configure { stream:plan, reply }).await.map_err(|_| ProtocolError::Closed)?;
                        rx.await.map_err(|_| ProtocolError::Closed)?.map_err(ProtocolError::Media)
                    }) => result.map_err(|_| ProtocolError::Timeout)??,
                };
                let client = match ports.rtcp {
                    Some(rtcp) => format!("{}-{rtcp}", ports.rtp),
                    None => ports.rtp.to_string(),
                };
                let server_value = match server.rtcp {
                    Some(rtcp) => format!("{}-{rtcp}", server.rtp),
                    None => server.rtp.to_string(),
                };
                self.setup = true;
                self.machine.advance(NegotiationPhase::Ready)?;
                self.response(message,200,vec![("Session",format!("{};timeout=30",self.session_id)),("Transport",format!("RTP/AVP/UDP;unicast;client_port={client};server_port={server_value}"))],cancel).await?;
            }
            "PLAY" | "PAUSE" | "TEARDOWN" if self.setup => {
                if !self.valid_session(message) {
                    self.response(message, 454, vec![], cancel).await?;
                    return Err(ProtocolError::Malformed("session ID"));
                }
                if method == "TEARDOWN" {
                    self.response(
                        message,
                        200,
                        vec![("Session", self.session_id.clone())],
                        cancel,
                    )
                    .await?;
                    self.machine.advance(NegotiationPhase::Closed)?;
                    return Ok(true);
                }
                let play = method == "PLAY";
                let valid = matches!(
                    (self.machine.phase(), play),
                    (NegotiationPhase::Ready, true) | (NegotiationPhase::Playing, false)
                );
                if !valid {
                    self.response(message, 455, vec![], cancel).await?;
                    return Err(ProtocolError::Malformed("PLAY/PAUSE phase"));
                }
                if play {
                    // Authorize playback on RTSP before the first UDP media packet.
                    self.response(
                        message,
                        200,
                        vec![("Session", self.session_id.clone())],
                        cancel,
                    )
                    .await?;
                }
                self.media_action(play, cancel).await?;
                self.machine.advance(if play {
                    NegotiationPhase::Playing
                } else {
                    NegotiationPhase::Ready
                })?;
                if !play {
                    self.response(
                        message,
                        200,
                        vec![("Session", self.session_id.clone())],
                        cancel,
                    )
                    .await?;
                }
            }
            "SET_PARAMETER" if self.setup && message.body == b"wfd_idr_request\r\n" => {
                if !self.valid_session(message) {
                    return Err(ProtocolError::Malformed("session ID"));
                }
                self.media
                    .send(MediaRequest::Keyframe)
                    .await
                    .map_err(|_| ProtocolError::Closed)?;
                self.response(
                    message,
                    200,
                    vec![("Session", self.session_id.clone())],
                    cancel,
                )
                .await?;
            }
            _ => {
                self.response(message, 455, vec![], cancel).await?;
                return Err(ProtocolError::Unsupported("RTSP method or phase"));
            }
        }
        Ok(false)
    }

    async fn negotiate(
        &mut self,
        offers: &[VideoOffer],
        peer: Ipv4Addr,
        cancel: &CancellationToken,
    ) -> Result<(), ProtocolError> {
        let reply = self
            .request("OPTIONS", "*", &[], /* require= */ true, cancel)
            .await?;
        if !reply
            .header("Public")
            .is_some_and(|v| v.split(',').any(|s| s.trim() == "org.wfa.wfd1.0"))
        {
            return Err(ProtocolError::Unsupported(
                "receiver does not advertise org.wfa.wfd1.0",
            ));
        }
        tokio::time::timeout(CONTROL_TIMEOUT, async {
            while !self.options_received {
                let request = self.transport.receive(cancel).await?;
                if self.incoming(&request, cancel).await? {
                    return Err(ProtocolError::Closed);
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| ProtocolError::Timeout)??;
        self.machine.advance(NegotiationPhase::CapabilityExchange)?;
        let mut query =
            "wfd_video_formats\r\nwfd_client_rtp_ports\r\nwfd_content_protection\r\n".to_owned();
        if offers
            .iter()
            .any(|offer| offer.preferred.mode.requires_r2())
        {
            query.push_str("wfd2_video_formats\r\n");
        }
        let reply = self
            .request(
                "GET_PARAMETER",
                QUERY,
                query.as_bytes(),
                /* require= */ false,
                cancel,
            )
            .await?;
        if !reply
            .header("Content-Type")
            .is_some_and(|v| v.eq_ignore_ascii_case("text/parameters"))
        {
            return Err(ProtocolError::Malformed("WFD response Content-Type"));
        }
        let parameters = WfdParameters::parse(&reply.body)?;
        let plan = video::negotiate(&parameters, offers, peer)?;
        self.machine.advance(NegotiationPhase::Configuring)?;
        let body = format!(
            "{}: {}\r\nwfd_client_rtp_ports: RTP/AVP/UDP;unicast {} {} mode=play\r\nwfd_presentation_URL: {} none\r\nwfd_content_protection: none\r\n",
            plan.video.parameter_name(),
            plan.video.parameter(),
            plan.client_ports.rtp,
            plan.client_ports.rtcp.unwrap_or(0),
            self.presentation
        );
        self.request(
            "SET_PARAMETER",
            QUERY,
            body.as_bytes(),
            /* require= */ false,
            cancel,
        )
        .await?;
        self.offer = Some(VideoOffer {
            preferred: plan.video,
            fallback: plan.fallback,
        });
        self.negotiated = Some(plan);
        self.request(
            "SET_PARAMETER",
            QUERY,
            b"wfd_trigger_method: SETUP\r\n",
            /* require= */ false,
            cancel,
        )
        .await?;
        Ok(())
    }

    async fn change_format(
        &mut self,
        request: QualityRequest,
        cancel: &CancellationToken,
    ) -> Result<(), ProtocolError> {
        let target = request.target;
        let permitted = self.machine.phase() == NegotiationPhase::Playing
            && self.offer.is_some_and(|offer| {
                target.video == offer.preferred || Some(target.video) == offer.fallback
            });
        if !permitted {
            let _ = request
                .reply
                .send(Err("Format not authorized for this session".into()));
            return Ok(());
        }
        let (reply, response) = oneshot::channel();
        self.media
            .send(MediaRequest::PrepareFormat { target, reply })
            .await
            .map_err(|_| ProtocolError::Closed)?;
        let prepared = tokio::select! {
            _ = cancel.cancelled() => return Err(ProtocolError::Cancelled),
            result = tokio::time::timeout(CONTROL_TIMEOUT, response) => result.map_err(|_| ProtocolError::Timeout)?.map_err(|_| ProtocolError::Closed)?,
        };
        let pts = match prepared {
            Ok(pts) => pts,
            Err(error) => {
                let _ = request.reply.send(Err(error));
                return Ok(());
            }
        };
        // WFD carries the 33-bit MPEG-TS PTS/DTS in the high bits of a 40-bit field.
        let timing = (pts & ((1u64 << 33) - 1)) << 7;
        let body = format!(
            "{}: {}\r\nwfd_av_format_change_timing: {timing:010x} {timing:010x}\r\n",
            target.video.parameter_name(),
            target.video.parameter()
        );
        let accepted = self
            .request(
                "SET_PARAMETER",
                QUERY,
                body.as_bytes(),
                /* require= */ false,
                cancel,
            )
            .await;
        let (reply, response) = oneshot::channel();
        self.media
            .send(MediaRequest::FinishFormat {
                commit: accepted.is_ok(),
                reply,
            })
            .await
            .map_err(|_| ProtocolError::Closed)?;
        let finished = tokio::select! {
            _ = cancel.cancelled() => return Err(ProtocolError::Cancelled),
            result = tokio::time::timeout(CONTROL_TIMEOUT, response) => result.map_err(|_| ProtocolError::Timeout)?.map_err(|_| ProtocolError::Closed)?.map_err(ProtocolError::Media),
        };
        finished?;
        match accepted {
            Ok(_) => {
                if let Some(stream) = self.negotiated.as_mut() {
                    stream.video = target.video;
                }
                let _ = request.reply.send(Ok(()));
                Ok(())
            }
            Err(ProtocolError::Rejected(status)) => {
                let _ = request.reply.send(Err(format!(
                    "The TV rejected the resolution change (RTSP {status})"
                )));
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub async fn run(
        mut self,
        offers: &[VideoOffer],
        peer: Ipv4Addr,
        quality: async_channel::Receiver<QualityRequest>,
        cancel: CancellationToken,
    ) -> Result<(), ProtocolError> {
        let result = async {
            self.negotiate(offers,peer,&cancel).await?;
            let mut keepalive = tokio::time::interval_at(tokio::time::Instant::now()+Duration::from_secs(5),Duration::from_secs(5));
            keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let ready_deadline = tokio::time::sleep(Duration::from_secs(15));
            tokio::pin!(ready_deadline);
            let mut played = self.machine.phase() == NegotiationPhase::Playing;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(ProtocolError::Cancelled),
                    _ = &mut ready_deadline, if !played => return Err(ProtocolError::Timeout),
                    _ = keepalive.tick(), if self.setup => { let url = self.presentation.clone(); self.request("GET_PARAMETER",&url, &[], /* require= */ false, &cancel).await?; },
                    request = quality.recv(), if self.setup && !quality.is_closed() => {
                        if let Ok(request) = request { self.change_format(request, &cancel).await?; }
                    },
                    message = self.transport.receive(&cancel) => {
                        if self.incoming(&message?,&cancel).await? { return Ok(()); }
                        if self.machine.phase() == NegotiationPhase::Playing { played = true; }
                    }
                }
                if self.machine.phase() == NegotiationPhase::Playing { played = true; }
            }
        }.await;
        if self.setup && self.machine.phase() != NegotiationPhase::Closed {
            // Cancellation cannot skip teardown; this independent bounded token
            // permits a final best-effort write before the TCP socket is dropped.
            let cleanup = CancellationToken::new();
            let cseq = self.next_cseq()?;
            let message = Message {
                start: StartLine::Request {
                    method: "TEARDOWN".into(),
                    uri: self.presentation.clone(),
                },
                headers: vec![
                    ("CSeq".into(), cseq.to_string()),
                    ("Session".into(), self.session_id.clone()),
                ],
                body: Vec::new(),
            };
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                self.transport.send(&message, &cleanup),
            )
            .await;
        }
        result
    }
}
