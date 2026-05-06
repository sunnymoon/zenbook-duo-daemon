use crate::events::Event;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::{Arc, RwLock};
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

fn load_display_state() -> PersistedDisplayState {
    let path = Path::new(DISPLAY_STATE_FILE);
    if !path.exists() {
        return PersistedDisplayState::default();
    }

    match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str::<PersistedDisplayState>(&content).unwrap_or_default(),
        Err(_) => PersistedDisplayState::default(),
    }
}

fn persist_display_state(state: &PersistedDisplayState) {
    let _ = fs::create_dir_all(STATE_DIR);
    if let Ok(json) = serde_json::to_string(state) {
        let _ = fs::write(DISPLAY_STATE_FILE, json);
    }
}

fn persist_desired_primary(primary: &str) {
    let mut state = load_display_state();
    state.desired_primary = Some(primary.to_string());
    persist_display_state(&state);
}

fn persist_desired_secondary(enabled: bool) {
    let mut state = load_display_state();
    state.desired_secondary_enabled = Some(enabled);
    persist_display_state(&state);
}

fn load_desired_primary() -> Option<String> {
    load_display_state().desired_primary
}

fn load_desired_secondary() -> Option<bool> {
    load_display_state().desired_secondary_enabled
}

fn persist_display_brightness(brightness: u32) {
    let mut state = load_display_state();
    state.display_brightness = Some(brightness);
    persist_display_state(&state);
}

fn load_display_brightness() -> Option<u32> {
    load_display_state().display_brightness
}

fn persist_ambient_light_enabled(enabled: bool) {
    let mut state = load_display_state();
    state.ambient_light_enabled = Some(enabled);
    persist_display_state(&state);
}

fn load_ambient_light_enabled() -> Option<bool> {
    load_display_state().ambient_light_enabled
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
        let desired_secondary_display_enabled =
            load_desired_secondary().unwrap_or(!is_usb_attached);
        let ambient_light_enabled = load_ambient_light_enabled().unwrap_or(false);
        let is_secondary_display_enabled = if is_usb_attached {
            false
        } else {
            desired_secondary_display_enabled
        };

        if load_ambient_light_enabled().is_none() {
            persist_ambient_light_enabled(ambient_light_enabled);
        }

        Self {
            state: Arc::new(RwLock::new(InnerState {
                backlight: load_backlight(),
                mic_mute_led: false,
                is_suspended: false,
                is_idle: false,
                is_usb_attached,
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
        let mut state = self.state.write().unwrap();
        state.backlight = new_state;
        persist_backlight(new_state);
        if !state.is_idle && !state.is_suspended {
            self.sender.send(Event::Backlight(new_state)).ok();
        }
    }

    pub fn toggle_keyboard_backlight(&self) {
        let mut state = self.state.write().unwrap();
        state.backlight = state.backlight.next();
        persist_backlight(state.backlight);
        if !state.is_idle && !state.is_suspended {
            self.sender.send(Event::Backlight(state.backlight)).ok();
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
        let mut state = self.state.write().unwrap();
        state.desired_secondary_display_enabled = enabled;
        state.is_secondary_display_enabled = if state.is_usb_attached { false } else { enabled };
        persist_desired_secondary(enabled);

        self.sender
            .send(Event::SecondaryDisplay(state.is_secondary_display_enabled))
            .ok();
    }

    pub fn set_secondary_display_tracked(&self, enabled: bool) {
        let mut state = self.state.write().unwrap();
        state.desired_secondary_display_enabled = enabled;
        state.is_secondary_display_enabled = if state.is_usb_attached { false } else { enabled };
        persist_desired_secondary(enabled);
    }

    pub fn toggle_secondary_display(&self) {
        let mut state = self.state.write().unwrap();
        state.desired_secondary_display_enabled = !state.desired_secondary_display_enabled;
        state.is_secondary_display_enabled = if state.is_usb_attached {
            false
        } else {
            state.desired_secondary_display_enabled
        };
        persist_desired_secondary(state.desired_secondary_display_enabled);

        self.sender
            .send(Event::SecondaryDisplay(state.is_secondary_display_enabled))
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
        persist_display_brightness(brightness);
    }

    pub fn get_display_brightness_value(&self) -> Option<u32> {
        load_display_brightness()
    }

    pub fn set_ambient_light_enabled(&self, enabled: bool) {
        let mut state = self.state.write().unwrap();
        state.ambient_light_enabled = enabled;
        persist_ambient_light_enabled(enabled);
    }

    pub fn get_ambient_light_enabled(&self) -> bool {
        let state = self.state.read().unwrap();
        state.ambient_light_enabled
    }
}
