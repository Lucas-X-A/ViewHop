use async_channel::Sender;
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use wireless_display_core::protocol::wfd::SOURCE_WFD_IES;
use wireless_display_core::protocol::wfd::video::P2pNetwork;
use wireless_display_core::traits::{DeviceDiscovery, P2pConnector};
use wireless_display_core::{
    DeviceId, DiscoveryEvent, ModelError, PeerType, WfdCapability, WirelessDisplayDevice,
};
use zbus::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Str, Value};

const NM: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
const DBUS_TIMEOUT: Duration = Duration::from_secs(8);

type Settings = HashMap<String, HashMap<String, OwnedValue>>;

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait Manager {
    fn get_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;
    fn add_and_activate_connection2(
        &self,
        connection: Settings,
        device: OwnedObjectPath,
        specific_object: OwnedObjectPath,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::Result<(
        OwnedObjectPath,
        OwnedObjectPath,
        HashMap<String, OwnedValue>,
    )>;
    fn deactivate_connection(&self, active_connection: OwnedObjectPath) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Device",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Device {
    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn state_reason(&self) -> zbus::Result<(u32, u32)>;
    #[zbus(signal)]
    fn state_changed(&self, new_state: u32, old_state: u32, reason: u32) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Device.WifiP2P",
    default_service = "org.freedesktop.NetworkManager"
)]
trait WifiP2p {
    fn start_find(&self, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    fn stop_find(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn peers(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    #[zbus(signal)]
    fn peer_added(&self, peer: OwnedObjectPath) -> zbus::Result<()>;
    #[zbus(signal)]
    fn peer_removed(&self, peer: OwnedObjectPath) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.WifiP2PPeer",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Peer {
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn hw_address(&self) -> zbus::Result<String>;
    #[zbus(property, name = "WfdIEs")]
    fn wfd_ies(&self) -> zbus::Result<Vec<u8>>;
    #[zbus(property)]
    fn strength(&self) -> zbus::Result<u8>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Active {
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn ip4_config(&self) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.IP4Config",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Ip4Config {
    #[zbus(property)]
    fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
    #[zbus(property)]
    fn gateway(&self) -> zbus::Result<String>;
}

#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("NetworkManager/D-Bus: {0}")]
    Dbus(#[from] zbus::Error),
    #[error("Invalid D-Bus value: {0}")]
    Value(#[from] zbus::zvariant::Error),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("No Wi-Fi Direct interface detected")]
    NoP2p,
    #[error("NetworkManager operation timed out")]
    Timeout,
    #[error("Wi-Fi Direct operation cancelled")]
    Cancelled,
    #[error("Wi-Fi Direct activation failed (state {0})")]
    Activation(u32),
    #[error(
        "The TV is no longer visible on Wi-Fi Direct (peer-not-found). Keep Screen Share open and click Search for devices again."
    )]
    PeerUnavailable,
    #[error(
        "The device is not available as a Wireless Display receiver. Enable Screen Share and search again."
    )]
    ReceiverUnavailable,
    #[error("Wi-Fi Direct activation failed (NetworkManager reason {0})")]
    DeviceActivation(u32),
    #[error("The Wi-Fi Direct connection's D-Bus monitor closed")]
    MonitorClosed,
    #[error("NetworkManager did not provide a valid IPv4 address for the P2P group")]
    AddressUnavailable,
    #[error(
        "NetworkManager activated Wi-Fi Direct but did not publish the group IPv4 address before the deadline"
    )]
    AddressTimeout,
    #[error("The Wi-Fi Direct connection closed (state {0})")]
    ConnectionLost(u32),
    #[error("The Wi-Fi Direct group lost its IPv4 configuration; the link may have disconnected")]
    AddressLost,
    #[error("The Wi-Fi Direct connection address changed during sharing")]
    AddressChanged,
}

fn device_activation_error(reason: u32) -> NetworkError {
    // NMDeviceStateReason, as defined by NetworkManager's public D-Bus API.
    match reason {
        67 => NetworkError::PeerUnavailable,
        other => NetworkError::DeviceActivation(other),
    }
}

fn validate_current_peer(
    selected: &WirelessDisplayDevice,
    current: &WirelessDisplayDevice,
) -> Result<(), NetworkError> {
    if selected.id != current.id
        || selected.mac != current.mac
        || selected.adapter_id != current.adapter_id
    {
        return Err(NetworkError::PeerUnavailable);
    }
    if current.peer_type != PeerType::WirelessDisplayReceiver
        || !current
            .wfd
            .as_ref()
            .is_some_and(|wfd| wfd.session_available)
    {
        return Err(NetworkError::ReceiverUnavailable);
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct NetworkCapabilities {
    pub version: String,
    pub wifi: Vec<String>,
    pub p2p: Vec<String>,
}

#[derive(Clone)]
pub struct NetworkManagerService {
    bus: Connection,
}

async fn bounded<T>(
    future: impl std::future::Future<Output = Result<T, NetworkError>>,
) -> Result<T, NetworkError> {
    tokio::time::timeout(DBUS_TIMEOUT, future)
        .await
        .map_err(|_| NetworkError::Timeout)?
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkDeviceType {
    Wifi,
    WifiP2p,
    Other(u32),
}
impl From<u32> for NetworkDeviceType {
    fn from(value: u32) -> Self {
        match value {
            2 => Self::Wifi,
            30 => Self::WifiP2p,
            other => Self::Other(other),
        }
    }
}

impl NetworkManagerService {
    /// Compatibility alias for [`Self::try_new`].
    pub async fn new() -> Result<Self, NetworkError> {
        Self::try_new().await
    }

    /// Opens the system D-Bus connection within the platform's timeout.
    ///
    /// Does not start discovery or activate a network connection.
    pub async fn try_new() -> Result<Self, NetworkError> {
        bounded(async {
            Ok(Self {
                bus: Connection::system().await?,
            })
        })
        .await
    }

    async fn adapters(
        &self,
    ) -> Result<Vec<(OwnedObjectPath, String, NetworkDeviceType)>, NetworkError> {
        bounded(async {
            let manager = ManagerProxy::new(&self.bus).await?;
            let mut devices = Vec::new();
            for path in manager.get_devices().await? {
                let device = DeviceProxy::builder(&self.bus)
                    .path(path.clone())?
                    .build()
                    .await?;
                devices.push((
                    path,
                    device.interface().await?,
                    device.device_type().await?.into(),
                ));
            }
            Ok(devices)
        })
        .await
    }

    pub async fn capabilities(&self) -> Result<NetworkCapabilities, NetworkError> {
        let devices = self.adapters().await?;
        bounded(async {
            let mut caps = NetworkCapabilities {
                version: ManagerProxy::new(&self.bus).await?.version().await?,
                ..Default::default()
            };
            for (_, name, kind) in devices {
                match kind {
                    NetworkDeviceType::Wifi => caps.wifi.push(name),
                    NetworkDeviceType::WifiP2p => caps.p2p.push(name),
                    _ => {}
                }
            }
            Ok(caps)
        })
        .await
    }

    async fn peer(
        &self,
        path: &OwnedObjectPath,
        adapter: &OwnedObjectPath,
    ) -> Result<WirelessDisplayDevice, NetworkError> {
        bounded(async {
            let proxy = PeerProxy::builder(&self.bus)
                .path(path.clone())?
                .build()
                .await?;
            let name = proxy.name().await?;
            let mac = proxy.hw_address().await?.parse()?;
            let ies = proxy.wfd_ies().await?;
            // Invalid IEs never elevate an unknown peer to a receiver.
            let wfd = match WfdCapability::parse(&ies) {
                Ok(wfd) => wfd,
                Err(_) => {
                    tracing::warn!(target: "wireless_display::network", "Peer with invalid WFD IE");
                    None
                }
            };
            let mut device = WirelessDisplayDevice::try_new(
                DeviceId::try_new(path.as_str())?,
                &name,
                mac,
                wfd,
                adapter.to_string(),
            )?;
            device.signal_percent = Some(proxy.strength().await?.min(100));
            Ok(device)
        })
        .await
    }

    async fn current_peer(
        &self,
        selected: &WirelessDisplayDevice,
    ) -> Result<WirelessDisplayDevice, NetworkError> {
        let adapter = OwnedObjectPath::try_from(selected.adapter_id.as_str())?;
        let path = OwnedObjectPath::try_from(selected.id.as_str())?;
        bounded(async {
            let proxy = WifiP2pProxy::builder(&self.bus)
                .path(adapter.clone())?
                .build()
                .await?;
            if !proxy.peers().await?.contains(&path) {
                return Err(NetworkError::PeerUnavailable);
            }
            let current = match self.peer(&path, &adapter).await {
                // A peer can expire between reading Peers and its properties.
                Err(NetworkError::Dbus(zbus::Error::MethodError(name, _, _)))
                    if matches!(
                        name.as_str(),
                        "org.freedesktop.DBus.Error.UnknownObject"
                            | "org.freedesktop.DBus.Error.UnknownInterface"
                    ) =>
                {
                    return Err(NetworkError::PeerUnavailable);
                }
                result => result?,
            };
            validate_current_peer(selected, &current)?;
            Ok(current)
        })
        .await
    }

    async fn watch_adapter(
        &self,
        path: OwnedObjectPath,
        events: Sender<DiscoveryEvent>,
        cancel: CancellationToken,
    ) -> Result<(), NetworkError> {
        let proxy = WifiP2pProxy::builder(&self.bus)
            .path(path.clone())?
            .build()
            .await?;
        // Subscribe before listing peers or starting discovery to avoid lost additions.
        let mut added = proxy.receive_peer_added().await?;
        let mut removed = proxy.receive_peer_removed().await?;
        // WfdIEs and availability can arrive after PeerAdded, or change while
        // Screen Share is opened/closed. Bind signals to NM's unique owner and
        // only accept paths belonging to this adapter's current peer list.
        let mut updates = bounded(async {
            let owner = zbus::fdo::DBusProxy::new(&self.bus)
                .await?
                .get_name_owner(proxy.inner().destination().clone())
                .await
                .map_err(zbus::Error::from)?;
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(owner.as_str())?
                .interface("org.freedesktop.DBus.Properties")?
                .member("PropertiesChanged")?
                .add_arg("org.freedesktop.NetworkManager.WifiP2PPeer")?
                .build();
            Ok(zbus::MessageStream::for_match_rule(rule, &self.bus, Some(64)).await?)
        })
        .await?;
        bounded(async {
            proxy
                .start_find(HashMap::from([(
                    "timeout".into(),
                    OwnedValue::from(120i32),
                )]))
                .await?;
            Ok(())
        })
        .await?;
        let work = async {
            let mut peers = HashSet::new();
            for peer in bounded(async { Ok(proxy.peers().await?) }).await? {
                peers.insert(peer.clone());
                match self.peer(&peer, &path).await {
                    Ok(peer) => {
                        if events.send(DiscoveryEvent::Upsert(peer)).await.is_err() {
                            return Ok(());
                        }
                    }
                    Err(error) => {
                        let _ = events
                            .send(DiscoveryEvent::Warning(error.to_string()))
                            .await;
                    }
                }
            }
            loop {
                let peer = tokio::select! {
                    item = added.next() => {
                        let Some(item) = item else { return Ok(()); };
                        let peer = item.args()?.peer;
                        peers.insert(peer.clone());
                        peer
                    }
                    item = removed.next() => {
                        let Some(item) = item else { return Ok(()); };
                        let peer = item.args()?.peer;
                        peers.remove(&peer);
                        let id = DeviceId::try_new(peer.as_str())?;
                        if events.send(DiscoveryEvent::Removed(id)).await.is_err() { return Ok(()); }
                        continue;
                    }
                    item = updates.next() => {
                        let message = item.ok_or(NetworkError::MonitorClosed)??;
                        let Some(peer) = message.header().path().map(|path| OwnedObjectPath::from(path.to_owned())) else { continue; };
                        if !peers.contains(&peer) { continue; }
                        peer
                    }
                };
                match self.peer(&peer, &path).await {
                    Ok(peer) => {
                        if events.send(DiscoveryEvent::Upsert(peer)).await.is_err() {
                            return Ok(());
                        }
                    }
                    Err(error) => {
                        let _ = events
                            .send(DiscoveryEvent::Warning(error.to_string()))
                            .await;
                    }
                }
            }
        };
        let result = tokio::select! { _ = cancel.cancelled() => Ok(()), result = tokio::time::timeout(Duration::from_secs(120), work) => result.unwrap_or(Ok(())) };
        let stopped = bounded(async {
            proxy.stop_find().await?;
            Ok(())
        })
        .await;
        result.and(stopped)
    }
}

impl DeviceDiscovery for NetworkManagerService {
    type Error = NetworkError;
    async fn discover(
        &self,
        events: Sender<DiscoveryEvent>,
        cancel: CancellationToken,
    ) -> Result<(), NetworkError> {
        let adapters = self.adapters().await?;
        let mut tasks = JoinSet::new();
        for (path, _, kind) in adapters {
            if kind == NetworkDeviceType::WifiP2p {
                let service = self.clone();
                let events = events.clone();
                let token = cancel.child_token();
                tasks.spawn(async move { service.watch_adapter(path, events, token).await });
            }
        }
        if tasks.is_empty() {
            return Err(NetworkError::NoP2p);
        }
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    let _ = events
                        .send(DiscoveryEvent::Warning(error.to_string()))
                        .await;
                }
                Err(error) => {
                    let _ = events
                        .send(DiscoveryEvent::Warning(error.to_string()))
                        .await;
                }
            }
        }
        Ok(())
    }
}

pub struct P2pConnection {
    bus: Connection,
    active: OwnedObjectPath,
}

impl P2pConnection {
    pub async fn network(&self, cancel: &CancellationToken) -> Result<P2pNetwork, NetworkError> {
        let ready = async {
            let active = ActiveProxy::builder(&self.bus)
                .path(self.active.clone())?
                .build()
                .await?;
            // Activation and publication of AddressData are separate D-Bus updates.
            // Subscribe before reading, including when Ip4Config is still '/'.
            let mut state_changes = active.receive_state_changed().await;
            let mut config_changes = active.receive_ip4_config_changed().await;
            loop {
                let state = active.state().await?;
                if state != 2 {
                    return Err(NetworkError::ConnectionLost(state));
                }
                let path = active.ip4_config().await?;
                if path.as_str() == "/" {
                    tokio::select! {
                        change = state_changes.next() => if change.is_none() { return Err(NetworkError::MonitorClosed); },
                        change = config_changes.next() => if change.is_none() { return Err(NetworkError::MonitorClosed); },
                    }
                    continue;
                }
                let config = Ip4ConfigProxy::builder(&self.bus)
                    .path(path.clone())?
                    .build()
                    .await?;
                let mut address_changes = config.receive_address_data_changed().await;
                let mut gateway_changes = config.receive_gateway_changed().await;
                loop {
                    let state = active.state().await?;
                    if state != 2 {
                        return Err(NetworkError::ConnectionLost(state));
                    }
                    if active.ip4_config().await? != path {
                        break;
                    }
                    let addresses = config.address_data().await?;
                    if !addresses.is_empty() {
                        return parse_ip4_config(addresses, &config.gateway().await?);
                    }
                    tokio::select! {
                        change = state_changes.next() => {
                            if change.is_none() { return Err(NetworkError::MonitorClosed); }
                            break;
                        },
                        change = config_changes.next() => {
                            if change.is_none() { return Err(NetworkError::MonitorClosed); }
                            break;
                        },
                        change = address_changes.next() => if change.is_none() { return Err(NetworkError::MonitorClosed); },
                        change = gateway_changes.next() => if change.is_none() { return Err(NetworkError::MonitorClosed); },
                    }
                }
            }
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(NetworkError::Cancelled),
            result = tokio::time::timeout(DBUS_TIMEOUT, ready) => result.map_err(|_| NetworkError::AddressTimeout)?,
        }
    }

    pub async fn monitor(
        &self,
        expected: P2pNetwork,
        cancel: CancellationToken,
    ) -> Result<(), NetworkError> {
        let active = ActiveProxy::builder(&self.bus)
            .path(self.active.clone())?
            .build()
            .await?;
        let mut state_changes = active.receive_state_changed().await;
        let mut config_changes = active.receive_ip4_config_changed().await;
        let path = active.ip4_config().await?;
        let config = Ip4ConfigProxy::builder(&self.bus)
            .path(path.clone())?
            .build()
            .await?;
        let mut address_changes = config.receive_address_data_changed().await;
        let mut gateway_changes = config.receive_gateway_changed().await;
        loop {
            let current = bounded(async {
                let state = active.state().await?;
                if state != 2 {
                    return Err(NetworkError::ConnectionLost(state));
                }
                let current_path = active.ip4_config().await?;
                if current_path.as_str() == "/" {
                    return Err(NetworkError::AddressLost);
                }
                if current_path != path {
                    return Err(NetworkError::AddressChanged);
                }
                parse_ip4_config(config.address_data().await?, &config.gateway().await?)
            })
            .await?;
            if current != expected {
                return Err(NetworkError::AddressChanged);
            }
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                change = state_changes.next() => if change.is_none() { return Err(NetworkError::MonitorClosed); },
                change = config_changes.next() => if change.is_none() { return Err(NetworkError::MonitorClosed); },
                change = address_changes.next() => if change.is_none() { return Err(NetworkError::MonitorClosed); },
                change = gateway_changes.next() => if change.is_none() { return Err(NetworkError::MonitorClosed); },
            }
        }
    }

    pub async fn disconnect(self) -> Result<(), NetworkError> {
        bounded(async {
            ManagerProxy::new(&self.bus)
                .await?
                .deactivate_connection(self.active)
                .await?;
            Ok(())
        })
        .await
    }
}

fn parse_ip4_config(
    addresses: Vec<HashMap<String, OwnedValue>>,
    gateway: &str,
) -> Result<P2pNetwork, NetworkError> {
    if addresses.len() != 1 {
        return Err(NetworkError::AddressUnavailable);
    }
    let address = &addresses[0];
    let local: std::net::Ipv4Addr = address
        .get("address")
        .and_then(|v| <&str>::try_from(v).ok())
        .and_then(|s| s.parse().ok())
        .ok_or(NetworkError::AddressUnavailable)?;
    let prefix = address
        .get("prefix")
        .and_then(|v| u32::try_from(v).ok())
        .filter(|p| (1..=32).contains(p))
        .ok_or(NetworkError::AddressUnavailable)? as u8;
    if local.is_unspecified() || local.is_multicast() || local.is_loopback() || local.is_broadcast()
    {
        return Err(NetworkError::AddressUnavailable);
    }
    let gateway = if gateway.is_empty() {
        None
    } else {
        Some(
            gateway
                .parse()
                .map_err(|_| NetworkError::AddressUnavailable)?,
        )
    };
    let network = P2pNetwork {
        local,
        prefix,
        gateway,
    };
    if let Some(gateway) = gateway {
        let without_gateway = P2pNetwork {
            gateway: None,
            ..network
        };
        if !without_gateway.permits(gateway) {
            return Err(NetworkError::AddressUnavailable);
        }
    }
    Ok(network)
}

fn string(value: &str) -> OwnedValue {
    OwnedValue::from(Str::from(value))
}

pub fn connection_settings(peer: &WirelessDisplayDevice) -> Result<Settings, NetworkError> {
    Ok(HashMap::from([
        (
            "connection".into(),
            HashMap::from([
                ("id".into(), string("WirelessDisplay temporary")),
                ("type".into(), string("wifi-p2p")),
                ("autoconnect".into(), OwnedValue::from(false)),
            ]),
        ),
        (
            "wifi-p2p".into(),
            HashMap::from([
                ("peer".into(), string(&peer.mac.to_string())),
                (
                    "wfd-ies".into(),
                    Value::new(SOURCE_WFD_IES.to_vec()).try_to_owned()?,
                ),
            ]),
        ),
        (
            "ipv4".into(),
            HashMap::from([("method".into(), string("auto"))]),
        ),
        (
            "ipv6".into(),
            HashMap::from([("method".into(), string("disabled"))]),
        ),
    ]))
}

impl P2pConnector for NetworkManagerService {
    type Error = NetworkError;
    type Connection = P2pConnection;
    async fn connect(
        &self,
        peer: &WirelessDisplayDevice,
        cancel: CancellationToken,
    ) -> Result<P2pConnection, NetworkError> {
        // Dedicated bus connection: bind-activation releases the temporary session even
        // if cancellation races AddAndActivateConnection2's reply.
        let work = async {
            // Discovery results are snapshots. Never activate a profile for an
            // expired peer, and recheck WFD availability before changing networking.
            let peer = self.current_peer(peer).await?;
            let bus = Connection::system().await?;
            let manager = ManagerProxy::new(&bus).await?;
            let device = DeviceProxy::builder(&bus)
                .path(peer.adapter_id.as_str())?
                .build()
                .await?;
            // Subscribe before activation so a fast failed -> disconnected
            // transition cannot erase the failure reason before we read it.
            let mut device_changes = device.receive_state_changed().await?;
            let options = HashMap::from([
                ("persist".into(), string("volatile")),
                ("bind-activation".into(), string("dbus-client")),
            ]);
            let (_, active, _) = manager
                .add_and_activate_connection2(
                    connection_settings(&peer)?,
                    OwnedObjectPath::try_from(peer.adapter_id.as_str())?,
                    OwnedObjectPath::try_from("/")?,
                    options,
                )
                .await?;
            let proxy = ActiveProxy::builder(&bus)
                .path(active.clone())?
                .build()
                .await?;
            let mut changes = proxy.receive_state_changed().await;
            {
                let activation = async {
                    loop {
                        let state = proxy.state().await?;
                        match state {
                            2 => return Ok::<(), NetworkError>(()),
                            3 | 4 => {
                                let (_, reason) = device.state_reason().await?;
                                return Err(if reason == 0 {
                                    NetworkError::Activation(state)
                                } else {
                                    device_activation_error(reason)
                                });
                            }
                            _ => {
                                if changes.next().await.is_none() {
                                    return Err(NetworkError::Activation(state));
                                }
                            }
                        }
                    }
                };
                tokio::pin!(activation);
                loop {
                    tokio::select! {
                        biased;
                        change = device_changes.next() => {
                            let change = change.ok_or(NetworkError::MonitorClosed)?;
                            let args = change.args()?;
                            if args.new_state == 120 { return Err(device_activation_error(args.reason)); }
                        },
                        result = &mut activation => { result?; break; }
                    }
                }
            }
            drop(device_changes);
            drop(device);
            drop(changes);
            drop(proxy);
            drop(manager);
            Ok(P2pConnection { bus, active })
        };
        tokio::select! { _ = cancel.cancelled() => Err(NetworkError::Cancelled), result = tokio::time::timeout(Duration::from_secs(90), work) => result.map_err(|_| NetworkError::Timeout)? }
    }

    async fn disconnect(&self, connection: P2pConnection) -> Result<(), NetworkError> {
        connection.disconnect().await
    }
}

pub const NETWORK_MANAGER_SERVICE: &str = NM;
pub const NETWORK_MANAGER_PATH: &str = ROOT;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wireless_display_core::dummy;

    struct DiscoveryAdapter {
        peers: Vec<OwnedObjectPath>,
        stops: Arc<AtomicUsize>,
    }

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device.WifiP2P")]
    impl DiscoveryAdapter {
        fn start_find(&self, _options: HashMap<String, OwnedValue>) {}

        fn stop_find(&self) {
            self.stops.fetch_add(1, Ordering::Relaxed);
        }

        #[zbus(property)]
        fn peers(&self) -> Vec<OwnedObjectPath> {
            self.peers.clone()
        }

        #[zbus(signal)]
        async fn peer_added(
            emitter: &zbus::object_server::SignalEmitter<'_>,
            peer: OwnedObjectPath,
        ) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn peer_removed(
            emitter: &zbus::object_server::SignalEmitter<'_>,
            peer: OwnedObjectPath,
        ) -> zbus::Result<()>;
    }

    #[derive(Default)]
    struct DiscoveryPeer {
        ies: Vec<u8>,
    }

    #[zbus::interface(name = "org.freedesktop.NetworkManager.WifiP2PPeer")]
    impl DiscoveryPeer {
        #[zbus(property)]
        fn name(&self) -> &str {
            "Test TV"
        }

        #[zbus(property)]
        fn hw_address(&self) -> &str {
            "02:00:00:00:00:01"
        }

        #[zbus(property, name = "WfdIEs")]
        fn wfd_ies(&self) -> Vec<u8> {
            self.ies.clone()
        }

        #[zbus(property)]
        fn strength(&self) -> u8 {
            80
        }
    }

    #[tokio::test]
    #[ignore = "requires an isolated D-Bus session; never uses the system bus"]
    async fn discovery_updates_existing_peer_capabilities_and_removal_on_dbus() {
        let peer_path = OwnedObjectPath::try_from("/test/peer").unwrap();
        let stops = Arc::new(AtomicUsize::new(0));
        let server = zbus::connection::Builder::session()
            .unwrap()
            .name(NM)
            .unwrap()
            .serve_at(
                "/test/adapter",
                DiscoveryAdapter {
                    peers: vec![peer_path.clone()],
                    stops: stops.clone(),
                },
            )
            .unwrap()
            .serve_at("/test/peer", DiscoveryPeer::default())
            .unwrap()
            .serve_at("/test/unrelated", DiscoveryPeer::default())
            .unwrap()
            .build()
            .await
            .unwrap();
        let service = NetworkManagerService {
            bus: Connection::session().await.unwrap(),
        };
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let (tx, rx) = async_channel::bounded(16);
        let task = tokio::spawn(async move {
            service
                .watch_adapter(
                    OwnedObjectPath::try_from("/test/adapter").unwrap(),
                    tx,
                    token,
                )
                .await
        });
        let initial = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(initial, DiscoveryEvent::Upsert(peer) if peer.peer_type == PeerType::UnknownP2P)
        );
        let unrelated = server
            .object_server()
            .interface::<_, DiscoveryPeer>("/test/unrelated")
            .await
            .unwrap();
        unrelated
            .get()
            .await
            .wfd_i_es_changed(unrelated.signal_emitter())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err()
        );

        let peer = server
            .object_server()
            .interface::<_, DiscoveryPeer>("/test/peer")
            .await
            .unwrap();
        for available in [true, false] {
            peer.get_mut().await.ies = vec![
                0,
                0,
                6,
                0,
                if available { 0x11 } else { 1 },
                0x1c,
                0x44,
                0,
                50,
            ];
            // The peer already exists: NetworkManager need not emit PeerAdded again.
            peer.get()
                .await
                .wfd_i_es_changed(peer.signal_emitter())
                .await
                .unwrap();
            let updated = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("WfdIEs changes must update an existing row without PeerAdded")
                .unwrap();
            assert!(matches!(updated, DiscoveryEvent::Upsert(peer)
                if peer.peer_type == PeerType::WirelessDisplayReceiver
                    && peer.wfd.as_ref().unwrap().session_available == available));
        }
        let adapter = server
            .object_server()
            .interface::<_, DiscoveryAdapter>("/test/adapter")
            .await
            .unwrap();
        adapter.get_mut().await.peers.clear();
        DiscoveryAdapter::peer_removed(adapter.signal_emitter(), peer_path.clone())
            .await
            .unwrap();
        let removed = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(removed, DiscoveryEvent::Removed(id) if id.as_str() == peer_path.as_str())
        );
        peer.get()
            .await
            .wfd_i_es_changed(peer.signal_emitter())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "late properties must not resurrect a removed peer"
        );
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(stops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn extracts_real_p2p_address_and_rejects_invalid_routes() {
        let address = || {
            vec![HashMap::from([
                ("address".into(), string("192.168.49.2")),
                ("prefix".into(), OwnedValue::from(24u32)),
            ])]
        };
        let network = parse_ip4_config(address(), "192.168.49.1").unwrap();
        assert_eq!(network.local.to_string(), "192.168.49.2");
        assert!(parse_ip4_config(address(), "10.0.0.1").is_err());
        assert!(parse_ip4_config(vec![], "").is_err());
        assert_eq!(parse_ip4_config(address(), "").unwrap().gateway, None);
    }
    struct NoPeers;

    struct ActiveNetwork {
        state: u32,
        path: &'static str,
    }

    impl Default for ActiveNetwork {
        fn default() -> Self {
            Self {
                state: 2,
                path: "/test/ip4",
            }
        }
    }

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
    impl ActiveNetwork {
        #[zbus(property)]
        fn state(&self) -> u32 {
            self.state
        }

        #[zbus(property, name = "Ip4Config")]
        fn ip4_config(&self) -> OwnedObjectPath {
            OwnedObjectPath::try_from(self.path).unwrap()
        }
    }

    struct GroupNetwork {
        address: Option<&'static str>,
    }

    #[zbus::interface(name = "org.freedesktop.NetworkManager.IP4Config")]
    impl GroupNetwork {
        #[zbus(property)]
        fn address_data(&self) -> Vec<HashMap<String, OwnedValue>> {
            self.address
                .map(|address| {
                    vec![HashMap::from([
                        ("address".into(), string(address)),
                        ("prefix".into(), OwnedValue::from(24u32)),
                    ])]
                })
                .unwrap_or_default()
        }

        #[zbus(property)]
        fn gateway(&self) -> String {
            String::new()
        }
    }

    #[tokio::test]
    #[ignore = "requires an isolated D-Bus session; never uses the system bus"]
    async fn active_group_network_is_read_and_monitored_on_dbus() {
        let _server = zbus::connection::Builder::session()
            .unwrap()
            .name(NM)
            .unwrap()
            .serve_at("/test/active", ActiveNetwork::default())
            .unwrap()
            .serve_at(
                "/test/ip4",
                GroupNetwork {
                    address: Some("192.168.49.10"),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let connection = P2pConnection {
            bus: Connection::session().await.unwrap(),
            active: OwnedObjectPath::try_from("/test/active").unwrap(),
        };
        let network = connection.network(&CancellationToken::new()).await.unwrap();
        assert_eq!(network.local.to_string(), "192.168.49.10");
        assert_eq!(network.prefix, 24);
        assert_eq!(network.gateway, None);
        let cancel = CancellationToken::new();
        let monitor = connection.monitor(network, cancel.clone());
        tokio::pin!(monitor);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut monitor)
                .await
                .is_err()
        );
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), monitor)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires an isolated D-Bus session; never uses the system bus"]
    async fn active_group_waits_for_addresses_on_dbus() {
        let server = zbus::connection::Builder::session()
            .unwrap()
            .name(NM)
            .unwrap()
            .serve_at("/test/active", ActiveNetwork::default())
            .unwrap()
            .serve_at("/test/ip4", GroupNetwork { address: None })
            .unwrap()
            .build()
            .await
            .unwrap();
        let connection = P2pConnection {
            bus: Connection::session().await.unwrap(),
            active: OwnedObjectPath::try_from("/test/active").unwrap(),
        };
        // Replay the host: activation succeeded while AddressData was still empty.
        let cancel = CancellationToken::new();
        let waiting = connection.network(&cancel);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiting)
                .await
                .is_err(),
            "activation must not fail before NetworkManager publishes AddressData"
        );
        let config = server
            .object_server()
            .interface::<_, GroupNetwork>("/test/ip4")
            .await
            .unwrap();
        config.get_mut().await.address = Some("192.168.49.10");
        config
            .get()
            .await
            .address_data_changed(config.signal_emitter())
            .await
            .unwrap();
        let network = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(network.local.to_string(), "192.168.49.10");

        // After sharing starts, losing the address must fail immediately instead
        // of applying the initial readiness wait to an existing stream.
        let monitoring = connection.monitor(network, cancel.clone());
        tokio::pin!(monitoring);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut monitoring)
                .await
                .is_err()
        );
        config.get_mut().await.address = None;
        config
            .get()
            .await
            .address_data_changed(config.signal_emitter())
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), monitoring)
                .await
                .unwrap(),
            Err(NetworkError::AddressUnavailable)
        ));

        let waiting = connection.network(&cancel);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiting)
                .await
                .is_err()
        );
        cancel.cancel();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .unwrap(),
            Err(NetworkError::Cancelled)
        ));
    }

    #[tokio::test]
    #[ignore = "requires an isolated D-Bus session; never uses the system bus"]
    async fn active_group_waits_for_config_and_observes_loss_on_dbus() {
        let server = zbus::connection::Builder::session()
            .unwrap()
            .name(NM)
            .unwrap()
            .serve_at(
                "/test/active",
                ActiveNetwork {
                    path: "/",
                    ..Default::default()
                },
            )
            .unwrap()
            .serve_at(
                "/test/ip4",
                GroupNetwork {
                    address: Some("192.168.49.10"),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let connection = P2pConnection {
            bus: Connection::session().await.unwrap(),
            active: OwnedObjectPath::try_from("/test/active").unwrap(),
        };
        let cancel = CancellationToken::new();
        let waiting = connection.network(&cancel);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiting)
                .await
                .is_err()
        );
        let active = server
            .object_server()
            .interface::<_, ActiveNetwork>("/test/active")
            .await
            .unwrap();
        active.get_mut().await.path = "/test/ip4";
        active
            .get()
            .await
            .ip4_config_changed(active.signal_emitter())
            .await
            .unwrap();
        let network = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        let monitoring = connection.monitor(network, cancel.clone());
        tokio::pin!(monitoring);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut monitoring)
                .await
                .is_err()
        );
        // During a real P2P teardown NM may clear Ip4Config before publishing
        // the disconnected state. Report address loss, not an IP reassignment.
        active.get_mut().await.path = "/";
        active
            .get()
            .await
            .ip4_config_changed(active.signal_emitter())
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), monitoring)
                .await
                .unwrap(),
            Err(NetworkError::AddressLost)
        ));
        active.get_mut().await.path = "/test/ip4";
        active
            .get()
            .await
            .ip4_config_changed(active.signal_emitter())
            .await
            .unwrap();
        let monitoring = connection.monitor(network, cancel.clone());
        tokio::pin!(monitoring);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut monitoring)
                .await
                .is_err()
        );
        active.get_mut().await.state = 4;
        active
            .get()
            .await
            .state_changed(active.signal_emitter())
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), monitoring)
                .await
                .unwrap(),
            Err(NetworkError::ConnectionLost(4))
        ));
    }

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device.WifiP2P")]
    impl NoPeers {
        #[zbus(property)]
        fn peers(&self) -> Vec<OwnedObjectPath> {
            Vec::new()
        }
    }

    #[tokio::test]
    #[ignore = "requires an isolated D-Bus session; never uses the system bus"]
    async fn expired_discovery_result_is_rejected_on_dbus() {
        let _server = zbus::connection::Builder::session()
            .unwrap()
            .name(NM)
            .unwrap()
            .serve_at("/test/adapter", NoPeers)
            .unwrap()
            .build()
            .await
            .unwrap();
        let service = NetworkManagerService {
            bus: Connection::session().await.unwrap(),
        };
        let mut selected = dummy::receiver();
        selected.id = DeviceId::try_new("/test/expired_peer").unwrap();
        selected.adapter_id = "/test/adapter".into();
        assert!(matches!(
            service.current_peer(&selected).await,
            Err(NetworkError::PeerUnavailable)
        ));
    }
    #[test]
    fn peer_not_found_has_actionable_error_without_addresses() {
        let error = device_activation_error(67);
        assert!(matches!(error, NetworkError::PeerUnavailable));
        assert!(error.to_string().contains("Search for devices"));
        assert!(matches!(
            device_activation_error(11),
            NetworkError::DeviceActivation(11)
        ));
    }

    #[test]
    fn revalidates_identity_and_current_wfd_availability() {
        let selected = dummy::receiver();
        let mut current = selected.clone();
        validate_current_peer(&selected, &current).unwrap();
        current.mac = "02:00:00:00:00:02".parse().unwrap();
        assert!(matches!(
            validate_current_peer(&selected, &current),
            Err(NetworkError::PeerUnavailable)
        ));
        current = selected.clone();
        current.wfd.as_mut().unwrap().session_available = false;
        assert!(matches!(
            validate_current_peer(&selected, &current),
            Err(NetworkError::ReceiverUnavailable)
        ));
    }
    #[test]
    fn device_types_match_networkmanager_enum() {
        assert_eq!(NetworkDeviceType::from(2), NetworkDeviceType::Wifi);
        assert_eq!(NetworkDeviceType::from(30), NetworkDeviceType::WifiP2p);
        assert_eq!(NetworkDeviceType::from(999), NetworkDeviceType::Other(999));
    }
    #[test]
    fn settings_are_temporary_source_and_peer_specific() {
        let settings = connection_settings(&dummy::receiver()).unwrap();
        assert_eq!(
            <&str>::try_from(&settings["connection"]["type"]).unwrap(),
            "wifi-p2p"
        );
        assert!(!bool::try_from(&settings["connection"]["autoconnect"]).unwrap());
        assert_eq!(
            <&str>::try_from(&settings["wifi-p2p"]["peer"]).unwrap(),
            "02:00:00:00:00:01"
        );
    }
}
