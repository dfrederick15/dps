use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name    = "dps",
    about   = "Debian Profile Sync — manage a Proxmox LXC container that syncs your user profile across machines",
    version,
)]
pub struct Cli {
    /// Path to config file (default: ~/.config/dps/config.toml).
    #[arg(long, short, env = "DPS_CONFIG", global = true)]
    pub config: Option<PathBuf>,

    /// Enable verbose/debug output.
    #[arg(long, short, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Create and bootstrap the sync LXC container on Proxmox.
    Setup {
        /// Skip bootstrapping (don't SSH in and install packages).
        #[arg(long)]
        no_bootstrap: bool,

        /// Save the detected container IP back into the config file.
        #[arg(long, default_value_t = true)]
        save_ip: bool,
    },

    /// Push the local user profile to the sync container.
    Push {
        /// Local username whose profile to push (default: current user).
        #[arg(long, short)]
        user: Option<String>,
        /// Show what would be transferred without touching any files.
        #[arg(long)]
        dry_run: bool,
    },

    /// Pull the user profile from the sync container to this machine.
    Pull {
        /// Local username whose profile to pull (default: current user).
        #[arg(long, short)]
        user: Option<String>,
        /// Show what would be transferred without touching any files.
        #[arg(long)]
        dry_run: bool,
    },

    /// Scan for conflicts and machine-specific content before syncing.
    ///
    /// Shows which files differ between local and container, and flags any
    /// file that contains the local hostname or machine-id.
    Check {
        /// Local username to check (default: current user).
        #[arg(long, short)]
        user: Option<String>,
    },

    /// Watch for file-system changes and sync in real time.
    ///
    /// Push mode:  inotify watches local paths and rsyncs on every change.
    /// Pull mode:  periodic rsync poll from the container (efficient — nothing
    ///             transferred when nothing changed).
    /// Both:       push watcher + pull poller run concurrently (default).
    Watch {
        /// Local username whose profile to sync (default: current user).
        #[arg(long, short)]
        user: Option<String>,

        /// Sync direction: push, pull, or both.
        #[arg(long, default_value = "both")]
        direction: String,

        /// Trailing-edge debounce window in milliseconds for the push watcher.
        #[arg(long, default_value_t = 500)]
        debounce_ms: u64,

        /// How often the pull poller runs, in seconds.
        #[arg(long, default_value_t = 30)]
        poll_secs: u64,
    },

    /// Show the status of the sync container and config summary.
    Status,

    /// Start the sync container (if it is stopped).
    Start,

    /// Stop the sync container.
    Stop,

    /// Destroy the sync container (IRREVERSIBLE — all stored profiles are lost).
    Destroy {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },

    /// Manage Tailscale on the sync container.
    Tailscale {
        #[command(subcommand)]
        action: TailscaleAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum TailscaleAction {
    /// Show Tailscale status on the container.
    Status,
    /// Re-authenticate Tailscale (use when the auth key has expired).
    Reauth {
        /// Auth key to use (overrides config).
        #[arg(long)]
        key: Option<String>,
    },
}
