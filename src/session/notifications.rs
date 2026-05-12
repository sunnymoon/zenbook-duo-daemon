use std::collections::HashMap;

use futures::StreamExt as _;
use log::{error, warn};
use tokio::sync::broadcast;
use tokio::time::{interval_at, Duration, Instant, MissedTickBehavior};
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

fn urgency_hints(urgency: u8) -> HashMap<&'static str, Value<'static>> {
    let mut hints = transient_hints();
    hints.insert("urgency", Value::U8(urgency));
    hints
}

/// Persistent: stays in the notification area until dismissed.
fn persistent_urgency_hints(urgency: u8) -> HashMap<&'static str, Value<'static>> {
    let mut hints = HashMap::new();
    hints.insert("urgency", Value::U8(urgency));
    hints
}

fn bullet_lines(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|line| format!("• {line}"))
        .collect::<Vec<_>>()
        .join("\u{2028}")
}

/// Persistent notification — stays in GNOME's notification area until dismissed.
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
//
// The Freedesktop notification spec has **no** per-notification RGB colour.
// The levers we have: urgency (BYTE 0-2), icon name, and the `image-path` hint
// (GNOME Shell shows `image-path` as a large icon beside the text with the
// `app_icon` as a small badge).  We use `image-path` pointing at Adwaita
// battery-level symbolic icons to give each tier a visually distinct image.

const BATTERY_WARN_PCT_LOW: u8 = 20;
const BATTERY_WARN_PCT_VERY_LOW: u8 = 10;
const BATTERY_WARN_PCT_CRITICAL: u8 = 5;

/// Poll BlueZ `Percentage` as a fallback when property-change signals are missed.
const BATTERY_POLL_SECS: u64 = 60;

/// Tracks which battery warnings have been shown during the current discharge.
/// Lives in [`run_battery_monitor`] so it survives [`battery_monitor_inner`]
/// restarts (BT reconnect, transient errors).  A flag is only cleared when the
/// battery recovers **at or above** the corresponding threshold.
#[derive(Clone, Default, Debug)]
struct KeyboardBatteryWarn {
    shown_below_20: bool,
    shown_below_10: bool,
    shown_below_5: bool,
}

impl KeyboardBatteryWarn {
    fn clear_recovered(&mut self, pct: u8) {
        if pct >= BATTERY_WARN_PCT_LOW {
            self.shown_below_20 = false;
        }
        if pct >= BATTERY_WARN_PCT_VERY_LOW {
            self.shown_below_10 = false;
        }
        if pct >= BATTERY_WARN_PCT_CRITICAL {
            self.shown_below_5 = false;
        }
    }
}

/// Emit at most **one** battery toast per call, highest threshold first.
/// The 20 % and 10 % tiers use the exact same `transient_hints()` as the
/// working keyboard-attach/detach banners so GNOME renders them as popup toasts.
async fn maybe_emit_battery_warning(
    notif: &NotificationsProxy<'_>,
    pct: u8,
    w: &mut KeyboardBatteryWarn,
) {
    // Check most-critical first: showing "critical" implicitly covers the
    // less-severe tiers, so we mark them all as shown.

    // ── Below 5 % ── persistent tray entry, urgency critical, 120 s ──
    if pct < BATTERY_WARN_PCT_CRITICAL && !w.shown_below_5 {
        w.shown_below_5 = true;
        w.shown_below_10 = true;
        w.shown_below_20 = true;
        let mut hints = persistent_urgency_hints(2);
        hints.insert("image-path", Value::from("battery-level-0-symbolic"));
        let _ = notif
            .notify(
                "zenbook-duo-daemon",
                0,
                "dialog-error",
                "Keyboard battery critical!",
                &format!(
                    "Keyboard battery at <b>{pct}%</b>.\u{2028}\
                     The keyboard could become unavailable at any moment \
                     if not reattached to charge."
                ),
                vec![],
                hints,
                120_000,
            )
            .await;
        return;
    }

    // ── Below 10 % ── transient banner, 120 s ──
    if pct < BATTERY_WARN_PCT_VERY_LOW && !w.shown_below_10 {
        w.shown_below_10 = true;
        w.shown_below_20 = true;
        let mut hints = transient_hints();
        hints.insert("image-path", Value::from("battery-caution-symbolic"));
        let _ = notif
            .notify(
                "zenbook-duo-daemon",
                0,
                "dialog-warning",
                "Keyboard battery below 10%",
                &format!(
                    "Keyboard battery at <b>{pct}%</b>.\u{2028}\
                     Attach the keyboard to USB to charge soon."
                ),
                vec![],
                hints,
                120_000,
            )
            .await;
        return;
    }

    // ── Below 20 % ── transient banner, 30 s ──
    if pct < BATTERY_WARN_PCT_LOW && !w.shown_below_20 {
        w.shown_below_20 = true;
        let mut hints = transient_hints();
        hints.insert("image-path", Value::from("battery-level-20-symbolic"));
        let _ = notif
            .notify(
                "zenbook-duo-daemon",
                0,
                "dialog-warning",
                "Keyboard battery below 20%",
                &format!(
                    "Keyboard battery at <b>{pct}%</b>.\u{2028}\
                     Consider attaching the keyboard to USB to charge."
                ),
                vec![],
                hints,
                30_000,
            )
            .await;
    }
}

/// Process a battery percentage reading.
/// Returns `true` when `pct` is unchanged and the caller can skip further work.
async fn on_battery_pct_update(
    notif: &NotificationsProxy<'_>,
    pct: u8,
    last_pct: &mut u8,
    was_below_full: &mut bool,
    w: &mut KeyboardBatteryWarn,
) -> bool {
    if pct == *last_pct {
        return true;
    }
    *last_pct = pct;

    w.clear_recovered(pct);
    maybe_emit_battery_warning(notif, pct, w).await;

    if pct < 100 {
        *was_below_full = true;
    }
    if pct == 100 && *was_below_full {
        *was_below_full = false;
        let _ = notif
            .notify(
                "zenbook-duo-daemon",
                0,
                "battery-full-charged",
                "Keyboard battery full",
                "Keyboard battery is fully charged.\u{2028}\
                 You can detach it from USB.",
                vec![],
                transient_hints(),
                4_000,
            )
            .await;
    }
    false
}

/// Discover the BlueZ object path of the ASUS keyboard (the one with Battery1
/// AND a Device1 whose name contains "ASUS" or "Keyboard").
async fn find_keyboard_bluez_path(conn: &Connection) -> Option<String> {
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

pub async fn run_battery_monitor() {
    let mut bw = KeyboardBatteryWarn::default();
    loop {
        if let Err(e) = battery_monitor_inner(&mut bw).await {
            warn!("Battery monitor: {e}, retrying in 15s");
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

async fn battery_monitor_inner(
    bw: &mut KeyboardBatteryWarn,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let system_conn = Connection::system().await?;
    let session_conn = Connection::session().await?;
    let notif = NotificationsProxy::new(&session_conn).await?;

    let root_proxy = RootStateProxy::new(&system_conn).await.ok();

    let device_path = loop {
        if let Some(p) = find_keyboard_bluez_path(&system_conn).await {
            break p;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    };

    let battery_proxy = BlueZBatteryProxy::builder(&system_conn)
        .destination("org.bluez")?
        .path(device_path.as_str())?
        .build()
        .await?;

    let device_proxy = BlueZDeviceProxy::builder(&system_conn)
        .destination("org.bluez")?
        .path(device_path.as_str())?
        .build()
        .await?;

    // If the initial read fails, skip the initial evaluation entirely rather
    // than assuming 100 % (which would wrongly clear all warning flags).
    let mut last_pct: u8;
    let mut was_below_full: bool;
    match battery_proxy.percentage().await {
        Ok(pct) => {
            last_pct = pct;
            was_below_full = pct < 100;
            bw.clear_recovered(pct);
            maybe_emit_battery_warning(&notif, pct, bw).await;
        }
        Err(_) => {
            last_pct = 0;
            was_below_full = false;
        }
    }

    let mut battery_stream = battery_proxy.receive_percentage_changed().await;
    let mut connected_stream = device_proxy.receive_connected_changed().await;
    let mut pct_poll = interval_at(
        Instant::now() + Duration::from_secs(BATTERY_POLL_SECS),
        Duration::from_secs(BATTERY_POLL_SECS),
    );
    pct_poll.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            Some(change) = battery_stream.next() => {
                let pct = match change.get().await {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if on_battery_pct_update(&notif, pct, &mut last_pct, &mut was_below_full, bw).await {
                    continue;
                }
            }

            _ = pct_poll.tick() => {
                let pct = battery_proxy.percentage().await.unwrap_or(last_pct);
                let _ = on_battery_pct_update(&notif, pct, &mut last_pct, &mut was_below_full, bw).await;
            }

            Some(change) = connected_stream.next() => {
                let connected = match change.get().await {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if connected {
                    return Ok(());
                }

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

                return Err("keyboard BT disconnected".into());
            }
        }
    }
}
