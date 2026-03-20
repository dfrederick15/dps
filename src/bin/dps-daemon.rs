//! dps-daemon — background service that keeps profiles in sync.
//!
//! Designed to run as a systemd user service.  On start it:
//!   1. Loads config from `~/.config/dps/config.toml`.
//!   2. Optionally starts the Proxmox container.
//!   3. Optionally starts the push-watcher + pull-poller.
//!   4. Listens on a Unix socket for GUI / CLI connections.
//!
//! Install:
//!   cp ~/.cargo/bin/dps-daemon ~/.local/bin/
//!   systemctl --user enable --now dps-daemon

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use dps::config::Config;
use dps::ipc::{
    DaemonEvent, DaemonRequest, DaemonResponse, DaemonStatus, SyncDir, WatchDir,
    socket_path,
};
use dps::proxmox::{lxc, ProxmoxClient};
use dps::sync::{
    profile::{Direction, Syncer},
    watcher::{spawn_pull_poller, spawn_push_watcher},
};

// ── Shared state ──────────────────────────────────────────────────────────────

struct DaemonState {
    status:       DaemonStatus,
    config:       Config,
    syncer:       Option<Syncer>,
    push_handle:  Option<JoinHandle<dps::error::Result<()>>>,
    pull_handle:  Option<JoinHandle<dps::error::Result<()>>>,
    event_tx:     broadcast::Sender<DaemonEvent>,
}

impl DaemonState {
    fn broadcast(&self, ev: DaemonEvent) {
        let _ = self.event_tx.send(ev);
    }

    fn broadcast_status(&self) {
        self.broadcast(DaemonEvent::Status(self.status.clone()));
    }

    fn set_watch_running(&mut self, running: bool, dir: Option<WatchDir>) {
        self.status.watch_running   = running;
        self.status.watch_direction = dir;
        self.broadcast_status();
    }

    fn stamp_sync(&mut self, ok: bool) {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Format as HH:MM:SS
        let h = (t / 3600) % 24;
        let m = (t / 60) % 60;
        let s = t % 60;
        self.status.last_sync_time = Some(format!("{h:02}:{m:02}:{s:02}"));
        self.status.last_sync_ok   = Some(ok);
        self.broadcast_status();
    }
}

type State = Arc<Mutex<DaemonState>>;

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    if let Err(e) = run().await {
        error!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::load(None)?;

    // ── Proxmox client ──────────────────────────────────────────────────────
    let client = ProxmoxClient::new(&cfg.proxmox)?;
    client.authenticate().await?;

    let node = cfg.proxmox.node.clone();
    let vmid = cfg.container.vmid;

    // ── Optionally start the container ─────────────────────────────────────
    if cfg.daemon.as_ref().map(|d| d.auto_start_container).unwrap_or(false) {
        match lxc::status(&client, &node, vmid).await {
            Ok(lxc::ContainerStatus::Stopped) => {
                info!("auto-starting container {vmid}…");
                if let Ok(upid) = lxc::start(&client, &node, vmid).await {
                    let _ = client.wait_for_task(&node, &upid).await;
                    let _ = lxc::wait_until_running(&client, &node, vmid).await;
                }
            }
            _ => {}
        }
    }

    // ── Resolve container IP ────────────────────────────────────────────────
    let container_ip = cfg.sync.container_ip.clone()
        .or_else(|| {
            // Use Tailscale IP if preferred
            cfg.tailscale.as_ref()
                .filter(|ts| ts.prefer_tailscale_ip)
                .and_then(|ts| ts.container_ts_ip.clone())
        });

    // ── Build Syncer ────────────────────────────────────────────────────────
    let username = cfg.daemon.as_ref()
        .and_then(|d| d.sync_user.clone())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "root".to_string());

    let syncer = container_ip.as_ref().map(|ip| {
        Syncer::new(cfg.sync.clone(), ip.clone(), username.clone())
    });

    // ── Shared state ────────────────────────────────────────────────────────
    let (event_tx, _) = broadcast::channel::<DaemonEvent>(64);

    let mut initial_status = DaemonStatus::default();
    initial_status.daemon_pid    = std::process::id();
    initial_status.container_ip  = container_ip.clone();

    if let Ok(st) = lxc::status(&client, &node, vmid).await {
        initial_status.container_status = match st {
            lxc::ContainerStatus::Running    => "running".into(),
            lxc::ContainerStatus::Stopped    => "stopped".into(),
            lxc::ContainerStatus::Unknown(s) => s,
        };
    }

    let state: State = Arc::new(Mutex::new(DaemonState {
        status:      initial_status,
        config:      cfg.clone(),
        syncer:      syncer.clone(),
        push_handle: None,
        pull_handle: None,
        event_tx,
    }));

    // ── Auto-start watcher ──────────────────────────────────────────────────
    if let (Some(d), Some(s)) = (cfg.daemon.as_ref(), syncer) {
        if d.auto_watch {
            let dir = match d.watch_direction.as_str() {
                "push" => WatchDir::Push,
                "pull" => WatchDir::Pull,
                _      => WatchDir::Both,
            };
            start_watch(state.clone(), s, dir, d.debounce_ms, d.poll_secs).await;
        }
    }

    // ── Unix socket IPC server ──────────────────────────────────────────────
    let sock = socket_path();
    std::fs::remove_file(&sock).ok(); // remove stale socket
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&sock)?;
    info!("daemon listening on {}", sock.display());

    // ── SIGTERM handler ─────────────────────────────────────────────────────
    let state_sig = state.clone();
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut sig) = signal(SignalKind::terminate()) {
            sig.recv().await;
            info!("received SIGTERM — shutting down");
            let st = state_sig.lock().await;
            st.broadcast(DaemonEvent::Shutdown);
        }
        std::process::exit(0);
    });

    // ── Accept loop ─────────────────────────────────────────────────────────
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let state  = state.clone();
                let client_clone = {
                    let cfg = state.lock().await.config.proxmox.clone();
                    ProxmoxClient::new(&cfg).ok()
                };
                tokio::spawn(handle_client(stream, state, client_clone));
            }
            Err(e) => error!("accept error: {e}"),
        }
    }
}

// ── Client handler ────────────────────────────────────────────────────────────

async fn handle_client(
    stream:  UnixStream,
    state:   State,
    client:  Option<ProxmoxClient>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut event_rx: Option<broadcast::Receiver<DaemonEvent>> = None;

    loop {
        // If subscribed, wait for either a new line from the client OR an event
        // to push; otherwise just wait for a command line.
        let line = if let Some(rx) = &mut event_rx {
            tokio::select! {
                l = lines.next_line() => match l { Ok(Some(s)) => Some(s), _ => break },
                ev = rx.recv() => {
                    match ev {
                        Ok(ev) => {
                            send_event(&mut writer, &ev).await;
                            if matches!(ev, DaemonEvent::Shutdown) { break; }
                            continue;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("GUI client lagged by {n} events");
                            continue;
                        }
                        Err(_) => break,
                    }
                }
            }
        } else {
            match lines.next_line().await {
                Ok(Some(s)) => Some(s),
                _ => break,
            }
        };

        let Some(line) = line else { break };
        let req: DaemonRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                send_response(&mut writer, &DaemonResponse::Error { message: e.to_string() }).await;
                continue;
            }
        };

        match req {
            DaemonRequest::Subscribe => {
                let (status, rx) = {
                    let st = state.lock().await;
                    (st.status.clone(), st.event_tx.subscribe())
                };
                event_rx = Some(rx);
                send_event(&mut writer, &DaemonEvent::Status(status)).await;
            }

            DaemonRequest::GetStatus => {
                let status = state.lock().await.status.clone();
                send_response(&mut writer, &DaemonResponse::Status(status)).await;
            }

            DaemonRequest::SyncNow { direction } => {
                let syncer = state.lock().await.syncer.clone();
                if let Some(syncer) = syncer {
                    let state2 = state.clone();
                    let dir    = direction;
                    tokio::spawn(async move {
                        let d = match dir { SyncDir::Push => Direction::Push, SyncDir::Pull => Direction::Pull };
                        let paths: Vec<_> = syncer.sync_paths().to_vec();
                        for sp in &paths {
                            {
                                let st = state2.lock().await;
                                st.broadcast(DaemonEvent::SyncStarted { direction: dir, path: sp.local.clone() });
                            }
                            match syncer.sync_one(sp, d).await {
                                Ok(()) => {
                                    let st = state2.lock().await;
                                    st.broadcast(DaemonEvent::SyncCompleted { direction: dir, path: sp.local.clone() });
                                }
                                Err(e) => {
                                    let st = state2.lock().await;
                                    st.broadcast(DaemonEvent::SyncFailed { direction: dir, path: sp.local.clone(), error: e.to_string() });
                                }
                            }
                        }
                        state2.lock().await.stamp_sync(true);
                    });
                    send_response(&mut writer, &DaemonResponse::Ok).await;
                } else {
                    send_response(&mut writer, &DaemonResponse::Error { message: "container IP not known — run `dps setup` first".into() }).await;
                }
            }

            DaemonRequest::StartWatch { direction, debounce_ms, poll_secs } => {
                let syncer = state.lock().await.syncer.clone();
                if let Some(syncer) = syncer {
                    start_watch(state.clone(), syncer, direction, debounce_ms, poll_secs).await;
                    send_response(&mut writer, &DaemonResponse::Ok).await;
                } else {
                    send_response(&mut writer, &DaemonResponse::Error { message: "no syncer available".into() }).await;
                }
            }

            DaemonRequest::StopWatch => {
                stop_watch(state.clone()).await;
                send_response(&mut writer, &DaemonResponse::Ok).await;
            }

            DaemonRequest::StartContainer => {
                if let Some(ref c) = client {
                    let st = state.lock().await;
                    let node = st.config.proxmox.node.clone();
                    let vmid = st.config.container.vmid;
                    drop(st);
                    match lxc::start(c, &node, vmid).await {
                        Ok(upid) => {
                            let _ = c.wait_for_task(&node, &upid).await;
                            send_response(&mut writer, &DaemonResponse::Ok).await;
                        }
                        Err(e) => send_response(&mut writer, &DaemonResponse::Error { message: e.to_string() }).await,
                    }
                } else {
                    send_response(&mut writer, &DaemonResponse::Error { message: "Proxmox client unavailable".into() }).await;
                }
            }

            DaemonRequest::StopContainer => {
                if let Some(ref c) = client {
                    let st = state.lock().await;
                    let node = st.config.proxmox.node.clone();
                    let vmid = st.config.container.vmid;
                    drop(st);
                    match lxc::stop(c, &node, vmid).await {
                        Ok(upid) => {
                            let _ = c.wait_for_task(&node, &upid).await;
                            send_response(&mut writer, &DaemonResponse::Ok).await;
                        }
                        Err(e) => send_response(&mut writer, &DaemonResponse::Error { message: e.to_string() }).await,
                    }
                } else {
                    send_response(&mut writer, &DaemonResponse::Error { message: "Proxmox client unavailable".into() }).await;
                }
            }

            DaemonRequest::Shutdown => {
                {
                    let st = state.lock().await;
                    st.broadcast(DaemonEvent::Shutdown);
                }
                send_response(&mut writer, &DaemonResponse::Ok).await;
                std::process::exit(0);
            }
        }
    }
}

// ── Watch task management ─────────────────────────────────────────────────────

async fn start_watch(
    state:       State,
    syncer:      Syncer,
    direction:   WatchDir,
    debounce_ms: u64,
    poll_secs:   u64,
) {
    stop_watch(state.clone()).await;

    let mut st = state.lock().await;

    match direction {
        WatchDir::Push | WatchDir::Both => {
            st.push_handle = Some(spawn_push_watcher(syncer.clone(), debounce_ms));
        }
        _ => {}
    }
    match direction {
        WatchDir::Pull | WatchDir::Both => {
            st.pull_handle = Some(spawn_pull_poller(syncer, poll_secs));
        }
        _ => {}
    }

    st.set_watch_running(true, Some(direction));
    info!("watcher started ({direction})");
}

async fn stop_watch(state: State) {
    let mut st = state.lock().await;

    if let Some(h) = st.push_handle.take() {
        h.abort();
        drop(st);
        // Re-acquire after await to avoid holding lock across await
        let _ = h.await;
        let mut st2 = state.lock().await;
        if let Some(h) = st2.pull_handle.take() { h.abort(); }
        st2.set_watch_running(false, None);
        return;
    }

    if let Some(h) = st.pull_handle.take() {
        drop(st);
        let _ = h.await;
        state.lock().await.set_watch_running(false, None);
        return;
    }
}

// ── Wire helpers ──────────────────────────────────────────────────────────────

async fn send_response(writer: &mut (impl AsyncWriteExt + Unpin), resp: &DaemonResponse) {
    if let Ok(mut s) = serde_json::to_string(resp) {
        s.push('\n');
        let _ = writer.write_all(s.as_bytes()).await;
    }
}

async fn send_event(writer: &mut (impl AsyncWriteExt + Unpin), ev: &DaemonEvent) {
    if let Ok(mut s) = serde_json::to_string(ev) {
        s.push('\n');
        let _ = writer.write_all(s.as_bytes()).await;
    }
}
