use std::{os::unix::io::AsRawFd, path::PathBuf, thread, time::Duration};

use evdev_rs::{
    Device, DeviceWrapper as _, EnableCodeData, InputEvent, ReadFlag, UInputDevice, UninitDevice,
    enums::{BusType, EV_ABS, EV_KEY, EV_MSC, EV_SYN, EventCode, EventType, InputProp},
};
use futures::StreamExt;
use inotify::{EventMask, Inotify, WatchMask};
use log::{debug, info, warn};
use nix::libc;
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::spawn_blocking,
    time::Instant,
};

use crate::events::Event;

/// How long after the last keypress the touchpad remains suppressed.
const DWT_TIMEOUT: Duration = Duration::from_millis(350);

/// Sentinel value used when the DWT timer is inactive (effectively "never fire").
const INACTIVE: Duration = Duration::from_secs(3600);

// EVIOCGRAB = _IOW('E', 0x90, int) — grab/release exclusive device access.
const EVIOCGRAB: u64 = 0x4004_4590;

// ── Touch state tracker ──────────────────────────────────────────────────────

const MAX_SLOTS: usize = 5;

#[derive(Clone, Copy)]
struct SlotState {
    tracking_id: i32,
    x: i32,
    y: i32,
    tool_type: i32,
}

impl Default for SlotState {
    fn default() -> Self {
        Self { tracking_id: -1, x: 0, y: 0, tool_type: 0 }
    }
}

/// Tracks MT/BTN state for either the real touchpad or the virtual device.
/// Used to detect what the virtual device already knows vs what the real device
/// currently reports, so we can emit only the diff at unsuppression time.
#[derive(Clone)]
struct TouchState {
    current_slot: usize,
    slots: [SlotState; MAX_SLOTS],
    btn_touch: i32,
    btn_tool_finger: i32,
    btn_tool_doubletap: i32,
    btn_tool_tripletap: i32,
    btn_tool_quadtap: i32,
    abs_x: i32,
    abs_y: i32,
}

impl TouchState {
    fn new() -> Self {
        Self {
            current_slot: 0,
            slots: [SlotState::default(); MAX_SLOTS],
            btn_touch: 0,
            btn_tool_finger: 0,
            btn_tool_doubletap: 0,
            btn_tool_tripletap: 0,
            btn_tool_quadtap: 0,
            abs_x: 0,
            abs_y: 0,
        }
    }

    fn update(&mut self, ev: &InputEvent) {
        match &ev.event_code {
            EventCode::EV_ABS(EV_ABS::ABS_MT_SLOT) => {
                let s = ev.value as usize;
                if s < MAX_SLOTS {
                    self.current_slot = s;
                }
            }
            EventCode::EV_ABS(EV_ABS::ABS_MT_TRACKING_ID) => {
                self.slots[self.current_slot].tracking_id = ev.value;
            }
            EventCode::EV_ABS(EV_ABS::ABS_MT_POSITION_X) => {
                self.slots[self.current_slot].x = ev.value;
            }
            EventCode::EV_ABS(EV_ABS::ABS_MT_POSITION_Y) => {
                self.slots[self.current_slot].y = ev.value;
            }
            EventCode::EV_ABS(EV_ABS::ABS_MT_TOOL_TYPE) => {
                self.slots[self.current_slot].tool_type = ev.value;
            }
            EventCode::EV_ABS(EV_ABS::ABS_X) => self.abs_x = ev.value,
            EventCode::EV_ABS(EV_ABS::ABS_Y) => self.abs_y = ev.value,
            EventCode::EV_KEY(EV_KEY::BTN_TOUCH) => self.btn_touch = ev.value,
            EventCode::EV_KEY(EV_KEY::BTN_TOOL_FINGER) => self.btn_tool_finger = ev.value,
            EventCode::EV_KEY(EV_KEY::BTN_TOOL_DOUBLETAP) => self.btn_tool_doubletap = ev.value,
            EventCode::EV_KEY(EV_KEY::BTN_TOOL_TRIPLETAP) => self.btn_tool_tripletap = ev.value,
            EventCode::EV_KEY(EV_KEY::BTN_TOOL_QUADTAP) => self.btn_tool_quadtap = ev.value,
            _ => {}
        }
    }
}

// ── Touch state reconciliation ───────────────────────────────────────────────

/// At DWT unsuppression: bring the virtual device's touch state in sync with
/// the real device by emitting only the *diff* (slot opens/closes and button
/// changes).  Crucially, if the same tracking ID is already active on the
/// virtual device, we skip re-establishing it — avoiding the double-TID error
/// that was preventing libinput from resuming pointer motion without a
/// lift+retouch.
fn reconcile_virt_with_real(virt_state: &TouchState, real_state: &TouchState, virt: &UInputDevice) {
    let time = std::time::SystemTime::now().try_into().unwrap();
    let put = |code: EventCode, value: i32| {
        let ev = InputEvent::new(&time, &code, value);
        if let Err(e) = virt.write_event(&ev) {
            warn!("DWT: reconcile write({code:?}={value}) failed: {e}");
        }
    };

    let mut changed = false;

    for i in 0..MAX_SLOTS {
        let v_tid = virt_state.slots[i].tracking_id;
        let r_tid = real_state.slots[i].tracking_id;
        if v_tid == r_tid {
            continue; // slot already in sync — no action (avoids double-TID)
        }
        put(EventCode::EV_ABS(EV_ABS::ABS_MT_SLOT), i as i32);
        changed = true;
        if v_tid >= 0 {
            put(EventCode::EV_ABS(EV_ABS::ABS_MT_TRACKING_ID), -1);
        }
        if r_tid >= 0 {
            put(EventCode::EV_ABS(EV_ABS::ABS_MT_TRACKING_ID), r_tid);
            put(EventCode::EV_ABS(EV_ABS::ABS_MT_POSITION_X), real_state.slots[i].x);
            put(EventCode::EV_ABS(EV_ABS::ABS_MT_POSITION_Y), real_state.slots[i].y);
            if real_state.slots[i].tool_type != 0 {
                put(EventCode::EV_ABS(EV_ABS::ABS_MT_TOOL_TYPE), real_state.slots[i].tool_type);
            }
        }
    }

    macro_rules! sync_btn {
        ($key:ident, $v:expr, $r:expr) => {
            // Normalize to 0/1: value=2 is EV_KEY auto-repeat ("key held"), which
            // libinput ignores for state changes.  If we inject =2 as the *first*
            // event for a button (virt had =0), libinput never registers a press and
            // keeps the touch inactive — pointer motion stays frozen.  Always send 1
            // (pressed) instead of 2 so libinput correctly starts tracking the touch.
            let nv = if $v >= 1 { 1i32 } else { 0i32 };
            let nr = if $r >= 1 { 1i32 } else { 0i32 };
            if nv != nr {
                put(EventCode::EV_KEY(EV_KEY::$key), nr);
                changed = true;
            }
        };
    }
    sync_btn!(BTN_TOUCH, virt_state.btn_touch, real_state.btn_touch);
    sync_btn!(BTN_TOOL_FINGER, virt_state.btn_tool_finger, real_state.btn_tool_finger);
    sync_btn!(BTN_TOOL_DOUBLETAP, virt_state.btn_tool_doubletap, real_state.btn_tool_doubletap);
    sync_btn!(BTN_TOOL_TRIPLETAP, virt_state.btn_tool_tripletap, real_state.btn_tool_tripletap);
    sync_btn!(BTN_TOOL_QUADTAP, virt_state.btn_tool_quadtap, real_state.btn_tool_quadtap);

    if changed {
        put(EventCode::EV_SYN(EV_SYN::SYN_REPORT), 0);
        debug!("DWT: touch state reconciled at unsuppression");
    } else {
        debug!("DWT: touch state already in sync at unsuppression — resuming forwarding");
    }
}

// ── Virtual device creation ──────────────────────────────────────────────────

/// Create a uinput virtual touchpad that exactly mirrors the capabilities of
/// `real`: all input properties, event types, key/abs/msc codes, and ABS axis
/// ranges are copied from the real device at runtime.
fn create_virtual_touchpad(real: &Device) -> Option<UInputDevice> {
    let u = UninitDevice::new()?;
    u.set_name("Zenbook Duo Daemon Improved Touchpad");
    u.set_bustype(BusType::BUS_VIRTUAL as u16);
    u.set_vendor_id(real.vendor_id());
    u.set_product_id(real.product_id());

    // Copy all input properties (INPUT_PROP_POINTER, INPUT_PROP_BUTTONPAD, …).
    for prop in &[
        InputProp::INPUT_PROP_POINTER,
        InputProp::INPUT_PROP_DIRECT,
        InputProp::INPUT_PROP_BUTTONPAD,
        InputProp::INPUT_PROP_SEMI_MT,
        InputProp::INPUT_PROP_TOPBUTTONPAD,
    ] {
        if real.has_property(prop) {
            if let Err(e) = u.enable_property(prop) {
                warn!("DWT: enable_property {prop:?} failed: {e}");
            }
        }
    }

    // Copy EV_KEY codes.
    if real.has_event_type(&EventType::EV_KEY) {
        for key in [
            EV_KEY::BTN_LEFT,
            EV_KEY::BTN_TOUCH,
            EV_KEY::BTN_TOOL_FINGER,
            EV_KEY::BTN_TOOL_DOUBLETAP,
            EV_KEY::BTN_TOOL_TRIPLETAP,
            EV_KEY::BTN_TOOL_QUADTAP,
            EV_KEY::BTN_TOOL_QUINTTAP,
        ] {
            let code = EventCode::EV_KEY(key);
            if real.has_event_code(&code) {
                if let Err(e) = u.enable(code) {
                    warn!("DWT: enable {key:?} failed: {e}");
                }
            }
        }
    }

    // Copy EV_ABS axes — must use enable_event_code with AbsInfo.
    if real.has_event_type(&EventType::EV_ABS) {
        for axis in [
            EV_ABS::ABS_X,
            EV_ABS::ABS_Y,
            EV_ABS::ABS_MT_SLOT,
            EV_ABS::ABS_MT_POSITION_X,
            EV_ABS::ABS_MT_POSITION_Y,
            EV_ABS::ABS_MT_TOOL_TYPE,
            EV_ABS::ABS_MT_TRACKING_ID,
            EV_ABS::ABS_MT_PRESSURE,
            EV_ABS::ABS_MT_TOUCH_MAJOR,
            EV_ABS::ABS_MT_TOUCH_MINOR,
            EV_ABS::ABS_MT_ORIENTATION,
        ] {
            let code = EventCode::EV_ABS(axis);
            if let Some(info) = real.abs_info(&code) {
                if let Err(e) = u.enable_event_code(&code, Some(EnableCodeData::AbsInfo(info))) {
                    warn!("DWT: enable_event_code {axis:?} failed: {e}");
                }
            }
        }
    }

    // Copy EV_MSC so MSC_TIMESTAMP events pass through cleanly.
    if real.has_event_code(&EventCode::EV_MSC(EV_MSC::MSC_TIMESTAMP)) {
        let _ = u.enable(EventCode::EV_MSC(EV_MSC::MSC_TIMESTAMP));
    }

    match UInputDevice::create_from_device(&u) {
        Ok(d) => {
            info!("DWT: virtual touchpad created");
            Some(d)
        }
        Err(e) => {
            warn!("DWT: create_from_device failed: {e}");
            None
        }
    }
}

// ── Keyboard reader thread ───────────────────────────────────────────────────

/// Spawn a blocking thread that signals `tx` on every keypress from the BT keyboard.
/// Exits when the device is removed or the receiver is dropped.
fn spawn_kb_reader(path: PathBuf, tx: mpsc::Sender<()>) {
    thread::spawn(move || {
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                warn!("DWT: open kb {} failed: {e}", path.display());
                return;
            }
        };
        let dev = match Device::new_from_file(file) {
            Ok(d) => d,
            Err(e) => {
                warn!("DWT: evdev kb {} failed: {e}", path.display());
                return;
            }
        };
        info!("DWT: keyboard reader active on {}", path.display());
        loop {
            match dev.next_event(ReadFlag::BLOCKING) {
                Ok((_, ev)) => {
                    let is_key = matches!(ev.event_code, EventCode::EV_KEY(_));
                    let is_abs_misc = ev.event_code == EventCode::EV_ABS(EV_ABS::ABS_MISC);
                    if (is_key || is_abs_misc) && ev.value > 0 {
                        if tx.blocking_send(()).is_err() {
                            break;
                        }
                    }
                }
                Err(e) if e.raw_os_error() == Some(libc::ENODEV) => {
                    info!("DWT: keyboard device removed");
                    break;
                }
                Err(e) => {
                    warn!("DWT: kb read error: {e}");
                    break;
                }
            }
        }
    });
}

// ── Session ──────────────────────────────────────────────────────────────────

enum InnerExit {
    DeviceGone,
    UsbAttached,
    Shutdown,
}

async fn run_dwt_session(
    tp_path: PathBuf,
    kb_path: PathBuf,
    event_rx: &mut broadcast::Receiver<Event>,
) -> InnerExit {
    // Spawn a blocking thread that:
    //   1. Opens the real touchpad
    //   2. Creates the virtual touchpad from its ABS capabilities
    //   3. Grabs the real device (compositor's libinput can no longer read it)
    //   4. Sends the virtual device back via oneshot
    //   5. Proxies all real events to the async task via mpsc
    //
    // When the async task drops ev_rx (session ends), blocking_send returns Err
    // and the thread exits, closing the real device fd and releasing EVIOCGRAB.
    let (virt_tx, virt_rx) = oneshot::channel::<UInputDevice>();
    let (ev_tx, mut ev_rx) = mpsc::channel::<InputEvent>(256);

    spawn_blocking(move || {
        let file = match std::fs::File::open(&tp_path) {
            Ok(f) => f,
            Err(e) => {
                warn!("DWT: open touchpad {} failed: {e}", tp_path.display());
                return;
            }
        };
        let raw_fd = file.as_raw_fd();
        let dev = match Device::new_from_file(file) {
            Ok(d) => d,
            Err(e) => {
                warn!("DWT: evdev touchpad failed: {e}");
                return;
            }
        };
        let virt = match create_virtual_touchpad(&dev) {
            Some(v) => v,
            None => {
                warn!("DWT: failed to create virtual touchpad");
                return;
            }
        };
        let ret = unsafe { libc::ioctl(raw_fd, EVIOCGRAB, 1_i32) };
        if ret < 0 {
            warn!("DWT: EVIOCGRAB failed: {}", std::io::Error::last_os_error());
        } else {
            info!("DWT: real touchpad grabbed, virtual proxy active");
        }
        if virt_tx.send(virt).is_err() {
            return; // async task already gone
        }
        loop {
            match dev.next_event(ReadFlag::BLOCKING) {
                Ok((_, ev)) => {
                    if ev_tx.blocking_send(ev).is_err() {
                        break; // async task dropped ev_rx — session ending
                    }
                }
                Err(e) if e.raw_os_error() == Some(libc::ENODEV) => {
                    info!("DWT: touchpad device removed");
                    break;
                }
                Err(e) => {
                    warn!("DWT: touchpad read error: {e}");
                    break;
                }
            }
        }
        // `dev` drops here: fd is closed → EVIOCGRAB is automatically released.
    });

    let virt = match virt_rx.await {
        Ok(v) => v,
        Err(_) => {
            warn!("DWT: proxy thread failed before sending virtual device");
            return InnerExit::DeviceGone;
        }
    };

    let (key_tx, mut key_rx) = mpsc::channel::<()>(32);
    spawn_kb_reader(kb_path, key_tx);

    let mut suppressed = false;
    let mut touch_state = TouchState::new(); // mirrors real device state
    let mut virt_state = TouchState::new();  // mirrors what we've forwarded to virtual device
    let dwt_sleep = tokio::time::sleep(INACTIVE);
    tokio::pin!(dwt_sleep);

    info!("DWT: session active ({}ms suppression after keypress)", DWT_TIMEOUT.as_millis());

    loop {
        tokio::select! {
            ev = ev_rx.recv() => {
                match ev {
                    Some(ev) => {
                        // Always track the real device's state so we can reconcile later.
                        touch_state.update(&ev);

                        if suppressed {
                            // Suppress: don't forward, just track.
                        } else {
                            // Filter duplicate MT_TRACKING_ID from BT firmware re-syncs.
                            // If the virtual device already has this TID on this slot,
                            // emitting it again causes a libevdev "double tracking ID" error.
                            let skip = if let EventCode::EV_ABS(EV_ABS::ABS_MT_TRACKING_ID) = ev.event_code {
                                if ev.value >= 0 {
                                    let slot_idx = touch_state.current_slot as usize;
                                    virt_state.slots.get(slot_idx).map(|s| s.tracking_id == ev.value).unwrap_or(false)
                                } else {
                                    false
                                }
                            } else {
                                false
                            };

                            if !skip {
                                if let Err(e) = virt.write_event(&ev) {
                                    warn!("DWT: write_event failed: {e}");
                                } else {
                                    virt_state.update(&ev);
                                }
                            }
                        }
                    }
                    None => {
                        // Proxy thread exited (device gone or session ending).
                        info!("DWT: touchpad proxy channel closed");
                        return InnerExit::DeviceGone;
                    }
                }
            }
            key = key_rx.recv() => {
                match key {
                    Some(()) => {
                        if !suppressed {
                            suppressed = true;
                            debug!("DWT: suppressing touchpad");
                        }
                        dwt_sleep.as_mut().reset(Instant::now() + DWT_TIMEOUT);
                    }
                    None => {
                        info!("DWT: keyboard reader stopped");
                        return InnerExit::DeviceGone;
                    }
                }
            }
            _ = &mut dwt_sleep => {
                if suppressed {
                    suppressed = false;
                    debug!("DWT: touchpad unsuppressed — reconciling state");
                    reconcile_virt_with_real(&virt_state, &touch_state, &virt);
                    virt_state = touch_state.clone();
                }
                dwt_sleep.as_mut().reset(Instant::now() + INACTIVE);
            }
            result = event_rx.recv() => {
                match result {
                    Ok(Event::KeyboardAttached(true)) => {
                        info!("DWT: USB keyboard attached, deactivating");
                        // Dropping `virt` removes the virtual device.
                        // Dropping `ev_rx` causes the proxy thread to exit,
                        // closing the real device fd and releasing EVIOCGRAB.
                        return InnerExit::UsbAttached;
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return InnerExit::Shutdown,
                }
            }
        }
    }
}

// ── Device discovery (inotify, same pattern as keyboard_bt.rs) ───────────────

async fn check_bt_device(
    path: &PathBuf,
    must_contain: &str,
    must_not_contain: &[&str],
    must_have_event_type: Option<EventType>,
) -> Option<()> {
    let path2 = path.clone();
    let mc = must_contain.to_string();
    let mnc: Vec<String> = must_not_contain.iter().map(|s| s.to_string()).collect();
    spawn_blocking(move || -> Option<()> {
        let file = std::fs::File::open(&path2).ok()?;
        let dev = Device::new_from_file(file).ok()?;
        let name = dev.name().unwrap_or("").to_string();
        let is_bt = dev.bustype() == 5; // BUS_BLUETOOTH
        let name_ok = name.contains(&mc) && mnc.iter().all(|ex| !name.contains(ex.as_str()));
        let type_ok = must_have_event_type
            .as_ref()
            .map_or(true, |et| dev.has_event_type(et));
        if is_bt && name_ok && type_ok { Some(()) } else { None }
    })
    .await
    .ok()?
}

async fn find_bt_device(
    must_contain: &str,
    must_not_contain: &[&str],
    must_have_event_type: Option<EventType>,
) -> Option<PathBuf> {
    let mut dir = tokio::fs::read_dir("/dev/input").await.ok()?;
    while let Ok(Some(entry)) = dir.next_entry().await {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .map_or(false, |n| n.starts_with("event"))
        {
            continue;
        }
        if check_bt_device(&path, must_contain, must_not_contain, must_have_event_type)
            .await
            .is_some()
        {
            return Some(path);
        }
    }
    None
}

async fn wait_for_bt_devices(
    event_rx: &mut broadcast::Receiver<Event>,
) -> Option<(PathBuf, PathBuf)> {
    // Set up inotify watch BEFORE scanning so we don't miss a CREATE that
    // races with the initial scan (classic TOCTOU: device appears after scan
    // but before inotify → stuck forever).
    let inotify = match Inotify::init() {
        Ok(i) => i,
        Err(e) => {
            warn!("DWT: inotify init failed: {e}");
            return None;
        }
    };
    if let Err(e) = inotify.watches().add("/dev/input/", WatchMask::CREATE) {
        warn!("DWT: inotify watch failed: {e}");
        return None;
    }
    let mut buf = [0u8; 1024];
    let mut stream = match inotify.into_event_stream(&mut buf) {
        Ok(s) => s,
        Err(e) => {
            warn!("DWT: inotify stream failed: {e}");
            return None;
        }
    };

    // Now scan what is already present (inotify is live, so nothing is missed).
    let mut kb_path = find_bt_device(
        "ASUS Zenbook Duo Keyboard",
        &["Touchpad", "Mouse"],
        Some(EventType::EV_KEY),
    )
    .await;
    let mut tp_path = find_bt_device("ASUS Zenbook Duo Keyboard Touchpad", &[], None).await;

    if kb_path.is_some() && tp_path.is_some() {
        return Some((kb_path.unwrap(), tp_path.unwrap()));
    }

    loop {
        tokio::select! {
            event = stream.next() => {
                let Some(Ok(ev)) = event else { continue };
                if !ev.mask.contains(EventMask::CREATE) { continue; }
                let Some(name) = ev.name else { continue };
                let name_str = name.to_str().unwrap_or("");
                if !name_str.starts_with("event") { continue; }
                let path = PathBuf::from("/dev/input/").join(name_str);
                if kb_path.is_none() {
                    if check_bt_device(&path, "ASUS Zenbook Duo Keyboard", &["Touchpad", "Mouse"], Some(EventType::EV_KEY)).await.is_some() {
                        info!("DWT: BT keyboard appeared at {}", path.display());
                        kb_path = Some(path.clone());
                    }
                }
                if tp_path.is_none() {
                    if check_bt_device(&path, "ASUS Zenbook Duo Keyboard Touchpad", &[], None).await.is_some() {
                        info!("DWT: BT touchpad appeared at {}", path.display());
                        tp_path = Some(path);
                    }
                }
                if kb_path.is_some() && tp_path.is_some() {
                    return Some((kb_path.unwrap(), tp_path.unwrap()));
                }
            }
            result = event_rx.recv() => {
                match result {
                    Ok(Event::KeyboardAttached(true)) => {
                        info!("DWT: USB attached while waiting for BT devices");
                        return None;
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        }
    }
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Software disable-while-typing for BT keyboard mode (root daemon).
pub fn start_task(initial_usb_attached: bool, event_tx: broadcast::Sender<Event>) {
    tokio::spawn(run(initial_usb_attached, event_tx));
}

async fn run(initial_usb_attached: bool, event_tx: broadcast::Sender<Event>) {
    let mut event_rx = event_tx.subscribe();
    let mut in_bt_mode = !initial_usb_attached;

    loop {
        if !in_bt_mode {
            loop {
                match event_rx.recv().await {
                    Ok(Event::KeyboardAttached(false)) => {
                        in_bt_mode = true;
                        break;
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }

        info!("DWT: BT mode — waiting for keyboard and touchpad...");

        let Some((kb_path, tp_path)) = wait_for_bt_devices(&mut event_rx).await else {
            in_bt_mode = false;
            continue;
        };

        info!("DWT: keyboard={}, touchpad={}", kb_path.display(), tp_path.display());

        match run_dwt_session(tp_path, kb_path, &mut event_rx).await {
            InnerExit::DeviceGone => {
                info!("DWT: device gone, waiting for reconnect...");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            InnerExit::UsbAttached => {
                in_bt_mode = false;
            }
            InnerExit::Shutdown => return,
        }
    }
}
