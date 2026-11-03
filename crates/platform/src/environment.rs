use std::env;
use std::path::PathBuf;
use wireless_display_core::traits::AudioCapture;
use wireless_display_core::{AudioCapabilities, SessionType};

#[derive(Clone, Debug)]
pub struct DesktopEnvironment {
    pub session_type: SessionType,
    pub desktop: String,
    pub x11_display: Option<String>,
    pub pipewire_socket_visible: bool,
    pub flatpak: bool,
}

impl DesktopEnvironment {
    #[must_use]
    pub fn detect(gdk_hint: Option<SessionType>) -> Self {
        let declared = match env::var("XDG_SESSION_TYPE").as_deref() {
            Ok("wayland") => SessionType::Wayland,
            Ok("x11") => SessionType::X11,
            _ if env::var_os("WAYLAND_DISPLAY").is_some() => SessionType::Wayland,
            _ if env::var_os("DISPLAY").is_some() => SessionType::X11,
            _ => SessionType::Unknown,
        };
        let session_type = resolve_session(declared, gdk_hint);
        let pipewire_socket_visible = env::var_os("XDG_RUNTIME_DIR")
            .is_some_and(|dir| PathBuf::from(dir).join("pipewire-0").exists());
        Self {
            session_type,
            desktop: env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unknown".into()),
            x11_display: env::var("DISPLAY").ok(),
            pipewire_socket_visible,
            flatpak: PathBuf::from("/.flatpak-info").exists(),
        }
    }
}

fn resolve_session(declared: SessionType, gdk_hint: Option<SessionType>) -> SessionType {
    // GDK X11 may be XWayland. It must never override a known Wayland session.
    if declared == SessionType::Wayland || gdk_hint == Some(SessionType::Wayland) {
        SessionType::Wayland
    } else {
        gdk_hint
            .filter(|hint| *hint != SessionType::Unknown)
            .unwrap_or(declared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xwayland_cannot_enable_x11_capture() {
        assert_eq!(
            resolve_session(SessionType::Wayland, Some(SessionType::X11)),
            SessionType::Wayland
        );
        assert_eq!(
            resolve_session(SessionType::X11, Some(SessionType::Wayland)),
            SessionType::Wayland
        );
        assert_eq!(
            resolve_session(SessionType::Unknown, Some(SessionType::X11)),
            SessionType::X11
        );
    }
}

impl AudioCapture for DesktopEnvironment {
    fn capabilities(&self) -> AudioCapabilities {
        AudioCapabilities {
            pipewire_detected: self.pipewire_socket_visible,
            enabled: false,
            codecs: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FlatpakEnvironmentError {
    #[error(
        "NetworkManager is blocked by the sandbox or unavailable. Check the manifest's system-talk-name permission."
    )]
    NetworkManagerAccess,
    #[error("The desktop backend does not provide ScreenCast Portal; Wayland requires it.")]
    ScreenCastUnavailable,
}
