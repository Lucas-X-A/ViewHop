use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wireless_display_core::{CaptureSource, MediaEvent};
use wireless_display_media::GstMediaPipeline;

#[tokio::test]
#[ignore = "requires X11 DISPLAY and ximagesrc; use a dedicated Xvfb display"]
async fn x11_source_delivers_frames() {
    let source = CaptureSource::X11 {
        display: std::env::var("DISPLAY").unwrap(),
    };
    let token = CancellationToken::new();
    let format = wireless_display_media::capture_format(source.clone(), &token).unwrap();
    let pixels = format.pixels;
    assert!(
        format.supports_60fps,
        "X11 can negotiate capture at 60 fps instead of duplicating 30 fps"
    );
    assert!(
        pixels.0 > 0 && pixels.1 > 0,
        "authorized source must provide pixel geometry"
    );
    let pipeline = GstMediaPipeline::try_new(source).unwrap();
    let cancel = token.clone();
    let (tx, rx) = async_channel::bounded(32);
    let task = tokio::spawn(async move { pipeline.run(tx, cancel).await });
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if matches!(rx.recv().await.unwrap(), MediaEvent::Frames(n) if n > 0) {
                break;
            }
        }
    })
    .await
    .unwrap();
    token.cancel();
    task.await.unwrap().unwrap();
}
