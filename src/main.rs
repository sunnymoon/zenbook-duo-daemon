use std::panic;
use std::time::Duration;
use std::{path::PathBuf, process, sync::Arc};
use tokio::fs;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, broadcast};
use crate::dbus_state::OperatorCommand;
use crate::{
    config::{Config, DEFAULT_CONFIG_PATH},
    events::Event,
    idle_detection::start_idle_detection_task,
    keyboard_usb::{find_wired_keyboard, start_usb_keyboard_monitor_task, start_usb_keyboard_task},
    mute_state::start_listen_mute_state_thread,
    secondary_display::start_secondary_display_task,
    state::KeyboardStateManager,
    virtual_keyboard::VirtualKeyboard,
};
use clap::{Parser, Subcommand, ValueEnum};
use keyboard_bt::start_bt_keyboard_monitor_task;
use log::{error, info, warn};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
enum Args {
    /// Run the root daemon (requires root privileges)
    Run {
        /// Path to the config file, defaults to /etc/zenbook-duo-daemon/config.toml
        #[arg(short, long)]
        config_path: Option<PathBuf>,
    },
    /// Run the session daemon (runs as the logged-in user, started by systemd user service)
    Session,
    /// Migrate config file - backs up old config and writes new default if read fails
    MigrateConfig {
        /// Path to the config file, defaults to /etc/zenbook-duo-daemon/config.toml
        #[arg(short, long)]
        config_path: Option<PathBuf>,
    },
    /// Resume display applies after root-side safety guard paused them (root only on bus)
    ResumeDisplayApplies,
    /// Print root daemon state from system D-Bus (`asus.zenbook.duo.State` properties).
    ///
    /// Uses the same property names as the running service. If a field shows `(error: UnknownProperty)`,
    /// restart `zenbook-duo-daemon.service` after installing a binary that exports that property.
    State,
    /// Operator command (D-Bus to running root daemon; requires `zenbook-duo` group or root)
    Control {
        #[command(subcommand)]
        cmd: ControlCmd,
    },
}

/// Backlight level for `control keyboard-backlight set`.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum BacklightLevelArg {
    Off,
    Low,
    Medium,
    High,
}

impl BacklightLevelArg {
    fn to_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
        }
    }
}

#[derive(Subcommand, Debug, Clone)]
enum ControlCmd {
    /// Toggle microphone-mute LED
    MicMuteLedToggle,
    /// Set microphone-mute LED on or off (`true` / `false`)
    MicMuteLed {
        #[arg(action = clap::ArgAction::Set, value_parser = clap::value_parser!(bool))]
        on: bool,
    },
    /// Cycle keyboard backlight
    KeyboardBacklightToggle,
    /// Set keyboard backlight level
    KeyboardBacklightSet {
        #[arg(value_enum)]
        level: BacklightLevelArg,
    },
    /// Toggle desired secondary display
    SecondaryDisplayToggle,
    /// Enable or disable desired secondary display (`true` / `false`)
    SecondaryDisplay {
        #[arg(action = clap::ArgAction::Set, value_parser = clap::value_parser!(bool))]
        on: bool,
    },
}

mod config;
mod dbus_state;
 mod events;
mod idle_detection;
mod keyboard_bt;
mod keyboard_usb;
mod mute_state;
mod secondary_display;
mod session;
mod state;
mod virtual_keyboard;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();

    let args = Args::parse();

    match args {
        Args::MigrateConfig { config_path } => {
            migrate_config(config_path.unwrap_or(PathBuf::from(DEFAULT_CONFIG_PATH))).await;
            return;
        }
        Args::Session => {
            session::run().await;
        }
        Args::Run { config_path } => {
            run_daemon(config_path.unwrap_or(PathBuf::from(DEFAULT_CONFIG_PATH))).await;
        }
        Args::ResumeDisplayApplies => {
            match dbus_state::resume_display_applies().await {
                Ok(()) => info!("Display apply guard resumed"),
                Err(e) => {
                    error!("Failed to resume display applies: {e}");
                    process::exit(1);
                }
            }
        }
        Args::State => {
            match dbus_state::query_root_state_for_cli().await {
                Ok(rows) => {
                    let width = rows
                        .iter()
                        .map(|(k, _)| k.len())
                        .max()
                        .unwrap_or(0);
                    for (k, v) in rows {
                        println!("{k:width$}  {v}", width = width);
                    }
                }
                Err(e) => {
                    error!("{e}");
                    process::exit(1);
                }
            }
        }
        Args::Control { cmd } => {
            let op = match cmd {
                ControlCmd::MicMuteLedToggle => OperatorCommand::ToggleMicMuteLed,
                ControlCmd::MicMuteLed { on } => OperatorCommand::SetMicMuteLed(on),
                ControlCmd::KeyboardBacklightToggle => OperatorCommand::ToggleKeyboardBacklight,
                ControlCmd::KeyboardBacklightSet { level } => {
                    OperatorCommand::SetKeyboardBacklightLevel(level.to_u8())
                }
                ControlCmd::SecondaryDisplayToggle => OperatorCommand::ToggleSecondaryDisplay,
                ControlCmd::SecondaryDisplay { on } => OperatorCommand::SetSecondaryDisplay(on),
            };
            if let Err(e) = dbus_state::run_operator_command(op).await {
                error!("{e}");
                process::exit(1);
            }
        }
    }
}

async fn migrate_config(config_path: PathBuf) {
    use log::{info, warn};

    // Try to read the config
    match Config::try_read(&config_path).await {
        Ok(_) => {
            info!("Config file is valid, no migration needed");
        }
        Err(e) => {
            warn!("Failed to read config file: {}", e);

            // Backup the old config file if it exists
            if fs::try_exists(&config_path).await.unwrap_or(false) {
                let backup_path = config_path.with_file_name(format!(
                    "{}.bak",
                    config_path.file_name().unwrap().to_string_lossy()
                ));
                fs::rename(&config_path, &backup_path).await.unwrap();
                info!(
                    "\x1b[31mBacked up old config to: {} because it was incompatible with the new version\x1b[0m",
                    backup_path.display()
                );
            }

            // Write new default config
            Config::write_default_config(&config_path).await;
            info!(
                "Created new default config file at: {}",
                config_path.display()
            );
        }
    }
}

async fn run_daemon(config_path: PathBuf) {
    let config = Config::read(&config_path).await;

    // Create event channel
    let (event_sender, _) = broadcast::channel::<Event>(64);
    
    // Create virtual keyboard
    let virtual_keyboard = Arc::new(Mutex::new(VirtualKeyboard::new(&config)));

    let (state_manager, activity_notifier, current_usb_keyboard) =
        if let Some(keyboard) = find_wired_keyboard(&config).await {
            let state_manager = KeyboardStateManager::new(true, event_sender.clone());
            let activity_notifier = start_idle_detection_task(&config, state_manager.clone());

            let current_usb_keyboard = start_usb_keyboard_task(
                &config,
                keyboard,
                event_sender.subscribe(),
                virtual_keyboard.clone(),
                state_manager.clone(),
                activity_notifier.clone(),
            )
            .await;
            (state_manager, activity_notifier, Some(current_usb_keyboard))
        } else {
            let state_manager = KeyboardStateManager::new(false, event_sender.clone());
            let activity_notifier = start_idle_detection_task(&config, state_manager.clone());

            (state_manager, activity_notifier, None)
        };

    start_secondary_display_task(
        config.clone(),
        state_manager.clone(),
        event_sender.subscribe(),
    )
    .await;

    if let Err(e) = dbus_state::start_root_service(
        state_manager.clone(),
        config.clone(),
        activity_notifier.clone(),
    )
    .await
    {
        error!("Failed to start root D-Bus service: {e}");
        process::exit(1);
    }

    start_bt_keyboard_monitor_task(
        &config,
        event_sender.clone(),
        virtual_keyboard.clone(),
        state_manager.clone(),
        activity_notifier.clone(),
    );

    start_usb_keyboard_monitor_task(
        &config,
        current_usb_keyboard,
        event_sender.clone(),
        virtual_keyboard.clone(),
        state_manager.clone(),
        activity_notifier.clone(),
    );

    start_listen_mute_state_thread(state_manager.clone());

    // Forward keyboard attachment state changes over D-Bus.
    // If no session is registered, or if it does not acknowledge in time, fall back to sysfs.
    {
        let mut event_rx = event_sender.subscribe();
        let state_manager_clone = state_manager.clone();
        tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(Event::KeyboardAttached(attached)) => {
                        info!("KeyboardAttached event: {}", attached);
                        secondary_display::pause_brightness_sync_for(Duration::from_secs(4));

                        // On detach, write sysfs `on` BEFORE telling the session daemon.
                        // The session daemon will ask GNOME to enable eDP-2 via DisplayConfig,
                        // but GNOME can only see the connector once sysfs status = "on".
                        if !attached {
                            secondary_display::ensure_secondary_display_on().await;
                        }

                        let session_registered = match dbus_state::notify_keyboard_attached_changed().await {
                            Ok(registered) => registered,
                            Err(e) => {
                                warn!("KeyboardAttached: failed to publish D-Bus state update: {e}");
                                false
                            }
                        };

                        if session_registered {
                            let acked = dbus_state::wait_for_keyboard_attached_ack(
                                attached,
                                Duration::from_secs(5),
                            )
                            .await;
                            if acked {
                                info!("KeyboardAttached: session daemon acknowledged request");
                            } else {
                                warn!("KeyboardAttached: no session daemon ack, falling back to sysfs");
                                secondary_display::arm_sysfs_fallback_once();
                                state_manager_clone.emit_secondary_display_state();
                            }
                        } else {
                            warn!("KeyboardAttached: no registered session daemon, falling back to sysfs");
                            secondary_display::arm_sysfs_fallback_once();
                            state_manager_clone.emit_secondary_display_state();
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                }
            }
        });
    }

    panic::set_hook(Box::new(|info| {
        error!("Thread panicked: {info}");
        process::exit(1);
    }));

    info!("Daemon started");

    // Gracefully shutdown
    let mut sigterm = signal(SignalKind::terminate()).unwrap();
    let mut sigint = signal(SignalKind::interrupt()).unwrap();
    tokio::select! {
        _ = sigterm.recv() => {
            info!("SIGTERM received, shutting down");
        }
        _ = sigint.recv() => {
            info!("SIGINT received, shutting down");
        }
    }
    state_manager.suspend_start();
    tokio::time::sleep(Duration::from_millis(500)).await;
    process::exit(0);
}

pub fn parse_hex_string(hex_string: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for i in (0..hex_string.len()).step_by(2) {
        bytes.push(u8::from_str_radix(&hex_string[i..i + 2], 16).unwrap());
    }
    bytes
}

pub fn execute_command(command: &str) {
    info!("Executing command: {}", command);
    let command = command.to_owned();
    tokio::spawn(async move {
        match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .output()
            .await
        {
            Ok(output) => {
                info!(
                    "Command '{}' exited with status {}.\nstdout:\n{}\nstderr:\n{}",
                    command,
                    output.status,
                    String::from_utf8_lossy(&output.stdout).trim(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Err(e) => {
                log::warn!("Failed to execute command '{}': {}", command, e);
            }
        }
    });
}
