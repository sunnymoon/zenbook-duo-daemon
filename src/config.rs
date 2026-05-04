use log::{info, warn};
use std::{path::PathBuf, sync::Arc};
use std::time::Duration;
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

                let state_tx = if let Some(mutex) = crate::STATE_BROADCAST.get() {
                    mutex.lock().await.clone()
                } else {
                    None
                };
                let ack_tx = if let Some(mutex) = crate::ACK_BROADCAST.get() {
                    mutex.lock().await.clone()
                } else {
                    None
                };

                if let (Some(state_tx), Some(ack_tx)) = (state_tx, ack_tx) {
                    let expected_ack = format!("toggle_secondary_received:{new_state}");
                    let mut ack_rx = ack_tx.subscribe();

                    if let Err(e) = state_tx.send(crate::daemon_socket::DaemonMsg::ToggleSecondaryDisplay { enable: new_state }) {
                        warn!("ToggleSecondaryDisplay: failed to notify session daemon over daemon socket: {}", e);
                        state_manager.toggle_secondary_display();
                    } else {
                        let acked = tokio::time::timeout(Duration::from_secs(5), async {
                            loop {
                                match ack_rx.recv().await {
                                    Ok(msg) if msg == expected_ack => break true,
                                    Ok(_) => continue,
                                    Err(_) => break false,
                                }
                            }
                        })
                        .await
                        .unwrap_or(false);

                        if acked {
                            info!("ToggleSecondaryDisplay: session daemon acknowledged request");
                            state_manager.set_secondary_display_tracked(new_state);
                        } else {
                            warn!("ToggleSecondaryDisplay: no session daemon ack, falling back to sysfs");
                            crate::secondary_display::arm_sysfs_fallback_once();
                            state_manager.toggle_secondary_display();
                        }
                    }
                } else {
                    warn!("ToggleSecondaryDisplay: daemon socket channels not initialized");
                    crate::secondary_display::arm_sysfs_fallback_once();
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
                
                // Desired primary is now persisted to state file.
                // Broadcast the update to daemon socket so session daemon gets notified immediately
                if let Some(mutex) = crate::STATE_BROADCAST.get() {
                    if let Some(tx) = mutex.lock().await.as_ref() {
                        if let Err(e) = tx.send(crate::daemon_socket::DaemonMsg::DesiredPrimaryUpdate {
                            value: new_desired.to_string(),
                        }) {
                            warn!("Failed to broadcast desired_primary update: {}", e);
                        } else {
                            info!("Broadcasted desired_primary={} to daemon socket", new_desired);
                        }
                    }
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
