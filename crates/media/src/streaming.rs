use crate::pipeline;
use crate::{EncoderSelector, GstMediaPipeline, MediaError};
use gst::glib::translate::IntoGlib;
use gst::prelude::*;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use wireless_display_core::CaptureSource;
use wireless_display_core::protocol::wfd::video::{
    H264Profile, NegotiatedStream, RtpPorts, VideoFormat, VideoMode, VideoOffer,
};
use wireless_display_core::traits::MediaPipeline;

#[derive(Clone, Debug)]
pub struct EncoderChoice {
    pub name: String,
    pub offer: VideoOffer,
}

#[derive(Clone, Copy, Debug)]
pub struct CaptureFormat {
    pub pixels: (u32, u32),
    /// Source caps accept nominal p60 or 59.94 Hz, possibly after renegotiation.
    pub supports_60fps: bool,
}

pub struct RtpSockets {
    rtp: gio::Socket,
    rtcp: gio::Socket,
    ports: RtpPorts,
}
impl RtpSockets {
    pub fn bind(local: Ipv4Addr) -> Result<Self, MediaError> {
        // Retain the actual descriptors; never advertise a port then reopen it.
        for _ in 0..32 {
            let rtp = UdpSocket::bind(SocketAddrV4::new(local, 0))?;
            let port = rtp.local_addr()?.port();
            if port % 2 != 0 || port == u16::MAX {
                continue;
            }
            let rtcp = match UdpSocket::bind(SocketAddrV4::new(local, port + 1)) {
                Ok(s) => s,
                Err(_) => continue,
            };
            return Ok(Self {
                rtp: gio::Socket::from_fd(rtp.into())?,
                rtcp: gio::Socket::from_fd(rtcp.into())?,
                ports: RtpPorts {
                    rtp: port,
                    rtcp: Some(port + 1),
                },
            });
        }
        Err(MediaError::Socket(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "no RTP/RTCP port pair available",
        )))
    }
    /// Returns reserved ports, exposing RTCP only when the receiver negotiated it.
    #[must_use]
    pub fn ports(&self, rtcp: bool) -> RtpPorts {
        RtpPorts {
            rtp: self.ports.rtp,
            rtcp: if rtcp { self.ports.rtcp } else { None },
        }
    }
}

fn set_uint(element: &gst::Element, name: &str, value: u32) {
    if let Some(property) = element.find_property(name) {
        if property.value_type() == u32::static_type() {
            element.set_property(name, value);
        } else if property.value_type() == i32::static_type() && value <= i32::MAX as u32 {
            element.set_property(name, value as i32);
        }
    }
}

fn configured_encoder(
    choice: &EncoderChoice,
    video: VideoFormat,
) -> Result<gst::Element, MediaError> {
    let encoder = pipeline::element(&choice.name)?;
    match choice.name.as_str() {
        "x264enc" => {
            encoder.set_property_from_str("tune", "zerolatency");
            encoder.set_property_from_str(
                "speed-preset",
                if video.mode == VideoMode::Vga {
                    "ultrafast"
                } else {
                    "superfast"
                },
            );

            if video.mode == VideoMode::Hd60 {
                // Frame threading improves compression efficiency versus slicing
                // one frame among many threads. No B-frames or lookahead; four
                // threads can add about three frames (50 ms) of encoder delay.
                let threads = std::thread::available_parallelism()
                    .map(|n| n.get().min(4) as u32)
                    .unwrap_or(1);
                encoder.set_property("threads", threads);
                encoder.set_property("sliced-threads", false);
                encoder.set_property("rc-lookahead", 0i32);
                encoder.set_property("sync-lookahead", 0i32);
            }
            encoder.set_property("bframes", 0u32);
            encoder.set_property("key-int-max", video.mode.fps());
            encoder.set_property("byte-stream", true);
            encoder.set_property("aud", true);
            // WFD bit 1 is Restricted High (RHP), which forbids CABAC.
            encoder.set_property("cabac", false);
            encoder.set_property("dct8x8", video.profile == H264Profile::High);
        }
        "openh264enc" => {
            encoder.set_property_from_str("rate-control", "bitrate");
            encoder.set_property_from_str("usage-type", "screen");
            encoder.set_property("gop-size", video.mode.fps());
        }
        _ => {
            if video.profile == H264Profile::High {
                let cabac = encoder
                    .find_property("cabac")
                    .filter(|property| property.value_type() == bool::static_type())
                    .ok_or(MediaError::NoEncoder)?;
                encoder.set_property(cabac.name(), false);
            }
            set_uint(&encoder, "key-int-max", video.mode.fps());
            set_uint(&encoder, "gop-size", video.mode.fps());
            set_uint(&encoder, "bframes", 0);
        }
    }
    set_bitrate(&encoder, video.mode.bitrate_range().0)?;
    Ok(encoder)
}

fn encoded_chain(
    choice: &EncoderChoice,
    video: VideoFormat,
) -> Result<[gst::Element; 4], MediaError> {
    let raw = pipeline::element("capsfilter")?;
    raw.set_property("caps", crate::quality::raw_caps(video));
    let encoder = configured_encoder(choice, video)?;
    let parser = pipeline::element("h264parse")?;
    parser.set_property("config-interval", -1i32);
    let h264 = pipeline::element("capsfilter")?;
    h264.set_property(
        "caps",
        gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .field("profile", video.profile.gst_name())
            .field("level", video.gst_level())
            .build(),
    );
    Ok([raw, encoder, parser, h264])
}

/// Reduce the RGB desktop before color conversion in the high-cadence mode.
/// Other modes keep their existing conversion/scaling and live renegotiation.
fn video_processing(mode: VideoMode) -> Result<Vec<gst::Element>, MediaError> {
    let convert = pipeline::element("videoconvert")?;
    convert.set_property("n-threads", 2u32);
    let scale = pipeline::element("videoscale")?;
    scale.set_property("n-threads", 2u32);
    // Preserve monitor aspect ratio using borders within the negotiated video mode.
    scale.set_property("add-borders", true);
    if mode == VideoMode::Hd60 {
        let size = pipeline::element("capsfilter")?;
        let (width, height) = mode.size();
        size.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", width as i32)
                .field("height", height as i32)
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                .build(),
        );
        Ok(vec![scale, size, convert])
    } else {
        Ok(vec![convert, scale])
    }
}

fn probe(
    choice: &EncoderChoice,
    size: (u32, u32),
    cancel: &CancellationToken,
) -> Result<(), MediaError> {
    let video = choice.offer.preferred;
    let src = pipeline::element("videotestsrc")?;
    src.set_property("is-live", true);
    // Exercise conversion/scaling from the authorized desktop size, rather than
    // only encoding an already reduced I420 test frame.
    let desktop = pipeline::element("capsfilter")?;
    desktop.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", "BGRx")
            .field("width", size.0 as i32)
            .field("height", size.1 as i32)
            .build(),
    );
    let processing = video_processing(video.mode)?;
    let chain = encoded_chain(choice, video)?;
    let sink = pipeline::element("fakesink")?;
    sink.set_property("sync", false);
    let pipeline = gst::Pipeline::new();
    let mut elements = vec![&src, &desktop];
    elements.extend(processing.iter());
    elements.extend(chain.iter());
    elements.push(&sink);
    pipeline.add_many(elements.iter().copied())?;
    gst::Element::link_many(elements.iter().copied())?;
    let guard = GstMediaPipeline {
        pipeline,
        sink,
        streaming: false,
        encoder: Some(chain[1].clone()),
        control: None,
        _source: CaptureSource::Test,
    };
    let bus = guard.pipeline.bus().ok_or(MediaError::NoBus)?;
    guard.start()?;
    // Exercise live fallback and recovery on this same encoder before offering it.
    let mut stages = vec![video];
    if let Some(fallback) = choice.offer.fallback {
        stages.extend([fallback, video]);
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    let stages = stages.iter().enumerate();
    for (index, stage) in stages {
        if index > 0 {
            set_bitrate(&chain[1], stage.mode.bitrate_range().0)?;
            chain[0].set_property("caps", crate::quality::raw_caps(*stage));
            guard.request_keyframe();
        }
        let mut first = None;
        let mut measured_from = Instant::now();
        loop {
            if cancel.is_cancelled() {
                return Err(MediaError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(MediaError::NoEncoder);
            }
            if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(20)) {
                match message.view() {
                    gst::MessageView::Error(error) => {
                        return Err(MediaError::Pipeline(error.error().to_string()));
                    }
                    gst::MessageView::Eos(_) => return Err(MediaError::NoEncoder),
                    _ => {}
                }
            }
            let size = guard
                .sink
                .static_pad("sink")
                .and_then(|pad| pad.current_caps())
                .and_then(|caps| {
                    caps.structure(0).and_then(|s| {
                        Some((
                            s.get::<i32>("width").ok()? as u32,
                            s.get::<i32>("height").ok()? as u32,
                        ))
                    })
                });
            if size == Some(stage.mode.size()) {
                let frames = guard.frames();
                let first = *first.get_or_insert_with(|| {
                    measured_from = Instant::now();
                    frames
                });
                let measured = frames.saturating_sub(first);
                let needed = if stage.mode == VideoMode::Hd60 {
                    119
                } else {
                    5
                };
                if measured >= needed {
                    if stage.mode == VideoMode::Hd60
                        && measured as f64 / measured_from.elapsed().as_secs_f64() < 57.0
                    {
                        return Err(MediaError::Pipeline(
                            "Encoder did not sustain the 60 fps target in the synthetic capture test"
                                .into(),
                        ));
                    }
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Inspect pixel geometry and source rate capabilities on the authorized stream.
/// No frames are mapped into Rust. This pipeline stops before activating P2P.
pub fn capture_format(
    source: CaptureSource,
    cancel: &CancellationToken,
) -> Result<CaptureFormat, MediaError> {
    gst::init()?;
    let src = pipeline::capture_element(&source)?;
    let sink = pipeline::element("fakesink")?;
    sink.set_property("sync", false);
    let pipeline = gst::Pipeline::new();
    pipeline.add_many([&src, &sink])?;
    src.link(&sink)?;
    let guard = GstMediaPipeline {
        pipeline,
        sink,
        streaming: false,
        encoder: None,
        control: None,
        _source: source,
    };
    let bus = guard.pipeline.bus().ok_or(MediaError::NoBus)?;
    guard.start()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if cancel.is_cancelled() {
            return Err(MediaError::Cancelled);
        }
        if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(50))
            && let gst::MessageView::Error(error) = message.view()
        {
            return Err(MediaError::Pipeline(error.error().to_string()));
        }
        if guard.frames() > 0
            && let Some(caps) = src.static_pad("src").and_then(|pad| pad.current_caps())
            && let Some(structure) = caps.structure(0)
        {
            let width = structure.get::<i32>("width").unwrap_or(0);
            let height = structure.get::<i32>("height").unwrap_or(0);
            if width > 0 && height > 0 {
                let wanted = gst::Caps::builder("video/x-raw")
                    .any_features()
                    .field("framerate", crate::quality::frame_rates(VideoMode::Hd60))
                    .build();
                let supports_60fps = src
                    .static_pad("src")
                    .is_some_and(|pad| !pad.query_caps(Some(&wanted)).is_empty());
                return Ok(CaptureFormat {
                    pixels: (width as u32, height as u32),
                    supports_60fps,
                });
            }
        }
    }
    Err(MediaError::NoGeometry)
}

/// Probe actual requested modes before P2P; registry presence does not prove usable hardware.
pub fn probe_encoders(
    preferred: VideoMode,
    size: (u32, u32),
    cancel: &CancellationToken,
) -> Result<Vec<EncoderChoice>, MediaError> {
    gst::init()?;
    let names = EncoderSelector::available();
    let candidates: Vec<_> = names.iter().map(String::as_str).collect();
    probe_modes(&candidates, preferred, size, cancel)
}

/// Hardware-free receivers/tests explicitly select software candidates.
pub fn probe_encoder_candidates(
    candidates: &[&str],
    cancel: &CancellationToken,
) -> Result<Vec<EncoderChoice>, MediaError> {
    probe_modes(candidates, VideoMode::Hd, VideoMode::Hd.size(), cancel)
}

pub fn probe_modes(
    candidates: &[&str],
    preferred: VideoMode,
    size: (u32, u32),
    cancel: &CancellationToken,
) -> Result<Vec<EncoderChoice>, MediaError> {
    gst::init()?;
    let mut choices = Vec::new();
    let modes = VideoMode::USER_MODES
        .into_iter()
        .rev()
        .filter(|mode| {
            *mode <= preferred
                && (*mode != VideoMode::Hd60 || preferred == VideoMode::Hd60)
                && (mode.fits(size) || *mode == VideoMode::Hd)
        })
        .chain([VideoMode::Vga]);
    for mode in modes {
        for &name in candidates {
            for profile in [H264Profile::High, H264Profile::Baseline] {
                if choices.iter().any(|c: &EncoderChoice| {
                    c.offer.preferred.mode == mode && c.offer.preferred.profile == profile
                }) {
                    continue;
                }
                let video = VideoFormat {
                    mode,
                    profile,
                    level: mode.minimum_level(),
                    r2: mode.requires_r2(),
                };
                let fallback = (mode > VideoMode::Hd && mode.fps() == VideoMode::Hd.fps())
                    .then_some(VideoFormat {
                        mode: VideoMode::Hd,
                        ..video
                    });
                let choice = EncoderChoice {
                    name: name.into(),
                    offer: VideoOffer {
                        preferred: video,
                        fallback,
                    },
                };
                let result = probe(&choice, size, cancel);
                match result {
                    Ok(()) => choices.push(choice),
                    Err(MediaError::Cancelled) => return Err(MediaError::Cancelled),
                    Err(error) => {
                        tracing::debug!(target:"wireless_display::media",encoder=name,?mode,%error,"Encoder rejected the test format")
                    }
                }
            }
        }
    }
    if choices.is_empty() {
        return Err(MediaError::NoEncoder);
    }
    Ok(choices)
}

/// Encoder properties have different units. Only known, live-mutable controls qualify.
pub(crate) fn set_bitrate(encoder: &gst::Element, kbps: u32) -> Result<(), MediaError> {
    let name = encoder
        .factory()
        .map(|factory| factory.name().to_string())
        .unwrap_or_default();
    let value = match name.as_str() {
        "openh264enc" => kbps.checked_mul(1000).ok_or(MediaError::NoEncoder)?,
        "x264enc" | "vah264enc" | "vaapih264enc" | "nvh264enc" | "nvautogpuh264enc"
        | "qsvh264enc" => kbps,
        _ => {
            return Err(MediaError::Element(format!(
                "{name}: bitrate control unavailable"
            )));
        }
    };
    let property = encoder
        .find_property("bitrate")
        .ok_or(MediaError::NoEncoder)?;
    if !property.flags().contains(gst::PARAM_FLAG_MUTABLE_PLAYING) {
        return Err(MediaError::Pipeline(format!(
            "{name}: the bitrate property does not advertise mutability in PLAYING"
        )));
    }
    if let Some(spec) = property.downcast_ref::<gst::glib::ParamSpecUInt>() {
        if value < spec.minimum() || value > spec.maximum() {
            return Err(MediaError::NoEncoder);
        }
        if name == "openh264enc" && value > encoder.property::<u32>("max-bitrate") {
            encoder.set_property("max-bitrate", value);
        }
        encoder.set_property("bitrate", value);
        if name == "openh264enc" {
            encoder.set_property("max-bitrate", value);
        }
    } else if let Some(spec) = property.downcast_ref::<gst::glib::ParamSpecInt>() {
        let value = i32::try_from(value).map_err(|_| MediaError::NoEncoder)?;
        if value < spec.minimum() || value > spec.maximum() {
            return Err(MediaError::NoEncoder);
        }
        encoder.set_property("bitrate", value);
    } else {
        return Err(MediaError::NoEncoder);
    }
    Ok(())
}

impl GstMediaPipeline {
    /// Compatibility alias for [`Self::try_new_streaming`].
    pub fn new_streaming(
        source: CaptureSource,
        plan: &NegotiatedStream,
        choice: &EncoderChoice,
        sockets: RtpSockets,
    ) -> Result<Self, MediaError> {
        Self::try_new_streaming(source, plan, choice, sockets)
    }

    /// Builds the negotiated H.264/MPEG-TS/RTP pipeline without sending media.
    ///
    /// Rejects a plan that differs from the tested encoder offer. Retains the
    /// capture FD and RTP/RTCP sockets until the pipeline is released.
    pub fn try_new_streaming(
        source: CaptureSource,
        plan: &NegotiatedStream,
        choice: &EncoderChoice,
        sockets: RtpSockets,
    ) -> Result<Self, MediaError> {
        gst::init()?;
        if choice.offer.preferred.profile != plan.video.profile
            || choice.offer.preferred.mode != plan.video.mode
            || choice.offer.preferred.level != plan.video.level
        {
            return Err(MediaError::NoEncoder);
        }
        let src = pipeline::capture_element(&source)?;
        let queue = pipeline::element("queue")?;
        queue.set_property("max-size-buffers", 2u32);
        queue.set_property("max-size-bytes", 0u32);
        queue.set_property("max-size-time", 0u64);
        queue.set_property_from_str("leaky", "downstream");
        let processing = video_processing(plan.video.mode)?;
        let rate = pipeline::element("videorate")?;
        if plan.video.mode == VideoMode::Hd60 {
            rate.set_property("drop-only", true);
        }
        let rate_caps = pipeline::element("capsfilter")?;
        rate_caps.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("framerate", crate::quality::frame_rates(plan.video.mode))
                .build(),
        );
        let chain = encoded_chain(choice, plan.video)?;
        let mux = pipeline::element("mpegtsmux")?;
        mux.set_property("alignment", 7i32);
        mux.set_property(
            "prog-map",
            gst::Structure::builder("program_map")
                .field("sink_4113", 1i32)
                .field("PMT_1", 4096u32)
                .field("PCR_1", "sink_4113")
                .build(),
        );
        let pay = pipeline::element("rtpmp2tpay")?;
        pay.set_property("pt", 33u32);
        pay.set_property("mtu", 1400u32);
        let ssrc = std::process::id().wrapping_add(gst::Seqnum::next().into_glib());
        pay.set_property("ssrc", ssrc);
        let telemetry = std::sync::Arc::new(crate::quality::Telemetry::default());
        telemetry.watch_queue(&queue);
        telemetry.watch_encoded(&chain[3]);
        let sink = pipeline::element("udpsink")?;
        sink.set_property("host", plan.peer.to_string());
        sink.set_property("port", i32::from(plan.client_ports.rtp));
        sink.set_property("socket", &sockets.rtp);
        sink.set_property("close-socket", false);
        sink.set_property("sync", false);
        sink.set_property("async", false);
        let pipeline = gst::Pipeline::new();
        let mut video = vec![&src, &rate, &rate_caps, &queue];
        video.extend(processing.iter());
        video.extend(chain.iter());
        pipeline.add_many(video.iter().copied())?;
        pipeline.add_many([&mux, &pay, &sink])?;
        gst::Element::link_many(video.iter().copied())?;
        let video_pad = mux
            .request_pad_simple("sink_4113")
            .ok_or_else(|| MediaError::Element("mpegtsmux sink_4113".into()))?;
        chain[3]
            .static_pad("src")
            .ok_or_else(|| MediaError::Element("h264 src pad".into()))?
            .link(&video_pad)
            .map_err(|e| MediaError::Element(e.to_string()))?;
        mux.link(&pay)?;
        if let Some(port) = plan.client_ports.rtcp {
            let bin = pipeline::element("rtpbin")?;
            // A connected UDP socket accepts RTCP only from this negotiated peer/port.
            let address = gio::InetAddress::from_string(&plan.peer.to_string())
                .ok_or(MediaError::NoEncoder)?;
            gio::prelude::SocketExt::connect(
                &sockets.rtcp,
                &gio::InetSocketAddress::new(&address, port),
                gio::Cancellable::NONE,
            )?;
            let rtcp_sink = pipeline::element("udpsink")?;
            rtcp_sink.set_property("host", plan.peer.to_string());
            rtcp_sink.set_property("port", i32::from(port));
            rtcp_sink.set_property("socket", &sockets.rtcp);
            rtcp_sink.set_property("close-socket", false);
            rtcp_sink.set_property("sync", false);
            rtcp_sink.set_property("async", false);
            let rtcp_src = pipeline::element("udpsrc")?;
            rtcp_src.set_property("socket", &sockets.rtcp);
            rtcp_src.set_property("close-socket", false);
            rtcp_src.set_property("caps", gst::Caps::builder("application/x-rtcp").build());
            telemetry.watch_rtcp(&rtcp_src, ssrc);
            pipeline.add_many([&bin, &rtcp_sink, &rtcp_src])?;
            pay.link_pads(Some("src"), &bin, Some("send_rtp_sink_0"))?;
            bin.link_pads(Some("send_rtp_src_0"), &sink, Some("sink"))?;
            bin.link_pads(Some("send_rtcp_src_0"), &rtcp_sink, Some("sink"))?;
            rtcp_src.link_pads(Some("src"), &bin, Some("recv_rtcp_sink_0"))?;
        } else {
            pay.link(&sink)?;
        }
        Ok(Self {
            pipeline,
            sink,
            streaming: true,
            encoder: Some(chain[1].clone()),
            control: Some(crate::quality::StreamingControl::new(
                chain[0].clone(),
                queue.static_pad("src").ok_or(MediaError::NoEncoder)?,
                telemetry,
            )),
            _source: source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wireless_display_core::MediaEvent;
    fn receiver() -> (GstMediaPipeline, u16) {
        gst::init().unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        let input = pipeline::element("udpsrc").unwrap();
        input.set_property("socket", gio::Socket::from_fd(socket.into()).unwrap());
        input.set_property(
            "caps",
            gst::Caps::builder("application/x-rtp")
                .field("media", "video")
                .field("encoding-name", "MP2T")
                .field("clock-rate", 90000i32)
                .field("payload", 33i32)
                .build(),
        );
        let depay = pipeline::element("rtpmp2tdepay").unwrap();
        let demux = pipeline::element("tsdemux").unwrap();
        let parse = pipeline::element("h264parse").unwrap();
        let decoder = pipeline::element("avdec_h264")
            .or_else(|_| pipeline::element("openh264dec"))
            .unwrap();
        let sink = pipeline::element("fakesink").unwrap();
        sink.set_property("sync", false);
        let receive = gst::Pipeline::new();
        receive
            .add_many([&input, &depay, &demux, &parse, &decoder, &sink])
            .unwrap();
        gst::Element::link_many([&input, &depay, &demux]).unwrap();
        gst::Element::link_many([&parse, &decoder, &sink]).unwrap();
        let parser = parse.clone();
        demux.connect_pad_added(move |_, pad| {
            if let Some(sink) = parser.static_pad("sink")
                && !sink.is_linked()
            {
                pad.link(&sink).unwrap();
            }
        });
        let receiver = GstMediaPipeline {
            pipeline: receive,
            sink,
            streaming: false,
            encoder: None,
            control: None,
            _source: CaptureSource::Test,
        };
        receiver.start().unwrap();
        (receiver, port)
    }

    #[tokio::test]
    async fn receiver_decodes_h264_frames_from_the_rtp_stream() {
        let (receiver, port) = receiver();
        let token = CancellationToken::new();
        let choices = probe_encoder_candidates(&["openh264enc", "x264enc"], &token).unwrap();
        let choice = &choices[0];
        let plan = NegotiatedStream {
            video: choice.offer.preferred,
            fallback: None,
            peer: Ipv4Addr::LOCALHOST,
            client_ports: RtpPorts {
                rtp: port,
                rtcp: None,
            },
        };
        let pipeline = GstMediaPipeline::try_new_streaming(
            CaptureSource::Test,
            &plan,
            choice,
            RtpSockets::bind(Ipv4Addr::LOCALHOST).unwrap(),
        )
        .unwrap();
        let (tx, _rx) = async_channel::bounded(32);
        let cancel = token.clone();
        let task = tokio::spawn(async move { pipeline.run(tx, cancel).await });
        let decoded = tokio::time::timeout(Duration::from_secs(8), async {
            while receiver.frames() < 3 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        token.cancel();
        task.await.unwrap().unwrap();
        receiver.stop().unwrap();
        decoded.unwrap();
    }

    #[tokio::test]
    async fn sixty_fps_reaches_the_decoder_and_reports_encoded_video_frames() {
        use std::sync::{Arc, Mutex};
        let (receiver, port) = receiver();
        let cancel = CancellationToken::new();
        // This fixture checks format/transport, not the machine's benchmark score.
        let video = VideoFormat {
            mode: VideoMode::Hd60,
            profile: H264Profile::High,
            level: 2,
            r2: false,
        };
        let choice = EncoderChoice {
            name: "x264enc".into(),
            offer: VideoOffer {
                preferred: video,
                fallback: None,
            },
        };
        let plan = NegotiatedStream {
            video,
            fallback: None,
            peer: Ipv4Addr::LOCALHOST,
            client_ports: RtpPorts {
                rtp: port,
                rtcp: None,
            },
        };
        let pipeline = Arc::new(
            GstMediaPipeline::try_new_streaming(
                CaptureSource::Test,
                &plan,
                &choice,
                RtpSockets::bind(Ipv4Addr::LOCALHOST).unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(
            pipeline
                .encoder
                .as_ref()
                .unwrap()
                .property::<u32>("bitrate"),
            8000
        );
        let timestamps = Arc::new(Mutex::new(Vec::new()));
        let seen = timestamps.clone();
        let mux = pipeline
            .pipeline
            .iterate_elements()
            .into_iter()
            .filter_map(Result::ok)
            .find(|element| {
                element
                    .factory()
                    .is_some_and(|factory| factory.name() == "mpegtsmux")
            })
            .unwrap();
        mux.static_pad("src").unwrap().add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
            move |_, info| {
                if let Some(buffer) = info.buffer() {
                    pes_timestamps(buffer, &seen);
                }
                if let Some(list) = info.buffer_list() {
                    for buffer in list.iter() {
                        pes_timestamps(buffer, &seen);
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );
        let (tx, rx) = async_channel::bounded(64);
        let task = tokio::spawn({
            let pipeline = pipeline.clone();
            let token = cancel.clone();
            async move { pipeline.run(tx, token).await }
        });
        wait_size(&receiver, VideoMode::Hd60.size()).await;
        let caps = receiver
            .sink
            .static_pad("sink")
            .unwrap()
            .current_caps()
            .unwrap();
        let rate = caps
            .structure(0)
            .unwrap()
            .get::<gst::Fraction>("framerate")
            .unwrap();
        assert!([gst::Fraction::new(60, 1), gst::Fraction::new(60_000, 1001)].contains(&rate));
        tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                if let wireless_display_core::MediaEvent::VideoStatistics(statistics) =
                    rx.recv().await.unwrap()
                {
                    assert!(
                        statistics.input_fps > 0.0 && statistics.encoded_fps > 0.0,
                        "count video access units, not RTP packets"
                    );
                    break;
                }
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        task.await.unwrap().unwrap();
        receiver.stop().unwrap();
        let timestamps = timestamps.lock().unwrap();
        let ticks = 90_000u64 * rate.denom() as u64;
        let divisor = rate.numer() as u64;
        let interval = ticks / divisor;
        let upper = ticks.div_ceil(divisor);
        assert!(timestamps.windows(2).any(|pair| ((interval-1)..=(upper+1)).contains(&pair[1].wrapping_sub(pair[0]))),"p60/59.94 needs a matching PES interval at 90 kHz");
    }

    fn pes_timestamps(buffer: &gst::BufferRef, timestamps: &std::sync::Mutex<Vec<u64>>) {
        if let Ok(map) = buffer.map_readable() {
            for ts in map.as_chunks::<188>().0 {
                let pid = ((u16::from(ts[1]) & 31) << 8) | u16::from(ts[2]);
                if ts[0] != 0x47 || pid != 0x1011 || ts[1] & 0x40 == 0 || ts[3] & 0x10 == 0 {
                    continue;
                }
                let offset = 4 + if ts[3] & 0x20 != 0 {
                    1 + usize::from(ts[4])
                } else {
                    0
                };
                if let Some(pes) = ts.get(offset..)
                    && pes.len() >= 14
                    && pes[..3] == [0, 0, 1]
                    && pes[7] & 0x80 != 0
                {
                    let p = &pes[9..14];
                    let pts = (u64::from((p[0] >> 1) & 7) << 30)
                        | (u64::from(p[1]) << 22)
                        | (u64::from(p[2] >> 1) << 15)
                        | (u64::from(p[3]) << 7)
                        | u64::from(p[4] >> 1);
                    timestamps.lock().unwrap().push(pts);
                }
            }
        }
    }

    async fn wait_size(receiver: &GstMediaPipeline, size: (u32, u32)) {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let current = receiver
                    .sink
                    .static_pad("sink")
                    .and_then(|pad| pad.current_caps())
                    .and_then(|caps| {
                        caps.structure(0).and_then(|s| {
                            Some((
                                s.get::<i32>("width").ok()? as u32,
                                s.get::<i32>("height").ok()? as u32,
                            ))
                        })
                    });
                if current == Some(size) && receiver.frames() >= 3 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .expect("decoder did not receive the requested geometry");
    }

    #[tokio::test]
    async fn live_full_hd_fallback_recovery_has_exact_pes_timing_and_cancellable_frame_gate() {
        use std::sync::{Arc, Mutex};
        use wireless_display_core::protocol::wfd::quality::QualityTarget;
        for name in ["x264enc", "openh264enc"] {
            let (receiver, port) = receiver();
            let cancel = CancellationToken::new();
            let choices = probe_modes(
                &[name],
                VideoMode::FullHd,
                VideoMode::FullHd.size(),
                &cancel,
            )
            .unwrap();
            let choice = choices
                .iter()
                .find(|c| c.offer.preferred.mode == VideoMode::FullHd)
                .expect("software encoder must support Full HD and fallback at one level");
            let plan = NegotiatedStream {
                video: choice.offer.preferred,
                fallback: choice.offer.fallback,
                peer: Ipv4Addr::LOCALHOST,
                client_ports: RtpPorts {
                    rtp: port,
                    rtcp: None,
                },
            };
            let pipeline = Arc::new(
                GstMediaPipeline::try_new_streaming(
                    CaptureSource::Test,
                    &plan,
                    choice,
                    RtpSockets::bind(Ipv4Addr::LOCALHOST).unwrap(),
                )
                .unwrap(),
            );
            let mux = pipeline
                .pipeline
                .iterate_elements()
                .into_iter()
                .filter_map(Result::ok)
                .find(|e| e.factory().is_some_and(|f| f.name() == "mpegtsmux"))
                .unwrap();
            let timestamps = Arc::new(Mutex::new(Vec::new()));
            let seen = timestamps.clone();
            mux.static_pad("src").unwrap().add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                move |_, info| {
                    if let Some(buffer) = info.buffer() {
                        pes_timestamps(buffer, &seen);
                    }
                    if let Some(list) = info.buffer_list() {
                        for buffer in list.iter() {
                            pes_timestamps(buffer, &seen);
                        }
                    }
                    gst::PadProbeReturn::Ok
                },
            );
            let parsed = Arc::new(Mutex::new(Vec::new()));
            let seen = parsed.clone();
            let parser = pipeline
                .pipeline
                .iterate_elements()
                .into_iter()
                .filter_map(Result::ok)
                .find(|e| e.factory().is_some_and(|f| f.name() == "h264parse"))
                .unwrap();
            parser.static_pad("src").unwrap().add_probe(
                gst::PadProbeType::BUFFER,
                move |pad, info| {
                    if let Some(buffer) = info.buffer()
                        && let Some(caps) = pad.current_caps()
                        && let Some(s) = caps.structure(0)
                        && let Ok(width) = s.get::<i32>("width")
                    {
                        let pts = buffer
                            .pts()
                            .and_then(|pts| {
                                pad.sticky_event::<gst::event::Segment>(0)
                                    .and_then(|event| {
                                        event
                                            .segment()
                                            .downcast_ref::<gst::ClockTime>()
                                            .and_then(|segment| segment.to_running_time(pts))
                                    })
                            })
                            .map(|time| {
                                ((u128::from(time.nseconds()) * 90_000 / 1_000_000_000) as u64
                                    + 3600 * 90_000)
                                    & ((1u64 << 33) - 1)
                            });
                        seen.lock().unwrap().push((
                            width,
                            !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
                            pts,
                        ));
                    }
                    gst::PadProbeReturn::Ok
                },
            );
            let (tx, _rx) = async_channel::bounded(128);
            let task = tokio::spawn({
                let pipeline = pipeline.clone();
                let token = cancel.clone();
                async move { pipeline.run(tx, token).await }
            });
            wait_size(&receiver, VideoMode::FullHd.size()).await;
            let before_bitrate = receiver.frames();
            pipeline.set_bitrate(16000).unwrap();
            assert_eq!(
                pipeline
                    .encoder
                    .as_ref()
                    .unwrap()
                    .property::<u32>("bitrate"),
                if name == "openh264enc" {
                    16_000_000
                } else {
                    16000
                }
            );
            tokio::time::timeout(Duration::from_secs(5), async {
                while receiver.frames() < before_bitrate + 3 {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
            })
            .await
            .expect("receiver must continue decoding after the live 16 Mbit/s change");
            for video in [plan.fallback.unwrap(), plan.video] {
                let target = QualityTarget {
                    video,
                    bitrate_kbps: video.mode.bitrate_range().0,
                };
                let prepared = pipeline.prepare_format(target, &cancel).await.unwrap();
                let timing = prepared.pts90k;
                let first = parsed.lock().unwrap().len();
                pipeline.commit_format(prepared).unwrap();
                wait_size(&receiver, video.mode.size()).await;
                let first_new = parsed.lock().unwrap()[first..]
                    .iter()
                    .find(|(width, _, _)| *width == video.mode.size().0 as i32)
                    .copied()
                    .unwrap();
                assert!(first_new.1, "first frame in changed format must be an IDR");
                assert_eq!(
                    first_new.2,
                    Some(timing),
                    "{name}: first changed access unit must match the announced timing"
                );
                let actual = timestamps.lock().unwrap().clone();
                assert!(
                    actual.contains(&timing),
                    "{name}: M4 timing {timing} must exist in PES; actual {actual:?}"
                );
            }
            let prepared = pipeline
                .prepare_format(
                    QualityTarget {
                        video: plan.fallback.unwrap(),
                        bitrate_kbps: 4000,
                    },
                    &cancel,
                )
                .await
                .unwrap();
            cancel.cancel();
            tokio::time::timeout(Duration::from_secs(3),task).await.expect("cancellation must release the blocked frame even while PreparedFormat is retained").unwrap().unwrap();
            drop(prepared);
            receiver.stop().unwrap();
        }
    }

    #[tokio::test]
    async fn sends_rtcp_sender_reports_when_receiver_negotiates_rtcp() {
        let cancel = CancellationToken::new();
        let choices = probe_encoder_candidates(&["openh264enc", "x264enc"], &cancel).unwrap();
        let choice = &choices[0];
        let rtp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rtcp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let plan = NegotiatedStream {
            video: choice.offer.preferred,
            fallback: None,
            peer: Ipv4Addr::LOCALHOST,
            client_ports: RtpPorts {
                rtp: rtp.local_addr().unwrap().port(),
                rtcp: Some(rtcp.local_addr().unwrap().port()),
            },
        };
        let pipeline = GstMediaPipeline::try_new_streaming(
            CaptureSource::Test,
            &plan,
            choice,
            RtpSockets::bind(Ipv4Addr::LOCALHOST).unwrap(),
        )
        .unwrap();
        let (tx, rx) = async_channel::bounded(32);
        let token = cancel.clone();
        let task = tokio::spawn(async move { pipeline.run(tx, token).await });
        let mut bytes = [0u8; 2048];
        let report = tokio::time::timeout(Duration::from_secs(8), rtcp.recv_from(&mut bytes)).await;
        let (size, source) = report.unwrap().unwrap();
        assert!(size >= 28);
        assert_eq!(bytes[0] >> 6, 2);
        assert_eq!(bytes[1], 200);
        let ssrc = &bytes[4..8];
        let mut rr = vec![0x81, 201, 0, 7, 0, 0, 0, 99];
        rr.extend(ssrc);
        rr.extend([64, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        rtcp.send_to(&rr, source).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5),async {
            loop { if matches!(rx.recv().await.unwrap(), wireless_display_core::MediaEvent::Quality(sample) if sample.loss == Some(64)) { break; } }
        }).await.unwrap();
        cancel.cancel();
        task.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn sends_real_rtp_with_transport_stream() {
        let cancel = CancellationToken::new();
        let choices = probe_encoder_candidates(&["openh264enc", "x264enc"], &cancel).unwrap();
        let choice = &choices[0];
        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let plan = NegotiatedStream {
            video: choice.offer.preferred,
            fallback: None,
            peer: Ipv4Addr::LOCALHOST,
            client_ports: RtpPorts {
                rtp: receiver.local_addr().unwrap().port(),
                rtcp: None,
            },
        };
        let pipeline = GstMediaPipeline::try_new_streaming(
            CaptureSource::Test,
            &plan,
            choice,
            RtpSockets::bind(Ipv4Addr::LOCALHOST).unwrap(),
        )
        .unwrap();
        let (tx, rx) = async_channel::bounded::<MediaEvent>(32);
        let token = cancel.clone();
        let task = tokio::spawn(async move { pipeline.run(tx, token).await });
        let mut bytes = [0u8; 2048];
        let result =
            tokio::time::timeout(Duration::from_secs(8), receiver.recv_from(&mut bytes)).await;
        cancel.cancel();
        task.await.unwrap().unwrap();
        let (size, _) = result.unwrap().unwrap();
        assert!(size >= 12 + 188);
        assert_eq!(bytes[0] >> 6, 2);
        assert_eq!(bytes[1] & 0x7f, 33);
        let offset = 12 + 4 * usize::from(bytes[0] & 0xf);
        assert_eq!(bytes[offset], 0x47);
        assert_eq!((size - offset) % 188, 0);
        drop(rx);
    }
}
