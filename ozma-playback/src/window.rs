// SPDX-License-Identifier: AGPL-3.0-only
//! A `minifb` window video sink, compiled only with `--features window`.
//!
//! This is a convenience for native clients and manual bring-up, not a
//! renderer. It converts each I420 frame to packed RGB and hands it to
//! `minifb`; scaling, letterboxing, and window management stay with the desktop
//! client. The default build of `ozma-playback` never links a display server.
//!
//! `minifb::Window` is not `Send`: windowing APIs generally require the window
//! to be created, updated, and dropped on one thread. [`WindowVideoSink`] is
//! therefore a handle to a dedicated window thread. That keeps the public sink
//! `Send`, so [`PlaybackEngine`](crate::PlaybackEngine) can own it like any
//! other sink, without moving a window handle across threads.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use minifb::{Key, Window, WindowOptions};
use ozma_decode::DecodedVideoFrame;
use tracing::{debug, info, warn};

use crate::error::PlaybackError;
use crate::video::VideoSink;

/// How long construction waits for the window thread to create its window.
const WINDOW_START_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a flush waits for the window thread to acknowledge.
const WINDOW_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// Commands accepted by the window thread.
enum WindowCommand {
    Frame(DecodedVideoFrame),
    Flush(std_mpsc::SyncSender<()>),
    Stop,
}

/// A video sink that presents frames in a `minifb` window.
pub struct WindowVideoSink {
    commands: Option<std_mpsc::SyncSender<WindowCommand>>,
    worker: Option<JoinHandle<()>>,
    frames_presented: Arc<AtomicU64>,
    frames_skipped: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    stopped: bool,
}

impl std::fmt::Debug for WindowVideoSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowVideoSink")
            .field("frames_presented", &self.frames_presented())
            .field("frames_skipped", &self.frames_skipped())
            .field("stopped", &self.stopped)
            .finish()
    }
}

impl WindowVideoSink {
    /// Open a window with the given title and initial size on a dedicated
    /// thread.
    pub fn new(title: &str, width: usize, height: usize) -> Result<Self, PlaybackError> {
        Self::with_options(title, width, height, WindowOptions::default())
    }

    /// Open a window with explicit `minifb` options.
    ///
    /// Returns [`PlaybackError::WindowUnavailable`] when no display server is
    /// reachable or the window cannot be created; the call is bounded by a
    /// five-second startup timeout.
    pub fn with_options(
        title: &str,
        width: usize,
        height: usize,
        options: WindowOptions,
    ) -> Result<Self, PlaybackError> {
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let (command_tx, command_rx) = std_mpsc::sync_channel(2);
        let frames_presented = Arc::new(AtomicU64::new(0));
        let frames_skipped = Arc::new(AtomicU64::new(0));
        let closed = Arc::new(AtomicBool::new(false));

        let title = title.to_owned();
        let worker_presented = frames_presented.clone();
        let worker_skipped = frames_skipped.clone();
        let worker_closed = closed.clone();
        let worker = std::thread::Builder::new()
            .name("ozma-playback-window".to_owned())
            .spawn(move || {
                window_worker(
                    title,
                    width,
                    height,
                    options,
                    ready_tx,
                    command_rx,
                    worker_presented,
                    worker_skipped,
                    worker_closed,
                )
            })
            .map_err(|error| {
                PlaybackError::window_unavailable(format!(
                    "could not start the window thread: {error}"
                ))
            })?;

        match ready_rx.recv_timeout(WINDOW_START_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                commands: Some(command_tx),
                worker: Some(worker),
                frames_presented,
                frames_skipped,
                closed,
                stopped: false,
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                // A wedged display server must not block construction forever.
                // Dropping the command sender asks the thread to exit after it
                // returns from `Window::new`.
                Err(PlaybackError::window_unavailable(
                    "timed out creating the window",
                ))
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                let _ = worker.join();
                Err(PlaybackError::window_unavailable(
                    "the window thread exited during setup",
                ))
            }
        }
    }

    /// Number of frames handed to the window.
    pub fn frames_presented(&self) -> u64 {
        self.frames_presented.load(Ordering::Relaxed)
    }

    /// Number of frames skipped because they had no RGB output.
    pub fn frames_skipped(&self) -> u64 {
        self.frames_skipped.load(Ordering::Relaxed)
    }

    /// True when the window was closed by the user or the display server.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

impl VideoSink for WindowVideoSink {
    fn present(&mut self, frame: &DecodedVideoFrame) -> Result<(), PlaybackError> {
        if self.stopped {
            return Err(PlaybackError::Stopped);
        }
        if self.is_closed() {
            return Err(PlaybackError::window_unavailable(
                "the window was closed or Escape was pressed",
            ));
        }
        let Some(commands) = self.commands.as_ref() else {
            return Err(PlaybackError::Stopped);
        };
        match commands.try_send(WindowCommand::Frame(frame.clone())) {
            Ok(()) => Ok(()),
            Err(std_mpsc::TrySendError::Full(_)) => Err(PlaybackError::busy(
                "window",
                "the window is behind on presentation",
            )),
            Err(std_mpsc::TrySendError::Disconnected(_)) => {
                self.closed.store(true, Ordering::Relaxed);
                Err(PlaybackError::window_unavailable(
                    "the window thread is gone",
                ))
            }
        }
    }

    fn flush(&mut self) -> Result<(), PlaybackError> {
        if self.stopped {
            return Ok(());
        }
        let Some(commands) = self.commands.as_ref() else {
            return Ok(());
        };
        let (ack_tx, ack_rx) = std_mpsc::sync_channel(1);
        if commands.send(WindowCommand::Flush(ack_tx)).is_err() {
            return Err(PlaybackError::window_unavailable(
                "the window thread is gone",
            ));
        }
        match ack_rx.recv_timeout(WINDOW_FLUSH_TIMEOUT) {
            Ok(()) => Ok(()),
            Err(std_mpsc::RecvTimeoutError::Timeout) => Err(PlaybackError::busy(
                "window",
                "the window did not flush in time",
            )),
            Err(std_mpsc::RecvTimeoutError::Disconnected) => Err(
                PlaybackError::window_unavailable("the window thread stopped before flushing"),
            ),
        }
    }

    fn stop(&mut self) -> Result<(), PlaybackError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(WindowCommand::Stop);
        }
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| {
                PlaybackError::window_unavailable("the window thread panicked while stopping")
            })?;
        }
        Ok(())
    }

    fn is_open(&self) -> bool {
        !self.stopped && !self.is_closed()
    }
}

impl Drop for WindowVideoSink {
    fn drop(&mut self) {
        // Dropping the sender makes the worker exit; do not join on drop, so a
        // wedged display server cannot hang the caller.
        let _ = self.commands.take();
        let _ = self.worker.take();
    }
}

#[allow(clippy::too_many_arguments)]
fn window_worker(
    title: String,
    width: usize,
    height: usize,
    options: WindowOptions,
    ready: std_mpsc::SyncSender<Result<(), PlaybackError>>,
    commands: std_mpsc::Receiver<WindowCommand>,
    frames_presented: Arc<AtomicU64>,
    frames_skipped: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
) {
    let mut window = match Window::new(&title, width, height, options) {
        Ok(window) => window,
        Err(error) => {
            let _ = ready.send(Err(PlaybackError::window_unavailable(format!(
                "cannot open window: {error}"
            ))));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    info!(width, height, "playback window opened");

    let mut buffer: Vec<u32> = Vec::new();
    loop {
        match commands.recv() {
            Ok(WindowCommand::Frame(frame)) => {
                if !window.is_open() || window.is_key_down(Key::Escape) {
                    closed.store(true, Ordering::Relaxed);
                    break;
                }
                let rgb = frame.to_rgb();
                if rgb.is_empty() {
                    frames_skipped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                buffer.clear();
                buffer.reserve(rgb.len() / 3);
                for pixel in rgb.chunks_exact(3) {
                    buffer.push(
                        (u32::from(pixel[0]) << 16)
                            | (u32::from(pixel[1]) << 8)
                            | u32::from(pixel[2]),
                    );
                }
                let width = usize::try_from(frame.width).unwrap_or(0);
                let height = usize::try_from(frame.height).unwrap_or(0);
                if let Err(error) = window.update_with_buffer(&buffer, width, height) {
                    warn!(%error, "minifb could not present a frame");
                    closed.store(true, Ordering::Relaxed);
                    break;
                }
                frames_presented.fetch_add(1, Ordering::Relaxed);
            }
            Ok(WindowCommand::Flush(ack)) => {
                let _ = ack.send(());
            }
            Ok(WindowCommand::Stop) | Err(_) => break,
        }
    }

    closed.store(true, Ordering::Relaxed);
    debug!("playback window closed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_decode::{DecodeMeta, I420Planes};

    fn frame(width: u32, height: u32) -> DecodedVideoFrame {
        let luma = usize::try_from(width * height).unwrap();
        let chroma = usize::try_from(width.div_ceil(2) * height.div_ceil(2)).unwrap();
        DecodedVideoFrame::new(
            DecodeMeta::new(0, 0, true),
            width,
            height,
            I420Planes::new(vec![16; luma], vec![128; chroma], vec![128; chroma]),
        )
        .unwrap()
    }

    /// Without a display server there is nothing to assert; the point of this
    /// test is that the feature builds and that a window failure is structured.
    #[test]
    fn window_sink_presents_or_skips_without_a_display() {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            eprintln!("skipping window sink test: no DISPLAY/WAYLAND_DISPLAY");
            return;
        }

        let mut sink = match WindowVideoSink::new("ozma-playback-test", 64, 32) {
            Ok(sink) => sink,
            Err(error) => {
                eprintln!("skipping window sink test: {error}");
                assert!(!error.to_string().is_empty());
                return;
            }
        };
        sink.present(&frame(64, 32)).expect("queue first frame");
        sink.flush().unwrap();
        // The window thread may have presented by now; a busy loop would make
        // this test dependent on compositor scheduling, so only require that
        // the frame was accepted or explicitly reported busy.
        sink.stop().unwrap();
        sink.stop().unwrap();
        assert!(!sink.is_open());
        assert_eq!(
            sink.present(&frame(64, 32)).unwrap_err(),
            PlaybackError::Stopped
        );
    }
}
