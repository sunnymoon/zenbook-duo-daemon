use crate::events::Event;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};
use tokio::sync::broadcast;
use tokio::sync::Mutex as TokioMutex;

const STATE_DIR: &str = "/var/lib/zenbook-duo-daemon";
const BACKLIGHT_STATE_FILE: &str = "/var/lib/zenbook-duo-daemon/backlight";
const DISPLAY_STATE_FILE: &str = "/var/lib/zenbook-duo-daemon/display-state";

#[derive(Default, Serialize, Deserialize)]
struct PersistedDisplayState {
    desired_primary: Option<String>,
    desired_secondary_enabled: Option<bool>,
    display_brightness: Option<u32>,
    ambient_light_enabled: Option<bool>,
    /// `builtin_only` | `external_only` | `all_connected` (see `plan.md` §6).
    desired_display_attachment: Option<String>,
    /// `mirror` | `joined`
    desired_display_layout: Option<String>,
}

/// Serializes all display-state JSON read/modify/write so concurrent tasks cannot clobber fields.
fn display_state_mutex() -> &'static TokioMutex<()> {
    static LOCK: OnceLock<TokioMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| TokioMutex::new(()))
}

async fn read_display_state_file_inner() -> PersistedDisplayState {
    let path = Path::new(DISPLAY_STATE_FILE);
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return PersistedDisplayState::default();
    }

    match tokio::fs::read_to_string(path).await {
        Ok(content) => serde_json::from_str::<PersistedDisplayState>(&content).unwrap_or_default(),
        Err(_) => PersistedDisplayState::default(),
    }
}

async fn write_display_state_file_atomically(state: &PersistedDisplayState) {
    let _ = tokio::fs::create_dir_all(STATE_DIR).await;
    let Ok(json) = serde_json::to_string(state) else {
        return;
    };
    let tmp = format!("{}.tmp", DISPLAY_STATE_FILE);
    if tokio::fs::write(&tmp, &json).await.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return;
    }
    let _ = tokio::fs::rename(&tmp, DISPLAY_STATE_FILE).await;
}

async fn with_display_state<R>(f: impl FnOnce(&mut PersistedDisplayState) -> R) -> R {
    let _guard = display_state_mutex().lock().await;
    let mut state = read_display_state_file_inner().await;
    let out = f(&mut state);
    write_display_state_file_atomically(&state).await;
    out
}

async fn persist_backlight_disk(state: KeyboardBacklightState) {
    let value = match state {
        KeyboardBacklightState::Off => "0",
        KeyboardBacklightState::Low => "1",
        KeyboardBacklightState::Medium => "2",
        KeyboardBacklightState::High => "3",
    };
    let _ = tokio::fs::create_dir_all(STATE_DIR).await;
    let _ = tokio::fs::write(BACKLIGHT_STATE_FILE, value).await;
}

async fn load_backlight_disk() -> KeyboardBacklightState {
    let path = Path::new(BACKLIGHT_STATE_FILE);
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return KeyboardBacklightState::Low;
    }
    match tokio::fs::read_to_string(path).await {
        Ok(content) => match content.trim() {
            "0" => KeyboardBacklightState::Off,
            "1" => KeyboardBacklightState::Low,
            "2" => KeyboardBacklightState::Medium,
            "3" => KeyboardBacklightState::High,
            _ => KeyboardBacklightState::Low,
        },
        Err(_) => KeyboardBacklightState::Low,
    }
}

fn get_desired_display_mode_from_snapshot(snap: &PersistedDisplayState) -> (String, String) {
    (
        snap.desired_display_attachment
            .as_deref()
            .unwrap_or("builtin_only")
            .to_string(),
        snap.desired_display_layout
            .as_deref()
            .unwrap_or("joined")
            .to_string(),
    )
}

pub fn validate_desired_display_mode_strings(attachment: &str, layout: &str) -> Result<(), String> {
    match attachment {
        "builtin_only" | "external_only" | "all_connected" => {}
        other => return Err(format!("invalid desired_display_attachment: {other}")),
    }
    match layout {
        "mirror" | "joined" => {}
        other => return Err(format!("invalid desired_display_layout: {other}")),
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub enum KeyboardBacklightState {
    Off,
    Low,
    Medium,
    High,
}

impl KeyboardBacklightState {
    pub fn next(&self) -> Self {
        match self {
            Self::Off => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Off,
        }
    }
}

/// Inner state structure containing all keyboard state
struct InnerState {
    backlight: KeyboardBacklightState,
    mic_mute_led: bool,

    /// when suspended, both backlight and mic mute led are disabled
    is_suspended: bool,

    /// when idle, only backlight is disabled
    is_idle: bool,
    is_usb_attached: bool,
    bt_connected_count: usize,
    desired_secondary_display_enabled: bool,
    is_secondary_display_enabled: bool,
    ambient_light_enabled: bool,
    /// Mirrors persisted display-state JSON (`desired_primary`).
    desired_primary: Option<String>,
    desired_display_attachment: String,
    desired_display_layout: String,
    display_brightness: Option<u32>,
}

/// Shared state manager that maintains keyboard state across attach/detach cycles
#[derive(Clone)]
pub struct KeyboardStateManager {
    state: Arc<RwLock<InnerState>>,
    sender: broadcast::Sender<Event>,
}

impl KeyboardStateManager {
    pub async fn new(is_usb_attached: bool, sender: broadcast::Sender<Event>) -> Self {
        let snap = read_display_state_file_inner().await;
        let desired_secondary_display_enabled = snap.desired_secondary_enabled.unwrap_or(true);
        let ambient_light_enabled = snap.ambient_light_enabled.unwrap_or(false);
        let is_secondary_display_enabled = if is_usb_attached {
            false
        } else {
            desired_secondary_display_enabled
        };

        let (desired_display_attachment, desired_display_layout) = get_desired_display_mode_from_snapshot(&snap);

        if snap.ambient_light_enabled.is_none() {
            let enabled = ambient_light_enabled;
            with_display_state(|state| {
                state.ambient_light_enabled = Some(enabled);
            })
            .await;
        }

        Self {
            state: Arc::new(RwLock::new(InnerState {
                backlight: load_backlight_disk().await,
                mic_mute_led: false,
                is_suspended: false,
                is_idle: false,
                is_usb_attached,
                bt_connected_count: 0,
                desired_secondary_display_enabled,
                is_secondary_display_enabled,
                ambient_light_enabled,
                desired_primary: snap.desired_primary.clone(),
                desired_display_attachment,
                desired_display_layout,
                display_brightness: snap.display_brightness,
            })),
            sender,
        }
    }

    pub fn suspend_start(&self) {
        let mut state = self.state.write().unwrap();
        state.is_suspended = true;
        self.sender.send(Event::MicMuteLed(false)).ok();
        self.sender
            .send(Event::Backlight(KeyboardBacklightState::Off))
            .ok();
    }

    pub fn suspend_end(&self) {
        let mut state = self.state.write().unwrap();
        state.is_suspended = false;
        drop(state);
        self.sender
            .send(Event::MicMuteLed(self.get_mic_mute_led()))
            .ok();
        self.sender
            .send(Event::Backlight(self.get_keyboard_backlight()))
            .ok();
    }

    pub fn idle_start(&self) {
        let mut state = self.state.write().unwrap();
        state.is_idle = true;
        self.sender
            .send(Event::Backlight(KeyboardBacklightState::Off))
            .ok();
    }

    pub fn idle_end(&self) {
        let mut state = self.state.write().unwrap();
        state.is_idle = false;
        drop(state);
        self.sender
            .send(Event::Backlight(self.get_keyboard_backlight()))
            .ok();
    }

    pub fn set_mic_mute_led(&self, enabled: bool) {
        let mut state = self.state.write().unwrap();
        state.mic_mute_led = enabled;
        if !state.is_suspended {
            self.sender.send(Event::MicMuteLed(enabled)).ok();
        }
    }

    pub fn toggle_mic_mute_led(&self) {
        let mut state = self.state.write().unwrap();
        state.mic_mute_led = !state.mic_mute_led;
        if !state.is_suspended {
            self.sender.send(Event::MicMuteLed(state.mic_mute_led)).ok();
        }
    }

    pub fn get_mic_mute_led(&self) -> bool {
        let state = self.state.read().unwrap();
        if state.is_suspended {
            false
        } else {
            state.mic_mute_led
        }
    }

    pub async fn set_keyboard_backlight(&self, new_state: KeyboardBacklightState) {
        let should_emit = {
            let mut state = self.state.write().unwrap();
            state.backlight = new_state;
            !state.is_idle && !state.is_suspended
        };
        persist_backlight_disk(new_state).await;
        if should_emit {
            self.sender.send(Event::Backlight(new_state)).ok();
        }
    }

    pub async fn toggle_keyboard_backlight(&self) {
        let (next, should_emit) = {
            let mut state = self.state.write().unwrap();
            state.backlight = state.backlight.next();
            (state.backlight, !state.is_idle && !state.is_suspended)
        };
        persist_backlight_disk(next).await;
        if should_emit {
            self.sender.send(Event::Backlight(next)).ok();
        }
    }

    pub fn get_keyboard_backlight(&self) -> KeyboardBacklightState {
        let state = self.state.read().unwrap();
        if state.is_suspended || state.is_idle {
            KeyboardBacklightState::Off
        } else {
            state.backlight
        }
    }

    pub async fn set_secondary_display_tracked(&self, enabled: bool) {
        {
            let mut state = self.state.write().unwrap();
            state.desired_secondary_display_enabled = enabled;
            state.is_secondary_display_enabled = if state.is_usb_attached { false } else { enabled };
        }
        with_display_state(|state| {
            state.desired_secondary_enabled = Some(enabled);
        })
        .await;
    }

    pub fn set_usb_keyboard_attached(&self, attached: bool) {
        let mut state = self.state.write().unwrap();
        state.is_usb_attached = attached;
        state.is_secondary_display_enabled = if attached {
            false
        } else {
            state.desired_secondary_display_enabled
        };

        self.sender.send(Event::KeyboardAttached(attached)).ok();
        self.sender
            .send(Event::SecondaryDisplay(state.is_secondary_display_enabled))
            .ok();
    }

    pub fn is_secondary_display_enabled(&self) -> bool {
        let state = self.state.read().unwrap();
        state.is_secondary_display_enabled
    }

    pub fn is_usb_keyboard_attached(&self) -> bool {
        let state = self.state.read().unwrap();
        state.is_usb_attached
    }

    pub fn bluetooth_connection_started(&self) {
        let mut state = self.state.write().unwrap();
        let was_connected = state.bt_connected_count > 0;
        state.bt_connected_count = state.bt_connected_count.saturating_add(1);
        let is_connected = state.bt_connected_count > 0;
        drop(state);
        if was_connected != is_connected {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    if let Err(e) = crate::dbus_state::notify_bluetooth_connected_changed().await {
                        log::warn!("Failed to publish bluetooth_connected update over D-Bus: {}", e);
                    }
                });
            }
        }
    }

    pub fn bluetooth_connection_stopped(&self) {
        let mut state = self.state.write().unwrap();
        let was_connected = state.bt_connected_count > 0;
        state.bt_connected_count = state.bt_connected_count.saturating_sub(1);
        let is_connected = state.bt_connected_count > 0;
        drop(state);
        if was_connected != is_connected {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    if let Err(e) = crate::dbus_state::notify_bluetooth_connected_changed().await {
                        log::warn!("Failed to publish bluetooth_connected update over D-Bus: {}", e);
                    }
                });
            }
        }
    }

    pub fn is_bluetooth_connected(&self) -> bool {
        let state = self.state.read().unwrap();
        state.bt_connected_count > 0
    }

    pub fn is_secondary_display_desired_enabled(&self) -> bool {
        let state = self.state.read().unwrap();
        state.desired_secondary_display_enabled
    }

    pub fn emit_secondary_display_state(&self) {
        let state = self.state.read().unwrap();
        self.sender
            .send(Event::SecondaryDisplay(state.is_secondary_display_enabled))
            .ok();
    }

    pub async fn set_desired_primary(&self, primary: &str) {
        with_display_state(|state| {
            state.desired_primary = Some(primary.to_string());
        })
        .await;
        let mut state = self.state.write().unwrap();
        state.desired_primary = Some(primary.to_string());
    }

    pub fn get_desired_primary(&self) -> Option<String> {
        let state = self.state.read().unwrap();
        state.desired_primary.clone()
    }

    pub fn get_desired_display_mode(&self) -> (String, String) {
        let state = self.state.read().unwrap();
        (
            state.desired_display_attachment.clone(),
            state.desired_display_layout.clone(),
        )
    }

    pub async fn persist_desired_display_mode(&self, attachment: &str, layout: &str) {
        with_display_state(|state| {
            state.desired_display_attachment = Some(attachment.to_string());
            state.desired_display_layout = Some(layout.to_string());
        })
        .await;
        let mut state = self.state.write().unwrap();
        state.desired_display_attachment = attachment.to_string();
        state.desired_display_layout = layout.to_string();
    }

    pub async fn set_display_brightness_value(&self, brightness: u32) {
        let changed = with_display_state(|state| {
            if state.display_brightness == Some(brightness) {
                false
            } else {
                state.display_brightness = Some(brightness);
                true
            }
        })
        .await;
        if changed {
            {
                let mut st = self.state.write().unwrap();
                st.display_brightness = Some(brightness);
            }
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    if let Err(e) = crate::dbus_state::notify_display_brightness_changed().await {
                        log::warn!("Failed to publish display_brightness update over D-Bus: {}", e);
                    }
                });
            }
        }
    }

    pub fn get_display_brightness_value(&self) -> Option<u32> {
        let state = self.state.read().unwrap();
        state.display_brightness
    }

    pub async fn set_ambient_light_enabled(&self, enabled: bool) {
        {
            let mut state = self.state.write().unwrap();
            state.ambient_light_enabled = enabled;
        }
        with_display_state(|state| {
            state.ambient_light_enabled = Some(enabled);
        })
        .await;
    }

    pub fn get_ambient_light_enabled(&self) -> bool {
        let state = self.state.read().unwrap();
        state.ambient_light_enabled
    }
}
