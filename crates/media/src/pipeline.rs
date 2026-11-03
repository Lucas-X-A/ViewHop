use futures_util::StreamExt;
use gst::prelude::*;
use std::os::fd::AsRawFd;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wireless_display_core::traits::MediaPipeline;
use wireless_display_core::{CaptureSource, MediaEvent};

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("GStreamer could not start: {0}")]
    Init(#[from] gst::glib::Error),
    #[error("Missing or invalid GStreamer plugin: {0}")]
    Element(String),
    #[error("Failed to build pipeline: {0}")]
    Build(#[from] gst::glib::BoolError),
    #[error("Pipeline rejected state change: {0}")]
    State(#[from] gst::StateChangeError),
    #[error("GStreamer: {0}")]
    Pipeline(String),
    #[error("Capture did not deliver its first frame within 15 seconds")]
    NoFrames,
    #[error("Capture did not report its pixel size within five seconds")]
    NoGeometry,
    #[error("Could not prepare a frame for a resolution change within two seconds")]
    NoFormatFrame,
    #[error("Pipeline has no GstBus")]
    NoBus,
    #[error("No usable H.264 encoder found")]
    NoEncoder,
    #[error("Media preparation cancelled")]
    Cancelled,
    #[error("RTP socket: {0}")]
    Socket(#[from] std::io::Error),
}

pub struct GstMediaPipeline {
    pub(crate) pipeline: gst::Pipeline,
    pub(crate) sink: gst::Element,
    pub(crate) streaming: bool,
    pub(crate) encoder: Option<gst::Element>,
    pub(crate) control: Option<crate::quality::StreamingControl>,
    // Keep the restricted remote FD alive until after the pipeline reaches NULL.
    pub(crate) _source: CaptureSource,
}

pub(crate) fn element(name: &str) -> Result<gst::Element, MediaError> {
    gst::ElementFactory::make(name)
        .build()
        .map_err(|error| MediaError::Element(format!("{name}: {error}")))
}

pub(crate) fn capture_element(source: &CaptureSource) -> Result<gst::Element, MediaError> {
    let src = match source {
        CaptureSource::PipeWire {
            remote,
            node_id,
            serial,
        } => {
            let src = element("pipewiresrc")?;
            src.set_property("fd", remote.as_raw_fd());
            src.set_property("do-timestamp", true);
            if let Some(serial) = serial {
                if src.find_property("target-object").is_some() {
                    src.set_property("target-object", serial.to_string());
                } else {
                    return Err(MediaError::Element(
                        "pipewiresrc must support target-object for the portal serial".into(),
                    ));
                }
            } else {
                src.set_property("path", node_id.to_string());
            }
            src
        }
        CaptureSource::X11 { display } => {
            let src = element("ximagesrc")?;
            src.set_property("display-name", display);
            src.set_property("use-damage", false);
            src.set_property("do-timestamp", true);
            src
        }
        CaptureSource::Test => {
            let src = element("videotestsrc")?;
            src.set_property("is-live", true);
            src
        }
    };
    Ok(src)
}

impl GstMediaPipeline {
    /// Compatibility alias for [`Self::try_new`].
    pub fn new(source: CaptureSource) -> Result<Self, MediaError> {
        Self::try_new(source)
    }

    /// Builds a local capture test pipeline without starting capture.
    ///
    /// Fails if GStreamer or a required element is unavailable. The pipeline
    /// retains ownership of the source's restricted PipeWire FD, when present.
    pub fn try_new(source: CaptureSource) -> Result<Self, MediaError> {
        gst::init()?;
        let src = capture_element(&source)?;
        let queue = element("queue")?;
        queue.set_property("max-size-buffers", 2u32);
        queue.set_property("max-size-bytes", 0u32);
        queue.set_property("max-size-time", 0u64);
        let convert = element("videoconvert")?;
        let scale = element("videoscale")?;
        let caps = element("capsfilter")?;
        // Only the generated test source has a fixed size. Real sources retain
        // negotiated geometry and may renegotiate after monitor changes.
        if matches!(source, CaptureSource::Test) {
            caps.set_property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", 1280i32)
                    .field("height", 720i32)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .build(),
            );
        }
        let sink = element("fakesink")?;
        sink.set_property("sync", false);
        let pipeline = gst::Pipeline::new();
        let elements = [&src, &queue, &convert, &scale, &caps, &sink];
        pipeline.add_many(elements)?;
        gst::Element::link_many(elements)?;
        Ok(Self {
            pipeline,
            sink,
            streaming: false,
            encoder: None,
            control: None,
            _source: source,
        })
    }

    #[must_use]
    pub fn frames(&self) -> u64 {
        self.sink
            .property::<gst::Structure>("stats")
            .get::<u64>("rendered")
            .unwrap_or(0)
    }

    pub async fn run(
        &self,
        events: async_channel::Sender<MediaEvent>,
        cancel: CancellationToken,
    ) -> Result<(), MediaError> {
        let bus = self.pipeline.bus().ok_or(MediaError::NoBus)?;
        let mut messages = bus.stream();
        self.start()?;
        if let Some(control) = self.control.as_ref() {
            let _ = control.telemetry.sample();
        }
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut started = tokio::time::Instant::now();
        let mut quality_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(2),
            Duration::from_secs(2),
        );
        quality_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let result = loop {
            tokio::select! {
                _ = cancel.cancelled() => break Ok(()),
                _ = tick.tick() => {
                    let frames = self.frames();
                    let event = if self.streaming { MediaEvent::Packets(frames) } else { MediaEvent::Frames(frames) };
                    if events.send(event).await.is_err() { break Ok(()); }
                    // Portal streams can be sparse on a static desktop. Only time out
                    // startup; after the first frame use EOS/Error/Portal Closed.
                    if frames == 0 && self.pipeline.current_state() == gst::State::Playing && started.elapsed() >= Duration::from_secs(15) { break Err(MediaError::NoFrames); }
                },
                _ = quality_tick.tick(), if self.control.is_some() => {
                    if let Some(control) = self.control.as_ref() {
                        let (quality,statistics) = control.telemetry.sample();
                        let _ = events.send(MediaEvent::VideoStatistics(statistics)).await;
                        let _ = events.send(MediaEvent::Quality(quality)).await;
                    }
                },
                message = messages.next() => {
                    let Some(message) = message else { break Err(MediaError::NoBus); };
                    match message.view() {
                        gst::MessageView::Error(error) => {
                            // Debug details can contain paths; opt-in log only.
                            tracing::debug!(target: "wireless_display::media", detail = ?error.debug(), "GstBus error");
                            break Err(MediaError::Pipeline(error.error().to_string()));
                        },
                        gst::MessageView::Eos(_) => { let _ = events.send(MediaEvent::Eos).await; break Ok(()); },
                        gst::MessageView::Warning(warning) => { let _ = events.send(MediaEvent::Warning(warning.error().to_string())).await; },
                        gst::MessageView::StateChanged(state) if message.src().is_some_and(|src| src == self.pipeline.upcast_ref::<gst::Object>()) => {
                            if state.current() == gst::State::Playing { started = tokio::time::Instant::now(); }
                            let _ = events.send(MediaEvent::StateChanged { playing: state.current() == gst::State::Playing }).await;
                        },
                        _ => {}
                    }
                }
            }
        };
        let stopped = self.stop();
        result.and(stopped)
    }

    pub fn request_keyframe(&self) {
        if self.encoder.is_some() {
            // Standard upstream GstForceKeyUnit event; only fixed trusted fields.
            let event = gst::event::CustomUpstream::new(
                gst::Structure::builder("GstForceKeyUnit")
                    .field("running-time", u64::MAX)
                    .field("all-headers", true)
                    .field("count", 0u32)
                    .build(),
            );
            self.pipeline.send_event(event);
        }
    }
}

impl MediaPipeline for GstMediaPipeline {
    type Error = MediaError;
    fn start(&self) -> Result<(), MediaError> {
        self.pipeline.set_state(gst::State::Playing)?;
        Ok(())
    }
    fn pause(&self) -> Result<(), MediaError> {
        self.pipeline.set_state(gst::State::Paused)?;
        Ok(())
    }
    fn stop(&self) -> Result<(), MediaError> {
        if let Some(control) = self.control.as_ref() {
            control.release_frame();
        }
        self.pipeline.set_state(gst::State::Null)?;
        Ok(())
    }
}

impl Drop for GstMediaPipeline {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::warn!(target: "wireless_display::media", %error, "Failed to stop pipeline");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn generated_video_reaches_sink_and_stops() {
        let pipeline = GstMediaPipeline::try_new(CaptureSource::Test).unwrap();
        let cancel = CancellationToken::new();
        let (tx, rx) = async_channel::bounded(32);
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move { pipeline.run(tx, cancel).await }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(event) = rx.recv().await {
                if matches!(event, MediaEvent::Frames(n) if n > 0) {
                    return;
                }
            }
            panic!("pipeline ended without frames");
        })
        .await
        .unwrap();
        cancel.cancel();
        task.await.unwrap().unwrap();
    }
}
