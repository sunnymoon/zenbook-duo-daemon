use log::{debug, info, warn};
use std::{path::PathBuf, sync::Arc};
use tokio::fs;
use tokio::sync::Mutex;

use evdev_rs::enums::EV_KEY;
use serde::{Deserialize, Serialize};

use crate::state::KeyboardStateManager;

// All the enum carries a value so the serialized toml looks better
#[derive(Serialize, Deserialize, Clone)]
pub enum KeyFunction {
    KeyboardBacklight(bool),
    ToggleSecondaryDisplay(bool),
    SwapDisplays(bool),
    KeyBind(Vec<EV_KEY>),
    Command(String),
    NoOp(bool),
}

impl KeyFunction {
    /// Execute a key function - handles KeyBind, Command, KeyboardBacklight, and ToggleSecondaryDisplay
    pub async fn execute(
        &self,
        virtual_keyboard: &Arc<Mutex<crate::virtual_keyboard::VirtualKeyboard>>,
        state_manager: &KeyboardStateManager,
    ) {
        match self {
            KeyFunction::KeyBind(items) => {
                virtual_keyboard
                    .lock()
                    .await
                    .release_prev_and_press_keys(items);
            }
            KeyFunction::Command(command) => {
                crate::execute_command(command);
            }
            KeyFunction::KeyboardBacklight(true) => {
                state_manager.toggle_keyboard_backlight();
            }
            KeyFunction::ToggleSecondaryDisplay(true) => {
                let new_state = !state_manager.is_secondary_display_enabled();
                info!("KeyFunction: executing ToggleSecondaryDisplay -> {}", new_state);
                
                let response = crate::session_client::try_send_with_response(
                    &crate::session_client::SessionCmd::ToggleSecondaryDisplay { enable: new_state },
                ).await;
                
                if response.starts_with("ok") {
                    info!("ToggleSecondaryDisplay: session daemon handled it");
                    state_manager.toggle_secondary_display();
                } else {
                    warn!("ToggleSecondaryDisplay: session daemon not available or error, falling back to sysfs");
                    state_manager.toggle_secondary_display();
                }
            }
            KeyFunction::SwapDisplays(true) => {
                info!("KeyFunction: executing SwapDisplays");
                
                // Read current desired state to determine what we want
                let current_desired = state_manager.get_desired_primary();
                let new_desired = if current_desired.as_deref() == Some("eDP-2") {
                    "eDP-1"
                } else {
                    "eDP-2"
                };
                
                // Save the intended primary BEFORE attempting swap (independent of success)
                state_manager.set_desired_primary(new_desired);
                info!("User intention: swap to {}", new_desired);
                
                // Now try to apply it via session daemon
                let response = crate::session_client::try_send_with_response(
                    &crate::session_client::SessionCmd::SwapDisplays,
                ).await;
                
                if response.starts_with("ok:") {
                    info!("SwapDisplays applied successfully");
                } else {
                    warn!("SwapDisplays not yet applied: {}, will retry on keyboard/display events", response);
                }
            }
            _ => {
                // do nothing
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    usb_vendor_id: String,
    usb_product_id: String,
    pub fn_lock: bool,
    pub keyboard_backlight_key: KeyFunction,
    pub brightness_down_key: KeyFunction,
    pub brightness_up_key: KeyFunction,
    pub swap_up_down_display_key: KeyFunction,
    pub microphone_mute_key: KeyFunction,
    pub emoji_picker_key: KeyFunction,
    pub myasus_key: KeyFunction,
    pub toggle_secondary_display_key: KeyFunction,
    pub secondary_display_status_path: String,
    pub primary_backlight_path: String,
    pub secondary_backlight_path: String,
    pub pipe_path: String,
    /// Idle timeout in seconds. Set to 0 to disable idle detection.
    pub idle_timeout_seconds: u64,
}

impl Config {
    pub fn vendor_id(&self) -> u16 {
        u16::from_str_radix(&self.usb_vendor_id, 16).unwrap()
    }

    pub fn product_id(&self) -> u16 {
        u16::from_str_radix(&self.usb_product_id, 16).unwrap()
    }
}

fn get_usb_product_id() -> String {
    let board_name = std::fs::read_to_string("/sys/class/dmi/id/board_name")
        .unwrap_or_default()
        .trim()
        .to_string();
    if board_name == "UX8406CA" {
        info!("Detected Zenbook Duo 2025");
        "1bf2".to_string()
    } else if board_name == "UX8406MA" {
        info!("Detected Zenbook Duo 2024");
        "1b2c".to_string()
    } else {
        warn!(
            "Unknown board name: {}, using default product id 1b2c",
            board_name
        );
        "1b2c".to_string()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            usb_vendor_id: "0b05".to_string(),
            usb_product_id: get_usb_product_id(),
            fn_lock: true,
            keyboard_backlight_key: KeyFunction::KeyboardBacklight(true),
            brightness_down_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_BRIGHTNESSDOWN]),
            brightness_up_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_BRIGHTNESSUP]),
            swap_up_down_display_key: KeyFunction::SwapDisplays(true),
            microphone_mute_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_MICMUTE]),
            emoji_picker_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_LEFTCTRL, EV_KEY::KEY_DOT]),
            myasus_key: KeyFunction::NoOp(true),
            toggle_secondary_display_key: KeyFunction::ToggleSecondaryDisplay(true),
            secondary_display_status_path: "/sys/class/drm/card1-eDP-2/status".to_string(),
            primary_backlight_path: "/sys/class/backlight/intel_backlight/brightness".to_string(),
            secondary_backlight_path: "/sys/class/backlight/card1-eDP-2-backlight/brightness"
                .to_string(),
            pipe_path: "/tmp/zenbook-duo-daemon.pipe".to_string(),
            idle_timeout_seconds: 300, // 5 minutes
        }
    }
}

pub const DEFAULT_CONFIG_PATH: &str = "/etc/zenbook-duo-daemon/config.toml";

impl Config {
    pub async fn write_default_config(config_path: &PathBuf) {
        let config = Config::default();
        let config_str = toml::to_string(&config).unwrap();
        let help = "
# # Example Configuration:
#
# [keyboard_backlight_key]                  # This specifies the physical key to configure
# # Only one of the following values is allowed:
# KeyBind = [\"KEY_LEFTCTRL\", \"KEY_F10\"]     # Maps the physical key to left ctrl + f10, a list of all the keys can be found in https://docs.rs/evdev-rs/0.6.3/evdev_rs/enums/enum.EV_KEY.html
# Command = \"echo 'Hello, world!'\"          # Runs a custom command as root when the physical key is pressed
# KeyboardBacklight = true                  # Toggles the keyboard backlight
# ToggleSecondaryDisplay = true             # Toggles the secondary display
# NoOp = true                               # Does nothing when the physical key is pressed
#
# fn_lock = true             # To input F1-F12, you need to press Fn + F1-F12
# idle_timeout_seconds = 300 # 5 minutes, set to 0 to disable idle detection
        ".trim();
        let config_str = format!("{}\n\n\n{}", help, config_str);

        let parent = config_path.parent().unwrap();
        if !fs::try_exists(parent).await.unwrap_or(false) {
            fs::create_dir_all(parent).await.unwrap();
        }
        fs::write(config_path, config_str).await.unwrap();
    }

    /// Try to read config file, returns error if read or parse fails
    pub async fn try_read(config_path: &PathBuf) -> Result<Config, String> {
        let config_str = fs::read_to_string(config_path)
            .await
            .map_err(|e| format!("Failed to read config file: {}", e))?;
        toml::from_str(&config_str).map_err(|e| format!("Failed to parse config file: {}", e))
    }

    /// Read config file, creating default if it doesn't exist
    pub async fn read(config_path: &PathBuf) -> Config {
        if !fs::try_exists(config_path).await.unwrap_or(false) {
            Self::write_default_config(config_path).await;
        }
        let config_str = fs::read_to_string(config_path).await.unwrap();
        toml::from_str(&config_str).unwrap()
    }
}

/// Persistent background task to sync desired_primary with session daemon
/// Only sends when session daemon reconnects (detected by new session ID)
/// No polling on desired_primary value - only reacts to reconnection
pub async fn sync_desired_primary_background(state_manager: &KeyboardStateManager) {
    let mut check_cooldown = std::time::Instant::now();
    
    loop {
        // Check for new session every 5 seconds (only when we detect it's new)
        if check_cooldown.elapsed() > std::time::Duration::from_secs(5) {
            debug!("Sync check: elapsed={:?}, checking for new session", check_cooldown.elapsed());
            if crate::session_client::is_new_session().await {
                // New session daemon detected - send desired_primary immediately
                if let Some(desired_primary) = state_manager.get_desired_primary() {
                    info!("New session daemon detected, sending desired_primary={}", desired_primary);
                    let response = crate::session_client::try_send_with_response(
                        &crate::session_client::SessionCmd::SetDesiredPrimary {
                            value: desired_primary.clone(),
                        },
                    ).await;
                    
                    if response.contains("ok") {
                        info!("Session daemon acknowledged desired_primary={}", desired_primary);
                    } else {
                        warn!("Session daemon did not acknowledge desired_primary");
                    }
                }
            } else {
                debug!("No new session detected");
            }
            check_cooldown = std::time::Instant::now();
        }
        
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}
