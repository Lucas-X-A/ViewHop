use super::capabilities::WfdParameters;
use super::rtsp::ProtocolError;
use std::net::Ipv4Addr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum H264Profile {
    Baseline,
    High,
}

impl H264Profile {
    #[must_use]
    pub fn gst_name(self) -> &'static str {
        match self {
            Self::Baseline => "constrained-baseline",
            Self::High => "high",
        }
    }
    fn bit(self) -> u8 {
        match self {
            Self::Baseline => 1,
            Self::High => 2,
        }
    }
}

/// Progressive modes exposed by the application. Higher modes require WFD R2.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum VideoMode {
    Vga,
    #[default]
    Hd,
    Hd60,
    FullHd,
    Wuxga,
    Qhd,
    Wqxga,
    Uhd,
    Dci4k,
}

impl VideoMode {
    pub const USER_MODES: [Self; 8] = [
        Self::Hd,
        Self::Hd60,
        Self::FullHd,
        Self::Wuxga,
        Self::Qhd,
        Self::Wqxga,
        Self::Uhd,
        Self::Dci4k,
    ];

    #[must_use]
    pub fn size(self) -> (u32, u32) {
        match self {
            Self::Vga => (640, 480),
            Self::Hd | Self::Hd60 => (1280, 720),
            Self::FullHd => (1920, 1080),
            Self::Wuxga => (1920, 1200),
            Self::Qhd => (2560, 1440),
            Self::Wqxga => (2560, 1600),
            Self::Uhd => (3840, 2160),
            Self::Dci4k => (4096, 2160),
        }
    }
    #[must_use]
    pub fn fps(self) -> u32 {
        if matches!(self, Self::Vga | Self::Hd60) {
            60
        } else {
            30
        }
    }
    #[must_use]
    pub fn fits(self, size: (u32, u32)) -> bool {
        let (width, height) = self.size();
        width <= size.0 && height <= size.1
    }
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Vga => "480p",
            Self::Hd => "720p — prioritize performance",
            Self::Hd60 => "720p at 60 fps — smoother motion",
            Self::FullHd => "1080p — Full HD",
            Self::Wuxga => "1920 × 1200",
            Self::Qhd => "1440p — QHD",
            Self::Wqxga => "2560 × 1600",
            Self::Uhd => "2160p — 4K UHD",
            Self::Dci4k => "4096 × 2160",
        }
    }
    #[must_use]
    pub fn minimum_level(self) -> u8 {
        match self {
            Self::Vga | Self::Hd => 1,
            Self::Hd60 => 2,
            Self::FullHd => 4,
            Self::Wuxga => 32,
            Self::Qhd | Self::Wqxga => 32,
            Self::Uhd => 64,
            Self::Dci4k => 128,
        }
    }
    #[must_use]
    pub fn requires_r2(self) -> bool {
        self > Self::FullHd
    }
    fn bitmap(self) -> (bool, u8) {
        match self {
            Self::Vga => (false, 0),
            Self::Hd => (false, 5),
            Self::Hd60 => (false, 6),
            Self::FullHd => (false, 7),
            Self::Wuxga => (true, 28),
            Self::Qhd => (true, 30),
            Self::Wqxga => (true, 32),
            Self::Uhd => (false, 19),
            Self::Dci4k => (false, 24),
        }
    }
    #[must_use]
    pub fn bitrate_range(self) -> (u32, u32) {
        match self {
            Self::Vga => (2000, 4000),
            Self::Hd => (4000, 6000),
            Self::Hd60 => (8000, 12000),
            Self::FullHd => (8000, 16000),
            Self::Wuxga => (10000, 16000),
            Self::Qhd => (12000, 20000),
            Self::Wqxga => (14000, 24000),
            Self::Uhd | Self::Dci4k => (20000, 40000),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoFormat {
    pub mode: VideoMode,
    pub profile: H264Profile,
    /// One WFD level bit, not the decimal H.264 level number.
    pub level: u8,
    pub r2: bool,
}

impl VideoFormat {
    #[must_use]
    pub fn gst_level(self) -> &'static str {
        match self.level {
            1 => "3.1",
            2 => "3.2",
            4 => "4",
            8 => "4.1",
            16 => "4.2",
            32 => "5",
            64 => "5.1",
            _ => "5.2",
        }
    }
    #[must_use]
    pub fn parameter_name(self) -> &'static str {
        if self.r2 {
            "wfd2_video_formats"
        } else {
            "wfd_video_formats"
        }
    }
    #[must_use]
    pub fn parameter(self) -> String {
        let (vesa, index) = self.mode.bitmap();
        let native = if self.r2 {
            0
        } else {
            (index << 3) | u8::from(vesa)
        };
        let bitmap = 1u64 << index;
        let (cea, vesa) = if vesa { (0, bitmap) } else { (bitmap, 0) };
        if self.r2 {
            format!(
                "{native:02x} 01 {:02x} {:04x} {cea:012x} {vesa:012x} 000000000000 00 0000 0000 00 00",
                self.profile.bit(),
                self.level
            )
        } else {
            format!(
                "{native:02x} 00 {:02x} {:02x} {cea:08x} {vesa:08x} 00000000 00 0000 0000 00 none none",
                self.profile.bit(),
                self.level
            )
        }
    }
}

/// Each offer was tested with one encoder at the same profile and level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoOffer {
    pub preferred: VideoFormat,
    pub fallback: Option<VideoFormat>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtpPorts {
    pub rtp: u16,
    pub rtcp: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegotiatedStream {
    pub video: VideoFormat,
    pub fallback: Option<VideoFormat>,
    pub peer: Ipv4Addr,
    pub client_ports: RtpPorts,
}

#[derive(Clone, Debug)]
pub struct StreamingInfo {
    pub video: VideoFormat,
    pub encoder: String,
    pub bitrate_kbps: u32,
    pub peer: Ipv4Addr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct P2pNetwork {
    pub local: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Option<Ipv4Addr>,
}

impl P2pNetwork {
    #[must_use]
    pub fn permits(self, peer: Ipv4Addr) -> bool {
        if self.prefix == 0
            || self.prefix > 32
            || peer == self.local
            || peer.is_unspecified()
            || peer.is_multicast()
            || peer.is_broadcast()
        {
            return false;
        }
        if let Some(gateway) = self.gateway {
            return peer == gateway;
        }
        let mask = u32::MAX << (32 - self.prefix);
        let host = u32::from(peer) & !mask;
        (u32::from(peer) & mask) == (u32::from(self.local) & mask)
            && (self.prefix >= 31 || (host != 0 && host != !mask))
    }
}

fn hex(value: &str, width: usize) -> Result<u64, ProtocolError> {
    if value.len() != width || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ProtocolError::Malformed("WFD hexadecimal field"));
    }
    u64::from_str_radix(value, 16).map_err(|_| ProtocolError::Malformed("WFD hexadecimal field"))
}

struct Codec {
    profile: u64,
    level: u64,
    cea: u64,
    vesa: u64,
    max_width: Option<u64>,
    max_height: Option<u64>,
    h264: bool,
}

fn codecs(value: &str, r2: bool) -> Result<Vec<Codec>, ProtocolError> {
    if value.len() > 8192 {
        return Err(ProtocolError::TooLarge);
    }
    let mut result = Vec::new();
    let mut sets = value.split(',').enumerate().peekable();
    while let Some((n, set)) = sets.next() {
        if n >= 16 {
            return Err(ProtocolError::TooLarge);
        }
        let mut fields: Vec<_> = set.split_ascii_whitespace().collect();
        if n == 0 {
            hex(
                fields
                    .first()
                    .ok_or(ProtocolError::Malformed("video native mode"))?,
                2,
            )?;
            fields.remove(0);
            if !r2 {
                if hex(
                    fields
                        .first()
                        .ok_or(ProtocolError::Malformed("preferred mode flag"))?,
                    2,
                )? > 1
                {
                    return Err(ProtocolError::Malformed("preferred display mode flag"));
                }
                fields.remove(0);
            }
        }
        if r2
            && sets.peek().is_none()
            && hex(
                fields
                    .pop()
                    .ok_or(ProtocolError::Malformed("transcoding flag"))?,
                2,
            )? > 1
        {
            return Err(ProtocolError::Malformed("non-transcoding flag"));
        }
        let (h264, level_width, bitmap_width) = if r2 {
            if fields.len() != 10 {
                return Err(ProtocolError::Malformed("WFD R2 codec fields"));
            }
            let h264 = hex(fields.remove(0), 2)? == 1;
            (h264, 4, 12)
        } else {
            if fields.len() != 11 {
                return Err(ProtocolError::Malformed("WFD codec fields"));
            }
            (true, 2, 8)
        };
        let profile = hex(fields[0], 2)?;
        let level = hex(fields[1], level_width)?;
        let cea = hex(fields[2], bitmap_width)?;
        let vesa = hex(fields[3], bitmap_width)?;
        hex(fields[4], bitmap_width)?;
        for (i, width) in [(5, 2), (6, 4), (7, 4), (8, 2)] {
            hex(fields[i], width)?;
        }
        let dimension = |i: usize| -> Result<Option<u64>, ProtocolError> {
            if r2 || fields[i] == "none" {
                Ok(None)
            } else {
                hex(fields[i], 4).map(Some)
            }
        };
        result.push(Codec {
            profile,
            level,
            cea,
            vesa,
            max_width: dimension(9)?,
            max_height: dimension(10)?,
            h264,
        });
    }
    Ok(result)
}

fn supported(format: VideoFormat, codecs: &[Codec], r2: bool) -> bool {
    if !r2 && format.mode.requires_r2() {
        return false;
    }
    let (vesa, index) = format.mode.bitmap();
    let (width, height) = format.mode.size();
    codecs.iter().any(|c| {
        c.h264
        && c.profile & u64::from(format.profile.bit()) != 0
        // M3 advertises the maximum level; lower levels are usable too.
        && c.level & 0xff >= u64::from(format.level)
        && (if vesa { c.vesa } else { c.cea }) & (1u64 << index) != 0
        && c.max_width.is_none_or(|max| max >= u64::from(width))
        && c.max_height.is_none_or(|max| max >= u64::from(height))
    })
}

/// Select only an encoder-proven offer. A fallback preserves profile, level and fps.
pub fn negotiate_video(
    value: &str,
    offers: &[VideoOffer],
    r2: bool,
) -> Result<VideoOffer, ProtocolError> {
    let codecs = codecs(value, r2)?;
    for offer in offers {
        if !supported(offer.preferred, &codecs, r2) {
            continue;
        }
        let mut selected = *offer;
        selected.preferred.r2 = r2;
        selected.fallback = offer
            .fallback
            .filter(|fallback| {
                fallback.profile == offer.preferred.profile
                    && fallback.level == offer.preferred.level
                    && fallback.mode.fps() == offer.preferred.mode.fps()
                    && supported(*fallback, &codecs, r2)
            })
            .map(|mut fallback| {
                fallback.r2 = r2;
                fallback
            });
        return Ok(selected);
    }
    Err(ProtocolError::Unsupported(
        "no common H.264 format supported by the display, encoder, and TV",
    ))
}

fn port(value: &str) -> Result<u16, ProtocolError> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ProtocolError::Malformed("RTP port"));
    }
    let port: u16 = value
        .parse()
        .map_err(|_| ProtocolError::Malformed("RTP port"))?;
    if port < 1024 {
        return Err(ProtocolError::Unsupported("privileged RTP port"));
    }
    Ok(port)
}

pub fn client_ports(value: &str) -> Result<RtpPorts, ProtocolError> {
    let parts: Vec<_> = value.split_ascii_whitespace().collect();
    if parts.len() != 4 || parts[0] != "RTP/AVP/UDP;unicast" || parts[3] != "mode=play" {
        return Err(ProtocolError::Unsupported(
            "WFD transport; only RTP/AVP/UDP unicast",
        ));
    }
    let rtp = port(parts[1])?;
    // WFD's second port is a second RTP stream, not the RTCP port. RTCP is
    // negotiated later by the SETUP Transport client_port range.
    if parts[2] != "0" {
        return Err(ProtocolError::Unsupported("multiple RTP streams"));
    }
    Ok(RtpPorts { rtp, rtcp: None })
}

pub fn setup_ports(value: &str) -> Result<RtpPorts, ProtocolError> {
    if value.len() > 1024 {
        return Err(ProtocolError::TooLarge);
    }
    let mut parts = value.split(';').map(str::trim);
    if !matches!(parts.next(), Some("RTP/AVP/UDP" | "RTP/AVP")) {
        return Err(ProtocolError::Unsupported("RTSP Transport"));
    }
    let mut unicast = false;
    let mut ports = None;
    for part in parts {
        if part == "unicast" {
            unicast = true;
        } else if let Some(value) = part.strip_prefix("client_port=") {
            if ports.is_some() {
                return Err(ProtocolError::Malformed("duplicate client_port"));
            }
            let (rtp, rtcp) = match value.split_once('-') {
                Some((a, b)) => (port(a)?, Some(port(b)?)),
                None => (port(value)?, None),
            };
            if rtcp == Some(rtp) {
                return Err(ProtocolError::Malformed("RTP/RTCP port"));
            }
            ports = Some(RtpPorts { rtp, rtcp });
        } else if part != "mode=play" && part != "mode=\"play\"" {
            return Err(ProtocolError::Unsupported("RTSP Transport option"));
        }
    }
    if !unicast {
        return Err(ProtocolError::Unsupported("multicast"));
    }
    ports.ok_or(ProtocolError::Malformed("missing client_port"))
}

pub fn negotiate(
    parameters: &WfdParameters,
    offers: &[VideoOffer],
    peer: Ipv4Addr,
) -> Result<NegotiatedStream, ProtocolError> {
    // Advertising HDCP support does not require protected transport. This source
    // negotiates unprotected desktop video and never establishes an HDCP session.
    let (value, r2) = match parameters
        .get("wfd2_video_formats")
        .filter(|value| *value != "none")
    {
        Some(value) => (value, true),
        None => (
            parameters
                .get("wfd_video_formats")
                .ok_or(ProtocolError::Malformed("missing wfd_video_formats"))?,
            false,
        ),
    };
    let offer = negotiate_video(value, offers, r2)?;
    Ok(NegotiatedStream {
        video: offer.preferred,
        fallback: offer.fallback,
        peer,
        client_ports: client_ports(
            parameters
                .get("wfd_client_rtp_ports")
                .ok_or(ProtocolError::Malformed("missing wfd_client_rtp_ports"))?,
        )?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn offer(mode: VideoMode) -> VideoOffer {
        let preferred = VideoFormat {
            mode,
            profile: H264Profile::High,
            level: mode.minimum_level(),
            r2: false,
        };
        VideoOffer {
            preferred,
            fallback: (mode > VideoMode::Hd && mode.fps() == VideoMode::Hd.fps()).then_some(
                VideoFormat {
                    mode: VideoMode::Hd,
                    ..preferred
                },
            ),
        }
    }
    #[test]
    fn preference_receiver_and_encoder_limit_the_mode() {
        let caps = "00 00 02 10 000000a0 00000000 00000000 00 0000 0000 00 none none";
        let full = negotiate_video(
            caps,
            &[offer(VideoMode::FullHd), offer(VideoMode::Hd)],
            false,
        )
        .unwrap();
        assert_eq!(full.preferred.mode, VideoMode::FullHd);
        assert_eq!(full.fallback.unwrap().level, full.preferred.level);
        let hd = negotiate_video(caps, &[offer(VideoMode::Hd)], false).unwrap();
        assert_eq!(hd.preferred.mode, VideoMode::Hd);
        let restricted = caps.replace("000000a0", "00000020");
        assert_eq!(
            negotiate_video(
                &restricted,
                &[offer(VideoMode::FullHd), offer(VideoMode::Hd)],
                false
            )
            .unwrap()
            .preferred
            .mode,
            VideoMode::Hd
        );
        assert!(
            negotiate_video(
                &caps.replace("000000a0", "garbage!"),
                &[offer(VideoMode::Hd)],
                false
            )
            .is_err()
        );
        assert!(
            negotiate_video(
                &caps.replace("02 10", "01 10"),
                &[offer(VideoMode::Hd)],
                false
            )
            .is_err()
        );
    }
    #[test]
    fn sixty_fps_requires_the_receivers_mode_and_level_and_keeps_thirty_as_initial_fallback() {
        let offers = [offer(VideoMode::Hd60), offer(VideoMode::Hd)];
        let caps = "00 00 02 02 00000060 00000000 00000000 00 0000 0000 00 none none";
        let selected = negotiate_video(caps, &offers, false).unwrap();
        assert_eq!(selected.preferred.mode, VideoMode::Hd60);
        assert_eq!(selected.preferred.mode.fps(), 60);
        assert_eq!(selected.preferred.level, 2);
        assert!(selected.preferred.parameter().contains("00000040"));
        assert!(
            selected.fallback.is_none(),
            "automatic FPS changes require separate sink authorization"
        );
        for restricted in [
            caps.replace("00000060", "00000020"),
            caps.replace("02 02", "02 01"),
        ] {
            assert_eq!(
                negotiate_video(&restricted, &offers, false)
                    .unwrap()
                    .preferred
                    .mode,
                VideoMode::Hd
            );
        }
    }

    #[test]
    fn r2_uhd_and_vesa_modes_round_trip_and_observe_level_limits() {
        for mode in VideoMode::USER_MODES {
            let mut wanted = offer(mode);
            wanted.preferred.r2 = true;
            let selected = negotiate_video(&wanted.preferred.parameter(), &[wanted], true).unwrap();
            assert_eq!(selected.preferred, wanted.preferred);
        }
        let caps = "00 01 02 0040 0000000800a0 000140000000 000000000000 00 0000 0000 00, 02 01 0008 0000000800a0 000000000000 000000000000 00 0000 0000 00 01";
        assert_eq!(
            negotiate_video(caps, &[offer(VideoMode::Uhd)], true)
                .unwrap()
                .preferred
                .mode,
            VideoMode::Uhd
        );
        assert!(
            negotiate_video(
                &caps.replace("0040", "0004"),
                &[offer(VideoMode::Uhd)],
                true
            )
            .is_err()
        );
        assert!(negotiate_video(caps, &[offer(VideoMode::Uhd)], false).is_err());
        assert!(negotiate_video("00", &[offer(VideoMode::Hd)], true).is_err());
    }
    #[test]
    fn validates_udp_and_rejects_redirection_or_privileged_ports() {
        assert_eq!(
            client_ports("RTP/AVP/UDP;unicast 19000 0 mode=play").unwrap(),
            RtpPorts {
                rtp: 19000,
                rtcp: None
            }
        );
        assert!(client_ports("RTP/AVP/TCP;unicast 19000 0 mode=play").is_err());
        assert!(client_ports("RTP/AVP/UDP;unicast 22 0 mode=play").is_err());
        assert!(client_ports("RTP/AVP/UDP;unicast 19000 19002 mode=play").is_err());
        assert!(setup_ports("RTP/AVP/UDP;unicast;destination=8.8.8.8;client_port=19000").is_err());
    }
    #[test]
    fn listener_accepts_only_group_network() {
        let network = P2pNetwork {
            local: "192.168.49.2".parse().unwrap(),
            prefix: 24,
            gateway: Some("192.168.49.1".parse().unwrap()),
        };
        assert!(network.permits("192.168.49.1".parse().unwrap()));
        assert!(!network.permits("192.168.1.1".parse().unwrap()));
        assert!(!network.permits(network.local));
    }
}
