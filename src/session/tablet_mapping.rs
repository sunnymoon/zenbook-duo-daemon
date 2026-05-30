//! GNOME Wayland tablet-to-output mapping for Zenbook Duo integrated pen devices.
//!
//! On **UX8406**-class machines the integrated Elan pen stack is typically exposed as two USB
//! tablet nodes (**`04f3:4447`**, **`04f3:4448`**). GNOME stores per-device mapping under the
//! relocatable schema
//! `org.gnome.desktop.peripherals.tablet:/org/gnome/desktop/peripherals/tablets/<vid>:<pid>/`
//! key **`output`**, as an `as` GVariant: `['VENDOR','PRODUCT','SERIAL']` matching Mutter’s
//! physical monitor EDID strings from `GetCurrentState`.

use log::{info, warn};

use crate::config::{TabletMapMode, TabletMappingConfig};
use crate::session::display::PhysicalMonitor;

const TABLET_A: &str = "04f3:4447";
const TABLET_B: &str = "04f3:4448";

pub(crate) fn edid_for_connector(
    physical: &[PhysicalMonitor],
    connector: &str,
) -> Option<(String, String, String)> {
    for ((conn, vendor, product, serial), _, _) in physical {
        if conn == connector {
            return Some((vendor.clone(), product.clone(), serial.clone()));
        }
    }
    None
}

fn gvariant_output(va: &str, vb: &str, vc: &str) -> String {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('\'', "\\'");
    format!("['{}','{}','{}']", esc(va), esc(vb), esc(vc))
}

async fn gsettings_set_tablet_output(usb_id: &str, value: &str) -> Result<(), String> {
    let schema_path = format!(
        "org.gnome.desktop.peripherals.tablet:/org/gnome/desktop/peripherals/tablets/{usb_id}/"
    );
    let out = tokio::process::Command::new("gsettings")
        .args(["set", &schema_path, "output", value])
        .output()
        .await
        .map_err(|e| format!("gsettings spawn: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "gsettings exit {:?}: {}",
            out.status.code(),
            stderr.trim()
        ));
    }
    Ok(())
}

/// Reapply GNOME tablet `output` keys from Mutter physical monitor EDIDs.
///
/// Call only after a **successful** display reconcile when topology should match Mutter.
pub(crate) async fn reapply_after_reconcile(
    cfg: &TabletMappingConfig,
    desired_primary_connector: &str,
    physical: &[PhysicalMonitor],
) {
    if !cfg.enable {
        return;
    }

    let Some(edp1) = edid_for_connector(physical, "eDP-1") else {
        warn!("Tablet mapping: no Mutter physical row for eDP-1; skipping pen remap");
        return;
    };

    let edp2 = edid_for_connector(physical, "eDP-2");
    let primary_triple: &(String, String, String) = match desired_primary_connector {
        "eDP-2" => edp2.as_ref().unwrap_or(&edp1),
        _ => &edp1,
    };

    match cfg.mode {
        TabletMapMode::OneToOne => {
            let v_top = gvariant_output(&edp1.0, &edp1.1, &edp1.2);
            match gsettings_set_tablet_output(TABLET_A, &v_top).await {
                Ok(()) => info!("Tablet mapping: {TABLET_A} → eDP-1 ({})", edp1.1),
                Err(e) => warn!("Tablet mapping: {TABLET_A}: {e}"),
            }
            if let Some(ref t2) = edp2 {
                let v_bot = gvariant_output(&t2.0, &t2.1, &t2.2);
                match gsettings_set_tablet_output(TABLET_B, &v_bot).await {
                    Ok(()) => info!("Tablet mapping: {TABLET_B} → eDP-2 ({})", t2.1),
                    Err(e) => warn!("Tablet mapping: {TABLET_B}: {e}"),
                }
            } else {
                warn!(
                    "Tablet mapping: no Mutter physical row for eDP-2; left {TABLET_B} unchanged"
                );
            }
        }
        TabletMapMode::AllToPrimary => {
            let v = gvariant_output(
                &primary_triple.0,
                &primary_triple.1,
                &primary_triple.2,
            );
            match gsettings_set_tablet_output(TABLET_A, &v).await {
                Ok(()) => info!(
                    "Tablet mapping: {TABLET_A} → primary {desired_primary_connector} ({})",
                    primary_triple.1
                ),
                Err(e) => warn!("Tablet mapping: {TABLET_A}: {e}"),
            }
            match gsettings_set_tablet_output(TABLET_B, &v).await {
                Ok(()) => info!(
                    "Tablet mapping: {TABLET_B} → primary {desired_primary_connector} ({})",
                    primary_triple.1
                ),
                Err(e) => warn!("Tablet mapping: {TABLET_B}: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::gvariant_output;

    #[test]
    fn gvariant_escapes_single_quote_in_edid_field() {
        assert_eq!(gvariant_output("x", "y", "a'b"), r"['x','y','a\'b']");
    }
}
