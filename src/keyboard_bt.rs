use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use evdev_rs::{
    Device, DeviceWrapper as _, InputEvent, ReadFlag,
    enums::{EV_ABS, EventCode},
};
use futures::stream::StreamExt;
use inotify::{Inotify, WatchMask};
use log::{debug, info, warn};
use nix::libc;
use tokio::sync::{Mutex, broadcast};
use tokio::{fs, task::spawn_blocking};
use zbus::Connection;
use zbus::fdo::ObjectManagerProxy;
use zbus::proxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::{
    config::Config,
    events::Event,
    idle_detection::ActivityNotifier,
    state::{KeyboardBacklightState, KeyboardStateManager},
    virtual_keyboard::VirtualKeyboard,
};

// ── BlueZ GATT proxies ───────────────────────────────────────────────────────

#[proxy(
    interface = "org.bluez.GattCharacteristic1",
    default_service = "org.bluez"
)]
trait GattCharacteristic {
    async fn write_value(
        &self,
        value: Vec<u8>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.bluez.GattDescriptor1",
    default_service = "org.bluez"
)]
trait GattDescriptor {
    async fn read_value(&self, options: HashMap<String, OwnedValue>) -> zbus::Result<Vec<u8>>;
}

// ── GATT helpers ─────────────────────────────────────────────────────────────

/// Derive BT MAC (BlueZ path format: "FD_0C_1A_6E_05_A3") from an evdev path
/// via the kernel sysfs `uniq` attribute.
async fn mac_from_evdev_sysfs(path: &PathBuf) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let uniq = fs::read_to_string(format!("/sys/class/input/{}/device/uniq", name))
        .await
        .ok()?;
    let mac = uniq.trim().to_uppercase().replace(':', "_");
    if mac.is_empty() { None } else { Some(mac) }
}

/// Find the GATT characteristic used for vendor HID feature reports.
/// Scans BlueZ objects under `device_path`, reads the Report Reference
/// descriptor (UUID 0x2908) on each HID Report char (UUID 0x2a4d), and
/// returns the path of the one with value [0x5a, 0x03] (report-id=90, feature).
async fn find_vendor_char(
    conn: &Connection,
    device_path: &str,
) -> Result<OwnedObjectPath, Box<dyn std::error::Error + Send + Sync>> {
    const REPORT_CHAR_UUID: &str = "00002a4d-0000-1000-8000-00805f9b34fb";
    const REPORT_REF_UUID: &str = "00002908-0000-1000-8000-00805f9b34fb";

    let manager = ObjectManagerProxy::builder(conn)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;

    let objects = manager.get_managed_objects().await?;

    let mut candidate_chars: Vec<String> = objects
        .iter()
        .filter_map(|(path, ifaces)| {
            let s = path.as_str();
            if !s.starts_with(device_path) {
                return None;
            }
            let char_iface = ifaces.get("org.bluez.GattCharacteristic1")?;
            let uuid = format!("{:?}", char_iface.get("UUID")?);
            if uuid.contains(REPORT_CHAR_UUID) { Some(s.to_string()) } else { None }
        })
        .collect();

    candidate_chars.sort();

    let opts: HashMap<String, OwnedValue> = HashMap::new();
    for char_path in &candidate_chars {
        let desc_paths: Vec<String> = objects
            .keys()
            .filter_map(|p| {
                let s = p.as_str();
                if s.starts_with(char_path.as_str()) && s.len() > char_path.len() {
                    Some(s.to_string())
                } else {
                    None
                }
            })
            .collect();

        for desc_path in desc_paths {
            let desc_ifaces = match objects.get(&OwnedObjectPath::try_from(desc_path.as_str())?) {
                Some(i) => i,
                None => continue,
            };
            let desc_iface = match desc_ifaces.get("org.bluez.GattDescriptor1") {
                Some(i) => i,
                None => continue,
            };
            let uuid = match desc_iface.get("UUID") {
                Some(u) => format!("{:?}", u),
                None => continue,
            };
            if !uuid.contains(REPORT_REF_UUID) {
                continue;
            }

            let desc_proxy = GattDescriptorProxy::builder(conn)
                .path(desc_path.as_str())?
                .build()
                .await?;
            match desc_proxy.read_value(opts.clone()).await {
                Ok(val) if val == vec![0x5a, 0x03] => {
                    return Ok(OwnedObjectPath::try_from(char_path.as_str())?);
                }
                _ => continue,
            }
        }
    }

    Err(format!("Could not find vendor HID feature char under {}", device_path).into())
}

async fn send_bt_backlight_state(proxy: &GattCharacteristicProxy<'_>, state: KeyboardBacklightState) {
    let level: u8 = match state {
        KeyboardBacklightState::Off => 0,
        KeyboardBacklightState::Low => 1,
        KeyboardBacklightState::Medium => 2,
        KeyboardBacklightState::High => 3,
    };
    if let Err(e) = proxy
        .write_value(vec![0xba, 0xc5, 0xc4, level], HashMap::new())
        .await
    {
        warn!("Failed to send BT backlight state: {}", e);
    }
}

async fn send_bt_mic_mute_state(proxy: &GattCharacteristicProxy<'_>, muted: bool) {
    if let Err(e) = proxy
        .write_value(vec![0xd0, 0x7c, if muted { 0x01 } else { 0x00 }], HashMap::new())
        .await
    {
        warn!("Failed to send BT mic mute state: {}", e);
    }
}

/// Set keyboard fn_lock state via BT GATT.
/// fn_lock=true  → multimedia keys are default (Fn needed for F1-F12).
/// fn_lock=false → F1-F12 are default (Fn needed for multimedia).
async fn send_bt_fn_lock_state(proxy: &GattCharacteristicProxy<'_>, fn_lock: bool) {
    // Mirror of the USB HID feature report 5a d0 4e [00/01], without the 0x5a report-ID prefix.
    let value: u8 = if fn_lock { 0x00 } else { 0x01 };
    if let Err(e) = proxy
        .write_value(vec![0xd0, 0x4e, value], HashMap::new())
        .await
    {
        warn!("Failed to send BT fn_lock state: {}", e);
    }
}

// ── Monitor / task startup ───────────────────────────────────────────────────

pub fn start_bt_keyboard_monitor_task(
    config: &Config,
    event_sender: broadcast::Sender<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
    activity_notifier: ActivityNotifier,
) {
    let config_clone = config.clone();
    let virtual_keyboard_clone = virtual_keyboard.clone();
    let state_manager_clone = state_manager.clone();
    // Track which BT MACs already have an active control task so we don't
    // spawn duplicate GATT writers when multiple evdev nodes share a MAC.
    let active_control_macs: Arc<Mutex<HashSet<String>>> =
        Arc::new(Mutex::new(HashSet::new()));

    tokio::spawn(async move {
        let mut entries = match fs::read_dir("/dev/input").await {
            Ok(entries) => entries,
            Err(e) => {
                warn!("Failed to read /dev/input: {}", e);
                return;
            }
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            try_start_bt_keyboard_task(
                &config_clone,
                path,
                event_sender.subscribe(),
                virtual_keyboard_clone.clone(),
                state_manager_clone.clone(),
                activity_notifier.clone(),
                active_control_macs.clone(),
            )
            .await;
        }

        let inotify = Inotify::init().expect("Failed to initialize inotify");
        inotify
            .watches()
            .add("/dev/input/", WatchMask::CREATE)
            .expect("Failed to add inotify watch");

        let mut buffer = [0; 1024];
        let mut stream = inotify.into_event_stream(&mut buffer).unwrap();

        while let Some(event_result) = stream.next().await {
            if let Ok(event) = event_result {
                if let Some(name) = event.name {
                    if event.mask.contains(inotify::EventMask::CREATE) {
                        if name.to_str().unwrap_or("").starts_with("event") {
                            let path = PathBuf::from("/dev/input/").join(name);
                            try_start_bt_keyboard_task(
                                &config_clone,
                                path,
                                event_sender.subscribe(),
                                virtual_keyboard_clone.clone(),
                                state_manager_clone.clone(),
                                activity_notifier.clone(),
                                active_control_macs.clone(),
                            )
                            .await;
                        }
                    }
                }
            }
        }
    });
}

async fn try_start_bt_keyboard_task(
    config: &Config,
    path: PathBuf,
    event_receiver: broadcast::Receiver<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
    activity_notifier: ActivityNotifier,
    active_control_macs: Arc<Mutex<HashSet<String>>>,
) {
    if let Ok(metadata) = fs::metadata(&path).await {
        if metadata.is_dir() {
            return;
        }
    } else {
        return;
    }

    if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
        if !fname.starts_with("event") {
            return;
        }
    } else {
        return;
    }

    let path_clone = path.clone();
    let input = spawn_blocking(move || {
        let file = std::fs::File::open(path_clone).unwrap();
        evdev_rs::Device::new_from_file(file).unwrap()
    })
    .await
    .unwrap();

    if input.name() == Some("ASUS Zenbook Duo Keyboard") {
        start_bt_keyboard_task(
            config,
            path,
            input,
            event_receiver,
            virtual_keyboard,
            state_manager,
            activity_notifier,
            active_control_macs,
        );
    }
}

pub fn start_bt_keyboard_task(
    config: &Config,
    path: PathBuf,
    keyboard: Device,
    mut event_receiver: broadcast::Receiver<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
    activity_notifier: ActivityNotifier,
    active_control_macs: Arc<Mutex<HashSet<String>>>,
) {
    info!("Bluetooth connected on {}", path.display());
    state_manager.bluetooth_connection_started();
    activity_notifier.notify();

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // Control task: discovers the GATT char, restores state, then handles events.
    // Only one control task runs per BT MAC — guarded by active_control_macs.
    let state_manager_ctrl = state_manager.clone();
    let fn_lock = config.fn_lock;
    tokio::spawn(async move {
        let mac = match mac_from_evdev_sysfs(&path).await {
            Some(m) => m,
            None => {
                warn!("Could not derive BT MAC from {:?} — backlight/mic LED won't work", path);
                return;
            }
        };

        // Skip if another event device for the same keyboard already owns control
        {
            let mut set = active_control_macs.lock().await;
            if set.contains(&mac) {
                debug!("Control task for MAC {} already running, skipping", mac);
                return;
            }
            set.insert(mac.clone());
        }

        let device_path = format!("/org/bluez/hci0/dev_{}", mac);

        let conn = match Connection::system().await {
            Ok(c) => c,
            Err(e) => {
                warn!("D-Bus connection failed: {} — backlight/mic LED won't work", e);
                active_control_macs.lock().await.remove(&mac);
                return;
            }
        };

        let char_path = match find_vendor_char(&conn, &device_path).await {
            Ok(p) => {
                info!("BT vendor char found: {}", p.as_str());
                p
            }
            Err(e) => {
                warn!("Could not find BT vendor char: {} — backlight/mic LED won't work", e);
                active_control_macs.lock().await.remove(&mac);
                return;
            }
        };

        let char_proxy = match GattCharacteristicProxy::builder(&conn)
            .path(char_path.as_str())
            .unwrap()
            .build()
            .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!("Failed to build GATT proxy: {} — backlight/mic LED won't work", e);
                active_control_macs.lock().await.remove(&mac);
                return;
            }
        };

        // Restore state on connect (mirrors USB behaviour)
        send_bt_fn_lock_state(&char_proxy, fn_lock).await;
        send_bt_backlight_state(&char_proxy, state_manager_ctrl.get_keyboard_backlight()).await;
        send_bt_mic_mute_state(&char_proxy, state_manager_ctrl.get_mic_mute_led()).await;

        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    info!("Bluetooth control task shutting down");
                    break;
                }
                result = event_receiver.recv() => {
                    match result {
                        Ok(Event::Backlight(state)) => {
                            send_bt_backlight_state(&char_proxy, state).await;
                        }
                        Ok(Event::MicMuteLed(enabled)) => {
                            send_bt_mic_mute_state(&char_proxy, enabled).await;
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
            }
        }

        active_control_macs.lock().await.remove(&mac);
    });

    let config = config.clone();
    let keyboard = Arc::new(std::sync::Mutex::new(keyboard));
    tokio::spawn(async move {
        loop {
            let keyboard_clone = keyboard.clone();
            let result = spawn_blocking(move || {
                let kb = keyboard_clone.lock().unwrap();
                kb.next_event(ReadFlag::NORMAL | ReadFlag::BLOCKING)
            })
            .await
            .unwrap();

            match result {
                Ok((_status, event)) => {
                    parse_keyboard_event(event, &config, &virtual_keyboard, &state_manager).await;
                }
                Err(e) => {
                    if let Some(libc::ENODEV) = e.raw_os_error() {
                        info!("Bluetooth device disconnected. Exiting task.");
                        state_manager.bluetooth_connection_stopped();
                        virtual_keyboard.lock().await.release_all_keys();
                        drop(shutdown_tx);
                        return;
                    } else {
                        warn!("Failed to read event: {:?}", e);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    });
}

// ── Key event parsing ────────────────────────────────────────────────────────

async fn parse_keyboard_event(
    event: InputEvent,
    config: &Config,
    virtual_keyboard: &Arc<Mutex<VirtualKeyboard>>,
    state_manager: &KeyboardStateManager,
) {
    if event.event_code == EventCode::EV_ABS(EV_ABS::ABS_MISC) {
        match event.value {
            0 => {
                debug!("No key pressed");
                virtual_keyboard.lock().await.release_all_keys();
            }
            199 => {
                debug!("Backlight key pressed");
                config
                    .keyboard_backlight_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            16 => {
                debug!("Brightness down key pressed");
                config
                    .brightness_down_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            32 => {
                debug!("Brightness up key pressed");
                config
                    .brightness_up_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            156 => {
                debug!("Swap up down display key pressed");
                config
                    .swap_up_down_display_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            124 => {
                debug!("Microphone mute key pressed");
                config
                    .microphone_mute_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            126 => {
                debug!("Emoji picker key pressed");
                config
                    .emoji_picker_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            134 => {
                debug!("MyASUS key pressed");
                config
                    .myasus_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            106 => {
                debug!("Toggle secondary display key pressed");
                config
                    .toggle_secondary_display_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            _ => {
                debug!("Unknown key pressed: {:?}", event);
                virtual_keyboard.lock().await.release_all_keys();
            }
        }
    }
}
