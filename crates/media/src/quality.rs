use crate::streaming;
use crate::{GstMediaPipeline, MediaError};
use gst::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use wireless_display_core::VideoStatistics;
use wireless_display_core::protocol::wfd::quality::{QualitySample, QualityTarget};
use wireless_display_core::protocol::wfd::video::{VideoFormat, VideoMode};

pub(crate) struct Telemetry {
    overruns: AtomicU64,
    input_frames: AtomicU64,
    encoded_frames: AtomicU64,
    sampled_at: Mutex<Instant>,
    report: Mutex<ReceiverReport>,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self {
            overruns: AtomicU64::new(0),
            input_frames: AtomicU64::new(0),
            encoded_frames: AtomicU64::new(0),
            sampled_at: Mutex::new(Instant::now()),
            report: Mutex::new(ReceiverReport::default()),
        }
    }
}

#[derive(Default)]
struct ReceiverReport {
    sequence: Option<u32>,
    loss: Option<u8>,
}

impl Telemetry {
    pub fn watch_queue(self: &Arc<Self>, queue: &gst::Element) {
        let telemetry = self.clone();
        if let Some(pad) = queue.static_pad("sink") {
            let input = self.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                input.input_frames.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        }
        queue.connect("overrun", false, move |_| {
            telemetry.overruns.fetch_add(1, Ordering::Relaxed);
            None
        });
    }

    pub fn watch_encoded(self: &Arc<Self>, output: &gst::Element) {
        let telemetry = self.clone();
        if let Some(pad) = output.static_pad("src") {
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if info.buffer().is_some_and(|buffer| buffer.pts().is_some()) {
                    telemetry.encoded_frames.fetch_add(1, Ordering::Relaxed);
                }
                gst::PadProbeReturn::Ok
            });
        }
    }

    pub fn watch_rtcp(self: &Arc<Self>, src: &gst::Element, ssrc: u32) {
        let telemetry = self.clone();
        if let Some(pad) = src.static_pad("src") {
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buffer) = info.buffer()
                    && let Ok(map) = buffer.map_readable()
                    && let Some((sequence, loss)) = receiver_report(&map, ssrc)
                    && let Ok(mut report) = telemetry.report.lock()
                    && report
                        .sequence
                        .is_none_or(|old| sequence != old && sequence.wrapping_sub(old) < (1 << 31))
                {
                    report.sequence = Some(sequence);
                    report.loss = Some(loss);
                }
                gst::PadProbeReturn::Ok
            });
        }
    }

    pub fn sample(&self) -> (QualitySample, VideoStatistics) {
        let seconds = self
            .sampled_at
            .lock()
            .map(|mut time| {
                let seconds = time.elapsed().as_secs_f64();
                *time = Instant::now();
                seconds.max(0.001)
            })
            .unwrap_or(2.0);
        let input = self.input_frames.swap(0, Ordering::Relaxed);
        let encoded = self.encoded_frames.swap(0, Ordering::Relaxed);
        let dropped = self.overruns.swap(0, Ordering::Relaxed);
        let statistics = VideoStatistics {
            input_fps: input as f64 / seconds,
            encoded_fps: encoded as f64 / seconds,
            dropped_frames: dropped,
        };
        (
            QualitySample {
                // At least three dropped input frames in two seconds. videorate runs
                // before this queue, so intentional 60-to-30 conversion is excluded.
                overloaded: dropped >= 3,
                loss: self
                    .report
                    .lock()
                    .ok()
                    .and_then(|mut report| report.loss.take()),
                statistics: Some(statistics),
            },
            statistics,
        )
    }
}

/// Validate a compound RTCP packet before accepting a report about our SSRC.
/// The caller also filters the negotiated source IP and port with a connected socket.
fn receiver_report(mut bytes: &[u8], ssrc: u32) -> Option<(u32, u8)> {
    let mut found = None;
    while !bytes.is_empty() {
        if bytes.len() < 4 || bytes[0] >> 6 != 2 {
            return None;
        }
        let size = (usize::from(u16::from_be_bytes([bytes[2], bytes[3]])) + 1) * 4;
        if size > bytes.len() || size < 4 {
            return None;
        }
        let mut payload = &bytes[..size];
        if bytes[0] & 0x20 != 0 {
            if size != bytes.len() {
                return None;
            }
            let padding = usize::from(*payload.last()?);
            if padding == 0 || padding > size - 4 {
                return None;
            }
            payload = &payload[..size - padding];
        }
        let offset = match bytes[1] {
            200 => Some(28),
            201 => Some(8),
            _ => None,
        };
        if let Some(offset) = offset {
            let count = usize::from(bytes[0] & 31);
            if offset + 24 * count > payload.len() {
                return None;
            }
            for block in payload[offset..offset + 24 * count].as_chunks::<24>().0 {
                if u32::from_be_bytes(block[0..4].try_into().ok()?) == ssrc {
                    found = Some((u32::from_be_bytes(block[8..12].try_into().ok()?), block[4]));
                }
            }
        }
        bytes = &bytes[size..];
    }
    found
}

pub(crate) struct StreamingControl {
    raw: gst::Element,
    gate: Arc<FormatGate>,
    pub telemetry: Arc<Telemetry>,
}

impl StreamingControl {
    pub fn new(raw: gst::Element, gate: gst::Pad, telemetry: Arc<Telemetry>) -> Self {
        Self {
            raw,
            gate: Arc::new(FormatGate {
                pad: gate,
                probe: Mutex::new(None),
            }),
            telemetry,
        }
    }
}

struct FormatGate {
    pad: gst::Pad,
    probe: Mutex<Option<gst::PadProbeId>>,
}

impl FormatGate {
    fn release(&self) {
        if let Ok(mut probe) = self.probe.lock()
            && let Some(id) = probe.take()
        {
            self.pad.remove_probe(id);
        }
    }
}

impl StreamingControl {
    pub fn release_frame(&self) {
        self.gate.release();
    }
}

/// Owns a blocked raw frame. Dropping it always releases the streaming thread.
#[must_use = "retain the prepared frame until the RTSP format change is acknowledged"]
pub struct PreparedFormat {
    gate: Arc<FormatGate>,
    pub target: QualityTarget,
    pub pts90k: u64,
}

impl Drop for PreparedFormat {
    fn drop(&mut self) {
        self.gate.release();
    }
}

pub(crate) fn frame_rates(mode: VideoMode) -> gst::List {
    let fps = mode.fps() as i32;
    if mode == VideoMode::Hd60 {
        // WFD p60 also includes 59.94 Hz. Never pad a 30 Hz source with duplicates.
        gst::List::new([gst::Fraction::new(60, 1), gst::Fraction::new(60_000, 1001)])
    } else {
        gst::List::new([gst::Fraction::new(fps, 1)])
    }
}

pub(crate) fn raw_caps(video: VideoFormat) -> gst::Caps {
    let (width, height) = video.mode.size();
    gst::Caps::builder("video/x-raw")
        .field("format", "I420")
        .field("width", width as i32)
        .field("height", height as i32)
        .field("framerate", frame_rates(video.mode))
        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
        .build()
}

impl GstMediaPipeline {
    /// Changes the encoder target without renegotiating the video format.
    pub fn set_bitrate(&self, kbps: u32) -> Result<(), MediaError> {
        streaming::set_bitrate(self.encoder.as_ref().ok_or(MediaError::NoEncoder)?, kbps)
    }

    /// Blocks one raw frame and reports its MPEG-TS timestamp for RTSP M4.
    ///
    /// Keep the returned guard until the receiver acknowledges the change.
    /// Cancellation, failure, and dropping the guard release the blocked frame.
    pub async fn prepare_format(
        &self,
        target: QualityTarget,
        cancel: &CancellationToken,
    ) -> Result<PreparedFormat, MediaError> {
        let control = self.control.as_ref().ok_or(MediaError::NoEncoder)?;
        let (tx, rx) = oneshot::channel();
        let tx = Mutex::new(Some(tx));
        {
            let mut guard = control
                .gate
                .probe
                .lock()
                .map_err(|_| MediaError::NoEncoder)?;
            if guard.is_some() {
                return Err(MediaError::Pipeline(
                    "Resolution change already prepared".into(),
                ));
            }
            let probe = control
                .gate
                .pad
                .add_probe(
                    gst::PadProbeType::BLOCK | gst::PadProbeType::BUFFER,
                    move |pad, info| {
                        if let Ok(mut tx) = tx.lock()
                            && let Some(tx) = tx.take()
                        {
                            let time =
                                info.buffer()
                                    .and_then(|buffer| buffer.pts())
                                    .and_then(|pts| {
                                        pad.sticky_event::<gst::event::Segment>(0).and_then(
                                            |event| {
                                                event
                                                    .segment()
                                                    .downcast_ref::<gst::ClockTime>()
                                                    .and_then(|segment| {
                                                        segment.to_running_time(pts)
                                                    })
                                            },
                                        )
                                    });
                            let _ = tx.send(time);
                        }
                        gst::PadProbeReturn::Ok
                    },
                )
                .ok_or(MediaError::NoFormatFrame)?;
            *guard = Some(probe);
        }
        let mut prepared = PreparedFormat {
            gate: control.gate.clone(),
            target,
            pts90k: 0,
        };
        let time = tokio::select! {
            _ = cancel.cancelled() => return Err(MediaError::Cancelled),
            result = tokio::time::timeout(Duration::from_secs(2),rx) => result.map_err(|_| MediaError::NoFormatFrame)?.map_err(|_| MediaError::NoFormatFrame)?.ok_or(MediaError::NoFormatFrame)?,
        };
        // GstBaseTsMux maps running-time to its one-hour MPEG-TS timestamp base.
        // No B-frames: DTS == PTS. Transport tests verify the PES timestamp itself.
        prepared.pts90k = ((u128::from(time.nseconds()) * 90_000 / 1_000_000_000) as u64
            + 3600 * 90_000)
            & ((1u64 << 33) - 1);
        Ok(prepared)
    }

    /// Applies an acknowledged format, requests an IDR, then releases the frame.
    pub fn commit_format(&self, prepared: PreparedFormat) -> Result<(), MediaError> {
        let control = self.control.as_ref().ok_or(MediaError::NoEncoder)?;
        self.set_bitrate(prepared.target.bitrate_kbps)?;
        control
            .raw
            .set_property("caps", raw_caps(prepared.target.video));
        self.request_keyframe();
        let _ = control.telemetry.sample();
        // Release only after caps and the keyframe request have been installed.
        drop(prepared);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wireless_display_core::traits::MediaPipeline;
    #[test]
    fn sixty_fps_accepts_5994_but_cannot_duplicate_a_thirty_hz_source() {
        gst::init().unwrap();
        for fps in [gst::Fraction::new(60_000, 1001), gst::Fraction::new(30, 1)] {
            let src = crate::pipeline::element("videotestsrc").unwrap();
            src.set_property("num-buffers", 6i32);
            let input = crate::pipeline::element("capsfilter").unwrap();
            input.set_property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", "I420")
                    .field("width", 1280i32)
                    .field("height", 720i32)
                    .field("framerate", fps)
                    .build(),
            );
            let rate = crate::pipeline::element("videorate").unwrap();
            rate.set_property("drop-only", true);
            let output = crate::pipeline::element("capsfilter").unwrap();
            let format = VideoFormat {
                mode: VideoMode::Hd60,
                profile: wireless_display_core::protocol::wfd::video::H264Profile::High,
                level: 2,
                r2: false,
            };
            output.set_property("caps", raw_caps(format));
            let sink = crate::pipeline::element("fakesink").unwrap();
            sink.set_property("sync", false);
            let pipeline = gst::Pipeline::new();
            let elements = [&src, &input, &rate, &output, &sink];
            pipeline.add_many(elements).unwrap();
            let linked = gst::Element::link_many(elements);
            if linked.is_err() {
                assert_eq!(fps, gst::Fraction::new(30, 1));
                continue;
            }
            let guard = GstMediaPipeline {
                pipeline,
                sink,
                streaming: false,
                encoder: None,
                control: None,
                _source: wireless_display_core::CaptureSource::Test,
            };
            let started = guard.start();
            let bus = guard.pipeline.bus().unwrap();
            let message = bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(3),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            );
            if fps == gst::Fraction::new(30, 1) {
                assert!(
                    started.is_err()
                        || message
                            .is_some_and(|message| message.type_() == gst::MessageType::Error)
                );
            } else {
                started.unwrap();
                assert!(message.is_some_and(|message| message.type_() == gst::MessageType::Eos));
                assert_eq!(guard.frames(), 6);
                let caps = guard
                    .sink
                    .static_pad("sink")
                    .unwrap()
                    .current_caps()
                    .unwrap();
                assert_eq!(
                    caps.structure(0)
                        .unwrap()
                        .get::<gst::Fraction>("framerate")
                        .unwrap(),
                    fps
                );
            }
        }
    }

    #[test]
    fn only_well_formed_reports_for_our_stream_are_used() {
        let mut rr = vec![0x81, 201, 0, 7, 0, 0, 0, 99];
        rr.extend([
            0, 0, 0, 42, 32, 0, 0, 3, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        assert_eq!(receiver_report(&rr, 42), Some((256, 32)));
        assert_eq!(receiver_report(&rr, 43), None);
        for size in 0..rr.len() {
            assert_eq!(receiver_report(&rr[..size], 42), None);
        }
        rr[0] = 0x82;
        assert_eq!(receiver_report(&rr, 42), None);
    }
}
