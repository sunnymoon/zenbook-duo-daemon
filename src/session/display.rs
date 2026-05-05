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

const DEFAULT_DUO_SCALE: f64 = 5.0 / 3.0;

#[proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
pub trait DisplayConfig {
    fn get_current_state(&self) -> zbus::Result<CurrentState>;
    fn apply_monitors_config(
        &self,
        serial: u32,
        method: u32,
        logical_monitors: Vec<ApplyLm>,
        properties: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;
    
    #[zbus(signal)]
    fn monitors_changed(&self) -> zbus::Result<()>;
}

// ── Layout helpers ────────────────────────────────────────────────────────────

fn logical_size(pw: i32, ph: i32, scale: f64, transform: u32) -> (i32, i32) {
    let lw = (pw as f64 / scale).round() as i32;
    let lh = (ph as f64 / scale).round() as i32;
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    const MAX_ATTEMPTS: usize = 2;
    let transform = orientation_to_transform(orientation);

    for attempt in 1..=MAX_ATTEMPTS {
        info!("Rotation: attempt {}/{} for orientation={}", attempt, MAX_ATTEMPTS, orientation);

        // Step 1: Rebuild full config
        let (serial, physical, logical, _): CurrentState = display.get_current_state().await?;
        let all_modes = extract_all_modes(&physical);

        let primary_lm = logical.iter().find(|lm| lm.4);
        let scale = primary_lm.map(|lm| lm.2).unwrap_or(DEFAULT_DUO_SCALE);
        let primary_connector = primary_lm
            .and_then(|lm| lm.5.first())
            .map(|r| r.0.as_str())
            .unwrap_or("eDP-1");
        let primary_mode = all_modes.get(primary_connector).ok_or("no mode for primary")?;

        // Only include secondary if it's currently active in logical monitors (avoids disabled eDP-2).
        // Guard: secondary must differ from primary — Mutter can return the primary connector in the
        // non-primary slot during a mid-rebuild transitional state. Passing primary=X, secondary=X
        // causes the input mapper to crash (SIGSEGV in mapper_input_info_set_output).
        let secondary_connector: Option<String> = logical.iter()
            .find(|lm| !lm.4)
            .and_then(|lm| lm.5.first())
            .map(|r| r.0.clone())
            .filter(|c| c != primary_connector);
        let secondary = secondary_connector.as_deref()
            .and_then(|c| all_modes.get(c).map(|m| (c, m)));

        info!(
            "Rotation: building config with primary={} secondary={:?} transform={}",
            primary_connector, secondary_connector, transform
        );

        let lms = build_duo_lms(primary_connector, primary_mode, secondary, transform, scale);

        // Step 2: Apply config
        display.apply_monitors_config(serial, 1, lms.clone(), HashMap::new()).await?;

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

        // Verify all requested logical monitors are present with correct transforms
        let mut all_match = true;
        for (i, lm_request) in lms.iter().enumerate() {
            let (x, y, scale, transform, is_primary, connectors) = lm_request;
            if let Some(connector) = connectors.first().map(|c| c.0.as_str()) {
                if let Some((actual_x, actual_y, actual_scale, actual_transform, actual_is_primary)) = config_after.get(connector) {
                    let matches = x == actual_x && y == actual_y 
                        && (scale - actual_scale).abs() < 0.01
                        && transform == actual_transform
                        && is_primary == actual_is_primary;
                    if !matches {
                        warn!(
                            "Rotation: LM[{}] {} mismatch: requested ({},{},{},{},{}), got ({},{},{},{},{})",
                            i, connector, x, y, scale, transform, is_primary,
                            actual_x, actual_y, actual_scale, actual_transform, actual_is_primary
                        );
                        all_match = false;
                    }
                } else {
                    warn!("Rotation: LM[{}] connector {} not in applied config", i, connector);
                    all_match = false;
                }
            }
        }

        if all_match {
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

    let Some(&(_, _, scale, _, _)) = config.get(target_primary) else {
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
    let Some(&(_, _, scale, _, _)) = config.get(target_primary) else {
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

async fn repair_layout_for_primary(
    display: &DisplayConfigProxy<'_>,
    target_primary: &str,
    desired_transform: u32,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let (serial, physical, logical, _): CurrentState = display.get_current_state().await?;
    let all_modes = extract_all_modes(&physical);
    let (mut config, _) = read_current_config(&logical);

    ensure_both_displays(display, serial, &physical, &all_modes, &mut config)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    if matches_expected_duo_layout(&config, &all_modes, target_primary, desired_transform) {
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

    info!(
        "DesiredPrimary: repairing layout for primary={} with {} logical monitors",
        target_primary,
        new_monitors.len()
    );
    display.apply_monitors_config(serial, 1, new_monitors, HashMap::new()).await?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let (_, physical, logical, _): CurrentState = display.get_current_state().await?;
    let all_modes = extract_all_modes(&physical);
    let (verify_config, verify_corrupted) = read_current_config(&logical);
    Ok(!verify_corrupted && matches_expected_duo_layout(
        &verify_config,
        &all_modes,
        target_primary,
        desired_transform,
    ))
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
async fn handle_keyboard_attached(
    display: &DisplayConfigProxy<'_>,
    attached: bool,
    cached_secondary: &mut Option<(String, String, i32, i32, f64)>,
    desired_primary: &Arc<tokio::sync::RwLock<String>>,
    desired_secondary: &Arc<tokio::sync::RwLock<bool>>,
    desired_transform: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if attached {
        // When attached: force eDP-1 primary (overriding user choice temporarily)
        info!("Keyboard attached: checking if swap needed");
        
        let (_serial, _physical, logical, _props) = display.get_current_state().await?;
        let (logical_map, _) = read_current_config(&logical);
        
        // Check if eDP-2 is currently primary
        if let Some((_, _, _, _, is_primary)) = logical_map.get("eDP-2") {
            info!("eDP-2 status: is_primary={}", is_primary);
            if *is_primary {
                info!("eDP-2 is primary, swapping to eDP-1 before disabling");
                match apply_display_swap(display, desired_transform).await {
                    Ok(new_primary) => {
                        info!("Swapped to: {}", new_primary);
                    }
                    Err(e) => {
                        warn!("Swap failed during keyboard attach: {}", e);
                        return Err(e);
                    }
                }
                // Sleep to let state settle
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                
                // Verify eDP-1 is now primary before disabling eDP-2
                if let Ok((_, _, logical2, _)) = display.get_current_state().await {
                    let (logical_map2, _) = read_current_config(&logical2);
                    if let Some((_, _, _, _, is_primary_now)) = logical_map2.get("eDP-1") {
                        if !*is_primary_now {
                            warn!("After swap, eDP-1 is still not primary! Aborting disable");
                            return Err("eDP-1 not primary after swap".into());
                        }
                        info!("Verified eDP-1 is now primary, safe to disable eDP-2");
                    }
                }
            } else {
                info!("eDP-2 is not primary, safe to disable");
            }
        }
        
        // Now disable eDP-2 (should be safe since eDP-1 should be primary)
        info!("Disabling eDP-2");
        apply_toggle_secondary_display(
            display,
            false,
            cached_secondary,
            desired_primary,
            desired_transform,
        )
        .await
    } else {
        let should_enable_secondary = *desired_secondary.read().await;
        info!(
            "Keyboard detached: restoring desired secondary state enable={}",
            should_enable_secondary
        );
        let result = apply_toggle_secondary_display(
            display,
            should_enable_secondary,
            cached_secondary,
            desired_primary,
            desired_transform,
        )
        .await;
        
        // After restoring secondary display, sync orientation with sensor to prevent
        // applying stale transforms (can happen if device was rotated while secondary was disabled)
        if should_enable_secondary && result.is_ok() {
            if let Ok(Some(current_orientation)) = super::orientation::current_orientation().await {
                if current_orientation != "normal" {
                    info!("Keyboard detached: syncing rotation to sensor orientation after secondary restore");
                    let _ = apply_rotation(display, &current_orientation).await;
                }
            }
        }
        
        result
    }
}

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
        let scale_hint = logical.first().map(|lm| lm.2).unwrap_or(1.0);

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
    _desired_primary: &Arc<tokio::sync::RwLock<String>>,
    desired_transform: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // If disabling and eDP-2 is primary, swap to eDP-1 first to prevent crash
    if !enable {
        let (_serial, _physical, logical, _props) = display.get_current_state().await?;
        let (logical_map, _) = read_current_config(&logical);
        
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
    if enable && !extract_all_modes(&state.1).contains_key(expected_secondary_hint) {
        if let Some(waited_state) = wait_for_connector_mode(display, expected_secondary_hint).await? {
            state = waited_state;
        }
    }

    let (serial, physical, logical, _): CurrentState = state;
    let all_modes = extract_all_modes(&physical);
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
        if let Some((_, _, primary_scale, primary_transform, _, monitors)) = new_monitors.first().cloned() {
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
                        primary_scale,
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
                        primary_scale,
                        "physical",
                        matched_mode,
                    ))
                } else {
                    None
                };

                if let Some((connector, mode_id, scale, source, secondary_mode)) = restore {
                    info!(
                        "Restoring secondary display from {} primary={} transform={} (current single-monitor transform={})",
                        source, primary_connector, desired_transform, primary_transform
                    );

                    new_monitors = build_duo_lms(
                        primary_connector.as_str(),
                        primary_mode,
                        Some((connector.as_str(), &secondary_mode)),
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
    display.apply_monitors_config(serial, 1, new_monitors.clone(), HashMap::new()).await?;
    info!("Secondary display toggle completed");
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
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let (_serial, _physical, logical, _) = display.get_current_state().await?;
    let (logical_map, _) = read_current_config(&logical);

    match logical_map.get(desired) {
        Some((_, _, _, _, is_primary)) if *is_primary => {
            let (serial, physical, logical, _): CurrentState = display.get_current_state().await?;
            let all_modes = extract_all_modes(&physical);
            let mut config = read_current_config(&logical).0;
            ensure_both_displays(display, serial, &physical, &all_modes, &mut config)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            if matches_expected_duo_layout(&config, &all_modes, desired, desired_transform) {
                info!("DesiredPrimary: {} already primary", desired);
                Ok(true)
            } else {
                warn!("DesiredPrimary: {} is primary but layout is wrong, repairing", desired);
                repair_layout_for_primary(display, desired, desired_transform).await
            }
        }
        Some(_) => {
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
        }
        None => {
            info!("DesiredPrimary: {} currently unavailable", desired);
            Ok(false)
        }
    }
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

    // Cache for secondary monitor config so we can restore it after disabling
    let mut cached_secondary: Option<(String, String, i32, i32, f64)> = None;  // (connector, mode_id, x, y, scale)
    let mut desired_transform = match super::orientation::current_orientation().await {
        Ok(Some(orientation)) => {
            let transform = orientation_to_transform(&orientation);
            info!(
                "Display: startup desired transform from accelerometer orientation={} transform={}",
                orientation, transform
            );
            transform
        }
        Ok(None) => display
            .get_current_state()
            .await
            .ok()
            .and_then(|(_, _, logical, _)| logical.iter().find(|lm| lm.4).map(|lm| lm.3))
            .unwrap_or(0),
        Err(e) => {
            warn!("Display: failed to read startup accelerometer orientation: {e}");
            display
                .get_current_state()
                .await
                .ok()
                .and_then(|(_, _, logical, _)| logical.iter().find(|lm| lm.4).map(|lm| lm.3))
                .unwrap_or(0)
        }
    };
    let mut availability_rx = availability_tx.subscribe();
    let mut availability_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut keyboard_attached = false;
    let mut keyboard_state_initialized = false;

    // Do not apply any desired_primary at startup from the local default.
    // Root daemon is the authority and will immediately send the real desired_primary
    // over the daemon socket; acting before that causes visible primary-display flips.

    loop {
        tokio::select! {
            msg = orient_rx.recv() => match msg {
                Ok(orientation) => {
                    desired_transform = orientation_to_transform(&orientation);
                    if let Err(e) = apply_rotation(&display, &orientation).await {
                        warn!("Rotation failed for '{orientation}': {e}");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("Display handler lagged by {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = desired_secondary_rx.recv() => match msg {
                Ok(enable) => {
                    info!("Display: desired_secondary update received enable={enable}");
                    let start = std::time::Instant::now();
                    let current_state = display.get_current_state().await.ok();
                    let current_secondary_present = current_state
                        .as_ref()
                        .map(|(_, _, logical, _)| read_current_config(logical).0.contains_key("eDP-2"))
                        .unwrap_or(false);

                    let result = if enable && keyboard_attached {
                        info!("Desired secondary is on, but keyboard is attached; leaving secondary logically disabled for now");
                        Ok(())
                    } else if enable == current_secondary_present {
                        info!("Desired secondary already satisfied (enable={}, present={})", enable, current_secondary_present);
                        Ok(())
                    } else {
                        apply_toggle_secondary_display(
                            &display,
                            enable,
                            &mut cached_secondary,
                            &desired_primary,
                            desired_transform,
                        )
                        .await
                    };

                    if let Err(e) = result {
                        warn!("Desired secondary application failed: {e}");
                    } else {
                        let desired = desired_primary.read().await.clone();
                        let desired_secondary_enabled = *desired_secondary.read().await;
                        let should_wait_for_edp2 = desired == "eDP-2" && desired_secondary_enabled;
                        if enable && should_wait_for_edp2 {
                            match apply_desired_primary_if_possible(&display, &desired, desired_transform).await {
                                Ok(true) => stop_availability_monitor(&mut availability_task),
                                Ok(false) => update_availability_monitor_for_edp2(
                                    &mut availability_task,
                                    &conn,
                                    &availability_tx,
                                    &desired_primary,
                                    true,
                                ),
                                Err(e) => {
                                    warn!("Failed to restore desired_primary {} after toggle-on: {}", desired, e);
                                    update_availability_monitor_for_edp2(
                                        &mut availability_task,
                                        &conn,
                                        &availability_tx,
                                        &desired_primary,
                                        true,
                                    );
                                }
                            }
                        } else {
                            update_availability_monitor_for_edp2(
                                &mut availability_task,
                                &conn,
                                &availability_tx,
                                &desired_primary,
                                should_wait_for_edp2,
                            );
                        }
                        info!("Desired secondary application completed in {:.2}ms", start.elapsed().as_secs_f64() * 1000.0);
                    }
                    let _ = secondary_result_tx.send(()).await;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("Desired secondary handler lagged by {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = kb_rx.recv() => match msg {
                Ok(attached) => {
                    let previous_keyboard_attached = keyboard_attached;
                    keyboard_attached = attached;
                    info!("Display: keyboard_attached={attached}");
                    if !keyboard_state_initialized {
                        keyboard_state_initialized = true;
                        if !attached {
                            info!("Display: initial keyboard state sync is detached; skipping detach restore actions");
                            let _ = kb_result_tx.send(()).await;
                            continue;
                        }
                    } else if attached == previous_keyboard_attached {
                        info!("Display: keyboard_attached unchanged; skipping duplicate edge handling");
                        let _ = kb_result_tx.send(()).await;
                        continue;
                    }
                    let start = std::time::Instant::now();
                    if let Err(e) = handle_keyboard_attached(
                        &display,
                        attached,
                        &mut cached_secondary,
                        &desired_primary,
                        &desired_secondary,
                        desired_transform,
                    ).await {
                        warn!("Handle keyboard attached failed: {e}");
                    } else {
                        let desired = desired_primary.read().await.clone();
                        let desired_secondary_enabled = *desired_secondary.read().await;
                        let should_wait_for_edp2 = desired == "eDP-2" && desired_secondary_enabled;
                        update_availability_monitor_for_edp2(
                            &mut availability_task,
                            &conn,
                            &availability_tx,
                            &desired_primary,
                            should_wait_for_edp2,
                        );
                        info!("Handle keyboard attached completed in {:.2}ms", start.elapsed().as_secs_f64() * 1000.0);
                    }
                    // Send ACK to root daemon
                    let _ = kb_result_tx.send(()).await;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("Keyboard handler lagged by {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = desired_primary_rx.recv() => match msg {
                Ok(desired) => {
                    info!("Display: desired_primary update received: {}", desired);
                    let desired_secondary_enabled = *desired_secondary.read().await;
                    if !desired_secondary_enabled {
                        stop_availability_monitor(&mut availability_task);
                        debug!("DesiredPrimary update deferred because secondary display is disabled");
                        continue;
                    }
                    match apply_desired_primary_if_possible(&display, &desired, desired_transform).await {
                        Ok(true) => {
                            stop_availability_monitor(&mut availability_task);
                        }
                        Ok(false) if desired == "eDP-2" => {
                            ensure_availability_monitor(
                                &mut availability_task,
                                &conn,
                                &availability_tx,
                                &desired_primary,
                            );
                        }
                        Ok(false) => {
                            stop_availability_monitor(&mut availability_task);
                            warn!("DesiredPrimary: {} could not be applied immediately", desired);
                        }
                        Err(e) => {
                            warn!("DesiredPrimary update failed for {}: {e}", desired);
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("DesiredPrimary handler lagged by {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = availability_rx.recv() => match msg {
                Ok(_) => {
                    if !*desired_secondary.read().await {
                        stop_availability_monitor(&mut availability_task);
                        debug!("Availability check skipped because secondary display is disabled");
                        continue;
                    }
                    let desired = desired_primary.read().await.clone();
                    match apply_desired_primary_if_possible(&display, &desired, desired_transform).await {
                        Ok(true) => stop_availability_monitor(&mut availability_task),
                        Ok(false) => debug!("Availability check: {} still not ready", desired),
                        Err(e) => warn!("Failed to restore desired_primary {}: {}", desired, e),
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {} // Ignore lag on availability checks
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
}
