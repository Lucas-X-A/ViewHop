use gtk::glib;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use tokio::sync::mpsc;
use wireless_display_core::protocol::wfd::video::VideoMode;
use wireless_display_core::{
    APP_NAME, AppCommand, AppEvent, CaptureRequest, CaptureState, DeviceId, MediaEvent, PeerType,
    SessionState,
};

struct DeviceRow {
    row: gtk::ListBoxRow,
    icon: gtk::Image,
    name: gtk::Label,
    kind: gtk::Label,
    connect: gtk::Button,
    eligible: bool,
}

fn send(commands: &mpsc::Sender<AppCommand>, command: AppCommand, notice: &gtk::Label) {
    if commands.try_send(command).is_err() {
        notice.set_text("Backend busy or stopped. Please try again.");
    }
}

fn label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .build()
}

fn card() -> gtk::Box {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.add_css_class("wd-card");
    content
}

fn heading(text: &str) -> gtk::Label {
    let title = label(text);
    title.add_css_class("wd-heading");
    title
}

fn device_description(peer_type: PeerType, eligible: bool) -> (&'static str, &'static str) {
    match (peer_type, eligible) {
        (PeerType::WirelessDisplayReceiver, true) => {
            ("Miracast display · Available", "video-display-symbolic")
        }
        (PeerType::WirelessDisplayReceiver, false) => (
            "Miracast display · Not available for a new session",
            "video-display-symbolic",
        ),
        (PeerType::UnknownP2P, _) => (
            "Wi-Fi Direct device · Miracast support unconfirmed",
            "dialog-question-symbolic",
        ),
    }
}

fn monitor_modes(display: &gtk::gdk::Display) -> Vec<VideoMode> {
    let monitors = display.monitors();
    VideoMode::USER_MODES
        .into_iter()
        .filter(|mode| {
            *mode == VideoMode::Hd
                || (0..monitors.n_items())
                    .filter_map(|i| monitors.item(i)?.downcast::<gtk::gdk::Monitor>().ok())
                    .any(|monitor| {
                        let geometry = monitor.geometry();
                        let scale = monitor.scale_factor().max(1) as u32;
                        (*mode != VideoMode::Hd60
                            || monitor.refresh_rate() == 0
                            || monitor.refresh_rate() >= 59000)
                            && mode.fits((
                                (geometry.width().max(0) as u32).saturating_mul(scale),
                                (geometry.height().max(0) as u32).saturating_mul(scale),
                            ))
                    })
        })
        .collect()
}

fn refresh_modes(
    display: &gtk::gdk::Display,
    dropdown: &gtk::DropDown,
    modes: &RefCell<Vec<VideoMode>>,
) {
    let selected = modes
        .borrow()
        .get(dropdown.selected() as usize)
        .copied()
        .unwrap_or_default();
    let available = monitor_modes(display);
    let labels: Vec<_> = available.iter().map(|mode| mode.label()).collect();
    dropdown.set_model(Some(&gtk::StringList::new(&labels)));
    dropdown.set_selected(
        available
            .iter()
            .position(|mode| *mode == selected)
            .unwrap_or(0) as u32,
    );
    *modes.borrow_mut() = available;
}

pub fn build(
    application: &gtk::Application,
    commands: mpsc::Sender<AppCommand>,
    events: async_channel::Receiver<AppEvent>,
    dummy: bool,
) -> gtk::ApplicationWindow {
    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title(APP_NAME)
        .default_width(780)
        .default_height(760)
        .width_request(560)
        .height_request(420)
        .build();
    window.add_css_class("wireless-display");
    let display = gtk::prelude::WidgetExt::display(&window);
    let stylesheet = gtk::CssProvider::new();
    stylesheet.load_from_data(include_str!("ui.css"));
    gtk::style_context_add_provider_for_display(
        &display,
        &stylesheet,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    {
        let display = display.clone();
        window.connect_destroy(move |_| {
            gtk::style_context_remove_provider_for_display(&display, &stylesheet);
        });
    }
    let header = gtk::HeaderBar::new();
    header.set_title_widget(Some(&heading(APP_NAME)));
    if dummy {
        let badge = label("Demo mode");
        badge.add_css_class("wd-badge");
        badge.set_tooltip_text(Some(
            "Devices and connections are simulated. Local capture uses test video.",
        ));
        header.pack_start(&badge);
    }
    window.set_titlebar(Some(&header));

    let root = gtk::Box::new(gtk::Orientation::Vertical, 20);
    root.set_margin_top(24);
    root.set_margin_bottom(24);
    root.set_margin_start(24);
    root.set_margin_end(24);
    let page = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&root)
        .build();
    window.set_child(Some(&page));

    let intro = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    let hero_icon = gtk::Image::from_icon_name("video-display-symbolic");
    hero_icon.set_pixel_size(32);
    hero_icon.add_css_class("wd-hero-icon");
    intro.append(&hero_icon);
    let intro_text = gtk::Box::new(gtk::Orientation::Vertical, 6);
    intro_text.set_hexpand(true);
    let title = label("Share your screen");
    title.add_css_class("wd-title");
    intro_text.append(&title);
    let subtitle = label("Connect to a nearby TV or display using Wi-Fi Direct.");
    subtitle.add_css_class("dim-label");
    intro_text.append(&subtitle);
    intro.append(&intro_text);
    root.append(&intro);
    let limitation = label(if dummy {
        "Demo mode · Simulated connections and test video. No video is sent to a TV."
    } else {
        "Experimental · Video only. Audio sharing is not available yet."
    });
    limitation.add_css_class("dim-label");
    limitation.add_css_class("wd-caption");
    root.append(&limitation);

    let notice_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    notice_box.add_css_class("wd-notice");
    notice_box.set_visible(false);
    let warning_icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
    warning_icon.set_valign(gtk::Align::Start);
    notice_box.append(&warning_icon);
    let notice = label("");
    notice.set_hexpand(true);
    notice.set_selectable(true);
    notice_box.append(&notice);
    let dismiss = gtk::Button::from_icon_name("window-close-symbolic");
    dismiss.set_tooltip_text(Some("Dismiss message"));
    dismiss.set_valign(gtk::Align::Start);
    dismiss.add_css_class("flat");
    notice_box.append(&dismiss);
    {
        let notice_box = notice_box.clone();
        notice
            .connect_label_notify(move |notice| notice_box.set_visible(!notice.text().is_empty()));
        let notice = notice.clone();
        dismiss.connect_clicked(move |_| notice.set_text(""));
    }
    root.append(&notice_box);

    let devices = card();
    let device_header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let devices_title = heading("Nearby displays");
    devices_title.set_hexpand(true);
    device_header.append(&devices_title);
    let search_spinner = gtk::Spinner::new();
    search_spinner.set_visible(false);
    device_header.append(&search_spinner);
    let scan = gtk::Button::with_label("Search for devices");
    scan.add_css_class("suggested-action");
    scan.set_tooltip_text(Some(
        "Search for Wi-Fi Direct displays for up to two minutes.",
    ));
    let stop_scan = gtk::Button::with_label("Stop search");
    stop_scan.set_visible(false);
    device_header.append(&scan);
    device_header.append(&stop_scan);
    devices.append(&device_header);
    let search_help = label("Turn on Screen Share or Miracast on your TV before searching.");
    search_help.add_css_class("dim-label");
    devices.append(&search_help);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("wd-devices");
    let empty = gtk::Box::new(gtk::Orientation::Vertical, 8);
    empty.add_css_class("wd-empty");
    let empty_icon = gtk::Image::from_icon_name("network-wireless-symbolic");
    empty_icon.set_pixel_size(36);
    empty_icon.add_css_class("dim-label");
    empty.append(&empty_icon);
    let empty_title = heading("Find your display");
    empty_title.set_xalign(0.5);
    empty.append(&empty_title);
    let empty_help = label("Search for nearby devices, then choose Share screen.");
    empty_help.set_xalign(0.5);
    empty_help.set_justify(gtk::Justification::Center);
    empty_help.add_css_class("dim-label");
    empty.append(&empty_help);
    list.set_placeholder(Some(&empty));
    devices.append(&list);
    let device_count = label("Your computer connects directly to the display.");
    device_count.add_css_class("dim-label");
    device_count.add_css_class("wd-caption");
    devices.append(&device_count);
    root.append(&devices);

    let quality = card();
    let resolution_box = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    let resolution_label = heading("_Preferred resolution");
    resolution_label.set_use_underline(true);
    resolution_label.set_hexpand(true);
    let resolution = gtk::DropDown::from_strings(&[]);
    resolution_label.set_mnemonic_widget(Some(&resolution));
    resolution.set_tooltip_text(Some("Choose before connecting. This is a preferred limit; the selected screen, encoder, and TV may require a lower mode. 60 fps requires support from all three."));
    let resolution_modes = Rc::new(RefCell::new(vec![VideoMode::Hd]));
    refresh_modes(&display, &resolution, &resolution_modes);
    // GDK objects stay on the GTK thread. Pixel geometry is verified again using
    // the authorized capture stream before any mode is offered to the TV.
    let monitors = display.monitors();
    let watch_monitor = {
        let display = display.clone();
        let dropdown = resolution.clone();
        let modes = resolution_modes.clone();
        move |monitor: gtk::gdk::Monitor| {
            let display = display.clone();
            let dropdown = dropdown.clone();
            let modes = modes.clone();
            monitor
                .connect_notify_local(None, move |_, _| refresh_modes(&display, &dropdown, &modes));
        }
    };
    for i in 0..monitors.n_items() {
        if let Some(monitor) = monitors
            .item(i)
            .and_then(|m| m.downcast::<gtk::gdk::Monitor>().ok())
        {
            watch_monitor(monitor);
        }
    }
    {
        let display = display.clone();
        let dropdown = resolution.clone();
        let modes = resolution_modes.clone();
        monitors.connect_items_changed(move |monitors, position, _, added| {
            for i in position..position + added {
                if let Some(monitor) = monitors
                    .item(i)
                    .and_then(|m| m.downcast::<gtk::gdk::Monitor>().ok())
                {
                    watch_monitor(monitor);
                }
            }
            refresh_modes(&display, &dropdown, &modes);
        });
    }
    resolution_box.append(&resolution_label);
    resolution_box.append(&resolution);
    quality.append(&resolution_box);
    let resolution_help = label(
        "720p prioritizes performance. The actual quality adapts to your screen, encoder, and TV.",
    );
    resolution_help.add_css_class("dim-label");
    quality.append(&resolution_help);

    let session = card();
    session.add_css_class("wd-session");
    session.set_visible(false);
    let session_header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let session_spinner = gtk::Spinner::new();
    session_header.append(&session_spinner);
    let session_text = gtk::Box::new(gtk::Orientation::Vertical, 4);
    session_text.set_hexpand(true);
    let status = heading("");
    let session_device = label("");
    session_device.set_selectable(true);
    session_device.add_css_class("dim-label");
    session_text.append(&status);
    session_text.append(&session_device);
    session_header.append(&session_text);
    let disconnect = gtk::Button::with_label("Stop sharing");
    disconnect.set_valign(gtk::Align::Center);
    session_header.append(&disconnect);
    session.append(&session_header);
    let session_help = label("");
    session.append(&session_help);
    let stream_details = label("");
    stream_details.set_selectable(true);
    stream_details.set_visible(false);
    session.append(&stream_details);
    root.insert_child_after(&session, Some(&notice_box));
    root.append(&quality);

    let capture_summary = card();
    capture_summary.set_visible(false);
    let capture_header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let capture_title = heading("Local capture test");
    capture_title.set_hexpand(true);
    capture_header.append(&capture_title);
    let stop_capture = gtk::Button::with_label("Stop capture");
    capture_header.append(&stop_capture);
    capture_summary.append(&capture_header);
    let capture_status = label("No local capture test running.");
    capture_summary.append(&capture_status);
    root.insert_child_after(&capture_summary, Some(&session));

    let tools = gtk::Box::new(gtk::Orientation::Vertical, 16);
    tools.set_margin_top(16);
    tools.append(&heading("Local capture test"));
    let capture_help = label("Check capture without a TV. No preview or recording is created.");
    capture_help.add_css_class("dim-label");
    tools.append(&capture_help);
    let capture = gtk::Button::with_label("Test local capture");
    capture.set_halign(gtk::Align::Start);
    tools.append(&capture);
    tools.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    tools.append(&heading("Session statistics"));
    let frames = label("Frames consumed: 0");
    let packets = label("RTP packets sent: 0");
    let encoding_details = label("No negotiated video stream.");
    let video_statistics = label("");
    for detail in [&frames, &packets, &encoding_details, &video_statistics] {
        detail.set_selectable(true);
        detail.add_css_class("wd-caption");
        tools.append(detail);
    }
    tools.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let diagnostic_header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let diagnostic_title = heading("System diagnostics");
    diagnostic_title.set_hexpand(true);
    diagnostic_header.append(&diagnostic_title);
    let diagnostic_button = gtk::Button::with_label("Refresh diagnostics");
    diagnostic_header.append(&diagnostic_button);
    tools.append(&diagnostic_header);
    let text = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(12)
        .right_margin(12)
        .build();
    text.buffer().set_text("Collecting capabilities…");
    let diagnostic_scroll = gtk::ScrolledWindow::builder()
        .child(&text)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(180)
        .has_frame(true)
        .build();
    tools.append(&diagnostic_scroll);
    if dummy {
        let error = gtk::Button::with_label("Simulate error");
        error.set_halign(gtk::Align::Start);
        tools.append(&error);
        let commands = commands.clone();
        let notice = notice.clone();
        error.connect_clicked(move |_| send(&commands, AppCommand::SimulateError, &notice));
    }
    let diagnostics = gtk::Expander::builder()
        .label("Tools & diagnostics")
        .child(&tools)
        .build();
    diagnostics.add_css_class("wd-tools");
    root.append(&diagnostics);

    for (button, command) in [
        (&scan, AppCommand::RefreshDevices),
        (&stop_scan, AppCommand::StopDiscovery),
        (&disconnect, AppCommand::Disconnect),
        (&capture, AppCommand::StartCapture(CaptureRequest)),
        (&stop_capture, AppCommand::StopCapture),
    ] {
        let commands = commands.clone();
        let notice = notice.clone();
        button.connect_clicked(move |_| {
            notice.set_text("");
            send(&commands, command.clone(), &notice);
        });
    }
    {
        let commands = commands.clone();
        let notice = notice.clone();
        diagnostic_button.connect_clicked(move |_| {
            send(&commands, AppCommand::RunDiagnostics, &notice);
        });
    }

    let closing = Rc::new(Cell::new(false));
    {
        let commands = commands.clone();
        let closing = closing.clone();
        let application = application.clone();
        window.connect_close_request(move |window| {
            if !closing.replace(true) {
                window.set_sensitive(false);
                let commands = commands.clone();
                let application = application.clone();
                glib::MainContext::default().spawn_local(async move {
                    if commands.send(AppCommand::Shutdown).await.is_err() {
                        application.quit();
                    }
                });
            }
            glib::Propagation::Stop
        });
    }
    let rows: Rc<RefCell<HashMap<DeviceId, DeviceRow>>> = Rc::new(RefCell::new(HashMap::new()));
    let application = application.clone();
    glib::MainContext::default().spawn_local(async move {
        let mut connect_allowed = true;
        let mut live_session = false;
        // Discovery removes rows before publishing the connection state. Retain
        // names for the session summary until the next search, never display IDs.
        let mut device_names = HashMap::new();
        while let Ok(event) = events.recv().await {
            match event {
                AppEvent::StateChanged(state) => {
                    connect_allowed = matches!(state.session, SessionState::Idle | SessionState::Discovering);
                    live_session = !dummy && matches!(state.session, SessionState::DeviceSelected(_) | SessionState::Connecting(_) | SessionState::Negotiating(_) | SessionState::Streaming { .. } | SessionState::Paused(_));
                    let session_active = !matches!(state.session, SessionState::Idle | SessionState::Discovering | SessionState::Error(_));
                    devices.set_visible(!session_active);
                    quality.set_visible(!session_active);
                    session.set_visible(!matches!(state.session, SessionState::Idle | SessionState::Discovering));
                    let (status_text, help_text) = match &state.session {
                        SessionState::Idle | SessionState::Discovering => ("", ""),
                        SessionState::DeviceSelected(_) | SessionState::Connecting(_) => ("Preparing screen sharing…", "Select a screen when the system asks. You can cancel at any time."),
                        SessionState::Negotiating(_) => ("Connecting to your display…", "Waiting for the TV to accept the video settings."),
                        SessionState::Streaming { simulated: true, .. } => ("Simulated connection active", "This connection is simulated. No video is sent to a TV."),
                        SessionState::Streaming { .. } => ("Sharing your screen", "Your selected screen is visible on the TV. Audio is not shared."),
                        SessionState::Paused(_) => ("Sharing paused by the TV", "The TV paused playback. Stop sharing to end the connection."),
                        SessionState::Disconnecting => ("Stopping screen sharing…", "Closing capture and releasing the Wi-Fi Direct connection."),
                        SessionState::Error(error) => {
                            notice.set_text(error);
                            ("Sharing stopped", "Search again to retry, or reset the connection.")
                        },
                    };
                    status.set_text(status_text);
                    session_help.set_text(help_text);
                    let device = match &state.session {
                        SessionState::DeviceSelected(id) | SessionState::Connecting(id) | SessionState::Negotiating(id) | SessionState::Paused(id) | SessionState::Streaming { device: id, .. } => Some(id),
                        _ => None,
                    };
                    session_device.set_text(device.and_then(|id| device_names.get(id)).map(String::as_str).unwrap_or("Wireless display"));
                    session_device.set_visible(device.is_some());
                    let pending = matches!(state.session, SessionState::DeviceSelected(_) | SessionState::Connecting(_) | SessionState::Negotiating(_) | SessionState::Disconnecting);
                    session_spinner.set_spinning(pending);
                    session_spinner.set_visible(pending);
                    disconnect.set_sensitive(!matches!(state.session, SessionState::Disconnecting));
                    disconnect.set_label(match &state.session {
                        SessionState::DeviceSelected(_) | SessionState::Connecting(_) | SessionState::Negotiating(_) => "Cancel connection",
                        SessionState::Streaming { simulated: true, .. } => "Disconnect",
                        SessionState::Error(_) => "Reset connection",
                        SessionState::Disconnecting => "Stopping…",
                        _ => "Stop sharing",
                    });
                    if matches!(state.session, SessionState::Streaming { simulated: false, .. } | SessionState::Paused(_)) {
                        disconnect.add_css_class("destructive-action");
                    } else {
                        disconnect.remove_css_class("destructive-action");
                    }
                    if !matches!(state.session, SessionState::Streaming { .. } | SessionState::Paused(_)) {
                        stream_details.set_visible(false);
                    }
                    resolution.set_sensitive(connect_allowed);
                    scan.set_sensitive(connect_allowed || matches!(state.session, SessionState::Error(_)));
                    scan.set_visible(!state.discovering);
                    stop_scan.set_visible(state.discovering);
                    search_spinner.set_spinning(state.discovering);
                    search_spinner.set_visible(state.discovering);
                    empty_title.set_text(if state.discovering { "Looking for displays…" } else { "Find your display" });
                    empty_help.set_text(if state.discovering { "Keep your TV in Screen Share or Miracast mode.\nYou can connect as soon as it appears." } else { "Search for nearby devices, then choose Share screen." });
                    if rows.borrow().is_empty() {
                        device_count.set_text(if state.discovering { "Searching nearby for up to two minutes…" } else { "Your computer connects directly to the display." });
                    }
                    capture.set_sensitive(state.capture == CaptureState::Stopped && !live_session);
                    capture_summary.set_visible(state.capture != CaptureState::Stopped && !live_session);
                    if live_session {
                        capture_status.set_text("Screen capture is in use by the sharing session.");
                    } else {
                        match state.capture {
                            CaptureState::Stopped => capture_status.set_text("No local capture test running."),
                            CaptureState::Selecting | CaptureState::Ready => capture_status.set_text(if dummy { "Preparing test video…" } else { "Select a screen in the system dialog…" }),
                            CaptureState::Running => {},
                        }
                    }
                    for row in rows.borrow().values() { row.connect.set_sensitive(connect_allowed && row.eligible); }
                },
                AppEvent::DiscoveryStarted => {
                    device_names.clear();
                },
                AppEvent::DiscoveryStopped => {
                    empty_title.set_text("Search finished");
                    empty_help.set_text("No displays are listed. Turn on Screen Share on your TV, then search again.");
                },
                AppEvent::DeviceAdded(device) | AppEvent::DeviceUpdated(device) => {
                    let eligible = device.peer_type == PeerType::WirelessDisplayReceiver && device.wfd.as_ref().is_some_and(|wfd| wfd.session_available);
                    let (kind_text, icon_name) = device_description(device.peer_type, eligible);
                    device_names.insert(device.id.clone(), device.name.clone());
                    if let Some(row) = rows.borrow_mut().get_mut(&device.id) {
                        row.name.set_text(&device.name);
                        row.name.set_tooltip_text(Some(&device.name));
                        row.kind.set_text(kind_text);
                        row.icon.set_icon_name(Some(icon_name));
                        row.eligible = eligible;
                        row.connect.set_sensitive(eligible && connect_allowed);
                        continue;
                    }
                    let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                    content.add_css_class("wd-device");
                    let icon = gtk::Image::from_icon_name(icon_name);
                    icon.set_pixel_size(28);
                    content.append(&icon);
                    let description = gtk::Box::new(gtk::Orientation::Vertical, 4);
                    description.set_hexpand(true);
                    let name = heading(&device.name);
                    name.set_wrap(false);
                    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
                    name.set_max_width_chars(32);
                    name.set_tooltip_text(Some(&device.name));
                    description.append(&name);
                    let kind = label(kind_text);
                    kind.add_css_class("dim-label");
                    kind.add_css_class("wd-caption");
                    description.append(&kind);
                    content.append(&description);
                    let connect = gtk::Button::with_label(if dummy { "Simulate connection" } else { "Share screen" });
                    connect.set_valign(gtk::Align::Center);
                    connect.add_css_class("suggested-action");
                    connect.set_tooltip_text(Some("Choose a screen, then connect to this receiver using Wi-Fi Direct and Miracast."));
                    connect.set_sensitive(eligible && connect_allowed);
                    let id = device.id.clone();
                    let commands = commands.clone();
                    let notice = notice.clone();
                    let resolution = resolution.clone();
                    let resolution_modes = resolution_modes.clone();
                    connect.connect_clicked(move |_| {
                        let mode = resolution_modes.borrow().get(resolution.selected() as usize).copied().unwrap_or_default();
                        notice.set_text("");
                        send(&commands, AppCommand::Connect(id.clone(), mode), &notice);
                    });
                    content.append(&connect);
                    let row = gtk::ListBoxRow::builder().child(&content).activatable(false).build();
                    list.append(&row);
                    rows.borrow_mut().insert(device.id, DeviceRow { row, icon, name, kind, connect, eligible });
                    device_count.set_text(&format!("Nearby devices: {} · Available Miracast displays can connect.", rows.borrow().len()));
                },
                AppEvent::DeviceRemoved(id) => {
                    if let Some(row) = rows.borrow_mut().remove(&id) { list.remove(&row.row); }
                    device_count.set_text(&format!("Nearby devices: {} · Available Miracast displays can connect.", rows.borrow().len()));
                },
                AppEvent::CaptureStarted(info) => {
                    if !live_session {
                        capture_status.set_text(match info.backend {
                            wireless_display_core::CaptureKind::Portal => "Local test active · Screen selected through the system portal.",
                            wireless_display_core::CaptureKind::X11 => "Local test active · Capturing the entire X11 desktop.",
                            wireless_display_core::CaptureKind::Dummy => "Local test active · Using simulated video.",
                        });
                    }
                },
                AppEvent::CaptureSourceSelected(_) => {
                    frames.set_text("Frames consumed: 0");
                    packets.set_text("RTP packets sent: 0");
                    stream_details.set_text("");
                    stream_details.set_visible(false);
                    encoding_details.set_text("No negotiated video stream.");
                    video_statistics.set_text("");
                },
                AppEvent::StreamNegotiated(info) => {
                    stream_details.set_text(&format!("{} × {} · {} fps · {:.1} Mbit/s target", info.video.mode.size().0, info.video.mode.size().1, info.video.mode.fps(), f64::from(info.bitrate_kbps)/1000.0));
                    stream_details.set_visible(true);
                    encoding_details.set_text(&format!("H.264 {:?} · Encoder: {}", info.video.profile, info.encoder));
                },
                AppEvent::CaptureStopped => {
                    if !live_session { capture_status.set_text("No local capture test running."); }
                    stream_details.set_visible(false);
                    video_statistics.set_text("");
                },
                AppEvent::Media(MediaEvent::Frames(count)) => frames.set_text(&format!("Frames consumed: {count}")),
                AppEvent::Media(MediaEvent::VideoStatistics(statistics)) if live_session => video_statistics.set_text(&format!("Encoding: {:.1} fps · Local frames dropped: {}", statistics.encoded_fps, statistics.dropped_frames)),
                AppEvent::Media(MediaEvent::Packets(count)) => packets.set_text(&format!("RTP packets sent: {count}")),
                AppEvent::Media(MediaEvent::Warning(message) | MediaEvent::Error(message)) | AppEvent::Warning(message) | AppEvent::Error(message) => notice.set_text(&message),
                AppEvent::DiagnosticResult(caps) => text.buffer().set_text(&caps.to_string()),
                AppEvent::ShutdownComplete => { tracing::info!(target: "wireless_display::ui", "Cooperative shutdown complete"); application.quit(); return; },
                _ => {}
            }
        }
        tracing::error!(target: "wireless_display::ui", "Backend event channel closed");
        application.quit();
    });
    window.present();
    window
}
