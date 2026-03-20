//! dps-gui — system-tray application for dps-daemon.
//!
//! Architecture:
//!   Main thread    — eframe event loop (egui rendering + tray polling)
//!   Background thread — tokio runtime; maintains Unix-socket connection to the
//!                       daemon; pushes `DaemonEvent`s into `state: GuiState`
//!                       protected by `Arc<Mutex<_>>`; calls `ctx.request_repaint()`
//!                       to wake egui whenever state changes.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use egui::{Color32, RichText, ScrollArea};
use tray_icon::{
    menu::{Menu, MenuItem, MenuEvent, PredefinedMenuItem},
    TrayIcon, TrayIconBuilder, TrayIconEvent,
};

use crate::ipc::{DaemonEvent, DaemonRequest, DaemonStatus, WatchDir};

// ── GUI state shared between egui thread and IPC thread ──────────────────────

#[derive(Default)]
pub struct GuiState {
    pub connected:  bool,
    pub status:     DaemonStatus,
    pub log:        VecDeque<LogEntry>,
    pub error:      Option<String>,
}

#[derive(Clone)]
pub struct LogEntry {
    pub time: String,
    pub msg:  String,
    pub ok:   bool,
}

impl GuiState {
    fn push_log(&mut self, msg: impl Into<String>, ok: bool) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let t   = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let h   = (t / 3600) % 24;
        let m   = (t / 60) % 60;
        let s   = t % 60;
        self.log.push_front(LogEntry { time: format!("{h:02}:{m:02}:{s:02}"), msg: msg.into(), ok });
        if self.log.len() > 200 { self.log.pop_back(); }
    }
}

// ── Command channel ───────────────────────────────────────────────────────────

pub type CmdTx = std::sync::mpsc::SyncSender<DaemonRequest>;

// ── App ───────────────────────────────────────────────────────────────────────

pub struct DpsApp {
    state:          Arc<Mutex<GuiState>>,
    cmd_tx:         CmdTx,
    _tray:          TrayIcon,
    tray_push_id:   tray_icon::menu::MenuId,
    tray_pull_id:   tray_icon::menu::MenuId,
    tray_watch_id:  tray_icon::menu::MenuId,
    tray_show_id:   tray_icon::menu::MenuId,
    tray_quit_id:   tray_icon::menu::MenuId,
    window_visible: bool,
}

impl DpsApp {
    pub fn new(
        _cc:    &eframe::CreationContext<'_>,
        state:  Arc<Mutex<GuiState>>,
        cmd_tx: CmdTx,
    ) -> Self {
        let push_item  = MenuItem::new("Push Now",         true, None);
        let pull_item  = MenuItem::new("Pull Now",         true, None);
        let watch_item = MenuItem::new("Toggle Watch",     true, None);
        let sep        = PredefinedMenuItem::separator();
        let show_item  = MenuItem::new("Show / Hide",      true, None);
        let quit_item  = MenuItem::new("Quit dps",         true, None);

        let tray_push_id  = push_item.id().clone();
        let tray_pull_id  = pull_item.id().clone();
        let tray_watch_id = watch_item.id().clone();
        let tray_show_id  = show_item.id().clone();
        let tray_quit_id  = quit_item.id().clone();

        let menu = Menu::new();
        menu.append_items(&[&push_item, &pull_item, &watch_item, &sep, &show_item, &quit_item])
            .ok();

        let icon  = build_icon(52, 199, 89, 255); // green
        let _tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("dps — profile sync")
            .with_icon(icon)
            .build()
            .expect("failed to create tray icon");

        Self {
            state,
            cmd_tx,
            _tray,
            tray_push_id,
            tray_pull_id,
            tray_watch_id,
            tray_show_id,
            tray_quit_id,
            window_visible: true,
        }
    }

    fn send(&self, req: DaemonRequest) {
        let _ = self.cmd_tx.try_send(req);
    }
}

impl eframe::App for DpsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── Intercept close — hide to tray instead ───────────────────────────
        if ctx.input(|i| i.viewport().close_requested()) {
            self.window_visible = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }

        // ── Visibility ──────────────────────────────────────────────────────
        if !self.window_visible {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        // ── Tray icon events ────────────────────────────────────────────────
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let tray_icon::TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left, ..
            } = ev {
                self.window_visible = !self.window_visible;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(self.window_visible));
            }
        }

        // ── Tray menu events ────────────────────────────────────────────────
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.tray_push_id {
                self.send(DaemonRequest::SyncNow { direction: crate::ipc::SyncDir::Push });
            } else if ev.id == self.tray_pull_id {
                self.send(DaemonRequest::SyncNow { direction: crate::ipc::SyncDir::Pull });
            } else if ev.id == self.tray_watch_id {
                let watching = self.state.lock().map(|s| s.status.watch_running).unwrap_or(false);
                if watching {
                    self.send(DaemonRequest::StopWatch);
                } else {
                    self.send(DaemonRequest::StartWatch {
                        direction:   WatchDir::Both,
                        debounce_ms: 500,
                        poll_secs:   30,
                    });
                }
            } else if ev.id == self.tray_show_id {
                self.window_visible = !self.window_visible;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(self.window_visible));
            } else if ev.id == self.tray_quit_id {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        if !self.window_visible { return; }

        // ── Snapshot state ──────────────────────────────────────────────────
        let (connected, status, log, err) = {
            let s = self.state.lock().unwrap();
            (s.connected, s.status.clone(), s.log.iter().cloned().collect::<Vec<_>>(), s.error.clone())
        };

        // ── UI ──────────────────────────────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("dps — Debian Profile Sync");
            ui.separator();

            // Status grid
            egui::Grid::new("status")
                .num_columns(2)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    // Daemon connection
                    ui.label("Daemon:");
                    if connected {
                        ui.colored_label(Color32::GREEN, "● connected");
                    } else {
                        ui.colored_label(Color32::RED, "● not running");
                    }
                    ui.end_row();

                    // Container
                    ui.label("Container:");
                    let (color, label) = match status.container_status.as_str() {
                        "running" => (Color32::GREEN, "● running"),
                        "stopped" => (Color32::RED,   "● stopped"),
                        _         => (Color32::YELLOW,"● unknown"),
                    };
                    ui.colored_label(color, label);
                    ui.end_row();

                    // IP
                    ui.label("IP:");
                    let ip_str = status.tailscale_ip.as_deref()
                        .or(status.container_ip.as_deref())
                        .unwrap_or("—");
                    ui.label(ip_str);
                    ui.end_row();

                    // Watcher
                    ui.label("Watcher:");
                    if status.watch_running {
                        let dir = status.watch_direction.map(|d| d.to_string()).unwrap_or_default();
                        ui.colored_label(Color32::GREEN, format!("● active ({dir})"));
                    } else {
                        ui.colored_label(Color32::GRAY, "○ stopped");
                    }
                    ui.end_row();

                    // Last sync
                    ui.label("Last sync:");
                    match (&status.last_sync_time, &status.last_sync_ok) {
                        (Some(t), Some(true))  => { ui.colored_label(Color32::GREEN, format!("✓ {t}")); }
                        (Some(t), Some(false)) => { ui.colored_label(Color32::RED,   format!("✗ {t}")); }
                        _                      => { ui.label("—"); }
                    }
                    ui.end_row();
                });

            if let Some(e) = err {
                ui.separator();
                ui.colored_label(Color32::RED, format!("⚠ {e}"));
            }

            ui.separator();

            // Control buttons
            ui.horizontal(|ui| {
                if ui.button("⬆  Push Now").clicked() {
                    self.send(DaemonRequest::SyncNow { direction: crate::ipc::SyncDir::Push });
                }
                if ui.button("⬇  Pull Now").clicked() {
                    self.send(DaemonRequest::SyncNow { direction: crate::ipc::SyncDir::Pull });
                }
                if status.watch_running {
                    if ui.button("■  Stop Watch").clicked() {
                        self.send(DaemonRequest::StopWatch);
                    }
                } else {
                    if ui.button("▶  Start Watch").clicked() {
                        self.send(DaemonRequest::StartWatch {
                            direction: WatchDir::Both,
                            debounce_ms: 500,
                            poll_secs:   30,
                        });
                    }
                }
            });

            ui.horizontal(|ui| {
                if status.container_status == "stopped" {
                    if ui.button("▶  Start Container").clicked() {
                        self.send(DaemonRequest::StartContainer);
                    }
                } else {
                    if ui.button("■  Stop Container").clicked() {
                        self.send(DaemonRequest::StopContainer);
                    }
                }
            });

            ui.separator();
            ui.label(RichText::new("Activity").strong());

            ScrollArea::vertical()
                .max_height(180.0)
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    if log.is_empty() {
                        ui.weak("no activity yet");
                    }
                    for entry in &log {
                        let color = if entry.ok { Color32::LIGHT_GREEN } else { Color32::LIGHT_RED };
                        ui.colored_label(color, format!("{} {}", entry.time, entry.msg));
                    }
                });

            if !connected {
                ui.separator();
                if ui.button("Start daemon").clicked() {
                    let _ = std::process::Command::new("dps-daemon").spawn();
                }
            }
        });

        // Request continuous repaint only when actively watching so we notice
        // new events.  Otherwise we just repaint on user input.
        if status.watch_running {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }
}

// ── IPC background task ───────────────────────────────────────────────────────

/// Spawn a background OS thread running a tokio runtime.
/// It connects to the daemon, subscribes, and keeps `state` up to date.
pub fn spawn_ipc_thread(
    state:  Arc<Mutex<GuiState>>,
    cmd_rx: std::sync::mpsc::Receiver<DaemonRequest>,
    ctx:    egui::Context,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(ipc_loop(state, cmd_rx, ctx));
    });
}

async fn ipc_loop(
    state:  Arc<Mutex<GuiState>>,
    cmd_rx: std::sync::mpsc::Receiver<DaemonRequest>,
    ctx:    egui::Context,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use crate::ipc::socket_path;

    loop {
        let sock = socket_path();

        match UnixStream::connect(&sock).await {
            Err(_) => {
                {
                    let mut s = state.lock().unwrap();
                    s.connected = false;
                    s.error     = Some("daemon not running".into());
                }
                ctx.request_repaint();
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            }
            Ok(stream) => {
                {
                    let mut s = state.lock().unwrap();
                    s.connected = true;
                    s.error     = None;
                }
                ctx.request_repaint();

                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();

                // Send subscribe request
                let sub = serde_json::to_string(&DaemonRequest::Subscribe).unwrap() + "\n";
                if writer.write_all(sub.as_bytes()).await.is_err() {
                    continue;
                }

                loop {
                    // Check for outgoing commands (non-blocking)
                    while let Ok(req) = cmd_rx.try_recv() {
                        if let Ok(mut s) = serde_json::to_string(&req) {
                            s.push('\n');
                            if writer.write_all(s.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                    }

                    // Wait for next event from daemon (with 100ms timeout so we
                    // can service the cmd_rx promptly)
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(100),
                        lines.next_line(),
                    )
                    .await
                    {
                        Ok(Ok(Some(line))) => {
                            if let Ok(ev) = serde_json::from_str::<DaemonEvent>(&line) {
                                handle_event(&state, ev);
                                ctx.request_repaint();
                            }
                        }
                        Ok(Ok(None)) | Ok(Err(_)) => break, // socket closed
                        Err(_) => {}                        // timeout — loop
                    }
                }

                {
                    let mut s = state.lock().unwrap();
                    s.connected = false;
                }
                ctx.request_repaint();
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

fn handle_event(state: &Arc<Mutex<GuiState>>, ev: DaemonEvent) {
    let mut s = state.lock().unwrap();
    match ev {
        DaemonEvent::Status(st) => { s.status = st; }
        DaemonEvent::SyncStarted { direction, path } => {
            s.push_log(format!("{direction:?} started: {path}"), true);
        }
        DaemonEvent::SyncCompleted { direction, path } => {
            s.push_log(format!("{direction:?} OK: {path}"), true);
        }
        DaemonEvent::SyncFailed { direction, path, error } => {
            s.push_log(format!("{direction:?} FAILED {path}: {error}"), false);
        }
        DaemonEvent::Shutdown => {
            s.connected = false;
            s.push_log("daemon shut down", false);
        }
    }
}

// ── Tray icon image ───────────────────────────────────────────────────────────

fn build_icon(r: u8, g: u8, b: u8, a: u8) -> tray_icon::Icon {
    let size = 32u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let cx = size as f32 / 2.0;
    let cy = size as f32 / 2.0;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            if dx * dx + dy * dy <= (cx - 1.0) * (cy - 1.0) {
                let i = ((y * size + x) * 4) as usize;
                rgba[i]     = r;
                rgba[i + 1] = g;
                rgba[i + 2] = b;
                rgba[i + 3] = a;
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, size, size).expect("invalid icon")
}
