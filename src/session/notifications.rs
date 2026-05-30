use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::OnceLock;

use futures::StreamExt as _;
use log::{error, warn};
use tokio::sync::broadcast;
use tokio::sync::Mutex;
use tokio::time::{interval_at, Duration, Instant, MissedTickBehavior};
use zbus::{Connection, proxy, zvariant::Value};

use crate::keyboard_battery::BATTERY_FULL_PCT_THRESHOLD;
use crate::keyboard_battery_bt::find_keyboard_bluez_path;

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
    fn keyboard_usb_connected(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn keyboard_pogo_docked(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn keyboard_attached(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn keyboard_battery_present(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn keyboard_battery_percentage(&self) -> zbus::Result<u8>;

    #[zbus(property)]
    fn keyboard_battery_charging(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn keyboard_battery_full(&self) -> zbus::Result<bool>;
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
    let chain = keyboard_display_notif_state();
    let mut st = chain.lock().await;
    let rid = st.replaces_id;
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
    st.replaces_id = id;
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
    let chain = keyboard_display_notif_state();
    let mut st = chain.lock().await;
    let old_id = st.replaces_id;
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
    st.replaces_id = id;
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
    let recovery = display_recovery_notif_state();
    let mut st = recovery.lock().await;
    let rid = st.replaces_id;
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
    st.replaces_id = id;
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
    let recovery = display_recovery_notif_state();
    let mut st = recovery.lock().await;
    let rid = st.replaces_id;
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
    st.replaces_id = id;
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

fn pogo_dock_notification_text(
    docked: bool,
    bluetooth_connected: bool,
    desired_primary: &str,
    desired_secondary_enabled: bool,
) -> (&'static str, String, &'static str) {
    if docked {
        let top_bar_moves = desired_primary == "eDP-2";

        let body = if desired_secondary_enabled {
            if top_bar_moves {
                bullet_lines(&[
                    "Keyboard docked on the bottom panel.",
                    "Top bar will move temporarily to the top display.",
                    "Secondary display will turn off.",
                ])
            } else {
                bullet_lines(&[
                    "Keyboard docked on the bottom panel.",
                    "Top display stays primary.",
                    "Secondary display will turn off.",
                ])
            }
        } else if top_bar_moves {
            bullet_lines(&[
                "Keyboard docked on the bottom panel.",
                "Top bar will move temporarily to the top display.",
                "Secondary display remains off.",
            ])
        } else {
            bullet_lines(&[
                "Keyboard docked on the bottom panel.",
                "Top display stays primary.",
                "Secondary display remains off.",
            ])
        };
        ("Keyboard docked", body, "input-keyboard")
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
        ("Keyboard undocked (Bluetooth)", body, "input-keyboard-virtual")
    } else {
        let body = if desired_secondary_enabled {
            bullet_lines(&[
                "Keyboard undocked — Bluetooth not connected.",
                "Secondary display is turning on.",
                "Keyboard unavailable until Bluetooth reconnects.",
            ])
        } else {
            bullet_lines(&[
                "Keyboard undocked — Bluetooth not connected.",
                "Keyboard unavailable until Bluetooth reconnects.",
            ])
        };
        ("Keyboard undocked (no Bluetooth)", body, "dialog-warning")
    }
}

fn usb_charge_notification_text() -> (&'static str, String, &'static str) {
    (
        "Keyboard on USB",
        bullet_lines(&[
            "Side USB connected (charge / wired input).",
            "Secondary display stays as configured.",
        ]),
        "input-keyboard",
    )
}

/// Side-USB unplug (charge cable removed while undocked) — back to Bluetooth.
fn side_usb_unplug_notification_text(
    bluetooth_connected: bool,
    desired_primary: &str,
    desired_secondary_enabled: bool,
) -> (&'static str, String, &'static str) {
    pogo_dock_notification_text(
        false,
        bluetooth_connected,
        desired_primary,
        desired_secondary_enabled,
    )
}

fn append_line_sep(base: &str, extra: &str) -> String {
    format!("{base}\u{2028}{extra}")
}

// ── Keyboard attach/detach notification loop ──────────────────────────────────

pub async fn run(
    mut kb_usb_rx: broadcast::Receiver<bool>,
    mut kb_pogo_rx: broadcast::Receiver<bool>,
) {
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

    let mut usb_initialized = false;
    let mut pogo_initialized = false;
    let mut last_usb: Option<bool> = None;
    let mut last_pogo: Option<bool> = None;
    // True while the keyboard is on side USB (undocked charge/data), not pogo dock.
    let mut side_usb_session = false;

    loop {
        tokio::select! {
            msg = kb_usb_rx.recv() => match msg {
                Ok(usb_connected) => {
                    if !usb_initialized {
                        usb_initialized = true;
                        last_usb = Some(usb_connected);
                        continue;
                    }
                    if last_usb == Some(usb_connected) {
                        continue;
                    }
                    last_usb = Some(usb_connected);

                    // Allow root daemon time to resolve hub port (sysfs devpath race on hotplug).
                    tokio::time::sleep(Duration::from_millis(400)).await;

                    let pogo_docked = match root_proxy.as_ref() {
                        Some(p) => p.keyboard_pogo_docked().await.unwrap_or(false),
                        None => false,
                    };

                    if !usb_connected {
                        if !side_usb_session {
                            continue;
                        }
                        side_usb_session = false;
                        tokio::time::sleep(Duration::from_millis(2500)).await;

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

                        let (summary, mut body, icon) = side_usb_unplug_notification_text(
                            bluetooth_connected,
                            &desired_primary,
                            desired_secondary_enabled,
                        );

                        if let Some(sys) = system_conn.as_ref() {
                            if bluetooth_connected {
                                let pct = read_keyboard_battery_pct(sys).await;
                                let pct_txt = pct
                                    .map(|p| format!("{p}%"))
                                    .unwrap_or_else(|| "unknown".to_string());
                                let extra =
                                    format!("discharging (current battery level: {pct_txt}).");
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
                            warn!("Failed to send side USB unplug notification: {e}");
                        }
                        continue;
                    }

                    if pogo_docked {
                        side_usb_session = false;
                        continue;
                    }

                    side_usb_session = true;

                    let (summary, mut body, icon) = usb_charge_notification_text();
                    if let Some(chain) = Some(keyboard_display_notif_state()) {
                        let last = chain.lock().await.last_battery_pct;
                        let pct_txt = last
                            .map(|p| format!("{p}%"))
                            .unwrap_or_else(|| "unknown".to_string());
                        let extra = format!("charging (last battery level: {pct_txt}).");
                        body = append_line_sep(&body, &format!("• {extra}"));
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
                        warn!("Failed to send USB charge notification: {e}");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = kb_pogo_rx.recv() => match msg {
                Ok(docked) => {
                    if !pogo_initialized {
                        pogo_initialized = true;
                        last_pogo = Some(docked);
                        continue;
                    }

                    if last_pogo == Some(docked) {
                        continue;
                    }
                    last_pogo = Some(docked);

                    if docked {
                        side_usb_session = false;
                    }

                    if !docked {
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
                        pogo_dock_notification_text(
                            docked,
                            bluetooth_connected,
                            &desired_primary,
                            desired_secondary_enabled,
                        )
                    } else if docked {
                        (
                            "Keyboard docked",
                            bullet_lines(&[
                                "Keyboard docked on the bottom panel.",
                                "Secondary display will turn off.",
                            ]),
                            "input-keyboard",
                        )
                    } else {
                        (
                            "Keyboard undocked",
                            bullet_lines(&[
                                "Keyboard undocked.",
                                "Bluetooth state could not be confirmed.",
                            ]),
                            "dialog-warning",
                        )
                    };

                    if docked {
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
                        warn!("Failed to send pogo dock notification: {e}");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
}

// ── Battery & BT-disconnect monitor ──────────────────────────────────────────
//
// GNOME Shell often ignores client `expire_timeout` for banner dwell time; urgency `critical`
// (byte 2) makes low-battery notifications sticky. See `docs/notes-gnome-shell-notifications.md`.

const BATTERY_WARN_PCT_LOW: u8 = 20;
const BATTERY_WARN_PCT_VERY_LOW: u8 = 10;
const BATTERY_WARN_PCT_CRITICAL: u8 = 5;

const CHARGE_MILESTONE_PCT_HALF: u8 = 50;
const CHARGE_MILESTONE_PCT_HIGH: u8 = 75;

const BATTERY_EXPIRE_MS_CRITICAL: i32 = 120_000;
const BATTERY_EXPIRE_MS_BELOW_10: i32 = 60_000;
const BATTERY_EXPIRE_MS_BELOW_20: i32 = 60_000;
const CHARGE_MILESTONE_EXPIRE_MS: i32 = 45_000;
const CHARGE_FULL_EXPIRE_MS: i32 = 60_000;

/// Poll BlueZ / root D-Bus `Percentage` as a fallback when property-change signals are missed.
const BATTERY_POLL_SECS: u64 = 60;

#[derive(Clone, Default, Debug)]
struct KeyboardBatteryWarn {
    shown_below_20: bool,
    shown_below_10: bool,
    shown_below_5: bool,
}

impl KeyboardBatteryWarn {
    fn clear_recovered(&mut self, pct: u8) {
        if pct > BATTERY_WARN_PCT_CRITICAL {
            self.shown_below_5 = false;
        }
        if pct > BATTERY_WARN_PCT_VERY_LOW {
            self.shown_below_10 = false;
        }
        if pct > BATTERY_WARN_PCT_LOW {
            self.shown_below_20 = false;
        }
    }
}

#[derive(Clone, Default, Debug)]
struct KeyboardBatteryChargeWarn {
    shown_half: bool,
    shown_high: bool,
    shown_full: bool,
}

impl KeyboardBatteryChargeWarn {
    fn reset_for_new_charge(&mut self) {
        *self = Self::default();
    }
}

/// Rolling samples while side-USB charging for ETA text on milestone toasts.
#[derive(Clone, Debug, Default)]
struct ChargingRateTracker {
    samples: VecDeque<(Instant, u8)>,
}

impl ChargingRateTracker {
    fn reset(&mut self) {
        self.samples.clear();
    }

    fn record(&mut self, pct: u8) {
        let now = Instant::now();
        if self.samples.back().is_some_and(|(_, p)| *p == pct) {
            return;
        }
        self.samples.push_back((now, pct));
        while self.samples.len() > 24 {
            self.samples.pop_front();
        }
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(45 * 60))
        {
            self.samples.pop_front();
        }
    }

    fn pct_per_minute(&self) -> Option<f64> {
        let (t0, p0) = self.samples.front()?;
        let (t1, p1) = self.samples.back()?;
        if self.samples.len() < 2 {
            return None;
        }
        let mins = t1.duration_since(*t0).as_secs_f64() / 60.0;
        if mins < 1.0 {
            return None;
        }
        let delta = f64::from(*p1) - f64::from(*p0);
        if delta <= 0.0 {
            return None;
        }
        Some(delta / mins)
    }

    fn eta_minutes_to(&self, current_pct: u8, target_pct: u8) -> Option<u32> {
        if current_pct >= target_pct {
            return Some(0);
        }
        let rate = self.pct_per_minute()?;
        let remaining = f64::from(target_pct.saturating_sub(current_pct));
        Some(((remaining / rate).ceil() as u32).max(1))
    }
}

fn format_charge_eta_minutes(eta_mins: Option<u32>) -> String {
    match eta_mins {
        None => String::new(),
        Some(0) => "Estimated time to full: less than a minute.".to_string(),
        Some(1) => "Estimated time to full: about 1 minute.".to_string(),
        Some(m) => format!("Estimated time to full: about {m} minutes."),
    }
}

fn is_keyboard_battery_full(pct: u8, firmware_full: bool) -> bool {
    firmware_full || pct >= BATTERY_FULL_PCT_THRESHOLD
}

/// Battery warnings while **USB detached** and keyboard on Bluetooth.
/// All tiers use urgency **critical** (byte 2) + **persistent** hints so GNOME treats them as
/// high-severity; `expire_timeout` is kept for spec compliance (mostly ignored by Shell).
/// Thresholds fire at **≤** the documented percentages (5%, 10%, 20%).
async fn maybe_emit_battery_warning_detached_bt(
    notif: &NotificationsProxy<'_>,
    pct: u8,
    w: &mut KeyboardBatteryWarn,
) {
    if pct <= BATTERY_WARN_PCT_CRITICAL && !w.shown_below_5 {
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

    if pct <= BATTERY_WARN_PCT_VERY_LOW && !w.shown_below_10 {
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

    if pct <= BATTERY_WARN_PCT_LOW && !w.shown_below_20 {
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

async fn maybe_emit_battery_charging_milestones(
    notif: &NotificationsProxy<'_>,
    pct: u8,
    w: &mut KeyboardBatteryChargeWarn,
    rate: &ChargingRateTracker,
) {
    if pct >= CHARGE_MILESTONE_PCT_HALF && !w.shown_half {
        w.shown_half = true;
        let eta = format_charge_eta_minutes(rate.eta_minutes_to(pct, 100));
        let mut hints = urgency_hints(1);
        hints.insert(
            "image-path",
            Value::from("battery-medium-charging-symbolic"),
        );
        let mut lines = vec![
            format!("Keyboard battery at <b>{pct}%</b> while charging on USB."),
        ];
        if !eta.is_empty() {
            lines.push(eta);
        }
        let _ = notify_keyboard_display_transient_fresh(
            notif,
            "battery-medium-charging-symbolic",
            "Keyboard charging — 50%",
            &bullet_lines(&lines.iter().map(String::as_str).collect::<Vec<_>>()),
            hints,
            CHARGE_MILESTONE_EXPIRE_MS,
        )
        .await;
    }

    if pct >= CHARGE_MILESTONE_PCT_HIGH && !w.shown_high {
        w.shown_high = true;
        let eta = format_charge_eta_minutes(rate.eta_minutes_to(pct, 100));
        let mut hints = urgency_hints(1);
        hints.insert("image-path", Value::from("battery-good-charging-symbolic"));
        let mut lines = vec![
            format!("Keyboard battery at <b>{pct}%</b> while charging on USB."),
        ];
        if !eta.is_empty() {
            lines.push(eta);
        }
        let _ = notify_keyboard_display_transient_fresh(
            notif,
            "battery-good-charging-symbolic",
            "Keyboard charging — 75%",
            &bullet_lines(&lines.iter().map(String::as_str).collect::<Vec<_>>()),
            hints,
            CHARGE_MILESTONE_EXPIRE_MS,
        )
        .await;
    }
}

async fn maybe_emit_battery_full_charged(
    notif: &NotificationsProxy<'_>,
    pct: u8,
    w: &mut KeyboardBatteryChargeWarn,
) {
    if w.shown_full {
        return;
    }
    w.shown_full = true;
    w.shown_half = true;
    w.shown_high = true;

    let body = if pct >= 100 {
        bullet_lines(&[
            "Keyboard battery is fully charged.",
            "You can unplug the side USB cable and use Bluetooth again.",
        ])
    } else {
        bullet_lines(&[
            &format!(
                "Keyboard battery is at <b>{pct}%</b> — the highest level this pack reports when full."
            ),
            "You can unplug the side USB cable and use Bluetooth again.",
        ])
    };

    let mut hints = urgency_hints(1);
    hints.insert("image-path", Value::from("battery-full-charged-symbolic"));
    let _ = notify_keyboard_display_transient_fresh(
        notif,
        "battery-full-charged",
        "Keyboard battery full",
        &body,
        hints,
        CHARGE_FULL_EXPIRE_MS,
    )
    .await;
}

async fn on_usb_battery_update(
    notif: &NotificationsProxy<'_>,
    pct: u8,
    charging: bool,
    firmware_full: bool,
    last_pct: &mut u8,
    was_below_full: &mut bool,
    bw_charge: &mut KeyboardBatteryChargeWarn,
    rate: &mut ChargingRateTracker,
) -> bool {
    if pct == *last_pct && !firmware_full {
        return true;
    }
    *last_pct = pct;

    {
        let chain = keyboard_display_notif_state();
        let mut st = chain.lock().await;
        st.last_battery_pct = Some(pct);
    }

    if charging {
        rate.record(pct);
        maybe_emit_battery_charging_milestones(notif, pct, bw_charge, rate).await;
    }

    let full = is_keyboard_battery_full(pct, firmware_full);
    if full {
        if *was_below_full {
            *was_below_full = false;
            maybe_emit_battery_full_charged(notif, pct, bw_charge).await;
        }
    } else if pct < BATTERY_FULL_PCT_THRESHOLD {
        *was_below_full = true;
    }
    false
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

    if pct < BATTERY_FULL_PCT_THRESHOLD {
        *was_below_full = true;
    }
    if pct >= BATTERY_FULL_PCT_THRESHOLD && *was_below_full {
        *was_below_full = false;
        let summary = if pct >= 100 {
            "Keyboard battery full"
        } else {
            "Keyboard battery full"
        };
        let body = if pct >= 100 {
            bullet_lines(&[
                "Keyboard battery is fully charged.",
                "You can unplug the side USB cable and use Bluetooth again.",
            ])
        } else {
            bullet_lines(&[
                &format!(
                    "Keyboard battery is at <b>{pct}%</b> — the highest level this pack reports when full."
                ),
                "You can unplug the side USB cable and use Bluetooth again.",
            ])
        };
        let _ = notify_keyboard_display_transient_fresh(
            notif,
            "battery-full-charged",
            summary,
            &body,
            urgency_hints(1),
            CHARGE_FULL_EXPIRE_MS,
        )
        .await;
    }
    false
}

pub async fn run_battery_monitor() {
    let mut bw_discharge = KeyboardBatteryWarn::default();
    let mut bw_charge = KeyboardBatteryChargeWarn::default();
    let mut rate = ChargingRateTracker::default();
    loop {
        if let Err(e) =
            battery_monitor_inner(&mut bw_discharge, &mut bw_charge, &mut rate).await
        {
            warn!("Battery monitor: {e}, retrying in 15s");
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

async fn battery_monitor_inner(
    bw_discharge: &mut KeyboardBatteryWarn,
    bw_charge: &mut KeyboardBatteryChargeWarn,
    rate: &mut ChargingRateTracker,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let system_conn = Connection::system().await?;
    let session_conn = Connection::session().await?;
    let notif = NotificationsProxy::new(&session_conn).await?;
    let root_proxy = RootStateProxy::new(&system_conn).await?;

    loop {
        let usb = root_proxy.keyboard_usb_connected().await.unwrap_or(false);
        if usb {
            monitor_usb_keyboard_battery(&root_proxy, &notif, bw_charge, rate).await?;
        } else {
            rate.reset();
            monitor_bluetooth_keyboard_battery(
                &system_conn,
                &notif,
                &root_proxy,
                bw_discharge,
            )
            .await?;
        }
    }
}

async fn monitor_usb_keyboard_battery(
    root_proxy: &RootStateProxy<'_>,
    notif: &NotificationsProxy<'_>,
    bw_charge: &mut KeyboardBatteryChargeWarn,
    rate: &mut ChargingRateTracker,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut last_pct: u8 = 0;
    let mut was_below_full = true;

    if root_proxy.keyboard_battery_present().await.unwrap_or(false) {
        let pct = root_proxy.keyboard_battery_percentage().await.unwrap_or(0);
        let charging = root_proxy.keyboard_battery_charging().await.unwrap_or(false);
        let full = root_proxy.keyboard_battery_full().await.unwrap_or(false);
        last_pct = pct;
        was_below_full = !is_keyboard_battery_full(pct, full);
        if pct < CHARGE_MILESTONE_PCT_HALF {
            bw_charge.reset_for_new_charge();
        }
        rate.reset();
        if charging {
            rate.record(pct);
        }
        let _ = on_usb_battery_update(
            notif,
            pct,
            charging,
            full,
            &mut last_pct,
            &mut was_below_full,
            bw_charge,
            rate,
        )
        .await;
    } else {
        bw_charge.reset_for_new_charge();
        rate.reset();
    }

    let mut pct_stream = root_proxy
        .receive_keyboard_battery_percentage_changed()
        .await;
    let mut charging_stream = root_proxy
        .receive_keyboard_battery_charging_changed()
        .await;
    let mut full_stream = root_proxy.receive_keyboard_battery_full_changed().await;
    let mut usb_stream = root_proxy.receive_keyboard_usb_connected_changed().await;
    let mut pct_poll = interval_at(
        Instant::now() + Duration::from_secs(BATTERY_POLL_SECS),
        Duration::from_secs(BATTERY_POLL_SECS),
    );
    pct_poll.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        if !root_proxy.keyboard_usb_connected().await.unwrap_or(false) {
            return Ok(());
        }

        tokio::select! {
            _ = pct_stream.next() => {}
            _ = charging_stream.next() => {}
            _ = full_stream.next() => {}
            _ = usb_stream.next() => {
                if !root_proxy.keyboard_usb_connected().await.unwrap_or(false) {
                    return Ok(());
                }
            }
            _ = pct_poll.tick() => {}
        }

        if !root_proxy.keyboard_battery_present().await.unwrap_or(false) {
            continue;
        }

        let pct = root_proxy.keyboard_battery_percentage().await.unwrap_or(last_pct);
        let charging = root_proxy.keyboard_battery_charging().await.unwrap_or(false);
        let full = root_proxy.keyboard_battery_full().await.unwrap_or(false);

        let _ = on_usb_battery_update(
            notif,
            pct,
            charging,
            full,
            &mut last_pct,
            &mut was_below_full,
            bw_charge,
            rate,
        )
        .await;
    }
}

async fn monitor_bluetooth_keyboard_battery(
    system_conn: &Connection,
    notif: &NotificationsProxy<'_>,
    root_proxy: &RootStateProxy<'_>,
    bw_discharge: &mut KeyboardBatteryWarn,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let device_path = loop {
        if let Some(p) = find_keyboard_bluez_path(system_conn).await {
            break p;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    };

    let battery_proxy = BlueZBatteryProxy::builder(system_conn)
        .destination("org.bluez")?
        .path(device_path.as_str())?
        .build()
        .await?;

    let device_proxy = BlueZDeviceProxy::builder(system_conn)
        .destination("org.bluez")?
        .path(device_path.as_str())?
        .build()
        .await?;

    let mut last_pct: u8;
    let mut was_below_full: bool;

    match battery_proxy.percentage().await {
        Ok(pct) => {
            last_pct = pct;
            was_below_full = pct < BATTERY_FULL_PCT_THRESHOLD;
            bw_discharge.clear_recovered(pct);
            maybe_emit_battery_warning_detached_bt(notif, pct, bw_discharge).await;
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
        if root_proxy.keyboard_usb_connected().await.unwrap_or(false) {
            return Ok(());
        }

        tokio::select! {
            Some(change) = battery_stream.next() => {
                let pct = match change.get().await {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if on_battery_pct_update(notif, pct, &mut last_pct, &mut was_below_full, bw_discharge, false).await {
                    continue;
                }
            }

            _ = pct_poll.tick() => {
                let pct = battery_proxy.percentage().await.unwrap_or(last_pct);
                let _ = on_battery_pct_update(notif, pct, &mut last_pct, &mut was_below_full, bw_discharge, false).await;
            }

            Some(change) = connected_stream.next() => {
                let connected = match change.get().await {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if connected {
                    return Ok(());
                }

                let usb_attached_now = root_proxy.keyboard_usb_connected().await.unwrap_or(false);

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
                            notif,
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
                            notif,
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
