use crate::mirroring;
use async_channel::{Receiver, Sender};
use futures_util::StreamExt;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use wireless_display_core::dummy::{MockCaptureBackend, MockDeviceDiscovery, MockP2pConnector};
use wireless_display_core::traits::{CaptureBackend, CaptureLease, DeviceDiscovery, P2pConnector};
use wireless_display_core::{
    AppCommand, AppEvent, CaptureInfo, CaptureRequest, CaptureState, DeviceId, DiscoveryEvent,
    MediaEvent, PeerType, SessionMachine, SessionState, SystemCapabilities, WirelessDisplayDevice,
    protocol,
};
use wireless_display_media::GstMediaPipeline;
use wireless_display_platform::capture::{self, SelectedCapture};
use wireless_display_platform::environment::DesktopEnvironment;
use wireless_display_platform::network::NetworkManagerService;

pub struct Backend {
    pub commands: mpsc::Sender<AppCommand>,
    pub events: Receiver<AppEvent>,
    cancel: CancellationToken,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Backend {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn spawn(dummy: bool, environment: DesktopEnvironment) -> std::io::Result<Self> {
        let (commands, receiver) = mpsc::channel(32);
        let (sender, events) = async_channel::bounded(256);
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let thread = std::thread::Builder::new()
            .name("wireless-display-backend".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(async {
                        let coordinator =
                            Coordinator::new(dummy, environment, receiver, sender, token.clone());
                        let running = coordinator.run();
                        tokio::pin!(running);
                        tokio::select! {
                            _ = &mut running => {},
                            _ = shutdown_signal() => { token.cancel(); running.await; },
                        }
                    }),
                    Err(error) => {
                        let _ = sender
                            .send_blocking(AppEvent::Error(format!("Tokio runtime: {error}")));
                    }
                }
            })?;
        Ok(Self {
            commands,
            events,
            cancel,
            thread: Some(thread),
        })
    }

    /// Called after GTK's main loop exits, never while updating widgets.
    pub fn join(mut self) {
        self.cancel.cancel();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!(target: "wireless_display::ui", "Backend terminated with a panic");
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{self, SignalKind};
    match (
        unix::signal(SignalKind::interrupt()),
        unix::signal(SignalKind::terminate()),
    ) {
        (Ok(mut interrupt), Ok(mut terminate)) => {
            tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
        }
        _ => {
            tracing::warn!(target: "wireless_display::ui", "Could not register SIGINT/SIGTERM handlers");
            std::future::pending::<()>().await;
        }
    }
}

struct Job {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}
impl Job {
    async fn stop(mut self) {
        self.cancel.cancel();
        if tokio::time::timeout(Duration::from_secs(20), &mut self.task)
            .await
            .is_err()
        {
            tracing::warn!(target: "wireless_display::ui", "Cleanup timed out; aborting task and releasing RAII resources");
            self.task.abort();
            let _ = self.task.await;
        }
    }
}

enum Internal {
    Discovery(u64, DiscoveryEvent),
    ConnectionEnded(u64, Result<(), String>),
    Mirror(u64, mirroring::Event),
    CaptureSelected(u64, CaptureInfo),
    CaptureRunning(u64, CaptureInfo),
    Media(u64, MediaEvent),
    CaptureEnded(u64, Result<(), String>),
    Diagnostics(Box<SystemCapabilities>),
}

async fn notify(tx: &Sender<Internal>, event: Internal, cancel: &CancellationToken) -> bool {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => false,
        sent = tx.send(event) => sent.is_ok(),
    }
}

pub struct Coordinator {
    dummy: bool,
    environment: DesktopEnvironment,
    commands: mpsc::Receiver<AppCommand>,
    events: Sender<AppEvent>,
    cancel: CancellationToken,
    machine: SessionMachine,
    devices: BTreeMap<DeviceId, WirelessDisplayDevice>,
    internal_tx: Sender<Internal>,
    internal_rx: Receiver<Internal>,
    discovery: Option<Job>,
    connection: Option<Job>,
    capture: Option<Job>,
    diagnostics: Option<Job>,
    discovery_epoch: u64,
    connection_epoch: u64,
    capture_epoch: u64,
}

impl Coordinator {
    pub fn new(
        dummy: bool,
        environment: DesktopEnvironment,
        commands: mpsc::Receiver<AppCommand>,
        events: Sender<AppEvent>,
        cancel: CancellationToken,
    ) -> Self {
        let (internal_tx, internal_rx) = async_channel::bounded(256);
        Self {
            dummy,
            environment,
            commands,
            events,
            cancel,
            machine: SessionMachine::default(),
            devices: BTreeMap::new(),
            internal_tx,
            internal_rx,
            discovery: None,
            connection: None,
            capture: None,
            diagnostics: None,
            discovery_epoch: 0,
            connection_epoch: 0,
            capture_epoch: 0,
        }
    }

    async fn emit(&self, event: AppEvent) {
        match &event {
            AppEvent::Error(error) => {
                tracing::error!(target: "wireless_display::backend", %error, "Operation failed");
            }
            AppEvent::Warning(warning) | AppEvent::Media(MediaEvent::Warning(warning)) => {
                tracing::warn!(target: "wireless_display::backend", %warning);
            }
            _ => {}
        }
        tokio::select! {
            biased;
            _ = self.events.send(event) => {},
            _ = self.cancel.cancelled() => {},
        }
    }
    async fn publish(&self) {
        self.emit(AppEvent::StateChanged(self.machine.state().clone()))
            .await;
    }
    async fn transition(&mut self, next: SessionState) {
        match self.machine.transition(next) {
            Ok(()) => self.publish().await,
            Err(error) => self.emit(AppEvent::Error(error.to_string())).await,
        }
    }

    pub async fn run(mut self) {
        self.publish().await;
        self.diagnose();
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                command = self.commands.recv() => match command {
                    None | Some(AppCommand::Shutdown) => break,
                    Some(command) => self.command(command).await,
                },
                event = self.internal_rx.recv() => if let Ok(event) = event { self.internal(event).await; },
            }
        }
        self.stop_discovery().await;
        // Cancel connection establishment before disposing capture; deactivate P2P last.
        self.connection_epoch += 1;
        if let Some(job) = self.connection.take() {
            job.stop().await;
        }
        self.stop_capture().await;
        if let Some(job) = self.diagnostics.take() {
            job.stop().await;
        }
        self.emit(AppEvent::ShutdownComplete).await;
    }

    fn diagnose(&mut self) {
        if self
            .diagnostics
            .as_ref()
            .is_some_and(|job| !job.task.is_finished())
        {
            return;
        }
        let environment = self.environment.clone();
        let dummy = self.dummy;
        let tx = self.internal_tx.clone();
        let token = self.cancel.child_token();
        let child = token.clone();
        let task = tokio::spawn(async move {
            tokio::select! { _ = child.cancelled() => {}, caps = async {
                if dummy { crate::diagnostics::dummy_capabilities(&environment) }
                else { crate::diagnostics::collect(&environment).await }
            } => { notify(&tx, Internal::Diagnostics(Box::new(caps)), &child).await; } }
        });
        self.diagnostics = Some(Job {
            cancel: token,
            task,
        });
    }

    async fn start_discovery(&mut self) {
        if !matches!(
            self.machine.state().session,
            SessionState::Idle | SessionState::Discovering | SessionState::Error(_)
        ) {
            self.emit(AppEvent::Warning(
                "Disconnect before searching for devices.".into(),
            ))
            .await;
            return;
        }
        self.stop_discovery().await;
        if matches!(self.machine.state().session, SessionState::Error(_)) {
            self.transition(SessionState::Idle).await;
        }
        self.transition(SessionState::Discovering).await;
        self.machine.set_discovering(true);
        self.publish().await;
        self.discovery_epoch += 1;
        let epoch = self.discovery_epoch;
        let dummy = self.dummy;
        let tx = self.internal_tx.clone();
        let token = self.cancel.child_token();
        let child = token.clone();
        let task = tokio::spawn(async move {
            let (events, rx) = async_channel::bounded(128);
            let work = async {
                if dummy {
                    MockDeviceDiscovery
                        .discover(events, child.clone())
                        .await
                        .map_err(|e| e.to_string())
                } else {
                    NetworkManagerService::try_new()
                        .await
                        .map_err(|e| e.to_string())?
                        .discover(events, child.clone())
                        .await
                        .map_err(|e| e.to_string())
                }
            };
            tokio::pin!(work);
            let result = loop {
                tokio::select! {
                    result = &mut work => break result,
                    _ = child.cancelled() => break work.await,
                    event = rx.recv() => match event { Ok(event) => { if !notify(&tx, Internal::Discovery(epoch, event), &child).await { child.cancel(); } }, Err(_) => { break work.await; } }
                }
            };
            if let Err(error) = result {
                notify(
                    &tx,
                    Internal::Discovery(epoch, DiscoveryEvent::Warning(error)),
                    &child,
                )
                .await;
            }
            notify(
                &tx,
                Internal::Discovery(epoch, DiscoveryEvent::Stopped),
                &child,
            )
            .await;
        });
        self.discovery = Some(Job {
            cancel: token,
            task,
        });
        self.emit(AppEvent::DiscoveryStarted).await;
    }

    async fn clear_devices(&mut self) {
        for id in std::mem::take(&mut self.devices).into_keys() {
            self.emit(AppEvent::DeviceRemoved(id)).await;
        }
    }

    async fn stop_discovery(&mut self) {
        self.discovery_epoch += 1;
        if let Some(job) = self.discovery.take() {
            job.stop().await;
        }
        // PeerRemoved is no longer observed after this job ends. Do not keep
        // presenting an unmonitored snapshot as available devices.
        self.clear_devices().await;
        if self.machine.state().discovering {
            self.machine.set_discovering(false);
            if self.machine.state().session == SessionState::Discovering {
                self.transition(SessionState::Idle).await;
            }
            self.publish().await;
            self.emit(AppEvent::DiscoveryStopped).await;
        }
    }

    async fn connect(&mut self, id: DeviceId, preferred: protocol::wfd::video::VideoMode) {
        if !matches!(
            self.machine.state().session,
            SessionState::Idle | SessionState::Discovering
        ) {
            self.emit(AppEvent::Warning(
                "A session already exists. Disconnect first.".into(),
            ))
            .await;
            return;
        }
        let Some(peer) = self.devices.get(&id).cloned() else {
            self.emit(AppEvent::Error("Device is no longer available.".into()))
                .await;
            return;
        };
        if peer.peer_type != PeerType::WirelessDisplayReceiver {
            self.emit(AppEvent::Warning(
                "P2P peer does not advertise a Wireless Display receiver.".into(),
            ))
            .await;
            return;
        }
        if peer
            .wfd
            .as_ref()
            .is_some_and(|caps| !caps.session_available)
        {
            self.emit(AppEvent::Warning(
                "Receiver is not available for a session.".into(),
            ))
            .await;
            return;
        }
        self.stop_discovery().await;
        self.transition(SessionState::DeviceSelected(id.clone()))
            .await;
        self.transition(SessionState::Connecting(id)).await;
        self.connection_epoch += 1;
        let epoch = self.connection_epoch;
        let dummy = self.dummy;
        let tx = self.internal_tx.clone();
        let token = self.cancel.child_token();
        let child = token.clone();
        let environment = self.environment.clone();
        if !dummy {
            self.stop_capture().await;
            self.machine.set_capture(CaptureState::Selecting);
            self.publish().await;
        }
        let task = tokio::spawn(async move {
            let (events, receive) = async_channel::bounded(32);
            let work = async {
                if dummy {
                    MockP2pConnector
                        .connect(&peer, child.clone())
                        .await
                        .map_err(|e| e.to_string())?;
                    events
                        .send(mirroring::Event::Connected)
                        .await
                        .map_err(|e| e.to_string())?;
                    child.cancelled().await;
                    Ok(())
                } else {
                    mirroring::run(peer, environment, preferred, events, child.clone()).await
                }
            };
            tokio::pin!(work);
            let result = loop {
                tokio::select! {
                    result = &mut work => break result,
                    event = receive.recv() => match event {
                        Ok(event) => { if !notify(&tx, Internal::Mirror(epoch,event), &child).await { child.cancel(); } },
                        Err(_) => break work.await,
                    }
                }
            };
            while let Ok(event) = receive.try_recv() {
                notify(&tx, Internal::Mirror(epoch, event), &child).await;
            }
            notify(&tx, Internal::ConnectionEnded(epoch, result), &child).await;
        });
        self.connection = Some(Job {
            cancel: token,
            task,
        });
    }

    async fn disconnect(&mut self) {
        self.connection_epoch += 1;
        if matches!(
            self.machine.state().session,
            SessionState::DeviceSelected(_)
                | SessionState::Connecting(_)
                | SessionState::Negotiating(_)
                | SessionState::Streaming { .. }
                | SessionState::Paused(_)
        ) {
            self.transition(SessionState::Disconnecting).await;
        }
        if let Some(job) = self.connection.take() {
            job.stop().await;
        }
        self.stop_capture().await;
        if matches!(
            self.machine.state().session,
            SessionState::Disconnecting | SessionState::Error(_)
        ) {
            self.transition(SessionState::Idle).await;
        }
    }

    async fn start_capture(&mut self, request: CaptureRequest) {
        if !self.dummy && self.connection.is_some() {
            self.emit(AppEvent::Warning(
                "Screen sharing already has an active capture.".into(),
            ))
            .await;
            return;
        }
        if self.capture.is_some() {
            self.emit(AppEvent::Warning(
                "Capture has already started or is waiting for selection.".into(),
            ))
            .await;
            return;
        }
        self.capture_epoch += 1;
        let epoch = self.capture_epoch;
        let dummy = self.dummy;
        let environment = self.environment.clone();
        let tx = self.internal_tx.clone();
        let token = self.cancel.child_token();
        let child = token.clone();
        self.machine.set_capture(CaptureState::Selecting);
        self.publish().await;
        let task = tokio::spawn(async move {
            let result = capture_worker(
                dummy,
                environment,
                request,
                child.clone(),
                tx.clone(),
                epoch,
            )
            .await;
            notify(&tx, Internal::CaptureEnded(epoch, result), &child).await;
        });
        self.capture = Some(Job {
            cancel: token,
            task,
        });
    }

    async fn stop_capture(&mut self) {
        self.capture_epoch += 1;
        if let Some(job) = self.capture.take() {
            job.stop().await;
        }
        self.machine.set_capture(CaptureState::Stopped);
        self.publish().await;
        self.emit(AppEvent::MediaPipelineStopped).await;
        self.emit(AppEvent::CaptureStopped).await;
    }

    async fn command(&mut self, command: AppCommand) {
        match command {
            AppCommand::StartDiscovery | AppCommand::RefreshDevices => self.start_discovery().await,
            AppCommand::StopDiscovery => self.stop_discovery().await,
            AppCommand::Connect(id, preferred) => self.connect(id, preferred).await,
            AppCommand::Disconnect => self.disconnect().await,
            AppCommand::StartCapture(request) => self.start_capture(request).await,
            AppCommand::StopCapture if !self.dummy && self.connection.is_some() => {
                self.disconnect().await
            }
            AppCommand::StopCapture => self.stop_capture().await,
            AppCommand::RunDiagnostics => self.diagnose(),
            AppCommand::SimulateError if self.dummy => {
                self.stop_discovery().await;
                self.disconnect().await;
                self.transition(SessionState::Error(
                    "Simulated error; disconnect to return to the initial state.".into(),
                ))
                .await;
            }
            AppCommand::SimulateError | AppCommand::Shutdown => {}
        }
    }

    async fn internal(&mut self, event: Internal) {
        match event {
            Internal::Discovery(epoch, event) if epoch == self.discovery_epoch => match event {
                DiscoveryEvent::Upsert(peer) => {
                    if self.devices.len() >= 256 && !self.devices.contains_key(&peer.id) {
                        self.emit(AppEvent::Warning(
                            "The limit of 256 peers has been reached for this search.".into(),
                        ))
                        .await;
                        return;
                    }
                    let old = self.devices.insert(peer.id.clone(), peer.clone());
                    self.emit(if old.is_some() {
                        AppEvent::DeviceUpdated(peer)
                    } else {
                        AppEvent::DeviceAdded(peer)
                    })
                    .await;
                }
                DiscoveryEvent::Removed(id) => {
                    self.devices.remove(&id);
                    self.emit(AppEvent::DeviceRemoved(id)).await;
                }
                DiscoveryEvent::Warning(message) => self.emit(AppEvent::Warning(message)).await,
                DiscoveryEvent::Stopped => {
                    self.clear_devices().await;
                    self.machine.set_discovering(false);
                    if self.machine.state().session == SessionState::Discovering {
                        self.transition(SessionState::Idle).await;
                    }
                    self.publish().await;
                    self.emit(AppEvent::DiscoveryStopped).await;
                }
            },
            Internal::Mirror(epoch, event) if epoch == self.connection_epoch => match event {
                mirroring::Event::SourceSelected(info) => {
                    self.machine.set_capture(CaptureState::Ready);
                    self.publish().await;
                    self.emit(AppEvent::CaptureSourceSelected(info)).await;
                }
                mirroring::Event::Connected => {
                    if let SessionState::Connecting(id) = self.machine.state().session.clone() {
                        self.transition(SessionState::Negotiating(id.clone())).await;
                        if self.dummy {
                            self.transition(SessionState::Streaming {
                                device: id,
                                simulated: true,
                            })
                            .await;
                        }
                    }
                }
                mirroring::Event::Negotiated(info) => {
                    self.emit(AppEvent::StreamNegotiated(info)).await
                }
                mirroring::Event::Playing(info) => {
                    if let SessionState::Negotiating(id) | SessionState::Paused(id) =
                        self.machine.state().session.clone()
                    {
                        self.transition(SessionState::Streaming {
                            device: id,
                            simulated: false,
                        })
                        .await;
                    }
                    self.machine.set_capture(CaptureState::Running);
                    self.publish().await;
                    self.emit(AppEvent::CaptureStarted(info)).await;
                    self.emit(AppEvent::MediaPipelineStarted).await;
                }
                mirroring::Event::Paused => {
                    if let SessionState::Streaming { device, .. }
                    | SessionState::Negotiating(device) = self.machine.state().session.clone()
                    {
                        self.transition(SessionState::Paused(device)).await;
                    }
                    self.machine.set_capture(CaptureState::Ready);
                    self.publish().await;
                }
                mirroring::Event::Media(event) => self.emit(AppEvent::Media(event)).await,
            },
            Internal::ConnectionEnded(epoch, result) if epoch == self.connection_epoch => {
                if let Some(job) = self.connection.take() {
                    job.stop().await;
                }
                self.machine.set_capture(CaptureState::Stopped);
                self.publish().await;
                self.emit(AppEvent::CaptureStopped).await;
                self.emit(AppEvent::MediaPipelineStopped).await;
                match result {
                    Err(error) => {
                        self.transition(SessionState::Error(error.clone())).await;
                        self.emit(AppEvent::Error(error)).await;
                    }
                    Ok(()) => {
                        self.transition(SessionState::Disconnecting).await;
                        self.transition(SessionState::Idle).await;
                    }
                }
            }
            Internal::CaptureSelected(epoch, info) if epoch == self.capture_epoch => {
                self.emit(AppEvent::CaptureSourceSelected(info)).await
            }
            Internal::CaptureRunning(epoch, info) if epoch == self.capture_epoch => {
                self.machine.set_capture(CaptureState::Running);
                self.publish().await;
                self.emit(AppEvent::CaptureStarted(info)).await;
            }
            Internal::Media(epoch, event) if epoch == self.capture_epoch => {
                if matches!(event, MediaEvent::StateChanged { playing: true }) {
                    self.emit(AppEvent::MediaPipelineStarted).await;
                }
                self.emit(AppEvent::Media(event)).await;
            }
            Internal::CaptureEnded(epoch, result) if epoch == self.capture_epoch => {
                if let Some(job) = self.capture.take() {
                    job.stop().await;
                }
                self.machine.set_capture(CaptureState::Stopped);
                self.publish().await;
                self.emit(AppEvent::MediaPipelineStopped).await;
                self.emit(AppEvent::CaptureStopped).await;
                if let Err(error) = result {
                    self.emit(AppEvent::Error(error)).await;
                }
            }
            Internal::Diagnostics(caps) => self.emit(AppEvent::DiagnosticResult(caps)).await,
            _ => {}
        }
    }
}

async fn capture_worker(
    dummy: bool,
    environment: DesktopEnvironment,
    request: CaptureRequest,
    cancel: CancellationToken,
    tx: Sender<Internal>,
    epoch: u64,
) -> Result<(), String> {
    let lease: Option<SelectedCapture> = if dummy {
        None
    } else {
        Some(
            capture::select_capture(&environment, request, cancel.clone())
                .await
                .map_err(|e| e.to_string())?,
        )
    };
    let (source, info) = if let Some(lease) = &lease {
        (lease.source(), lease.info())
    } else {
        let lease = MockCaptureBackend
            .capture(request, cancel.clone())
            .await
            .map_err(|e| e.to_string())?;
        (lease.source(), lease.info())
    };
    let session = lease.as_ref().and_then(SelectedCapture::portal_session);
    let result = async {
        if !notify(&tx, Internal::CaptureSelected(epoch, info.clone()), &cancel).await { return Ok(()); }
        let pipeline = GstMediaPipeline::try_new(source).map_err(|e| e.to_string())?;
        let (events, receiver) = async_channel::bounded(32);
        let media_token = cancel.child_token();
        let future = pipeline.run(events, media_token.clone());
        tokio::pin!(future);
        let closed = async {
            if let Some(session) = session.as_ref() {
                let mut stream = session.receive_closed().await.map_err(|e| e.to_string())?;
                stream.next().await;
                Err::<(), String>("The portal session was closed by the desktop or the user.".into())
            } else { std::future::pending().await }
        };
        tokio::pin!(closed);
        let mut running = false;
        loop {
            tokio::select! {
                result = &mut future => break result.map_err(|e| e.to_string()),
                result = &mut closed => { media_token.cancel(); let _ = (&mut future).await; break result; },
                event = receiver.recv() => {
                    match event {
                        Ok(event) => {
                            if !running && matches!(event, MediaEvent::Frames(frames) if frames > 0) {
                                running = true; notify(&tx, Internal::CaptureRunning(epoch, info.clone()), &cancel).await;
                            }
                            notify(&tx, Internal::Media(epoch, event), &cancel).await;
                        },
                        Err(_) => { break (&mut future).await.map_err(|e| e.to_string()); }
                    }
                }
            }
        }
    }.await;
    // The pipeline is dropped/NULL before closing the portal and remote FD.
    drop(session);
    if let Some(lease) = lease
        && let Err(error) = lease.close().await
        && result.is_ok()
    {
        return Err(error.to_string());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use wireless_display_core::{AppState, dummy};
    #[tokio::test]
    async fn discovery_end_invalidates_unmonitored_receivers() {
        let (_tx, rx) = mpsc::channel(32);
        let (events, received) = async_channel::bounded(32);
        let mut coordinator = Coordinator::new(
            true,
            DesktopEnvironment::detect(None),
            rx,
            events,
            CancellationToken::new(),
        );
        let peer = dummy::receiver();
        coordinator.devices.insert(peer.id.clone(), peer.clone());
        coordinator
            .machine
            .transition(SessionState::Discovering)
            .unwrap();
        coordinator.machine.set_discovering(true);
        coordinator
            .internal(Internal::Discovery(0, DiscoveryEvent::Stopped))
            .await;
        assert!(coordinator.devices.is_empty());
        assert_eq!(coordinator.machine.state().session, SessionState::Idle);
        assert!(!coordinator.machine.state().discovering);
        assert!(
            matches!(received.recv().await.unwrap(), AppEvent::DeviceRemoved(id) if id == peer.id)
        );
    }
    #[tokio::test]
    async fn shutdown_completes_with_saturated_ui_channel() {
        let (tx, rx) = mpsc::channel(32);
        let (events, _received) = async_channel::bounded(1);
        let token = CancellationToken::new();
        let task = tokio::spawn(
            Coordinator::new(
                true,
                DesktopEnvironment::detect(None),
                rx,
                events,
                token.clone(),
            )
            .run(),
        );
        tx.send(AppCommand::StartDiscovery).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        token.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }
    async fn event_until(
        events: &Receiver<AppEvent>,
        predicate: impl Fn(&AppEvent) -> bool,
    ) -> AppEvent {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let event = events.recv().await.unwrap();
                if predicate(&event) {
                    return event;
                }
            }
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn dummy_connect_disconnect_error_and_shutdown() {
        let (tx, rx) = mpsc::channel(32);
        let (events, received) = async_channel::bounded(256);
        let coordinator = Coordinator::new(
            true,
            DesktopEnvironment::detect(None),
            rx,
            events,
            CancellationToken::new(),
        );
        let task = tokio::spawn(coordinator.run());
        tx.send(AppCommand::StartDiscovery).await.unwrap();
        let AppEvent::DeviceAdded(peer) =
            event_until(&received, |e| matches!(e, AppEvent::DeviceAdded(_))).await
        else {
            unreachable!()
        };
        tx.send(AppCommand::Connect(peer.id, Default::default()))
            .await
            .unwrap();
        event_until(&received, |e| {
            matches!(
                e,
                AppEvent::StateChanged(AppState {
                    session: SessionState::Streaming {
                        simulated: true,
                        ..
                    },
                    ..
                })
            )
        })
        .await;
        tx.send(AppCommand::Disconnect).await.unwrap();
        event_until(&received, |e| {
            matches!(
                e,
                AppEvent::StateChanged(AppState {
                    session: SessionState::Idle,
                    ..
                })
            )
        })
        .await;
        tx.send(AppCommand::SimulateError).await.unwrap();
        event_until(&received, |e| {
            matches!(
                e,
                AppEvent::StateChanged(AppState {
                    session: SessionState::Error(_),
                    ..
                })
            )
        })
        .await;
        tx.send(AppCommand::Shutdown).await.unwrap();
        event_until(&received, |e| matches!(e, AppEvent::ShutdownComplete)).await;
        task.await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_connect_never_streams_after_disconnect() {
        let (tx, rx) = mpsc::channel(32);
        let (events, received) = async_channel::bounded(256);
        let task = tokio::spawn(
            Coordinator::new(
                true,
                DesktopEnvironment::detect(None),
                rx,
                events,
                CancellationToken::new(),
            )
            .run(),
        );
        tx.send(AppCommand::StartDiscovery).await.unwrap();
        let AppEvent::DeviceAdded(peer) =
            event_until(&received, |e| matches!(e, AppEvent::DeviceAdded(_))).await
        else {
            unreachable!()
        };
        tx.send(AppCommand::Connect(peer.id, Default::default()))
            .await
            .unwrap();
        tx.send(AppCommand::Disconnect).await.unwrap();
        tx.send(AppCommand::Shutdown).await.unwrap();
        loop {
            let event = received.recv().await.unwrap();
            assert!(!matches!(
                event,
                AppEvent::StateChanged(AppState {
                    session: SessionState::Streaming { .. },
                    ..
                })
            ));
            if matches!(event, AppEvent::ShutdownComplete) {
                break;
            }
        }
        task.await.unwrap();
    }
}
