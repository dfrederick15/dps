//! IPC protocol between dps-daemon and dps-gui.
//!
//! Transport: Unix domain socket at `$XDG_RUNTIME_DIR/dps.sock`.
//! Encoding: newline-delimited JSON — one JSON object per line.
//!
//! Flow:
//! 1. GUI connects, sends `DaemonRequest::Subscribe`.
//! 2. Daemon sends back the current `DaemonStatus` as a `DaemonEvent::Status`.
//! 3. Daemon continues pushing `DaemonEvent`s as they occur.
//! 4. GUI may send additional one-shot `DaemonRequest`s at any time.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── Socket path ───────────────────────────────────────────────────────────────

pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("dps.sock");
    }
    // Fallback: derive from /proc/self/status Uid line (no libc dep)
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        if let Some(uid) = s.lines()
            .find(|l| l.starts_with("Uid:"))
            .and_then(|l| l.split_whitespace().nth(1))
        {
            return PathBuf::from(format!("/run/user/{uid}/dps.sock"));
        }
    }
    dirs::home_dir()
        .map(|h| h.join(".local/share/dps/daemon.sock"))
        .unwrap_or_else(|| PathBuf::from("/tmp/dps-daemon.sock"))
}

// ── Request (GUI → daemon) ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonRequest {
    /// Subscribe to streaming events (daemon keeps sending after this).
    Subscribe,
    /// One-shot status query (no subscription).
    GetStatus,
    /// Trigger an immediate sync.
    SyncNow { direction: SyncDir },
    /// Start the background watcher.
    StartWatch { direction: WatchDir, debounce_ms: u64, poll_secs: u64 },
    /// Stop the background watcher.
    StopWatch,
    /// Start the container.
    StartContainer,
    /// Stop the container.
    StopContainer,
    /// Ask the daemon to exit.
    Shutdown,
}

// ── Response (daemon → GUI, one per request) ──────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonResponse {
    Ok,
    Error { message: String },
    Status(DaemonStatus),
}

// ── Event (daemon → subscribed GUI clients, pushed any time) ─────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonEvent {
    /// Full status snapshot (sent immediately on subscribe, and on any change).
    Status(DaemonStatus),
    /// A sync operation started.
    SyncStarted { direction: SyncDir, path: String },
    /// A sync operation completed successfully.
    SyncCompleted { direction: SyncDir, path: String },
    /// A sync operation failed.
    SyncFailed { direction: SyncDir, path: String, error: String },
    /// The daemon is shutting down.
    Shutdown,
}

// ── Status snapshot ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub container_status:     String,          // "running" | "stopped" | "unknown"
    pub container_ip:         Option<String>,
    pub tailscale_ip:         Option<String>,
    pub watch_running:        bool,
    pub watch_direction:      Option<WatchDir>,
    pub last_sync_time:       Option<String>,  // "HH:MM:SS"
    pub last_sync_ok:         Option<bool>,
    pub daemon_pid:           u32,
}

// ── Direction enums ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncDir { Push, Pull }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchDir { Push, Pull, Both }

impl std::fmt::Display for WatchDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { Self::Push => write!(f, "push"), Self::Pull => write!(f, "pull"), Self::Both => write!(f, "both") }
    }
}
