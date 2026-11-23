//! Native PipeWire capture: a `Stream` in `Direction::Input` on its own OS
//! thread, feeding an async bounded channel of [`PcmFrame`]s.
//!
//! The realtime `process` callback never waits: it converts to interleaved
//! `i16`, offers the frame with `try_send`, and drops (counting) the frame when
//! the async consumer is behind.

use std::{
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use pipewire as pw;
use pw::{
    main_loop::MainLoopRc,
    properties::properties,
    spa::{self, pod::Pod, utils::Direction},
    stream::{StreamBox, StreamFlags, StreamState},
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::error::PipeWireError;
use crate::pcm::{convert_to_s16, NegotiatedFormat, PcmFrame, SampleFormat};
use crate::pw_common::{build_format_pod, parse_negotiated, ReadySlot, SharedError, StreamTarget};

/// Configuration for [`PipeWireAudioSource`].
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Default source, a named object, or a node id.
    pub target: StreamTarget,
    /// Stream/node name advertised in the graph.
    pub stream_name: String,
    /// Optional PipeWire remote name override (e.g. `pipewire-0`).
    pub remote: Option<String>,
    /// Requested rate. PipeWire converts the graph to this rate.
    pub sample_rate: u32,
    /// Requested channel count.
    pub channels: u16,
    /// Number of complete frames buffered between the realtime callback and
    /// `next_frame()`.  Larger smooths scheduling jitter; smaller bounds latency.
    pub queue_capacity: usize,
    /// Maximum time to wait for the first negotiated format.
    pub startup_timeout: Duration,
    /// Maximum time to wait for the PipeWire thread to exit during `stop`.
    pub stop_timeout: Duration,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            target: StreamTarget::Default,
            stream_name: "ozma-audio-capture".to_string(),
            remote: None,
            sample_rate: NegotiatedFormat::TARGET_RATE,
            channels: NegotiatedFormat::TARGET_CHANNELS,
            queue_capacity: 8,
            startup_timeout: Duration::from_secs(5),
            stop_timeout: Duration::from_secs(2),
        }
    }
}

/// Snapshot of capture counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureStats {
    /// Frames delivered to `next_frame()` / `try_next_frame()`.
    pub frames: u64,
    /// Total interleaved samples captured.
    pub samples: u64,
    /// Frames dropped because the async queue was full or closed.
    pub dropped_frames: u64,
    /// Dequeue/parse/conversion errors.
    pub errors: u64,
}

#[derive(Default)]
struct CaptureCounters {
    frames: AtomicU64,
    samples: AtomicU64,
    dropped_frames: AtomicU64,
    errors: AtomicU64,
}

impl CaptureCounters {
    fn snapshot(&self) -> CaptureStats {
        CaptureStats {
            frames: self.frames.load(Ordering::Relaxed),
            samples: self.samples.load(Ordering::Relaxed),
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

enum CaptureCommand {
    Quit,
}

struct CaptureUserData {
    format: Option<NegotiatedFormat>,
    sequence: u64,
    started: Instant,
    tx: mpsc::Sender<PcmFrame>,
    ready: Arc<ReadySlot>,
    shared: Arc<SharedError>,
    stats: Arc<CaptureCounters>,
    bound_node: Arc<AtomicU32>,
}

/// Native PipeWire audio capture stream.
///
/// The PipeWire main loop runs on a dedicated OS thread.  Dropping the source
/// (or calling [`stop`](Self::stop)) quits that loop and joins the thread.
pub struct PipeWireAudioSource {
    format: NegotiatedFormat,
    frames: mpsc::Receiver<PcmFrame>,
    cmd_tx: Option<pw::channel::Sender<CaptureCommand>>,
    done_rx: Option<std::sync::mpsc::Receiver<()>>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<CaptureCounters>,
    shared: Arc<SharedError>,
    bound_node: Arc<AtomicU32>,
    stop_timeout: Duration,
}

impl PipeWireAudioSource {
    /// Open the default source (or the configured target) and wait for format
    /// negotiation.
    pub async fn new(config: CaptureConfig) -> Result<Self, PipeWireError> {
        let startup_timeout = config.startup_timeout;
        let stop_timeout = config.stop_timeout;
        let target = config.target.clone();

        let (frame_tx, frames) = mpsc::channel::<PcmFrame>(config.queue_capacity.max(1));
        let (ready_tx, ready_rx) = oneshot::channel();
        let ready = ReadySlot::new(ready_tx);
        let (cmd_tx, cmd_rx) = pw::channel::channel::<CaptureCommand>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let stats = Arc::new(CaptureCounters::default());
        let shared = Arc::new(SharedError::default());
        let bound_node = Arc::new(AtomicU32::new(u32::MAX));

        let thread_stats = Arc::clone(&stats);
        let thread_shared = Arc::clone(&shared);
        let thread_bound = Arc::clone(&bound_node);
        let thread_ready = Arc::clone(&ready);

        let handle = thread::Builder::new()
            .name("ozma-pw-capture".into())
            .spawn(move || {
                capture_thread_main(
                    config,
                    frame_tx,
                    thread_ready,
                    cmd_rx,
                    thread_stats,
                    thread_shared,
                    thread_bound,
                );
                drop(done_tx);
            })
            .map_err(|e| PipeWireError::Thread {
                detail: e.to_string(),
            })?;

        let negotiated = match tokio::time::timeout(startup_timeout, ready_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(PipeWireError::disconnected(
                "capture thread exited before the stream negotiated a format",
            )),
            Err(_) => Err(PipeWireError::StartTimeout {
                detail: format!(
                    "no format negotiated for {} within {:?}",
                    target.describe(),
                    startup_timeout
                ),
            }),
        };

        match negotiated {
            Ok(format) => {
                debug!(
                    rate = format.sample_rate,
                    channels = format.channels,
                    sample_format = format.sample_format.label(),
                    target = %target.describe(),
                    "PipeWire capture ready"
                );
                Ok(Self {
                    format,
                    frames,
                    cmd_tx: Some(cmd_tx),
                    done_rx: Some(done_rx),
                    thread: Some(handle),
                    stats,
                    shared,
                    bound_node,
                    stop_timeout,
                })
            }
            Err(error) => {
                let _ = cmd_tx.send(CaptureCommand::Quit);
                let finished = matches!(
                    done_rx.recv_timeout(stop_timeout),
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
                );
                if finished {
                    let _ = handle.join();
                } else {
                    warn!("capture thread did not exit after startup failure; detaching");
                }
                Err(error)
            }
        }
    }

    /// The actual sample rate/channels/format negotiated with the graph.
    pub fn negotiated_format(&self) -> NegotiatedFormat {
        self.format
    }

    /// Graph node id this stream is connected to, once known.
    pub fn bound_node_id(&self) -> Option<u32> {
        match self.bound_node.load(Ordering::Relaxed) {
            u32::MAX => None,
            id => Some(id),
        }
    }

    pub fn stats(&self) -> CaptureStats {
        self.stats.snapshot()
    }

    /// Take the last fatal stream error, if any.
    pub fn take_error(&self) -> Option<PipeWireError> {
        self.shared.take()
    }

    /// Wait for the next captured frame.
    ///
    /// Returns [`PipeWireError::Disconnected`] if the stream stopped.  Use
    /// `tokio::time::timeout` at the call site when a bounded wait is needed.
    pub async fn next_frame(&mut self) -> Result<PcmFrame, PipeWireError> {
        match self.frames.recv().await {
            Some(frame) => Ok(frame),
            None => Err(self
                .shared
                .take()
                .unwrap_or_else(|| PipeWireError::disconnected("capture stream closed"))),
        }
    }

    /// Non-blocking variant of [`next_frame`](Self::next_frame).
    pub fn try_next_frame(&mut self) -> Result<Option<PcmFrame>, PipeWireError> {
        match self.frames.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(self
                .shared
                .take()
                .unwrap_or_else(|| PipeWireError::disconnected("capture stream closed"))),
        }
    }

    /// Quit the PipeWire main loop and join its thread.
    ///
    /// Safe to call more than once; also called by `Drop`.
    pub fn stop(&mut self) {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(CaptureCommand::Quit);
        }
        let finished = match self.done_rx.take() {
            Some(done) => matches!(
                done.recv_timeout(self.stop_timeout),
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            ),
            None => true,
        };
        if let Some(handle) = self.thread.take() {
            if finished {
                let _ = handle.join();
            } else {
                warn!(
                    "capture thread did not exit within {:?}; detaching",
                    self.stop_timeout
                );
            }
        }
    }
}

impl Drop for PipeWireAudioSource {
    fn drop(&mut self) {
        self.stop();
    }
}

const FLAGS: StreamFlags = StreamFlags::AUTOCONNECT
    .union(StreamFlags::MAP_BUFFERS)
    .union(StreamFlags::RT_PROCESS);

fn capture_thread_main(
    config: CaptureConfig,
    frame_tx: mpsc::Sender<PcmFrame>,
    ready: Arc<ReadySlot>,
    cmd_rx: pw::channel::Receiver<CaptureCommand>,
    stats: Arc<CaptureCounters>,
    shared: Arc<SharedError>,
    bound_node: Arc<AtomicU32>,
) {
    pw::init();

    let main_loop = match MainLoopRc::new(None) {
        Ok(main_loop) => main_loop,
        Err(e) => {
            let err = PipeWireError::unavailable(format!("create PipeWire main loop: {e}"));
            shared.set(err.clone());
            ready.send(Err(err));
            return;
        }
    };

    let quit_loop = main_loop.clone();
    let _cmd_listener = cmd_rx.attach(main_loop.loop_(), move |_| quit_loop.quit());

    let mut context_props = properties! {
        *pw::keys::APP_NAME => "ozma-pipewire",
    };
    if let Some(remote) = config.remote.as_deref() {
        context_props.insert(*pw::keys::REMOTE_NAME, remote);
    }
    let context = match pw::context::ContextRc::new(&main_loop, Some(context_props)) {
        Ok(context) => context,
        Err(e) => {
            let err = PipeWireError::unavailable(format!("create PipeWire context: {e}"));
            shared.set(err.clone());
            ready.send(Err(err));
            return;
        }
    };
    let core = match context.connect_rc(None) {
        Ok(core) => core,
        Err(e) => {
            let err = PipeWireError::unavailable(format!("connect to PipeWire: {e}"));
            shared.set(err.clone());
            ready.send(Err(err));
            return;
        }
    };

    let mut stream_props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => config.stream_name.as_str(),
        *pw::keys::NODE_DESCRIPTION => "Ozma audio capture",
        *pw::keys::APP_NAME => "ozma-pipewire",
    };
    if let Some(name) = config.target.object_name() {
        stream_props.insert("target.object", name);
    }

    let stream = match StreamBox::new(&core, &config.stream_name, stream_props) {
        Ok(stream) => stream,
        Err(e) => {
            let err = PipeWireError::stream(format!("create capture stream: {e}"));
            shared.set(err.clone());
            ready.send(Err(err));
            return;
        }
    };

    let user_data = CaptureUserData {
        format: None,
        sequence: 0,
        started: Instant::now(),
        tx: frame_tx,
        ready: Arc::clone(&ready),
        shared: Arc::clone(&shared),
        stats: Arc::clone(&stats),
        bound_node: Arc::clone(&bound_node),
    };

    let quit_on_closed = main_loop.clone();
    let proc_stats = Arc::clone(&stats);
    let proc_bound = Arc::clone(&bound_node);
    let _listener = match stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(move |stream, user_data, _old, new| match new {
            StreamState::Streaming => {
                user_data
                    .bound_node
                    .store(stream.node_id(), Ordering::Relaxed);
            }
            StreamState::Error(message) => {
                user_data.stats.errors.fetch_add(1, Ordering::Relaxed);
                let err = PipeWireError::stream_or_unavailable(message.clone());
                user_data.shared.set(err.clone());
                user_data.ready.send(Err(err));
            }
            _ => {}
        })
        .param_changed(move |stream, user_data, id, param| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            // `None` clears the format; the next `Some` renegotiates.
            let Some(param) = param else {
                user_data.format = None;
                return;
            };
            match parse_negotiated(param) {
                Ok(format) => {
                    user_data.format = Some(format);
                    user_data
                        .bound_node
                        .store(stream.node_id(), Ordering::Relaxed);
                    debug!(
                        rate = format.sample_rate,
                        channels = format.channels,
                        sample_format = format.sample_format.label(),
                        "capture format negotiated"
                    );
                    user_data.ready.send(Ok(format));
                }
                Err(e) => {
                    // Do not keep producing with a stale format.
                    user_data.format = None;
                    user_data.stats.errors.fetch_add(1, Ordering::Relaxed);
                    user_data.shared.set(e.clone());
                    user_data.ready.send(Err(e));
                }
            }
        })
        .process(move |stream, user_data| {
            process_capture(stream, user_data, &proc_stats, &proc_bound, &quit_on_closed);
        })
        .register()
    {
        Ok(listener) => listener,
        Err(e) => {
            let err = PipeWireError::stream(format!("register capture callbacks: {e}"));
            shared.set(err.clone());
            ready.send(Err(err));
            return;
        }
    };

    let formats = [
        build_format_pod(SampleFormat::S16Le, config.sample_rate, config.channels),
        build_format_pod(SampleFormat::F32Le, config.sample_rate, config.channels),
    ];
    let mut format_bytes = Vec::new();
    for format in formats {
        match format {
            Ok(bytes) => format_bytes.push(bytes),
            Err(e) => {
                shared.set(e.clone());
                ready.send(Err(e));
                return;
            }
        }
    }
    let mut params: Vec<&Pod> = Vec::new();
    for bytes in &format_bytes {
        match Pod::from_bytes(bytes) {
            Some(pod) => params.push(pod),
            None => {
                let err = PipeWireError::stream("invalid serialized format pod");
                shared.set(err.clone());
                ready.send(Err(err));
                return;
            }
        }
    }

    if let Err(e) = stream.connect(
        Direction::Input,
        config.target.node_id(),
        FLAGS,
        &mut params,
    ) {
        let err = PipeWireError::stream(format!("connect capture stream: {e}"));
        shared.set(err.clone());
        ready.send(Err(err));
        return;
    }

    main_loop.run();
}

fn process_capture(
    stream: &pw::stream::Stream,
    user_data: &mut CaptureUserData,
    stats: &CaptureCounters,
    bound_node: &AtomicU32,
    main_loop: &MainLoopRc,
) {
    let Some(format) = user_data.format else {
        return;
    };
    let node_id = stream.node_id();
    if node_id != u32::MAX {
        bound_node.store(node_id, Ordering::Relaxed);
    }
    let Some(mut buffer) = stream.dequeue_buffer() else {
        stats.errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else {
        stats.errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let offset = data.chunk().offset() as usize;
    let size = data.chunk().size() as usize;
    let Some(slice) = data.data() else {
        stats.errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Some(end) = offset.checked_add(size) else {
        stats.errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if offset > slice.len() || end > slice.len() {
        stats.errors.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let mut samples = match convert_to_s16(&slice[offset..end], format.sample_format) {
        Ok(samples) => samples,
        Err(_) => {
            stats.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let channels = format.channels as usize;
    if channels > 1 {
        samples.truncate(samples.len() / channels * channels);
    }
    if samples.is_empty() {
        return;
    }
    let sample_count = samples.len() as u64;
    let frame = PcmFrame {
        pts_micros: user_data.started.elapsed().as_micros() as u64,
        sequence: user_data.sequence,
        sample_rate: format.sample_rate,
        channels: format.channels,
        samples,
    };
    user_data.sequence = user_data.sequence.wrapping_add(1);

    match user_data.tx.try_send(frame) {
        Ok(()) => {
            stats.frames.fetch_add(1, Ordering::Relaxed);
            stats.samples.fetch_add(sample_count, Ordering::Relaxed);
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
            // The async owner is gone; stop capturing.
            main_loop.quit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_snapshot_without_a_daemon() {
        let counters = CaptureCounters::default();
        counters.frames.store(4, Ordering::Relaxed);
        counters.samples.store(4 * 2 * 480, Ordering::Relaxed);
        counters.dropped_frames.store(1, Ordering::Relaxed);
        counters.errors.store(2, Ordering::Relaxed);
        assert_eq!(
            counters.snapshot(),
            CaptureStats {
                frames: 4,
                samples: 4 * 2 * 480,
                dropped_frames: 1,
                errors: 2,
            }
        );
    }
}
