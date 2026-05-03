use crate::events::Event;
use std::fs;
use std::path::Path;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;

const STATE_DIR: &str = "/var/lib/zenbook-duo-daemon";
const BACKLIGHT_STATE_FILE: &str = "/var/lib/zenbook-duo-daemon/backlight";
const DISPLAY_STATE_FILE: &str = "/var/lib/zenbook-duo-daemon/display-state";

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
    let json = serde_json::json!({"desired_primary": primary});
    let _ = fs::create_dir_all(STATE_DIR);
    let _ = fs::write(DISPLAY_STATE_FILE, json.to_string());
}

fn load_desired_primary() -> Option<String> {
    let path = Path::new(DISPLAY_STATE_FILE);
    if !path.exists() {
        return None;
    }
    match fs::read_to_string(path) {
        Ok(content) => {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                json.get("desired_primary")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        }
        Err(_) => None,
    }
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
    is_secondary_display_enabled: bool,
}

/// Shared state manager that maintains keyboard state across attach/detach cycles
#[derive(Clone)]
pub struct KeyboardStateManager {
    state: Arc<RwLock<InnerState>>,
    sender: broadcast::Sender<Event>,
}

impl KeyboardStateManager {
    pub fn new(is_usb_attached: bool, sender: broadcast::Sender<Event>) -> Self {
        Self {
            state: Arc::new(RwLock::new(InnerState {
                backlight: load_backlight(),
                mic_mute_led: false,
                is_suspended: false,
                is_idle: false,
                is_usb_attached,
                is_secondary_display_enabled: !is_usb_attached,
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
        state.is_secondary_display_enabled = enabled;

        if state.is_usb_attached {
            state.is_secondary_display_enabled = false;
        }

        self.sender
            .send(Event::SecondaryDisplay(state.is_secondary_display_enabled))
            .ok();
    }

    pub fn toggle_secondary_display(&self) {
        let mut state = self.state.write().unwrap();
        state.is_secondary_display_enabled = !state.is_secondary_display_enabled;

        if state.is_usb_attached {
            state.is_secondary_display_enabled = false;
        }

        self.sender
            .send(Event::SecondaryDisplay(state.is_secondary_display_enabled))
            .ok();
    }

    pub fn set_usb_keyboard_attached(&self, attached: bool) {
        let mut state = self.state.write().unwrap();
        state.is_usb_attached = attached;

        if attached {
            state.is_secondary_display_enabled = false;
        } else {
            state.is_secondary_display_enabled = true;
        }

        self.sender.send(Event::KeyboardAttached(attached)).ok();
        self.sender
            .send(Event::SecondaryDisplay(state.is_secondary_display_enabled))
            .ok();
    }

    pub fn is_secondary_display_enabled(&self) -> bool {
        let state = self.state.read().unwrap();
        state.is_secondary_display_enabled
    }

    pub fn set_desired_primary(&self, primary: &str) {
        persist_desired_primary(primary);
    }

    pub fn get_desired_primary(&self) -> Option<String> {
        load_desired_primary()
    }
}
