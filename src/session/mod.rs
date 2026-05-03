mod display;
mod notifications;
mod orientation;
// mod osk; // OSK support not yet implemented, kept for future use

use log::{error, info, warn};
use serde::Deserialize;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::broadcast;

#[derive(Deserialize, Debug)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum RootCmd {
    KeyboardAttached { value: bool },
    SwapDisplays,
    ToggleSecondaryDisplay { enable: bool },
    SetDesiredPrimary { value: String },
    RestoreDesiredPrimary,
}

pub async fn run() {
    let uid = unsafe { nix::libc::getuid() };
    let sock_path = format!("/run/user/{uid}/zenbook-duo-session.sock");

    // Remove stale socket from a previous crash
    let _ = tokio::fs::remove_file(&sock_path).await;

    let (kb_tx, _) = broadcast::channel::<bool>(8);
    let (orient_tx, _) = broadcast::channel::<String>(8);
    let (swap_tx, _) = broadcast::channel::<()>(8);
    let (toggle_tx, _) = broadcast::channel::<bool>(8);
    
    // Channel for swap results: (request_id, new_primary) - shared by all connections
    let (swap_result_tx, swap_result_rx) = tokio::sync::mpsc::channel(8);
    let swap_result_rx = Arc::new(tokio::sync::Mutex::new(swap_result_rx));
    
    // Channel for keyboard attach/detach result - shared by all connections
    let (kb_result_tx, kb_result_rx) = tokio::sync::mpsc::channel(8);
    let kb_result_rx = Arc::new(tokio::sync::Mutex::new(kb_result_rx));
    
    // Channel for toggle secondary display result - shared by all connections
    let (toggle_result_tx, toggle_result_rx) = tokio::sync::mpsc::channel(8);
    let toggle_result_rx = Arc::new(tokio::sync::Mutex::new(toggle_result_rx));
    
    // Persistent desired_primary state (in memory, set by root daemon on reconnect)
    let desired_primary = Arc::new(tokio::sync::RwLock::new(String::from("eDP-1")));
    
    // Unique session ID to detect when session daemon restarts
    let session_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let session_id_str = format!("session_ready:{}\n", session_id);

    // Orientation monitor → display rotation
    {
        let tx = orient_tx.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = orientation::run(tx.clone()).await {
                    error!("Orientation monitor error: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });
    }

    tokio::spawn(display::run(orient_tx.subscribe(), kb_tx.subscribe(), swap_tx.subscribe(), toggle_tx.subscribe(), swap_result_tx.clone(), kb_result_tx.clone(), toggle_result_tx.clone(), desired_primary.clone()));
    tokio::spawn(notifications::run(kb_tx.subscribe()));

    // Unix socket listener for commands from the root daemon
    let listener = match UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind session socket at {sock_path}: {e}");
            return;
        }
    };
    info!("Session daemon listening on {sock_path}");

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                error!("Session socket accept error: {e}");
                break;
            }
        };

        let kb_tx = kb_tx.clone();
        let swap_tx = swap_tx.clone();
        let toggle_tx = toggle_tx.clone();
        let swap_result_rx = Arc::clone(&swap_result_rx);
        let kb_result_rx = Arc::clone(&kb_result_rx);
        let toggle_result_rx = Arc::clone(&toggle_result_rx);
        let desired_primary = Arc::clone(&desired_primary);
        let session_id_str = session_id_str.clone();
        
        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            
            // Send session identity first so root daemon knows if this is a new session
            if let Err(e) = writer.write_all(session_id_str.as_bytes()).await {
                warn!("Failed to write session identity to root daemon: {e}");
                return;
            }
            
            let reader = BufReader::new(reader);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let handled = match serde_json::from_str::<RootCmd>(&line) {
                    Ok(RootCmd::KeyboardAttached { value }) => {
                        info!("Received keyboard_attached={value}");
                        kb_tx.send(value).ok();
                        // Wait for display module to handle and ack with 5s timeout
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            kb_result_rx.lock().await.recv()
                        ).await {
                            Ok(Some(_)) => {
                                let response = "ok\n";
                                if let Err(e) = writer.write_all(response.as_bytes()).await {
                                    warn!("Failed to write response to root daemon: {e}");
                                    break;
                                }
                            }
                            _ => {
                                if let Err(e) = writer.write_all(b"error:timeout\n").await {
                                    warn!("Failed to write response to root daemon: {e}");
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                    Ok(RootCmd::SetDesiredPrimary { value }) => {
                        info!("Received set_desired_primary={}", value);
                        *desired_primary.write().await = value.clone();
                        
                        // Store the desired state WITHOUT attempting swap
                        // Swaps should only be triggered by keyboard attach/detach or secondary display toggle events
                        // This avoids conflicts between multiple swap requests
                        info!("Stored desired_primary={} (swap will be applied by attach/detach/toggle events)", value);
                        
                        if let Err(e) = writer.write_all(b"ok\n").await {
                            warn!("Failed to write response to root daemon: {e}");
                            break;
                        }
                        continue;
                    }
                    Ok(RootCmd::RestoreDesiredPrimary) => {
                        info!("Received restore_desired_primary");
                        let restore_to = desired_primary.read().await.clone();
                        info!("RestoreDesiredPrimary: desired_primary is: {}", restore_to);
                        // Just acknowledge - the display module handles actual swap logic based on events
                        if let Err(e) = writer.write_all(b"ok\n").await {
                            warn!("Failed to write response to root daemon: {e}");
                            break;
                        }
                        continue;
                    }
                    Ok(RootCmd::SwapDisplays) => {
                        info!("Received swap_displays");
                        swap_tx.send(()).ok();
                        // Wait for result from display module with 5s timeout
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            swap_result_rx.lock().await.recv()
                        ).await {
                            Ok(Some(new_primary)) => {
                                let response = format!("ok:{}\n", new_primary);
                                if let Err(e) = writer.write_all(response.as_bytes()).await {
                                    warn!("Failed to write response to root daemon: {e}");
                                    break;
                                }
                            }
                            _ => {
                                if let Err(e) = writer.write_all(b"error:timeout\n").await {
                                    warn!("Failed to write response to root daemon: {e}");
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                    Ok(RootCmd::ToggleSecondaryDisplay { enable }) => {
                        info!("Received toggle_secondary_display enable={enable}");
                        toggle_tx.send(enable).ok();
                        // Wait for display module to handle and ack with 5s timeout
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            toggle_result_rx.lock().await.recv()
                        ).await {
                            Ok(Some(_)) => {
                                let response = "ok\n";
                                if let Err(e) = writer.write_all(response.as_bytes()).await {
                                    warn!("Failed to write response to root daemon: {e}");
                                    break;
                                }
                            }
                            _ => {
                                if let Err(e) = writer.write_all(b"error:timeout\n").await {
                                    warn!("Failed to write response to root daemon: {e}");
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                    Err(e) => {
                        warn!("Unknown root command: {line:?} ({e})");
                        false
                    }
                };
                
                let response = if handled { "ok\n" } else { "error: unknown command\n" };
                if let Err(e) = writer.write_all(response.as_bytes()).await {
                    warn!("Failed to write response to root daemon: {e}");
                    break;
                }
            }
        });
    }
}

