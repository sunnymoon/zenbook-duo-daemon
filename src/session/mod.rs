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
    SetDesiredPrimary { value: String },
    ToggleSecondaryDisplay { enable: bool },
}

pub async fn run() {
    let uid = unsafe { nix::libc::getuid() };
    let sock_path = format!("/run/user/{uid}/zenbook-duo-session.sock");

    // Remove stale socket from a previous crash
    let _ = tokio::fs::remove_file(&sock_path).await;

    let (kb_tx, _) = broadcast::channel::<bool>(8);
    let (orient_tx, _) = broadcast::channel::<String>(8);
    let (toggle_tx, _) = broadcast::channel::<bool>(8);
    let (desired_primary_tx, _) = broadcast::channel::<String>(8);
    
    // Channel for keyboard attach/detach result - shared by all connections
    let (kb_result_tx, kb_result_rx) = tokio::sync::mpsc::channel(8);
    let kb_result_rx = Arc::new(tokio::sync::Mutex::new(kb_result_rx));
    
    // Channel for toggle secondary display result - shared by all connections
    let (toggle_result_tx, toggle_result_rx) = tokio::sync::mpsc::channel(8);
    let toggle_result_rx = Arc::new(tokio::sync::Mutex::new(toggle_result_rx));
    
    // Persistent desired_primary state (in memory, initially eDP-1)
    // Will be updated by root daemon on connect via daemon socket
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

    tokio::spawn(display::run(
        orient_tx.subscribe(),
        kb_tx.subscribe(),
        toggle_tx.subscribe(),
        desired_primary_tx.subscribe(),
        toggle_result_tx.clone(),
        kb_result_tx.clone(),
        desired_primary.clone(),
    ));
    tokio::spawn(notifications::run(kb_tx.subscribe()));

    // Start connecting to root daemon socket to get desired_primary updates
    {
        let desired_primary_clone = Arc::clone(&desired_primary);
        let desired_primary_tx = desired_primary_tx.clone();
        let kb_tx_for_root = kb_tx.clone();
        let toggle_tx_for_root = toggle_tx.clone();
        tokio::spawn(async move {
            loop {
                info!("SESSION: Attempting to connect to root daemon socket...");
                match crate::daemon_socket::connect_to_root().await {
                    Ok(stream) => {
                        if let Err(e) = crate::daemon_socket::listen_from_root_and_update(
                            stream,
                            desired_primary_clone.clone(),
                            desired_primary_tx.clone(),
                            kb_tx_for_root.clone(),
                            toggle_tx_for_root.clone(),
                        ).await {
                            error!("SESSION: Error listening to root daemon: {}", e);
                        }
                        info!("SESSION: Root daemon connection closed, will reconnect...");
                    }
                    Err(e) => {
                        error!("SESSION: Failed to connect to root daemon: {}", e);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
    }

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
        let toggle_tx = toggle_tx.clone();
        let kb_result_rx = Arc::clone(&kb_result_rx);
        let toggle_result_rx = Arc::clone(&toggle_result_rx);
        let desired_primary = Arc::clone(&desired_primary);
        let desired_primary_tx = desired_primary_tx.clone();
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
                match serde_json::from_str::<RootCmd>(&line) {
                    Ok(RootCmd::KeyboardAttached { value }) => {
                        info!("Received keyboard_attached={value}");
                        kb_tx.send(value).ok();
                        // Wait for display module to handle and ack with 5s timeout
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            kb_result_rx.lock().await.recv()
                        ).await {
                            Ok(Some(_)) => {
                                if let Err(e) = writer.write_all(b"ok\n").await {
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
                    }
                    Ok(RootCmd::SetDesiredPrimary { value }) => {
                        info!("SESSION: Received SetDesiredPrimary from root daemon, value={}", value);
                        *desired_primary.write().await = value.clone();
                        info!("SESSION: Stored desired_primary={}", value);
                        desired_primary_tx.send(value).ok();
                        
                        if let Err(e) = writer.write_all(b"ok\n").await {
                            warn!("Failed to write response to root daemon: {e}");
                            break;
                        }
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
                                if let Err(e) = writer.write_all(b"ok\n").await {
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
                    }
                    Err(e) => {
                        warn!("Unknown root command: {line:?} ({e})");
                        if let Err(e) = writer.write_all(b"error:unknown\n").await {
                            warn!("Failed to write response to root daemon: {e}");
                            break;
                        }
                    }
                }
            }
        });
    }
}
