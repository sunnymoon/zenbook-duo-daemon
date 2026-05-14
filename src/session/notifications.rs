use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

use futures::StreamExt as _;
use log::{error, warn};
use tokio::sync::broadcast;
use tokio::sync::Mutex;
use tokio::time::{interval_at, Duration, Instant, MissedTickBehavior};
use zbus::fdo::ObjectManagerProxy;
use zbus::{Connection, proxy, zvariant::Value};

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
        hints: HashMap<&'static str, Value<'static>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    /// Remove a notification from the server; required before a **transient** toast when the
    /// prior entry was **persistent** — GNOME does not reliably downgrade in-place `replaces_id`.
    fn close_notification(&self, id: u32) -> zbus::Result<()>;
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

// ── Shared replace-id chains (keyboard + battery + BT disconnect vs display recovery) ──

/// Session-local state: one `replaces_id` stream for keyboard/battery/BT-disconnect toasts.
struct KeyboardDisplayNotifState {
    replaces_id: u32,
    last_battery_pct: Option<u8>,
    had_battery_tier_while_usb_detached: bool,
}

static KBD_DISPLAY_NOTIF: OnceLock<Arc<Mutex<KeyboardDisplayNotifState>>> = OnceLock::new();

fn keyboard_display_notif_state() -> Arc<Mutex<KeyboardDisplayNotifState>> {
    KBD_DISPLAY_NOTIF
        .get_or_init(|| {
            Arc::new(Mutex::new(KeyboardDisplayNotifState {
                replaces_id: 0,
                last_battery_pct: None,
                had_battery_tier_while_usb_detached: false,
            }))
        })
        .clone()
}

struct DisplayRecoveryNotifState {
    replaces_id: u32,
}

static DISPLAY_RECOVERY_NOTIF: OnceLock<Arc<Mutex<DisplayRecoveryNotifState>>> = OnceLock::new();

fn display_recovery_notif_state() -> Arc<Mutex<DisplayRecoveryNotifState>> {
    DISPLAY_RECOVERY_NOTIF
        .get_or_init(|| Arc::new(Mutex::new(DisplayRecoveryNotifState { replaces_id: 0 })))
        .clone()
}

/// Clears the display-recovery `replaces_id` chain after a successful recovery campaign.
pub(crate) async fn reset_display_recovery_notif_chain() {
    let recovery = display_recovery_notif_state();
    let mut st = recovery.lock().await;
    st.replaces_id = 0;
}

async fn notify_keyboard_display_chain(
    proxy: &NotificationsProxy<'_>,
    icon: &str,
    summary: &str,
    body: &str,
    hints: HashMap<&'static str, Value<'static>>,
    expire_timeout: i32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let rid = {
        let chain = keyboard_display_notif_state();
        let st = chain.lock().await;
        st.replaces_id
    };
    let id = proxy
        .notify(
            "zenbook-duo-daemon",
            rid,
            icon,
            summary,
            body,
            vec![],
            hints,
            expire_timeout,
        )
        .await?;
    {
        let chain = keyboard_display_notif_state();
        let mut st = chain.lock().await;
        st.replaces_id = id;
    }
    Ok(())
}

/// Transient toast: **close** any tracked notification first, then `Notify` with `replaces_id` 0.
/// GNOME Shell keeps persistent/tray behaviour when a sticky notification is only *replaced* by a
/// transient one; closing first avoids that.
async fn notify_keyboard_display_transient_fresh(
    proxy: &NotificationsProxy<'_>,
    icon: &str,
    summary: &str,
    body: &str,
    hints: HashMap<&'static str, Value<'static>>,
    expire_timeout: i32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let old_id = {
        let chain = keyboard_display_notif_state();
        let st = chain.lock().await;
        st.replaces_id
    };
    if old_id != 0 {
        if let Err(e) = proxy.close_notification(old_id).await {
            warn!("CloseNotification({old_id}) before transient toast: {e}");
        }
    }
    let id = proxy
        .notify(
            "zenbook-duo-daemon",
            0,
            icon,
            summary,
            body,
            vec![],
            hints,
            expire_timeout,
        )
        .await?;
    {
        let chain = keyboard_display_notif_state();
        let mut st = chain.lock().await;
        st.replaces_id = id;
    }
    Ok(())
}

/// Display recovery campaign: each retry replaces the previous; final failure is critical + persistent.
pub async fn send_display_recovery_transient_chained(
    summary: &str,
    body: &str,
    icon: &str,
    expire_timeout: i32,
    urgency: u8,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let conn = Connection::session().await?;
    let proxy = NotificationsProxy::new(&conn).await?;
    let mut hints = transient_hints();
    hints.insert("urgency", Value::U8(urgency));
    let rid = {
        let recovery = display_recovery_notif_state();
        let st = recovery.lock().await;
        st.replaces_id
    };
    let id = proxy
        .notify(
            "zenbook-duo-daemon",
            rid,
            icon,
            summary,
            body,
            vec![],
            hints,
            expire_timeout,
        )
        .await?;
    {
        let recovery = display_recovery_notif_state();
        let mut st = recovery.lock().await;
        st.replaces_id = id;
    }
    Ok(())
}

pub async fn send_display_recovery_persistent_critical_chained(
    summary: &str,
    body: &str,
    icon: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let conn = Connection::session().await?;
    let proxy = NotificationsProxy::new(&conn).await?;
    let hints = persistent_urgency_hints(2);
    let rid = {
        let recovery = display_recovery_notif_state();
        let st = recovery.lock().await;
        st.replaces_id
    };
    let id = proxy
        .notify(
            "zenbook-duo-daemon",
            rid,
            icon,
            summary,
            body,
            vec![],
            hints,
            0,
        )
        .await?;
    {
        let recovery = display_recovery_notif_state();
        let mut st = recovery.lock().await;
        st.replaces_id = id;
    }
    Ok(())
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

/// Read ASUS keyboard battery from BlueZ (system bus). Used when sending detach+Bluetooth copy.
pub async fn read_keyboard_battery_pct(system: &Connection) -> Option<u8> {
    let path = find_keyboard_bluez_path(system).await?;
    let proxy = BlueZBatteryProxy::builder(system)
        .destination("org.bluez")
        .ok()?
        .path(path.as_str())
        .ok()?
        .build()
        .await
        .ok()?;
    proxy.percentage().await.ok()
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
    let mut hints = transient_hints();
    hints.insert("urgency", Value::U8(urgency));
    let _ = notif
        .notify(
            "zenbook-duo-daemon",
            0,
            icon,
            summary,
            body,
            vec![],
            hints,
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

fn append_line_sep(base: &str, extra: &str) -> String {
    format!("{base}\u{2028}{extra}")
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

                let (bluetooth_connected, desired_primary, desired_secondary_enabled) =
                    match root_proxy.as_ref() {
                        Some(p) => (
                            p.bluetooth_connected().await.unwrap_or(false),
                            p.desired_primary()
                                .await
                                .unwrap_or_else(|_| "eDP-1".to_string()),
                            p.desired_secondary_enabled().await.unwrap_or(true),
                        ),
                        None => (false, "eDP-1".to_string(), true),
                    };

                let (summary, mut body, icon) = if root_proxy.is_some() {
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

                if attached {
                    let chain = keyboard_display_notif_state();
                    let last = chain.lock().await.last_battery_pct;
                    let pct_txt = last
                        .map(|p| format!("{p}%"))
                        .unwrap_or_else(|| "unknown".to_string());
                    let extra = format!("charging (last battery level: {pct_txt}).");
                    body = append_line_sep(&body, &format!("• {extra}"));
                } else if let Some(sys) = system_conn.as_ref() {
                    if bluetooth_connected {
                        let pct = read_keyboard_battery_pct(sys).await;
                        let pct_txt = pct
                            .map(|p| format!("{p}%"))
                            .unwrap_or_else(|| "unknown".to_string());
                        let extra = format!("discharging (current battery level: {pct_txt}).");
                        body = append_line_sep(&body, &format!("• {extra}"));
                    }
                }

                if let Err(e) = notify_keyboard_display_transient_fresh(
                    &notif,
                    icon,
                    summary,
                    &body,
                    urgency_hints(1),
                    10_000,
                )
                .await
                {
                    warn!("Failed to send notification: {e}");
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

// ── Battery & BT-disconnect monitor ──────────────────────────────────────────
//
// GNOME Shell often ignores client `expire_timeout` for banner dwell time; urgency `critical`
// (byte 2) makes low-battery notifications sticky. See `plan.md` §9.

const BATTERY_WARN_PCT_LOW: u8 = 20;
const BATTERY_WARN_PCT_VERY_LOW: u8 = 10;
const BATTERY_WARN_PCT_CRITICAL: u8 = 5;

const BATTERY_EXPIRE_MS_CRITICAL: i32 = 120_000;
const BATTERY_EXPIRE_MS_BELOW_10: i32 = 60_000;
const BATTERY_EXPIRE_MS_BELOW_20: i32 = 60_000;

/// Poll BlueZ `Percentage` as a fallback when property-change signals are missed.
const BATTERY_POLL_SECS: u64 = 60;

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

/// Battery warnings while **USB detached** and keyboard on Bluetooth.
/// All tiers use urgency **critical** (byte 2) + **persistent** hints so GNOME treats them as
/// high-severity; `expire_timeout` is kept for spec compliance (mostly ignored by Shell).
async fn maybe_emit_battery_warning_detached_bt(
    notif: &NotificationsProxy<'_>,
    pct: u8,
    w: &mut KeyboardBatteryWarn,
) {
    if pct < BATTERY_WARN_PCT_CRITICAL && !w.shown_below_5 {
        w.shown_below_5 = true;
        w.shown_below_10 = true;
        w.shown_below_20 = true;
        let mut hints = persistent_urgency_hints(2);
        hints.insert("image-path", Value::from("battery-level-0-symbolic"));
        let body = format!(
            "Keyboard battery at <b>{pct}%</b>.\u{2028}\
             Reattach the keyboard to USB immediately — it could become unavailable at any moment."
        );
        let _ = notify_keyboard_display_chain(
            notif,
            "dialog-error",
            "Keyboard battery critical!",
            &body,
            hints,
            BATTERY_EXPIRE_MS_CRITICAL,
        )
        .await;
        {
            let chain = keyboard_display_notif_state();
            let mut st = chain.lock().await;
            st.had_battery_tier_while_usb_detached = true;
        }
        return;
    }

    if pct < BATTERY_WARN_PCT_VERY_LOW && !w.shown_below_10 {
        w.shown_below_10 = true;
        w.shown_below_20 = true;
        let mut hints = persistent_urgency_hints(2);
        hints.insert("image-path", Value::from("battery-caution-symbolic"));
        let body = format!(
            "Keyboard battery at <b>{pct}%</b>.\u{2028}\
             Attach the keyboard to USB to charge before it becomes critical."
        );
        let _ = notify_keyboard_display_chain(
            notif,
            "dialog-warning",
            "Keyboard battery below 10%",
            &body,
            hints,
            BATTERY_EXPIRE_MS_BELOW_10,
        )
        .await;
        {
            let chain = keyboard_display_notif_state();
            let mut st = chain.lock().await;
            st.had_battery_tier_while_usb_detached = true;
        }
        return;
    }

    if pct < BATTERY_WARN_PCT_LOW && !w.shown_below_20 {
        w.shown_below_20 = true;
        let mut hints = persistent_urgency_hints(2);
        hints.insert("image-path", Value::from("battery-full-charging-symbolic"));
        let body = format!(
            "Keyboard battery at <b>{pct}%</b>.\u{2028}\
             Running on Bluetooth power — level is informational only."
        );
        let _ = notify_keyboard_display_chain(
            notif,
            "dialog-warning",
            "Keyboard battery below 20%",
            &body,
            hints,
            BATTERY_EXPIRE_MS_BELOW_20,
        )
        .await;
        {
            let chain = keyboard_display_notif_state();
            let mut st = chain.lock().await;
            st.had_battery_tier_while_usb_detached = true;
        }
    }
}

async fn on_battery_pct_update(
    notif: &NotificationsProxy<'_>,
    pct: u8,
    last_pct: &mut u8,
    was_below_full: &mut bool,
    w: &mut KeyboardBatteryWarn,
    usb_attached: bool,
) -> bool {
    if pct == *last_pct {
        return true;
    }
    *last_pct = pct;

    {
        let chain = keyboard_display_notif_state();
        let mut st = chain.lock().await;
        st.last_battery_pct = Some(pct);
    }

    w.clear_recovered(pct);
    if !usb_attached {
        maybe_emit_battery_warning_detached_bt(notif, pct, w).await;
    }

    if pct < 100 {
        *was_below_full = true;
    }
    if pct == 100 && *was_below_full {
        *was_below_full = false;
        let _ = notify_keyboard_display_transient_fresh(
            notif,
            "battery-full-charged",
            "Keyboard battery full",
            "Keyboard battery is fully charged.\u{2028}\
             You can detach it from USB.",
            urgency_hints(1),
            4_000,
        )
        .await;
    }
    false
}

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

    let mut last_pct: u8;
    let mut was_below_full: bool;
    let mut prev_usb_attached: bool = false;

    match battery_proxy.percentage().await {
        Ok(pct) => {
            last_pct = pct;
            was_below_full = pct < 100;
            let usb = match root_proxy.as_ref() {
                Some(p) => p.keyboard_attached().await.unwrap_or(false),
                None => false,
            };
            if usb {
                prev_usb_attached = true;
            } else {
                prev_usb_attached = false;
                bw.clear_recovered(pct);
                maybe_emit_battery_warning_detached_bt(&notif, pct, bw).await;
            }
            {
                let chain = keyboard_display_notif_state();
                let mut st = chain.lock().await;
                st.last_battery_pct = Some(pct);
            }
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
        let usb_attached = match root_proxy.as_ref() {
            Some(p) => p.keyboard_attached().await.unwrap_or(false),
            None => false,
        };

        if usb_attached && !prev_usb_attached {
            *bw = KeyboardBatteryWarn::default();
            let chain = keyboard_display_notif_state();
            let mut st = chain.lock().await;
            st.had_battery_tier_while_usb_detached = false;
        }
        prev_usb_attached = usb_attached;

        tokio::select! {
            Some(change) = battery_stream.next() => {
                let pct = match change.get().await {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if on_battery_pct_update(&notif, pct, &mut last_pct, &mut was_below_full, bw, usb_attached).await {
                    continue;
                }
            }

            _ = pct_poll.tick() => {
                let pct = battery_proxy.percentage().await.unwrap_or(last_pct);
                let _ = on_battery_pct_update(&notif, pct, &mut last_pct, &mut was_below_full, bw, usb_attached).await;
            }

            Some(change) = connected_stream.next() => {
                let connected = match change.get().await {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if connected {
                    return Ok(());
                }

                let usb_attached_now = match root_proxy.as_ref() {
                    Some(p) => p.keyboard_attached().await.unwrap_or(false),
                    None => false,
                };

                if !usb_attached_now {
                    let (had_tier, last_known) = {
                        let chain = keyboard_display_notif_state();
                        let st = chain.lock().await;
                        (st.had_battery_tier_while_usb_detached, st.last_battery_pct)
                    };
                    if had_tier {
                        let pct_txt = last_known
                            .map(|p| format!("{p}%"))
                            .unwrap_or_else(|| "unknown".to_string());
                        let body = bullet_lines(&[
                            "Did you disable the keyboard or did it run out of battery?",
                            &format!("Last known battery percentage: {pct_txt}."),
                        ]);
                        let _ = notify_keyboard_display_chain(
                            &notif,
                            "dialog-warning",
                            "Bluetooth keyboard disconnected",
                            &body,
                            persistent_urgency_hints(2),
                            BATTERY_EXPIRE_MS_CRITICAL,
                        )
                        .await;
                    } else {
                        let body = bullet_lines(&[
                            "Keyboard Bluetooth has disconnected.",
                            "The keyboard may have run out of battery or been switched off.",
                            "Attach it via USB to check or recharge.",
                        ]);
                        let _ = notify_keyboard_display_transient_fresh(
                            &notif,
                            "dialog-warning",
                            "Keyboard disconnected",
                            &body,
                            urgency_hints(1),
                            6_000,
                        )
                        .await;
                    }
                }

                return Err("keyboard BT disconnected".into());
            }
        }
    }
}
