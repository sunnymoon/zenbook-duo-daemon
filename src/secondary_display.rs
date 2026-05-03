use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::warn;
use tokio::fs;
use tokio::sync::broadcast;

use crate::config::Config;
use crate::events::Event;
use crate::state::KeyboardStateManager;

const ENFORCER_COOLDOWN_SECS: u64 = 8;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn control_secondary_display(status_path: &str, enable: bool, last_change: &Arc<AtomicU64>) {
    let data: &[u8] = if enable { b"on" } else { b"off" };
    last_change.store(now_secs(), Ordering::Relaxed);
    if let Err(e) = fs::write(status_path, data).await {
        warn!("Failed to control secondary display: {}", e);
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

/// Secondary display consumer - manages secondary display state and syncs with hardware
pub async fn start_secondary_display_task(
    config: Config,
    state_manager: KeyboardStateManager,
    mut event_receiver: broadcast::Receiver<Event>,
) {
    let status_path = config.secondary_display_status_path.clone();
    let last_change = Arc::new(AtomicU64::new(0));

    control_secondary_display(&status_path, state_manager.is_secondary_display_enabled(), &last_change).await;

    // Task to handle events
    {
        let status_path = status_path.clone();
        let last_change = last_change.clone();
        tokio::spawn(async move {
            loop {
                match event_receiver.recv().await {
                    Ok(Event::SecondaryDisplay(new_state)) => {
                        control_secondary_display(&status_path, new_state, &last_change).await;
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

    // Task to sync secondary display brightness
    {
        let source = config.primary_backlight_path.clone();
        let target = config.secondary_backlight_path.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                if let Ok(brightness) = fs::read_to_string(&source).await {
                    fs::write(&target, brightness.trim()).await.ok();
                }
            }
        });
    }
}
