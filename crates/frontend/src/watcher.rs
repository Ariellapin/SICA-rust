//! File watchers, 1 s debounced.
//!
//! Two trees are watched and both report on the same channel:
//!
//! - **`crates/`** — any workspace crate's source affects the BE binary, so
//!   the watcher signals on the whole tree and `app.rs` recomputes
//!   `source_version` on each event, keeping the footer's restart-pending
//!   check current.
//! - **`sica-settings.json` and `sica-settings/`** (harness §14.6, UI
//!   §7.2) — an external edit of the settings document, a provider TOML or
//!   an MCP file is applied without a restart, so "Open configuration file"
//!   round-trips: edit, save, see it. `app.rs` tells the two apart by path.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use tokio::sync::mpsc;

use crate::child::workspace_root;

const DEBOUNCE: Duration = Duration::from_millis(1000);

pub struct WatcherHandle {
    _debouncer: Box<dyn std::any::Any + Send>,
}

pub fn start(tx: mpsc::UnboundedSender<Vec<PathBuf>>) -> Result<WatcherHandle> {
    let root = workspace_root()?;
    let watch_dir = root.join("crates");

    let mut debouncer = new_debouncer(DEBOUNCE, move |res: notify_debouncer_mini::DebounceEventResult| {
        match res {
            Ok(events) => {
                let paths: Vec<PathBuf> = events.into_iter().map(|e| e.path).collect();
                if !paths.is_empty() {
                    let _ = tx.send(paths);
                }
            }
            Err(e) => {
                eprintln!("watcher error: {e:?}");
            }
        }
    })
    .context("create debouncer")?;

    debouncer
        .watcher()
        .watch(&watch_dir, RecursiveMode::Recursive)
        .with_context(|| format!("watch {}", watch_dir.display()))?;

    // The settings document and the folder of per-provider / per-server
    // files beside it. Both are created by the app on first run, so a
    // missing one is not fatal — it is watched once it exists on the next
    // start, and the app's own writes keep working meanwhile.
    let settings = sica_core::paths::settings_file();
    if settings.is_file() {
        if let Err(e) = debouncer.watcher().watch(&settings, RecursiveMode::NonRecursive) {
            eprintln!("watcher: settings file not watched: {e}");
        }
    }
    let settings_dir = sica_core::paths::settings_dir();
    if settings_dir.is_dir() {
        if let Err(e) = debouncer.watcher().watch(&settings_dir, RecursiveMode::Recursive) {
            eprintln!("watcher: settings dir not watched: {e}");
        }
    }

    Ok(WatcherHandle { _debouncer: Box::new(debouncer) })
}

/// Whether a watcher path belongs to the settings surface rather than to
/// the source tree.
pub fn is_settings_path(path: &std::path::Path) -> bool {
    path == sica_core::paths::settings_file() || path.starts_with(sica_core::paths::settings_dir())
}
