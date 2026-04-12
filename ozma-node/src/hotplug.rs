//! V4L2 device hot-plug detection via inotify (Linux).
//!
//! Watches `/dev` for `video*` create/delete events and notifies the caller
//! via a tokio channel so the capture pipeline can be restarted.

use std::path::PathBuf;

use anyhow::Result;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum HotplugEvent {
    Added(PathBuf),
    Removed(PathBuf),
}

/// Start watching `/dev` for V4L2 device changes.
///
/// Returns `(watcher, receiver)`.  Keep the watcher alive for as long as you
/// want events — dropping it stops the watch.
pub fn watch_v4l2_devices() -> Result<(RecommendedWatcher, mpsc::Receiver<HotplugEvent>)> {
    let (tx, rx) = mpsc::channel::<HotplugEvent>(32);

    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };

            for path in event.paths {
                let is_video = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("video"))
                    .unwrap_or(false);
                if !is_video {
                    continue;
                }

                let hp = match event.kind {
                    EventKind::Create(_) => HotplugEvent::Added(path),
                    EventKind::Remove(_) => HotplugEvent::Removed(path),
                    _ => continue,
                };

                // Non-blocking send; silently drop if the receiver is gone.
                let _ = tx.try_send(hp);
            }
        },
        Config::default(),
    )?;

    watcher.watch(std::path::Path::new("/dev"), RecursiveMode::NonRecursive)?;

    Ok((watcher, rx))
}
