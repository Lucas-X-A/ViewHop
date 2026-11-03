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
