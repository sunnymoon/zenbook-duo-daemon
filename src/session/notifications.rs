use std::collections::HashMap;

use futures::StreamExt as _;
use log::{error, warn};
use tokio::sync::broadcast;
use tokio::time::Duration;
use zbus::{Connection, proxy, zvariant::Value};
use zbus::fdo::ObjectManagerProxy;

// ── D-Bus proxies ─────────────────────────────────────────────────────────────

#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<&str>,
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

#[proxy(
    interface = "asus.zenbook.duo.State",
    default_service = "asus.zenbook.duo",
    default_path = "/asus/zenbook/duo/State"
)]
trait RootState {
    #[zbus(property)]
    fn bluetooth_connected(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn desired_primary(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn desired_secondary_enabled(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn keyboard_attached(&self) -> zbus::Result<bool>;
}

#[proxy(
    interface = "org.bluez.Battery1",
    default_service = "org.bluez"
)]
trait BlueZBattery {
    #[zbus(property)]
    fn percentage(&self) -> zbus::Result<u8>;
}

#[proxy(
    interface = "org.bluez.Device1",
    default_service = "org.bluez"
)]
trait BlueZDevice {
    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
}

// ── Notification helpers ──────────────────────────────────────────────────────

fn transient_hints() -> HashMap<&'static str, Value<'static>> {
    let mut hints = HashMap::new();
    hints.insert("transient", Value::from(true));
    hints.insert("resident", Value::from(false));
    hints
}

// Transient: shown briefly, not stored in the notification area.
fn urgency_hints(urgency: u8) -> HashMap<&'static str, Value<'static>> {
    let mut hints = transient_hints();
    hints.insert("urgency", Value::from(urgency));
    hints
}

// Persistent: stays in the notification area until dismissed.
fn persistent_urgency_hints(urgency: u8) -> HashMap<&'static str, Value<'static>> {
    let mut hints = HashMap::new();
    hints.insert("urgency", Value::from(urgency));
    hints
}

fn bullet_lines(lines: &[&str]) -> String {
    // GNOME Shell collapses '\n' but honours U+2028 (Line Separator) for
    // visual line breaks.  Only <b> and <i> inline markup is rendered;
    // structural tags (div, ul, li, br) are escaped and shown literally.
    lines
        .iter()
        .map(|line| format!("• {line}"))
        .collect::<Vec<_>>()
        .join("\u{2028}")
}

/// Persistent notification — stays in GNOME's notification area until dismissed.
/// Use only for unrecoverable failures that require user attention.
pub async fn send_notification(
    summary: &str,
    body: &str,
    icon: &str,
    expire_timeout: i32,
    urgency: u8,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let conn = Connection::session().await?;
    let notif = NotificationsProxy::new(&conn).await?;
    let _ = notif
        .notify(
            "zenbook-duo-daemon",
            0,
            icon,
            summary,
            body,
            vec![],
            persistent_urgency_hints(urgency),
            expire_timeout,
        )
        .await?;
    Ok(())
}

/// Transient notification — shown briefly, never stored in the notification area.
pub async fn send_transient_notification(
    summary: &str,
    body: &str,
    icon: &str,
    expire_timeout: i32,
    urgency: u8,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let conn = Connection::session().await?;
    let notif = NotificationsProxy::new(&conn).await?;
    let _ = notif
        .notify(
            "zenbook-duo-daemon",
            0,
            icon,
            summary,
            body,
            vec![],
            urgency_hints(urgency),
            expire_timeout,
        )
        .await?;
    Ok(())
}

// ── Keyboard attach/detach notification text ──────────────────────────────────

fn keyboard_notification_text(
    attached: bool,
    bluetooth_connected: bool,
    desired_primary: &str,
    desired_secondary_enabled: bool,
) -> (&'static str, String, &'static str) {
    if attached {
        // When attaching on USB, only mention the top-bar moving if eDP-2 was
        // actually the primary (meaning the bar truly needs to move to the top).
        let top_bar_moves = desired_primary == "eDP-2";

        let body = if desired_secondary_enabled {
            if top_bar_moves {
                bullet_lines(&[
                    "Keyboard attached on USB.",
                    "Top bar will move temporarily to the top display.",
                    "Secondary display will turn off.",
                ])
            } else {
                bullet_lines(&[
                    "Keyboard attached on USB.",
                    "Top display stays primary.",
                    "Secondary display will turn off.",
                ])
            }
        } else if top_bar_moves {
            bullet_lines(&[
                "Keyboard attached on USB.",
                "Top bar will move temporarily to the top display.",
                "Secondary display remains off.",
            ])
        } else {
            bullet_lines(&[
                "Keyboard attached on USB.",
                "Top display stays primary.",
                "Secondary display remains off.",
            ])
        };
        ("Keyboard attached (USB)", body, "input-keyboard")
    } else if bluetooth_connected {
        let body = if desired_secondary_enabled {
            if desired_primary == "eDP-2" {
                bullet_lines(&[
                    "Keyboard detached — Bluetooth connected.",
                    "Secondary display is turning on.",
                    "Top bar moving back to the secondary display.",
                ])
            } else {
                bullet_lines(&[
                    "Keyboard detached — Bluetooth connected.",
                    "Secondary display is turning on.",
                    "Top display stays primary.",
                ])
            }
        } else {
            bullet_lines(&[
                "Keyboard detached — Bluetooth connected.",
                "Secondary display is still configured to stay off.",
            ])
        };
        ("Keyboard detached (Bluetooth)", body, "input-keyboard-virtual")
    } else {
        // BT not yet connected (may still be reconnecting)
        let body = if desired_secondary_enabled {
            bullet_lines(&[
                "Keyboard detached — Bluetooth not connected.",
                "Secondary display is turning on.",
                "Keyboard unavailable until Bluetooth reconnects.",
            ])
        } else {
            bullet_lines(&[
                "Keyboard detached — Bluetooth not connected.",
                "Keyboard unavailable until Bluetooth reconnects.",
            ])
        };
        ("Keyboard detached (no Bluetooth)", body, "dialog-warning")
    }
}

// ── Keyboard attach/detach notification loop ──────────────────────────────────

pub async fn run(mut kb_rx: broadcast::Receiver<bool>) {
    let conn = match Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            error!("Notifications: failed to connect to session D-Bus: {e}");
            return;
        }
    };

    let notif = match NotificationsProxy::new(&conn).await {
        Ok(n) => n,
        Err(e) => {
            error!("Notifications: failed to create proxy: {e}");
            return;
        }
    };

    let system_conn = match Connection::system().await {
        Ok(c) => Some(c),
        Err(e) => {
            warn!("Notifications: failed to connect to system D-Bus for root state: {e}");
            None
        }
    };
    let root_proxy = if let Some(system_conn) = system_conn.as_ref() {
        match RootStateProxy::new(system_conn).await {
            Ok(proxy) => Some(proxy),
            Err(e) => {
                warn!("Notifications: failed to create root state proxy: {e}");
                None
            }
        }
    } else {
        None
    };

    let mut replaces_id = 0u32;
    let mut initialized = false;
    let mut last_attached: Option<bool> = None;

    loop {
        match kb_rx.recv().await {
            Ok(attached) => {
                if !initialized {
                    initialized = true;
                    last_attached = Some(attached);
                    continue;
                }

                if last_attached == Some(attached) {
                    continue;
                }
                last_attached = Some(attached);

                // When detaching, wait a few seconds for Bluetooth to reconnect
                // before we read its state — BT reconnection takes 1-3 seconds.
                if !attached {
                    tokio::time::sleep(Duration::from_millis(2500)).await;
                }

                let (summary, body, icon) = if let Some(proxy) = root_proxy.as_ref() {
                    let bluetooth_connected = proxy.bluetooth_connected().await.unwrap_or(false);
                    let desired_primary = proxy
                        .desired_primary()
                        .await
                        .unwrap_or_else(|_| "eDP-1".to_string());
                    let desired_secondary_enabled =
                        proxy.desired_secondary_enabled().await.unwrap_or(true);
                    keyboard_notification_text(
                        attached,
                        bluetooth_connected,
                        &desired_primary,
                        desired_secondary_enabled,
                    )
                } else if attached {
                    (
                        "Keyboard attached (USB)",
                        bullet_lines(&[
                            "Keyboard attached on USB.",
                            "Secondary display will turn off.",
                        ]),
                        "input-keyboard",
                    )
                } else {
                    (
                        "Keyboard detached",
                        bullet_lines(&[
                            "Keyboard detached.",
                            "Bluetooth state could not be confirmed.",
                        ]),
                        "dialog-warning",
                    )
                };

                match notif
                    .notify(
                        "zenbook-duo-daemon",
                        replaces_id,
                        icon,
                        summary,
                        &body,
                        vec![],
                        transient_hints(),
                        1200,
                    )
                    .await
                {
                    Ok(id) => replaces_id = id,
                    Err(e) => warn!("Failed to send notification: {e}"),
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

// ── Battery & BT-disconnect monitor ──────────────────────────────────────────

/// Threshold levels we warn at (percentage values, descending).
const WARN_LOW: u8 = 5;
const WARN_MEDIUM: u8 = 15;

/// Discover the BlueZ object path of the ASUS keyboard (the one with Battery1
/// AND a Device1 whose name contains "ASUS" or "Keyboard").
async fn find_keyboard_bluez_path(
    conn: &Connection,
) -> Option<String> {
    let manager = ObjectManagerProxy::builder(conn)
        .destination("org.bluez")
        .ok()?
        .path("/")
        .ok()?
        .build()
        .await
        .ok()?;

    let objects = manager.get_managed_objects().await.ok()?;

    for (path, ifaces) in &objects {
        if !ifaces.contains_key("org.bluez.Battery1") {
            continue;
        }
        // Check device name contains a keyboard identifier
        let is_keyboard = ifaces
            .get("org.bluez.Device1")
            .and_then(|props| props.get("Name"))
            .and_then(|v| v.downcast_ref::<zbus::zvariant::Str>().ok())
            .map(|name| {
                let n = name.as_str().to_uppercase();
                n.contains("ASUS") || n.contains("KEYBOARD")
            })
            .unwrap_or(false);

        if is_keyboard {
            return Some(path.to_string());
        }
    }
    None
}

/// Long-running task that monitors the Bluetooth keyboard battery level and
/// sends transient notifications when level crosses thresholds.
/// Also warns if the keyboard BT disconnects unexpectedly (battery dead / hard-off).
pub async fn run_battery_monitor() {
    loop {
        if let Err(e) = battery_monitor_inner().await {
            warn!("Battery monitor: {e}, retrying in 15s");
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

async fn battery_monitor_inner() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let system_conn = Connection::system().await?;
    let session_conn = Connection::session().await?;
    let notif = NotificationsProxy::new(&session_conn).await?;

    // Find root state proxy to check whether keyboard is USB-attached
    let root_proxy = RootStateProxy::new(&system_conn).await.ok();

    // Locate the keyboard in BlueZ — retry until found (BT may not be connected yet)
    let device_path = loop {
        if let Some(p) = find_keyboard_bluez_path(&system_conn).await {
            break p;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    };

    // Subscribe to Battery1 property changes for this device
    let battery_proxy = BlueZBatteryProxy::builder(&system_conn)
        .destination("org.bluez")?
        .path(device_path.as_str())?
        .build()
        .await?;

    // Subscribe to Device1 property changes to detect disconnect
    let device_proxy = BlueZDeviceProxy::builder(&system_conn)
        .destination("org.bluez")?
        .path(device_path.as_str())?
        .build()
        .await?;

    // Read initial battery level
    let initial_pct = battery_proxy.percentage().await.unwrap_or(100);

    let mut last_pct = initial_pct;
    let mut warned_low = initial_pct <= WARN_LOW;
    let mut warned_medium = initial_pct <= WARN_MEDIUM;
    let mut was_below_full = initial_pct < 100;
    let mut battery_stream = battery_proxy.receive_percentage_changed().await;
    let mut connected_stream = device_proxy.receive_connected_changed().await;

    loop {
        tokio::select! {
            Some(change) = battery_stream.next() => {
                let pct = match change.get().await {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if pct == last_pct {
                    continue;
                }
                last_pct = pct;

                // Rising: reset warning flags when battery climbs back up
                if pct > WARN_MEDIUM + 5 {
                    warned_medium = false;
                }
                if pct > WARN_LOW + 5 {
                    warned_low = false;
                }
                if pct < 100 {
                    was_below_full = true;
                }

                // Falling: fire warnings at thresholds
                if pct == 0 {
                    // Dead — persistent critical, requires user action
                    let _ = notif
                        .notify(
                            "zenbook-duo-daemon",
                            0,
                            "battery-empty",
                            "Keyboard battery dead",
                            "• Keyboard battery is at <b>0%</b>.\u{2028}• Attach keyboard to USB immediately to recharge.",
                            vec![],
                            persistent_urgency_hints(2), // critical + persistent
                            0, // stays until dismissed
                        )
                        .await;
                } else if pct <= WARN_LOW && !warned_low {
                    warned_low = true;
                    let body = bullet_lines(&[
                        &format!("Keyboard battery at <b>{pct}%</b> — critically low."),
                        "Attach keyboard to USB to charge.",
                    ]);
                    let _ = notif
                        .notify(
                            "zenbook-duo-daemon",
                            0,
                            "battery-caution",
                            "Keyboard battery critical",
                            &body,
                            vec![],
                            urgency_hints(2), // urgent, transient
                            120_000, // 2 minutes
                        )
                        .await;
                } else if pct <= WARN_MEDIUM && !warned_medium {
                    warned_medium = true;
                    let body = bullet_lines(&[
                        &format!("Keyboard battery at <b>{pct}%</b> — getting low."),
                        "Consider attaching keyboard to USB to charge.",
                    ]);
                    let _ = notif
                        .notify(
                            "zenbook-duo-daemon",
                            0,
                            "battery-low",
                            "Keyboard battery low",
                            &body,
                            vec![],
                            urgency_hints(1), // normal
                            30_000, // 30 seconds
                        )
                        .await;
                } else if pct == 100 && was_below_full {
                    was_below_full = false;
                    let body = bullet_lines(&[
                        "Keyboard battery is fully charged.",
                        "You can detach it from USB.",
                    ]);
                    let _ = notif
                        .notify(
                            "zenbook-duo-daemon",
                            0,
                            "battery-full-charged",
                            "Keyboard battery full",
                            &body,
                            vec![],
                            transient_hints(),
                            4000,
                        )
                        .await;
                }
            }

            Some(change) = connected_stream.next() => {
                let connected = match change.get().await {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if connected {
                    // BT reconnected: stop this inner loop so we re-subscribe
                    // with a fresh state (avoids stale threshold flags).
                    return Ok(());
                }

                // BT disconnected — only warn if keyboard is also not USB-attached.
                let usb_attached = root_proxy
                    .as_ref()
                    .and_then(|p| {
                        tokio::task::block_in_place(|| {
                            tokio::runtime::Handle::current()
                                .block_on(p.keyboard_attached())
                                .ok()
                        })
                    })
                    .unwrap_or(false);

                if !usb_attached {
                    let body = bullet_lines(&[
                        "Keyboard Bluetooth has disconnected.",
                        "The keyboard may have run out of battery or been switched off.",
                        "Attach it via USB to check or recharge.",
                    ]);
                    let _ = notif
                        .notify(
                            "zenbook-duo-daemon",
                            0,
                            "dialog-warning",
                            "Keyboard disconnected",
                            &body,
                            vec![],
                            urgency_hints(1),
                            6000,
                        )
                        .await;
                }

                // Device disconnected: break inner loop, outer will retry
                return Err("keyboard BT disconnected".into());
            }
        }
    }
}
