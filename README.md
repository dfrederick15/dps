# dps — Debian Profile Sync

Keep your shell config, dotfiles, and editor settings identical across every Debian machine you own — automatically, in real time, over any network.

`dps` spins up a lightweight Proxmox LXC container that acts as the central sync hub.  Every machine runs the `dps-daemon` in the background; it watches for local file changes and pushes them to the container, and periodically pulls changes made from other machines.  A system-tray GUI (`dps-gui`) gives you live status at a glance.

---

## Features

- **One-command setup** — interactive wizard creates the LXC container, injects your SSH key, and optionally joins Tailscale
- **Real-time push** — inotify watcher debounces changes and rsyncs instantly
- **Periodic pull** — background poller keeps you in sync when you return to a machine
- **Tailscale integration** — sync works from any network without opening firewall ports
- **Machine-specific file filtering** — built-in patterns skip hostname files, dconf, pulse TDB, recently-used lists, etc.
- **`.syncignore` support** — per-directory exclusion rules (gitignore syntax)
- **System-tray GUI** — egui app connects to the daemon over a Unix socket
- **systemd user service** — daemon starts at login, restarts on failure

---

## Requirements

| Requirement | Notes |
|---|---|
| Proxmox VE 7+ | Any node with LXC support |
| Debian/Ubuntu client machines | `rsync` and `ssh` must be in PATH |
| Rust 1.75+ | Only needed to build from source |
| GTK3 + libayatana-appindicator3 | Only for the GUI binary |

Install GUI system deps on Debian/Ubuntu:
```sh
sudo apt install libgtk-3-dev libayatana-appindicator3-dev
```

---

## Installation

### From crates.io (CLI + daemon only)
```sh
cargo install dps
```

### From source (all binaries including GUI)
```sh
git clone https://github.com/dfrederick15/dps
cd dps
cargo build --release                        # CLI + daemon
cargo build --release --features gui         # + tray GUI
install -Dm755 target/release/dps       ~/.local/bin/dps
install -Dm755 target/release/dps-daemon ~/.local/bin/dps-daemon
install -Dm755 target/release/dps-gui    ~/.local/bin/dps-gui   # if built
```

---

## Quick Start

```sh
# 1. Run the interactive setup wizard — creates config + provisions the container
dps setup

# 2. Enable the background daemon
mkdir -p ~/.config/systemd/user
cp systemd/dps-daemon.service ~/.config/systemd/user/
systemctl --user enable --now dps-daemon

# 3. On every other machine: install dps, copy your config, enable the daemon
#    (no `dps setup` needed — the container already exists)

# 4. Manual sync
dps push          # push local → container
dps pull          # pull container → local

# 5. Or watch continuously
dps watch
```

---

## Setup Wizard

Running `dps setup` with no config file launches a step-by-step wizard:

```
╔══════════════════════════════════════╗
║        dps — first-time setup        ║
╚══════════════════════════════════════╝

── Proxmox Connection ───────────────────────────────────────────────
  Host (IP or hostname): 192.168.1.10
  Port [8006]:
  Node name [pve]:
  Verify SSL certificate [y/N]:

── Authentication ────────────────────────────────────────────────────
  Type (token/password) [token]:
  User [root@pam]:
  Token name [dps]:
  Token value: ****

── LXC Container ─────────────────────────────────────────────────────
  VMID [200]:
  Hostname [dps-sync]:
  ...

── Profile Sync ──────────────────────────────────────────────────────
  Local path [1/?]: ~/.bashrc
  Remote path [.bashrc]:
  ...

── Tailscale (optional) ──────────────────────────────────────────────
  Enable Tailscale integration [y/N]:
```

The wizard saves `~/.config/dps/config.toml` and immediately provisions the container.

---

## CLI Reference

```
dps setup                   First-time setup (wizard + container provisioning)
dps push [--dry-run]        Push local files → container
dps pull [--dry-run]        Pull container → local files
dps check                   Report machine-specific files that would be skipped
dps watch                   Real-time push watcher + periodic pull poller
dps status                  Show container and sync status
dps start                   Start the LXC container
dps stop                    Stop the LXC container
dps destroy                 Destroy the LXC container (requires --yes)
dps tailscale status        Show Tailscale status inside the container
dps tailscale reauth <key>  Re-authenticate Tailscale with a new auth key
```

---

## Configuration

Config lives at `~/.config/dps/config.toml`.  A full annotated example is at [`config.example.toml`](config.example.toml).

### Minimal example (API token auth)

```toml
[proxmox]
host = "192.168.1.10"
node = "pve"

[proxmox.auth]
type        = "token"
user        = "root@pam"
token_name  = "dps"
token_value = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"

[container]
vmid          = 200
hostname      = "dps-sync"
template      = "local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst"
storage       = "local-lvm"
disk_size     = "8G"
root_password = "change-me"

[container.network]
bridge = "vmbr0"
ip     = "dhcp"

[sync]
[[sync.paths]]
local  = "~/.bashrc"
remote = ".bashrc"

[[sync.paths]]
local  = "~/.gitconfig"
remote = ".gitconfig"
```

### Tailscale

```toml
[tailscale]
auth_key            = "tskey-auth-..."
prefer_tailscale_ip = true
```

When `prefer_tailscale_ip = true`, all push/pull transfers use the Tailscale IP (stored automatically after `dps setup`), so sync works from any network.

### Daemon

```toml
[daemon]
auto_start_container = true
auto_watch           = true
watch_direction      = "both"   # "push", "pull", or "both"
debounce_ms          = 500
poll_secs            = 30
```

---

## Background Daemon

The daemon maintains the watcher processes and exposes a Unix socket at `$XDG_RUNTIME_DIR/dps.sock` for the CLI and GUI to connect to.

```sh
# Install and enable
mkdir -p ~/.config/systemd/user
cp systemd/dps-daemon.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now dps-daemon

# Logs
journalctl --user -u dps-daemon -f
```

---

## System-Tray GUI

Build and install (requires GTK3 + libayatana-appindicator3):

```sh
cargo build --release --features gui
install -Dm755 target/release/dps-gui ~/.local/bin/dps-gui
cp autostart/dps-gui.desktop ~/.config/autostart/   # auto-start on login
```

The tray icon turns green when connected to the daemon.  Left-click toggles the control window; right-click shows the menu.

---

## Ignoring Files

### `.syncignore`

Create a `.syncignore` file inside any synced directory to exclude files from that directory (gitignore syntax):

```
*.log
.env
secrets/
```

### Global ignore

Add patterns to `~/.config/dps/syncignore` to exclude files across all paths.

### Built-in machine-specific patterns

`dps` automatically skips files that contain your hostname or machine-id, plus a built-in list of known machine-specific paths:

```
.cache/
monitors.xml
recently-used.xbel
pulse/*.tdb
tracker/
dconf/
```

Run `dps check` to see which files in your sync paths would be flagged.

---

## License

MIT
