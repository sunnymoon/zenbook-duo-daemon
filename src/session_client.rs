use log::{debug, info, warn};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt, AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use std::time::Duration;
use tokio::sync::Mutex;

// Global session ID tracker to detect when session daemon restarts
static LAST_SESSION_ID: Mutex<Option<u64>> = tokio::sync::Mutex::const_new(None);

#[derive(Serialize, Debug)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum SessionCmd {
    KeyboardAttached { value: bool },
    SwapDisplays,
    ToggleSecondaryDisplay { enable: bool },
    SetDesiredPrimary { value: String },
}

/// Check if a new session daemon has connected
/// Returns true if this is a NEW session (different ID from last seen)
pub async fn is_new_session() -> bool {
    let sock_path = {
        let uid = unsafe { nix::libc::getuid() };
        format!("/run/user/{uid}/zenbook-duo-session.sock")
    };
    
    if !tokio::fs::try_exists(&sock_path).await.unwrap_or(false) {
        debug!("is_new_session: socket not found at {}", sock_path);
        return false;
    }
    
    match UnixStream::connect(&sock_path).await {
        Ok(stream) => {
            let (reader, _) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            
            // Read the session identity message: "session_ready:{session_id}"
            if let Ok(_) = reader.read_line(&mut line).await {
                if let Some(session_id_str) = line.strip_prefix("session_ready:") {
                    if let Ok(session_id) = session_id_str.trim().parse::<u64>() {
                        let mut last_id = LAST_SESSION_ID.lock().await;
                        
                        // Check if this is a NEW session
                        let is_new = last_id.as_ref() != Some(&session_id);
                        
                        if is_new {
                            info!("is_new_session: Detected new session daemon: {}", session_id);
                            *last_id = Some(session_id);
                        }
                        
                        return is_new;
                    }
                }
            }
            debug!("is_new_session: failed to read session_ready message");
        }
        Err(e) => {
            debug!("is_new_session: Failed to connect to session socket: {}", e);
        }
    }
    
    false
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
            Ok(mut stream) => {
                if let Err(e) = stream.write_all(json.as_bytes()).await {
                    warn!("Failed to write to session daemon {:?}: {e}", sock_path);
                    continue;
                }
                debug!("Sent to session daemon {:?}: {json}", sock_path);
                
                // Wait for response with generous timeout (5s) for display operations
                let mut buf = vec![0; 1024];
                match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
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

