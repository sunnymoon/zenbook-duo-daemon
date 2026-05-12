use crate::events::Event;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};
use tokio::sync::broadcast;

const STATE_DIR: &str = "/var/lib/zenbook-duo-daemon";
const BACKLIGHT_STATE_FILE: &str = "/var/lib/zenbook-duo-daemon/backlight";
const DISPLAY_STATE_FILE: &str = "/var/lib/zenbook-duo-daemon/display-state";

#[derive(Default, Serialize, Deserialize)]
struct PersistedDisplayState {
    desired_primary: Option<String>,
    desired_secondary_enabled: Option<bool>,
    display_brightness: Option<u32>,
    ambient_light_enabled: Option<bool>,
    /// `builtin_only` | `external_only` | `all_connected` (see `plan.md` §8).
    desired_display_attachment: Option<String>,
    /// `mirror` | `joined`
    desired_display_layout: Option<String>,
}

/// Serializes all display-state JSON read/modify/write so concurrent tasks cannot clobber fields.
fn display_state_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

fn read_display_state_file_inner() -> PersistedDisplayState {
    let path = Path::new(DISPLAY_STATE_FILE);
    if !path.exists() {
        return PersistedDisplayState::default();
    }

    match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str::<PersistedDisplayState>(&content).unwrap_or_default(),
        Err(_) => PersistedDisplayState::default(),
    }
}

fn write_display_state_file_atomically(state: &PersistedDisplayState) {
    let _ = fs::create_dir_all(STATE_DIR);
    let Ok(json) = serde_json::to_string(state) else {
        return;
    };
    let tmp = format!("{}.tmp", DISPLAY_STATE_FILE);
    if fs::write(&tmp, &json).is_err() {
        let _ = fs::remove_file(&tmp);
        return;
    }
    let _ = fs::rename(&tmp, DISPLAY_STATE_FILE);
}

fn with_display_state<R>(f: impl FnOnce(&mut PersistedDisplayState) -> R) -> R {
    let _guard = display_state_lock().lock().unwrap();
    let mut state = read_display_state_file_inner();
    let out = f(&mut state);
    write_display_state_file_atomically(&state);
    out
}

fn persist_backlight(state: KeyboardBacklightState) {
    let value = match state {
        KeyboardBacklightState::Off => "0",
        KeyboardBacklightState::Low => "1",
        KeyboardBacklightState::Medium => "2",
        KeyboardBacklightState::High => "3",
    };
    let _ = fs::create_dir_all(STATE_DIR);
    let _ = fs::write(BACKLIGHT_STATE_FILE, value);
}

fn load_backlight() -> KeyboardBacklightState {
    let path = Path::new(BACKLIGHT_STATE_FILE);
    if !path.exists() {
        return KeyboardBacklightState::Low;
    }
    match fs::read_to_string(path).unwrap_or_default().trim() {
        "0" => KeyboardBacklightState::Off,
        "1" => KeyboardBacklightState::Low,
        "2" => KeyboardBacklightState::Medium,
        "3" => KeyboardBacklightState::High,
        _ => KeyboardBacklightState::Low,
    }
}

fn persist_desired_primary(primary: &str) {
    with_display_state(|state| {
        state.desired_primary = Some(primary.to_string());
    });
}

fn persist_desired_secondary(enabled: bool) {
    with_display_state(|state| {
        state.desired_secondary_enabled = Some(enabled);
    });
}

fn load_desired_primary() -> Option<String> {
    let _guard = display_state_lock().lock().unwrap();
    read_display_state_file_inner().desired_primary
}

fn load_display_brightness() -> Option<u32> {
    let _guard = display_state_lock().lock().unwrap();
    read_display_state_file_inner().display_brightness
}

fn persist_ambient_light_enabled(enabled: bool) {
    with_display_state(|state| {
        state.ambient_light_enabled = Some(enabled);
    });
}

fn load_display_snapshot() -> PersistedDisplayState {
    let _guard = display_state_lock().lock().unwrap();
    read_display_state_file_inner()
}

pub fn load_desired_display_mode() -> (String, String) {
    let snap = load_display_snapshot();
    (
        snap.desired_display_attachment
            .unwrap_or_else(|| "builtin_only".to_string()),
        snap.desired_display_layout
            .unwrap_or_else(|| "joined".to_string()),
    )
}

pub fn persist_desired_display_mode(attachment: &str, layout: &str) {
    with_display_state(|state| {
        state.desired_display_attachment = Some(attachment.to_string());
        state.desired_display_layout = Some(layout.to_string());
    });
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
}

/// Shared state manager that maintains keyboard state across attach/detach cycles
#[derive(Clone)]
pub struct KeyboardStateManager {
    state: Arc<RwLock<InnerState>>,
    sender: broadcast::Sender<Event>,
}

impl KeyboardStateManager {
    pub fn new(is_usb_attached: bool, sender: broadcast::Sender<Event>) -> Self {
        let snap = load_display_snapshot();
        let desired_secondary_display_enabled = snap.desired_secondary_enabled.unwrap_or(true);
        let ambient_light_enabled = snap.ambient_light_enabled.unwrap_or(false);
        let is_secondary_display_enabled = if is_usb_attached {
            false
        } else {
            desired_secondary_display_enabled
        };

        if snap.ambient_light_enabled.is_none() {
            persist_ambient_light_enabled(ambient_light_enabled);
        }

        Self {
            state: Arc::new(RwLock::new(InnerState {
                backlight: load_backlight(),
                mic_mute_led: false,
                is_suspended: false,
                is_idle: false,
                is_usb_attached,
                bt_connected_count: 0,
                desired_secondary_display_enabled,
                is_secondary_display_enabled,
                ambient_light_enabled,
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

    pub fn set_keyboard_backlight(&self, new_state: KeyboardBacklightState) {
        let should_emit = {
            let mut state = self.state.write().unwrap();
            state.backlight = new_state;
            !state.is_idle && !state.is_suspended
        };
        persist_backlight(new_state);
        if should_emit {
            self.sender.send(Event::Backlight(new_state)).ok();
        }
    }

    pub fn toggle_keyboard_backlight(&self) {
        let (next, should_emit) = {
            let mut state = self.state.write().unwrap();
            state.backlight = state.backlight.next();
            (state.backlight, !state.is_idle && !state.is_suspended)
        };
        persist_backlight(next);
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

    pub fn set_secondary_display(&self, enabled: bool) {
        let is_secondary_display_enabled = {
            let mut state = self.state.write().unwrap();
            state.desired_secondary_display_enabled = enabled;
            state.is_secondary_display_enabled = if state.is_usb_attached { false } else { enabled };
            state.is_secondary_display_enabled
        };
        persist_desired_secondary(enabled);

        self.sender
            .send(Event::SecondaryDisplay(is_secondary_display_enabled))
            .ok();
    }

    pub fn set_secondary_display_tracked(&self, enabled: bool) {
        {
            let mut state = self.state.write().unwrap();
            state.desired_secondary_display_enabled = enabled;
            state.is_secondary_display_enabled = if state.is_usb_attached { false } else { enabled };
        }
        persist_desired_secondary(enabled);
    }

    pub fn toggle_secondary_display(&self) {
        let (desired, is_secondary_display_enabled) = {
            let mut state = self.state.write().unwrap();
            state.desired_secondary_display_enabled = !state.desired_secondary_display_enabled;
            state.is_secondary_display_enabled = if state.is_usb_attached {
                false
            } else {
                state.desired_secondary_display_enabled
            };
            (
                state.desired_secondary_display_enabled,
                state.is_secondary_display_enabled,
            )
        };
        persist_desired_secondary(desired);

        self.sender
            .send(Event::SecondaryDisplay(is_secondary_display_enabled))
            .ok();
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

    pub fn set_desired_primary(&self, primary: &str) {
        persist_desired_primary(primary);
    }

    pub fn get_desired_primary(&self) -> Option<String> {
        load_desired_primary()
    }

    pub fn set_display_brightness_value(&self, brightness: u32) {
        let changed = {
            let _guard = display_state_lock().lock().unwrap();
            let mut s = read_display_state_file_inner();
            if s.display_brightness == Some(brightness) {
                false
            } else {
                s.display_brightness = Some(brightness);
                write_display_state_file_atomically(&s);
                true
            }
        };
        if changed {
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
        load_display_brightness()
    }

    pub fn set_ambient_light_enabled(&self, enabled: bool) {
        {
            let mut state = self.state.write().unwrap();
            state.ambient_light_enabled = enabled;
        }
        persist_ambient_light_enabled(enabled);
    }

    pub fn get_ambient_light_enabled(&self) -> bool {
        let state = self.state.read().unwrap();
        state.ambient_light_enabled
    }
}
