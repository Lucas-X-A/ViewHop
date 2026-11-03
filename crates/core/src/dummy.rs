use crate::traits::{CaptureBackend, CaptureLease, DeviceDiscovery, MediaPipeline, P2pConnector};
use crate::{
    CaptureInfo, CaptureKind, CaptureRequest, CaptureSource, DeviceId, DiscoveryEvent,
    WfdCapability, WirelessDisplayDevice,
};
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct MockDeviceDiscovery;

#[must_use]
pub fn receiver() -> WirelessDisplayDevice {
    // Fixed, locally administered test MAC and explicitly simulated device.
    WirelessDisplayDevice::try_new(
        DeviceId::try_new("dummy:tv")
            .expect("the fixed dummy ID is nonempty and contains no controls"),
        "Test TV (simulated)",
        "02:00:00:00:00:01"
            .parse()
            .expect("the fixed dummy MAC contains six hexadecimal octets"),
        Some(WfdCapability {
            device_type: 1,
            session_available: true,
            rtsp_port: 7236,
            max_throughput_mbps: 50,
        }),
        "dummy".into(),
    )
    .expect("the fixed dummy name is nonempty and shorter than 256 bytes")
}

impl DeviceDiscovery for MockDeviceDiscovery {
    type Error = Infallible;
    async fn discover(
        &self,
        events: async_channel::Sender<DiscoveryEvent>,
        cancel: CancellationToken,
    ) -> Result<(), Self::Error> {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = events.send(DiscoveryEvent::Upsert(receiver())) => { if result.is_err() { return Ok(()); } }
        }
        cancel.cancelled().await;
        Ok(())
    }
}

pub struct MockCaptureBackend;
pub struct MockCaptureLease;
impl CaptureLease for MockCaptureLease {
    fn source(&self) -> CaptureSource {
        CaptureSource::Test
    }
    fn info(&self) -> CaptureInfo {
        CaptureInfo {
            backend: CaptureKind::Dummy,
            logical_size: Some((1280, 720)),
        }
    }
}
impl CaptureBackend for MockCaptureBackend {
    type Error = Infallible;
    type Lease = MockCaptureLease;
    async fn capture(
        &self,
        _: CaptureRequest,
        _: CancellationToken,
    ) -> Result<Self::Lease, Self::Error> {
        Ok(MockCaptureLease)
    }
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("Simulated operation cancelled")]
pub struct MockCancelled;
pub struct MockP2pConnector;
impl P2pConnector for MockP2pConnector {
    type Error = MockCancelled;
    type Connection = ();
    async fn connect(
        &self,
        _: &WirelessDisplayDevice,
        cancel: CancellationToken,
    ) -> Result<(), Self::Error> {
        tokio::select! { _ = cancel.cancelled() => Err(MockCancelled), _ = tokio::time::sleep(Duration::from_millis(100)) => Ok(()) }
    }
    async fn disconnect(&self, _: ()) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Default)]
pub struct MockMediaPipeline(AtomicBool);
impl MediaPipeline for MockMediaPipeline {
    type Error = Infallible;
    fn start(&self) -> Result<(), Self::Error> {
        self.0.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn pause(&self) -> Result<(), Self::Error> {
        self.0.store(false, Ordering::Relaxed);
        Ok(())
    }
    fn stop(&self) -> Result<(), Self::Error> {
        self.pause()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn dummy_discovers_and_cancels() {
        let token = CancellationToken::new();
        let (tx, rx) = async_channel::bounded(4);
        let task = tokio::spawn({
            let token = token.clone();
            async move { MockDeviceDiscovery.discover(tx, token).await }
        });
        assert!(matches!(
            rx.recv().await.unwrap(),
            DiscoveryEvent::Upsert(_)
        ));
        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    #[tokio::test]
    async fn dummy_connection_cancellation() {
        let token = CancellationToken::new();
        token.cancel();
        assert!(MockP2pConnector.connect(&receiver(), token).await.is_err());
    }
}
