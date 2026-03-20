use std::time::Duration;

use tracing::info;

use crate::config::ContainerConfig;
use crate::error::{Error, Result};
use super::client::ProxmoxClient;

// ── Status ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerStatus {
    Running,
    Stopped,
    Unknown(String),
}

impl ContainerStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "stopped" => Self::Stopped,
            other => Self::Unknown(other.to_string()),
        }
    }
}

// ── Core operations ───────────────────────────────────────────────────────────

/// Check whether a container with the given VMID exists.
pub async fn exists(client: &ProxmoxClient, node: &str, vmid: u32) -> Result<bool> {
    let list = client.get(&format!("nodes/{node}/lxc")).await?;
    if let Some(arr) = list.as_array() {
        for ct in arr {
            if ct["vmid"].as_u64() == Some(vmid as u64) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Create the LXC container from config.  Returns the task UPID.
pub async fn create(
    client:   &ProxmoxClient,
    node:     &str,
    cfg:      &ContainerConfig,
    ssh_key:  Option<&str>,
) -> Result<String> {
    let rootfs = format!("{}:{}", cfg.storage, cfg.disk_size);

    let mut params: Vec<(&str, String)> = vec![
        ("vmid",        cfg.vmid.to_string()),
        ("hostname",    cfg.hostname.clone()),
        ("ostemplate",  cfg.template.clone()),
        ("rootfs",      rootfs),
        ("memory",      cfg.memory.to_string()),
        ("cores",       cfg.cores.to_string()),
        ("net0",        cfg.network.net0_param()),
        ("password",    cfg.root_password.clone()),
        ("unprivileged","1".into()),
        ("features",    "nesting=1".into()),
        ("onboot",      "1".into()),
    ];

    if let Some(key) = ssh_key {
        // URL-encode the key since Proxmox expects it in that form
        params.push(("ssh-public-keys", key.to_string()));
    }

    let form: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let data = client.post_form(&format!("nodes/{node}/lxc"), &form).await?;

    let upid = data.as_str()
        .ok_or_else(|| Error::ProxmoxApi {
            status: 200,
            message: "create returned no UPID".into(),
        })?
        .to_string();

    Ok(upid)
}

/// Start the container.  Returns the task UPID.
pub async fn start(client: &ProxmoxClient, node: &str, vmid: u32) -> Result<String> {
    let data = client
        .post_form(&format!("nodes/{node}/lxc/{vmid}/status/start"), &[])
        .await?;
    let upid = data.as_str()
        .ok_or_else(|| Error::ProxmoxApi { status: 200, message: "no UPID from start".into() })?
        .to_string();
    Ok(upid)
}

/// Stop the container.  Returns the task UPID.
pub async fn stop(client: &ProxmoxClient, node: &str, vmid: u32) -> Result<String> {
    let data = client
        .post_form(&format!("nodes/{node}/lxc/{vmid}/status/stop"), &[])
        .await?;
    let upid = data.as_str()
        .ok_or_else(|| Error::ProxmoxApi { status: 200, message: "no UPID from stop".into() })?
        .to_string();
    Ok(upid)
}

/// Get the current status of a container.
pub async fn status(client: &ProxmoxClient, node: &str, vmid: u32) -> Result<ContainerStatus> {
    let data = client
        .get(&format!("nodes/{node}/lxc/{vmid}/status/current"))
        .await
        .map_err(|e| match e {
            Error::ProxmoxApi { status: 500, .. } => Error::ContainerNotFound(vmid),
            other => other,
        })?;

    Ok(ContainerStatus::from_str(
        data["status"].as_str().unwrap_or("unknown"),
    ))
}

/// Destroy the container (must be stopped first).
pub async fn destroy(client: &ProxmoxClient, node: &str, vmid: u32) -> Result<String> {
    let data = client
        .delete(&format!("nodes/{node}/lxc/{vmid}"))
        .await?;
    let upid = data.as_str()
        .ok_or_else(|| Error::ProxmoxApi { status: 200, message: "no UPID from destroy".into() })?
        .to_string();
    Ok(upid)
}

/// Query the container's network interfaces to find its primary IPv4 address.
pub async fn get_ip(client: &ProxmoxClient, node: &str, vmid: u32) -> Result<Option<String>> {
    let data = match client.get(&format!("nodes/{node}/lxc/{vmid}/interfaces")).await {
        Ok(v)  => v,
        // 500 often means the guest agent isn't ready yet — treat as "no IP yet"
        Err(Error::ProxmoxApi { status: 500, .. }) => return Ok(None),
        Err(e) => return Err(e),
    };

    if let Some(ifaces) = data.as_array() {
        for iface in ifaces {
            let name = iface["name"].as_str().unwrap_or("");
            if name == "lo" {
                continue;
            }
            if let Some(inet) = iface["inet"].as_str() {
                // inet is "address/prefix" — strip the prefix
                let addr = inet.split('/').next().unwrap_or(inet);
                return Ok(Some(addr.to_string()));
            }
        }
    }
    Ok(None)
}

// ── High-level helpers ────────────────────────────────────────────────────────

/// Wait until the container reports `running` status.
pub async fn wait_until_running(
    client: &ProxmoxClient,
    node:   &str,
    vmid:   u32,
) -> Result<()> {
    let max = 60u32;
    for i in 0..max {
        tokio::time::sleep(Duration::from_secs(2)).await;
        match status(client, node, vmid).await? {
            ContainerStatus::Running => {
                info!("container {vmid} is running");
                return Ok(());
            }
            s => {
                if i % 5 == 0 {
                    info!("waiting for container {vmid} to start ({i}/{max})… status={s:?}");
                }
            }
        }
    }
    Err(Error::Timeout(format!("container {vmid} to start")))
}

/// Poll until a non-loopback IP is available (the DHCP lease may arrive late).
pub async fn wait_for_ip(
    client: &ProxmoxClient,
    node:   &str,
    vmid:   u32,
) -> Result<String> {
    let max = 30u32;
    for i in 0..max {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Some(ip) = get_ip(client, node, vmid).await? {
            info!("container {vmid} has IP {ip}");
            return Ok(ip);
        }
        if i % 5 == 0 {
            info!("waiting for container {vmid} IP assignment ({i}/{max})…");
        }
    }
    Err(Error::Timeout(format!("IP assignment for container {vmid}")))
}
