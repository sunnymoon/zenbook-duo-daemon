use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use inotify::{Inotify, WatchMask};
use log::warn;
use tokio::fs;
use tokio::sync::broadcast;

use crate::config::Config;
use crate::events::Event;
use crate::state::KeyboardStateManager;

const ENFORCER_COOLDOWN_SECS: u64 = 8;
static FORCE_SYSFS_FALLBACK_ONCE: AtomicBool = AtomicBool::new(false);
static SECONDARY_STATUS_PATH: OnceLock<String> = OnceLock::new();
static BRIGHTNESS_SYNC_PAUSED_UNTIL: AtomicU64 = AtomicU64::new(0);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn brightness_sync_paused() -> bool {
    now_secs() < BRIGHTNESS_SYNC_PAUSED_UNTIL.load(Ordering::Relaxed)
}

pub fn pause_brightness_sync_for(duration: Duration) {
    let until = now_secs().saturating_add(duration.as_secs().max(1));
    BRIGHTNESS_SYNC_PAUSED_UNTIL.store(until, Ordering::Relaxed);
}

async fn control_secondary_display(status_path: &str, enable: bool, last_change: &Arc<AtomicU64>) {
    // Check current state first to avoid redundant sysfs writes (which cause flicker)
    let actual_enabled = is_secondary_display_enabled_actual(status_path).await;
    if actual_enabled == enable {
        // Already in the desired state, no need to write
        return;
    }
    
    let data: &[u8] = if enable { b"on" } else { b"off" };
    last_change.store(now_secs(), Ordering::Relaxed);
    if let Err(e) = fs::write(status_path, data).await {
        warn!("Failed to control secondary display: {}", e);
    }
}

pub fn arm_sysfs_fallback_once() {
    FORCE_SYSFS_FALLBACK_ONCE.store(true, Ordering::SeqCst);
}

/// Write sysfs `on` for the secondary display connector if it is not already on.
/// Must be called before notifying the session daemon so that GNOME can see the
/// connector when it applies the DisplayConfig change.
pub async fn ensure_secondary_display_on() {
    let Some(status_path) = SECONDARY_STATUS_PATH.get() else {
        warn!("ensure_secondary_display_on: status path not initialised yet");
        return;
    };
    let actual = is_secondary_display_enabled_actual(status_path).await;
    if actual {
        return; // already on, nothing to write
    }
    if let Err(e) = fs::write(status_path, b"on").await {
        warn!("ensure_secondary_display_on: failed to write sysfs: {e}");
    }
}

fn is_secondary_display_enabled_actual_blocking(status_path: &str) -> bool {
    if let Ok(contents) = std::fs::read_to_string(status_path) {
        let status = contents.trim();
        status == "on" || status == "connected"
    } else {
        false
    }
}

/// Check if the secondary display is currently enabled by reading its status
async fn is_secondary_display_enabled_actual(status_path: &str) -> bool {
    if let Ok(contents) = fs::read_to_string(status_path).await {
        let status = contents.trim();
        // Display is enabled if status is "on" or "connected" (when enabled)
        status == "on" || status == "connected"
    } else {
        false
    }
}

fn wait_for_secondary_display_state_blocking(status_path: String, enable: bool, cancel: Arc<AtomicBool>) -> bool {
    if is_secondary_display_enabled_actual_blocking(&status_path) == enable {
        return true;
    }

    let path = Path::new(&status_path);
    let Some(parent) = path.parent() else {
        return false;
    };
    let target_name = path.file_name().map(|name| name.to_os_string());

    let mut inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(_) => return false,
    };

    if inotify
        .watches()
        .add(
            parent,
            WatchMask::MODIFY
                | WatchMask::ATTRIB
                | WatchMask::CLOSE_WRITE
                | WatchMask::CREATE
                | WatchMask::MOVED_TO
                | WatchMask::MOVE_SELF
                | WatchMask::DELETE_SELF,
        )
        .is_err()
    {
        return false;
    }

    let mut buffer = [0u8; 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let events = match inotify.read_events_blocking(&mut buffer) {
            Ok(events) => events.collect::<Vec<_>>(),
            Err(_) => return false,
        };

        let saw_status_change = events.iter().any(|event| {
            target_name
                .as_ref()
                .map(|name| event.name.as_ref().map(|event_name| event_name == name).unwrap_or(true))
                .unwrap_or(true)
        });

        if saw_status_change && is_secondary_display_enabled_actual_blocking(&status_path) == enable {
            return true;
        }
    }
}

pub async fn wait_for_secondary_display_state(enable: bool, timeout: Duration) -> bool {
    let Some(status_path) = SECONDARY_STATUS_PATH.get().cloned() else {
        return false;
    };

    if is_secondary_display_enabled_actual(&status_path).await == enable {
        return true;
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_clone = cancel.clone();

    match tokio::time::timeout(
        timeout,
        tokio::task::spawn_blocking(move || wait_for_secondary_display_state_blocking(status_path, enable, cancel_clone)),
    )
    .await
    {
        Ok(Ok(result)) => result,
        _ => {
            cancel.store(true, Ordering::Relaxed);
            false
        }
    }
}

fn sync_secondary_brightness_once(
    source_path: &str,
    target_path: &str,
    status_path: &str,
    state_manager: &KeyboardStateManager,
) {
    let Some(trimmed) = persist_primary_brightness_value(source_path, state_manager) else {
        return;
    };

    if brightness_sync_paused() {
        return;
    }

    if !state_manager.is_secondary_display_desired_enabled() {
        return;
    }

    if !is_secondary_display_enabled_actual_blocking(status_path) {
        return;
    }

    if let Err(e) = std::fs::write(target_path, trimmed) {
        warn!("Failed to sync secondary display brightness: {}", e);
    }
}

fn persist_primary_brightness_value(
    source_path: &str,
    state_manager: &KeyboardStateManager,
) -> Option<String> {
    let brightness = std::fs::read_to_string(source_path).ok()?;
    let trimmed = brightness.trim();
    let value = trimmed.parse::<u32>().ok()?;
    state_manager.set_display_brightness_value(value);
    Some(trimmed.to_string())
}

fn apply_persisted_display_brightness(
    primary_path: &str,
    secondary_path: &str,
    state_manager: &KeyboardStateManager,
) {
    let Some(brightness) = state_manager.get_display_brightness_value() else {
        return;
    };

    for path in [primary_path, secondary_path] {
        if let Err(e) = std::fs::write(path, brightness.to_string()) {
            warn!("Failed to restore persisted display brightness on {}: {}", path, e);
        }
    }
}

async fn restore_secondary_brightness_after_enable(
    primary_path: String,
    secondary_path: String,
    state_manager: KeyboardStateManager,
) {
    let brightness = tokio::task::spawn_blocking({
        let primary_path = primary_path.clone();
        let state_manager = state_manager.clone();
        move || persist_primary_brightness_value(&primary_path, &state_manager)
    })
    .await
    .ok()
    .flatten();

    if !wait_for_secondary_display_state(true, Duration::from_secs(8)).await {
        return;
    }

    let Some(brightness) = brightness.or_else(|| {
        state_manager
            .get_display_brightness_value()
            .map(|value| value.to_string())
    }) else {
        return;
    };

    if let Err(e) = fs::write(&secondary_path, brightness.to_string()).await {
        warn!(
            "Failed to restore secondary display brightness after enable on {}: {}",
            secondary_path, e
        );
    }
}

fn watch_primary_brightness_blocking(
    source_path: String,
    target_path: String,
    status_path: String,
    state_manager: KeyboardStateManager,
) {
    let path = Path::new(&source_path);
    let Some(parent) = path.parent() else {
        return;
    };
    let target_name = path.file_name().map(|name| name.to_os_string());

    sync_secondary_brightness_once(&source_path, &target_path, &status_path, &state_manager);

    let mut inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(e) => {
            warn!("Failed to initialize brightness inotify watcher: {}", e);
            return;
        }
    };

    if let Err(e) = inotify.watches().add(
        parent,
        WatchMask::MODIFY
            | WatchMask::ATTRIB
            | WatchMask::CLOSE_WRITE
            | WatchMask::CREATE
            | WatchMask::MOVED_TO
            | WatchMask::MOVE_SELF
            | WatchMask::DELETE_SELF,
    ) {
        warn!("Failed to watch primary brightness path: {}", e);
        return;
    }

    let mut buffer = [0u8; 1024];
    loop {
        let events = match inotify.read_events_blocking(&mut buffer) {
            Ok(events) => events.collect::<Vec<_>>(),
            Err(e) => {
                warn!("Brightness watcher failed to read inotify events: {}", e);
                return;
            }
        };

        let saw_brightness_change = events.iter().any(|event| {
            target_name
                .as_ref()
                .map(|name| event.name.as_ref().map(|event_name| event_name == name).unwrap_or(true))
                .unwrap_or(true)
        });

        if saw_brightness_change {
            sync_secondary_brightness_once(&source_path, &target_path, &status_path, &state_manager);
        }
    }
}

/// Secondary display consumer - manages secondary display state and syncs with hardware
pub async fn start_secondary_display_task(
    config: Config,
    state_manager: KeyboardStateManager,
    mut event_receiver: broadcast::Receiver<Event>,
) {
    let status_path = config.secondary_display_status_path.clone();
    let _ = SECONDARY_STATUS_PATH.set(status_path.clone());
    let last_change = Arc::new(AtomicU64::new(0));

    control_secondary_display(&status_path, state_manager.is_secondary_display_enabled(), &last_change).await;
    apply_persisted_display_brightness(
        &config.primary_backlight_path,
        &config.secondary_backlight_path,
        &state_manager,
    );

    // Task to handle events
    {
        let status_path = status_path.clone();
        let last_change = last_change.clone();
        let primary_path = config.primary_backlight_path.clone();
        let secondary_path = config.secondary_backlight_path.clone();
        let state_manager = state_manager.clone();
        tokio::spawn(async move {
            loop {
                match event_receiver.recv().await {
                    Ok(Event::SecondaryDisplay(new_state)) => {
                        let forced_fallback = FORCE_SYSFS_FALLBACK_ONCE.swap(false, Ordering::SeqCst);
                        // For disable: session daemon owns GNOME logical state; skip sysfs unless
                        // no session is present or a forced fallback was requested.
                        // For enable: sysfs must always be written first (already done before the
                        // session was notified), but we still call control here to ensure it — the
                        // check inside means it's a no-op if already on.
                        if !new_state && crate::dbus_state::is_session_registered() && !forced_fallback {
                            warn!(
                                "Skipping sysfs secondary display disable because session daemon is connected; session path owns display state"
                            );
                            continue;
                        }
                        control_secondary_display(&status_path, new_state, &last_change).await;
                        if new_state {
                            tokio::spawn(restore_secondary_brightness_after_enable(
                                primary_path.clone(),
                                secondary_path.clone(),
                                state_manager.clone(),
                            ));
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        });
    }

    // Task to periodically verify and enforce secondary display state
    // For some reason the secondary display always get enabled when resuming from suspend.
    // A cooldown prevents the enforcer from fighting the kernel while it processes a state change.
    {
        let state_manager = state_manager.clone();
        let status_path = status_path.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                if crate::dbus_state::is_session_registered() {
                    continue;
                }
                if now_secs().saturating_sub(last_change.load(Ordering::Relaxed)) < ENFORCER_COOLDOWN_SECS {
                    continue;
                }
                let actual_enabled = is_secondary_display_enabled_actual(&status_path).await;
                let desired_enabled = state_manager.is_secondary_display_enabled();
                if actual_enabled != desired_enabled {
                    warn!(
                        "Secondary display is not in the desired state, actual: {}, desired: {}",
                        actual_enabled, desired_enabled
                    );
                    control_secondary_display(&status_path, desired_enabled, &last_change).await;
                }
            }
        });
    }

    // Task to sync secondary display brightness from primary brightness events.
    // Only mirror brightness while the secondary is desired and actually present.
    {
        let source = config.primary_backlight_path.clone();
        let target = config.secondary_backlight_path.clone();
        let status_path = status_path.clone();
        let state_manager = state_manager.clone();
        tokio::spawn(async move {
            loop {
                let source = source.clone();
                let target = target.clone();
                let status_path = status_path.clone();
                let state_manager = state_manager.clone();

                match tokio::task::spawn_blocking(move || {
                    watch_primary_brightness_blocking(source, target, status_path, state_manager)
                })
                .await
                {
                    Ok(()) => break,
                    Err(e) => {
                        warn!("Brightness watcher task failed: {}", e);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });
    }
}
