use gtk::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;
use wireless_display_app::coordinator::Backend;
use wireless_display_app::ui;
use wireless_display_core::{
    AppCommand, AppEvent, AppState, CaptureRequest, SessionState, protocol,
};
use wireless_display_platform::environment::DesktopEnvironment;

fn button_named(widget: &gtk::Widget, text: &str) -> Option<gtk::Button> {
    if let Some(button) = widget.downcast_ref::<gtk::Button>()
        && button.label().as_deref() == Some(text)
    {
        return Some(button.clone());
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        if let Some(button) = button_named(&widget, text) {
            return Some(button);
        }
        child = widget.next_sibling();
    }
    None
}

fn dropdown_in(widget: &gtk::Widget) -> Option<gtk::DropDown> {
    if let Some(dropdown) = widget.downcast_ref::<gtk::DropDown>() {
        return Some(dropdown.clone());
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        if let Some(dropdown) = dropdown_in(&widget) {
            return Some(dropdown);
        }
        child = widget.next_sibling();
    }
    None
}

/// Runs GTK on the test's main thread and Tokio on the backend thread.
#[test]
#[ignore = "requires a display; run with Xvfb and --test-threads=1"]
fn gtk_dummy_vertical_slice() {
    let application = gtk::Application::builder()
        .application_id("org.example.WirelessDisplay.SmokeTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let backend = Rc::new(RefCell::new(None::<Backend>));
    let holder = backend.clone();
    let completed = Rc::new(Cell::new(false));
    let passed = completed.clone();
    application.connect_activate(move |application| {
        let backend = Backend::spawn(true, DesktopEnvironment::detect(None)).unwrap();
        let commands = backend.commands.clone();
        let events = backend.events.clone();
        let (ui_tx, ui_rx) = async_channel::bounded(256);
        let (ui_commands, mut sent) = tokio::sync::mpsc::channel(64);
        let window = ui::build(application, ui_commands, ui_rx, true);
        let dropdown = dropdown_in(window.upcast_ref()).expect("resolution selector");
        assert!(dropdown.is_sensitive());
        let model = dropdown
            .model()
            .unwrap()
            .downcast::<gtk::StringList>()
            .unwrap();
        assert_eq!(
            model.string(0).as_deref(),
            Some("720p — prioritize performance")
        );
        let sixty =
            (0..model.n_items()).find(|i| model.string(*i).is_some_and(|s| s.contains("60 fps")));
        let selected = sixty
            .or_else(|| {
                (0..model.n_items())
                    .find(|i| model.string(*i).is_some_and(|s| s.starts_with("1080p")))
            })
            .unwrap_or(0);
        dropdown.set_selected(selected);
        let preferred = if sixty == Some(selected) {
            protocol::wfd::video::VideoMode::Hd60
        } else if selected == 0 {
            protocol::wfd::video::VideoMode::Hd
        } else {
            protocol::wfd::video::VideoMode::FullHd
        };
        let forwarded = commands.clone();
        let preference_seen = Rc::new(Cell::new(false));
        let seen = preference_seen.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Some(command) = sent.recv().await {
                if let AppCommand::Connect(_, mode) = &command {
                    assert_eq!(*mode, preferred);
                    seen.set(true);
                }
                forwarded.send(command).await.unwrap();
            }
        });
        assert!(window.is_visible());
        *holder.borrow_mut() = Some(backend);
        let timeout_application = application.clone();
        let application = application.clone();
        let passed = passed.clone();
        glib::MainContext::default().spawn_local(async move {
            commands.send(AppCommand::StartDiscovery).await.unwrap();
            let mut phase = 0;
            while let Ok(event) = events.recv().await {
                ui_tx.send(event.clone()).await.unwrap();
                match (&event, phase) {
                    (AppEvent::DeviceAdded(peer), 0) => {
                        phase = 1;
                        glib::timeout_future(Duration::from_millis(30)).await;
                        let connect =
                            button_named(window.upcast_ref(), "Simulate connection").unwrap();
                        let row = connect.parent().unwrap();
                        let mut updated = peer.clone();
                        for available in [false, true] {
                            updated.wfd.as_mut().unwrap().session_available = available;
                            updated.name = "Updated TV".into();
                            ui_tx
                                .send(AppEvent::DeviceUpdated(updated.clone()))
                                .await
                                .unwrap();
                            glib::timeout_future(Duration::from_millis(30)).await;
                            assert_eq!(
                                button_named(window.upcast_ref(), "Simulate connection").unwrap(),
                                connect,
                                "updates must preserve the connection button and row"
                            );
                            assert_eq!(connect.parent().unwrap(), row);
                            assert_eq!(connect.is_sensitive(), available);
                        }
                        connect.emit_clicked();
                    }
                    (
                        AppEvent::StateChanged(AppState {
                            session:
                                SessionState::Streaming {
                                    simulated: true, ..
                                },
                            ..
                        }),
                        1,
                    ) => {
                        phase = 2;
                        glib::timeout_future(Duration::from_millis(30)).await;
                        assert!(
                            preference_seen.get(),
                            "GUI must send the selected preference through Connect"
                        );
                        assert!(
                            !dropdown.is_sensitive(),
                            "resolution changes apply to the next connection"
                        );
                        commands
                            .send(AppCommand::StartCapture(CaptureRequest))
                            .await
                            .unwrap();
                    }
                    (AppEvent::CaptureStarted(_), 2) => {
                        phase = 3;
                        commands.send(AppCommand::StopCapture).await.unwrap();
                    }
                    (AppEvent::CaptureStopped, 3) => {
                        phase = 4;
                        commands.send(AppCommand::Disconnect).await.unwrap();
                    }
                    (
                        AppEvent::StateChanged(AppState {
                            session: SessionState::Idle,
                            ..
                        }),
                        4,
                    ) => {
                        phase = 5;
                        commands.send(AppCommand::SimulateError).await.unwrap();
                    }
                    (
                        AppEvent::StateChanged(AppState {
                            session: SessionState::Error(_),
                            ..
                        }),
                        5,
                    ) => {
                        phase = 6;
                        glib::timeout_future(Duration::from_millis(30)).await;
                        assert!(
                            button_named(window.upcast_ref(), "Search for devices")
                                .unwrap()
                                .is_sensitive()
                        );
                        commands.send(AppCommand::RefreshDevices).await.unwrap();
                    }
                    (AppEvent::DeviceAdded(_), 6) => {
                        phase = 7;
                        commands.send(AppCommand::Shutdown).await.unwrap();
                    }
                    (AppEvent::ShutdownComplete, 7) => {
                        passed.set(true);
                        application.quit();
                        return;
                    }
                    (AppEvent::Error(error), _) => panic!("Unexpected backend error: {error}"),
                    _ => {}
                }
            }
        });
        glib::timeout_add_local_once(Duration::from_secs(20), move || timeout_application.quit());
    });
    application.run_with_args(&["wireless-display-smoke"]);
    if let Some(backend) = backend.borrow_mut().take() {
        backend.join();
    }
    assert!(completed.get(), "GTK dummy lifecycle timed out");
}
