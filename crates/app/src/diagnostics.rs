use wireless_display_core::SystemCapabilities;
use wireless_display_core::traits::AudioCapture;
use wireless_display_media::{self as media, EncoderSelector};
use wireless_display_platform::capture::PortalCaptureBackend;
use wireless_display_platform::environment::DesktopEnvironment;
use wireless_display_platform::network::NetworkManagerService;

#[must_use]
pub fn dummy_capabilities(environment: &DesktopEnvironment) -> SystemCapabilities {
    let mut caps = SystemCapabilities {
        session_type: environment.session_type,
        desktop: environment.desktop.clone(),
        limitations: vec!["Dummy mode: the portal and NetworkManager are not queried. Fields for these integrations were not measured. Use --diagnostics for actual system diagnostics.".into()],
        ..Default::default()
    };
    add_media_capabilities(&mut caps);
    caps
}

fn add_media_capabilities(caps: &mut SystemCapabilities) {
    match media::initialize() {
        Ok(version) => {
            caps.gstreamer_version = Some(version);
            caps.plugins = media::plugins();
            caps.h264_encoders = EncoderSelector::available();
            caps.audio_encoders = media::audio_encoders();
        }
        Err(error) => caps.limitations.push(error.to_string()),
    }
}

pub async fn collect(environment: &DesktopEnvironment) -> SystemCapabilities {
    let mut caps = SystemCapabilities {
        session_type: environment.session_type,
        desktop: environment.desktop.clone(),
        pipewire_available: environment.pipewire_socket_visible,
        audio: environment.capabilities(),
        ..Default::default()
    };
    let (portal, network) = tokio::join!(PortalCaptureBackend::capabilities(), async {
        NetworkManagerService::try_new().await?.capabilities().await
    });
    match portal {
        Ok(portal) => {
            caps.portal_available = portal.available;
            caps.screencast_version = portal.version;
            caps.source_types = portal.source_types;
        }
        Err(error) => caps.limitations.push(error.to_string()),
    }
    match network {
        Ok(network) => {
            caps.network_manager_version = Some(network.version);
            caps.wifi_interfaces = network.wifi;
            caps.p2p_interfaces = network.p2p;
        }
        Err(error) => caps.limitations.push(error.to_string()),
    }
    add_media_capabilities(&mut caps);
    if caps.p2p_interfaces.is_empty() {
        caps.limitations
            .push("Wi-Fi Direct not detected; TV discovery/transport unavailable.".into());
    }
    caps.limitations
        .push("Experimental WFD/RTSP video over MPEG-TS/RTP UDP. Audio, HDCP, TCP, and UIBC are not enabled yet.".into());
    caps
}
