//! File watcher: 1s debounced notifications for changes anywhere under
//! `crates/`. Any workspace crate's source affects the BE binary, so the
//! watcher signals on the whole tree — `app.rs` recomputes `source_version`
//! on each event so the footer's restart-pending check stays current.

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

    Ok(WatcherHandle { _debouncer: Box::new(debouncer) })
}
