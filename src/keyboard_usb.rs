use std::{sync::Arc, time::Duration};

use futures::stream::StreamExt;
use log::{debug, info, warn};
use nusb::{
    Device, DeviceId, DeviceInfo,
    hotplug::HotplugEvent,
    transfer::{ControlOut, ControlType, In, Interrupt, Recipient},
};
use tokio::sync::{Mutex, broadcast};

use crate::{
    config::Config,
    events::Event,
    idle_detection::ActivityNotifier,
    parse_hex_string,
    state::{KeyboardBacklightState, KeyboardStateManager},
    virtual_keyboard::VirtualKeyboard,
};

pub async fn find_wired_keyboard(config: &Config) -> Option<DeviceInfo> {
    nusb::list_devices()
        .await
        .unwrap()
        .find(|d| d.vendor_id() == config.vendor_id() && d.product_id() == config.product_id())
}

/// Monitor USB keyboard hotplug events and start wired_keyboard_task when keyboard connects
pub fn start_usb_keyboard_monitor_task(
    config: &Config,
    mut current_keyboard: Option<(DeviceId, broadcast::Sender<()>)>,
    event_sender: broadcast::Sender<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
    activity_notifier: ActivityNotifier,
) {
    let config = config.clone();
    tokio::spawn(async move {
        let mut watch = nusb::watch_devices().unwrap();

        while let Some(event) = watch.next().await {
            match event {
                HotplugEvent::Connected(device)
                    if device.vendor_id() == config.vendor_id()
                        && device.product_id() == config.product_id() =>
                {
                    if let Some((prev_id, shutdown_tx)) = current_keyboard.take() {
                        info!(
                            "USB keyboard hotplug: replacing stale session {:?} with new connection",
                            prev_id
                        );
                        let _ = shutdown_tx.send(());
                        tokio::time::sleep(Duration::from_millis(120)).await;
                    }
                    current_keyboard = Some(
                        start_usb_keyboard_task(
                            &config,
                            device,
                            event_sender.subscribe(),
                            virtual_keyboard.clone(),
                            state_manager.clone(),
                            activity_notifier.clone(),
                        )
                        .await,
                    );
                }
                HotplugEvent::Disconnected(device_id) => {
                    if let Some((id, shutdown_tx)) = &current_keyboard
                        && id == &device_id
                    {
                        shutdown_tx.send(()).ok();
                        current_keyboard = None;
                    }
                }
                _ => {}
            }
        }
    });
}

pub async fn start_usb_keyboard_task(
    config: &Config,
    keyboard: DeviceInfo,
    mut event_receiver: broadcast::Receiver<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
    activity_notifier: ActivityNotifier,
) -> (DeviceId, broadcast::Sender<()>) {
    let (shutdown_tx, mut shutdown_rx1) = broadcast::channel::<()>(1);
    let device_id = keyboard.id();

    let keyboard_device = Arc::new(keyboard.open().await.unwrap());
    state_manager.set_usb_keyboard_attached(true);
    activity_notifier.notify();
    info!("USB connected");

    let interface_4 = keyboard_device.detach_and_claim_interface(4).await.unwrap();
    let mut endpoint_5 = interface_4.endpoint::<Interrupt, In>(0x85).unwrap();

    // enable fn keys
    keyboard_device
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &if config.fn_lock {
                    parse_hex_string("5ad04e00000000000000000000000000")
                } else {
                    parse_hex_string("5ad04e01000000000000000000000000")
                },
            },
            Duration::from_millis(100),
        )
        .await
        .unwrap();

    // Restore backlight state
    let backlight_state = state_manager.get_keyboard_backlight();
    send_backlight_state(&keyboard_device, backlight_state).await;

    // Restore mic mute LED state
    let mic_mute_state = state_manager.get_mic_mute_led();
    send_mute_microphone_state(&keyboard_device, mic_mute_state).await;

    // Create a cancellation token for the control task

    // Spawn a task to handle backlight/mic mute events
    let keyboard_device2 = keyboard_device.clone();
    let mut shutdown_rx2 = shutdown_rx1.resubscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx1.recv() => {
                    info!("USB control task shutting down");
                    break;
                }
                result = event_receiver.recv() => {
                    match result {
                        Ok(Event::Backlight(state)) => {
                            send_backlight_state(&keyboard_device2, state).await;
                        }
                        Ok(Event::MicMuteLed(enabled)) => {
                            send_mute_microphone_state(&keyboard_device2, enabled).await;
                        }
                        Ok(_) => {
                            // dont care about other events
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Skip lagged messages
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
            }
        }
    });

    let config = config.clone();
    tokio::spawn(async move {
        loop {
            while endpoint_5.pending() < 3 {
                endpoint_5.submit(vec![0u8; 64].into());
            }

            tokio::select! {
                _ = shutdown_rx2.recv() => {
                    info!("USB receive task shutting down");
                    state_manager.set_usb_keyboard_attached(false);
                    virtual_keyboard.lock().await.release_all_keys();
                    break;
                }
                completion = endpoint_5.next_complete() => {
                    match completion.status {
                        Ok(_) => {
                            let data = &completion.buffer[..completion.actual_len];
                            // endpoint 5 is not a HID device so the idle detection module needs to be notified manually
                            activity_notifier.notify();
                            parse_keyboard_data(data, &config, &virtual_keyboard, &state_manager)
                                .await;
                        }
                        Err(e) => {
                            warn!("USB error: {:?}", e);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    }
                }
            }
        }
    });

    (device_id, shutdown_tx)
}

async fn parse_keyboard_data(
    data: &[u8],
    config: &Config,
    virtual_keyboard: &Arc<Mutex<VirtualKeyboard>>,
    state_manager: &KeyboardStateManager,
) {
    // ASUS vendor HID reports on interrupt endpoint 5 (`0x5a` prefix = 90). Hardware allows only one
    // such special key at a time.
    //
    // Zenbook Duo row (Fn+F1–F3 mute / volume are ordinary evdev keys on the main keyboard node,
    // not on this vendor channel). Fn+F7 is also **not** here: firmware injects **Super+P**
    // (`KEY_LEFTMETA` + `KEY_P`) on the main keyboard for GNOME’s display-mode UI (join / mirror /
    // built-in only / external), so `evtest` on event3 shows meta+P while event4/event5 stay quiet.
    // Fn+F10 (Bluetooth pairing) is firmware/BlueZ — intentionally unmapped.
    //
    // | Physical key              | Vendor byte | Config action                    |
    // |---------------------------+------------+----------------------------------|
    // | Fn+F4 keyboard backlight  | 199        | keyboard_backlight_key           |
    // | Fn+F5 brightness down     | 16         | brightness_down_key              |
    // | Fn+F6 brightness up       | 32         | brightness_up_key                |
    // | Fn+F8 swap primaries      | 156        | swap_up_down_display_key         |
    // | Fn+F9 mic mute LED        | 124        | microphone_mute_key              |
    // | Fn+F11 emoji              | 126        | emoji_picker_key                 |
    // | Fn+F12 ASUS / MyASUS      | 134        | myasus_key                       |
    // | Key right of F12 (bottom)| 106        | toggle_secondary_display_key     |
    match data {
        [90, 0, 0, 0, 0, 0] => {
            debug!("No key pressed");
            virtual_keyboard.lock().await.release_all_keys();
        }
        [90, 199, 0, 0, 0, 0] => {
            // Fn+F4
            debug!("Backlight key pressed (Fn+F4)");
            config
                .keyboard_backlight_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        [90, 16, 0, 0, 0, 0] => {
            // Fn+F5
            debug!("Brightness down key pressed (Fn+F5)");
            config
                .brightness_down_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        [90, 32, 0, 0, 0, 0] => {
            // Fn+F6
            debug!("Brightness up key pressed (Fn+F6)");
            config
                .brightness_up_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        [90, 156, 0, 0, 0, 0] => {
            // Fn+F8
            debug!("Swap up down display key pressed (Fn+F8)");
            config
                .swap_up_down_display_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        [90, 124, 0, 0, 0, 0] => {
            // Fn+F9
            debug!("Microphone mute key pressed (Fn+F9)");
            config
                .microphone_mute_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        [90, 126, 0, 0, 0, 0] => {
            // Fn+F11
            debug!("Emoji picker key pressed (Fn+F11)");
            config
                .emoji_picker_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        [90, 134, 0, 0, 0, 0] => {
            // Fn+F12
            debug!("MyASUS key pressed (Fn+F12)");
            config
                .myasus_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        [90, 106, 0, 0, 0, 0] => {
            // Dedicated key right of F12 — bottom panel on/off
            debug!("Toggle secondary display key pressed (key right of F12)");
            config
                .toggle_secondary_display_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        _ => {
            debug!("Unknown key pressed: {:?}", data);
            virtual_keyboard.lock().await.release_all_keys();
        }
    }
}

async fn send_backlight_state(keyboard: &Arc<Device>, state: KeyboardBacklightState) {
    let data = match state {
        KeyboardBacklightState::Off => parse_hex_string("5abac5c4000000000000000000000000"),
        KeyboardBacklightState::Low => parse_hex_string("5abac5c4010000000000000000000000"),
        KeyboardBacklightState::Medium => parse_hex_string("5abac5c4020000000000000000000000"),
        KeyboardBacklightState::High => parse_hex_string("5abac5c4030000000000000000000000"),
    };

    if let Err(e) = keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &data,
            },
            Duration::from_millis(100),
        )
        .await
    {
        warn!("Failed to send backlight state: {:?}", e);
    }
}

async fn send_mute_microphone_state(keyboard: &Arc<Device>, state: bool) {
    let data = if state {
        // turn on microphone mute led
        parse_hex_string("5ad07c01000000000000000000000000")
    } else {
        parse_hex_string("5ad07c00000000000000000000000000")
    };

    if let Err(e) = keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &data,
            },
            Duration::from_millis(100),
        )
        .await
    {
        warn!("Failed to send mic mute state: {:?}", e);
    }
}
