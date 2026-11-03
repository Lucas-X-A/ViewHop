use gtk::prelude::*;
use gtk::{gio, glib};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;
use wireless_display_app::ui;
use wireless_display_core::protocol::wfd::video::{
    H264Profile, StreamingInfo, VideoFormat, VideoMode,
};
use wireless_display_core::{
    AppCommand, AppEvent, AppState, CaptureState, DeviceId, PeerType, SessionState, WfdCapability,
    WirelessDisplayDevice,
};

fn find<T: IsA<gtk::Widget> + Clone>(widget: &gtk::Widget, matches: &impl Fn(&T) -> bool) -> T {
    fn visit<T: IsA<gtk::Widget> + Clone>(
        widget: &gtk::Widget,
        matches: &impl Fn(&T) -> bool,
    ) -> Option<T> {
        if let Some(value) = widget.downcast_ref::<T>()
            && matches(value)
        {
            return Some(value.clone());
        }
        if let Some(expander) = widget.downcast_ref::<gtk::Expander>()
            && let Some(content) = expander.child()
            && let Some(found) = visit(&content, matches)
        {
            return Some(found);
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            if let Some(found) = visit(&widget, matches) {
                return Some(found);
            }
            child = widget.next_sibling();
        }
        None
    }
    visit(widget, matches).expect("expected UI element")
}

fn button(window: &gtk::ApplicationWindow, name: &str) -> gtk::Button {
    find(window.upcast_ref(), &|button: &gtk::Button| {
        button.label().as_deref() == Some(name)
    })
}

fn text(window: &gtk::ApplicationWindow, value: &str) -> gtk::Label {
    find(window.upcast_ref(), &|label: &gtk::Label| {
        label.text() == value
    })
}

async fn settle() {
    glib::timeout_future(Duration::from_millis(60)).await;
}

async fn state(
    events: &async_channel::Sender<AppEvent>,
    session: SessionState,
    capture: CaptureState,
) {
    let discovering = session == SessionState::Discovering;
    events
        .send(AppEvent::StateChanged(AppState {
            session,
            capture,
            discovering,
        }))
        .await
        .unwrap();
    settle().await;
}

/// Exercises real-session presentation without accessing a portal or Wi-Fi.
#[test]
#[ignore = "requires a display; run with Xvfb and --test-threads=1"]
fn gtk_session_actions_and_disclosure() {
    let application = gtk::Application::builder()
        .application_id("org.example.WirelessDisplay.PresentationTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let completed = Rc::new(Cell::new(false));
    let passed = completed.clone();
    application.connect_activate(move |application| {
        let (commands, mut received) = tokio::sync::mpsc::channel(32);
        let (events, receiver) = async_channel::bounded(32);
        let window = ui::build(application, commands, receiver, false);
        let application = application.clone();
        let timeout_application = application.clone();
        let passed = passed.clone();
        glib::MainContext::default().spawn_local(async move {
            settle().await;
            let tools: gtk::Expander = find(window.upcast_ref(), &|_: &gtk::Expander| true);
            assert!(!tools.is_expanded());
            assert!(!text(&window, "Frames consumed: 0").is_mapped());
            assert!(!button(&window, "Stop capture").is_mapped());
            assert!(text(&window, "Experimental · Video only. Audio sharing is not available yet.").is_mapped());
            button(&window, "Search for devices").emit_clicked();
            assert!(matches!(received.try_recv(), Ok(AppCommand::RefreshDevices)));
            state(&events, SessionState::Discovering, CaptureState::Stopped).await;
            events.send(AppEvent::DiscoveryStarted).await.unwrap();
            assert!(!button(&window, "Search for devices").is_mapped());
            assert!(button(&window, "Stop search").is_mapped());
            button(&window, "Stop search").emit_clicked();
            assert!(matches!(received.try_recv(), Ok(AppCommand::StopDiscovery)));

            let id = DeviceId::try_new("presentation-tv").unwrap();
            let mut peer = WirelessDisplayDevice::try_new(
                id.clone(),
                "<b>Living room TV</b>",
                "02:00:00:00:00:01".parse().unwrap(),
                Some(WfdCapability { device_type: 1, session_available: true, rtsp_port: 7236, max_throughput_mbps: 50 }),
                "test-adapter".into(),
            ).unwrap();
            events.send(AppEvent::DeviceAdded(peer.clone())).await.unwrap();
            settle().await;
            let connect = button(&window, "Share screen");
            assert!(connect.is_sensitive());
            assert!(!text(&window, &peer.name).uses_markup());
            peer.wfd.as_mut().unwrap().session_available = false;
            events.send(AppEvent::DeviceUpdated(peer.clone())).await.unwrap();
            settle().await;
            assert_eq!(connect, button(&window, "Share screen"));
            assert!(!connect.is_sensitive());
            assert!(text(&window, "Miracast display · Not available for a new session").is_mapped());
            let mut unconfirmed = peer.clone();
            unconfirmed.peer_type = PeerType::UnknownP2P;
            unconfirmed.wfd = None;
            events.send(AppEvent::DeviceUpdated(unconfirmed)).await.unwrap();
            settle().await;
            assert!(!connect.is_sensitive());
            assert!(text(&window, "Wi-Fi Direct device · Miracast support unconfirmed").is_mapped());
            peer.wfd.as_mut().unwrap().session_available = true;
            events.send(AppEvent::DeviceUpdated(peer.clone())).await.unwrap();
            settle().await;
            connect.emit_clicked();
            assert!(matches!(received.try_recv(), Ok(AppCommand::Connect(device, VideoMode::Hd)) if device == id));
            events.send(AppEvent::DeviceRemoved(id.clone())).await.unwrap();
            state(&events, SessionState::Connecting(id.clone()), CaptureState::Selecting).await;
            assert!(button(&window, "Cancel connection").is_mapped());
            assert!(!text(&window, "Nearby displays").is_mapped());
            assert!(!button(&window, "Test local capture").is_sensitive());
            assert!(text(&window, &peer.name).is_mapped(), "retain the receiver name after discovery removes its row");
            button(&window, "Cancel connection").emit_clicked();
            assert!(matches!(received.try_recv(), Ok(AppCommand::Disconnect)));

            state(&events, SessionState::Streaming { device: id.clone(), simulated: false }, CaptureState::Running).await;
            events.send(AppEvent::StreamNegotiated(StreamingInfo {
                video: VideoFormat { mode: VideoMode::FullHd, profile: H264Profile::High, level: 4, r2: false },
                encoder: "test-encoder".into(), bitrate_kbps: 8000,
                peer: "192.0.2.2".parse().unwrap(),
            })).await.unwrap();
            settle().await;
            assert!(text(&window, "1920 × 1080 · 30 fps · 8.0 Mbit/s target").is_mapped());
            assert!(button(&window, "Stop sharing").is_mapped());
            assert!(!button(&window, "Stop capture").is_mapped());
            state(&events, SessionState::Paused(id), CaptureState::Ready).await;
            assert!(text(&window, "Sharing paused by the TV").is_mapped());
            assert!(text(&window, "1920 × 1080 · 30 fps · 8.0 Mbit/s target").is_mapped());
            button(&window, "Stop sharing").emit_clicked();
            assert!(matches!(received.try_recv(), Ok(AppCommand::Disconnect)));
            state(&events, SessionState::Disconnecting, CaptureState::Stopped).await;
            assert!(!button(&window, "Stopping…").is_sensitive());
            state(&events, SessionState::Idle, CaptureState::Stopped).await;
            assert!(text(&window, "Nearby displays").is_mapped());
            assert!(button(&window, "Search for devices").is_sensitive());
            assert!(button(&window, "Test local capture").is_sensitive());

            state(&events, SessionState::Error("Receiver unavailable".into()), CaptureState::Stopped).await;
            assert!(text(&window, "Receiver unavailable").is_mapped());
            assert!(button(&window, "Reset connection").is_mapped());
            button(&window, "Reset connection").emit_clicked();
            assert!(matches!(received.try_recv(), Ok(AppCommand::Disconnect)));
            state(&events, SessionState::Idle, CaptureState::Stopped).await;
            tools.set_expanded(true);
            settle().await;
            assert!(text(&window, "Frames consumed: 0").is_mapped());
            button(&window, "Test local capture").emit_clicked();
            assert!(matches!(received.try_recv(), Ok(AppCommand::StartCapture(_))));
            state(&events, SessionState::Idle, CaptureState::Selecting).await;
            assert!(!button(&window, "Test local capture").is_sensitive());
            tools.set_expanded(false);
            settle().await;
            assert!(button(&window, "Stop capture").is_mapped(), "stopping local capture must remain available with tools collapsed");
            button(&window, "Stop capture").emit_clicked();
            assert!(matches!(received.try_recv(), Ok(AppCommand::StopCapture)));
            tools.set_expanded(true);
            button(&window, "Refresh diagnostics").emit_clicked();
            assert!(matches!(received.try_recv(), Ok(AppCommand::RunDiagnostics)));
            window.close();
            assert!(matches!(received.recv().await, Some(AppCommand::Shutdown)));
            passed.set(true);
            events.send(AppEvent::ShutdownComplete).await.unwrap();
            application.quit();
        });
        glib::timeout_add_local_once(Duration::from_secs(15), move || timeout_application.quit());
    });
    application.run_with_args(&["wireless-display-presentation"]);
    assert!(completed.get(), "GTK presentation checks timed out");
}
