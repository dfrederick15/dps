use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("proxmox API error (HTTP {status}): {message}")]
    ProxmoxApi { status: u16, message: String },

    #[error("proxmox task failed: {0}")]
    TaskFailed(String),

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sync failed: command exited with status {0}")]
    SyncCommand(i32),

    #[error("container {0} not found")]
    ContainerNotFound(u32),

    #[error("container {0} already exists — use `dps destroy` first or choose a different vmid")]
    ContainerExists(u32),

    #[error("timed out waiting for {0}")]
    Timeout(String),

    #[error("could not determine container IP — check Proxmox interfaces or set sync.container_ip in config")]
    NoContainerIp,

    #[error("SSH command failed: {0}")]
    Ssh(String),
}

pub type Result<T> = std::result::Result<T, Error>;
