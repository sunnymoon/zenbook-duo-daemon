use std::collections::HashMap;
use std::sync::Arc;
use log::{debug, error, info, warn};
use tokio::sync::broadcast;
use zbus::{Connection, proxy, zvariant::OwnedValue};

// ── Mutter D-Bus type aliases ─────────────────────────────────────────────────

type ModeProps = HashMap<String, OwnedValue>;
type MonitorMode = (String, i32, i32, f64, f64, Vec<f64>, ModeProps);
type PhysicalMonitor = ((String, String, String, String), Vec<MonitorMode>, HashMap<String, OwnedValue>);
type LmRef = (String, String, String, String);
type LogicalMonitor = (i32, i32, f64, u32, bool, Vec<LmRef>, HashMap<String, OwnedValue>);
type CurrentState = (u32, Vec<PhysicalMonitor>, Vec<LogicalMonitor>, HashMap<String, OwnedValue>);
type ApplyMonSpec = (String, String, HashMap<String, OwnedValue>);
type ApplyLm = (i32, i32, f64, u32, bool, Vec<ApplyMonSpec>);

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
/// Verified layout:
///   t=0 normal:    eDP-1(0,0)   eDP-2(0,lh)   stacked, eDP-2 below
///   t=1 left-up:   eDP-1(lw,0)  eDP-2(0,0)    eDP-2 left, eDP-1 right
///   t=2 bottom-up: eDP-1(0,lh)  eDP-2(0,0)    eDP-2 above, eDP-1 below
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
        let (slw, slh) = logical_size(sm.1, sm.2, scale, transform);
        let (px, py, sx, sy) = match transform {
            0 => (0,   0,   0,   slh),
            1 => (slw, 0,   0,   0),
            2 => (0,   slh, 0,   0),
            3 => (0,   0,   slw, 0),
            _ => (0,   0,   0,   slh),
        };
        lms.push((
            px, py, scale, transform, true,
            vec![(primary_connector.to_string(), primary_mode.0.clone(), HashMap::new())],
        ));
        lms.push((
            sx, sy, scale, transform, false,
            vec![(sc.to_string(), sm.0.clone(), HashMap::new())],
        ));
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
    let transform = orientation_to_transform(orientation);
    let (serial, physical, logical, _): CurrentState = display.get_current_state().await?;

    let all_modes = extract_all_modes(&physical);

    let primary_lm = logical.iter().find(|lm| lm.4);
    let scale = primary_lm.map(|lm| lm.2).unwrap_or(5.0 / 3.0);
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
        "Rotation: orientation={orientation} transform={transform} primary={primary_connector} secondary={:?}",
        secondary_connector
    );

    let lms = build_duo_lms(primary_connector, primary_mode, secondary, transform, scale);
    display.apply_monitors_config(serial, 1, lms, HashMap::new()).await?;
    Ok(())
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
    _all_modes: &HashMap<String, (String, i32, i32)>,
    config: &mut HashMap<String, (i32, i32, f64, u32, bool)>,
) -> Result<(), String> {
    if config.contains_key("eDP-1") && config.contains_key("eDP-2") {
        return Ok(());
    }

    info!("SwapDisplays: incomplete state, recovering...");

    if config.contains_key("eDP-1") && !config.contains_key("eDP-2") {
        // Only eDP-1, mirror to eDP-2 at Y=1200
        let (x, y, scale, transform, _) = config["eDP-1"];
        let edp2_y = y + 1200;
        config.insert("eDP-2".to_string(), (x, edp2_y, scale, transform, false));
        info!("SwapDisplays: recovered - mirrored eDP-1 to eDP-2 at Y={}", edp2_y);
    } else if config.contains_key("eDP-2") && !config.contains_key("eDP-1") {
        // Only eDP-2, mirror to eDP-1 at Y=0
        let (x, _y, scale, transform, _) = config["eDP-2"];
        config.insert("eDP-1".to_string(), (x, 0, scale, transform, true));
        info!("SwapDisplays: recovered - mirrored eDP-2 to eDP-1 at Y=0");
    } else {
        return Err("Both displays missing".to_string());
    }

    Ok(())
}

/// Validate and fix Y positions (eDP-1 at Y=0, eDP-2 at Y=1200).
fn validate_y_positions(config: &mut HashMap<String, (i32, i32, f64, u32, bool)>) -> bool {
    let edp1_y = config.get("eDP-1").map(|c| c.1);
    let edp2_y = config.get("eDP-2").map(|c| c.1);

    if let (Some(y1), Some(y2)) = (edp1_y, edp2_y) {
        if y1 >= y2 {
            warn!(
                "SwapDisplays: incorrect Y positions (eDP-1={}, eDP-2={}), correcting",
                y1, y2
            );
            if let Some((x, _, scale, transform, is_primary)) = config.get("eDP-1").copied() {
                config.insert("eDP-1".to_string(), (x, 0, scale, transform, is_primary));
            }
            if let Some((x, _, scale, transform, is_primary)) = config.get("eDP-2").copied() {
                config.insert("eDP-2".to_string(), (x, 1200, scale, transform, is_primary));
            }
            return true;
        }
    }

    info!(
        "SwapDisplays: Y positions correct (eDP-1={:?}, eDP-2={:?})",
        edp1_y, edp2_y
    );
    false
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
                match apply_display_swap(display).await {
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
        apply_toggle_secondary_display(display, false, cached_secondary, desired_primary).await
    } else {
        // When detached: re-enable eDP-2 (apply_toggle_secondary_display handles restoration)
        info!("Keyboard detached: re-enabling eDP-2");
        apply_toggle_secondary_display(display, true, cached_secondary, desired_primary).await
    }
}

pub async fn apply_display_swap(
    display: &DisplayConfigProxy<'_>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    const MAX_ATTEMPTS: usize = 3;
    let mut desired_primary: Option<String> = None;  // Track what the user WANTS, survives retries

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
        
        // Step 2a: Check for corruption and apply reset if needed (mirrors Python)
        if config_corrupted {
            warn!("SwapDisplays: corruption detected in logical monitors, resetting to known good state");
            
            // Apply reset config to D-Bus (mirrors Python line 227)
            let mut reset_config = Vec::new();
            let reset_edp1 = (0i32, 0i32, 1.0f64, 0u32, true, vec![(
                "eDP-1".to_string(),
                all_modes.get("eDP-1").map(|(m, _, _)| m.clone()).unwrap_or_else(|| "1920x1200@119.909".to_string()),
                HashMap::new()
            )]);
            let reset_edp2 = (0i32, 1200i32, 1.0f64, 0u32, false, vec![(
                "eDP-2".to_string(),
                all_modes.get("eDP-2").map(|(m, _, _)| m.clone()).unwrap_or_else(|| "1920x1200@119.909".to_string()),
                HashMap::new()
            )]);
            reset_config.push(reset_edp1);
            reset_config.push(reset_edp2);
            
            if let Err(e) = display.apply_monitors_config(serial, 1, reset_config, HashMap::new()).await {
                warn!("SwapDisplays: reset config apply failed: {e}");
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                info!("SwapDisplays: reset config applied to D-Bus");
            }
            
            // Now re-read state from D-Bus after reset (mirrors Python line 197 after continue)
            if let Ok((_, _, verify_logical, _)) = display.get_current_state().await {
                let (reset_verify_config, _) = read_current_config(&verify_logical);
                info!("SwapDisplays: state after reset: eDP-1={:?}, eDP-2={:?}", 
                    reset_verify_config.get("eDP-1"), reset_verify_config.get("eDP-2"));
                config = reset_verify_config;
            }
        }
        
        // Step 2b: Recover incomplete state (mirrors Python)
        if let Err(e) = ensure_both_displays(display, serial, &all_modes, &mut config).await {
            warn!("SwapDisplays: recovery failed: {}", e);
            if attempt < MAX_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
            return Err(e.into());
        }

        // Step 3: Validate and fix Y positions
        validate_y_positions(&mut config);

        // Step 4: Determine current and new primary (mirrors Python Step 3)
        let current_primary = config
            .iter()
            .find(|(_, (_, _, _, _, is_primary))| *is_primary)
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| "eDP-1".to_string());

        // On first attempt, determine what we want to swap TO. On retries, use the same goal
        if desired_primary.is_none() {
            desired_primary = Some(if current_primary == "eDP-1" {
                "eDP-2".to_string()
            } else {
                "eDP-1".to_string()
            });
        }

        let new_primary = desired_primary.as_ref().unwrap().as_str();

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
                        
                        // Comprehensive verification like Python v3 (mirrors Python checks)
                        let checks_passed = verify_config.len() == 2 &&  // both displays present
                            verify_config.contains_key("eDP-1") &&
                            verify_config.contains_key("eDP-2") &&
                            verify_config.get(new_primary).map_or(false, |(_, _, _, _, is_primary)| *is_primary) &&  // new_primary is primary
                            verify_config.get("eDP-1").map_or(false, |(_, y1, scale1, _, _)| {
                                verify_config.get("eDP-2").map_or(false, |(_, y2, scale2, _, _)| {
                                    *y1 < *y2 &&  // Y positions correct
                                    (scale1 - 1.0).abs() < 0.01 &&  // scale preserved (should be 1.0)
                                    (scale2 - 1.0).abs() < 0.01
                                })
                            });
                        
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // If disabling and eDP-2 is primary, swap to eDP-1 first to prevent crash
    if !enable {
        let (_serial, _physical, logical, _props) = display.get_current_state().await?;
        let (logical_map, _) = read_current_config(&logical);
        
        if let Some((_, _, _, _, is_primary)) = logical_map.get("eDP-2") {
            if *is_primary {
                info!("eDP-2 is primary, must swap to eDP-1 before disabling");
                match apply_display_swap(display).await {
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
    let (serial, physical, logical, _): CurrentState = display.get_current_state().await?;
    let all_modes = extract_all_modes(&physical);

    let mut new_monitors = Vec::new();
    for lm in logical {
        let connector = lm.5.first().map(|r| r.0.as_str()).ok_or("monitor has no connector")?;
        let is_secondary = !lm.4;  // not primary = is secondary

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
        let apply_lm: ApplyLm = (lm.0, lm.1, lm.2, lm.3, lm.4, vec![apply_mon]);
        new_monitors.push(apply_lm);
    }

    // If enabling but no secondary found in logical monitors, try to restore from cache
    if enable && new_monitors.len() == 1 {
        if let Some((connector, mode_id, x, y, scale)) = cached_secondary.take() {
            info!("Restoring secondary display from cache at ({},{})", x, y);
            let apply_mon: ApplyMonSpec = (connector, mode_id, HashMap::new());
            let apply_lm: ApplyLm = (x, y, scale, 0, false, vec![apply_mon]);  // is_primary=false
            new_monitors.push(apply_lm);
        } else {
            warn!("Enable requested but no secondary monitor found and no cache available");
        }
    }

    info!("Applying toggle secondary display (enable={}), monitors count={}", enable, new_monitors.len());
    display.apply_monitors_config(serial, 1, new_monitors.clone(), HashMap::new()).await?;
    info!("Secondary display toggle completed");
    
    // When re-enabling eDP-2, D-Bus will emit PropertiesChanged signal
    // The availability signal listener will check and restore desired_primary if needed
    // (no polling here - purely event-driven)
    
    Ok(())
}

/// Monitor display configuration changes by subscribing to PropertiesChanged signals
/// When display state changes, trigger availability check
async fn monitor_display_availability(
    display: &DisplayConfigProxy<'_>,
    availability_tx: broadcast::Sender<()>,
    desired_primary: Arc<tokio::sync::RwLock<String>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("DisplayConfig: monitor availability started (event-driven via display state polling)");
    
    let mut last_monitor_count = 0;
    
    loop {
        // Check display state every 2 seconds
        // This is lightweight - just checking current config
        match display.get_current_state().await {
            Ok((_, _physical, logical, _)) => {
                let current_count = logical.len();
                
                // If monitor count changed, trigger availability check
                if current_count != last_monitor_count {
                    info!("DisplayConfig: monitor count changed {} -> {}", last_monitor_count, current_count);
                    last_monitor_count = current_count;
                    let _ = availability_tx.send(());
                } else if current_count >= 2 {
                    // Both monitors present - check if swap needed (safety check)
                    let desired = desired_primary.read().await.clone();
                    let (logical_map, _) = read_current_config(&logical);
                    
                    if let Some((_, _, _, _, is_primary)) = logical_map.get(&desired) {
                        if !*is_primary {
                            // Desired monitor available but not primary - trigger check
                            let _ = availability_tx.send(());
                        }
                    }
                }
            }
            Err(e) => {
                warn!("DisplayConfig: failed to get state: {}", e);
            }
        }
        
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

pub async fn run(mut orient_rx: broadcast::Receiver<String>, mut kb_rx: broadcast::Receiver<bool>, mut swap_rx: broadcast::Receiver<()>, mut toggle_rx: broadcast::Receiver<bool>, swap_result_tx: tokio::sync::mpsc::Sender<String>, kb_result_tx: tokio::sync::mpsc::Sender<()>, toggle_result_tx: tokio::sync::mpsc::Sender<()>, desired_primary: Arc<tokio::sync::RwLock<String>>) {
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
    
    // Start display availability monitor task
    {
        let display_clone = display.clone();
        let availability_tx_clone = availability_tx.clone();
        let desired_primary_clone = Arc::clone(&desired_primary);
        tokio::spawn(async move {
            loop {
                if let Err(e) = monitor_display_availability(&display_clone, availability_tx_clone.clone(), desired_primary_clone.clone()).await {
                    error!("DisplayConfig: availability monitor error: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
    }

    // Cache for secondary monitor config so we can restore it after disabling
    let mut cached_secondary: Option<(String, String, i32, i32, f64)> = None;  // (connector, mode_id, x, y, scale)
    let mut availability_rx = availability_tx.subscribe();

    loop {
        tokio::select! {
            msg = orient_rx.recv() => match msg {
                Ok(orientation) => {
                    if let Err(e) = apply_rotation(&display, &orientation).await {
                        warn!("Rotation failed for '{orientation}': {e}");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("Display handler lagged by {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = swap_rx.recv() => match msg {
                Ok(_response_tx) => {
                    info!("Display: swap_rx received event");
                    let start = std::time::Instant::now();
                    match apply_display_swap(&display).await {
                        Ok(new_primary) => {
                            info!("SwapDisplays completed in {:.2}ms, new_primary={}", start.elapsed().as_secs_f64() * 1000.0, new_primary);
                            let _ = swap_result_tx.send(new_primary).await;
                        }
                        Err(e) => {
                            warn!("SwapDisplays failed: {e}");
                            let _ = swap_result_tx.send("error".to_string()).await;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("Swap handler lagged by {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = toggle_rx.recv() => match msg {
                Ok(enable) => {
                    info!("Display: toggle_rx received event enable={enable}");
                    let start = std::time::Instant::now();
                    if let Err(e) = apply_toggle_secondary_display(&display, enable, &mut cached_secondary, &desired_primary).await {
                        warn!("Toggle secondary display failed: {e}");
                    } else {
                        info!("Toggle secondary display completed in {:.2}ms", start.elapsed().as_secs_f64() * 1000.0);
                    }
                    // Send ACK to root daemon
                    let _ = toggle_result_tx.send(()).await;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("Toggle handler lagged by {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = kb_rx.recv() => match msg {
                Ok(attached) => {
                    info!("Display: keyboard_attached={attached}");
                    let start = std::time::Instant::now();
                    if let Err(e) = handle_keyboard_attached(&display, attached, &mut cached_secondary, &desired_primary).await {
                        warn!("Handle keyboard attached failed: {e}");
                    } else {
                        info!("Handle keyboard attached completed in {:.2}ms", start.elapsed().as_secs_f64() * 1000.0);
                    }
                    // Send ACK to root daemon
                    let _ = kb_result_tx.send(()).await;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("Keyboard handler lagged by {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = availability_rx.recv() => match msg {
                Ok(_) => {
                    // D-Bus signal indicates display configuration changed
                    // Check if desired_primary is now available and not already primary
                    if let Ok((_serial, _physical, logical, _)) = display.get_current_state().await {
                        let (logical_map, _) = read_current_config(&logical);
                        let desired = desired_primary.read().await.clone();
                        
                        // Verify: desired is available AND not primary
                        if let Some((_, _, _, _, is_primary)) = logical_map.get(&desired) {
                            if !*is_primary {
                                info!("Availability check: {} is available but not primary, attempting swap", desired);
                                match apply_display_swap(&display).await {
                                    Ok(new_primary) => {
                                        info!("Restored desired_primary via availability signal: {}", new_primary);
                                    }
                                    Err(e) => {
                                        warn!("Failed to restore desired_primary: {}", e);
                                    }
                                }
                            } else {
                                debug!("Availability check: {} is already primary", desired);
                            }
                        } else {
                            debug!("Availability check: {} not available", desired);
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {} // Ignore lag on availability checks
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
}
