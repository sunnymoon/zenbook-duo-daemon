use log::{debug, warn};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use std::time::Duration;

#[derive(Serialize, Debug)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum SessionCmd {
    KeyboardAttached { value: bool },
    #[allow(dead_code)]
    SetDesiredPrimary { value: String },
    ToggleSecondaryDisplay { enable: bool },
}

/// Send command to session daemon and wait for response.
/// Returns the response string if successful, empty string if no session or error.
/// Uses a generous timeout (5 seconds) since some commands (swap, toggle) may take time.
pub async fn try_send_with_response(cmd: &SessionCmd) -> String {
    let json = match serde_json::to_string(cmd) {
        Ok(j) => format!("{j}\n"),
        Err(e) => {
            warn!("Failed to serialize session command: {e}");
            return String::new();
        }
    };

    let Ok(mut dir) = tokio::fs::read_dir("/run/user").await else {
        debug!("No /run/user directory");
        return String::new();
    };

    while let Ok(Some(entry)) = dir.next_entry().await {
        let sock_path = entry.path().join("zenbook-duo-session.sock");
        if !tokio::fs::try_exists(&sock_path).await.unwrap_or(false) {
            continue;
        }
        match UnixStream::connect(&sock_path).await {
            Ok(stream) => {
                // Split stream: separate reader and writer to avoid conflicts
                let (mut reader, mut writer) = tokio::io::split(stream);
                
                // Write command
                if let Err(e) = writer.write_all(json.as_bytes()).await {
                    warn!("Failed to write to session daemon {:?}: {e}", sock_path);
                    continue;
                }
                debug!("Sent to session daemon {:?}: {json}", sock_path);
                
                // Ensure write is flushed before reading
                if let Err(e) = writer.flush().await {
                    warn!("Failed to flush to session daemon: {e}");
                    continue;
                }
                
                // Wait for response with generous timeout (5s) for display operations
                let mut buf = vec![0; 1024];
                match tokio::time::timeout(Duration::from_secs(5), reader.read(&mut buf)).await {
                    Ok(Ok(n)) => {
                        if n > 0 {
                            let response = String::from_utf8_lossy(&buf[..n]).trim().to_string();
                            debug!("Session daemon response: {}", response);
                            return response;
                        }
                    }
                    Ok(Err(e)) => {
                        warn!("Failed to read response from session daemon: {e}");
                    }
                    Err(_) => {
                        warn!("Timeout waiting for session daemon response (5s)");
                    }
                }
            }
            Err(e) => debug!("No session daemon at {:?}: {e}", sock_path),
        }
    }
    
    String::new()
}

