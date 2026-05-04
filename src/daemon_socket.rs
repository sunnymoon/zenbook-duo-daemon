// Bidirectional socket communication between root daemon and session daemon
// Root daemon: creates and listens on the socket, broadcasts updates to all connected clients
// Session daemon: connects to the root daemon's socket and receives broadcasts

use log::{error, info, warn, debug};
use serde::{Serialize, Deserialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use std::path::Path;
use std::os::unix::fs::PermissionsExt;
use tokio::sync::broadcast;

static SESSION_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

struct SessionConnectionGuard;

impl SessionConnectionGuard {
    fn new() -> Self {
        SESSION_CONNECTIONS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for SessionConnectionGuard {
    fn drop(&mut self) {
        SESSION_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
    }
}

pub fn is_session_connected() -> bool {
    SESSION_CONNECTIONS.load(Ordering::SeqCst) > 0
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum DaemonMsg {
    /// Root daemon → Session daemon: Send current desired_primary on connect
    DesiredPrimaryUpdate { value: String },
    
    /// Session daemon → Root daemon: Acknowledge receipt
    Ack { msg: String },
    
    /// Root daemon → Session daemon: Keyboard attached/detached
    KeyboardAttached { value: bool },
    
    /// Root daemon → Session daemon: Toggle secondary display
    ToggleSecondaryDisplay { enable: bool },
}

/// Start the daemon socket server (called by root daemon)
/// Listens on a Unix socket and broadcasts desired_primary updates to all connected session daemons
/// Takes a broadcast sender to notify about state changes
pub async fn start_server(
    state_manager: &crate::state::KeyboardStateManager,
    state_update_tx: broadcast::Sender<DaemonMsg>,
    ack_tx: broadcast::Sender<String>,
) {
    let sock_path = "/run/zenbook-duo-daemon.sock";
    
    // Remove stale socket if it exists
    if Path::new(sock_path).exists() {
        match std::fs::remove_file(sock_path) {
            Ok(_) => info!("Removed stale daemon socket"),
            Err(e) => warn!("Failed to remove stale socket: {}", e),
        }
    }
    
    match UnixListener::bind(sock_path) {
        Ok(listener) => {
            // Set socket permissions to 0o666 so any user can connect
            if let Err(e) = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o666)) {
                warn!("Failed to set socket permissions: {}", e);
            }
            
            info!("Daemon socket server listening on {}", sock_path);
            
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let state_mgr = state_manager.clone();
                        let update_tx = state_update_tx.clone();
                        let ack_tx = ack_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_session_connection(stream, state_mgr, update_tx, ack_tx).await {
                                warn!("Session connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        error!("Socket accept error: {}", e);
                    }
                }
            }
        }
        Err(e) => {
            error!("Failed to bind daemon socket: {}", e);
        }
    }
}

/// Handle incoming session daemon connection
/// Send desired_primary immediately and listen for broadcasts/updates
async fn handle_session_connection(
    stream: tokio::net::UnixStream,
    state_manager: crate::state::KeyboardStateManager,
    state_update_tx: broadcast::Sender<DaemonMsg>,
    ack_tx: broadcast::Sender<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _connection_guard = SessionConnectionGuard::new();
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let mut broadcast_rx = state_update_tx.subscribe();
    
    // Send desired_primary immediately on connect
    if let Some(desired_primary) = state_manager.get_desired_primary() {
        let msg = DaemonMsg::DesiredPrimaryUpdate {
            value: desired_primary.clone(),
        };
        let json = serde_json::to_string(&msg)?;
        writer.write_all(format!("{}\n", json).as_bytes()).await?;
        writer.flush().await?;
        info!("ROOT: Sent DesiredPrimaryUpdate={} to session daemon", desired_primary);
    }
    
    // Listen for incoming messages from session and broadcasts from state changes
    loop {
        tokio::select! {
            // Handle messages from session daemon
            result = reader.read_line(&mut line) => {
                match result {
                    Ok(0) => {
                        // Connection closed
                        info!("ROOT: Session daemon connection closed");
                        break;
                    }
                    Ok(_) => {
                        // Parse and handle message
                        match serde_json::from_str::<DaemonMsg>(&line) {
                            Ok(msg) => {
                                debug!("ROOT: Received message from session: {:?}", msg);
                                // Handle message if needed (e.g., acknowledgments)
                                match msg {
                                    DaemonMsg::Ack { msg: ack_msg } => {
                                        debug!("ROOT: Session acknowledged: {}", ack_msg);
                                        let _ = ack_tx.send(ack_msg);
                                    }
                                    _ => {
                                        // Other messages can be handled here if needed
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("ROOT: Failed to parse message from session: {}", e);
                            }
                        }
                        line.clear();
                    }
                    Err(e) => {
                        error!("ROOT: Error reading from session: {}", e);
                        break;
                    }
                }
            }
            
            // Handle broadcasts from root daemon state changes
            result = broadcast_rx.recv() => {
                match result {
                    Ok(msg) => {
                        match &msg {
                            DaemonMsg::DesiredPrimaryUpdate { value } => {
                                info!("ROOT: Broadcasting DesiredPrimaryUpdate={} to connected session daemon", value);
                            }
                            DaemonMsg::KeyboardAttached { value } => {
                                info!("ROOT: Broadcasting KeyboardAttached={} to connected session daemon", value);
                            }
                            DaemonMsg::ToggleSecondaryDisplay { enable } => {
                                info!("ROOT: Broadcasting ToggleSecondaryDisplay={} to connected session daemon", enable);
                            }
                            DaemonMsg::Ack { .. } => {}
                        }
                        if let Ok(json) = serde_json::to_string(&msg) {
                            if let Err(e) = writer.write_all(format!("{}\n", json).as_bytes()).await {
                                error!("ROOT: Failed to send broadcast to session daemon: {}", e);
                                break;
                            }
                            if let Err(e) = writer.flush().await {
                                error!("ROOT: Failed to flush to session daemon: {}", e);
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        warn!("ROOT: Session daemon fell behind on broadcasts");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("ROOT: State update broadcast channel closed");
                        break;
                    }
                }
            }
        }
    }
    
    Ok(())
}

/// Broadcast a daemon message to all connected session daemons
#[allow(dead_code)]
pub async fn broadcast_message(tx: &broadcast::Sender<DaemonMsg>, msg: DaemonMsg) {
    match tx.send(msg) {
        Ok(_) => {
            debug!("ROOT: Broadcasted daemon message");
        }
        Err(e) => {
            warn!("ROOT: Failed to broadcast daemon message: {}", e);
        }
    }
}

/// Connect to root daemon socket (called by session daemon)
/// Returns a connection that can read messages from root daemon
pub async fn connect_to_root() -> Result<tokio::net::UnixStream, Box<dyn std::error::Error + Send + Sync>> {
    let sock_path = "/run/zenbook-duo-daemon.sock";
    
    loop {
        match tokio::net::UnixStream::connect(sock_path).await {
            Ok(stream) => {
                info!("SESSION: Connected to root daemon at {}", sock_path);
                return Ok(stream);
            }
            Err(e) => {
                debug!("SESSION: Failed to connect to root daemon: {}", e);
                debug!("SESSION: Retrying in 2 seconds...");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

/// Listen for messages from root daemon and update session state
/// Called by session daemon in a loop to handle incoming messages
pub async fn listen_from_root_and_update(
    stream: tokio::net::UnixStream,
    desired_primary: Arc<tokio::sync::RwLock<String>>,
    desired_primary_tx: broadcast::Sender<String>,
    kb_tx: broadcast::Sender<bool>,
    toggle_tx: broadcast::Sender<bool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                info!("SESSION: Root daemon connection closed");
                break;
            }
            Ok(_) => {
                match serde_json::from_str::<DaemonMsg>(&line) {
                    Ok(msg) => {
                        info!("SESSION: Received from root: {:?}", msg);
                        match msg {
                            DaemonMsg::DesiredPrimaryUpdate { value } => {
                                info!("SESSION: Root daemon sent DesiredPrimaryUpdate={}", value);
                                *desired_primary.write().await = value.clone();
                                if let Err(e) = desired_primary_tx.send(value.clone()) {
                                    warn!("SESSION: Failed to notify display loop about desired_primary update: {}", e);
                                }
                                let ack = serde_json::to_string(&DaemonMsg::Ack {
                                    msg: format!("desired_primary_received:{value}"),
                                })?;
                                writer.write_all(format!("{ack}\n").as_bytes()).await?;
                                writer.flush().await?;
                            }
                            DaemonMsg::KeyboardAttached { value } => {
                                info!("SESSION: Root daemon sent KeyboardAttached={}", value);
                                kb_tx.send(value).ok();
                                let ack = serde_json::to_string(&DaemonMsg::Ack {
                                    msg: format!("keyboard_attached_received:{value}"),
                                })?;
                                writer.write_all(format!("{ack}\n").as_bytes()).await?;
                                writer.flush().await?;
                            }
                            DaemonMsg::ToggleSecondaryDisplay { enable } => {
                                info!("SESSION: Root daemon sent ToggleSecondaryDisplay={}", enable);
                                toggle_tx.send(enable).ok();
                                let ack = serde_json::to_string(&DaemonMsg::Ack {
                                    msg: format!("toggle_secondary_received:{enable}"),
                                })?;
                                writer.write_all(format!("{ack}\n").as_bytes()).await?;
                                writer.flush().await?;
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        warn!("SESSION: Failed to parse message from root: {}", e);
                    }
                }
            }
            Err(e) => {
                error!("SESSION: Error reading from root: {}", e);
                break;
            }
        }
    }
    
    Ok(())
}
