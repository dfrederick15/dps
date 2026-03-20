//! Initial container provisioning over SSH.
//!
//! Called once after `dps setup` has created and started the container.
//! Connects as root (password or key), installs packages, creates the
//! `/profiles` directory tree, injects the user's SSH public key, and
//! optionally installs + joins Tailscale.

use std::time::Duration;

use tracing::info;

use crate::config::{SyncConfig, TailscaleConfig};
use crate::error::{Error, Result};
use super::profile::run_command;

// ── SSH readiness ─────────────────────────────────────────────────────────────

/// Poll until SSH is accepting connections (or timeout expires).
pub async fn wait_for_ssh(ip: &str, port: u16, timeout_secs: u64) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    info!("waiting for SSH on {ip}:{port}…");
    loop {
        if tokio::net::TcpStream::connect((ip, port)).await.is_ok() {
            info!("SSH reachable at {ip}:{port}");
            // Give sshd a moment to finish initialising after TCP accepts.
            tokio::time::sleep(Duration::from_secs(2)).await;
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::Timeout(format!("SSH on {ip}:{port}")));
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

// ── Bootstrap ─────────────────────────────────────────────────────────────────

/// Bootstrap the container over SSH:
///
/// 1. Update apt, install openssh-server + rsync.
/// 2. Create `/profiles` directory.
/// 3. Inject the SSH public key → `/root/.ssh/authorized_keys`.
/// 4. Enforce key-only SSH (disable password login).
/// 5. Optionally install Tailscale and join the tailnet.
pub async fn bootstrap(
    ip:             &str,
    root_password:  &str,
    ssh_public_key: &str,
    sync_cfg:       &SyncConfig,
    ts_cfg:         Option<&TailscaleConfig>,
) -> Result<()> {
    let ts_block = build_tailscale_block(ts_cfg);

    let pub_key_escaped = ssh_public_key.trim().replace('\'', r"'\''");

    let setup_script = format!(
        r#"set -e
export DEBIAN_FRONTEND=noninteractive

# ── Base packages ──────────────────────────────────────────────────────────────
apt-get update -qq
apt-get install -y -qq openssh-server rsync curl

# ── Profile storage ────────────────────────────────────────────────────────────
mkdir -p /profiles
chmod 700 /profiles

# ── SSH hardening ──────────────────────────────────────────────────────────────
mkdir -p /root/.ssh
chmod 700 /root/.ssh
echo '{pub_key}' >> /root/.ssh/authorized_keys
chmod 600 /root/.ssh/authorized_keys

sed -i 's/^#*PermitRootLogin.*/PermitRootLogin prohibit-password/'   /etc/ssh/sshd_config
sed -i 's/^#*PasswordAuthentication.*/PasswordAuthentication no/'    /etc/ssh/sshd_config
sed -i 's/^#*PubkeyAuthentication.*/PubkeyAuthentication yes/'       /etc/ssh/sshd_config

systemctl enable ssh  2>/dev/null || systemctl enable sshd  2>/dev/null || true
systemctl restart ssh 2>/dev/null || systemctl restart sshd 2>/dev/null || true

{ts_block}

echo "dps bootstrap complete"
"#,
        pub_key  = pub_key_escaped,
        ts_block = ts_block,
    );

    info!("running bootstrap script on container {ip}…");
    run_script_over_ssh(ip, root_password, &setup_script, sync_cfg).await?;
    info!("container bootstrapped successfully");
    Ok(())
}

/// Build the Tailscale install+join shell fragment, or empty string if disabled.
fn build_tailscale_block(ts_cfg: Option<&TailscaleConfig>) -> String {
    let Some(ts) = ts_cfg else {
        return String::new();
    };

    // Build the `tailscale up` flags
    let hostname_flag = ts
        .hostname
        .as_deref()
        .map(|h| format!(" --hostname={h}"))
        .unwrap_or_default();

    let tags_flag = if ts.tags.is_empty() {
        String::new()
    } else {
        format!(" --advertise-tags={}", ts.tags.join(","))
    };

    format!(
        r#"
# ── Tailscale ──────────────────────────────────────────────────────────────────
curl -fsSL https://tailscale.com/install.sh | sh

# Start the daemon
systemctl enable tailscaled
systemctl start  tailscaled

# Join the tailnet
tailscale up \
  --authkey='{auth_key}' \
  --accept-routes \
  --ssh{hostname_flag}{tags_flag}

echo "Tailscale status:"
tailscale status
"#,
        auth_key     = ts.auth_key.replace('\'', r"'\''"),
        hostname_flag = hostname_flag,
        tags_flag     = tags_flag,
    )
}

// ── Tailscale IP query ────────────────────────────────────────────────────────

/// SSH into the container and return the Tailscale IP (100.x.x.x).
pub async fn get_tailscale_ip(
    ip:       &str,
    sync_cfg: &SyncConfig,
) -> Result<Option<String>> {
    let mut args = ssh_base_args(sync_cfg);
    args.push(format!("root@{ip}"));
    args.push("tailscale ip --4 2>/dev/null || true".to_string());

    let output = super::profile::run_command_output("ssh", &args).await?;
    let ts_ip = output.trim().to_string();
    if ts_ip.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ts_ip))
    }
}

/// Re-authenticate Tailscale in the container (e.g. after key expiry).
pub async fn tailscale_reauth(
    ip:       &str,
    auth_key: &str,
    sync_cfg: &SyncConfig,
) -> Result<()> {
    let cmd = format!(
        "tailscale up --authkey='{}' --accept-routes --ssh",
        auth_key.replace('\'', r"'\''")
    );
    let mut args = ssh_base_args(sync_cfg);
    args.push(format!("root@{ip}"));
    args.push(cmd);
    run_command("ssh", &args).await
}

/// Show `tailscale status` output from the container.
pub async fn tailscale_status(ip: &str, sync_cfg: &SyncConfig) -> Result<String> {
    let mut args = ssh_base_args(sync_cfg);
    args.push(format!("root@{ip}"));
    args.push("tailscale status".to_string());
    super::profile::run_command_output("ssh", &args).await
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn ssh_base_args(sync_cfg: &SyncConfig) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        sync_cfg.ssh_port.to_string(),
        "-o".to_string(), "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(), "ConnectTimeout=15".to_string(),
        "-o".to_string(), "BatchMode=yes".to_string(),
    ];
    if let Some(id) = &sync_cfg.ssh_identity_file {
        args.push("-i".to_string());
        args.push(id.clone());
    }
    args
}

async fn run_script_over_ssh(
    ip:            &str,
    root_password: &str,
    script:        &str,
    sync_cfg:      &SyncConfig,
) -> Result<()> {
    let port_str = sync_cfg.ssh_port.to_string();
    let target   = format!("root@{ip}");
    let cmd      = format!("bash -s <<'__DPS_EOF__'\n{script}\n__DPS_EOF__");

    if which_available("sshpass").await {
        let args = vec![
            "-p".to_string(), root_password.to_string(),
            "ssh".to_string(),
            "-p".to_string(), port_str,
            "-o".to_string(), "StrictHostKeyChecking=no".to_string(),
            "-o".to_string(), "ConnectTimeout=15".to_string(),
            target,
            cmd,
        ];
        run_command("sshpass", &args).await
    } else {
        info!("`sshpass` not found — using key-only SSH (container must already accept your key)");
        let mut args = vec![
            "-p".to_string(), port_str,
            "-o".to_string(), "StrictHostKeyChecking=no".to_string(),
            "-o".to_string(), "ConnectTimeout=15".to_string(),
            "-o".to_string(), "BatchMode=yes".to_string(),
        ];
        if let Some(id) = &sync_cfg.ssh_identity_file {
            args.push("-i".to_string());
            args.push(id.clone());
        }
        args.push(target);
        args.push(cmd);
        run_command("ssh", &args).await
    }
}

async fn which_available(prog: &str) -> bool {
    tokio::process::Command::new("which")
        .arg(prog)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Find the user's SSH public key: explicit config > ~/.ssh/id_*.pub.
pub fn resolve_public_key(sync_cfg: &SyncConfig) -> Result<String> {
    if let Some(path) = &sync_cfg.ssh_public_key_file {
        let expanded = if let Some(rest) = path.strip_prefix("~/") {
            dirs::home_dir()
                .ok_or_else(|| Error::Config("cannot determine home directory".into()))?
                .join(rest)
        } else {
            std::path::PathBuf::from(path)
        };
        return std::fs::read_to_string(&expanded).map_err(|e| {
            Error::Config(format!("cannot read SSH public key {}: {e}", expanded.display()))
        });
    }

    let candidates = ["id_ed25519.pub", "id_ecdsa.pub", "id_rsa.pub"];
    if let Some(home) = dirs::home_dir() {
        let ssh_dir = home.join(".ssh");
        for name in &candidates {
            let p = ssh_dir.join(name);
            if p.exists() {
                return std::fs::read_to_string(&p).map_err(|e| {
                    Error::Config(format!("cannot read {}: {e}", p.display()))
                });
            }
        }
    }

    Err(Error::Config(
        "no SSH public key found — set sync.ssh_public_key_file in config \
         or generate a key with `ssh-keygen`"
            .into(),
    ))
}
