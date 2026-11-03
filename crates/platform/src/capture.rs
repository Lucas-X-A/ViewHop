use crate::environment::DesktopEnvironment;
use ashpd::desktop::Session;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wireless_display_core::traits::{CaptureBackend, CaptureLease};
use wireless_display_core::{CaptureInfo, CaptureKind, CaptureRequest, CaptureSource, SessionType};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Str};
use zbus::{Connection, Proxy};

#[derive(Debug, thiserror::Error)]
pub enum PortalError {
    #[error("ScreenCast Portal: {0}")]
    Portal(#[from] ashpd::Error),
    #[error("ScreenCast D-Bus: {0}")]
    Dbus(#[from] zbus::Error),
    #[error("Invalid portal response: {0}")]
    Value(#[from] zbus::zvariant::Error),
    #[error("Display selection cancelled")]
    Cancelled,
    #[error("The portal does not provide monitor capture")]
    Unsupported,
    #[error("The portal did not return a valid monitor stream")]
    NoStream,
    #[error("ScreenCast Portal timed out")]
    Timeout,
    #[error("ScreenCast Portal request failed (code {0})")]
    Response(u32),
    #[error("X11 capture unavailable: the session is not X11 or DISPLAY is missing")]
    X11Unavailable,
    #[error(
        "Wayland requires an XDG Desktop Portal ScreenCast backend. Install/configure the backend for your desktop."
    )]
    WaylandUnavailable,
}

impl PortalError {
    fn unavailable(&self) -> bool {
        match self {
            Self::Portal(ashpd::Error::PortalNotFound(_)) | Self::Unsupported => true,
            Self::Portal(ashpd::Error::Zbus(zbus::Error::MethodError(name, _, _))) => matches!(
                name.as_str(),
                "org.freedesktop.DBus.Error.ServiceUnknown"
                    | "org.freedesktop.DBus.Error.NameHasNoOwner"
                    | "org.freedesktop.DBus.Error.UnknownObject"
                    | "org.freedesktop.DBus.Error.UnknownInterface"
                    | "org.freedesktop.DBus.Error.UnknownMethod"
                    | "org.freedesktop.DBus.Error.UnknownProperty"
            ),
            _ => false,
        }
    }
}

pub struct SelectedCapture {
    source: CaptureSource,
    info: CaptureInfo,
    session: Option<Arc<Session<Screencast>>>,
    portal: Option<Screencast>,
}

pub type PortalSession = Session<Screencast>;

impl CaptureLease for SelectedCapture {
    fn source(&self) -> CaptureSource {
        self.source.clone()
    }
    fn info(&self) -> CaptureInfo {
        self.info.clone()
    }
}

impl SelectedCapture {
    #[must_use]
    pub fn portal_session(&self) -> Option<Arc<Session<Screencast>>> {
        self.session.clone()
    }

    /// A PipeWire remote is a protocol connection, not a reusable stream locator.
    /// After the geometry probe disconnects its core, obtain a fresh restricted
    /// remote using the same authorized session and D-Bus owner connection.
    pub async fn reopen_remote(&mut self, cancel: &CancellationToken) -> Result<(), PortalError> {
        let CaptureSource::PipeWire { remote, .. } = &mut self.source else {
            return Ok(());
        };
        let portal = self.portal.as_ref().ok_or(PortalError::Unsupported)?;
        let session = self.session.as_ref().ok_or(PortalError::Unsupported)?;
        let fd = tokio::select! {
            _ = cancel.cancelled() => return Err(PortalError::Cancelled),
            result = tokio::time::timeout(Duration::from_secs(5),portal.open_pipe_wire_remote(session,Default::default())) => result.map_err(|_| PortalError::Timeout)??,
        };
        *remote = Arc::new(fd);
        Ok(())
    }

    pub async fn close(mut self) -> Result<(), PortalError> {
        if let Some(session) = self.session.take() {
            tokio::time::timeout(Duration::from_secs(5), session.close())
                .await
                .map_err(|_| PortalError::Timeout)??;
        }
        Ok(())
    }
}

pub struct PortalCaptureBackend;
pub struct X11CaptureBackend {
    pub environment: DesktopEnvironment,
}

#[derive(Debug, Default)]
pub struct PortalCapabilities {
    pub available: bool,
    pub version: Option<u32>,
    pub source_types: Vec<String>,
    pub monitor: bool,
}

impl PortalCaptureBackend {
    pub async fn capabilities() -> Result<PortalCapabilities, PortalError> {
        tokio::time::timeout(Duration::from_secs(8), async {
            let portal = Screencast::new().await?;
            let types = portal.available_source_types().await?;
            let mut source_types = Vec::new();
            if types.contains(SourceType::Monitor) {
                source_types.push("Monitor".into());
            }
            if types.contains(SourceType::Window) {
                source_types.push("Window".into());
            }
            Ok(PortalCapabilities {
                available: true,
                version: Some(portal.version()),
                source_types,
                monitor: types.contains(SourceType::Monitor),
            })
        })
        .await
        .map_err(|_| PortalError::Timeout)?
    }
}

#[derive(Debug)]
struct PortalStream {
    node: u32,
    serial: Option<u64>,
    size: Option<(i32, i32)>,
}

fn stream_from_results(
    mut values: HashMap<String, OwnedValue>,
) -> Result<PortalStream, PortalError> {
    let streams = values.remove("streams").ok_or(PortalError::NoStream)?;
    let streams = Vec::<(u32, HashMap<String, OwnedValue>)>::try_from(streams)?;
    if streams.len() != 1 {
        return Err(PortalError::NoStream);
    }
    let (node, mut properties) = streams.into_iter().next().ok_or(PortalError::NoStream)?;
    let serial = properties
        .remove("pipewire-serial")
        .map(u64::try_from)
        .transpose()?;
    let size = properties
        .remove("size")
        .map(<(i32, i32)>::try_from)
        .transpose()?;
    if node == 0 || serial == Some(0) || size.is_some_and(|(w, h)| w <= 0 || h <= 0) {
        return Err(PortalError::NoStream);
    }
    Ok(PortalStream { node, serial, size })
}

/// ashpd 0.13's typed Streams discards the ScreenCast v6 pipewire-serial.
/// Keep ashpd session/source/FD handling and decode Start's dictionary here.
/// Subscribe to the exact request path before invoking Start (portal protocol).
async fn start_with_serial(
    portal: &Screencast,
    session: &Session<Screencast>,
) -> Result<PortalStream, PortalError> {
    static REQUEST_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let token = format!(
        "wireless_display_{}_{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let sender = portal
        .connection()
        .unique_name()
        .ok_or(PortalError::NoStream)?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let request = Proxy::new(
        portal.connection(),
        "org.freedesktop.portal.Desktop",
        path.as_str(),
        "org.freedesktop.portal.Request",
    )
    .await?;
    let mut responses = request.receive_signal("Response").await?;
    let options = HashMap::from([("handle_token", OwnedValue::from(Str::from(token.as_str())))]);
    let actual: OwnedObjectPath = portal.call("Start", &(session, "", options)).await?;
    if actual.as_str() != path {
        return Err(PortalError::NoStream);
    }
    let message = responses.next().await.ok_or(PortalError::NoStream)?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = message.body().deserialize()?;
    match code {
        0 => stream_from_results(results),
        1 => Err(PortalError::Cancelled),
        other => Err(PortalError::Response(other)),
    }
}

impl CaptureBackend for PortalCaptureBackend {
    type Error = PortalError;
    type Lease = SelectedCapture;

    async fn capture(
        &self,
        _: CaptureRequest,
        cancel: CancellationToken,
    ) -> Result<Self::Lease, PortalError> {
        // A dedicated connection also releases sessions if CreateSession races cancellation.
        let setup = async {
            let portal = Screencast::with_connection(Connection::session().await?).await?;
            let types = portal.available_source_types().await?;
            if !types.contains(SourceType::Monitor) {
                return Err(PortalError::Unsupported);
            }
            let session = portal.create_session(Default::default()).await?;
            Ok((portal, session))
        };
        let (portal, session) = tokio::select! {
            _ = cancel.cancelled() => return Err(PortalError::Cancelled),
            result = tokio::time::timeout(Duration::from_secs(10), setup) => result.map_err(|_| PortalError::Timeout)??,
        };
        let select = async {
            let mut options = SelectSourcesOptions::default()
                .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                .set_multiple(false);
            if portal.version() >= 2 {
                let modes = portal.available_cursor_modes().await?;
                if modes.contains(CursorMode::Embedded) {
                    options = options.set_cursor_mode(CursorMode::Embedded);
                }
            }
            portal.select_sources(&session, options).await?.response()?;
            let stream = start_with_serial(&portal, &session).await?;
            let fd = portal
                .open_pipe_wire_remote(&session, Default::default())
                .await?;
            Ok((stream, fd))
        };
        let result: Result<_, PortalError> = tokio::select! {
            _ = cancel.cancelled() => Err(PortalError::Cancelled),
            result = tokio::time::timeout(Duration::from_secs(120), select) => result.unwrap_or(Err(PortalError::Timeout)),
        };
        match result {
            Ok((stream, fd)) => {
                tracing::info!(target: "wireless_display::portal", "Obtained restricted portal stream");
                Ok(SelectedCapture {
                    source: CaptureSource::PipeWire {
                        remote: Arc::new(fd),
                        node_id: stream.node,
                        serial: stream.serial,
                    },
                    info: CaptureInfo {
                        backend: CaptureKind::Portal,
                        logical_size: stream.size,
                    },
                    session: Some(Arc::new(session)),
                    portal: Some(portal),
                })
            }
            Err(error) => {
                let _ = tokio::time::timeout(Duration::from_secs(5), session.close()).await;
                Err(error)
            }
        }
    }
}

impl CaptureBackend for X11CaptureBackend {
    type Error = PortalError;
    type Lease = SelectedCapture;
    async fn capture(
        &self,
        _: CaptureRequest,
        cancel: CancellationToken,
    ) -> Result<Self::Lease, PortalError> {
        if cancel.is_cancelled() {
            return Err(PortalError::Cancelled);
        }
        if self.environment.session_type != SessionType::X11 {
            return Err(PortalError::X11Unavailable);
        }
        let display = self
            .environment
            .x11_display
            .clone()
            .filter(|v| !v.is_empty())
            .ok_or(PortalError::X11Unavailable)?;
        Ok(SelectedCapture {
            source: CaptureSource::X11 { display },
            info: CaptureInfo {
                backend: CaptureKind::X11,
                logical_size: None,
            },
            session: None,
            portal: None,
        })
    }
}

pub async fn select_capture(
    environment: &DesktopEnvironment,
    request: CaptureRequest,
    cancel: CancellationToken,
) -> Result<SelectedCapture, PortalError> {
    // Only lack of ScreenCast support permits fallback. User denial, runtime failures,
    // or cancellation must never start recording through another backend.
    let probe = tokio::select! {
        _ = cancel.cancelled() => return Err(PortalError::Cancelled),
        result = PortalCaptureBackend::capabilities() => result,
    };
    match probe {
        Ok(caps) if caps.monitor => PortalCaptureBackend.capture(request, cancel).await,
        Err(error) if !error.unavailable() => Err(error),
        Ok(_) | Err(_) => {
            if environment.session_type == SessionType::X11 {
                X11CaptureBackend {
                    environment: environment.clone(),
                }
                .capture(request, cancel)
                .await
            } else {
                Err(PortalError::WaylandUnavailable)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_or_failure_never_authorizes_fallback() {
        assert!(!PortalError::Cancelled.unavailable());
        assert!(!PortalError::Timeout.unavailable());
        assert!(!PortalError::Response(2).unavailable());
        assert!(PortalError::Unsupported.unavailable());
    }
    #[tokio::test]
    async fn wayland_never_uses_x11_even_with_display() {
        let environment = DesktopEnvironment {
            session_type: SessionType::Wayland,
            desktop: "test".into(),
            x11_display: Some(":0".into()),
            pipewire_socket_visible: false,
            flatpak: false,
        };
        assert!(matches!(
            X11CaptureBackend { environment }
                .capture(CaptureRequest, CancellationToken::new())
                .await,
            Err(PortalError::X11Unavailable)
        ));
    }
    #[test]
    fn reads_modern_serial_and_legacy_node_id() {
        use zbus::zvariant::Value;
        for serial in [None, Some(123456u64)] {
            let mut props = HashMap::new();
            if let Some(serial) = serial {
                props.insert("pipewire-serial".to_string(), OwnedValue::from(serial));
            }
            let value = Value::new(vec![(42u32, props)]).try_to_owned().unwrap();
            let result = stream_from_results(HashMap::from([("streams".into(), value)])).unwrap();
            assert_eq!(result.node, 42);
            assert_eq!(result.serial, serial);
        }
    }
}
