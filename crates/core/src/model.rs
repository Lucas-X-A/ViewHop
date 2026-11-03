use std::fmt;
use std::net::IpAddr;
use std::os::fd::OwnedFd;
use std::str::FromStr;
use std::sync::Arc;

use crate::protocol;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Error)]
pub enum ModelError {
    #[error("Invalid MAC address")]
    InvalidMac,
    #[error("Invalid device ID")]
    InvalidId,
    #[error("Device name is empty or longer than 256 bytes")]
    InvalidName,
    #[error("Truncated or invalid WFD information element")]
    InvalidWfd,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId(String);

impl DeviceId {
    /// Compatibility alias for [`Self::try_new`].
    pub fn new(value: impl Into<String>) -> Result<Self, ModelError> {
        Self::try_new(value)
    }

    /// Rejects empty IDs, IDs exceeding 512 bytes, and control characters.
    pub fn try_new(value: impl Into<String>) -> Result<Self, ModelError> {
        let value = value.into();
        if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
            return Err(ModelError::InvalidId);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacAddress([u8; 6]);

impl FromStr for MacAddress {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0; 6];
        let mut parts = value.split(':');
        for byte in &mut bytes {
            let part = parts.next().ok_or(ModelError::InvalidMac)?;
            if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(ModelError::InvalidMac);
            }
            *byte = u8::from_str_radix(part, 16).map_err(|_| ModelError::InvalidMac)?;
        }
        if parts.next().is_some() {
            return Err(ModelError::InvalidMac);
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0;
        write!(
            f,
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

impl MacAddress {
    #[must_use]
    pub fn masked(&self) -> String {
        format!("{:02X}:{:02X}:**:**:**:**", self.0[0], self.0[1])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerType {
    UnknownP2P,
    WirelessDisplayReceiver,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WfdCapability {
    pub device_type: u8,
    pub session_available: bool,
    pub rtsp_port: u16,
    pub max_throughput_mbps: u16,
}

impl WfdCapability {
    /// NetworkManager WfdIEs is a sequence of WFD subelements, not 802.11 IEs.
    /// Each subelement has an ID (u8), big-endian length (u16), then payload.
    pub fn parse(bytes: &[u8]) -> Result<Option<Self>, ModelError> {
        if bytes.len() > 4096 {
            return Err(ModelError::InvalidWfd);
        }
        let mut remaining = bytes;
        let mut device_info = None;
        while !remaining.is_empty() {
            if remaining.len() < 3 {
                return Err(ModelError::InvalidWfd);
            }
            let len = u16::from_be_bytes([remaining[1], remaining[2]]) as usize;
            if len > remaining.len() - 3 {
                return Err(ModelError::InvalidWfd);
            }
            if remaining[0] == 0 {
                if len != 6 || device_info.is_some() {
                    return Err(ModelError::InvalidWfd);
                }
                let data = &remaining[3..9];
                let flags = u16::from_be_bytes([data[0], data[1]]);
                device_info = Some(Self {
                    device_type: (flags & 3) as u8,
                    session_available: flags & 0x10 != 0,
                    rtsp_port: u16::from_be_bytes([data[2], data[3]]),
                    max_throughput_mbps: u16::from_be_bytes([data[4], data[5]]),
                });
            }
            remaining = &remaining[3 + len..];
        }
        Ok(device_info)
    }

    #[must_use]
    pub fn is_receiver(&self) -> bool {
        matches!(self.device_type, 1..=3) && self.rtsp_port != 0
    }
}

#[derive(Clone, Debug)]
pub struct WirelessDisplayDevice {
    pub id: DeviceId,
    pub name: String,
    pub mac: MacAddress,
    pub peer_type: PeerType,
    pub wfd: Option<WfdCapability>,
    pub signal_percent: Option<u8>,
    pub ip: Option<IpAddr>,
    pub adapter_id: String,
}

impl WirelessDisplayDevice {
    /// Compatibility alias for [`Self::try_new`].
    pub fn new(
        id: DeviceId,
        name: &str,
        mac: MacAddress,
        wfd: Option<WfdCapability>,
        adapter_id: String,
    ) -> Result<Self, ModelError> {
        Self::try_new(id, name, mac, wfd, adapter_id)
    }

    /// Validates the name and removes control characters and bidi overrides.
    ///
    /// Rejects blank names, names exceeding 256 bytes, and names left blank by
    /// sanitization. Classifies receivers using the supplied WFD capabilities.
    pub fn try_new(
        id: DeviceId,
        name: &str,
        mac: MacAddress,
        wfd: Option<WfdCapability>,
        adapter_id: String,
    ) -> Result<Self, ModelError> {
        if name.trim().is_empty() || name.len() > 256 {
            return Err(ModelError::InvalidName);
        }
        // Device names are untrusted, plain text. Remove controls and bidi overrides.
        let name: String = name
            .chars()
            .filter(|c| {
                !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            })
            .collect();
        if name.trim().is_empty() {
            return Err(ModelError::InvalidName);
        }
        let peer_type = if wfd.as_ref().is_some_and(WfdCapability::is_receiver) {
            PeerType::WirelessDisplayReceiver
        } else {
            PeerType::UnknownP2P
        };
        Ok(Self {
            id,
            name,
            mac,
            peer_type,
            wfd,
            signal_percent: None,
            ip: None,
            adapter_id,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionType {
    X11,
    Wayland,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CaptureRequest;

#[derive(Clone, Debug)]
pub enum CaptureSource {
    PipeWire {
        remote: Arc<OwnedFd>,
        node_id: u32,
        serial: Option<u64>,
    },
    X11 {
        display: String,
    },
    Test,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureInfo {
    pub backend: CaptureKind,
    pub logical_size: Option<(i32, i32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureKind {
    Portal,
    X11,
    Dummy,
}

#[derive(Clone, Debug, Default)]
pub struct SystemCapabilities {
    pub session_type: SessionType,
    pub desktop: String,
    pub portal_available: bool,
    pub screencast_version: Option<u32>,
    pub source_types: Vec<String>,
    /// Service socket visible; consuming a restricted portal stream is verified separately.
    pub pipewire_available: bool,
    pub gstreamer_version: Option<String>,
    pub plugins: Vec<(String, bool)>,
    pub h264_encoders: Vec<String>,
    pub audio_encoders: Vec<String>,
    pub network_manager_version: Option<String>,
    pub wifi_interfaces: Vec<String>,
    pub p2p_interfaces: Vec<String>,
    pub audio: AudioCapabilities,
    pub limitations: Vec<String>,
}

impl fmt::Display for SystemCapabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Session: {:?}\nXDG_CURRENT_DESKTOP: {}",
            self.session_type, self.desktop
        )?;
        writeln!(
            f,
            "ScreenCast Portal: {}\nVersion: {:?}\nSources: {}",
            self.portal_available,
            self.screencast_version,
            self.source_types.join(", ")
        )?;
        writeln!(
            f,
            "PipeWire socket visible: {} (the portal remote is checked during capture)",
            self.pipewire_available
        )?;
        writeln!(
            f,
            "GStreamer: {}",
            self.gstreamer_version.as_deref().unwrap_or("unavailable")
        )?;
        for (plugin, available) in &self.plugins {
            writeln!(f, "  {plugin}: {available}")?;
        }
        writeln!(
            f,
            "H.264 encoders: {}\nAudio encoders: {}",
            self.h264_encoders.join(", "),
            self.audio_encoders.join(", ")
        )?;
        writeln!(
            f,
            "NetworkManager: {}\nWi-Fi: {}\nWi-Fi P2P: {}\nDiscovery available: {}",
            self.network_manager_version
                .as_deref()
                .unwrap_or("unavailable"),
            self.wifi_interfaces.join(", "),
            self.p2p_interfaces.join(", "),
            !self.p2p_interfaces.is_empty()
        )?;
        writeln!(
            f,
            "Audio: not enabled yet; PipeWire detected: {}",
            self.audio.pipewire_detected
        )?;
        for limitation in &self.limitations {
            writeln!(f, "• {limitation}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct AudioCapabilities {
    pub pipewire_detected: bool,
    pub enabled: bool,
    pub codecs: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum AppCommand {
    StartDiscovery,
    StopDiscovery,
    RefreshDevices,
    Connect(DeviceId, protocol::wfd::video::VideoMode),
    Disconnect,
    StartCapture(CaptureRequest),
    StopCapture,
    RunDiagnostics,
    SimulateError,
    Shutdown,
}

#[derive(Clone, Debug)]
pub enum DiscoveryEvent {
    Upsert(WirelessDisplayDevice),
    Removed(DeviceId),
    Warning(String),
    Stopped,
}

/// Measured on the source pipeline, not the receiver's display refresh rate.
#[derive(Clone, Copy, Debug, Default)]
pub struct VideoStatistics {
    pub input_fps: f64,
    pub encoded_fps: f64,
    pub dropped_frames: u64,
}

#[derive(Clone, Debug)]
pub enum MediaEvent {
    Error(String),
    Eos,
    StateChanged { playing: bool },
    Warning(String),
    Frames(u64),
    Packets(u64),
    VideoStatistics(VideoStatistics),
    Quality(protocol::wfd::quality::QualitySample),
}

#[derive(Clone, Debug)]
pub enum AppEvent {
    StateChanged(crate::AppState),
    DeviceAdded(WirelessDisplayDevice),
    DeviceUpdated(WirelessDisplayDevice),
    DeviceRemoved(DeviceId),
    DiscoveryStarted,
    DiscoveryStopped,
    CaptureSourceSelected(CaptureInfo),
    CaptureStarted(CaptureInfo),
    CaptureStopped,
    MediaPipelineStarted,
    MediaPipelineStopped,
    Media(MediaEvent),
    StreamNegotiated(protocol::wfd::video::StreamingInfo),
    DiagnosticResult(Box<SystemCapabilities>),
    Warning(String),
    Error(String),
    ShutdownComplete,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_mac_and_names() {
        let mac: MacAddress = "AA:bb:CC:00:12:34".parse().unwrap();
        assert_eq!(mac.masked(), "AA:BB:**:**:**:**");
        for bad in ["a:b:c:d:e:f", "00:00:00:00:00:00:01", "GG:00:00:00:00:00"] {
            assert!(bad.parse::<MacAddress>().is_err());
        }
        assert!(
            WirelessDisplayDevice::try_new(
                DeviceId::try_new("peer").unwrap(),
                "",
                mac,
                None,
                String::new()
            )
            .is_err()
        );
    }

    #[test]
    fn validates_wfd_and_classifies_only_sinks() {
        let caps = WfdCapability::parse(&[0, 0, 6, 0, 0x11, 0x1c, 0x44, 0, 50])
            .unwrap()
            .unwrap();
        assert!(caps.is_receiver());
        assert_eq!(caps.rtsp_port, 7236);
        assert!(caps.session_available);
        assert!(WfdCapability::parse(&[0, 0, 6, 0]).is_err());
        assert!(WfdCapability::parse(&[]).unwrap().is_none());
        assert!(
            !WfdCapability {
                device_type: 0,
                ..caps
            }
            .is_receiver()
        );
    }
}
