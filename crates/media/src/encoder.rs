use crate::{MediaError, VideoProcessing, pipeline};
use gst::prelude::*;
use wireless_display_core::protocol::wfd::video::{H264Profile, VideoFormat, VideoMode};

/// Supported 8-bit 4:2:0 inputs. Allocator padding can differ between formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderInputFormat {
    I420,
    Nv12,
}

impl EncoderInputFormat {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::I420 => "I420",
            Self::Nv12 => "NV12",
        }
    }

    pub(crate) fn select(
        name: &str,
        supported: &gst::CapsRef,
        video: VideoFormat,
        processing: VideoProcessing,
    ) -> Result<Self, MediaError> {
        let formats = if matches!(name, "x264enc" | "openh264enc") {
            [Self::I420, Self::Nv12]
        } else {
            [Self::Nv12, Self::I420]
        };
        formats
            .into_iter()
            .find(|format| supported.can_intersect(&processing.encoder_caps(video, *format)))
            .ok_or_else(|| {
                MediaError::Element(format!(
                    "{name}: no compatible I420/NV12 input for {processing:?}"
                ))
            })
    }
}

fn setting_error(encoder: &gst::Element, name: &str) -> MediaError {
    let factory = encoder
        .factory()
        .map(|f| f.name().to_string())
        .unwrap_or_default();
    MediaError::Element(format!("{factory}: unsupported encoder setting {name}"))
}

fn writable_property(
    encoder: &gst::Element,
    name: &str,
) -> Result<gst::glib::ParamSpec, MediaError> {
    encoder
        .find_property(name)
        .filter(|property| {
            property.flags().contains(gst::glib::ParamFlags::WRITABLE)
                && !property
                    .flags()
                    .contains(gst::glib::ParamFlags::CONSTRUCT_ONLY)
        })
        .ok_or_else(|| setting_error(encoder, name))
}

pub(crate) fn set_bool(encoder: &gst::Element, name: &str, value: bool) -> Result<(), MediaError> {
    let property = writable_property(encoder, name)?;
    if property.value_type() != bool::static_type() {
        return Err(setting_error(encoder, name));
    }
    encoder.set_property(name, value);
    Ok(())
}

fn set_uint(encoder: &gst::Element, name: &str, value: u32) -> Result<(), MediaError> {
    let property = writable_property(encoder, name)?;
    if let Some(spec) = property.downcast_ref::<gst::glib::ParamSpecUInt>() {
        if value < spec.minimum() || value > spec.maximum() {
            return Err(setting_error(encoder, name));
        }
        encoder.set_property(name, value);
    } else if let Some(spec) = property.downcast_ref::<gst::glib::ParamSpecInt>() {
        let value = i32::try_from(value).map_err(|_| setting_error(encoder, name))?;
        if value < spec.minimum() || value > spec.maximum() {
            return Err(setting_error(encoder, name));
        }
        encoder.set_property(name, value);
    } else {
        return Err(setting_error(encoder, name));
    }
    Ok(())
}

fn set_enum(encoder: &gst::Element, name: &str, nick: &str) -> Result<(), MediaError> {
    let property = writable_property(encoder, name)?;
    let value = property
        .downcast_ref::<gst::glib::ParamSpecEnum>()
        .and_then(|spec| spec.enum_class().to_value_by_nick(nick))
        .ok_or_else(|| setting_error(encoder, name))?;
    encoder.set_property_from_value(name, &value);
    Ok(())
}

pub(crate) fn configured_encoder(
    name: &str,
    video: VideoFormat,
) -> Result<gst::Element, MediaError> {
    let encoder = pipeline::element(name)?;
    match name {
        "x264enc" => {
            set_enum(&encoder, "pass", "cbr")?;
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
        "vah264enc" => {
            set_enum(&encoder, "rate-control", "cbr")?;
            set_uint(&encoder, "b-frames", 0)?;
            set_uint(&encoder, "key-int-max", video.mode.fps())?;
            set_bool(&encoder, "cabac", false)?;
            set_bool(&encoder, "aud", true)?;
        }
        "vaapih264enc" => {
            set_enum(&encoder, "rate-control", "cbr")?;
            set_uint(&encoder, "max-bframes", 0)?;
            set_uint(&encoder, "keyframe-period", video.mode.fps())?;
            set_bool(&encoder, "cabac", false)?;
        }
        "nvh264enc" | "nvautogpuh264enc" => {
            set_enum(&encoder, "rc-mode", "cbr")?;
            set_bool(&encoder, "zerolatency", true)?;
            set_uint(&encoder, "rc-lookahead", 0)?;
            set_uint(&encoder, "bframes", 0)?;
            set_uint(&encoder, "gop-size", video.mode.fps())?;
            set_bool(&encoder, "cabac", false)?;
            set_bool(&encoder, "aud", true)?;
        }
        "qsvh264enc" => {
            set_enum(&encoder, "rate-control", "cbr")?;
            set_bool(&encoder, "low-latency", true)?;
            set_uint(&encoder, "b-frames", 0)?;
            set_uint(&encoder, "gop-size", video.mode.fps())?;
            // QSV exposes a coding-option enum, unlike the boolean VA/NV controls.
            set_enum(&encoder, "cabac", "off")?;
        }
        _ => {
            return Err(MediaError::Element(format!(
                "{name}: live bitrate control unavailable"
            )));
        }
    }
    set_bitrate(&encoder, video.mode.bitrate_range().0)?;
    Ok(encoder)
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

pub struct EncoderSelector;

impl EncoderSelector {
    pub const H264: [&str; 9] = [
        "vah264enc",
        "vaapih264enc",
        "nvh264enc",
        "nvautogpuh264enc",
        "v4l2slh264enc",
        "v4l2h264enc",
        "qsvh264enc",
        "x264enc",
        "openh264enc",
    ];

    #[must_use]
    pub fn candidates_with(mut available: impl FnMut(&str) -> bool) -> Vec<String> {
        Self::H264
            .into_iter()
            .filter(|name| available(name))
            .map(str::to_owned)
            .collect()
    }

    #[must_use]
    pub fn available() -> Vec<String> {
        Self::candidates_with(|name| {
            gst::ElementFactory::find(name)
                .is_some_and(|factory| factory.has_type(gst::ElementFactoryType::VIDEO_ENCODER))
        })
    }
}

#[must_use]
pub fn plugins() -> Vec<(String, bool)> {
    [
        "pipewiresrc",
        "ximagesrc",
        "videoconvert",
        "videoscale",
        "videorate",
        // Optional GPU processing; absence never disables software capture/encoding.
        "vapostproc",
        "vaapipostproc",
        "cudaupload",
        "cudaconvertscale",
        "capsfilter",
        "queue",
        "fakesink",
        "videotestsrc",
        "rtph264pay",
        "h264parse",
        "mpegtsmux",
        "rtpmp2tpay",
        "rtpbin",
        "udpsrc",
        "udpsink",
    ]
    .into_iter()
    .map(|name| (name.into(), gst::ElementFactory::find(name).is_some()))
    .collect()
}

#[must_use]
pub fn audio_encoders() -> Vec<String> {
    ["fdkaacenc", "avenc_aac", "voaacenc"]
        .into_iter()
        .filter(|name| gst::ElementFactory::find(name).is_some())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video() -> VideoFormat {
        VideoFormat {
            mode: VideoMode::FullHd,
            profile: H264Profile::High,
            level: 4,
            r2: false,
        }
    }

    #[test]
    fn input_selection_accepts_nv12_only_hardware_and_preserves_software_preferences() {
        gst::init().unwrap();
        let nv12 = crate::quality::raw_caps(video(), EncoderInputFormat::Nv12);
        assert!(!crate::quality::raw_caps(video(), EncoderInputFormat::I420).can_intersect(&nv12));
        assert_eq!(
            EncoderInputFormat::select("nvh264enc", &nv12, video(), VideoProcessing::Cpu).unwrap(),
            EncoderInputFormat::Nv12
        );
        let both = gst::Caps::builder("video/x-raw")
            .field("format", gst::List::new(["I420", "NV12"]))
            .build();
        assert_eq!(
            EncoderInputFormat::select("x264enc", &both, video(), VideoProcessing::Cpu).unwrap(),
            EncoderInputFormat::I420
        );
        assert_eq!(
            EncoderInputFormat::select("vah264enc", &both, video(), VideoProcessing::Cpu).unwrap(),
            EncoderInputFormat::Nv12
        );
        let gpu_only = gst::Caps::builder("video/x-raw")
            .features(["memory:CUDAMemory"])
            .field("format", "NV12")
            .build();
        assert!(
            EncoderInputFormat::select("nvh264enc", &gpu_only, video(), VideoProcessing::Cpu)
                .is_err()
        );
        assert!(
            EncoderInputFormat::select(
                "nvh264enc",
                &gst::Caps::new_empty(),
                video(),
                VideoProcessing::Cpu
            )
            .is_err()
        );
    }

    #[test]
    fn unsupported_properties_fail_without_panicking_and_software_remains_live_mutable() {
        gst::init().unwrap();
        let encoder = configured_encoder("x264enc", video()).unwrap();
        assert_eq!(encoder.property::<u32>("bframes"), 0);
        assert!(!encoder.property::<bool>("cabac"));
        assert!(set_bool(&encoder, "bitrate", false).is_err());
        assert!(set_enum(&encoder, "pass", "not-an-encoding-mode").is_err());
        assert!(set_uint(&encoder, "bframes", u32::MAX).is_err());
        assert!(set_bool(&encoder, "not-a-property", true).is_err());
        for bitrate in [8000, 16000, 8000] {
            set_bitrate(&encoder, bitrate).unwrap();
            assert_eq!(encoder.property::<u32>("bitrate"), bitrate);
        }
    }

    #[test]
    fn selects_existing_hardware_before_software() {
        assert_eq!(
            EncoderSelector::candidates_with(
                |name| ["x264enc", "vah264enc", "openh264enc"].contains(&name)
            ),
            vec!["vah264enc", "x264enc", "openh264enc"]
        );
        assert!(EncoderSelector::candidates_with(|_| false).is_empty());
    }
}
