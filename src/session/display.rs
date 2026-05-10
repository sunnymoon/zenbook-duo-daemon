use std::collections::HashMap;
use std::sync::Arc;
use log::{debug, error, info, warn};
use tokio::sync::broadcast;
use zbus::{Connection, proxy, zvariant::OwnedValue};
use futures::stream::StreamExt;


// ── Mutter D-Bus type aliases ─────────────────────────────────────────────────

type ModeProps = HashMap<String, OwnedValue>;
type MonitorMode = (String, i32, i32, f64, f64, Vec<f64>, ModeProps);
type PhysicalMonitor = ((String, String, String, String), Vec<MonitorMode>, HashMap<String, OwnedValue>);
type LmRef = (String, String, String, String);
type LogicalMonitor = (i32, i32, f64, u32, bool, Vec<LmRef>, HashMap<String, OwnedValue>);
type CurrentState = (u32, Vec<PhysicalMonitor>, Vec<LogicalMonitor>, HashMap<String, OwnedValue>);
type ApplyMonSpec = (String, String, HashMap<String, OwnedValue>);
type ApplyLm = (i32, i32, f64, u32, bool, Vec<ApplyMonSpec>);
type BacklightState = (u32, Vec<HashMap<String, OwnedValue>>);

const DEFAULT_DUO_SCALE: f64 = 5.0 / 3.0;
const RECONCILE_DEBOUNCE_MS: u64 = 1200;
const ORIENTATION_STABILIZATION_MS: u64 = 1800;
/// After phase 1 of the eDP-2-primary two-step apply, wait before phase 2 so KMS/Mutter can finish
/// committing the dual layout. See README "Dual display / Mutter apply ordering".
const EDPTWO_PRIMARY_PHASE1_STABILITY_MS: u64 = 300;


#[proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
pub trait DisplayConfig {
    fn get_current_state(&self) -> zbus::Result<CurrentState>;
    fn set_backlight(&self, serial: u32, connector: String, value: i32) -> zbus::Result<()>;
    fn apply_monitors_config(
        &self,
        serial: u32,
        method: u32,
        logical_monitors: Vec<ApplyLm>,
        properties: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;
    
    #[zbus(signal)]
    fn monitors_changed(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn backlight(&self) -> zbus::Result<BacklightState>;
}

// ── Layout helpers ────────────────────────────────────────────────────────────

fn logical_size(pw: i32, ph: i32, scale: f64, transform: u32) -> (i32, i32) {
    // Use floor() not round(): Mutter validates adjacency using floor division internally.
    // Using round() can produce a position 1px past the edge, causing "displays not adjacent" errors
    // at non-integer scales (e.g. scale=5/3: 2880/1.6667 rounds to 1728, floor also gives 1728,
    // but at other scales they can diverge).
    let lw = (pw as f64 / scale).floor() as i32;
    let lh = (ph as f64 / scale).floor() as i32;
    if transform == 1 || transform == 3 { (lh, lw) } else { (lw, lh) }
}

fn extract_all_modes(physical: &[PhysicalMonitor]) -> HashMap<String, (String, i32, i32)> {
    let mut map = HashMap::new();
    for (info, modes, _) in physical {
        let has_flag = |key: &str, m: &MonitorMode| {
            m.6.get(key)
                .and_then(|v: &OwnedValue| bool::try_from(v.clone()).ok())
                .unwrap_or(false)
        };
        let chosen = modes.iter().find(|m| has_flag("is-current", m))
            .or_else(|| modes.iter().find(|m| has_flag("is-preferred", m)))
            .or_else(|| modes.first());
        if let Some((mode_id, w, h, ..)) = chosen {
            map.insert(info.0.clone(), (mode_id.clone(), *w, *h));
        }
    }
    map
}

fn find_mode_matching_size(
    physical: &[PhysicalMonitor],
    connector: &str,
    width: i32,
    height: i32,
) -> Option<(String, i32, i32)> {
    physical
        .iter()
        .find(|(info, _, _)| info.0 == connector)
        .and_then(|(_, modes, _)| {
            modes
                .iter()
                .find(|(_, w, h, ..)| *w == width && *h == height)
                .or_else(|| modes.first())
                .map(|(mode_id, w, h, ..)| (mode_id.clone(), *w, *h))
        })
}

pub async fn apply_display_brightness_value(
    value: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let conn = Connection::session().await?;
    let display = DisplayConfigProxy::new(&conn).await?;
    let (serial, entries) = display.backlight().await?;

    for entry in entries {
        let connector = entry
            .get("connector")
            .and_then(|value| String::try_from(value.clone()).ok());
        let active = entry
            .get("active")
            .and_then(|value| bool::try_from(value.clone()).ok())
            .unwrap_or(false);
        let Some(connector) = connector else {
            continue;
        };
        if active && matches!(connector.as_str(), "eDP-1" | "eDP-2") {
            display.set_backlight(serial, connector, value as i32).await?;
        }
    }

    Ok(())
}

/// iio orientation string → Mutter transform u32.
/// Verified: normal=0, left-up=1, bottom-up=2, right-up=3.
fn orientation_to_transform(orientation: &str) -> u32 {
    match orientation {
        "left-up"   => 1,
        "bottom-up" => 2,
        "right-up"  => 3,
        _           => 0,
    }
}

/// Build Zenbook Duo logical monitors for a given transform.
/// Physical connector placement is fixed regardless of which display is primary:
///   t=0 normal:    eDP-1(0,0)   eDP-2(0,lh)   stacked, eDP-2 below
///   t=1 left-up:   eDP-2(0,0)   eDP-1(lw,0)   eDP-2 left, eDP-1 right
///   t=2 bottom-up: eDP-2(0,0)   eDP-1(0,lh)   eDP-2 above, eDP-1 below
///   t=3 right-up:  eDP-1(0,0)   eDP-2(lw,0)   eDP-1 left, eDP-2 right
///
/// **Mutter apply order:** the logical monitor with `primary=true` must be **last** in the vector
/// passed to `apply_monitors_config` (same as `gdctl set`: specify non-primary first, then primary).
/// Otherwise the Zenbook Duo stack can end up with duplicate logical rows for one connector.
fn build_duo_lms(
    primary_connector: &str,
    primary_mode: &(String, i32, i32),
    secondary: Option<(&str, &(String, i32, i32))>,
    transform: u32,
    scale: f64,
) -> Vec<ApplyLm> {
    let mut lms = vec![];
    if let Some((sc, sm)) = secondary {
        if (primary_connector == "eDP-1" && sc == "eDP-2")
            || (primary_connector == "eDP-2" && sc == "eDP-1")
        {
            let (edp1_mode, edp2_mode) = if primary_connector == "eDP-1" {
                (primary_mode, sm)
            } else {
                (sm, primary_mode)
            };
            let (e1lw, e1lh) = logical_size(edp1_mode.1, edp1_mode.2, scale, transform);
            let (e2lw, e2lh) = logical_size(edp2_mode.1, edp2_mode.2, scale, transform);
            let ((e1x, e1y), (e2x, e2y)) = match transform {
                0 => ((0, 0), (0, e1lh)),
                1 => ((e2lw, 0), (0, 0)),
                2 => ((0, e2lh), (0, 0)),
                3 => ((0, 0), (e1lw, 0)),
                _ => ((0, 0), (0, e1lh)),
            };

            let edp1_is_primary = primary_connector == "eDP-1";
            let edp2_is_primary = primary_connector == "eDP-2";

            lms.push((
                e1x, e1y, scale, transform, edp1_is_primary,
                vec![("eDP-1".to_string(), edp1_mode.0.clone(), HashMap::new())],
            ));
            lms.push((
                e2x, e2y, scale, transform, edp2_is_primary,
                vec![("eDP-2".to_string(), edp2_mode.0.clone(), HashMap::new())],
            ));
            // eDP-1-then-eDP-2 is correct when eDP-2 is primary (non-primary first). When eDP-1 is
            // primary, swap so the primary monitor is last for Mutter.
            if primary_connector == "eDP-1" {
                lms.swap(0, 1);
            }
        } else {
            let (plw, plh) = logical_size(primary_mode.1, primary_mode.2, scale, transform);
            let (slw, slh) = logical_size(sm.1, sm.2, scale, transform);
            let (px, py, sx, sy) = match transform {
                0 => (0,   0,   0,   plh),
                1 => (slw, 0,   0,   0),
                2 => (0,   slh, 0,   0),
                3 => (0,   0,   plw, 0),
                _ => (0,   0,   0,   plh),
            };
            lms.push((
                px, py, scale, transform, true,
                vec![(primary_connector.to_string(), primary_mode.0.clone(), HashMap::new())],
            ));
            lms.push((
                sx, sy, scale, transform, false,
                vec![(sc.to_string(), sm.0.clone(), HashMap::new())],
            ));
            // Non-primary logical monitor must precede primary for Mutter (see module note).
            lms.swap(0, 1);
        }
    } else {
        lms.push((
            0, 0, scale, transform, true,
            vec![(primary_connector.to_string(), primary_mode.0.clone(), HashMap::new())],
        ));
    }
    lms
}

/// Read current display config from logical monitors.
/// Returns (config, is_corrupted) where is_corrupted=true if same connector appears twice.
/// Mirrors Python's read_current_state() logic with duplicate detection.
pub fn read_current_config(logical: &[LogicalMonitor]) -> (HashMap<String, (i32, i32, f64, u32, bool)>, bool) {
    let mut config = HashMap::new();
    let mut is_corrupted = false;
    
    for lm in logical {
        if let Some(connector_ref) = lm.5.first() {
            let connector = connector_ref.0.clone();
            if config.contains_key(&connector) {
                warn!("Display: corruption detected - {} appears twice in logical monitors", connector);
                is_corrupted = true;
            }
            config.insert(connector, (lm.0, lm.1, lm.2, lm.3, lm.4));
        }
    }
    
    (config, is_corrupted)
}

/// Read scale from eDP-1 logical monitor. Falls back to primary logical monitor then DEFAULT.
/// eDP-1 is the reference: it never changes mode on its own and is always active.
fn read_edp1_scale(logical: &[LogicalMonitor]) -> f64 {
    if let Some(lm) = logical.iter().find(|lm| lm.5.first().map(|r| r.0.as_str()) == Some("eDP-1")) {
        return lm.2;
    }
    logical.iter().find(|lm| lm.4).map(|lm| lm.2).unwrap_or(DEFAULT_DUO_SCALE)
}

/// Returns true if the error is a Mutter "displays not adjacent" error.
fn is_non_adjacent_error<E: std::fmt::Display>(e: &E) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("adjacent")
}

/// Adjust the non-origin monitor's offset by `delta` pixels along the correct axis.
/// For left/right orientations (transforms 1,3): X axis. For normal/bottom-up (0,2): Y axis.
fn adjust_offset_pixel(lms: &[ApplyLm], transform: u32, delta: i32) -> Vec<ApplyLm> {
    lms.iter().cloned().map(|(x, y, scale, tr, primary, monitors)| {
        let (nx, ny) = match transform {
            1 | 3 => (if x != 0 { x + delta } else { x }, y),
            _     => (x, if y != 0 { y + delta } else { y }),
        };
        (nx, ny, scale, tr, primary, monitors)
    }).collect()
}

/// Apply a dual-monitor configuration with automatic ±1 pixel adjacency correction.
/// Returns Ok(true) = applied, Ok(false) = non-adjacent and unrecoverable after ±1px, Err = other error.
async fn apply_duo_config_with_adjacency_fix(
    display: &DisplayConfigProxy<'_>,
    serial: u32,
    lms: Vec<ApplyLm>,
    transform: u32,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    match display.apply_monitors_config(serial, 1, lms.clone(), HashMap::new()).await {
        Ok(()) => return Ok(true),
        Err(ref e) if is_non_adjacent_error(e) => {
            warn!("DisplayConfig: non-adjacent error on original config, trying +1px on offset axis");
        }
        Err(e) => return Err(e.into()),
    }

    let lms_p1 = adjust_offset_pixel(&lms, transform, 1);
    match display.apply_monitors_config(serial, 1, lms_p1, HashMap::new()).await {
        Ok(()) => return Ok(true),
        Err(ref e) if is_non_adjacent_error(e) => {
            warn!("DisplayConfig: +1px still non-adjacent, trying -1px from original");
        }
        Err(e) => return Err(e.into()),
    }

    let lms_m1 = adjust_offset_pixel(&lms, transform, -1);
    match display.apply_monitors_config(serial, 1, lms_m1, HashMap::new()).await {
        Ok(()) => Ok(true),
        Err(ref e) if is_non_adjacent_error(e) => {
            warn!("DisplayConfig: -1px also non-adjacent — unrecoverable adjacency failure");
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}

/// Compare requested logical monitors to Mutter's `GetCurrentState` snapshot.
///
/// `warn_on_field_mismatch`: when `false`, field differences are **debug** only (current vs desired
/// before apply — mismatch means we need an apply, not an error). When `true`, use **warn**
/// (post-apply verification).
fn requested_layout_matches_full(
    requested: &[ApplyLm],
    actual_config: &HashMap<String, (i32, i32, f64, u32, bool)>,
    actual_modes: &HashMap<String, (String, i32, i32)>,
    warn_on_field_mismatch: bool,
) -> bool {
    if requested.len() != actual_config.len() {
        return false;
    }

    for (i, lm_request) in requested.iter().enumerate() {
        let (x, y, scale, transform, is_primary, connectors) = lm_request;
        let Some((connector, mode_id, _)) = connectors.first() else {
            warn!("Display: requested LM[{i}] has no connector spec");
            return false;
        };

        let Some((actual_x, actual_y, actual_scale, actual_transform, actual_is_primary)) =
            actual_config.get(connector)
        else {
            warn!("Display: connector {connector} missing from actual config");
            return false;
        };

        let Some((actual_mode_id, _, _)) = actual_modes.get(connector) else {
            warn!("Display: connector {connector} missing current mode");
            return false;
        };

        let matches = x == actual_x
            && y == actual_y
            && (scale - actual_scale).abs() < 0.01
            && transform == actual_transform
            && is_primary == actual_is_primary
            && mode_id == actual_mode_id;
        if !matches {
            let msg = format!(
                "Display: LM[{i}] {connector} mismatch: requested ({x},{y},{scale},{transform},{is_primary},mode={mode_id}), got ({actual_x},{actual_y},{actual_scale},{actual_transform},{actual_is_primary},mode={actual_mode_id})"
            );
            if warn_on_field_mismatch {
                warn!("{msg}");
            } else {
                debug!("{msg} (current vs desired — apply needed)");
            }
            return false;
        }
    }

    true
}

#[derive(Debug)]
enum DisplayApplyError {
    /// Root daemon guard paused or over budget.
    GuardPaused,
    /// Could not query the root daemon; session must not apply without permission.
    GuardCheck(String),
    /// Mutter/D-Bus or verification failure after permission was granted.
    Apply(String),
}

impl std::fmt::Display for DisplayApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisplayApplyError::GuardPaused => write!(
                f,
                "root display apply guard is paused — run sudo zenbook-duo-daemon resume-display-applies"
            ),
            DisplayApplyError::GuardCheck(s) => write!(f, "display apply guard unreachable: {s}"),
            DisplayApplyError::Apply(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for DisplayApplyError {}

impl DisplayApplyError {
    fn is_guard_block(&self) -> bool {
        matches!(self, Self::GuardPaused | Self::GuardCheck(_))
    }
}

/// True when the current Mutter layout is **only eDP-1** (single logical monitor) and the desired
/// state is **dual with eDP-2 as primary**.
///
/// **Why two applies:** On Zenbook Duo + GNOME/Mutter (~50.x), a single `apply_monitors_config` that
/// both enables the second internal panel and moves the primary to eDP-2 in one shot often leaves
/// inconsistent logical-monitor state (duplicate connector on two LMs, both `primary: true` in
/// `GetCurrentState`, etc.). Manual `gdctl` experiments and journal evidence (`drmModeAtomicCommit:
/// Invalid argument`, `Page flip failed`, prior gnome-shell crashes in `meta_monitor_mode_foreach_crtc`)
/// point to KMS atomic / Mutter monitor graph updates failing or racing when too much changes at once.
///
/// **Phase 1:** dual layout with **eDP-1 still primary** (non-primary output specified first, primary
/// last — see [`build_duo_lms`]). **Phase 2:** swap to **eDP-2 primary** with the same ordering rule.
/// Between phases we sleep [`EDPTWO_PRIMARY_PHASE1_STABILITY_MS`] and re-read state (fresh **serial**)
/// before the second apply.
fn requires_phase1_edp2_primary_from_edp1_solo(
    current_corrupted: bool,
    edp2_should_be_enabled: bool,
    desired_primary_effective: &str,
    logical: &[LogicalMonitor],
    current_config: &HashMap<String, (i32, i32, f64, u32, bool)>,
) -> bool {
    !current_corrupted
        && edp2_should_be_enabled
        && desired_primary_effective == "eDP-2"
        && logical.len() == 1
        && current_config.len() == 1
        && current_config.contains_key("eDP-1")
}

/// Unified display state: read current → build desired → (permission) → apply once → verify.
/// All `apply_monitors_config` calls go through this path only.
///
/// **Two-phase eDP-2 primary:** when [`requires_phase1_edp2_primary_from_edp1_solo`] holds, this
/// performs an extra apply + stability delay before the final layout. **Do not merge into a single
/// apply** without re-validating on hardware; see README.
async fn apply_desired_display_state(
    display: &DisplayConfigProxy<'_>,
    keyboard_attached: bool,
    desired_secondary_enabled: bool,
    desired_primary: &str,
    desired_transform: u32,
) -> Result<bool, DisplayApplyError> {
    let (mut serial, physical, logical, _props) = display
        .get_current_state()
        .await
        .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
    let all_modes = extract_all_modes(&physical);
    let (current_config, current_corrupted) = read_current_config(&logical);

    let edp2_physically_available = all_modes.contains_key("eDP-2");
    let edp2_should_be_enabled =
        edp2_physically_available && desired_secondary_enabled && !keyboard_attached;

    let edp1_scale = read_edp1_scale(&logical);

    let desired_primary_effective = if desired_primary == "eDP-2" {
        if !edp2_physically_available || !desired_secondary_enabled || keyboard_attached {
            "eDP-1".to_string()
        } else {
            "eDP-2".to_string()
        }
    } else {
        "eDP-1".to_string()
    };

    let edp1_mode = all_modes
        .get("eDP-1")
        .ok_or_else(|| DisplayApplyError::Apply("No mode found for eDP-1".to_string()))?;
    let edp2_mode_if_duo: Option<(String, i32, i32)> = if edp2_should_be_enabled {
        Some(
            find_mode_matching_size(&physical, "eDP-2", edp1_mode.1, edp1_mode.2)
                .or_else(|| all_modes.get("eDP-2").cloned())
                .ok_or_else(|| DisplayApplyError::Apply("No mode found for eDP-2".to_string()))?,
        )
    } else {
        None
    };

    let desired_lms = if let Some(ref edp2_mode) = edp2_mode_if_duo {
        let (primary_mode, secondary_connector, secondary_mode) = if desired_primary_effective == "eDP-1" {
            (edp1_mode, "eDP-2", edp2_mode)
        } else {
            (edp2_mode, "eDP-1", edp1_mode)
        };
        build_duo_lms(
            &desired_primary_effective,
            primary_mode,
            Some((secondary_connector, secondary_mode)),
            desired_transform,
            edp1_scale,
        )
    } else {
        build_duo_lms(
            &desired_primary_effective,
            edp1_mode,
            None,
            desired_transform,
            edp1_scale,
        )
    };

    let current_matches_desired =
        !current_corrupted
            && requested_layout_matches_full(&desired_lms, &current_config, &all_modes, false);
    if current_matches_desired {
        info!("Display state already matches desired (no change needed)");
        return Ok(false);
    }

    let run_edp2_primary_phase1 = requires_phase1_edp2_primary_from_edp1_solo(
        current_corrupted,
        edp2_should_be_enabled,
        &desired_primary_effective,
        &logical,
        &current_config,
    );

    if run_edp2_primary_phase1 {
        let Some(ref edp2_mode) = edp2_mode_if_duo else {
            return Err(DisplayApplyError::Apply(
                "internal error: two-phase eDP-2 primary requires eDP-2 mode".to_string(),
            ));
        };
        let intermediate_lms = build_duo_lms(
            "eDP-1",
            edp1_mode,
            Some(("eDP-2", edp2_mode)),
            desired_transform,
            edp1_scale,
        );
        info!(
            "Display: two-phase eDP-2 primary — phase 1: enable dual layout with eDP-1 still primary, then pause for KMS/Mutter"
        );
        match crate::dbus_state::register_display_apply_attempt().await {
            Ok(crate::dbus_state::DisplayApplyPermit::Allowed) => {}
            Ok(crate::dbus_state::DisplayApplyPermit::Paused) => {
                return Err(DisplayApplyError::GuardPaused);
            }
            Err(e) => return Err(DisplayApplyError::GuardCheck(e)),
        }

        let applied = apply_duo_config_with_adjacency_fix(
            display,
            serial,
            intermediate_lms,
            desired_transform,
        )
        .await
        .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
        if !applied {
            return Err(DisplayApplyError::Apply(
                "Two-phase eDP-2 primary: phase 1 failed (dual layout with eDP-1 primary)".to_string(),
            ));
        }

        tokio::time::sleep(std::time::Duration::from_millis(
            EDPTWO_PRIMARY_PHASE1_STABILITY_MS,
        ))
        .await;

        let (serial_after_p1, _phys_p1, logical_after_p1, _) = display
            .get_current_state()
            .await
            .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
        let (cfg_p1, corrupt_p1) = read_current_config(&logical_after_p1);
        if corrupt_p1 || !cfg_p1.contains_key("eDP-1") || !cfg_p1.contains_key("eDP-2") {
            return Err(DisplayApplyError::Apply(
                "Two-phase eDP-2 primary: phase 1 did not leave a stable dual layout".to_string(),
            ));
        }
        serial = serial_after_p1;
    }

    match crate::dbus_state::register_display_apply_attempt().await {
        Ok(crate::dbus_state::DisplayApplyPermit::Allowed) => {}
        Ok(crate::dbus_state::DisplayApplyPermit::Paused) => {
            return Err(DisplayApplyError::GuardPaused);
        }
        Err(e) => return Err(DisplayApplyError::GuardCheck(e)),
    }

    if desired_lms.len() == 2 {
        let applied = apply_duo_config_with_adjacency_fix(display, serial, desired_lms.clone(), desired_transform)
            .await
            .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
        if !applied {
            return Err(DisplayApplyError::Apply(
                "Failed to apply desired dual-monitor layout after adjacency correction".to_string(),
            ));
        }
    } else {
        display
            .apply_monitors_config(serial, 1, desired_lms.clone(), HashMap::new())
            .await
            .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (_, physical_after, logical_after, _) = display
        .get_current_state()
        .await
        .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
    let (after_config, after_corrupted) = read_current_config(&logical_after);
    let after_modes = extract_all_modes(&physical_after);
    let verified =
        !after_corrupted
            && requested_layout_matches_full(&desired_lms, &after_config, &after_modes, true);
    if !verified {
        return Err(DisplayApplyError::Apply(
            "Desired display layout mismatch after apply".to_string(),
        ));
    }

    info!("Display state applied successfully");
    Ok(true)
}

/// Subscribe to D-Bus MonitorsChanged from Mutter when `desired_primary` may need eDP-2 hot-plug handling.
async fn subscribe_monitors_changed(
    display: &DisplayConfigProxy<'_>,
    availability_tx: broadcast::Sender<()>,
    desired_primary: Arc<tokio::sync::RwLock<String>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("DisplayConfig: subscribing to MonitorsChanged signal (event-driven, no polling)");

    let mut signal_rx = display.receive_monitors_changed().await?;

    while signal_rx.next().await.is_some() {
        let desired = desired_primary.read().await.clone();

        if desired != "eDP-2" {
            debug!("MonitorsChanged: desired_primary is {desired}, not eDP-2, skipping");
            continue;
        }

        if let Ok((_serial, _physical, logical, _)) = display.get_current_state().await {
            let (logical_map, _) = read_current_config(&logical);

            match logical_map.get("eDP-2") {
                Some((_, _, _, _, is_primary)) if !*is_primary => {
                    info!("MonitorsChanged: eDP-2 is available and desired, attempting to restore as primary");
                    let _ = availability_tx.send(());
                }
                Some((_, _, _, _, is_primary)) if *is_primary => {
                    debug!("MonitorsChanged: eDP-2 already primary, no action needed");
                }
                None => {
                    debug!("MonitorsChanged: eDP-2 not available");
                }
                _ => {}
            }
        }
    }

    Ok(())
}

fn effective_secondary_enabled(desired_secondary_enabled: bool, keyboard_attached: bool) -> bool {
    desired_secondary_enabled && !keyboard_attached
}

fn stop_availability_monitor(
    availability_task: &mut Option<tokio::task::JoinHandle<()>>,
) {
    if let Some(task) = availability_task.take() {
        task.abort();
        info!("DisplayConfig: stopped MonitorsChanged listener");
    }
}

fn ensure_availability_monitor(
    availability_task: &mut Option<tokio::task::JoinHandle<()>>,
    conn: &Connection,
    availability_tx: &broadcast::Sender<()>,
    desired_primary: &Arc<tokio::sync::RwLock<String>>,
) {
    let already_running = availability_task
        .as_ref()
        .is_some_and(|task| !task.is_finished());
    if already_running {
        return;
    }

    let conn_clone = conn.clone();
    let availability_tx_clone = availability_tx.clone();
    let desired_primary_clone = Arc::clone(desired_primary);
    *availability_task = Some(tokio::spawn(async move {
        match DisplayConfigProxy::new(&conn_clone).await {
            Ok(display) => {
                if let Err(e) = subscribe_monitors_changed(
                    &display,
                    availability_tx_clone,
                    desired_primary_clone,
                )
                .await
                {
                    error!("DisplayConfig: MonitorsChanged subscription error: {e}");
                }
            }
            Err(e) => {
                error!("DisplayConfig: failed to create proxy for MonitorsChanged listener: {e}");
            }
        }
    }));
}

fn update_availability_monitor_for_edp2(
    availability_task: &mut Option<tokio::task::JoinHandle<()>>,
    conn: &Connection,
    availability_tx: &broadcast::Sender<()>,
    desired_primary: &Arc<tokio::sync::RwLock<String>>,
    should_wait_for_edp2: bool,
) {
    if should_wait_for_edp2 {
        ensure_availability_monitor(availability_task, conn, availability_tx, desired_primary);
    } else {
        stop_availability_monitor(availability_task);
    }
}

async fn current_desired_transform(
    display: &DisplayConfigProxy<'_>,
) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
    match super::orientation::current_orientation().await {
        Ok(Some(orientation)) => Ok(orientation_to_transform(&orientation)),
        Ok(None) => Ok(display
            .get_current_state()
            .await?
            .2
            .iter()
            .find(|lm| lm.4)
            .map(|lm| lm.3)
            .unwrap_or(0)),
        Err(e) => {
            warn!("Display recovery: failed to read current orientation: {e}");
            Ok(display
                .get_current_state()
                .await?
                .2
                .iter()
                .find(|lm| lm.4)
                .map(|lm| lm.3)
                .unwrap_or(0))
        }
    }
}

async fn reconcile_display_state(
    display: &DisplayConfigProxy<'_>,
    conn: &Connection,
    availability_task: &mut Option<tokio::task::JoinHandle<()>>,
    availability_tx: &broadcast::Sender<()>,
    desired_primary: &Arc<tokio::sync::RwLock<String>>,
    desired_secondary: &Arc<tokio::sync::RwLock<bool>>,
    keyboard_attached: bool,
) -> Result<bool, DisplayApplyError> {
    let desired_transform = current_desired_transform(display)
        .await
        .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
    let desired_primary_value = desired_primary.read().await.clone();
    let desired_secondary_enabled = *desired_secondary.read().await;

    let changed = apply_desired_display_state(
        display,
        keyboard_attached,
        desired_secondary_enabled,
        &desired_primary_value,
        desired_transform,
    )
    .await?;

    let should_wait_for_edp2 =
        desired_primary_value == "eDP-2"
            && effective_secondary_enabled(desired_secondary_enabled, keyboard_attached);
    update_availability_monitor_for_edp2(
        availability_task,
        conn,
        availability_tx,
        desired_primary,
        should_wait_for_edp2,
    );
    Ok(changed)
}

async fn attempt_display_recovery(
    conn: &Connection,
    desired_primary: &Arc<tokio::sync::RwLock<String>>,
    desired_secondary: &Arc<tokio::sync::RwLock<bool>>,
    keyboard_attached: &Arc<tokio::sync::RwLock<bool>>,
) -> Result<(), DisplayApplyError> {
    let display = DisplayConfigProxy::new(conn)
        .await
        .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
    let desired_transform = current_desired_transform(&display)
        .await
        .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;

    let kb_before = *keyboard_attached.read().await;
    let sec_before = *desired_secondary.read().await;
    let pri_before = desired_primary.read().await.clone();

    apply_desired_display_state(
        &display,
        kb_before,
        sec_before,
        &pri_before,
        desired_transform,
    )
    .await?;

    let kb_after = *keyboard_attached.read().await;
    let sec_after = *desired_secondary.read().await;
    let pri_after = desired_primary.read().await.clone();
    if (kb_before, sec_before, pri_before) != (kb_after, sec_after, pri_after) {
        return Err(DisplayApplyError::Apply(
            "keyboard or desired display inputs changed during recovery; not treating as layout convergence"
                .to_string(),
        ));
    }

    Ok(())
}

fn ensure_display_recovery_task(
    recovery_task: &mut Option<tokio::task::JoinHandle<()>>,
    conn: &Connection,
    desired_primary: &Arc<tokio::sync::RwLock<String>>,
    desired_secondary: &Arc<tokio::sync::RwLock<bool>>,
    keyboard_attached: &Arc<tokio::sync::RwLock<bool>>,
    reason: &'static str,
) {
    let already_running = recovery_task
        .as_ref()
        .is_some_and(|task| !task.is_finished());
    if already_running {
        return;
    }

    let conn = conn.clone();
    let desired_primary = Arc::clone(desired_primary);
    let desired_secondary = Arc::clone(desired_secondary);
    let keyboard_attached = Arc::clone(keyboard_attached);
    *recovery_task = Some(tokio::spawn(async move {
        const MAX_ATTEMPTS: usize = 20;
        const DELAY_SECS: u64 = 1;

        for attempt in 1..=MAX_ATTEMPTS {
            match attempt_display_recovery(
                &conn,
                &desired_primary,
                &desired_secondary,
                &keyboard_attached,
            )
            .await
            {
                Ok(()) => {
                    info!(
                        "Display recovery: converged successfully on attempt {}/{} after {}",
                        attempt, MAX_ATTEMPTS, reason
                    );
                    return;
                }
                Err(e) if e.is_guard_block() => {
                    warn!("Display recovery: aborting — {e}");
                    if let Err(notify_err) = super::notifications::send_notification(
                        "Display applies blocked",
                        "The root display guard paused changes or could not be reached, so recovery stopped. If applies were paused due to repeated failures, run: sudo zenbook-duo-daemon resume-display-applies",
                        "dialog-warning",
                        0,
                        2,
                    )
                    .await
                    {
                        warn!("Display recovery: failed to send notification: {notify_err}");
                    }
                    return;
                }
                Err(e) => {
                    warn!(
                        "Display recovery: attempt {}/{} failed after {}: {}",
                        attempt, MAX_ATTEMPTS, reason, e
                    );
                    if attempt < MAX_ATTEMPTS {
                        let body = format!(
                            "Display recovery attempt {attempt}/{MAX_ATTEMPTS} did not converge yet. Retrying automatically."
                        );
                        if let Err(notify_err) = super::notifications::send_transient_notification(
                            "Display recovery retrying",
                            &body,
                            "dialog-warning",
                            800,
                            1, // urgency: normal/yellow
                        )
                        .await
                        {
                            warn!("Display recovery: failed to send transient retry warning: {notify_err}");
                        }
                    }
                }
            }

            if attempt < MAX_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_secs(DELAY_SECS)).await;
            }
        }

        if let Err(e) = super::notifications::send_notification(
            "Display Recovery Failed",
            "Zenbook Duo daemon could not restore the desired display layout after repeated retries.",
            "dialog-error",
            0, // persistent — stays until dismissed
            2, // urgency: critical
        )
        .await
        {
            warn!("Display recovery: failed to send desktop notification: {e}");
        }
    }));
}

fn cancel_display_recovery_task(
    recovery_task: &mut Option<tokio::task::JoinHandle<()>>,
    reason: &str,
) {
    if let Some(task) = recovery_task.take() {
        if !task.is_finished() {
            info!("Display recovery: cancelling in-flight recovery after {}", reason);
            task.abort();
        }
    }
}

fn schedule_reconcile(
    debounce_deadline: &mut Option<tokio::time::Instant>,
    pending_reason: &mut &'static str,
    new_reason: &'static str,
    delay_ms: u64,
) {
    let now = tokio::time::Instant::now();
    let new_deadline = now + std::time::Duration::from_millis(delay_ms);
    if let Some(old_deadline) = *debounce_deadline {
        info!(
            "Display debounce: event='{}' rescheduling previous='{}' old_due_in={}ms new_due_in={}ms",
            new_reason,
            *pending_reason,
            old_deadline.saturating_duration_since(now).as_millis(),
            delay_ms
        );
    } else {
        info!(
            "Display debounce: event='{}' scheduling apply in {}ms",
            new_reason,
            delay_ms
        );
    }
    *pending_reason = new_reason;
    *debounce_deadline = Some(new_deadline);
}

pub async fn run(
    mut orient_rx: broadcast::Receiver<String>,
    mut kb_rx: broadcast::Receiver<bool>,
    mut desired_secondary_rx: broadcast::Receiver<bool>,
    mut desired_primary_rx: broadcast::Receiver<String>,
    secondary_result_tx: tokio::sync::mpsc::Sender<()>,
    kb_result_tx: tokio::sync::mpsc::Sender<()>,
    desired_primary: Arc<tokio::sync::RwLock<String>>,
    desired_secondary: Arc<tokio::sync::RwLock<bool>>,
) {
    let conn = loop {
        match Connection::session().await {
            Ok(c) => break c,
            Err(e) => {
                error!("DisplayConfig: session D-Bus unavailable: {e}, retrying in 3s");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        }
    };

    let display = loop {
        match DisplayConfigProxy::new(&conn).await {
            Ok(d) => break d,
            Err(e) => {
                error!("DisplayConfig: proxy unavailable: {e}, retrying in 3s");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        }
    };

    // Broadcast channel for availability checks triggered by display state changes
    let (availability_tx, _) = broadcast::channel::<()>(8);

    if let Ok(transform) = current_desired_transform(&display).await {
        info!("Display: startup desired transform={transform}");
    }
    let mut availability_rx = availability_tx.subscribe();
    let mut availability_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut recovery_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut keyboard_attached = false;
    let keyboard_attached_state = Arc::new(tokio::sync::RwLock::new(false));
    let mut keyboard_state_initialized = false;
    let mut delayed_reconciles_deepness: u32 = 0;
    let mut quiet_until: Option<tokio::time::Instant> = None;
    let mut debounce_deadline: Option<tokio::time::Instant> = None;
    let mut pending_reason: &'static str = "display state update";
    let mut changed_apply_waiting_followup = false;

    // Do not apply any desired_primary at startup from the local default.
    // Root daemon is the authority and will publish the real desired_primary
    // over D-Bus; acting before that causes visible primary-display flips.

    loop {
        tokio::select! {
            _ = async {
                if let Some(deadline) = debounce_deadline {
                    tokio::time::sleep_until(deadline).await;
                }
            }, if debounce_deadline.is_some() => {
                info!("Display debounce: applying scheduled reconcile for '{}'", pending_reason);
                debounce_deadline = None;
                if let Some(until) = quiet_until {
                    if tokio::time::Instant::now() < until {
                        info!("Display reconcile: quiet period active, skipping apply");
                        continue;
                    }
                    quiet_until = None;
                    delayed_reconciles_deepness = 0;
                }

                cancel_display_recovery_task(&mut recovery_task, pending_reason);
                let start = std::time::Instant::now();
                match reconcile_display_state(
                    &display,
                    &conn,
                    &mut availability_task,
                    &availability_tx,
                    &desired_primary,
                    &desired_secondary,
                    keyboard_attached,
                )
                .await
                {
                    Ok(changed) => {
                        if changed {
                            changed_apply_waiting_followup = true;
                        } else {
                            delayed_reconciles_deepness = 0;
                            changed_apply_waiting_followup = false;
                        }
                        info!(
                            "Display reconcile completed in {:.2}ms",
                            start.elapsed().as_secs_f64() * 1000.0
                        );
                    }
                    Err(e) if e.is_guard_block() => {
                        warn!("Display reconcile skipped ({e}); not starting recovery without apply guard");
                        delayed_reconciles_deepness = 0;
                        changed_apply_waiting_followup = false;
                        info!(
                            "Display reconcile completed in {:.2}ms",
                            start.elapsed().as_secs_f64() * 1000.0
                        );
                    }
                    Err(e) => {
                        warn!("Display reconcile failed after {pending_reason}: {e}");
                        ensure_display_recovery_task(
                            &mut recovery_task,
                            &conn,
                            &desired_primary,
                            &desired_secondary,
                            &keyboard_attached_state,
                            pending_reason,
                        );
                    }
                }
            }
            msg = orient_rx.recv() => match msg {
                Ok(_orientation) => {
                    if let Some(until) = quiet_until {
                        if tokio::time::Instant::now() < until {
                            info!("Display reconcile: dropping orientation signal during quiet period");
                            continue;
                        }
                        quiet_until = None;
                        delayed_reconciles_deepness = 0;
                        changed_apply_waiting_followup = false;
                    }
                    if changed_apply_waiting_followup {
                        delayed_reconciles_deepness = delayed_reconciles_deepness.saturating_add(1);
                        changed_apply_waiting_followup = false;
                    }
                    schedule_reconcile(
                        &mut debounce_deadline,
                        &mut pending_reason,
                        "orientation update",
                        ORIENTATION_STABILIZATION_MS,
                    );
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Display handler lagged by {n}");
                    schedule_reconcile(
                        &mut debounce_deadline,
                        &mut pending_reason,
                        "orientation lag",
                        ORIENTATION_STABILIZATION_MS,
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = desired_secondary_rx.recv() => match msg {
                Ok(enable) => {
                    *desired_secondary.write().await = enable;
                    // ACK is receive-only; display apply is performed by debounced reconciler.
                    let _ = secondary_result_tx.send(()).await;

                    if let Some(until) = quiet_until {
                        if tokio::time::Instant::now() < until {
                            info!("Display reconcile: dropping desired_secondary signal during quiet period");
                            continue;
                        }
                        quiet_until = None;
                        delayed_reconciles_deepness = 0;
                        changed_apply_waiting_followup = false;
                    }
                    if changed_apply_waiting_followup {
                        delayed_reconciles_deepness = delayed_reconciles_deepness.saturating_add(1);
                        changed_apply_waiting_followup = false;
                    }
                    schedule_reconcile(
                        &mut debounce_deadline,
                        &mut pending_reason,
                        "desired secondary update",
                        RECONCILE_DEBOUNCE_MS,
                    );
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("Desired secondary handler lagged by {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = kb_rx.recv() => match msg {
                Ok(attached) => {
                    let previous_keyboard_attached = keyboard_attached;
                    keyboard_attached = attached;
                    *keyboard_attached_state.write().await = attached;
                    info!("Display: keyboard_attached={attached}");
                    // ACK is receive-only; display apply is performed by debounced reconciler.
                    let _ = kb_result_tx.send(()).await;

                    if !keyboard_state_initialized {
                        keyboard_state_initialized = true;
                        if !attached {
                            info!("Display: initial keyboard state sync is detached; skipping detach restore actions");
                            continue;
                        }
                    } else if attached == previous_keyboard_attached {
                        info!("Display: keyboard_attached unchanged; skipping duplicate edge handling");
                        continue;
                    }

                    if let Some(until) = quiet_until {
                        if tokio::time::Instant::now() < until {
                            info!("Display reconcile: dropping keyboard signal during quiet period");
                            continue;
                        }
                        quiet_until = None;
                        delayed_reconciles_deepness = 0;
                        changed_apply_waiting_followup = false;
                    }
                    if changed_apply_waiting_followup {
                        delayed_reconciles_deepness = delayed_reconciles_deepness.saturating_add(1);
                        changed_apply_waiting_followup = false;
                    }
                    schedule_reconcile(
                        &mut debounce_deadline,
                        &mut pending_reason,
                        "keyboard attach state update",
                        RECONCILE_DEBOUNCE_MS,
                    );
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("Keyboard handler lagged by {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = desired_primary_rx.recv() => match msg {
                Ok(desired) => {
                    *desired_primary.write().await = desired;
                    if let Some(until) = quiet_until {
                        if tokio::time::Instant::now() < until {
                            info!("Display reconcile: dropping desired_primary signal during quiet period");
                            continue;
                        }
                        quiet_until = None;
                        delayed_reconciles_deepness = 0;
                        changed_apply_waiting_followup = false;
                    }
                    if changed_apply_waiting_followup {
                        delayed_reconciles_deepness = delayed_reconciles_deepness.saturating_add(1);
                        changed_apply_waiting_followup = false;
                    }
                    schedule_reconcile(
                        &mut debounce_deadline,
                        &mut pending_reason,
                        "desired primary update",
                        RECONCILE_DEBOUNCE_MS,
                    );
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("DesiredPrimary handler lagged by {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = availability_rx.recv() => match msg {
                Ok(_) => {
                    if let Some(until) = quiet_until {
                        if tokio::time::Instant::now() < until {
                            info!("Display reconcile: dropping availability signal during quiet period");
                            continue;
                        }
                        quiet_until = None;
                        delayed_reconciles_deepness = 0;
                        changed_apply_waiting_followup = false;
                    }
                    if changed_apply_waiting_followup {
                        delayed_reconciles_deepness = delayed_reconciles_deepness.saturating_add(1);
                        changed_apply_waiting_followup = false;
                    }
                    schedule_reconcile(
                        &mut debounce_deadline,
                        &mut pending_reason,
                        "display availability update",
                        RECONCILE_DEBOUNCE_MS,
                    );
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }

        if delayed_reconciles_deepness >= 20 {
            let quiet_period = std::time::Duration::from_secs(60);
            let timeout = std::cmp::max(
                std::time::Duration::from_secs(1),
                quiet_period.saturating_sub(std::time::Duration::from_secs(1)),
            );
            let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
            if let Err(e) = super::notifications::send_transient_notification(
                "Display changes throttled",
                "You are changing your mind too fast or hardware is sending too many display-change signals. Quieting down for 1 minute.",
                "dialog-warning",
                timeout_ms,
                1,
            ).await {
                warn!("Display reconcile: failed to send throttling warning: {e}");
            }
            quiet_until = Some(tokio::time::Instant::now() + quiet_period);
            debounce_deadline = None;
            delayed_reconciles_deepness = 0;
            changed_apply_waiting_followup = false;
        }
    }
}
