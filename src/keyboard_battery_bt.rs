//! Mirror BlueZ `org.bluez.Battery1` into root D-Bus when the keyboard is on Bluetooth only.
//!
//! While [`crate::state::KeyboardStateManager::is_usb_keyboard_connected`] is true, battery
//! must come from USB vendor reports (side USB or pogo); this module does not update D-Bus.

use std::time::Duration;

use futures::stream::StreamExt;
use log::{debug, warn};
use tokio::time::{MissedTickBehavior, interval, interval_at};
use zbus::Connection;
use zbus::fdo::ObjectManagerProxy;
use zbus::proxy;
use zbus::zvariant::Str;

use crate::state::KeyboardStateManager;

const BATTERY_POLL_SECS: u64 = 60;

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
}

/// Find the ASUS keyboard device path that exposes `org.bluez.Battery1`.
pub async fn find_keyboard_bluez_path(conn: &Connection) -> Option<String> {
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
            .and_then(|v| v.downcast_ref::<Str>().ok())
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

pub fn start_bt_battery_monitor_task(state_manager: KeyboardStateManager) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = monitor_bt_battery_once(&state_manager).await {
                debug!("BT battery monitor: {e}");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

/// One-shot read after USB unplug so D-Bus updates without waiting for the monitor loop.
pub async fn refresh_keyboard_battery_from_bluetooth(state_manager: &KeyboardStateManager) {
    if state_manager.is_usb_keyboard_connected() {
        return;
    }
    let Ok(conn) = Connection::system().await else {
        return;
    };
    let Some(path) = find_keyboard_bluez_path(&conn).await else {
        return;
    };
    let Ok(proxy) = BlueZBatteryProxy::builder(&conn)
        .destination("org.bluez")
        .map_err(|_| ())
        .and_then(|b| b.path(path.as_str()).map_err(|_| ()))
    else {
        return;
    };
    let Ok(proxy) = proxy.build().await else {
        return;
    };
    if let Ok(pct) = proxy.percentage().await {
        state_manager.set_keyboard_battery_bluetooth(pct);
    }
}

async fn monitor_bt_battery_once(
    state_manager: &KeyboardStateManager,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if state_manager.is_usb_keyboard_connected() {
        return Ok(());
    }

    let conn = Connection::system().await?;
    let device_path = loop {
        if state_manager.is_usb_keyboard_connected() {
            return Ok(());
        }
        if let Some(p) = find_keyboard_bluez_path(&conn).await {
            break p;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    };

    let battery_proxy = BlueZBatteryProxy::builder(&conn)
        .destination("org.bluez")?
        .path(device_path.as_str())?
        .build()
        .await?;

    let device_proxy = BlueZDeviceProxy::builder(&conn)
        .destination("org.bluez")?
        .path(device_path.as_str())?
        .build()
        .await?;

    if let Ok(pct) = battery_proxy.percentage().await {
        state_manager.set_keyboard_battery_bluetooth(pct);
    }

    let mut battery_stream = battery_proxy.receive_percentage_changed().await;
    let mut connected_stream = device_proxy.receive_connected_changed().await;
    let mut usb_poll = interval(Duration::from_secs(2));
    usb_poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut pct_poll = interval_at(
        tokio::time::Instant::now() + Duration::from_secs(BATTERY_POLL_SECS),
        Duration::from_secs(BATTERY_POLL_SECS),
    );
    pct_poll.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        if state_manager.is_usb_keyboard_connected() {
            return Ok(());
        }

        tokio::select! {
            Some(change) = battery_stream.next() => {
                match change.get().await {
                    Ok(pct) => state_manager.set_keyboard_battery_bluetooth(pct),
                    Err(e) => warn!("BT battery Percentage property: {e}"),
                }
            }
            _ = pct_poll.tick() => {
                if let Ok(pct) = battery_proxy.percentage().await {
                    state_manager.set_keyboard_battery_bluetooth(pct);
                }
            }
            Some(change) = connected_stream.next() => {
                match change.get().await {
                    Ok(true) => continue,
                    Ok(false) => {
                        state_manager.clear_keyboard_battery_if_no_source();
                        return Err("keyboard BT disconnected".into());
                    }
                    Err(e) => warn!("BT device Connected property: {e}"),
                }
            }
            _ = usb_poll.tick() => {
                if state_manager.is_usb_keyboard_connected() {
                    return Ok(());
                }
            }
        }
    }
}
