//! dps-gui — system-tray profile-sync control panel.
//!
//! Runs as a systemd user autostart service.  Connects to dps-daemon via
//! a Unix socket and shows a tray icon + control window.
//!
//! Build:   cargo build --features gui --bin dps-gui
//! Install: cp target/release/dps-gui ~/.local/bin/
//!          cp autostart/dps-gui.desktop ~/.config/autostart/

use std::sync::{Arc, Mutex};

use dps::gui::{spawn_ipc_thread, DpsApp, GuiState};

fn main() -> eframe::Result<()> {
    // Shared state between egui thread and IPC thread
    let state: Arc<Mutex<GuiState>> = Arc::new(Mutex::new(GuiState::default()));

    // Channel: egui → IPC thread (outgoing commands)
    let (cmd_tx, cmd_rx) = std::sync::mpsc::sync_channel(64);

    // Create eframe options before starting the IPC thread so we can clone ctx
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("dps — Debian Profile Sync")
            .with_inner_size([420.0, 420.0])
            .with_resizable(false),
        ..Default::default()
    };

    eframe::run_native(
        "dps",
        opts,
        Box::new(move |cc| {
            // Now we have the egui Context — spawn the IPC thread.
            spawn_ipc_thread(state.clone(), cmd_rx, cc.egui_ctx.clone());

            Ok(Box::new(DpsApp::new(cc, state, cmd_tx)))
        }),
    )
}
