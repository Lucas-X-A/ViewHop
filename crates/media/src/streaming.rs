use crate::encoder::set_bitrate;
use crate::pipeline;
use crate::{
    CaptureMemory, EncoderInputFormat, EncoderSelector, GstMediaPipeline, MediaError,
    VideoProcessing,
};
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
    pub input_format: EncoderInputFormat,
    pub processing: VideoProcessing,
    pub capture_memory: CaptureMemory,
    pub offer: VideoOffer,
}

#[derive(Clone, Copy, Debug)]
pub struct CaptureFormat {
    pub pixels: (u32, u32),
    /// Source caps accept nominal p60 or 59.94 Hz, possibly after renegotiation.
    pub supports_60fps: bool,
}

fn watch_video_caps(
    element: &gst::Element,
    pad_name: &str,
    stage: &'static str,
    encoder: &str,
) -> Result<(), MediaError> {
    let pad = element.static_pad(pad_name).ok_or(MediaError::NoEncoder)?;
    let encoder = encoder.to_owned();
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(event) = info.event()
            && let gst::EventView::Caps(event) = event.view()
            && let Some(video) = event.caps().structure(0)
        {
            // Record negotiated metadata only. Caps features describe the memory
            // contract, not proof of zero-copy or a physical VRAM allocation.
            let features = event
                .caps()
                .features(0)
                .map(ToString::to_string)
                .unwrap_or_default();
            tracing::info!(target: "wireless_display::media", stage, %encoder,
                raw_format = video.get::<&str>("format").unwrap_or("unknown"),
                width = ?video.get::<i32>("width").ok(),
                height = ?video.get::<i32>("height").ok(),
                framerate = ?video.get::<gst::Fraction>("framerate").ok(),
                memory_features = %features,
                "Negotiated video input");
        }
        gst::PadProbeReturn::Ok
    });
    Ok(())
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

fn encoded_chain(
    choice: &EncoderChoice,
    video: VideoFormat,
) -> Result<[gst::Element; 4], MediaError> {
    if choice.capture_memory == CaptureMemory::DmaBuf && !choice.processing.is_gpu() {
        return Err(MediaError::NoEncoder);
    }
    let raw = pipeline::element("capsfilter")?;
    let caps = choice.processing.encoder_caps(video, choice.input_format);
    raw.set_property("caps", &caps);
    let encoder = crate::encoder::configured_encoder(&choice.name, video)?;
    let pad = encoder.static_pad("sink").ok_or(MediaError::NoEncoder)?;
    if !pad.query_caps(None).can_intersect(&caps) {
        return Err(MediaError::Element(format!(
            "{}: tested input format {} is no longer supported",
            choice.name,
            choice.input_format.name()
        )));
    }
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

fn probe(
    choice: &EncoderChoice,
    size: (u32, u32),
    cancel: &CancellationToken,
) -> Result<(), MediaError> {
    probe_source(choice, CaptureSource::Test, Some(size), cancel)
}

/// Validate import/processing/encoding on the already authorized capture.
/// This sends no network packets and never maps frame data into Rust.
pub fn probe_capture(
    choice: &EncoderChoice,
    source: CaptureSource,
    cancel: &CancellationToken,
) -> Result<(), MediaError> {
    probe_source(choice, source, None, cancel)
}

fn probe_source(
    choice: &EncoderChoice,
    source: CaptureSource,
    synthetic_size: Option<(u32, u32)>,
    cancel: &CancellationToken,
) -> Result<(), MediaError> {
    if cancel.is_cancelled() {
        return Err(MediaError::Cancelled);
    }
    let video = choice.offer.preferred;
    let src = pipeline::capture_element(&source)?;
    let processing = choice.processing.build(video.mode)?;
    // Exercise conversion/scaling from the authorized desktop size, rather than
    // only encoding an already reduced I420 test frame.
    let desktop = pipeline::element("capsfilter")?;
    let desktop_caps = if let Some(size) = synthetic_size {
        let input = processing
            .first()
            .and_then(|element| element.static_pad("sink"))
            .ok_or(MediaError::NoEncoder)?
            .query_caps(None);
        ["BGRx", "BGRA", "RGBA"]
            .into_iter()
            .map(|format| {
                gst::Caps::builder("video/x-raw")
                    .field("format", format)
                    .field("width", size.0 as i32)
                    .field("height", size.1 as i32)
                    .build()
            })
            .find(|caps| input.can_intersect(caps))
            .ok_or(MediaError::NoEncoder)?
    } else {
        gst::Caps::builder("video/x-raw").any_features().build()
    };
    desktop.set_property("caps", desktop_caps);
    let [rate, rate_caps, queue] =
        crate::processing::capture_pacing(video.mode, choice.capture_memory)?;
    let chain = encoded_chain(choice, video)?;
    let sink = pipeline::element("fakesink")?;
    sink.set_property("sync", false);
    let pipeline = gst::Pipeline::new();
    let mut elements = vec![&src, &desktop, &rate, &rate_caps, &queue];
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
        _source: source,
    };
    let bus = guard.pipeline.bus().ok_or(MediaError::NoBus)?;
    guard.start()?;
    // Exercise live fallback and recovery on this same encoder before offering it.
    let mut stages = vec![(video, video.mode.bitrate_range().0)];
    if synthetic_size.is_some() {
        if video.mode == VideoMode::FullHd {
            stages.push((video, video.mode.bitrate_range().1));
        }
        if let Some(fallback) = choice.offer.fallback {
            stages.extend([
                (fallback, fallback.mode.bitrate_range().0),
                (video, video.mode.bitrate_range().0),
            ]);
        }
    }
    let deadline =
        Instant::now() + Duration::from_secs(if choice.processing.is_gpu() { 8 } else { 4 });
    let stages = stages.iter().enumerate();
    for (index, (stage, bitrate)) in stages {
        if index > 0 {
            set_bitrate(&chain[1], *bitrate)?;
            chain[0].set_property(
                "caps",
                choice.processing.encoder_caps(*stage, choice.input_format),
            );
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
                let needed = if synthetic_size.is_none() {
                    // Static portal desktops can be sparse. One real encoded
                    // frame proves import; cadence is measured by synthetic preflight.
                    0
                } else if stage.mode == VideoMode::Hd60 {
                    119
                } else {
                    5
                };
                if frames > 0 && measured >= needed {
                    if synthetic_size.is_some()
                        && stage.mode == VideoMode::Hd60
                        && measured as f64 / measured_from.elapsed().as_secs_f64() < 57.0
                    {
                        return Err(MediaError::Pipeline(
                            "Encoder did not sustain the 60 fps target in the synthetic capture test"
                                .into(),
                        ));
                    }
                    let encoder_caps = chain[1]
                        .static_pad("sink")
                        .and_then(|pad| pad.current_caps())
                        .ok_or(MediaError::NoEncoder)?;
                    if !encoder_caps
                        .is_subset(&choice.processing.encoder_caps(*stage, choice.input_format))
                    {
                        return Err(MediaError::NoEncoder);
                    }
                    let capture_caps = src
                        .static_pad("src")
                        .and_then(|pad| pad.current_caps())
                        .ok_or(MediaError::NoEncoder)?;
                    let dma_buf = capture_caps
                        .features(0)
                        .is_some_and(|features| features.contains("memory:DMABuf"));
                    if dma_buf != (choice.capture_memory == CaptureMemory::DmaBuf) {
                        return Err(MediaError::NoEncoder);
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
    excluded: &[(String, VideoProcessing)],
    cancel: &CancellationToken,
) -> Result<Vec<EncoderChoice>, MediaError> {
    gst::init()?;
    let names = EncoderSelector::available();
    let candidates: Vec<_> = names.iter().map(String::as_str).collect();
    probe_modes_excluding(&candidates, preferred, size, excluded, cancel)
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
    probe_modes_excluding(candidates, preferred, size, &[], cancel)
}

fn probe_modes_excluding(
    candidates: &[&str],
    preferred: VideoMode,
    size: (u32, u32),
    excluded: &[(String, VideoProcessing)],
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
                for processing in VideoProcessing::candidates(name, |name| {
                    gst::ElementFactory::find(name).is_some()
                })
                .into_iter()
                .filter(|path| {
                    *path == VideoProcessing::Cpu
                        || !excluded
                            .iter()
                            .any(|(encoder, rejected)| encoder == name && rejected == path)
                }) {
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
                    let result = (|| {
                        let encoder = crate::encoder::configured_encoder(name, video)?;
                        let supported = encoder
                            .static_pad("sink")
                            .ok_or(MediaError::NoEncoder)?
                            .query_caps(None);
                        let input_format =
                            EncoderInputFormat::select(name, &supported, video, processing)?;
                        // Release any device resources before the live probe builds
                        // its own encoder instance.
                        drop(encoder);
                        let choice = EncoderChoice {
                            name: name.into(),
                            input_format,
                            processing,
                            capture_memory: CaptureMemory::System,
                            offer: VideoOffer {
                                preferred: video,
                                fallback,
                            },
                        };
                        probe(&choice, size, cancel)?;
                        Ok(choice)
                    })();
                    match result {
                        Ok(choice) => choices.push(choice),
                        Err(MediaError::Cancelled) => return Err(MediaError::Cancelled),
                        Err(error) => {
                            tracing::debug!(target:"wireless_display::media",encoder=name,?mode,?processing,%error,"Encoder rejected the test format")
                        }
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
        let processing = choice.processing.build(plan.video.mode)?;
        let [rate, rate_caps, queue] =
            crate::processing::capture_pacing(plan.video.mode, choice.capture_memory)?;
        let chain = encoded_chain(choice, plan.video)?;
        watch_video_caps(&src, "src", "capture", &choice.name)?;
        watch_video_caps(&chain[0], "src", "encoder-input", &choice.name)?;
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
            let address = gio::InetAddress::from_string(&plan.peer.to_string())
                .ok_or(MediaError::NoEncoder)?;
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
            rtcp_src.set_property("retrieve-sender-address", true);
            rtcp_src.set_property("caps", gst::Caps::builder("application/x-rtcp").build());
            telemetry.watch_rtcp(&rtcp_src, ssrc, address);
            pipeline.add_many([&bin, &rtcp_sink, &rtcp_src])?;
            pay.link_pads(Some("src"), &bin, Some("send_rtp_sink_0"))?;
            bin.link_pads(Some("send_rtp_src_0"), &sink, Some("sink"))?;
            bin.link_pads(Some("send_rtcp_src_0"), &rtcp_sink, Some("sink"))?;
            rtcp_src.link_pads(Some("src"), &bin, Some("recv_rtcp_sink_0"))?;
        } else {
            pay.link(&sink)?;
        }
        tracing::info!(target: "wireless_display::media", encoder = %choice.name,
            processing = ?choice.processing, capture_memory = ?choice.capture_memory,
            "Video processing configured");
        Ok(Self {
            pipeline,
            sink,
            streaming: true,
            encoder: Some(chain[1].clone()),
            control: Some(crate::quality::StreamingControl::new(
                chain[0].clone(),
                choice.input_format,
                choice.processing,
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

    #[test]
    fn capture_probe_and_exclusions_preserve_a_working_software_path() {
        let cancel = CancellationToken::new();
        let choices = probe_modes_excluding(
            &["x264enc"],
            VideoMode::Hd,
            VideoMode::Hd.size(),
            &[
                ("vah264enc".into(), VideoProcessing::Va),
                ("vaapih264enc".into(), VideoProcessing::Vaapi),
                ("nvh264enc".into(), VideoProcessing::Cuda),
                ("x264enc".into(), VideoProcessing::Cpu),
            ],
            &cancel,
        )
        .unwrap();
        let mut choice = choices[0].clone();
        assert_eq!(choice.processing, VideoProcessing::Cpu);
        probe_capture(&choice, CaptureSource::Test, &cancel).unwrap();
        choice.capture_memory = CaptureMemory::DmaBuf;
        assert!(probe_capture(&choice, CaptureSource::Test, &cancel).is_err());
        choice.capture_memory = CaptureMemory::System;
        cancel.cancel();
        assert!(matches!(
            probe_capture(&choice, CaptureSource::Test, &cancel),
            Err(MediaError::Cancelled)
        ));
    }

    #[test]
    #[ignore = "requires a GPU, compatible drivers, and encoder/postprocessor plugins; synthetic video only"]
    fn gpu_processing_encodes_and_preserves_live_bitrate_and_resolution_changes() {
        let cancel = CancellationToken::new();
        let choices =
            probe_encoders(VideoMode::FullHd, VideoMode::FullHd.size(), &[], &cancel).unwrap();
        let choice = choices
            .iter()
            .find(|choice| {
                choice.processing.is_gpu() && choice.offer.preferred.mode == VideoMode::FullHd
            })
            .expect(
                "no GPU path passed preflight; verify driver, plugins and live bitrate support",
            );
        // Synthetic preflight exercises the same processing graph as streaming,
        // including 8/16 Mbit/s and 1080p/720p/1080p before returning a choice.
        probe(choice, VideoMode::FullHd.size(), &cancel).unwrap();
    }

    #[test]
    fn rejected_encoder_keeps_the_software_fallback_available() {
        let choices = probe_encoder_candidates(
            &["identity", "x264enc", "openh264enc"],
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(!choices.is_empty());
        assert!(
            choices
                .iter()
                .all(|choice| matches!(choice.name.as_str(), "x264enc" | "openh264enc"))
        );
        assert!(
            choices
                .iter()
                .all(|choice| choice.input_format == EncoderInputFormat::I420)
        );
    }

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
            input_format: EncoderInputFormat::I420,
            processing: VideoProcessing::Cpu,
            capture_memory: CaptureMemory::System,
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
        for (name, input_format) in [
            ("x264enc", EncoderInputFormat::I420),
            ("x264enc", EncoderInputFormat::Nv12),
            ("openh264enc", EncoderInputFormat::I420),
        ] {
            let (receiver, port) = receiver();
            let cancel = CancellationToken::new();
            let choices = probe_modes(
                &[name],
                VideoMode::FullHd,
                VideoMode::FullHd.size(),
                &cancel,
            )
            .unwrap();
            let mut choice = choices
                .iter()
                .find(|c| c.offer.preferred.mode == VideoMode::FullHd)
                .cloned()
                .expect("software encoder must support Full HD and fallback at one level");
            // Exercise NV12 through a real encoder/decoder without needing a GPU.
            if choice.input_format != input_format {
                choice.input_format = input_format;
                probe(&choice, VideoMode::FullHd.size(), &cancel).unwrap();
            }
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
                    &choice,
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
                let caps = pipeline
                    .encoder
                    .as_ref()
                    .unwrap()
                    .static_pad("sink")
                    .unwrap()
                    .current_caps()
                    .unwrap();
                assert_eq!(
                    caps.structure(0).unwrap().get::<&str>("format").unwrap(),
                    input_format.name()
                );
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
        // A valid report from a different host must not reach the controller.
        let other_host = tokio::net::UdpSocket::bind("127.0.0.2:0").await.unwrap();
        other_host.send_to(&rr, source).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let MediaEvent::Quality(sample) = rx.recv().await.unwrap() {
                    assert_eq!(sample.loss, None);
                    break;
                }
            }
        })
        .await
        .unwrap();
        // SETUP advertises where the TV receives RTCP; its feedback may use a
        // separate sending socket, as observed with the physical receiver.
        let feedback = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert_ne!(
            feedback.local_addr().unwrap().port(),
            rtcp.local_addr().unwrap().port()
        );
        feedback.send_to(&rr, source).await.unwrap();
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
