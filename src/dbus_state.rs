use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use log::{error, info, warn};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use zbus::fdo::{self, DBusProxy};
use zbus::message::{Header, Type};
use zbus::{MatchRule, MessageStream, connection, proxy, Connection};

use crate::config::Config;
use crate::idle_detection::ActivityNotifier;
use crate::state::{KeyboardBacklightState, KeyboardStateManager};

pub const ROOT_DBUS_SERVICE: &str = "asus.zenbook.duo";
pub const ROOT_DBUS_PATH: &str = "/asus/zenbook/duo/State";
/// Budget for apply attempts in a rolling window (see decay below). Higher than the session
/// recovery retry count so a full recovery pass is not cut off by decay, while still
/// bounding bursts that destabilize KMS.
const DISPLAY_APPLY_GUARD_MAX_ATTEMPTS: u32 = 15;
const DISPLAY_APPLY_GUARD_DECAY_SECS: u64 = 5;

#[derive(Default)]
struct DisplayApplyGuard {
    attempts: u32,
    paused: bool,
}

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

/// System bus connection and session id for `register_session` / `register_display_apply_attempt`.
/// Reusing one connection is required so the message sender matches the registered session owner.
/// The `u64` is the same `session_id` passed to `register_session` for self-heal retries.
type SessionSystemBusRegistration = (Connection, u64);
static SESSION_REGISTERED_SYSTEM_BUS: OnceLock<Mutex<Option<SessionSystemBusRegistration>>> =
    OnceLock::new();

fn session_registered_system_bus_cell() -> &'static Mutex<Option<SessionSystemBusRegistration>> {
    SESSION_REGISTERED_SYSTEM_BUS.get_or_init(|| Mutex::new(None))
}

async fn replace_session_registered_system_bus(reg: Option<SessionSystemBusRegistration>) {
    *session_registered_system_bus_cell().lock().await = reg;
}

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
    apply_guard: Arc<Mutex<DisplayApplyGuard>>,
}

impl RootStateInterface {
    fn new(
        state_manager: KeyboardStateManager,
        registration: Arc<RwLock<Option<RegisteredSession>>>,
        registered: Arc<AtomicBool>,
        ack_tx: broadcast::Sender<AckEvent>,
        apply_guard: Arc<Mutex<DisplayApplyGuard>>,
    ) -> Self {
        Self {
            state_manager,
            registration,
            registered,
            ack_tx,
            apply_guard,
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

    /// Operators (CLI / scripts): root UID or normal user (UID ≥ 1000). System accounts in between are rejected.
    async fn require_operator(
        &self,
        header: &Header<'_>,
        connection: &Connection,
    ) -> fdo::Result<()> {
        let Some(sender) = header.sender() else {
            return Err(fdo::Error::AccessDenied("missing D-Bus sender".into()));
        };
        let dbus_proxy = DBusProxy::new(connection)
            .await
            .map_err(|e| fdo::Error::Failed(format!("failed to query D-Bus for caller UID: {e}")))?;
        let bus_name = zbus::names::BusName::try_from(sender.as_str())
            .map_err(|e| fdo::Error::Failed(format!("invalid sender bus name: {e}")))?;
        let uid = dbus_proxy
            .get_connection_unix_user(bus_name)
            .await
            .map_err(|e| fdo::Error::Failed(format!("failed to get caller UID: {e}")))?;
        if uid > 0 && uid < 1000 {
            warn!("ROOT DBus: rejected operator command from system UID {uid}");
            return Err(fdo::Error::AccessDenied(format!(
                "UID {uid} is not permitted to run operator commands"
            )));
        }
        Ok(())
    }
}

/// One-shot commands forwarded to the running root daemon (see CLI `control` / legacy shell scripts).
#[derive(Debug, Clone)]
pub enum OperatorCommand {
    ToggleMicMuteLed,
    SetMicMuteLed(bool),
    ToggleKeyboardBacklight,
    /// 0 = off, 1 = low, 2 = medium, 3 = high
    SetKeyboardBacklightLevel(u8),
    ToggleSecondaryDisplay,
    SetSecondaryDisplay(bool),
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
    fn desired_display_attachment(&self) -> String {
        crate::state::load_desired_display_mode().0
    }

    #[zbus(property)]
    fn desired_display_layout(&self) -> String {
        crate::state::load_desired_display_mode().1
    }

    async fn report_observed_display_mode(
        &self,
        attachment: String,
        layout: String,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.ensure_registered_sender(&header).await?;
        if let Err(e) =
            crate::state::validate_desired_display_mode_strings(&attachment, &layout)
        {
            return Err(fdo::Error::Failed(e));
        }
        crate::state::persist_desired_display_mode(&attachment, &layout);
        info!(
            "ROOT DBus: persisted observed display mode attachment={attachment} layout={layout}"
        );
        if let Err(e) = emit_desired_display_mode_changed().await {
            warn!("report_observed_display_mode: failed to emit property change: {e}");
        }
        Ok(())
    }

    #[zbus(property)]
    fn display_brightness(&self) -> u32 {
        self.state_manager.get_display_brightness_value().unwrap_or(0)
    }

    #[zbus(property)]
    async fn display_apply_paused(&self) -> bool {
        self.apply_guard.lock().await.paused
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

    async fn register_display_apply_attempt(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<bool> {
        self.ensure_registered_sender(&header).await?;

        let mut guard = self.apply_guard.lock().await;
        if guard.paused {
            return Ok(false);
        }

        guard.attempts = guard.attempts.saturating_add(1);
        if guard.attempts > DISPLAY_APPLY_GUARD_MAX_ATTEMPTS {
            guard.paused = true;
            warn!(
                "ROOT DBus: pausing display applies (attempt counter {} exceeded max {})",
                guard.attempts, DISPLAY_APPLY_GUARD_MAX_ATTEMPTS
            );
            return Ok(false);
        }
        drop(guard);

        let apply_guard = Arc::clone(&self.apply_guard);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(DISPLAY_APPLY_GUARD_DECAY_SECS)).await;
            let mut guard = apply_guard.lock().await;
            guard.attempts = guard.attempts.saturating_sub(1);
        });

        Ok(true)
    }

    async fn resume_display_applies(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        let Some(sender) = header.sender() else {
            return Err(fdo::Error::AccessDenied("missing D-Bus sender".into()));
        };

        let dbus_proxy = DBusProxy::new(connection)
            .await
            .map_err(|e| fdo::Error::Failed(format!("failed to query D-Bus for caller UID: {e}")))?;
        let bus_name = zbus::names::BusName::try_from(sender.as_str())
            .map_err(|e| fdo::Error::Failed(format!("invalid sender bus name: {e}")))?;
        let uid = dbus_proxy
            .get_connection_unix_user(bus_name)
            .await
            .map_err(|e| fdo::Error::Failed(format!("failed to get caller UID: {e}")))?;
        if uid != 0 {
            return Err(fdo::Error::AccessDenied(
                "only root can resume display applies".into(),
            ));
        }

        let mut guard = self.apply_guard.lock().await;
        guard.attempts = 0;
        guard.paused = false;
        info!("ROOT DBus: display apply guard resumed by root command");
        Ok(())
    }

    async fn toggle_mic_mute_led(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        self.state_manager.toggle_mic_mute_led();
        Ok(())
    }

    async fn set_mic_mute_led(
        &self,
        enabled: bool,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        self.state_manager.set_mic_mute_led(enabled);
        Ok(())
    }

    async fn toggle_keyboard_backlight(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        self.state_manager.toggle_keyboard_backlight();
        Ok(())
    }

    async fn set_keyboard_backlight_level(
        &self,
        level: u8,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        let state = match level {
            0 => KeyboardBacklightState::Off,
            1 => KeyboardBacklightState::Low,
            2 => KeyboardBacklightState::Medium,
            3 => KeyboardBacklightState::High,
            _ => {
                return Err(fdo::Error::Failed(format!(
                    "keyboard backlight level must be 0..=3, got {level}"
                )));
            }
        };
        self.state_manager.set_keyboard_backlight(state);
        Ok(())
    }

    async fn toggle_secondary_display(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        self.state_manager.toggle_secondary_display();
        if let Err(e) = emit_desired_secondary_changed().await {
            warn!("toggle_secondary_display: failed to notify session D-Bus: {e}");
        }
        Ok(())
    }

    async fn set_secondary_display_desired(
        &self,
        enabled: bool,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        self.state_manager.set_secondary_display(enabled);
        if let Err(e) = emit_desired_secondary_changed().await {
            warn!("set_secondary_display_desired: failed to notify session D-Bus: {e}");
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
    fn report_observed_display_mode(&self, attachment: String, layout: String) -> zbus::Result<()>;
    fn register_display_apply_attempt(&self) -> zbus::Result<bool>;
    fn resume_display_applies(&self) -> zbus::Result<()>;
    fn toggle_mic_mute_led(&self) -> zbus::Result<()>;
    fn set_mic_mute_led(&self, enabled: bool) -> zbus::Result<()>;
    fn toggle_keyboard_backlight(&self) -> zbus::Result<()>;
    fn set_keyboard_backlight_level(&self, level: u8) -> zbus::Result<()>;
    fn toggle_secondary_display(&self) -> zbus::Result<()>;
    fn set_secondary_display_desired(&self, enabled: bool) -> zbus::Result<()>;

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
    fn desired_display_attachment(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn desired_display_layout(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn display_brightness(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn display_apply_paused(&self) -> zbus::Result<bool>;
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

async fn emit_desired_display_mode_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
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
        .desired_display_attachment_changed(iface_ref.signal_emitter())
        .await?;
    iface_ref
        .get()
        .await
        .desired_display_layout_changed(iface_ref.signal_emitter())
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

async fn watch_prepare_for_sleep(
    config: Config,
    state_manager: KeyboardStateManager,
    activity_notifier: ActivityNotifier,
) {
    const RULE: &str = "type='signal',interface='org.freedesktop.login1.Manager',member='PrepareForSleep',path='/org/freedesktop/login1'";
    let rule = match MatchRule::try_from(RULE) {
        Ok(r) => r,
        Err(e) => {
            warn!("logind PrepareForSleep: invalid match rule: {e}");
            return;
        }
    };

    let connection = match Connection::system().await {
        Ok(c) => c,
        Err(e) => {
            warn!("logind PrepareForSleep: system bus failed: {e}");
            return;
        }
    };

    let mut stream = match MessageStream::for_match_rule(rule, &connection, None).await {
        Ok(s) => s,
        Err(e) => {
            warn!("logind PrepareForSleep: subscribe failed: {e}");
            return;
        }
    };

    info!("logind: subscribed to PrepareForSleep");
    while let Some(item) = stream.next().await {
        let msg = match item {
            Ok(m) => m,
            Err(e) => {
                warn!("logind PrepareForSleep stream: {e}");
                continue;
            }
        };
        if msg.header().message_type() != Type::Signal {
            continue;
        }

        let start: bool = match msg.body().deserialize_unchecked() {
            Ok(v) => v,
            Err(e) => {
                warn!("logind PrepareForSleep: bad signal body: {e}");
                continue;
            }
        };

        if start {
            info!("PrepareForSleep(start=true): suspending");
            state_manager.suspend_start();
        } else {
            info!("PrepareForSleep(start=false): resumed");
            state_manager.suspend_end();
            activity_notifier.notify();

            let current_usb_attached =
                crate::keyboard_usb::find_wired_keyboard(&config).await.is_some();
            let reported_attached = state_manager.is_usb_keyboard_attached();
            if current_usb_attached != reported_attached {
                info!(
                    "Post-resume keyboard state mismatch: usb_attached={current_usb_attached}, reported={reported_attached} — updating state"
                );
                state_manager.set_usb_keyboard_attached(current_usb_attached);
            }
        }
    }

    warn!("logind PrepareForSleep stream ended");
}

pub async fn start_root_service(
    state_manager: KeyboardStateManager,
    config: Config,
    activity_notifier: ActivityNotifier,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let registration = Arc::new(RwLock::new(None));
    let registered = Arc::new(AtomicBool::new(false));
    let (ack_tx, _) = broadcast::channel(16);
    let apply_guard = Arc::new(Mutex::new(DisplayApplyGuard::default()));

    let connection = connection::Builder::system()?
        .name(ROOT_DBUS_SERVICE)?
        .serve_at(
            ROOT_DBUS_PATH,
            RootStateInterface::new(
                state_manager.clone(),
                registration.clone(),
                registered.clone(),
                ack_tx.clone(),
                apply_guard,
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

    tokio::spawn(watch_registered_session(connection.clone()));
    let cfg = config.clone();
    let sm = state_manager.clone();
    let an = activity_notifier.clone();
    tokio::spawn(async move {
        watch_prepare_for_sleep(cfg, sm, an).await;
    });
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayApplyPermit {
    /// Root cleared this apply; session may call Mutter `apply_monitors_config`.
    Allowed,
    /// Root guard is paused or over budget; session must not apply.
    Paused,
}

/// Ask the root daemon for permission before each `apply_monitors_config`.
/// Fail-closed: if the system bus or method call fails, returns `Err` and the session must not apply.
///
/// Uses the same system bus connection as [`run_session_client`]'s successful `register_session`
/// so the D-Bus sender matches the registered session owner.
///
/// If the root daemon still has an older unique name (e.g. identity drift without triggering our
/// reconnect loop), we call [`RootState::register_session`] once and retry the guard call.
pub async fn register_display_apply_attempt() -> Result<DisplayApplyPermit, String> {
    let (connection, session_id) = {
        let guard = session_registered_system_bus_cell().lock().await;
        guard
            .as_ref()
            .map(|(c, sid)| (c.clone(), *sid))
            .ok_or_else(|| {
                "session system bus not registered yet (is the session daemon running and connected to root?)"
                    .to_string()
            })?
    };

    let mut re_registered = false;
    loop {
        let proxy = RootStateProxy::new(&connection)
            .await
            .map_err(|e| format!("root state proxy for display apply guard: {e}"))?;

        match proxy.register_display_apply_attempt().await {
            Ok(true) => return Ok(DisplayApplyPermit::Allowed),
            Ok(false) => return Ok(DisplayApplyPermit::Paused),
            Err(e) => {
                let msg = e.to_string();
                let sender_mismatch = msg.contains("does not match registered session");
                if sender_mismatch && !re_registered {
                    warn!(
                        "SESSION DBus: display apply guard reports sender mismatch vs root registration; \
                         calling register_session again (session_id={session_id})"
                    );
                    proxy
                        .register_session(session_id)
                        .await
                        .map_err(|e2| {
                            format!(
                                "register_session after display guard sender mismatch: {e2} (was: {msg})"
                            )
                        })?;
                    re_registered = true;
                    continue;
                }
                return Err(format!("register_display_apply_attempt: {e}"));
            }
        }
    }
}

/// Read persisted desired display attachment/layout from the root daemon (same bus registration as
/// [`register_display_apply_attempt`]).
pub async fn read_persisted_display_mode_from_root() -> Result<(String, String), String> {
    let (connection, _) = {
        let guard = session_registered_system_bus_cell().lock().await;
        guard
            .as_ref()
            .map(|(c, sid)| (c.clone(), *sid))
            .ok_or_else(|| {
                "session system bus not registered yet (is the session daemon running and connected to root?)"
                    .to_string()
            })?
    };

    let proxy = RootStateProxy::new(&connection)
        .await
        .map_err(|e| format!("root state proxy for persisted display mode: {e}"))?;
    let attachment = proxy
        .desired_display_attachment()
        .await
        .map_err(|e| format!("desired_display_attachment: {e}"))?;
    let layout = proxy
        .desired_display_layout()
        .await
        .map_err(|e| format!("desired_display_layout: {e}"))?;
    Ok((attachment, layout))
}

/// Push compositor-observed attachment/layout into root persistence (validates on root side).
pub async fn report_observed_display_mode_from_session(
    attachment: String,
    layout: String,
) -> Result<(), String> {
    let (connection, _) = {
        let guard = session_registered_system_bus_cell().lock().await;
        guard
            .as_ref()
            .map(|(c, sid)| (c.clone(), *sid))
            .ok_or_else(|| {
                "session system bus not registered yet (is the session daemon running and connected to root?)"
                    .to_string()
            })?
    };

    let proxy = RootStateProxy::new(&connection)
        .await
        .map_err(|e| format!("root state proxy for report_observed_display_mode: {e}"))?;
    proxy
        .report_observed_display_mode(attachment, layout)
        .await
        .map_err(|e| format!("report_observed_display_mode: {e}"))?;
    Ok(())
}

pub async fn resume_display_applies() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let connection = Connection::system().await?;
    let proxy = RootStateProxy::new(&connection).await?;
    proxy.resume_display_applies().await?;
    Ok(())
}

fn push_root_state_property<T: std::fmt::Display>(
    rows: &mut Vec<(String, String)>,
    key: &'static str,
    res: zbus::Result<T>,
) {
    match res {
        Ok(v) => rows.push((key.to_string(), v.to_string())),
        Err(e) => rows.push((key.to_string(), format!("(error: {e})"))),
    }
}

/// Read all [`RootState`] D-Bus properties for the CLI (`zenbook-duo-daemon state`).
///
/// Uses the same [`RootStateProxy`] as operator commands. If some properties show `(error: …
/// UnknownProperty…)`, the **running** root daemon is older than this binary — restart the root
/// service after installing a build that exports those members.
pub async fn query_root_state_for_cli() -> Result<Vec<(String, String)>, String> {
    let connection = Connection::system()
        .await
        .map_err(|e| format!("cannot connect to system D-Bus: {e}"))?;
    let proxy = RootStateProxy::new(&connection)
        .await
        .map_err(|e| format!("cannot attach to {}: {e}", ROOT_DBUS_SERVICE))?;

    let mut rows = Vec::new();
    push_root_state_property(&mut rows, "keyboard_attached", proxy.keyboard_attached().await);
    push_root_state_property(
        &mut rows,
        "bluetooth_connected",
        proxy.bluetooth_connected().await,
    );
    push_root_state_property(&mut rows, "desired_primary", proxy.desired_primary().await);
    push_root_state_property(
        &mut rows,
        "desired_secondary_enabled",
        proxy.desired_secondary_enabled().await,
    );
    push_root_state_property(
        &mut rows,
        "ambient_light_enabled",
        proxy.ambient_light_enabled().await,
    );
    push_root_state_property(
        &mut rows,
        "desired_display_attachment",
        proxy.desired_display_attachment().await,
    );
    push_root_state_property(
        &mut rows,
        "desired_display_layout",
        proxy.desired_display_layout().await,
    );
    push_root_state_property(&mut rows, "display_brightness", proxy.display_brightness().await);
    push_root_state_property(
        &mut rows,
        "display_apply_paused",
        proxy.display_apply_paused().await,
    );

    Ok(rows)
}

pub async fn run_operator_command(cmd: OperatorCommand) -> Result<(), String> {
    let connection = Connection::system()
        .await
        .map_err(|e| format!("system bus: {e}"))?;
    let proxy = RootStateProxy::new(&connection)
        .await
        .map_err(|e| format!("Connect to {}: {e}", ROOT_DBUS_SERVICE))?;

    match cmd {
        OperatorCommand::ToggleMicMuteLed => proxy.toggle_mic_mute_led().await,
        OperatorCommand::SetMicMuteLed(enabled) => proxy.set_mic_mute_led(enabled).await,
        OperatorCommand::ToggleKeyboardBacklight => proxy.toggle_keyboard_backlight().await,
        OperatorCommand::SetKeyboardBacklightLevel(level) => {
            proxy.set_keyboard_backlight_level(level).await
        }
        OperatorCommand::ToggleSecondaryDisplay => proxy.toggle_secondary_display().await,
        OperatorCommand::SetSecondaryDisplay(enabled) => {
            proxy.set_secondary_display_desired(enabled).await
        }
    }
    .map_err(|e| e.to_string())
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
    // ACK means "received by session daemon", not "fully applied".
    if let Err(e) = proxy.acknowledge_desired_secondary_applied(value).await {
        warn!("SESSION DBus: failed to acknowledge desired_secondary_enabled={value}: {e}");
    }

    // Keep draining internal completion ACKs for observability/backpressure, but do not
    // block root-facing ACK flow on full display application.
    let secondary_result_rx = Arc::clone(secondary_result_rx);
    tokio::spawn(async move {
        let _ = wait_for_internal_ack(
            &secondary_result_rx,
            Duration::from_secs(8),
            "desired secondary handler",
        )
        .await;
    });
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
            replace_session_registered_system_bus(None).await;
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
        // Publish immediately so display reconcile can use the guard before stream setup finishes;
        // keep the same `Connection` as `register_session` for matching bus unique name.
        replace_session_registered_system_bus(Some((connection.clone(), session_id))).await;

        if !coalesce_latest_ambient_value(&mut ambient_report_rx, &mut latest_ambient_value) {
            replace_session_registered_system_bus(None).await;
            return;
        }

        if let Some(value) = latest_ambient_value {
            if let Err(e) = proxy.report_ambient_light_enabled(value).await {
                warn!("SESSION DBus: failed to replay ambient_light_enabled={value} after registration: {e}");
                replace_session_registered_system_bus(None).await;
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
                replace_session_registered_system_bus(None).await;
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
                replace_session_registered_system_bus(None).await;
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

        replace_session_registered_system_bus(None).await;
        info!("SESSION DBus: reconnecting to root daemon");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
