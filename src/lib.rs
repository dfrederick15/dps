pub mod config;
pub mod error;
pub mod ipc;
pub mod proxmox;
pub mod sync;
pub mod wizard;

/// GUI application — only compiled with `--features gui`.
#[cfg(feature = "gui")]
pub mod gui;
