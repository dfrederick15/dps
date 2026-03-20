use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

// ── Top-level ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub proxmox:    ProxmoxConfig,
    pub container:  ContainerConfig,
    pub sync:       SyncConfig,
    /// Optional Tailscale integration — omit the section to disable.
    pub tailscale:  Option<TailscaleConfig>,

    /// Optional daemon configuration.  Required to run dps-daemon.
    pub daemon:     Option<DaemonConfig>,
}

// ── Proxmox ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxmoxConfig {
    pub host:       String,
    #[serde(default = "default_proxmox_port")]
    pub port:       u16,
    pub node:       String,
    pub auth:       AuthConfig,
    #[serde(default = "default_true")]
    pub verify_ssl: bool,
}

impl ProxmoxConfig {
    pub fn base_url(&self) -> String {
        format!("https://{}:{}/api2/json", self.host, self.port)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuthConfig {
    Token {
        user:        String,
        token_name:  String,
        token_value: String,
    },
    Password {
        user:     String,
        realm:    String,
        password: String,
    },
}

// ── Container ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContainerConfig {
    pub vmid:          u32,
    pub hostname:      String,
    pub template:      String,
    pub storage:       String,
    pub disk_size:     String,
    #[serde(default = "default_memory")]
    pub memory:        u32,
    #[serde(default = "default_cores")]
    pub cores:         u32,
    pub root_password: String,
    pub network:       NetworkConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkConfig {
    #[serde(default = "default_bridge")]
    pub bridge:  String,
    #[serde(default = "default_dhcp")]
    pub ip:      String,
    pub gateway: Option<String>,
}

impl NetworkConfig {
    /// Build the Proxmox `net0` parameter string.
    pub fn net0_param(&self) -> String {
        let mut s = format!("name=eth0,bridge={},ip={}", self.bridge, self.ip);
        if let Some(gw) = &self.gateway {
            s.push_str(&format!(",gw={}", gw));
        }
        s
    }
}

// ── Sync ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SyncConfig {
    /// User/login on the sync container (usually "root").
    #[serde(default = "default_container_user")]
    pub container_user: String,

    /// SSH port the container listens on.
    #[serde(default = "default_ssh_port")]
    pub ssh_port: u16,

    /// Override the detected container IP (e.g. when behind NAT).
    pub container_ip: Option<String>,

    /// SSH public key file to inject into the container.
    pub ssh_public_key_file: Option<String>,

    /// SSH private key/identity file used for subsequent connections.
    pub ssh_identity_file: Option<String>,

    /// Paths to sync between the local machine and the container.
    pub paths: Vec<SyncPath>,

    /// rsync `--exclude` patterns applied to every transfer.
    #[serde(default)]
    pub exclude_patterns: Vec<String>,

    /// Extra machine-specific patterns appended to the built-in list.
    /// These are combined with MACHINE_SPECIFIC_PATTERNS in ignore.rs.
    #[serde(default)]
    pub machine_specific_patterns: Vec<String>,
}

// ── Daemon ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonConfig {
    /// Start the container automatically if it is stopped when the daemon starts.
    #[serde(default = "default_true")]
    pub auto_start_container: bool,

    /// Begin watching immediately on daemon start.
    #[serde(default = "default_true")]
    pub auto_watch: bool,

    /// Watch direction used when auto_watch is true: "push", "pull", or "both".
    #[serde(default = "default_both")]
    pub watch_direction: String,

    /// Debounce window in ms for the push watcher.
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,

    /// Pull poll interval in seconds.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,

    /// Username whose profile is synced.  Defaults to $USER at runtime.
    pub sync_user: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SyncPath {
    /// Local path (tilde-expanded at runtime).
    pub local: String,
    /// Path relative to `/profiles/<username>/` on the container.
    pub remote: String,
    /// Pass `--delete` to rsync for this path.
    #[serde(default)]
    pub delete: bool,
}

// ── Tailscale ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TailscaleConfig {
    /// Tailscale auth key (reusable or one-time).  Required for `dps setup`.
    /// Generate one at https://login.tailscale.com/admin/settings/keys
    pub auth_key: String,

    /// Override the MagicDNS hostname assigned to the container.
    /// Defaults to the container hostname if omitted.
    pub hostname: Option<String>,

    /// Optional ACL tags applied when joining the tailnet (e.g. ["tag:sync"]).
    #[serde(default)]
    pub tags: Vec<String>,

    /// When true, `dps push/pull` prefers the Tailscale IP over the LAN IP.
    /// Defaults to true when this section is present.
    #[serde(default = "default_true")]
    pub prefer_tailscale_ip: bool,

    /// Cached Tailscale IP — written by `dps setup`, read by push/pull.
    pub container_ts_ip: Option<String>,
}

// ── Loading / saving ──────────────────────────────────────────────────────────

/// The bundled example config, compiled into the binary.
pub const EXAMPLE_CONFIG: &str = include_str!("../config.example.toml");

impl Config {
    /// Load config from an explicit path or the default location.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config_path = match path {
            Some(p) => p.to_path_buf(),
            None => default_config_path()
                .ok_or_else(|| Error::Config("cannot determine home directory".into()))?,
        };

        let contents = std::fs::read_to_string(&config_path).map_err(|e| {
            Error::Config(format!(
                "cannot read config file {}: {}",
                config_path.display(),
                e
            ))
        })?;

        toml::from_str(&contents)
            .map_err(|e| Error::Config(format!("parse error in {}: {}", config_path.display(), e)))
    }

    /// Write the bundled example config to the default (or given) path.
    /// Returns the path it was written to.
    pub fn write_example(path: Option<&Path>) -> Result<PathBuf> {
        let config_path = match path {
            Some(p) => p.to_path_buf(),
            None => default_config_path()
                .ok_or_else(|| Error::Config("cannot determine home directory".into()))?,
        };
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&config_path, EXAMPLE_CONFIG)?;
        Ok(config_path)
    }

    /// Returns true if the config file already exists at the given (or default) path.
    pub fn exists(path: Option<&Path>) -> bool {
        match path {
            Some(p) => p.exists(),
            None    => default_config_path().map(|p| p.exists()).unwrap_or(false),
        }
    }

    /// Save the config (used to persist the detected container IP, etc.).
    /// Returns the path it was written to.
    pub fn save(&self, path: Option<&Path>) -> Result<PathBuf> {
        let config_path = match path {
            Some(p) => p.to_path_buf(),
            None => default_config_path()
                .ok_or_else(|| Error::Config("cannot determine home directory".into()))?,
        };

        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let contents = toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("serialization error: {e}")))?;

        std::fs::write(&config_path, &contents)?;
        Ok(config_path)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("dps").join("config.toml"))
}

fn default_both()         -> String { "both".into() }
fn default_debounce_ms()  -> u64   { 500 }
fn default_poll_secs()    -> u64   { 30 }
fn default_proxmox_port() -> u16 { 8006 }
fn default_memory()       -> u32  { 512 }
fn default_cores()        -> u32  { 1 }
fn default_bridge()       -> String { "vmbr0".into() }
fn default_dhcp()         -> String { "dhcp".into() }
fn default_container_user() -> String { "root".into() }
fn default_ssh_port()     -> u16  { 22 }
fn default_true()         -> bool { true }
