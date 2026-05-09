use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use log::{error, info, warn};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use zbus::fdo::{self, DBusProxy};
use zbus::message::Header;
use zbus::{Connection, connection, proxy};

use crate::state::KeyboardStateManager;

pub const ROOT_DBUS_SERVICE: &str = "asus.zenbook.duo";
pub const ROOT_DBUS_PATH: &str = "/asus/zenbook/duo/State";

#[derive(Clone, Debug, PartialEq, Eq)]
enum AckEvent {
    KeyboardAttached(bool),
    DesiredSecondaryApplied(bool),
}

#[derive(Clone, Debug)]
struct RegisteredSession {
    session_id: u64,
    owner: String,
}

#[derive(Clone)]
struct RootDbusHandle {
    connection: Connection,
    ack_tx: broadcast::Sender<AckEvent>,
    registration: Arc<RwLock<Option<RegisteredSession>>>,
    registered: Arc<AtomicBool>,
}

static ROOT_DBUS_HANDLE: OnceLock<Mutex<Option<RootDbusHandle>>> = OnceLock::new();

fn dbus_handle_cell() -> &'static Mutex<Option<RootDbusHandle>> {
    ROOT_DBUS_HANDLE.get_or_init(|| Mutex::new(None))
}

async fn get_root_handle() -> Option<RootDbusHandle> {
    dbus_handle_cell().lock().await.clone()
}

struct RootStateInterface {
    state_manager: KeyboardStateManager,
    registration: Arc<RwLock<Option<RegisteredSession>>>,
    registered: Arc<AtomicBool>,
    ack_tx: broadcast::Sender<AckEvent>,
}

impl RootStateInterface {
    fn new(
        state_manager: KeyboardStateManager,
        registration: Arc<RwLock<Option<RegisteredSession>>>,
        registered: Arc<AtomicBool>,
        ack_tx: broadcast::Sender<AckEvent>,
    ) -> Self {
        Self {
            state_manager,
            registration,
            registered,
            ack_tx,
        }
    }

    async fn ensure_registered_sender(&self, header: &Header<'_>) -> fdo::Result<()> {
        let Some(sender) = header.sender() else {
            return Err(fdo::Error::AccessDenied("missing D-Bus sender".into()));
        };

        let registration = self.registration.read().await;
        match registration.as_ref() {
            Some(session) if session.owner == sender.as_str() => Ok(()),
            Some(session) => Err(fdo::Error::AccessDenied(format!(
                "sender {} does not match registered session {}",
                sender.as_str(),
                session.owner
            ))),
            None => Err(fdo::Error::AccessDenied(
                "no session daemon is currently registered".into(),
            )),
        }
    }
}

#[zbus::interface(name = "asus.zenbook.duo.State")]
impl RootStateInterface {
    #[zbus(property)]
    fn keyboard_attached(&self) -> bool {
        self.state_manager.is_usb_keyboard_attached()
    }

    #[zbus(property)]
    fn bluetooth_connected(&self) -> bool {
        self.state_manager.is_bluetooth_connected()
    }

    #[zbus(property)]
    fn desired_primary(&self) -> String {
        self.state_manager
            .get_desired_primary()
            .unwrap_or_else(|| "eDP-1".to_string())
    }

    #[zbus(property)]
    fn desired_secondary_enabled(&self) -> bool {
        self.state_manager.is_secondary_display_desired_enabled()
    }

    #[zbus(property)]
    fn ambient_light_enabled(&self) -> bool {
        self.state_manager.get_ambient_light_enabled()
    }

    #[zbus(property)]
    fn display_brightness(&self) -> u32 {
        self.state_manager.get_display_brightness_value().unwrap_or(0)
    }

    async fn register_session(
        &self,
        session_id: u64,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        let Some(sender) = header.sender() else {
            return Err(fdo::Error::Failed(
                "session registration is missing sender information".into(),
            ));
        };

        // Verify the caller is a real user (UID >= 1000), not a system service.
        let dbus_proxy = DBusProxy::new(connection)
            .await
            .map_err(|e| fdo::Error::Failed(format!("failed to query D-Bus for caller UID: {e}")))?;
        let bus_name = zbus::names::BusName::try_from(sender.as_str())
            .map_err(|e| fdo::Error::Failed(format!("invalid sender bus name: {e}")))?;
        let uid = dbus_proxy
            .get_connection_unix_user(bus_name)
            .await
            .map_err(|e| fdo::Error::Failed(format!("failed to get caller UID: {e}")))?;
        if uid < 1000 {
            warn!("ROOT DBus: rejected register_session from UID {uid} (not a session user)");
            return Err(fdo::Error::AccessDenied(
                format!("UID {uid} is not permitted to register a session"),
            ));
        }

        if self.state_manager.is_secondary_display_enabled() {
            crate::secondary_display::arm_sysfs_fallback_once();
            self.state_manager.emit_secondary_display_state();
            if crate::secondary_display::wait_for_secondary_display_state(true, Duration::from_secs(8))
                .await
            {
                info!("ROOT DBus: secondary display reported enabled before session registration sync");
            } else {
                warn!("ROOT DBus: timed out waiting for secondary display enable during session registration");
            }
        }

        let new_registration = RegisteredSession {
            session_id,
            owner: sender.as_str().to_string(),
        };

        {
            let mut registration = self.registration.write().await;
            if let Some(previous) = registration.as_ref() {
                if previous.owner != new_registration.owner || previous.session_id != session_id {
                    info!(
                        "ROOT DBus: replacing registered session owner={} session_id={} with owner={} session_id={}",
                        previous.owner, previous.session_id, new_registration.owner, session_id
                    );
                }
            } else {
                info!(
                    "ROOT DBus: registered session owner={} session_id={}",
                    new_registration.owner, session_id
                );
            }
            *registration = Some(new_registration);
        }
        self.registered.store(true, Ordering::SeqCst);

        Ok(())
    }

    async fn acknowledge_keyboard_attached(
        &self,
        value: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.ensure_registered_sender(&header).await?;
        let _ = self.ack_tx.send(AckEvent::KeyboardAttached(value));
        Ok(())
    }

    async fn acknowledge_desired_secondary_applied(
        &self,
        value: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.ensure_registered_sender(&header).await?;
        let _ = self.ack_tx.send(AckEvent::DesiredSecondaryApplied(value));
        Ok(())
    }

    async fn report_ambient_light_enabled(
        &self,
        value: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.ensure_registered_sender(&header).await?;
        if self.state_manager.get_ambient_light_enabled() != value {
            info!("ROOT DBus: ambient light setting updated to {}", value);
            self.state_manager.set_ambient_light_enabled(value);
        }
        Ok(())
    }
}

#[proxy(
    interface = "asus.zenbook.duo.State",
    default_service = "asus.zenbook.duo",
    default_path = "/asus/zenbook/duo/State"
)]
trait RootState {
    fn register_session(&self, session_id: u64) -> zbus::Result<()>;
    fn acknowledge_keyboard_attached(&self, value: bool) -> zbus::Result<()>;
    fn acknowledge_desired_secondary_applied(&self, value: bool) -> zbus::Result<()>;
    fn report_ambient_light_enabled(&self, value: bool) -> zbus::Result<()>;

    #[zbus(property)]
    fn keyboard_attached(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn bluetooth_connected(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn desired_primary(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn desired_secondary_enabled(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn ambient_light_enabled(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn display_brightness(&self) -> zbus::Result<u32>;
}

async fn emit_keyboard_attached_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .keyboard_attached_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

async fn emit_bluetooth_connected_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .bluetooth_connected_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

async fn emit_desired_primary_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .desired_primary_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

async fn emit_desired_secondary_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .desired_secondary_enabled_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

async fn emit_display_brightness_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .display_brightness_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

async fn clear_registration_for_owner(owner: &str) {
    let Some(handle) = get_root_handle().await else {
        return;
    };

    let mut registration = handle.registration.write().await;
    if registration
        .as_ref()
        .is_some_and(|registered| registered.owner == owner)
    {
        info!("ROOT DBus: registered session owner {} disappeared", owner);
        *registration = None;
        handle.registered.store(false, Ordering::SeqCst);
    }
}

async fn watch_registered_session(connection: Connection) {
    let proxy = match DBusProxy::new(&connection).await {
        Ok(proxy) => proxy,
        Err(e) => {
            error!("ROOT DBus: failed to create org.freedesktop.DBus proxy: {e}");
            return;
        }
    };

    let mut changes = match proxy.receive_name_owner_changed().await {
        Ok(changes) => changes,
        Err(e) => {
            error!("ROOT DBus: failed to subscribe to NameOwnerChanged: {e}");
            return;
        }
    };

    while let Some(signal) = changes.next().await {
        match signal.args() {
            Ok(args) if args.new_owner().is_none() => {
                clear_registration_for_owner(args.name().as_str()).await;
            }
            Ok(_) => {}
            Err(e) => warn!("ROOT DBus: failed to parse NameOwnerChanged args: {e}"),
        }
    }
}

pub async fn start_root_service(
    state_manager: KeyboardStateManager,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let registration = Arc::new(RwLock::new(None));
    let registered = Arc::new(AtomicBool::new(false));
    let (ack_tx, _) = broadcast::channel(16);

    let connection = connection::Builder::system()?
        .name(ROOT_DBUS_SERVICE)?
        .serve_at(
            ROOT_DBUS_PATH,
            RootStateInterface::new(
                state_manager,
                registration.clone(),
                registered.clone(),
                ack_tx.clone(),
            ),
        )?
        .build()
        .await?;

    {
        let mut slot = dbus_handle_cell().lock().await;
        *slot = Some(RootDbusHandle {
            connection: connection.clone(),
            ack_tx,
            registration,
            registered,
        });
    }

    tokio::spawn(watch_registered_session(connection));
    info!("ROOT DBus: exported {}", ROOT_DBUS_SERVICE);
    Ok(())
}

pub fn is_session_registered() -> bool {
    ROOT_DBUS_HANDLE
        .get()
        .and_then(|cell| cell.try_lock().ok())
        .and_then(|guard| guard.as_ref().cloned())
        .is_some_and(|handle| handle.registered.load(Ordering::SeqCst))
}

async fn wait_for_ack(expected: AckEvent, timeout: Duration) -> bool {
    let Some(handle) = get_root_handle().await else {
        return false;
    };

    let mut ack_rx = handle.ack_tx.subscribe();
    tokio::time::timeout(timeout, async move {
        loop {
            match ack_rx.recv().await {
                Ok(ack) if ack == expected => break true,
                Ok(_) => continue,
                Err(_) => break false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

pub async fn notify_keyboard_attached_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_keyboard_attached_changed().await
}

pub async fn notify_bluetooth_connected_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_bluetooth_connected_changed().await
}

pub async fn notify_desired_primary_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_desired_primary_changed().await
}

pub async fn notify_desired_secondary_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_desired_secondary_changed().await
}

pub async fn notify_display_brightness_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_display_brightness_changed().await
}

pub async fn wait_for_keyboard_attached_ack(attached: bool, timeout: Duration) -> bool {
    wait_for_ack(AckEvent::KeyboardAttached(attached), timeout).await
}

pub async fn wait_for_desired_secondary_ack(enabled: bool, timeout: Duration) -> bool {
    wait_for_ack(AckEvent::DesiredSecondaryApplied(enabled), timeout).await
}

async fn wait_for_internal_ack(
    rx: &Arc<Mutex<mpsc::Receiver<()>>>,
    timeout: Duration,
    what: &str,
) -> bool {
    match tokio::time::timeout(timeout, async { rx.lock().await.recv().await }).await {
        Ok(Some(_)) => true,
        Ok(None) => {
            warn!("SESSION DBus: {what} channel closed before handler completed");
            false
        }
        Err(_) => {
            warn!("SESSION DBus: timed out waiting for {what}");
            false
        }
    }
}

async fn apply_keyboard_update(
    proxy: &RootStateProxy<'_>,
    kb_tx: &broadcast::Sender<bool>,
    kb_result_rx: &Arc<Mutex<mpsc::Receiver<()>>>,
    value: bool,
) {
    kb_tx.send(value).ok();
    if wait_for_internal_ack(kb_result_rx, Duration::from_secs(5), "keyboard handler").await {
        if let Err(e) = proxy.acknowledge_keyboard_attached(value).await {
            warn!("SESSION DBus: failed to acknowledge keyboard_attached={value}: {e}");
        }
    }
}

async fn apply_desired_secondary_update(
    proxy: &RootStateProxy<'_>,
    desired_secondary: &Arc<RwLock<bool>>,
    desired_secondary_tx: &broadcast::Sender<bool>,
    secondary_result_rx: &Arc<Mutex<mpsc::Receiver<()>>>,
    value: bool,
) {
    *desired_secondary.write().await = value;
    desired_secondary_tx.send(value).ok();
    if wait_for_internal_ack(
        secondary_result_rx,
        Duration::from_secs(8),
        "desired secondary handler",
    )
    .await
    {
        if let Err(e) = proxy.acknowledge_desired_secondary_applied(value).await {
            warn!("SESSION DBus: failed to acknowledge desired_secondary_enabled={value}: {e}");
        }
    }
}

async fn apply_desired_primary_update(
    desired_primary: &Arc<RwLock<String>>,
    desired_primary_tx: &broadcast::Sender<String>,
    value: String,
) {
    *desired_primary.write().await = value.clone();
    desired_primary_tx.send(value).ok();
}

fn coalesce_latest_ambient_value(
    ambient_report_rx: &mut mpsc::Receiver<bool>,
    latest_ambient_value: &mut Option<bool>,
) -> bool {
    loop {
        match ambient_report_rx.try_recv() {
            Ok(value) => *latest_ambient_value = Some(value),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return true,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return false,
        }
    }
}

pub async fn run_session_client(
    session_id: u64,
    desired_primary: Arc<RwLock<String>>,
    desired_secondary: Arc<RwLock<bool>>,
    desired_primary_tx: broadcast::Sender<String>,
    desired_secondary_tx: broadcast::Sender<bool>,
    kb_tx: broadcast::Sender<bool>,
    secondary_result_rx: Arc<Mutex<mpsc::Receiver<()>>>,
    kb_result_rx: Arc<Mutex<mpsc::Receiver<()>>>,
    mut ambient_report_rx: mpsc::Receiver<bool>,
) {
    let mut latest_ambient_value = None;

    loop {
        if !coalesce_latest_ambient_value(&mut ambient_report_rx, &mut latest_ambient_value) {
            return;
        }

        let connection = match Connection::system().await {
            Ok(connection) => connection,
            Err(e) => {
                error!("SESSION DBus: failed to connect to system bus: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let proxy = match RootStateProxy::new(&connection).await {
            Ok(proxy) => proxy,
            Err(e) => {
                error!("SESSION DBus: failed to create root state proxy: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        if let Err(e) = proxy.register_session(session_id).await {
            error!("SESSION DBus: failed to register with root daemon: {e}");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        info!("SESSION DBus: registered with root daemon");

        if !coalesce_latest_ambient_value(&mut ambient_report_rx, &mut latest_ambient_value) {
            return;
        }

        if let Some(value) = latest_ambient_value {
            if let Err(e) = proxy.report_ambient_light_enabled(value).await {
                warn!("SESSION DBus: failed to replay ambient_light_enabled={value} after registration: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        }

        match proxy.keyboard_attached().await {
            Ok(value) => apply_keyboard_update(&proxy, &kb_tx, &kb_result_rx, value).await,
            Err(e) => warn!("SESSION DBus: failed to read initial keyboard_attached property: {e}"),
        }

        match proxy.desired_secondary_enabled().await {
            Ok(value) => {
                apply_desired_secondary_update(
                    &proxy,
                    &desired_secondary,
                    &desired_secondary_tx,
                    &secondary_result_rx,
                    value,
                )
                .await;
            }
            Err(e) => warn!("SESSION DBus: failed to read initial desired_secondary_enabled property: {e}"),
        }

        match proxy.desired_primary().await {
            Ok(value) => apply_desired_primary_update(&desired_primary, &desired_primary_tx, value).await,
            Err(e) => warn!("SESSION DBus: failed to read initial desired_primary property: {e}"),
        }

        match proxy.display_brightness().await {
            Ok(value) if value > 0 => {
                if let Err(e) = crate::session::display::apply_display_brightness_value(value).await {
                    warn!("SESSION DBus: failed to apply initial display_brightness={value}: {e}");
                }
            }
            Ok(_) => {}
            Err(e) => warn!("SESSION DBus: failed to read initial display_brightness property: {e}"),
        }

        let dbus_proxy = match DBusProxy::new(&connection).await {
            Ok(proxy) => proxy,
            Err(e) => {
                error!("SESSION DBus: failed to create org.freedesktop.DBus proxy: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let mut owner_changes = match dbus_proxy
            .receive_name_owner_changed_with_args(&[(0, ROOT_DBUS_SERVICE)])
            .await
        {
            Ok(stream) => stream,
            Err(e) => {
                error!("SESSION DBus: failed to subscribe to root owner changes: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let mut keyboard_changes = proxy.inner().receive_property_changed("KeyboardAttached").await;
        let mut desired_secondary_changes = proxy
            .inner()
            .receive_property_changed("DesiredSecondaryEnabled")
            .await;
        let mut desired_primary_changes = proxy.inner().receive_property_changed("DesiredPrimary").await;
        let mut display_brightness_changes = proxy.inner().receive_property_changed("DisplayBrightness").await;

        let mut reconnect = false;
        while !reconnect {
            tokio::select! {
                change = owner_changes.next() => {
                    match change {
                        Some(signal) => match signal.args() {
                            Ok(args) if args.new_owner().is_none() => {
                                info!("SESSION DBus: root daemon disappeared from system bus");
                                reconnect = true;
                            }
                            Ok(_) => {}
                            Err(e) => {
                                warn!("SESSION DBus: failed to parse root owner change: {e}");
                                reconnect = true;
                            }
                        },
                        None => reconnect = true,
                    }
                }
                change = keyboard_changes.next() => {
                    match change {
                        Some(change) => match change.get().await {
                            Ok(value) => apply_keyboard_update(&proxy, &kb_tx, &kb_result_rx, value).await,
                            Err(e) => warn!("SESSION DBus: failed to decode KeyboardAttached change: {e}"),
                        },
                        None => reconnect = true,
                    }
                }
                change = desired_secondary_changes.next() => {
                    match change {
                        Some(change) => match change.get().await {
                            Ok(value) => {
                                apply_desired_secondary_update(
                                    &proxy,
                                    &desired_secondary,
                                    &desired_secondary_tx,
                                    &secondary_result_rx,
                                    value,
                                )
                                .await;
                            }
                            Err(e) => warn!("SESSION DBus: failed to decode DesiredSecondaryEnabled change: {e}"),
                        },
                        None => reconnect = true,
                    }
                }
                change = desired_primary_changes.next() => {
                    match change {
                        Some(change) => match change.get().await {
                            Ok(value) => apply_desired_primary_update(&desired_primary, &desired_primary_tx, value).await,
                            Err(e) => warn!("SESSION DBus: failed to decode DesiredPrimary change: {e}"),
                        },
                        None => reconnect = true,
                    }
                }
                change = display_brightness_changes.next() => {
                    match change {
                        Some(change) => match change.get().await {
                            Ok(value) if value > 0 => {
                                if let Err(e) = crate::session::display::apply_display_brightness_value(value).await {
                                    warn!("SESSION DBus: failed to apply display_brightness={value}: {e}");
                                }
                            }
                            Ok(_) => {}
                            Err(e) => warn!("SESSION DBus: failed to decode DisplayBrightness change: {e}"),
                        },
                        None => reconnect = true,
                    }
                }
                ambient_value = ambient_report_rx.recv() => {
                    match ambient_value {
                        Some(value) => {
                            latest_ambient_value = Some(value);
                            if let Err(e) = proxy.report_ambient_light_enabled(value).await {
                                warn!("SESSION DBus: failed to report ambient_light_enabled={value}: {e}");
                                reconnect = true;
                            }
                        }
                        None => reconnect = true,
                    }
                }
            }
        }

        info!("SESSION DBus: reconnecting to root daemon");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
