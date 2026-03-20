mod cli;  // CLI-specific; not part of the library

use std::path::Path;

use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use cli::{Cli, Commands, TailscaleAction};
use dps::config::Config;
use dps::error::{Error, Result};
use dps::proxmox::{lxc, ProxmoxClient};
use dps::sync::{
    profile::{Direction, Syncer},
    setup,
    watcher::{self, WatchArgs, WatchDirection},
};

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let filter = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter)),
        )
        .with_target(false)
        .init();

    if let Err(e) = run(cli).await {
        error!("{e}");
        std::process::exit(1);
    }
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

async fn run(cli: Cli) -> Result<()> {
    let config_path = cli.config.as_deref();

    match cli.command {
        Commands::Setup { no_bootstrap, save_ip } => {
            cmd_setup(config_path, no_bootstrap, save_ip).await
        }
        Commands::Push { user, dry_run } => {
            if dry_run { cmd_dry_run(config_path, Direction::Push, user).await }
            else       { cmd_sync(config_path, Direction::Push, user).await }
        }
        Commands::Pull { user, dry_run } => {
            if dry_run { cmd_dry_run(config_path, Direction::Pull, user).await }
            else       { cmd_sync(config_path, Direction::Pull, user).await }
        }
        Commands::Check { user } => cmd_check(config_path, user).await,
        Commands::Status => cmd_status(config_path).await,
        Commands::Start  => cmd_start(config_path).await,
        Commands::Stop   => cmd_stop(config_path).await,
        Commands::Destroy { yes } => cmd_destroy(config_path, yes).await,
        Commands::Watch { user, direction, debounce_ms, poll_secs } => {
            cmd_watch(config_path, user, &direction, debounce_ms, poll_secs).await
        }
        Commands::Tailscale { action } => cmd_tailscale(config_path, action).await,
    }
}

// ── setup ─────────────────────────────────────────────────────────────────────

async fn cmd_setup(
    config_path:  Option<&Path>,
    no_bootstrap: bool,
    save_ip:      bool,
) -> Result<()> {
    let mut cfg = if !Config::exists(config_path) {
        let cfg = dps::wizard::run()?;
        let written = cfg.save(config_path)?;
        println!("Config saved to {}\n", written.display());
        cfg
    } else {
        Config::load(config_path)?
    };
    let client  = build_client(&cfg).await?;

    let node = cfg.proxmox.node.clone();
    let vmid = cfg.container.vmid;

    if lxc::exists(&client, &node, vmid).await? {
        return Err(Error::ContainerExists(vmid));
    }

    let pub_key = setup::resolve_public_key(&cfg.sync)
        .map_err(|e| {
            warn!("could not resolve SSH public key: {e} — container created without key injection");
            e
        })
        .ok();

    // ── Create container ──────────────────────────────────────────────────────
    info!("creating LXC container {vmid} ({}) on node '{node}'…", cfg.container.hostname);
    let upid = lxc::create(&client, &node, &cfg.container, pub_key.as_deref()).await?;
    info!("create task: {upid}");
    client.wait_for_task(&node, &upid).await?;
    info!("container created");

    // ── Start ─────────────────────────────────────────────────────────────────
    info!("starting container {vmid}…");
    let upid = lxc::start(&client, &node, vmid).await?;
    client.wait_for_task(&node, &upid).await?;
    lxc::wait_until_running(&client, &node, vmid).await?;

    // ── Discover LAN IP ───────────────────────────────────────────────────────
    let lan_ip = if let Some(ip) = &cfg.sync.container_ip {
        info!("using static container IP from config: {ip}");
        ip.clone()
    } else {
        let detected = lxc::wait_for_ip(&client, &node, vmid).await?;
        if save_ip {
            info!("saving detected LAN IP {detected} to config");
            cfg.sync.container_ip = Some(detected.clone());
            cfg.save(config_path)?;
        }
        detected
    };

    // ── Bootstrap ─────────────────────────────────────────────────────────────
    if !no_bootstrap {
        let pub_key_str = pub_key.ok_or_else(|| {
            Error::Config("SSH public key is required for bootstrap".into())
        })?;

        setup::wait_for_ssh(&lan_ip, cfg.sync.ssh_port, 120).await?;

        setup::bootstrap(
            &lan_ip,
            &cfg.container.root_password,
            &pub_key_str,
            &cfg.sync,
            cfg.tailscale.as_ref(),
        )
        .await?;

        // ── Discover and cache Tailscale IP ───────────────────────────────────
        if let Some(ts) = cfg.tailscale.as_mut() {
            if ts.prefer_tailscale_ip {
                match setup::get_tailscale_ip(&lan_ip, &cfg.sync).await {
                    Ok(Some(ts_ip)) => {
                        info!("Tailscale IP: {ts_ip}");
                        ts.container_ts_ip = Some(ts_ip);
                        cfg.save(config_path)?;
                    }
                    Ok(None) => warn!("Tailscale installed but no IP yet — run `dps tailscale status` to verify"),
                    Err(e)   => warn!("could not query Tailscale IP: {e}"),
                }
            }
        }
    }

    let effective_ip = effective_ip(&cfg).unwrap_or(lan_ip);
    info!("✓ sync container is ready at {effective_ip}");
    info!("  push your profile:   dps push");
    info!("  pull your profile:   dps pull");
    Ok(())
}

// ── push / pull ───────────────────────────────────────────────────────────────

async fn cmd_sync(
    config_path: Option<&Path>,
    direction:   Direction,
    user:        Option<String>,
) -> Result<()> {
    let cfg    = Config::load(config_path)?;
    let client = build_client(&cfg).await?;

    let node = cfg.proxmox.node.clone();
    let vmid = cfg.container.vmid;

    match lxc::status(&client, &node, vmid).await? {
        lxc::ContainerStatus::Running => {}
        lxc::ContainerStatus::Stopped => {
            return Err(Error::Config(format!(
                "container {vmid} is stopped — run `dps start` first"
            )));
        }
        lxc::ContainerStatus::Unknown(s) => {
            warn!("unexpected container status '{s}', proceeding anyway");
        }
    }

    let ip = resolve_sync_ip(&cfg, &client, &node, vmid).await?;

    let username = resolve_username(user)?;

    let dir_label = match direction {
        Direction::Push => "push",
        Direction::Pull => "pull",
    };
    info!("{dir_label} profile for '{username}' via {ip}");

    let syncer = Syncer::new(cfg.sync.clone(), ip, username);
    syncer.run(direction).await?;

    info!("sync complete");
    Ok(())
}

// ── status ────────────────────────────────────────────────────────────────────

async fn cmd_status(config_path: Option<&Path>) -> Result<()> {
    let cfg    = Config::load(config_path)?;
    let client = build_client(&cfg).await?;

    let node = &cfg.proxmox.node;
    let vmid = cfg.container.vmid;

    println!("Proxmox host : {}:{}", cfg.proxmox.host, cfg.proxmox.port);
    println!("Node         : {node}");
    println!("Container ID : {vmid}");
    println!("Hostname     : {}", cfg.container.hostname);

    match lxc::status(&client, node, vmid).await {
        Ok(s) => {
            let label = match &s {
                lxc::ContainerStatus::Running    => "running ✓",
                lxc::ContainerStatus::Stopped    => "stopped",
                lxc::ContainerStatus::Unknown(_) => "unknown",
            };
            println!("Status       : {label}");

            if s == lxc::ContainerStatus::Running {
                match lxc::get_ip(&client, node, vmid).await {
                    Ok(Some(ip)) => println!("LAN IP       : {ip}"),
                    Ok(None)     => println!("LAN IP       : (not yet assigned)"),
                    Err(e)       => println!("LAN IP       : error ({e})"),
                }
            }
        }
        Err(Error::ContainerNotFound(_)) => {
            println!("Status       : not found — run `dps setup` to create it");
        }
        Err(e) => return Err(e),
    }

    if let Some(ts) = &cfg.tailscale {
        let ts_ip = ts.container_ts_ip.as_deref().unwrap_or("(not yet detected)");
        println!("Tailscale IP : {ts_ip}");
        println!("TS preferred : {}", ts.prefer_tailscale_ip);
    } else {
        println!("Tailscale    : disabled (add [tailscale] section to config to enable)");
    }

    println!("\nSync paths:");
    for p in &cfg.sync.paths {
        println!("  {} → /profiles/<user>/{}", p.local, p.remote);
    }

    Ok(())
}

// ── start ─────────────────────────────────────────────────────────────────────

async fn cmd_start(config_path: Option<&Path>) -> Result<()> {
    let cfg    = Config::load(config_path)?;
    let client = build_client(&cfg).await?;
    let node   = cfg.proxmox.node.clone();
    let vmid   = cfg.container.vmid;

    info!("starting container {vmid}…");
    let upid = lxc::start(&client, &node, vmid).await?;
    client.wait_for_task(&node, &upid).await?;
    lxc::wait_until_running(&client, &node, vmid).await?;
    info!("container {vmid} is running");
    Ok(())
}

// ── stop ──────────────────────────────────────────────────────────────────────

async fn cmd_stop(config_path: Option<&Path>) -> Result<()> {
    let cfg    = Config::load(config_path)?;
    let client = build_client(&cfg).await?;
    let node   = cfg.proxmox.node.clone();
    let vmid   = cfg.container.vmid;

    info!("stopping container {vmid}…");
    let upid = lxc::stop(&client, &node, vmid).await?;
    client.wait_for_task(&node, &upid).await?;
    info!("container {vmid} stopped");
    Ok(())
}

// ── destroy ───────────────────────────────────────────────────────────────────

async fn cmd_destroy(config_path: Option<&Path>, yes: bool) -> Result<()> {
    let cfg = Config::load(config_path)?;

    if !yes {
        print!(
            "This will permanently destroy container {} and all stored profiles. \
             Type 'yes' to confirm: ",
            cfg.container.vmid
        );
        use std::io::BufRead;
        let line = std::io::stdin()
            .lock()
            .lines()
            .next()
            .and_then(|l| l.ok())
            .unwrap_or_default();
        if line.trim() != "yes" {
            info!("aborted");
            return Ok(());
        }
    }

    let client = build_client(&cfg).await?;
    let node   = cfg.proxmox.node.clone();
    let vmid   = cfg.container.vmid;

    if let Ok(lxc::ContainerStatus::Running) = lxc::status(&client, &node, vmid).await {
        info!("stopping container {vmid} before deletion…");
        let upid = lxc::stop(&client, &node, vmid).await?;
        client.wait_for_task(&node, &upid).await?;
    }

    info!("destroying container {vmid}…");
    let upid = lxc::destroy(&client, &node, vmid).await?;
    client.wait_for_task(&node, &upid).await?;
    info!("container {vmid} destroyed");
    Ok(())
}

// ── dry-run ───────────────────────────────────────────────────────────────────

async fn cmd_dry_run(
    config_path: Option<&Path>,
    direction:   Direction,
    user:        Option<String>,
) -> Result<()> {
    let cfg    = Config::load(config_path)?;
    let client = build_client(&cfg).await?;
    let ip     = resolve_sync_ip(&cfg, &client, &cfg.proxmox.node.clone(), cfg.container.vmid).await?;
    let username = resolve_username(user)?;
    let syncer   = Syncer::new(cfg.sync.clone(), ip, username);

    println!("Dry run ({direction:?}) — files that would be transferred:\n");
    for sp in syncer.sync_paths() {
        let out = syncer.dry_run(sp, direction).await?;
        if out.trim().is_empty() {
            println!("  {} — no changes", sp.local);
        } else {
            println!("  {}:", sp.local);
            for line in out.lines() { println!("    {line}"); }
        }
    }
    Ok(())
}

// ── check ─────────────────────────────────────────────────────────────────────

async fn cmd_check(config_path: Option<&Path>, user: Option<String>) -> Result<()> {
    use dps::sync::ignore::{find_machine_specific_files, machine_fingerprint, MACHINE_SPECIFIC_PATTERNS};
    use dps::sync::profile::expand_tilde;

    let cfg      = Config::load(config_path)?;
    let client   = build_client(&cfg).await?;
    let ip       = resolve_sync_ip(&cfg, &client, &cfg.proxmox.node.clone(), cfg.container.vmid).await?;
    let username = resolve_username(user)?;
    let syncer   = Syncer::new(cfg.sync.clone(), ip, username);

    let (hostname, machine_id) = machine_fingerprint();
    println!("Machine fingerprint:");
    println!("  hostname:   {hostname}");
    println!("  machine-id: {machine_id}");
    println!();

    println!("Built-in excluded patterns ({}):", MACHINE_SPECIFIC_PATTERNS.len());
    for p in MACHINE_SPECIFIC_PATTERNS { println!("  {p}"); }
    println!();

    let mut any_flagged = false;
    for sp in syncer.sync_paths() {
        let local = expand_tilde(&sp.local);
        if !local.exists() { continue; }

        let flagged = find_machine_specific_files(&local);
        if !flagged.is_empty() {
            any_flagged = true;
            println!("⚠  Machine-specific content in '{}':", sp.local);
            for f in &flagged {
                let rel = f.strip_prefix(&local).unwrap_or(f);
                println!("     {}", rel.display());
            }
            println!("   → Add these to {}/.syncignore to suppress this warning.", sp.local);
            println!();
        }
    }

    if !any_flagged {
        println!("✓ No machine-specific content detected in synced paths.");
    }

    println!("Diff (what push would transfer):");
    for sp in syncer.sync_paths() {
        let out = syncer.dry_run(sp, Direction::Push).await?;
        if out.trim().is_empty() {
            println!("  {} — in sync", sp.local);
        } else {
            println!("  {} — changes:", sp.local);
            for line in out.lines() { println!("    {line}"); }
        }
    }
    Ok(())
}

fn resolve_username(user: Option<String>) -> Result<String> {
    user.or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("LOGNAME").ok())
        .ok_or_else(|| Error::Config("cannot determine local username — use --user".into()))
}

// ── watch ─────────────────────────────────────────────────────────────────────

async fn cmd_watch(
    config_path: Option<&Path>,
    user:        Option<String>,
    direction:   &str,
    debounce_ms: u64,
    poll_secs:   u64,
) -> Result<()> {
    let cfg    = Config::load(config_path)?;
    let client = build_client(&cfg).await?;

    let node = cfg.proxmox.node.clone();
    let vmid = cfg.container.vmid;

    match lxc::status(&client, &node, vmid).await? {
        lxc::ContainerStatus::Running => {}
        lxc::ContainerStatus::Stopped => {
            return Err(Error::Config(format!(
                "container {vmid} is stopped — run `dps start` first"
            )));
        }
        lxc::ContainerStatus::Unknown(s) => {
            warn!("unexpected container status '{s}', proceeding anyway");
        }
    }

    let ip = resolve_sync_ip(&cfg, &client, &node, vmid).await?;

    let username = resolve_username(user)?;

    let watch_dir = match direction {
        "push" => WatchDirection::Push,
        "pull" => WatchDirection::Pull,
        "both" => WatchDirection::Both,
        other  => return Err(Error::Config(format!(
            "unknown direction '{other}' — use push, pull, or both"
        ))),
    };

    info!("starting watch ({direction}) for '{username}' via {ip}");

    let syncer = Syncer::new(cfg.sync.clone(), ip, username);
    watcher::run(syncer, &WatchArgs { direction: watch_dir, debounce_ms, poll_secs }).await
}

// ── tailscale ─────────────────────────────────────────────────────────────────

async fn cmd_tailscale(config_path: Option<&Path>, action: TailscaleAction) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let ts  = cfg.tailscale.as_ref().ok_or_else(|| {
        Error::Config(
            "Tailscale is not configured — add a [tailscale] section to config".into(),
        )
    })?;

    let client = build_client(&cfg).await?;
    let node   = &cfg.proxmox.node;
    let vmid   = cfg.container.vmid;

    let ip = resolve_sync_ip(&cfg, &client, node, vmid).await?;

    match action {
        TailscaleAction::Status => {
            let out = setup::tailscale_status(&ip, &cfg.sync).await?;
            print!("{out}");
        }
        TailscaleAction::Reauth { key } => {
            let auth_key = key.as_deref().unwrap_or(&ts.auth_key);
            setup::tailscale_reauth(&ip, auth_key, &cfg.sync).await?;
            info!("Tailscale re-authenticated");
        }
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn build_client(cfg: &Config) -> Result<ProxmoxClient> {
    let c = ProxmoxClient::new(&cfg.proxmox)?;
    // Fetches a session ticket for password auth; no-op for API token auth.
    c.authenticate().await?;
    Ok(c)
}

/// Return the IP to use for SSH/rsync:
/// - Tailscale IP (if configured and `prefer_tailscale_ip` is true)
/// - explicit `sync.container_ip` override
/// - dynamically queried LAN IP from Proxmox
async fn resolve_sync_ip(
    cfg:    &Config,
    client: &ProxmoxClient,
    node:   &str,
    vmid:   u32,
) -> Result<String> {
    // Prefer Tailscale IP when available and configured.
    if let Some(ts) = &cfg.tailscale {
        if ts.prefer_tailscale_ip {
            if let Some(ts_ip) = &ts.container_ts_ip {
                return Ok(ts_ip.clone());
            }
            // Not cached yet — fall through to LAN IP and warn.
            warn!("Tailscale preferred but no cached TS IP; using LAN IP (run `dps tailscale status` after setup)");
        }
    }

    if let Some(ip) = &cfg.sync.container_ip {
        return Ok(ip.clone());
    }

    lxc::get_ip(client, node, vmid)
        .await?
        .ok_or(Error::NoContainerIp)
}

/// Best-effort: return whichever IP would be used for syncing (no Proxmox call).
fn effective_ip(cfg: &Config) -> Option<String> {
    if let Some(ts) = &cfg.tailscale {
        if ts.prefer_tailscale_ip {
            if let Some(ip) = &ts.container_ts_ip {
                return Some(ip.clone());
            }
        }
    }
    cfg.sync.container_ip.clone()
}
