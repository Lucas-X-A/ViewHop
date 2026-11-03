use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wireless_display_core::DiscoveryEvent;
use wireless_display_core::traits::DeviceDiscovery;
use wireless_display_platform::network::NetworkManagerService;

#[tokio::test]
#[ignore = "requires real NetworkManager, Wi-Fi P2P hardware and user authorization"]
async fn real_discovery_starts_and_stops() {
    let service = NetworkManagerService::try_new().await.unwrap();
    assert!(
        !service.capabilities().await.unwrap().p2p.is_empty(),
        "No Wi-Fi P2P device"
    );
    let token = CancellationToken::new();
    let child = token.clone();
    let (tx, rx) = async_channel::bounded(128);
    let mut task = tokio::spawn(async move { service.discover(tx, child).await });
    let deadline = tokio::time::sleep(Duration::from_secs(3));
    tokio::pin!(deadline);
    let mut warnings = Vec::new();
    let mut early = false;
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            result = &mut task => { result.unwrap().unwrap(); early = true; break; },
            event = rx.recv() => match event { Ok(DiscoveryEvent::Warning(warning)) => warnings.push(warning), Ok(_) => {}, Err(_) => break }
        }
    }
    token.cancel();
    if !early {
        tokio::time::timeout(Duration::from_secs(12), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    while let Ok(event) = rx.try_recv() {
        if let DiscoveryEvent::Warning(warning) = event {
            warnings.push(warning);
        }
    }
    assert!(warnings.is_empty(), "Discovery warnings: {warnings:?}");
    assert!(!early, "Discovery ended before cancellation");
}
