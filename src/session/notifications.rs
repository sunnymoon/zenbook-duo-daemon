use std::collections::HashMap;
use log::{error, warn};
use tokio::sync::broadcast;
use zbus::{Connection, proxy, zvariant::Value};

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
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

pub async fn send_notification(
    summary: &str,
    body: &str,
    icon: &str,
    expire_timeout: i32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let conn = Connection::session().await?;
    let notif = NotificationsProxy::new(&conn).await?;
    let _ = notif
        .notify(
            "zenbook-duo-daemon",
            0,
            icon,
            summary,
            body,
            vec![],
            HashMap::new(),
            expire_timeout,
        )
        .await?;
    Ok(())
}

pub async fn run(mut kb_rx: broadcast::Receiver<bool>) {
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

    let mut replaces_id = 0u32;
    let mut initialized = false;
    let mut last_attached: Option<bool> = None;

    loop {
        match kb_rx.recv().await {
            Ok(attached) => {
                if !initialized {
                    initialized = true;
                    last_attached = Some(attached);
                    continue;
                }

                if last_attached == Some(attached) {
                    continue;
                }
                last_attached = Some(attached);

                let (summary, body, icon) = if attached {
                    ("Keyboard Connected", "Zenbook Duo keyboard is now attached", "input-keyboard")
                } else {
                    ("Keyboard Detached", "Zenbook Duo keyboard has been removed", "input-keyboard-virtual")
                };
                match notif.notify(
                    "zenbook-duo-daemon",
                    replaces_id,
                    icon,
                    summary,
                    body,
                    vec![],
                    HashMap::new(),
                    3000,
                ).await {
                    Ok(id) => replaces_id = id,
                    Err(e) => warn!("Failed to send notification: {e}"),
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}
