use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wireless_display_core::traits::CaptureLease;
use wireless_display_core::{CaptureKind, CaptureRequest, MediaEvent, SessionType};
use wireless_display_media::GstMediaPipeline;
use wireless_display_platform::capture::{self, PortalError};
use wireless_display_platform::environment::DesktopEnvironment;

#[tokio::test]
#[ignore = "requires Xvfb and a private D-Bus session without ScreenCast Portal"]
async fn unavailable_portal_falls_back_only_on_x11() {
    let cancel = CancellationToken::new();
    let mut environment = DesktopEnvironment::detect(Some(SessionType::X11));
    environment.session_type = SessionType::Wayland;
    assert!(matches!(
        capture::select_capture(&environment, CaptureRequest, cancel.clone()).await,
        Err(PortalError::WaylandUnavailable)
    ));
    environment.session_type = SessionType::X11;
    let lease = capture::select_capture(&environment, CaptureRequest, cancel.clone())
        .await
        .unwrap();
    assert_eq!(lease.info().backend, CaptureKind::X11);
    let pipeline = GstMediaPipeline::try_new(lease.source()).unwrap();
    let (tx, rx) = async_channel::bounded(32);
    let child = cancel.clone();
    let task = tokio::spawn(async move { pipeline.run(tx, child).await });
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if matches!(rx.recv().await.unwrap(), MediaEvent::Frames(n) if n > 0) {
                break;
            }
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    task.await.unwrap().unwrap();
    lease.close().await.unwrap();
}
