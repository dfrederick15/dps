//! Interactive setup wizard — collects all required config values from the user
//! when no config file exists.

use std::io::{self, Write};

use crate::config::{
    AuthConfig, Config, ContainerConfig, DaemonConfig, NetworkConfig,
    ProxmoxConfig, SyncConfig, SyncPath, TailscaleConfig,
};
use crate::error::{Error, Result};

// ── Public entry point ────────────────────────────────────────────────────────

/// Walk the user through creating a new config interactively.
/// Returns the completed `Config` (not yet saved to disk).
pub fn run() -> Result<Config> {
    println!("\n╔══════════════════════════════════════╗");
    println!("║        dps — first-time setup        ║");
    println!("╚══════════════════════════════════════╝\n");
    println!("Answer each prompt (press Enter to accept the [default]).\n");

    // ── Proxmox ───────────────────────────────────────────────────────────────
    section("Proxmox Connection");
    let host       = required("  Host (IP or hostname)")?;
    let port: u16  = parsed("  Port", "8006")?;
    let node       = required_default("  Node name", "pve")?;
    let verify_ssl = yes_no("  Verify SSL certificate", false)?;

    // ── Auth ──────────────────────────────────────────────────────────────────
    section("Authentication");
    let auth_type = choice("  Type", &["token", "password"], "token")?;
    let auth = if auth_type == "token" {
        let user        = required_default("  User", "root@pam")?;
        let token_name  = required_default("  Token name", "dps")?;
        let token_value = password("  Token value")?;
        AuthConfig::Token { user, token_name, token_value }
    } else {
        let user     = required_default("  User", "root")?;
        let realm    = required_default("  Realm", "pam")?;
        let password = password("  Password")?;
        AuthConfig::Password { user, realm, password }
    };

    // ── Container ─────────────────────────────────────────────────────────────
    section("LXC Container");
    let vmid: u32    = parsed("  VMID", "200")?;
    let hostname     = required_default("  Hostname", "dps-sync")?;
    let template     = required_default(
        "  Template",
        "local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst",
    )?;
    let storage      = required_default("  Storage pool", "local-lvm")?;
    let disk_size    = required_default("  Disk size", "8G")?;
    let memory: u32  = parsed("  Memory (MB)", "512")?;
    let cores: u32   = parsed("  Cores", "1")?;
    let root_password = password("  Root password (used only during bootstrap)")?;

    let bridge       = required_default("  Network bridge", "vmbr0")?;
    let ip           = required_default("  IP  (\"dhcp\" or CIDR e.g. 192.168.1.50/24)", "dhcp")?;
    let gateway      = if ip == "dhcp" {
        None
    } else {
        optional("  Gateway (leave blank if not needed)")?
    };

    // ── Sync ──────────────────────────────────────────────────────────────────
    section("Profile Sync");
    let ssh_public_key_file = optional("  SSH public key file  (blank = auto-detect)")?;
    let ssh_identity_file   = optional("  SSH private key file (blank = auto-detect)")?;

    println!("\n  Paths to sync between this machine and the container.");
    println!("  Press Enter with a blank local path when done.\n");

    let mut paths: Vec<SyncPath> = Vec::new();
    loop {
        let local = optional(&format!("  Local path  [{}/{}]",
            paths.len() + 1,
            if paths.is_empty() { "?" } else { "..." }))?;

        let Some(local) = local else { break };

        let remote_default = local
            .trim_start_matches("~/")
            .trim_start_matches('~')
            .trim_start_matches('/')
            .to_string();
        let remote = required_default("  Remote path (relative to /profiles/<user>/)", &remote_default)?;
        let delete = yes_no("  Delete remote files that no longer exist locally", false)?;
        paths.push(SyncPath { local, remote, delete });
        println!();
    }

    if paths.is_empty() {
        paths = default_paths();
        println!("  No paths entered — using defaults:");
        for p in &paths {
            println!("    {} → {}", p.local, p.remote);
        }
    }

    // ── Tailscale ─────────────────────────────────────────────────────────────
    section("Tailscale (optional)");
    println!("  When enabled, dps installs Tailscale in the container during setup");
    println!("  and uses the Tailscale IP for all transfers (works across networks).\n");
    let tailscale = if yes_no("  Enable Tailscale integration", false)? {
        let auth_key = password("  Tailscale auth key (tskey-auth-...)")?;
        let prefer   = yes_no("  Prefer Tailscale IP for push/pull", true)?;
        Some(TailscaleConfig {
            auth_key,
            hostname:            None,
            tags:                vec![],
            prefer_tailscale_ip: prefer,
            container_ts_ip:     None,
        })
    } else {
        None
    };

    // ── Daemon ────────────────────────────────────────────────────────────────
    section("Daemon (optional)");
    let daemon = if yes_no("  Configure the dps-daemon background service", false)? {
        let auto_start = yes_no("  Auto-start container when daemon starts", true)?;
        let auto_watch = yes_no("  Auto-start watcher when daemon starts", true)?;
        let dir = choice("  Watch direction", &["both", "push", "pull"], "both")?;
        Some(DaemonConfig {
            auto_start_container: auto_start,
            auto_watch,
            watch_direction: dir,
            debounce_ms: 500,
            poll_secs: 30,
            sync_user: None,
        })
    } else {
        None
    };

    // ── Done ──────────────────────────────────────────────────────────────────
    println!();
    Ok(Config {
        proxmox: ProxmoxConfig { host, port, node, auth, verify_ssl },
        container: ContainerConfig {
            vmid, hostname, template, storage, disk_size,
            memory, cores, root_password,
            network: NetworkConfig { bridge, ip, gateway },
        },
        sync: SyncConfig {
            container_user:          "root".into(),
            ssh_port:                22,
            container_ip:            None,
            ssh_public_key_file,
            ssh_identity_file,
            paths,
            exclude_patterns: vec![
                ".git/".into(),
                "__pycache__/".into(),
                "*.pyc".into(),
                "node_modules/".into(),
            ],
            machine_specific_patterns: vec![],
        },
        tailscale,
        daemon,
    })
}

// ── Prompt helpers ────────────────────────────────────────────────────────────

fn section(title: &str) {
    println!("── {title} {}", "─".repeat(52usize.saturating_sub(title.len() + 4)));
}

/// Prompt for a required value with no default.
fn required(label: &str) -> Result<String> {
    loop {
        let s = ask(label, None)?;
        if !s.is_empty() { return Ok(s); }
        println!("  (this field is required)");
    }
}

/// Prompt with a non-empty default.
fn required_default(label: &str, default: &str) -> Result<String> {
    let s = ask(label, Some(default))?;
    Ok(if s.is_empty() { default.to_string() } else { s })
}

/// Prompt for an optional value; returns None on blank input.
fn optional(label: &str) -> Result<Option<String>> {
    let s = ask(label, None)?;
    Ok(if s.is_empty() { None } else { Some(s) })
}

/// Prompt for a value and parse it; retries on parse failure.
fn parsed<T: std::str::FromStr>(label: &str, default: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    loop {
        let s = ask(label, Some(default))?;
        let s = if s.is_empty() { default.to_string() } else { s };
        match s.parse::<T>() {
            Ok(v)  => return Ok(v),
            Err(e) => println!("  invalid value: {e}"),
        }
    }
}

/// Yes/no prompt.
fn yes_no(label: &str, default: bool) -> Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    let s = ask(&format!("{label} [{hint}]"), None)?;
    Ok(match s.trim().to_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no"  => false,
        _           => default,
    })
}

/// Multiple-choice prompt; retries on invalid input.
fn choice(label: &str, options: &[&str], default: &str) -> Result<String> {
    let opts = options.join("/");
    loop {
        let s = ask(&format!("{label} ({opts})", ), Some(default))?;
        let s = if s.is_empty() { default.to_string() } else { s };
        if options.contains(&s.as_str()) { return Ok(s); }
        println!("  must be one of: {opts}");
    }
}

/// Password prompt (input is hidden).
fn password(label: &str) -> Result<String> {
    loop {
        let s = rpassword::prompt_password(format!("{label}: "))
            .map_err(|e| Error::Config(format!("failed to read input: {e}")))?;
        if !s.is_empty() { return Ok(s); }
        println!("  (this field is required)");
    }
}

/// Raw ask — prints prompt, reads a line, strips trailing newline.
fn ask(label: &str, default: Option<&str>) -> Result<String> {
    let prompt = match default {
        Some(d) => format!("{label} [{d}]: "),
        None    => format!("{label}: "),
    };
    print!("{prompt}");
    io::stdout().flush().map_err(Error::Io)?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf).map_err(Error::Io)?;
    Ok(buf.trim_end_matches('\n').trim_end_matches('\r').to_string())
}

// ── Defaults ──────────────────────────────────────────────────────────────────

fn default_paths() -> Vec<SyncPath> {
    vec![
        SyncPath { local: "~/.bashrc".into(),       remote: ".bashrc".into(),       delete: false },
        SyncPath { local: "~/.bash_profile".into(), remote: ".bash_profile".into(), delete: false },
        SyncPath { local: "~/.profile".into(),      remote: ".profile".into(),      delete: false },
        SyncPath { local: "~/.gitconfig".into(),    remote: ".gitconfig".into(),    delete: false },
    ]
}
