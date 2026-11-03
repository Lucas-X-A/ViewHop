use crate::{
    AudioCapabilities, CaptureInfo, CaptureRequest, CaptureSource, DiscoveryEvent,
    WirelessDisplayDevice,
};
use std::future::Future;
use tokio_util::sync::CancellationToken;

/// Reports discovery updates through a bounded channel until cancelled.
///
/// Implementations finish their native discovery cleanup before returning.
pub trait DeviceDiscovery: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;
    fn discover(
        &self,
        events: async_channel::Sender<DiscoveryEvent>,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Keeps the authorized capture session alive while its source is in use.
///
/// A source clone shares its FD but does not replace the session lease. Stop
/// the media pipeline before closing the lease or its portal session.
pub trait CaptureLease: Send {
    #[must_use]
    fn source(&self) -> CaptureSource;
    #[must_use]
    fn info(&self) -> CaptureInfo;
}

/// Acquires a capture lease without starting the media pipeline.
pub trait CaptureBackend: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;
    type Lease: CaptureLease;
    fn capture(
        &self,
        request: CaptureRequest,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<Self::Lease, Self::Error>> + Send;
}

/// Owns temporary P2P activation through the returned connection.
///
/// Callers stop control/media traffic before consuming it with `disconnect`.
pub trait P2pConnector: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;
    type Connection: Send;
    fn connect(
        &self,
        peer: &WirelessDisplayDevice,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<Self::Connection, Self::Error>> + Send;
    fn disconnect(
        &self,
        connection: Self::Connection,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Controls media independently of the GUI and protocol state machines.
///
/// Implementations release native streaming resources when stopped or dropped.
pub trait MediaPipeline: Send {
    type Error: std::error::Error + Send + Sync + 'static;
    fn start(&self) -> Result<(), Self::Error>;
    fn pause(&self) -> Result<(), Self::Error>;
    fn stop(&self) -> Result<(), Self::Error>;
}

pub trait AudioCapture {
    #[must_use]
    fn capabilities(&self) -> AudioCapabilities;
}
