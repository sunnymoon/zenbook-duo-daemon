use std::collections::{HashMap, HashSet};
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

async fn wait_for_connector_mode(
    display: &DisplayConfigProxy<'_>,
    connector: &str,
) -> Result<Option<CurrentState>, Box<dyn std::error::Error + Send + Sync>> {
    const ATTEMPTS: usize = 10;
    const DELAY_MS: u64 = 150;

    for attempt in 1..=ATTEMPTS {
        let state = display.get_current_state().await?;
        if extract_all_modes(&state.1).contains_key(connector) {
            if attempt > 1 {
                info!("Display: connector {} modes became available after {} attempts", connector, attempt);
            }
            return Ok(Some(state));
        }

        if attempt < ATTEMPTS {
            debug!(
                "Display: connector {} modes not available yet (attempt {}/{}), waiting",
                connector, attempt, ATTEMPTS
            );
            tokio::time::sleep(std::time::Duration::from_millis(DELAY_MS)).await;
        }
    }

    Ok(None)
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
        }
    } else {
        lms.push((
            0, 0, scale, transform, true,
            vec![(primary_connector.to_string(), primary_mode.0.clone(), HashMap::new())],
        ));
    }
    lms
}

async fn apply_rotation(
    display: &DisplayConfigProxy<'_>,
    orientation: &str,
    keyboard_attached: bool,
    desired_secondary_enabled: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    const MAX_ATTEMPTS: usize = 2;
    let transform = orientation_to_transform(orientation);

    for attempt in 1..=MAX_ATTEMPTS {
        info!("Rotation: attempt {}/{} for orientation={}", attempt, MAX_ATTEMPTS, orientation);

        // Step 1: Rebuild full config
        let (serial, physical, logical, _): CurrentState = display.get_current_state().await?;
        let all_modes = extract_all_modes(&physical);

        // Scale always comes from eDP-1 (stable reference, never changes mode on its own).
        let scale = read_edp1_scale(&logical);

        let primary_lm = logical.iter().find(|lm| lm.4);
        let primary_connector = primary_lm
            .and_then(|lm| lm.5.first())
            .map(|r| r.0.as_str())
            .unwrap_or("eDP-1");
        let primary_mode = all_modes.get(primary_connector).ok_or("no mode for primary")?;

        // Determine secondary from desired state (not current logical).
        // If GNOME reverted our dual-monitor config between events, current logical state may
        // show only one display — using desired state prevents permanent loss of eDP-2.
        let include_secondary = desired_secondary_enabled && !keyboard_attached;
        let sec_connector_name = if primary_connector == "eDP-1" { "eDP-2" } else { "eDP-1" };
        let secondary = if include_secondary {
            all_modes.get(sec_connector_name).map(|m| (sec_connector_name, m))
        } else {
            None
        };
        let secondary_connector: Option<String> = secondary.map(|(c, _)| c.to_string());

        debug!("Rotation: physical monitors available: {:?}", all_modes.keys().collect::<Vec<_>>());
        debug!("Rotation: logical monitors count: {}", logical.len());
        for (i, lm) in logical.iter().enumerate() {
            if let Some(conn) = lm.5.first() {
                debug!("  LM[{}]: connector={} primary={}", i, conn.0, lm.4);
            }
        }

        info!(
            "Rotation: building config with primary={} secondary={:?} transform={}",
            primary_connector, secondary_connector, transform
        );

        let lms = build_duo_lms(primary_connector, primary_mode, secondary, transform, scale);

        let (config_before, is_corrupted_before) = read_current_config(&logical);
        if !is_corrupted_before && requested_layout_matches_config(&lms, &config_before) {
            info!("Rotation: current config already matches desired state");
            return Ok(());
        }

        // Step 2: Apply config (with non-adjacent pixel fix for dual-monitor configs)
        if lms.len() > 1 {
            match apply_duo_config_with_adjacency_fix(display, serial, lms.clone(), transform).await? {
                true => {}
                false => {
                    // Non-adjacent unrecoverable: can't fix adjacency, let recovery handle
                    if attempt < MAX_ATTEMPTS {
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                        continue;
                    }
                    return Err("Rotation: non-adjacent error unrecoverable after ±1px adjustment".into());
                }
            }
        } else {
            display.apply_monitors_config(serial, 1, lms.clone(), HashMap::new()).await?;
        }

        // Step 3: Verify applied state matches desired state
        let (_, _, logical_after, _): CurrentState = display.get_current_state().await?;
        let (config_after, is_corrupted) = read_current_config(&logical_after);

        if is_corrupted {
            warn!("Rotation: config is corrupted (duplicate connector), retrying...");
            if attempt < MAX_ATTEMPTS {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                continue;
            } else {
                return Err("Rotation: config remains corrupted after retries".into());
            }
        }

        if requested_layout_matches_config(&lms, &config_after) {
            info!("Rotation: applied config matches desired state");
            return Ok(());
        } else {
            warn!("Rotation: applied config doesn't match desired, retrying...");
            if attempt < MAX_ATTEMPTS {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                continue;
            } else {
                return Err("Rotation: config mismatch persists after retries".into());
            }
        }
    }

    Err("Rotation: failed after all attempts".into())
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
                warn!("SwapDisplays: corruption detected - {} appears twice in logical monitors", connector);
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

fn requested_layout_matches_config(
    requested: &[ApplyLm],
    actual: &HashMap<String, (i32, i32, f64, u32, bool)>,
) -> bool {
    for (i, lm_request) in requested.iter().enumerate() {
        let (x, y, scale, transform, is_primary, connectors) = lm_request;
        if let Some(connector) = connectors.first().map(|c| c.0.as_str()) {
            if let Some((actual_x, actual_y, actual_scale, actual_transform, actual_is_primary)) =
                actual.get(connector)
            {
                let matches = x == actual_x
                    && y == actual_y
                    && (scale - actual_scale).abs() < 0.01
                    && transform == actual_transform
                    && is_primary == actual_is_primary;
                if !matches {
                    warn!(
                        "Rotation: LM[{}] {} mismatch: requested ({},{},{},{},{}), got ({},{},{},{},{})",
                        i,
                        connector,
                        x,
                        y,
                        scale,
                        transform,
                        is_primary,
                        actual_x,
                        actual_y,
                        actual_scale,
                        actual_transform,
                        actual_is_primary
                    );
                    return false;
                }
            } else {
                warn!("Rotation: LM[{}] connector {} not in applied config", i, connector);
                return false;
            }
        }
    }

    true
}

/// Ensure both displays exist. If one is missing, mirror the other.
/// Mirrors Python's incomplete state handling.
async fn ensure_both_displays(
    _display: &DisplayConfigProxy<'_>,
    _serial: u32,
    physical: &[PhysicalMonitor],
    all_modes: &HashMap<String, (String, i32, i32)>,
    config: &mut HashMap<String, (i32, i32, f64, u32, bool)>,
) -> Result<(), String> {
    if config.contains_key("eDP-1") && config.contains_key("eDP-2") {
        return Ok(());
    }

    info!("SwapDisplays: incomplete state, recovering...");

    if config.contains_key("eDP-1") && !config.contains_key("eDP-2") {
        let (_, _, scale, transform, is_primary) = config["eDP-1"];
        let primary_connector = if is_primary { "eDP-1" } else { "eDP-2" };
        let primary_mode = all_modes.get("eDP-1").ok_or("No mode for eDP-1")?;
        let secondary_mode = find_mode_matching_size(physical, "eDP-2", primary_mode.1, primary_mode.2)
            .or_else(|| all_modes.get("eDP-2").cloned())
            .ok_or("No mode for eDP-2")?;
        for (x, y, scale, transform, is_primary, monitors) in
            build_duo_lms(primary_connector, primary_mode, Some(("eDP-2", &secondary_mode)), transform, scale)
        {
            if let Some((connector, _, _)) = monitors.first() {
                config.insert(connector.clone(), (x, y, scale, transform, is_primary));
            }
        }
        info!("SwapDisplays: recovered missing eDP-2 using transform-aware layout");
    } else if config.contains_key("eDP-2") && !config.contains_key("eDP-1") {
        let (_, _, scale, transform, is_primary) = config["eDP-2"];
        let primary_connector = if is_primary { "eDP-2" } else { "eDP-1" };
        let primary_mode = all_modes.get("eDP-2").ok_or("No mode for eDP-2")?;
        let secondary_mode = find_mode_matching_size(physical, "eDP-1", primary_mode.1, primary_mode.2)
            .or_else(|| all_modes.get("eDP-1").cloned())
            .ok_or("No mode for eDP-1")?;
        for (x, y, scale, transform, is_primary, monitors) in
            build_duo_lms(primary_connector, primary_mode, Some(("eDP-1", &secondary_mode)), transform, scale)
        {
            if let Some((connector, _, _)) = monitors.first() {
                config.insert(connector.clone(), (x, y, scale, transform, is_primary));
            }
        }
        info!("SwapDisplays: recovered missing eDP-1 using transform-aware layout");
    } else {
        return Err("Both displays missing".to_string());
    }

    Ok(())
}

fn normalize_duo_layout(
    config: &mut HashMap<String, (i32, i32, f64, u32, bool)>,
    all_modes: &HashMap<String, (String, i32, i32)>,
    target_primary: &str,
    desired_transform: u32,
) -> bool {
    let secondary_connector = if target_primary == "eDP-1" { "eDP-2" } else { "eDP-1" };

    // Always source scale from eDP-1 (the stable reference display).
    let scale = config.get("eDP-1")
        .map(|(_, _, s, _, _)| *s)
        .or_else(|| config.get(target_primary).map(|(_, _, s, _, _)| *s));
    let Some(scale) = scale else {
        return false;
    };
    let Some(primary_mode) = all_modes.get(target_primary) else {
        return false;
    };
    let Some(secondary_mode) = all_modes.get(secondary_connector) else {
        return false;
    };

    let mut changed = false;
    for (x, y, scale, transform, is_primary, monitors) in
        build_duo_lms(
            target_primary,
            primary_mode,
            Some((secondary_connector, secondary_mode)),
            desired_transform,
            scale,
        )
    {
        if let Some((connector, _, _)) = monitors.first() {
            let new_entry = (x, y, scale, transform, is_primary);
            if config.get(connector) != Some(&new_entry) {
                config.insert(connector.clone(), new_entry);
                changed = true;
            }
        }
    }

    if changed {
        info!(
            "SwapDisplays: normalized transform-aware layout for primary={} transform={}",
            target_primary, desired_transform
        );
    }
    changed
}

fn matches_expected_duo_layout(
    config: &HashMap<String, (i32, i32, f64, u32, bool)>,
    all_modes: &HashMap<String, (String, i32, i32)>,
    target_primary: &str,
    desired_transform: u32,
) -> bool {
    if config.len() != 2 || !config.contains_key("eDP-1") || !config.contains_key("eDP-2") {
        return false;
    }

    let secondary_connector = if target_primary == "eDP-1" { "eDP-2" } else { "eDP-1" };
    // Always source scale from eDP-1 (the stable reference display).
    let scale = config.get("eDP-1")
        .map(|(_, _, s, _, _)| *s)
        .or_else(|| config.get(target_primary).map(|(_, _, s, _, _)| *s));
    let Some(scale) = scale else {
        return false;
    };
    let Some(primary_mode) = all_modes.get(target_primary) else {
        return false;
    };
    let Some(secondary_mode) = all_modes.get(secondary_connector) else {
        return false;
    };

    let mut expected = HashMap::new();
    for (x, y, scale, transform, is_primary, monitors) in build_duo_lms(
        target_primary,
        primary_mode,
        Some((secondary_connector, secondary_mode)),
        desired_transform,
        scale,
    ) {
        if let Some((connector, _, _)) = monitors.first() {
            expected.insert(connector.clone(), (x, y, scale, transform, is_primary));
        }
    }

    expected.len() == 2
        && expected
            .iter()
            .all(|(connector, expected_entry)| config.get(connector) == Some(expected_entry))
}

fn matches_expected_single_layout(
    config: &HashMap<String, (i32, i32, f64, u32, bool)>,
    target_primary: &str,
    desired_transform: u32,
) -> bool {
    if config.len() != 1 {
        return false;
    }

    matches!(
        config.get(target_primary),
        Some((0, 0, _, transform, true)) if *transform == desired_transform
    )
}

async fn repair_layout_for_primary(
    display: &DisplayConfigProxy<'_>,
    target_primary: &str,
    desired_transform: u32,
    require_secondary: bool,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let (serial, physical, logical, _): CurrentState = display.get_current_state().await?;
    let all_modes = extract_all_modes(&physical);
    let (mut config, was_corrupted) = read_current_config(&logical);

    let new_monitors = if require_secondary {
        ensure_both_displays(display, serial, &physical, &all_modes, &mut config)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

        // Only skip the apply if the GNOME state was already clean AND matches the expected
        // layout. When was_corrupted is true, the fix is only in-memory; we must push it to GNOME.
        if !was_corrupted && matches_expected_duo_layout(&config, &all_modes, target_primary, desired_transform) {
            return Ok(true);
        }

        normalize_duo_layout(&mut config, &all_modes, target_primary, desired_transform);
        let mut new_monitors = Vec::new();
        for connector in &["eDP-1", "eDP-2"] {
            if let Some(&(x, y, scale, transform, is_primary)) = config.get(*connector) {
                if let Some((mode_id, _, _)) = all_modes.get(*connector) {
                    new_monitors.push((
                        x,
                        y,
                        scale,
                        transform,
                        is_primary,
                        vec![(connector.to_string(), mode_id.clone(), HashMap::new())],
                    ));
                }
            }
        }
        new_monitors
    } else {
        if matches_expected_single_layout(&config, target_primary, desired_transform) {
            return Ok(true);
        }

        let scale = read_edp1_scale(&logical);
        let primary_mode = all_modes
            .get(target_primary)
            .ok_or_else(|| format!("No mode for {target_primary}"))?;
        build_duo_lms(target_primary, primary_mode, None, desired_transform, scale)
    };

    info!(
        "DesiredPrimary: repairing layout for primary={} with {} logical monitors (require_secondary={})",
        target_primary,
        new_monitors.len(),
        require_secondary
    );
    display.apply_monitors_config(serial, 1, new_monitors, HashMap::new()).await?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let (_, physical, logical, _): CurrentState = display.get_current_state().await?;
    let all_modes = extract_all_modes(&physical);
    let (verify_config, verify_corrupted) = read_current_config(&logical);
    Ok(!verify_corrupted
        && if require_secondary {
            matches_expected_duo_layout(
                &verify_config,
                &all_modes,
                target_primary,
                desired_transform,
            )
        } else {
            matches_expected_single_layout(&verify_config, target_primary, desired_transform)
        })
}

/// Unified display state application - replaces scattered apply calls
/// Reads current → calculates desired → applies once → verifies
/// Special case: disable secondary while eDP-2 primary requires two sequential applies
async fn apply_desired_display_state(
    display: &DisplayConfigProxy<'_>,
    keyboard_attached: bool,
    desired_secondary_enabled: bool,
    desired_primary: &str,
    desired_transform: u32,
    cached_secondary: &mut Option<(String, String, i32, i32, f64)>,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    // Read current state
    let (serial, physical, logical, _props) = display.get_current_state().await?;
    let all_modes = extract_all_modes(&physical);
    let (current_config, current_corrupted) = read_current_config(&logical);
    
    // Determine physical availability
    let edp2_physically_available = all_modes.contains_key("eDP-2");
    
    // Calculate desired: eDP-2 enabled if physically available AND desired AND keyboard not attached
    let edp2_should_be_enabled = edp2_physically_available && desired_secondary_enabled && !keyboard_attached;
    
    // Get current state
    let edp2_current = current_config.get("eDP-2");
    let edp1_scale = read_edp1_scale(&logical);
    
    // Determine current/desired primary
    let current_primary = current_config
        .iter()
        .find(|(_, (_, _, _, _, is_primary))| *is_primary)
        .map(|(conn, _)| conn.clone())
        .unwrap_or_else(|| "eDP-1".to_string());
    
    let desired_primary_effective = if keyboard_attached && desired_primary == "eDP-2" {
        "eDP-1".to_string()
    } else {
        desired_primary.to_string()
    };
    
    // Check if state already matches (including transform).
    let edp2_currently_enabled = edp2_current.is_some();
    let current_transform = current_config
        .iter()
        .find(|(_, (_, _, _, _, is_primary))| *is_primary)
        .map(|(_, (_, _, _, transform, _))| *transform)
        .unwrap_or(0);
    if current_primary == desired_primary_effective 
        && edp2_currently_enabled == edp2_should_be_enabled 
        && current_transform == desired_transform
        && !current_corrupted {
        info!("Display state already matches desired (no change needed)");
        return Ok(false);
    }
    
    // Special case: disabling eDP-2 while it's primary - must swap first
    if !edp2_should_be_enabled && edp2_currently_enabled && current_primary == "eDP-2" {
        info!("Disabling secondary: eDP-2 is primary, swapping to eDP-1 first");
        
        match apply_display_swap(display, desired_transform).await {
            Ok(_) => {
                info!("Swapped to eDP-1 successfully");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(e) => {
                warn!("Failed to swap to eDP-1: {}", e);
                return Err(format!("Cannot disable secondary - swap failed: {}", e).into());
            }
        }
        
        // Now disable eDP-2
        let desired_primary_arc = Arc::new(tokio::sync::RwLock::new("eDP-1".to_string()));
        return apply_toggle_secondary_display(
            display,
            false,
            cached_secondary,
            &desired_primary_arc,
            desired_transform,
        )
        .await
        .map(|_| true);
    }
    
    // Normal case: single atomic apply
    // Build desired config
    let mut new_monitors = Vec::new();
    
    // Add eDP-1
    if let Some((mode_id, _w, _h)) = all_modes.get("eDP-1") {
        let is_primary = desired_primary_effective == "eDP-1";
        let apply_mon = ("eDP-1".to_string(), mode_id.clone(), HashMap::new());
        let apply_lm = (0, 0, edp1_scale, desired_transform, is_primary, vec![apply_mon]);
        new_monitors.push(apply_lm);
    }
    
    // Add eDP-2 if enabled
    if edp2_should_be_enabled {
        if let Some((mode_id, w, h)) = all_modes.get("eDP-2") {
            let (_, lh) = logical_size(*w, *h, edp1_scale, desired_transform);
            let is_primary = desired_primary_effective == "eDP-2";
            let apply_mon = ("eDP-2".to_string(), mode_id.clone(), HashMap::new());
            let apply_lm = (0, lh, edp1_scale, desired_transform, is_primary, vec![apply_mon]);
            new_monitors.push(apply_lm);
        }
    }
    
    // Apply once
    display.apply_monitors_config(serial, 1, new_monitors, HashMap::new()).await?;
    
    // Verify
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (_, _, logical_after, _) = display.get_current_state().await?;
    let (after_config, _) = read_current_config(&logical_after);
    
    let after_primary = after_config
        .iter()
        .find(|(_, (_, _, _, _, is_primary))| *is_primary)
        .map(|(c, _)| c.clone());
    
    if after_primary.as_ref() != Some(&desired_primary_effective) {
        return Err(format!("Primary mismatch after apply").into());
    }
    
    if after_config.contains_key("eDP-2") != edp2_should_be_enabled {
        return Err(format!("Secondary mismatch after apply").into());
    }
    
    info!("Display state applied successfully");
    Ok(true)
}

/// Swap which monitor is primary. Keeps physical positions unchanged.
/// Implements display_swap_working_v3.py logic:
/// - Read current state
/// - Recover if displays incomplete
/// - Validate Y positions
/// - Determine current and new primary
/// - Build config with swapped primary (NOT toggled)
/// - Apply → wait → verify
/// - Retry up to 3 times on failure

/// Handle keyboard attach/detach safely:
/// When attached: If eDP-2 is primary, swap to eDP-1 first, then disable eDP-2
/// When detached: Re-enable eDP-2, then restore desired_primary if needed
pub async fn apply_display_swap(
    display: &DisplayConfigProxy<'_>,
    desired_transform: u32,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    const MAX_ATTEMPTS: usize = 3;
    let mut desired_primary: Option<String> = None;  // Track what the user WANTS, survives retries
    let mut last_requested_monitors: Option<Vec<ApplyLm>> = None;
    let mut last_requested_primary: Option<String> = None;

    for attempt in 1..=MAX_ATTEMPTS {
        info!("SwapDisplays: attempt {}/{}", attempt, MAX_ATTEMPTS);

        // Step 1: Read current state
        let (serial, physical, logical, _): CurrentState = match display.get_current_state().await {
            Ok(state) => state,
            Err(e) => {
                warn!("SwapDisplays: failed to get current state: {e}");
                if attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
                return Err(e.into());
            }
        };

        if logical.is_empty() {
            warn!("SwapDisplays: no logical monitors found");
            if attempt < MAX_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
            return Err("SwapDisplays: no displays available".into());
        }

        let all_modes = extract_all_modes(&physical);
        let (mut config, config_corrupted) = read_current_config(&logical);
        
        // Log the raw D-Bus state
        info!("SwapDisplays: raw logical monitors from D-Bus:");
        for (i, lm) in logical.iter().enumerate() {
            if let Some(connector_ref) = lm.5.first() {
                info!("  [{}] connector={}, pos=({},{}), primary={}, transform={}", 
                    i, connector_ref.0, lm.0, lm.1, lm.4, lm.3);
            }
        }
        
        // Log the read config state
        info!("SwapDisplays: read_current_config: eDP-1={:?}, eDP-2={:?}", 
            config.get("eDP-1"), config.get("eDP-2"));

        let current_primary_hint = config
            .iter()
            .find(|(_, (_, _, _, _, is_primary))| *is_primary)
            .map(|(name, _)| name.clone())
            .or_else(|| {
                logical
                    .iter()
                    .find(|lm| lm.4)
                    .and_then(|lm| lm.5.first().map(|r| r.0.clone()))
            })
            .unwrap_or_else(|| "eDP-1".to_string());
        let transform_hint = desired_transform;
        let scale_hint = read_edp1_scale(&logical);

        // On first attempt, determine what we want to swap TO. On retries, use the same goal.
        if desired_primary.is_none() {
            desired_primary = Some(if current_primary_hint == "eDP-1" {
                "eDP-2".to_string()
            } else {
                "eDP-1".to_string()
            });
        }
        
        // Step 2a: if the previous apply left Mutter in a broken transitional state,
        // retry by sending the exact same payload again before rebuilding anything.
        if config_corrupted || config.len() < 2 {
            let reapply_primary = last_requested_primary
                .as_deref()
                .or(desired_primary.as_deref())
                .unwrap_or(current_primary_hint.as_str());
            let reapply_config = if let Some(previous_payload) = last_requested_monitors.clone() {
                warn!(
                    "SwapDisplays: state is incomplete/corrupted, reapplying exact previous configuration"
                );
                previous_payload
            } else {
                warn!("SwapDisplays: corruption detected in logical monitors, reapplying desired configuration");
                let reapply_secondary = if reapply_primary == "eDP-1" { "eDP-2" } else { "eDP-1" };
                let primary_mode = all_modes
                    .get(reapply_primary)
                    .ok_or_else(|| format!("No mode found for connector {}", reapply_primary))?;
                let secondary_mode = all_modes
                    .get(reapply_secondary)
                    .ok_or_else(|| format!("No mode found for connector {}", reapply_secondary))?;
                build_duo_lms(
                    reapply_primary,
                    primary_mode,
                    Some((reapply_secondary, secondary_mode)),
                    transform_hint,
                    scale_hint,
                )
            };

            if let Err(e) = display
                .apply_monitors_config(serial, 1, reapply_config, HashMap::new())
                .await
            {
                warn!("SwapDisplays: desired config reapply failed: {e}");
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                info!("SwapDisplays: desired config reapplied to D-Bus after corruption");
            }

            if let Ok((_, _, verify_logical, _)) = display.get_current_state().await {
                let (reapply_verify_config, reapply_verify_corrupted) = read_current_config(&verify_logical);
                info!(
                    "SwapDisplays: state after desired reapply: eDP-1={:?}, eDP-2={:?}",
                    reapply_verify_config.get("eDP-1"),
                    reapply_verify_config.get("eDP-2")
                );

                let reapply_succeeded =
                    !reapply_verify_corrupted
                        && reapply_verify_config
                            .get(reapply_primary)
                            .map_or(false, |(_, _, _, _, is_primary)| *is_primary)
                        && matches_expected_duo_layout(
                            &reapply_verify_config,
                            &all_modes,
                            reapply_primary,
                            desired_transform,
                        );

                if reapply_succeeded {
                    info!("SwapDisplays: desired config reapply restored the expected state");
                    return Ok(reapply_primary.to_string());
                }

                config = reapply_verify_config;
            }

            if attempt < MAX_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
        }
        
        // Step 2b: Recover incomplete state (mirrors Python)
        if let Err(e) = ensure_both_displays(display, serial, &physical, &all_modes, &mut config).await {
            warn!("SwapDisplays: recovery failed: {}", e);
            if attempt < MAX_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
            return Err(e.into());
        }

        // Step 3: Determine current and new primary (mirrors Python Step 3)
        let current_primary = config
            .iter()
            .find(|(_, (_, _, _, _, is_primary))| *is_primary)
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| "eDP-1".to_string());

        let new_primary = desired_primary.as_ref().unwrap().as_str();

        // Normalize layout for the target primary before applying so swap+disable
        // does not visibly bounce through an intermediate wrong arrangement.
        normalize_duo_layout(&mut config, &all_modes, new_primary, desired_transform);

        info!(
            "SwapDisplays: attempt {} - current primary={}, desired swap to={} (user goal: stay consistent across retries)",
            attempt, current_primary, new_primary
        );

        // Step 5: Build new config with ONLY new_primary set to true (mirrors Python)
        let mut new_monitors = Vec::new();
        for connector in &["eDP-2", "eDP-1"] {
            if let Some(&(x, y, scale, transform, _)) = config.get(*connector) {
                let is_primary = *connector == new_primary;
                if let Some((mode_id, _, _)) = all_modes.get(*connector) {
                    let apply_mon: ApplyMonSpec = (connector.to_string(), mode_id.clone(), HashMap::new());
                    let apply_lm: ApplyLm = (x, y, scale, transform, is_primary, vec![apply_mon]);
                    new_monitors.push(apply_lm);

                    info!(
                        "SwapDisplays: building {}: pos=({},{}), scale={}, transform={}, primary={}, mode={}",
                        connector, x, y, scale, transform, is_primary, mode_id
                    );
                }
            }
        }

        // Step 6: Apply configuration
        info!(
            "SwapDisplays: applying new configuration (attempt {}/{}) with {} monitors",
            attempt, MAX_ATTEMPTS, new_monitors.len()
        );
        last_requested_primary = Some(new_primary.to_string());
        last_requested_monitors = Some(new_monitors.clone());
        match display
            .apply_monitors_config(serial, 1, new_monitors, HashMap::new())
            .await
        {
            Ok(()) => {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                // Step 7: Verify result
                match display.get_current_state().await {
                    Ok((_, _, verify_logical, _)) => {
                        if verify_logical.len() < 2 {
                            warn!(
                                "SwapDisplays: only {} monitor(s) after apply (attempt {}/{})",
                                verify_logical.len(),
                                attempt,
                                MAX_ATTEMPTS
                            );
                            if attempt < MAX_ATTEMPTS {
                                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                continue;
                            } else {
                                return Err("SwapDisplays: missing display after apply".into());
                            }
                        }

                        let (verify_config, verify_corrupted) = read_current_config(&verify_logical);
                        
                        info!("SwapDisplays: post-apply verification state: eDP-1={:?}, eDP-2={:?}", 
                            verify_config.get("eDP-1"), verify_config.get("eDP-2"));
                        
                        if verify_corrupted {
                            warn!("SwapDisplays: corruption detected in verification state (attempt {}/{}), retrying", attempt, MAX_ATTEMPTS);
                            if attempt < MAX_ATTEMPTS {
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                continue;
                            } else {
                                return Err("SwapDisplays: persistent corruption in verification state".into());
                            }
                        }
                        
                        let checks_passed =
                            verify_config.get(new_primary).map_or(false, |(_, _, _, _, is_primary)| *is_primary)
                                && matches_expected_duo_layout(
                                    &verify_config,
                                    &all_modes,
                                    new_primary,
                                    desired_transform,
                                );
                        
                        if checks_passed {
                            info!("SwapDisplays: all verification checks passed (attempt {}/{})", attempt, MAX_ATTEMPTS);
                            info!(
                                "SwapDisplays: swap successful on attempt {}/{}, new_primary={} at {:?}",
                                attempt, MAX_ATTEMPTS, new_primary,
                                verify_config.get(new_primary)
                            );
                            return Ok(new_primary.to_string());
                        }

                        warn!(
                            "SwapDisplays: verification checks failed (attempt {}/{}), retrying",
                            attempt, MAX_ATTEMPTS
                        );
                        if attempt < MAX_ATTEMPTS {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            continue;
                        } else {
                            return Err("SwapDisplays: swap verification failed".into());
                        }
                    }
                    Err(e) => {
                        warn!("SwapDisplays: failed to verify state: {e}");
                        if attempt < MAX_ATTEMPTS {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            continue;
                        } else {
                            return Err(e.into());
                        }
                    }
                }
            }
            Err(e) => {
                warn!("SwapDisplays: apply failed: {e}");
                if attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                } else {
                    return Err(e.into());
                }
            }
        }
    }

    Err("SwapDisplays: exhausted all retry attempts".into())
}




async fn apply_toggle_secondary_display(
    display: &DisplayConfigProxy<'_>,
    enable: bool,
    cached_secondary: &mut Option<(String, String, i32, i32, f64)>,
    desired_primary: &Arc<tokio::sync::RwLock<String>>,
    desired_transform: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // If disabling and eDP-2 is primary, swap to eDP-1 first to prevent crash
    if !enable {
        let (_serial, _physical, logical, _props) = display.get_current_state().await?;
        let (logical_map, disable_state_corrupted) = read_current_config(&logical);

        if disable_state_corrupted {
            // Corrupted state: eDP-2 may appear absent even though it's physically active.
            // Skip ALL pre-checks and proceed directly to single-monitor apply below.
            warn!("ToggleSecondaryDisplay: corrupted state detected on disable — skipping pre-checks, forcing single-monitor apply");
        } else {
            // Early exit: if secondary is already absent from logical monitors, nothing to do.
            if !logical_map.contains_key("eDP-2") {
                info!("ToggleSecondaryDisplay: eDP-2 already absent from logical monitors — skipping apply");
                return Ok(());
            }

            if let Some((_, _, _, _, is_primary)) = logical_map.get("eDP-2") {
                if *is_primary {
                    info!("eDP-2 is primary, must swap to eDP-1 before disabling");
                    match apply_display_swap(display, desired_transform).await {
                        Ok(new_primary) => {
                            info!("Swapped to: {}", new_primary);
                        }
                        Err(e) => {
                            warn!("Swap failed before toggle disable: {}", e);
                        }
                    }
                    // Sleep to let state settle
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }

        // Note: sysfs control (on/off) is handled by root daemon's secondary_display task,
        // not by session daemon (which runs as user and lacks sysfs write permissions).
        // Root daemon monitors desired_secondary state and manages sysfs accordingly.
    }
    
    // Now proceed with actual toggle
    let mut state: CurrentState = display.get_current_state().await?;
    let current_primary_hint = state.2
        .iter()
        .find(|lm| lm.4)
        .and_then(|lm| lm.5.first())
        .map(|r| r.0.clone())
        .unwrap_or_else(|| "eDP-1".to_string());
    let expected_secondary_hint = if current_primary_hint == "eDP-1" {
        "eDP-2"
    } else {
        "eDP-1"
    };
    
    // When enabling, wait for Mutter to recognize the connector
    // (root daemon's sysfs control should enable it via secondary_display task)
    if enable && !extract_all_modes(&state.1).contains_key(expected_secondary_hint) {
        info!("Enabling secondary: waiting for root daemon sysfs control to make connector available");
        if let Some(waited_state) = wait_for_connector_mode(display, expected_secondary_hint).await? {
            state = waited_state;
        }
    }


    let (serial, physical, logical, _): CurrentState = state;
    let all_modes = extract_all_modes(&physical);
    // Scale always sourced from eDP-1 (stable reference display).
    let edp1_scale = read_edp1_scale(&logical);
    let current_primary_connector = logical
        .iter()
        .find(|lm| lm.4)
        .and_then(|lm| lm.5.first())
        .map(|r| r.0.clone())
        .unwrap_or_else(|| "eDP-1".to_string());
    let expected_secondary_connector = if current_primary_connector == "eDP-1" {
        "eDP-2".to_string()
    } else {
        "eDP-1".to_string()
    };

    let mut new_monitors = Vec::new();
    let mut seen_connectors = HashSet::new();
    for lm in logical {
        let connector = lm.5.first().map(|r| r.0.as_str()).ok_or("monitor has no connector")?;
        if !seen_connectors.insert(connector.to_string()) {
            warn!("ToggleSecondaryDisplay: ignoring duplicate logical monitor for {}", connector);
            continue;
        }
        let is_secondary = connector == expected_secondary_connector;

        if is_secondary && !enable {
            // Disable secondary: skip it, but cache its config first
            info!("Disabling secondary display at ({},{})", lm.0, lm.1);
            let mode_id = all_modes.get(connector)
                .map(|(id, _, _)| id.clone())
                .ok_or_else(|| format!("No mode found for connector {}", connector))?;
            *cached_secondary = Some((connector.to_string(), mode_id, lm.0, lm.1, lm.2));
            continue;
        }
        if is_secondary && enable {
            // Enable secondary: include it with current settings
            info!("Enabling secondary display at ({},{})", lm.0, lm.1);
        }

        // Get the actual current mode ID for this connector
        let mode_id = all_modes.get(connector)
            .map(|(id, _, _)| id.clone())
            .ok_or_else(|| format!("No mode found for connector {}", connector))?;

        let apply_mon: ApplyMonSpec = (connector.to_string(), mode_id, HashMap::new());
        let (x, y) = if !enable && connector == current_primary_connector {
            // When collapsing from duo -> single monitor, the remaining logical monitor
            // must be re-anchored at the origin or Mutter rejects the config as offset.
            (0, 0)
        } else {
            (lm.0, lm.1)
        };
        let apply_lm: ApplyLm = (x, y, lm.2, desired_transform, lm.4, vec![apply_mon]);
        new_monitors.push(apply_lm);
    }

    // If enabling but no secondary found in logical monitors, restore from cache or
    // rebuild from the physical monitor list when eDP-2 is present but currently inactive.
    if enable && new_monitors.len() == 1 {
        if let Some((_, _, _primary_scale, primary_transform, _, monitors)) = new_monitors.first().cloned() {
            if let Some((primary_connector, _, _)) = monitors.first().cloned() {
                if primary_connector != "eDP-1" {
                    warn!(
                        "Enable requested from single-monitor state with primary={}, expected eDP-1 as source",
                        primary_connector
                    );
                }
                let primary_mode = all_modes
                    .get(primary_connector.as_str())
                    .ok_or_else(|| format!("No mode found for connector {}", primary_connector))?;

                let restore: Option<(String, String, f64, &'static str, (String, i32, i32))> =
                if let Some((connector, _mode_id, _x, _y, _scale)) = cached_secondary.clone() {
                    if connector == primary_connector {
                        None
                    } else {
                    let secondary_mode = find_mode_matching_size(
                        &physical,
                        connector.as_str(),
                        primary_mode.1,
                        primary_mode.2,
                    )
                    .or_else(|| all_modes.get(connector.as_str()).cloned())
                    .ok_or_else(|| format!("No mode found for connector {}", connector))?;
                    Some((
                        connector,
                        secondary_mode.0.clone(),
                        edp1_scale, // always use eDP-1 scale
                        "cache",
                        secondary_mode,
                    ))
                    }
                } else if let Some((connector, secondary_mode)) = all_modes
                    .iter()
                    .find(|(connector, _)| connector.as_str() == expected_secondary_connector)
                {
                    info!(
                        "Enable requested without cache; rebuilding secondary from physical monitor list connector={}",
                        connector
                    );
                    let matched_mode = find_mode_matching_size(
                        &physical,
                        connector,
                        primary_mode.1,
                        primary_mode.2,
                    )
                    .unwrap_or_else(|| secondary_mode.clone());
                    Some((
                        connector.clone(),
                        matched_mode.0.clone(),
                        edp1_scale, // always use eDP-1 scale
                        "physical",
                        matched_mode,
                    ))
                } else {
                    None
                };

                if let Some((connector, mode_id, scale, source, secondary_mode)) = restore {
                    let desired_primary_value = desired_primary.read().await.clone();
                    // Always restore secondary with the current single-monitor primary (eDP-1).
                    // Mutter silently ignores the primary flag when enabling a display for the
                    // first time in a transition, so trying to set eDP-2 as primary in the same
                    // call that enables it produces a verification mismatch and triggers the
                    // recovery loop. The caller (run loop) will swap primary to eDP-2 afterwards
                    // via apply_desired_primary_if_possible.
                    let restore_primary_connector = primary_connector.as_str();
                    let (restore_primary_mode, restore_secondary_connector, restore_secondary_mode) =
                        (primary_mode, connector.as_str(), &secondary_mode);

                    info!(
                        "Restoring secondary display from {} primary={} desired_primary={} transform={} (current single-monitor transform={})",
                        source,
                        restore_primary_connector,
                        desired_primary_value,
                        desired_transform,
                        primary_transform
                    );

                    new_monitors = build_duo_lms(
                        restore_primary_connector,
                        restore_primary_mode,
                        Some((restore_secondary_connector, restore_secondary_mode)),
                        desired_transform,
                        scale,
                    )
                    .into_iter()
                    .map(|(x, y, scale, transform, is_primary, monitors)| {
                        if let Some((lm_connector, _, _)) = monitors.first() {
                            let lm_mode_id = if lm_connector == &connector {
                                mode_id.clone()
                            } else {
                                all_modes
                                    .get(lm_connector)
                                    .map(|(id, _, _)| id.clone())
                                    .unwrap_or_default()
                            };
                            (
                                x,
                                y,
                                scale,
                                transform,
                                is_primary,
                                vec![(lm_connector.clone(), lm_mode_id, HashMap::new())],
                            )
                        } else {
                            (x, y, scale, transform, is_primary, monitors)
                        }
                    })
                    .collect();
                } else {
                    warn!("Enable requested but no secondary monitor found in cache or physical state");
                }
            } else {
                warn!("Enable requested but current primary logical monitor has no connector");
            }
        } else {
            warn!("Enable requested but could not infer current primary layout for restore");
        }
    }

    info!("Applying toggle secondary display (enable={}), monitors count={}", enable, new_monitors.len());

    // Apply with non-adjacent pixel adjustment when applying a dual-monitor config.
    // On unrecoverable non-adjacent (enable only): fall back to eDP-1-only + user notification.
    let non_adjacent_unrecoverable = if new_monitors.len() > 1 {
        match apply_duo_config_with_adjacency_fix(display, serial, new_monitors.clone(), desired_transform).await? {
            true  => false,
            false => {
                warn!("ToggleSecondaryDisplay: non-adjacent error unrecoverable after ±1px adjustment");
                true
            }
        }
    } else {
        display.apply_monitors_config(serial, 1, new_monitors.clone(), HashMap::new()).await?;
        false
    };

    if non_adjacent_unrecoverable && enable {
        warn!("ToggleSecondaryDisplay: falling back to eDP-1-only due to unrecoverable non-adjacent failure");
        if let Ok((fb_serial, fb_physical, fb_logical, _)) = display.get_current_state().await {
            let fb_scale = read_edp1_scale(&fb_logical);
            let fb_modes = extract_all_modes(&fb_physical);
            if let Some(fb_mode) = fb_modes.get("eDP-1") {
                let fb_lms = build_duo_lms("eDP-1", fb_mode, None, desired_transform, fb_scale);
                let _ = display.apply_monitors_config(fb_serial, 1, fb_lms, HashMap::new()).await;
            }
        }
        let _ = super::notifications::send_notification(
            "Display Configuration Error",
            "Could not arrange both displays adjacently.\u{2028}eDP-2 has been disabled. Use GNOME Display Settings to reconfigure.",
            "dialog-error",
            0,
            2,
        ).await;
        return Ok(()); // don't trigger recovery retry
    }

    info!("Secondary display toggle applied");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let (_, _, verify_logical, _): CurrentState = display.get_current_state().await?;
    let (verify_config, verify_corrupted) = read_current_config(&verify_logical);

    if verify_corrupted {
        return Err("ToggleSecondaryDisplay: corrupted logical monitor state after apply".into());
    }

    // Verify the full applied config matches what we requested (position, scale, transform, primary).
    if !requested_layout_matches_config(&new_monitors, &verify_config) {
        return Err(format!(
            "ToggleSecondaryDisplay: applied config doesn't match desired (enable={})",
            enable
        ).into());
    }

    // Secondary presence check (complements requested_layout_matches_config).
    if enable && !verify_config.contains_key(&expected_secondary_connector) {
        return Err(format!(
            "ToggleSecondaryDisplay: {} missing after enable (state={:?})",
            expected_secondary_connector, verify_config
        ).into());
    } else if !enable && verify_config.contains_key(&expected_secondary_connector) {
        return Err(format!(
            "ToggleSecondaryDisplay: {} still present after disable (state={:?})",
            expected_secondary_connector, verify_config
        ).into());
    }

    // Note: sysfs control (on/off) for eDP-2 is handled by root daemon's secondary_display task
    // based on desired_secondary_enabled state, not by this session daemon.

    Ok(())
}

/// Subscribe to D-Bus MonitorsChanged signal from Mutter
/// When signal fires, check if eDP-2 availability changed (needed to restore desired_primary)
/// Only acts when: desired_primary=eDP-2 AND eDP-2 is now available AND not primary
async fn subscribe_monitors_changed(
    display: &DisplayConfigProxy<'_>,
    availability_tx: broadcast::Sender<()>,
    desired_primary: Arc<tokio::sync::RwLock<String>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("DisplayConfig: subscribing to MonitorsChanged signal (event-driven, no polling)");
    
    let mut signal_rx = display.receive_monitors_changed().await?;
    
    while signal_rx.next().await.is_some() {
        // D-Bus signal fired: monitors changed
        // Check if this affects eDP-2 availability for desired_primary restoration
        let desired = desired_primary.read().await.clone();
        
        // Only care if we want eDP-2
        if desired != "eDP-2" {
            debug!("MonitorsChanged: desired_primary is {}, not eDP-2, skipping", desired);
            continue;
        }
        
        // Get current display state
        if let Ok((_serial, _physical, logical, _)) = display.get_current_state().await {
            let (logical_map, _) = read_current_config(&logical);
            
            // Check eDP-2: is it available AND not primary?
            match logical_map.get("eDP-2") {
                Some((_, _, _, _, is_primary)) if !*is_primary => {
                    // eDP-2 available but not primary - trigger restoration
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

async fn apply_desired_primary_if_possible(
    display: &DisplayConfigProxy<'_>,
    desired: &str,
    desired_transform: u32,
    require_secondary: bool,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let (_serial, _physical, logical, _) = display.get_current_state().await?;
    let (logical_map, _) = read_current_config(&logical);

    match logical_map.get(desired) {
        Some((_, _, _, _, is_primary)) if *is_primary => {
            let (_serial, physical, logical, _): CurrentState = display.get_current_state().await?;
            let all_modes = extract_all_modes(&physical);
            let config = read_current_config(&logical).0;
            let secondary_connector = if desired == "eDP-1" { "eDP-2" } else { "eDP-1" };
            if require_secondary && !all_modes.contains_key(secondary_connector) {
                info!(
                    "DesiredPrimary: {} is primary but {} has no available mode yet; accepting temporary single-display layout",
                    desired, secondary_connector
                );
                return if matches_expected_single_layout(&config, desired, desired_transform) {
                    Ok(true)
                } else {
                    repair_layout_for_primary(display, desired, desired_transform, false).await
                };
            }
            let layout_matches = if require_secondary {
                matches_expected_duo_layout(&config, &all_modes, desired, desired_transform)
            } else {
                matches_expected_single_layout(&config, desired, desired_transform)
            };
            if layout_matches {
                info!("DesiredPrimary: {} already primary", desired);
                Ok(true)
            } else {
                warn!("DesiredPrimary: {} is primary but layout is wrong, repairing", desired);
                repair_layout_for_primary(display, desired, desired_transform, require_secondary).await
            }
        }
        Some(_) => {
            let (_serial, physical, _logical, _): CurrentState = display.get_current_state().await?;
            let all_modes = extract_all_modes(&physical);
            let secondary_connector = if desired == "eDP-1" { "eDP-2" } else { "eDP-1" };
            if require_secondary {
                if !all_modes.contains_key(secondary_connector) {
                    info!(
                        "DesiredPrimary: {} available but {} has no available mode yet; applying temporary single-display layout",
                        desired, secondary_connector
                    );
                    return repair_layout_for_primary(display, desired, desired_transform, false).await;
                }
                info!("DesiredPrimary: {} available and not primary, applying now", desired);
                let new_primary = apply_display_swap(display, desired_transform).await?;
                if new_primary == desired {
                    info!("DesiredPrimary: switched primary to {}", desired);
                    Ok(true)
                } else {
                    warn!(
                        "DesiredPrimary: swap completed but primary is {} instead of {}",
                        new_primary, desired
                    );
                    Ok(false)
                }
            } else {
                info!(
                    "DesiredPrimary: {} available but secondary is inactive, repairing single-display layout",
                    desired
                );
                repair_layout_for_primary(display, desired, desired_transform, false).await
            }
        }
        None => {
            info!("DesiredPrimary: {} currently unavailable", desired);
            Ok(false)
        }
    }
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
    cached_secondary: &mut Option<(String, String, i32, i32, f64)>,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let desired_transform = current_desired_transform(display).await?;
    let desired_primary_value = desired_primary.read().await.clone();
    let desired_secondary_enabled = *desired_secondary.read().await;

    let changed = apply_desired_display_state(
        display,
        keyboard_attached,
        desired_secondary_enabled,
        &desired_primary_value,
        desired_transform,
        cached_secondary,
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let display = DisplayConfigProxy::new(conn).await?;
    let desired_transform = current_desired_transform(&display).await?;
    let desired_primary_value = desired_primary.read().await.clone();
    let desired_secondary_enabled = *desired_secondary.read().await;
    let keyboard_attached = *keyboard_attached.read().await;
    let effective_secondary = effective_secondary_enabled(desired_secondary_enabled, keyboard_attached);
    let effective_primary = if effective_secondary {
        desired_primary_value.as_str()
    } else {
        "eDP-1"
    };

    let mut cached_secondary = None;
    let (_, _, logical, _) = display.get_current_state().await?;
    let current_secondary_present = read_current_config(&logical).0.contains_key("eDP-2");

    if effective_secondary != current_secondary_present {
        apply_toggle_secondary_display(
            &display,
            effective_secondary,
            &mut cached_secondary,
            desired_primary,
            desired_transform,
        )
        .await?;
    }

    match apply_desired_primary_if_possible(
        &display,
        effective_primary,
        desired_transform,
        effective_secondary,
    )
    .await?
    {
        true => {}
        false => {
            return Err(format!(
                "Display recovery: effective primary {effective_primary} still unavailable after apply"
            )
            .into());
        }
    }

    if let Ok(Some(orientation)) = super::orientation::current_orientation().await {
        apply_rotation(&display, &orientation, keyboard_attached, effective_secondary).await?;
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

    // Cache for secondary monitor config so we can restore it after disabling.
    let mut cached_secondary: Option<(String, String, i32, i32, f64)> = None; // (connector, mode_id, x, y, scale)
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
                    &mut cached_secondary,
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
