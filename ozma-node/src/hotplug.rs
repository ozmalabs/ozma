//! V4L2 device hot-plug detection via `notify` (inotify on Linux).
//!
//! Watches `/dev` for `CREATE`/`REMOVE` events on `videoN` nodes and
//! sends [`HotplugEvent`]s on a tokio channel so the capture manager can react.

use std::path::Path;

use anyhow::Result;
use notify::{
    event::{CreateKind, EventKind, RemoveKind},
    recommended_watcher, Event, RecursiveMode, Watcher,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotplugEvent {
    Added(String),
    Removed(String),
}

/// Spawn a background task that sends [`HotplugEvent`]s on `tx` whenever a
/// `/dev/videoN` node is created or removed.
pub async fn watch_v4l2_hotplug(tx: mpsc::Sender<HotplugEvent>) -> Result<()> {
    let (sync_tx, mut sync_rx) = mpsc::channel::<notify::Result<Event>>(64);

    let mut watcher = recommended_watcher(move |res| {
        let _ = sync_tx.blocking_send(res);
    })?;

    watcher.watch(Path::new("/dev"), RecursiveMode::NonRecursive)?;
    info!("Watching /dev for V4L2 hot-plug events");

    tokio::spawn(async move {
        // Keep `watcher` alive for the duration of the task.
        let _watcher = watcher;

        while let Some(res) = sync_rx.recv().await {
            match res {
                Ok(event) => {
                    for path in &event.paths {
                        let name = match path.file_name().and_then(|n| n.to_str()) {
                            Some(n) => n.to_string(),
                            None => continue,
                        };
                        if !name.starts_with("video") {
                            continue;
                        }
                        let path_str = path.to_string_lossy().into_owned();
                        let hp = match event.kind {
                            EventKind::Create(CreateKind::Any)
                            | EventKind::Create(CreateKind::File)
                            | EventKind::Create(CreateKind::Other) => {
                                info!("V4L2 device added: {path_str}");
                                HotplugEvent::Added(path_str)
                            }
                            EventKind::Remove(RemoveKind::Any)
                            | EventKind::Remove(RemoveKind::File)
                            | EventKind::Remove(RemoveKind::Other) => {
                                info!("V4L2 device removed: {path_str}");
                                HotplugEvent::Removed(path_str)
                            }
                            other => {
                                debug!("Ignoring notify event {other:?} for {path_str}");
                                continue;
                            }
                        };
                        if tx.send(hp).await.is_err() {
                            return; // receiver dropped
                        }
                    }
                }
                Err(e) => warn!("notify error: {e}"),
            }
        }
    });

    Ok(())
}
