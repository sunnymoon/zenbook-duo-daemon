use std::panic;
use std::time::Duration;
use std::{path::PathBuf, process, sync::Arc};
use std::sync::OnceLock;

use tokio::fs;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, broadcast};
use crate::{
    config::{Config, DEFAULT_CONFIG_PATH},
    events::Event,
    idle_detection::start_idle_detection_task,
    keyboard_usb::{find_wired_keyboard, start_usb_keyboard_monitor_task, start_usb_keyboard_task},
    mute_state::start_listen_mute_state_thread,
    secondary_display::start_secondary_display_task,
    state::{KeyboardBacklightState, KeyboardStateManager},
    unix_pipe::start_receive_commands_task,
    virtual_keyboard::VirtualKeyboard,
};
use clap::Parser;
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
}

mod config;
mod daemon_socket;
mod dwt;
mod events;
mod idle_detection;
mod keyboard_bt;
mod keyboard_usb;
mod mute_state;
mod secondary_display;
mod session;
mod state;
mod unix_pipe;
mod virtual_keyboard;

// Global daemon-message broadcast sender - initialized in run_daemon and used by config.rs
static STATE_BROADCAST: OnceLock<Mutex<Option<broadcast::Sender<daemon_socket::DaemonMsg>>>> = OnceLock::new();
static ACK_BROADCAST: OnceLock<Mutex<Option<broadcast::Sender<String>>>> = OnceLock::new();

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
    
    // Create state update broadcast channel (for desired_primary changes)
    let (state_update_tx, _) = broadcast::channel::<daemon_socket::DaemonMsg>(16);
    let (ack_tx, _) = broadcast::channel::<String>(16);
    
    // Store the broadcast sender globally so config.rs can access it
    let _ = STATE_BROADCAST.get_or_init(|| Mutex::new(Some(state_update_tx.clone())));
    let _ = ACK_BROADCAST.get_or_init(|| Mutex::new(Some(ack_tx.clone())));

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

    // Start bidirectional daemon socket server (root daemon listens for session daemon connections)
    let state_mgr = state_manager.clone();
    let update_tx = state_update_tx.clone();
    let ack_tx_clone = ack_tx.clone();
    tokio::spawn(async move {
        daemon_socket::start_server(&state_mgr, update_tx, ack_tx_clone).await;
    });

    start_secondary_display_task(
        config.clone(),
        state_manager.clone(),
        event_sender.subscribe(),
    )
    .await;

    start_bt_keyboard_monitor_task(
        &config,
        event_sender.clone(),
        virtual_keyboard.clone(),
        state_manager.clone(),
        activity_notifier.clone(),
    );

    // Software disable-while-typing for BT mode (needs root to grab /dev/input).
    let initial_usb_attached = current_usb_keyboard.is_some();
    start_usb_keyboard_monitor_task(
        &config,
        current_usb_keyboard,
        event_sender.clone(),
        virtual_keyboard.clone(),
        state_manager.clone(),
        activity_notifier.clone(),
    );

    start_listen_mute_state_thread(state_manager.clone());

    start_receive_commands_task(&config, state_manager.clone(), activity_notifier.clone(), event_sender.clone());

    dwt::start_task(initial_usb_attached, event_sender.clone());

    // Forward keyboard attachment state changes to the session daemon
    // Try to let session daemon handle it gracefully first, fall back to sysfs if needed
    {
        let mut event_rx = event_sender.subscribe();
        let state_manager_clone = state_manager.clone();
        tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(Event::KeyboardAttached(attached)) => {
                        info!("KeyboardAttached event: {}", attached);

                        let state_tx = if let Some(mutex) = STATE_BROADCAST.get() {
                            mutex.lock().await.clone()
                        } else {
                            None
                        };
                        let ack_tx = if let Some(mutex) = ACK_BROADCAST.get() {
                            mutex.lock().await.clone()
                        } else {
                            None
                        };

                        if let (Some(state_tx), Some(ack_tx)) = (state_tx, ack_tx) {
                            let expected_ack = format!("keyboard_attached_received:{attached}");
                            let mut ack_rx = ack_tx.subscribe();

                            if let Err(e) = state_tx.send(daemon_socket::DaemonMsg::KeyboardAttached { value: attached }) {
                                warn!("KeyboardAttached: failed to notify session daemon over daemon socket: {}", e);
                                secondary_display::arm_sysfs_fallback_once();
                                state_manager_clone.emit_secondary_display_state();
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
                                    info!("KeyboardAttached: session daemon acknowledged request");
                                } else {
                                    warn!("KeyboardAttached: no session daemon ack, falling back to sysfs");
                                    secondary_display::arm_sysfs_fallback_once();
                                    state_manager_clone.emit_secondary_display_state();
                                }
                            }
                        } else {
                            warn!("KeyboardAttached: daemon socket channels not initialized");
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
