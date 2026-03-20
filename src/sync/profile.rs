//! Profile synchronisation between the local machine and the dps LXC container.
//!
//! **Backends** (auto-detected):
//! 1. rsync — preferred; incremental, `--checksum`, `--delete`, `--exclude`.
//! 2. scp   — fallback when rsync is absent.
//!
//! **Ignore rules** — applied on every transfer (see `ignore.rs`):
//! * Built-in machine-specific patterns.
//! * `~/.config/dps/syncignore` (global).
//! * `.syncignore` inside each synced directory (per-tree override).
//!
//! **Machine-specific content warnings** — before a push, text files are
//! scanned for the local hostname / machine-id.  Matches are logged as
//! warnings; the file is still transferred unless the user adds it to
//! `.syncignore`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::{SyncConfig, SyncPath};
use crate::error::{Error, Result};
use super::ignore::{find_machine_specific_files, IgnoreRules};

// ── Direction ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Local → container.
    Push,
    /// Container → local.
    Pull,
}

// ── Syncer ────────────────────────────────────────────────────────────────────

/// Owns everything needed to perform a sync operation.
/// Uses an owned `SyncConfig` so it can be freely cloned and moved into
/// `tokio::spawn` tasks without lifetime constraints.
#[derive(Clone)]
pub struct Syncer {
    cfg:          SyncConfig,
    container_ip: String,
    username:     String,
}

impl Syncer {
    pub fn new(cfg: SyncConfig, container_ip: String, username: String) -> Self {
        Self { cfg, container_ip, username }
    }

    /// Run push or pull for every configured path.
    pub async fn run(&self, dir: Direction) -> Result<()> {
        let backend = detect_backend().await;
        info!("sync backend: {backend:?}");

        let mut errors = Vec::new();
        for path in &self.cfg.paths {
            if let Err(e) = self.sync_one(path, dir).await {
                errors.push(format!("{}: {e}", path.local));
            }
        }

        if errors.is_empty() { Ok(()) } else { Err(Error::SyncCommand(-1)) }
    }

    /// Sync a single `SyncPath`.  Used by the real-time watcher and daemon.
    pub async fn sync_one(&self, sp: &SyncPath, dir: Direction) -> Result<()> {
        let backend = detect_backend().await;
        let local   = expand_tilde(&sp.local);

        if dir == Direction::Push && !local.exists() {
            warn!("skipping push of '{}': path does not exist locally", sp.local);
            return Ok(());
        }

        // Warn about machine-specific content in directories being pushed.
        if dir == Direction::Push && local.is_dir() {
            for f in find_machine_specific_files(&local) {
                warn!(
                    "machine-specific content detected in '{}' — \
                     consider adding it to .syncignore",
                    f.display()
                );
            }
        }

        match backend {
            Backend::Rsync => self.rsync(dir, sp, &local).await,
            Backend::Scp   => self.scp(dir, sp, &local).await,
        }
    }

    /// Dry-run: show what rsync *would* transfer without touching any files.
    pub async fn dry_run(&self, sp: &SyncPath, dir: Direction) -> Result<String> {
        let local = expand_tilde(&sp.local);
        let remote_path = self.remote_path(&sp.remote);
        let ssh_opt = self.ssh_opt();

        let ignore = self.ignore_rules_for(&local);

        let (src, dst) = match dir {
            Direction::Push => (
                local_to_rsync_src(&local),
                format!("{}@{}:{}", self.cfg.container_user, self.container_ip, remote_path),
            ),
            Direction::Pull => (
                format!("{}@{}:{}/", self.cfg.container_user, self.container_ip, remote_path),
                local.display().to_string() + "/",
            ),
        };

        let mut args = vec![
            "-avz".to_string(),
            "--checksum".to_string(),
            "--dry-run".to_string(),
            "-e".to_string(), ssh_opt,
        ];
        args.extend(ignore.to_rsync_args());
        for p in &self.cfg.exclude_patterns { args.push(format!("--exclude={p}")); }
        args.push(src);
        args.push(dst);

        run_command_output("rsync", &args).await
    }

    /// Expose configured paths for the watcher and daemon.
    pub fn sync_paths(&self) -> &[SyncPath] {
        &self.cfg.paths
    }

    // ── rsync backend ─────────────────────────────────────────────────────────

    async fn rsync(&self, dir: Direction, sp: &SyncPath, local: &Path) -> Result<()> {
        let remote_path = self.remote_path(&sp.remote);
        let ssh_opt     = self.ssh_opt();
        let ignore      = self.ignore_rules_for(local);

        let (src, dst) = match dir {
            Direction::Push => (
                local_to_rsync_src(local),
                format!("{}@{}:{}", self.cfg.container_user, self.container_ip, remote_path),
            ),
            Direction::Pull => (
                format!("{}@{}:{}/", self.cfg.container_user, self.container_ip, remote_path),
                local.display().to_string() + "/",
            ),
        };

        let mut args = vec![
            "-avz".to_string(),
            "--checksum".to_string(), // compare by content, not just mtime/size
            "--progress".to_string(),
            "-e".to_string(), ssh_opt,
        ];

        if sp.delete { args.push("--delete".to_string()); }

        // Ignore rules: built-in + global syncignore + per-dir .syncignore
        args.extend(ignore.to_rsync_args());
        // Explicit patterns from config
        for p in &self.cfg.exclude_patterns { args.push(format!("--exclude={p}")); }

        args.push(src);
        args.push(dst.clone());

        info!("rsync → {dst}");
        debug!("rsync {}", args.join(" "));

        if dir == Direction::Push {
            self.ssh_mkdir(&remote_path).await?;
        } else if let Some(parent) = local.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        run_command("rsync", &args).await
    }

    // ── scp fallback backend ──────────────────────────────────────────────────

    async fn scp(&self, dir: Direction, sp: &SyncPath, local: &Path) -> Result<()> {
        let remote_path = self.remote_path(&sp.remote);

        match dir {
            Direction::Push => {
                self.ssh_mkdir(&remote_path).await?;

                let mut args = self.scp_base_args();
                if local.is_dir() { args.push("-r".to_string()); }
                args.push(local.display().to_string());
                args.push(format!("{}@{}:{}", self.cfg.container_user, self.container_ip, remote_path));

                info!("scp push → {remote_path}");
                run_command("scp", &args).await
            }
            Direction::Pull => {
                if let Some(parent) = local.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                let mut args = self.scp_base_args();
                args.push("-r".to_string());
                args.push(format!("{}@{}:{}", self.cfg.container_user, self.container_ip, remote_path));
                args.push(local.display().to_string());

                info!("scp pull ← {remote_path}");
                run_command("scp", &args).await
            }
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn remote_path(&self, remote: &str) -> String {
        format!("/profiles/{}/{}", self.username, remote.trim_start_matches('/'))
    }

    fn ssh_opt(&self) -> String {
        let mut s = format!("ssh -p {}", self.cfg.ssh_port);
        if let Some(id) = &self.cfg.ssh_identity_file {
            s.push_str(&format!(" -i {}", expand_tilde_str(id)));
        }
        s.push_str(" -o StrictHostKeyChecking=no -o BatchMode=yes");
        s
    }

    fn scp_base_args(&self) -> Vec<String> {
        let mut args = vec![
            "-P".to_string(), self.cfg.ssh_port.to_string(),
            "-o".to_string(), "StrictHostKeyChecking=no".to_string(),
            "-o".to_string(), "BatchMode=yes".to_string(),
        ];
        if let Some(id) = &self.cfg.ssh_identity_file {
            args.push("-i".to_string());
            args.push(expand_tilde_str(id));
        }
        args
    }

    fn ssh_base_args(&self) -> Vec<String> {
        let mut args = vec![
            "-p".to_string(), self.cfg.ssh_port.to_string(),
            "-o".to_string(), "StrictHostKeyChecking=no".to_string(),
            "-o".to_string(), "BatchMode=yes".to_string(),
        ];
        if let Some(id) = &self.cfg.ssh_identity_file {
            args.push("-i".to_string());
            args.push(expand_tilde_str(id));
        }
        args
    }

    async fn ssh_mkdir(&self, path: &str) -> Result<()> {
        let mut args = self.ssh_base_args();
        args.push(format!("{}@{}", self.cfg.container_user, self.container_ip));
        args.push(format!("mkdir -p '{path}'"));
        run_command("ssh", &args).await
    }

    /// Build ignore rules for a given local path.
    fn ignore_rules_for(&self, local: &Path) -> IgnoreRules {
        let cfg_dir = crate::config::default_config_path()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));

        IgnoreRules::load(
            local,
            &self.cfg.machine_specific_patterns,
            cfg_dir.as_deref(),
        )
    }
}

// ── Backend detection ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum Backend { Rsync, Scp }

async fn detect_backend() -> Backend {
    let ok = Command::new("which")
        .arg("rsync")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if ok { Backend::Rsync } else { Backend::Scp }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

pub async fn run_command(prog: &str, args: &[String]) -> Result<()> {
    let status = Command::new(prog)
        .args(args)
        .status()
        .await
        .map_err(|e| Error::Ssh(format!("failed to launch '{prog}': {e}")))?;

    if status.success() { Ok(()) } else { Err(Error::SyncCommand(status.code().unwrap_or(-1))) }
}

pub async fn run_command_output(prog: &str, args: &[String]) -> Result<String> {
    let out = Command::new(prog)
        .args(args)
        .output()
        .await
        .map_err(|e| Error::Ssh(format!("failed to launch '{prog}': {e}")))?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(Error::SyncCommand(out.status.code().unwrap_or(-1)))
    }
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

fn expand_tilde_str(p: &str) -> String {
    expand_tilde(p).display().to_string()
}

fn local_to_rsync_src(p: &Path) -> String {
    let s = p.display().to_string();
    if p.is_dir() && !s.ends_with('/') { format!("{s}/") } else { s }
}
