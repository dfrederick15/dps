//! `.syncignore` parsing and machine-specific file detection.
//!
//! Three layers of exclusion are applied before every rsync/scp transfer:
//!
//! 1. **Built-in machine-specific patterns** — hardware, session, and cache
//!    paths that must never leave a single machine.
//! 2. **Global syncignore** — `~/.config/dps/syncignore`, gitignore-style.
//! 3. **Per-directory `.syncignore`** — placed inside the directory being
//!    synced; anchored patterns (starting with `/`) are relative to that root.
//!
//! On push, every small text file is also scanned for the local hostname and
//! `/etc/machine-id`.  Matches are **warned about but not automatically
//! excluded** — the user decides whether to add them to `.syncignore`.

use std::path::{Path, PathBuf};

use tracing::warn;

// ── Built-in exclusions ───────────────────────────────────────────────────────

/// Always-excluded patterns — machine-specific by their very nature.
pub const MACHINE_SPECIFIC_PATTERNS: &[&str] = &[
    // Caches & temp files
    ".cache/",
    "*.log",
    "*.pid",
    "*.lock",
    "*~",
    "*.swp",
    "*.swo",
    "*.tmp",
    // Display / hardware
    ".config/monitors.xml",
    ".config/xrandr/",
    ".local/share/xorg/",
    // Audio (device-specific IDs/volumes)
    ".config/pulse/*.tdb",
    ".config/pipewire/media-session.d/volumes.conf",
    // Session state & recently-used lists
    ".local/share/recently-used.xbel",
    ".local/share/gvfs-metadata/",
    ".local/share/gnome-shell/",
    // File indexers
    ".local/share/tracker/",
    ".local/share/baloo/",
    // Network / Bluetooth (per-machine addresses)
    ".local/share/networkmanagement/",
    ".config/dconf/user",
    // Thumbnails
    ".local/share/thumbnails/",
    // Trash
    ".local/share/Trash/",
    // Cross-platform noise
    ".DS_Store",
    "Thumbs.db",
    "desktop.ini",
];

// ── IgnoreRules ───────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub struct IgnoreRules {
    patterns: Vec<String>,
}

impl IgnoreRules {
    /// Build rules for one sync path.
    ///
    /// `local_path`         — the local directory being synced.
    /// `extra_patterns`     — additional patterns from `[sync] machine_specific_patterns`.
    /// `global_cfg_dir`     — `~/.config/dps/` where the global `syncignore` lives.
    pub fn load(
        local_path:      &Path,
        extra_patterns:  &[String],
        global_cfg_dir:  Option<&Path>,
    ) -> Self {
        let mut patterns: Vec<String> = MACHINE_SPECIFIC_PATTERNS
            .iter()
            .map(|&s| s.to_string())
            .collect();

        // User-supplied extra patterns from config
        patterns.extend_from_slice(extra_patterns);

        // Global ~/.config/dps/syncignore
        if let Some(dir) = global_cfg_dir {
            let p = dir.join("syncignore");
            if p.exists() {
                patterns.extend(parse_ignore_file(&p));
            }
        }

        // Per-directory .syncignore inside the synced tree
        let local_ignore = local_path.join(".syncignore");
        if local_ignore.exists() {
            patterns.extend(parse_ignore_file(&local_ignore));
        }

        Self { patterns }
    }

    /// Emit `--exclude=PATTERN` args ready to pass to rsync.
    pub fn to_rsync_args(&self) -> Vec<String> {
        self.patterns
            .iter()
            .map(|p| format!("--exclude={p}"))
            .collect()
    }
}

// ── .syncignore parser ────────────────────────────────────────────────────────

/// Parse a gitignore-style file: skip blank lines and `#` comments.
pub fn parse_ignore_file(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => contents
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_owned)
            .collect(),
        Err(e) => {
            warn!("could not read {}: {e}", path.display());
            Vec::new()
        }
    }
}

// ── Machine fingerprint ───────────────────────────────────────────────────────

/// `(hostname, machine_id)` — both trimmed, may be empty on error.
pub fn machine_fingerprint() -> (String, String) {
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();

    let machine_id = std::fs::read_to_string("/etc/machine-id")
        .unwrap_or_default()
        .trim()
        .to_string();

    (hostname, machine_id)
}

/// Walk a directory and return files whose content contains the machine
/// hostname or machine-id.  Only UTF-8 text files ≤ 64 KiB are scanned.
/// Files are **not** automatically excluded — callers log a warning instead.
pub fn find_machine_specific_files(root: &Path) -> Vec<PathBuf> {
    let (hostname, machine_id) = machine_fingerprint();
    if hostname.is_empty() && machine_id.is_empty() {
        return Vec::new();
    }

    let mut found = Vec::new();
    scan_dir(root, &hostname, &machine_id, &mut found);
    found
}

fn scan_dir(dir: &Path, hostname: &str, machine_id: &str, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_dir() {
            // Skip known noisy directories to keep scanning fast.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(name, ".cache" | ".git" | ".local" | "node_modules" | "__pycache__") {
                continue;
            }
            scan_dir(&path, hostname, machine_id, found);
        } else if path.is_file() && contains_fingerprint(&path, hostname, machine_id) {
            found.push(path);
        }
    }
}

const MAX_SCAN_BYTES: u64 = 64 * 1024;

fn contains_fingerprint(path: &Path, hostname: &str, machine_id: &str) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if meta.len() > MAX_SCAN_BYTES {
        return false;
    }
    let Ok(bytes) = std::fs::read(path) else { return false };
    let Ok(text)  = std::str::from_utf8(&bytes) else { return false };
    (!hostname.is_empty()   && text.contains(hostname))
        || (!machine_id.is_empty() && text.contains(machine_id))
}
