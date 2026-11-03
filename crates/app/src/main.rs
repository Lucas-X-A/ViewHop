use gtk::gio;
use gtk::prelude::*;
use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;
use wireless_display_app::coordinator::Backend;
use wireless_display_app::{diagnostics, ui};
use wireless_display_core::{APP_ID, SessionType};
use wireless_display_platform::environment::DesktopEnvironment;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "wireless_display=info".into()),
        )
        .with_target(true)
        .with_writer(std::io::stderr)
        .init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(target: "wireless_display::ui", "{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let mut dummy = false;
    let mut diagnostic = false;
    let arguments = std::env::args().skip(1);
    for argument in arguments {
        match argument.as_str() {
            "--dummy" => dummy = true,
            "--diagnostics" => diagnostic = true,
            "--version" => {
                writeln!(
                    std::io::stdout(),
                    "WirelessDisplay {}",
                    env!("CARGO_PKG_VERSION")
                )?;
                return Ok(());
            }
            "--help" | "-h" => {
                writeln!(
                    std::io::stdout(),
                    "wireless-display [--dummy] [--diagnostics] [--version]\n--dummy: UI, simulated connection, and videotestsrc\n--diagnostics: actual system capabilities in the terminal, without opening GTK"
                )?;
                return Ok(());
            }
            _ => return Err(format!("Unknown option: {argument}").into()),
        }
    }
    if diagnostic {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let caps = runtime.block_on(diagnostics::collect(&DesktopEnvironment::detect(None)));
        writeln!(std::io::stdout(), "{caps}")?;
        return Ok(());
    }
    // GTK owns this process's main thread. No Tokio runtime is entered here.
    let application = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let backend = Rc::new(RefCell::new(None::<Backend>));
    // Linux SIGINT/SIGTERM are handled by Backend's Tokio runtime.
    let holder = backend.clone();
    application.connect_activate(move |application| {
        if let Some(window) = application.active_window() {
            window.present();
            return;
        }
        let hint = gtk::gdk::Display::default().map(|display| {
            let name = display.type_().name();
            if name.contains("Wayland") {
                SessionType::Wayland
            } else if name.contains("X11") {
                SessionType::X11
            } else {
                SessionType::Unknown
            }
        });
        match Backend::spawn(dummy, DesktopEnvironment::detect(hint)) {
            Ok(worker) => {
                ui::build(
                    application,
                    worker.commands.clone(),
                    worker.events.clone(),
                    dummy,
                );
                *holder.borrow_mut() = Some(worker);
            }
            Err(error) => {
                tracing::error!(target: "wireless_display::ui", %error, "Backend unavailable");
                application.quit();
            }
        }
    });
    let exit = application.run_with_args(&["wireless-display"]);
    if let Some(worker) = backend.borrow_mut().take() {
        worker.join();
    }
    if exit != gtk::glib::ExitCode::SUCCESS {
        return Err("GTK exited with an error".into());
    }
    Ok(())
}
