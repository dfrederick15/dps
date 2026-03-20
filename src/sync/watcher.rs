//! Real-time profile synchronisation.
//!
//! Push direction: inotify (via `notify`) watches every configured local path;
//! events are debounced then rsync fires for the affected `SyncPath`.
//!
//! Pull direction: periodic rsync poll from the container.  rsync's
//! `--checksum` flag means nothing is transferred when nothing changed.
//!
//! Both directions run concurrently under a single Ctrl-C handler.
//! The daemon calls `spawn_push_watcher` / `spawn_pull_poller` directly
//! so it can manage `JoinHandle`s independently.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::SyncPath;
use crate::error::Result;
use super::profile::{Direction, Syncer, expand_tilde};

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchDirection {
    Push,
    Pull,
    Both,
}

pub struct WatchArgs {
    pub direction:   WatchDirection,
    pub debounce_ms: u64,
    pub poll_secs:   u64,
}

// ── CLI entry point ───────────────────────────────────────────────────────────

/// Run watcher(s) until Ctrl-C or a fatal error.  Takes ownership of the
/// Syncer so it can be safely cloned into spawned tasks.
pub async fn run(syncer: Syncer, args: &WatchArgs) -> Result<()> {
    tokio::select! {
        r = run_inner(syncer, args) => r,
        _ = tokio::signal::ctrl_c() => {
            info!("received Ctrl-C — stopping watcher");
            Ok(())
        }
    }
}

async fn run_inner(syncer: Syncer, args: &WatchArgs) -> Result<()> {
    match args.direction {
        WatchDirection::Push => run_push_watcher(syncer, args.debounce_ms).await,
        WatchDirection::Pull => run_pull_poller(syncer, args.poll_secs).await,
        WatchDirection::Both => {
            let (r1, r2) = tokio::join!(
                run_push_watcher(syncer.clone(), args.debounce_ms),
                run_pull_poller(syncer, args.poll_secs),
            );
            r1.and(r2)
        }
    }
}

// ── Daemon entry points (spawn-friendly) ─────────────────────────────────────

/// Spawn the push watcher as a detached Tokio task.
/// Returns the `JoinHandle` so the daemon can abort it.
pub fn spawn_push_watcher(
    syncer:      Syncer,
    debounce_ms: u64,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(run_push_watcher(syncer, debounce_ms))
}

/// Spawn the pull poller as a detached Tokio task.
pub fn spawn_pull_poller(
    syncer:    Syncer,
    poll_secs: u64,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(run_pull_poller(syncer, poll_secs))
}

// ── Push watcher ──────────────────────────────────────────────────────────────

pub async fn run_push_watcher(syncer: Syncer, debounce_ms: u64) -> Result<()> {
    let debounce = Duration::from_millis(debounce_ms);
    let (tx, mut rx) = mpsc::channel::<PathBuf>(1024);

    let paths: Vec<(PathBuf, SyncPath)> = syncer
        .sync_paths()
        .iter()
        .map(|sp| (expand_tilde(&sp.local), sp.clone()))
        .collect();

    let tx_cb = tx.clone();
    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                if matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) {
                    for path in event.paths {
                        let _ = tx_cb.blocking_send(path);
                    }
                }
            }
        },
        notify::Config::default(),
    )
    .map_err(|e| crate::error::Error::Config(format!("inotify init failed: {e}")))?;

    let mut watched = 0usize;
    for (local, sp) in &paths {
        if local.exists() {
            watcher
                .watch(local, RecursiveMode::Recursive)
                .map_err(|e| crate::error::Error::Config(
                    format!("cannot watch {}: {e}", local.display())
                ))?;
            info!("watching: {}", sp.local);
            watched += 1;
        } else {
            warn!("skipping watch of '{}': path does not exist locally", sp.local);
        }
    }

    if watched == 0 {
        warn!("no paths to watch — all configured local paths are missing");
        return Ok(());
    }

    info!("push watcher active on {watched} path(s), debounce {}ms", debounce_ms);

    let mut pending: HashMap<PathBuf, tokio::time::Instant> = HashMap::new();

    loop {
        let next = pending.values().copied().min();

        if let Some(deadline) = next {
            tokio::select! {
                maybe = rx.recv() => match maybe {
                    Some(p) => reset_deadline(&mut pending, p, debounce),
                    None    => break,
                },
                _ = tokio::time::sleep_until(deadline) => {
                    fire_due(&mut pending, &syncer, &paths, deadline).await;
                }
            }
        } else {
            match rx.recv().await {
                Some(p) => reset_deadline(&mut pending, p, debounce),
                None    => break,
            }
        }
    }

    Ok(())
}

fn reset_deadline(pending: &mut HashMap<PathBuf, tokio::time::Instant>, path: PathBuf, d: Duration) {
    pending.insert(path, tokio::time::Instant::now() + d);
}

async fn fire_due(
    pending:  &mut HashMap<PathBuf, tokio::time::Instant>,
    syncer:   &Syncer,
    paths:    &[(PathBuf, SyncPath)],
    now:      tokio::time::Instant,
) {
    let due: Vec<PathBuf> = pending.iter()
        .filter(|(_, &dl)| dl <= now)
        .map(|(p, _)| p.clone())
        .collect();

    for changed in &due {
        pending.remove(changed);

        let matched = paths.iter()
            .filter(|(local, _)| changed.starts_with(local))
            .max_by_key(|(local, _)| local.components().count());

        if let Some((_, sp)) = matched {
            info!("change in '{}' — pushing", sp.local);
            if let Err(e) = syncer.sync_one(sp, Direction::Push).await {
                error!("push failed for '{}': {e}", sp.local);
            }
        }
    }
}

// ── Pull poller ───────────────────────────────────────────────────────────────

pub async fn run_pull_poller(syncer: Syncer, poll_secs: u64) -> Result<()> {
    let interval = Duration::from_secs(poll_secs);
    info!("pull poller active (every {}s)", poll_secs);

    loop {
        info!("pull poll: syncing all paths from container");
        for sp in syncer.sync_paths() {
            if let Err(e) = syncer.sync_one(sp, Direction::Pull).await {
                error!("pull failed for '{}': {e}", sp.local);
            }
        }
        tokio::time::sleep(interval).await;
    }
}
