use crate::{EncoderInputFormat, MediaError, pipeline};
use gst::prelude::*;
use wireless_display_core::protocol::wfd::video::{VideoFormat, VideoMode};

/// Processing backend, selected by plugins/caps and tested before use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoProcessing {
    Cpu,
    Va,
    Vaapi,
    Cuda,
}

/// The capture memory contract must also pass the authorized-source probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMemory {
    System,
    DmaBuf,
}

impl VideoProcessing {
    pub fn is_gpu(self) -> bool {
        self != Self::Cpu
    }

    pub(crate) fn candidates(name: &str, mut available: impl FnMut(&str) -> bool) -> Vec<Self> {
        let gpu = match name {
            "vah264enc" | "qsvh264enc" if available("vapostproc") => Some(Self::Va),
            "vaapih264enc" if available("vaapipostproc") => Some(Self::Vaapi),
            "nvh264enc" | "nvautogpuh264enc"
                if available("cudaupload") && available("cudaconvertscale") =>
            {
                Some(Self::Cuda)
            }
            _ => None,
        };
        gpu.into_iter().chain([Self::Cpu]).collect()
    }

    pub(crate) fn encoder_caps(self, video: VideoFormat, format: EncoderInputFormat) -> gst::Caps {
        let mut caps = crate::quality::raw_caps(video, format);
        let feature = match self {
            Self::Cpu => None,
            Self::Va => Some("memory:VAMemory"),
            Self::Vaapi => Some("memory:VASurface"),
            Self::Cuda => Some("memory:CUDAMemory"),
        };
        if let Some(feature) = feature {
            caps.make_mut()
                .set_features_simple(Some(gst::CapsFeatures::new([feature])));
        }
        caps
    }

    pub(crate) fn build(self, mode: VideoMode) -> Result<Vec<gst::Element>, MediaError> {
        let (names, aspect) = match self {
            Self::Cpu => return cpu_processing(mode),
            Self::Va => (&["vapostproc"][..], "add-borders"),
            Self::Vaapi => (&["vaapipostproc"][..], "force-aspect-ratio"),
            Self::Cuda => (&["cudaupload", "cudaconvertscale"][..], "add-borders"),
        };
        let elements = names
            .iter()
            .map(|name| pipeline::element(name))
            .collect::<Result<Vec<_>, _>>()?;
        // Never stretch the desktop to fit a receiver with a different aspect ratio.
        if let Some(last) = elements.last() {
            crate::encoder::set_bool(last, aspect, true)?;
        }
        Ok(elements)
    }
}

pub(crate) fn capture_pacing(
    mode: VideoMode,
    memory: CaptureMemory,
) -> Result<[gst::Element; 3], MediaError> {
    let rate = pipeline::element("videorate")?;
    if mode == VideoMode::Hd60 {
        rate.set_property("drop-only", true);
    }
    let filter = pipeline::element("capsfilter")?;
    let mut caps = gst::Caps::builder("video/x-raw")
        .field("framerate", crate::quality::frame_rates(mode))
        .build();
    if memory == CaptureMemory::DmaBuf {
        caps.make_mut()
            .set_features_simple(Some(gst::CapsFeatures::new(["memory:DMABuf"])));
    }
    filter.set_property("caps", caps);
    let queue = pipeline::element("queue")?;
    queue.set_property("max-size-buffers", 2u32);
    queue.set_property("max-size-bytes", 0u32);
    queue.set_property("max-size-time", 0u64);
    queue.set_property_from_str("leaky", "downstream");
    Ok([rate, filter, queue])
}

/// Reduce the RGB desktop before color conversion in the high-cadence mode.
/// Other modes keep their existing conversion/scaling and live renegotiation.
fn cpu_processing(mode: VideoMode) -> Result<Vec<gst::Element>, MediaError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use wireless_display_core::protocol::wfd::video::H264Profile;

    fn video() -> VideoFormat {
        VideoFormat {
            mode: VideoMode::FullHd,
            profile: H264Profile::High,
            level: 4,
            r2: false,
        }
    }

    #[test]
    fn gpu_candidates_depend_on_plugins_and_always_retain_cpu() {
        for (name, path) in [
            ("vah264enc", VideoProcessing::Va),
            ("qsvh264enc", VideoProcessing::Va),
            ("vaapih264enc", VideoProcessing::Vaapi),
            ("nvh264enc", VideoProcessing::Cuda),
            ("nvautogpuh264enc", VideoProcessing::Cuda),
        ] {
            assert_eq!(
                VideoProcessing::candidates(name, |_| true),
                [path, VideoProcessing::Cpu]
            );
            assert_eq!(
                VideoProcessing::candidates(name, |_| false),
                [VideoProcessing::Cpu]
            );
        }
        assert_eq!(
            VideoProcessing::candidates("nvh264enc", |name| name == "cudaupload"),
            [VideoProcessing::Cpu]
        );
        for name in ["x264enc", "openh264enc", "unknown-hardware"] {
            assert_eq!(
                VideoProcessing::candidates(name, |_| true),
                [VideoProcessing::Cpu]
            );
        }
    }

    #[test]
    fn gpu_memory_contracts_cannot_silently_become_system_memory() {
        gst::init().unwrap();
        let system = VideoProcessing::Cpu.encoder_caps(video(), EncoderInputFormat::Nv12);
        for path in [
            VideoProcessing::Va,
            VideoProcessing::Vaapi,
            VideoProcessing::Cuda,
        ] {
            let gpu = path.encoder_caps(video(), EncoderInputFormat::Nv12);
            assert!(!gpu.can_intersect(&system));
            assert!(
                EncoderInputFormat::select("hardware", &gpu, video(), VideoProcessing::Cpu)
                    .is_err()
            );
            assert_eq!(
                EncoderInputFormat::select("hardware", &gpu, video(), path).unwrap(),
                EncoderInputFormat::Nv12
            );
        }
    }

    #[test]
    fn dma_buf_pacing_preserves_drm_format_modifiers_and_bounded_queues() {
        gst::init().unwrap();
        let [rate, filter, queue] = capture_pacing(VideoMode::Hd60, CaptureMemory::DmaBuf).unwrap();
        assert!(rate.property::<bool>("drop-only"));
        let caps = filter.property::<gst::Caps>("caps");
        let source = gst::Caps::builder("video/x-raw")
            .features(["memory:DMABuf"])
            .field("format", "DMA_DRM")
            .field("drm-format", "NV12:0x0100000000000002")
            .field("framerate", gst::Fraction::new(60, 1))
            .build();
        assert!(caps.can_intersect(&source));
        let [_, system, _] = capture_pacing(VideoMode::Hd60, CaptureMemory::System).unwrap();
        assert!(!system.property::<gst::Caps>("caps").can_intersect(&source));
        assert_eq!(queue.property::<u32>("max-size-buffers"), 2);
        assert_eq!(queue.property::<u32>("max-size-bytes"), 0);
        assert_eq!(queue.property::<u64>("max-size-time"), 0);
    }
}
