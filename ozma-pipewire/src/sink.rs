//! Native PipeWire playback: a `Stream` in `Direction::Output` on its own OS
//! thread, draining a bounded sample queue from the realtime `process`
//! callback and writing silence on underrun.

use std::{
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc, Mutex, TryLockError,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use pipewire as pw;
use pw::{
    main_loop::MainLoopRc,
    properties::properties,
    spa::{self, pod::Pod, utils::Direction},
    stream::{StreamBox, StreamFlags, StreamState},
};
use tokio::sync::oneshot;
use tracing::{debug, warn};

use crate::error::PipeWireError;
use crate::pcm::{NegotiatedFormat, PcmFrame, PcmQueue, SampleFormat};
use crate::pw_common::{build_format_pod, parse_negotiated, ReadySlot, SharedError, StreamTarget};

/// Configuration for [`PipeWireAudioSink`].
#[derive(Debug, Clone)]
pub struct SinkConfig {
    /// Default sink, a named object, or a node id.
    pub target: StreamTarget,
    pub stream_name: String,
    pub remote: Option<String>,
    pub sample_rate: u32,
    pub channels: u16,
    /// Bounded queue capacity in interleaved samples (not frames).
    pub queue_capacity_samples: usize,
    pub startup_timeout: Duration,
    pub stop_timeout: Duration,
}

impl Default for SinkConfig {
    fn default() -> Self {
        Self {
            target: StreamTarget::Default,
            stream_name: "ozma-audio-playback".to_string(),
            remote: None,
            sample_rate: NegotiatedFormat::TARGET_RATE,
            channels: NegotiatedFormat::TARGET_CHANNELS,
            // Two seconds of 48 kHz stereo.
            queue_capacity_samples: 48_000 * 2 * 2,
            startup_timeout: Duration::from_secs(5),
            stop_timeout: Duration::from_secs(2),
        }
    }
}

/// Snapshot of playback counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SinkStats {
    /// Interleaved samples currently buffered.
    pub queued_samples: u64,
    /// Total samples handed to PipeWire.
    pub played_samples: u64,
    /// Process callbacks that could not be filled completely.
    pub underruns: u64,
    /// Samples emitted as silence to cover underruns.
    pub silence_samples: u64,
    /// Total process callbacks.
    pub process_calls: u64,
    /// Callbacks that skipped the queue because the lock was busy/poisoned.
    pub lock_failures: u64,
    /// Buffer/format errors.
    pub errors: u64,
}

#[derive(Default)]
struct SinkCounters {
    played_samples: AtomicU64,
    underruns: AtomicU64,
    silence_samples: AtomicU64,
    process_calls: AtomicU64,
    lock_failures: AtomicU64,
    errors: AtomicU64,
}

impl SinkCounters {
    fn snapshot(&self, queued_samples: u64) -> SinkStats {
        SinkStats {
            queued_samples,
            played_samples: self.played_samples.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            silence_samples: self.silence_samples.load(Ordering::Relaxed),
            process_calls: self.process_calls.load(Ordering::Relaxed),
            lock_failures: self.lock_failures.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

struct SinkUserData {
    format: Option<NegotiatedFormat>,
    queue: Arc<Mutex<PcmQueue>>,
    negotiated: Arc<Mutex<NegotiatedFormat>>,
    ready: Arc<ReadySlot>,
    shared: Arc<SharedError>,
    stats: Arc<SinkCounters>,
    bound_node: Arc<AtomicU32>,
}

/// Native PipeWire audio playback stream.
pub struct PipeWireAudioSink {
    queue: Arc<Mutex<PcmQueue>>,
    negotiated: Arc<Mutex<NegotiatedFormat>>,
    cmd_tx: Option<pw::channel::Sender<SinkCommand>>,
    done_rx: Option<std::sync::mpsc::Receiver<()>>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<SinkCounters>,
    shared: Arc<SharedError>,
    bound_node: Arc<AtomicU32>,
    stop_timeout: Duration,
}

impl PipeWireAudioSink {
    /// Open the default sink (or the configured target) and wait for format
    /// negotiation.
    pub async fn new(config: SinkConfig) -> Result<Self, PipeWireError> {
        let startup_timeout = config.startup_timeout;
        let stop_timeout = config.stop_timeout;
        let target = config.target.clone();
        let capacity = config.queue_capacity_samples.max(1);

        let queue = Arc::new(Mutex::new(PcmQueue::with_capacity(capacity)));
        let requested =
            NegotiatedFormat::new(config.sample_rate, config.channels, SampleFormat::S16Le)?;
        let negotiated = Arc::new(Mutex::new(requested));

        let (ready_tx, ready_rx) = oneshot::channel();
        let ready = ReadySlot::new(ready_tx);
        let (cmd_tx, cmd_rx) = pw::channel::channel::<SinkCommand>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let stats = Arc::new(SinkCounters::default());
        let shared = Arc::new(SharedError::default());
        let bound_node = Arc::new(AtomicU32::new(u32::MAX));

        let thread_queue = Arc::clone(&queue);
        let thread_negotiated = Arc::clone(&negotiated);
        let thread_stats = Arc::clone(&stats);
        let thread_shared = Arc::clone(&shared);
        let thread_bound = Arc::clone(&bound_node);
        let thread_ready = Arc::clone(&ready);

        let handle = thread::Builder::new()
            .name("ozma-pw-playback".into())
            .spawn(move || {
                sink_thread_main(
                    config,
                    thread_queue,
                    thread_negotiated,
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

        let result = match tokio::time::timeout(startup_timeout, ready_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(PipeWireError::disconnected(
                "playback thread exited before the stream negotiated a format",
            )),
            Err(_) => Err(PipeWireError::StartTimeout {
                detail: format!(
                    "no format negotiated for {} within {:?}",
                    target.describe(),
                    startup_timeout
                ),
            }),
        };

        match result {
            Ok(format) => {
                if let Ok(mut slot) = negotiated.lock() {
                    *slot = format;
                }
                debug!(
                    rate = format.sample_rate,
                    channels = format.channels,
                    sample_format = format.sample_format.label(),
                    target = %target.describe(),
                    "PipeWire playback ready"
                );
                Ok(Self {
                    queue,
                    negotiated,
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
                let _ = cmd_tx.send(SinkCommand::Quit);
                let finished = matches!(
                    done_rx.recv_timeout(stop_timeout),
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
                );
                if finished {
                    let _ = handle.join();
                } else {
                    warn!("playback thread did not exit after startup failure; detaching");
                }
                Err(error)
            }
        }
    }

    /// The sample rate/channels/format currently negotiated with the graph.
    pub fn negotiated_format(&self) -> NegotiatedFormat {
        self.negotiated
            .lock()
            .map(|format| *format)
            .unwrap_or_else(|poisoned| *poisoned.into_inner())
    }

    pub fn bound_node_id(&self) -> Option<u32> {
        match self.bound_node.load(Ordering::Relaxed) {
            u32::MAX => None,
            id => Some(id),
        }
    }

    /// Push as many interleaved samples as fit, returning the accepted count.
    pub fn write(&self, samples: &[i16]) -> Result<usize, PipeWireError> {
        let mut queue = self.queue.lock().map_err(|_| PipeWireError::Poisoned)?;
        Ok(queue.push(samples))
    }

    /// Push all samples or fail without partially filling the queue.
    pub fn write_all(&self, samples: &[i16]) -> Result<(), PipeWireError> {
        let mut queue = self.queue.lock().map_err(|_| PipeWireError::Poisoned)?;
        let available = queue.available();
        if available < samples.len() {
            return Err(PipeWireError::QueueFull {
                capacity: queue.capacity(),
                requested: samples.len(),
                available,
            });
        }
        let accepted = queue.push(samples);
        debug_assert_eq!(accepted, samples.len());
        Ok(())
    }

    /// Queue a captured frame, rejecting rate/channel mismatches.
    ///
    /// Resampling is deliberately out of scope: the graph converts to the sink
    /// format, and callers with a different rate should convert explicitly.
    pub fn play(&self, frame: PcmFrame) -> Result<(), PipeWireError> {
        let format = self.negotiated_format();
        if frame.sample_rate != format.sample_rate || frame.channels != format.channels {
            return Err(PipeWireError::format_mismatch(
                format!("{} Hz / {} ch", frame.sample_rate, frame.channels),
                format!("{} Hz / {} ch", format.sample_rate, format.channels),
            ));
        }
        if !frame.is_aligned() {
            return Err(PipeWireError::format_mismatch(
                format!(
                    "{} samples for {} channels",
                    frame.samples.len(),
                    frame.channels
                ),
                "whole interleaved frames",
            ));
        }
        self.write_all(&frame.samples)
    }

    /// Discard all queued samples.
    pub fn flush(&self) -> Result<(), PipeWireError> {
        let mut queue = self.queue.lock().map_err(|_| PipeWireError::Poisoned)?;
        queue.clear();
        Ok(())
    }

    pub fn queued_samples(&self) -> usize {
        self.queue
            .lock()
            .map(|queue| queue.len())
            .unwrap_or_else(|poisoned| poisoned.into_inner().len())
    }

    pub fn queue_capacity(&self) -> usize {
        self.queue
            .lock()
            .map(|queue| queue.capacity())
            .unwrap_or_else(|poisoned| poisoned.into_inner().capacity())
    }

    /// Take the last fatal stream error, if any.
    pub fn take_error(&self) -> Option<PipeWireError> {
        self.shared.take()
    }

    pub fn stats(&self) -> SinkStats {
        self.stats.snapshot(self.queued_samples() as u64)
    }

    /// Quit the PipeWire main loop and join its thread.
    pub fn stop(&mut self) {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(SinkCommand::Quit);
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
                    "playback thread did not exit within {:?}; detaching",
                    self.stop_timeout
                );
            }
        }
    }
}

impl Drop for PipeWireAudioSink {
    fn drop(&mut self) {
        self.stop();
    }
}

enum SinkCommand {
    Quit,
}

const FLAGS: StreamFlags = StreamFlags::AUTOCONNECT
    .union(StreamFlags::MAP_BUFFERS)
    .union(StreamFlags::RT_PROCESS);

fn sink_thread_main(
    config: SinkConfig,
    queue: Arc<Mutex<PcmQueue>>,
    negotiated: Arc<Mutex<NegotiatedFormat>>,
    ready: Arc<ReadySlot>,
    cmd_rx: pw::channel::Receiver<SinkCommand>,
    stats: Arc<SinkCounters>,
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
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => config.stream_name.as_str(),
        *pw::keys::NODE_DESCRIPTION => "Ozma audio playback",
        *pw::keys::APP_NAME => "ozma-pipewire",
        *pw::keys::AUDIO_CHANNELS => config.channels.to_string(),
    };
    if let Some(name) = config.target.object_name() {
        stream_props.insert("target.object", name);
    }

    let stream = match StreamBox::new(&core, &config.stream_name, stream_props) {
        Ok(stream) => stream,
        Err(e) => {
            let err = PipeWireError::stream(format!("create playback stream: {e}"));
            shared.set(err.clone());
            ready.send(Err(err));
            return;
        }
    };

    let user_data = SinkUserData {
        format: None,
        queue,
        negotiated,
        ready: Arc::clone(&ready),
        shared: Arc::clone(&shared),
        stats: Arc::clone(&stats),
        bound_node: Arc::clone(&bound_node),
    };

    let quit_on_drop = main_loop.clone();
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
            let Some(param) = param else {
                user_data.format = None;
                return;
            };
            match parse_negotiated(param) {
                Ok(format) => {
                    user_data.format = Some(format);
                    if let Ok(mut slot) = user_data.negotiated.lock() {
                        *slot = format;
                    }
                    user_data
                        .bound_node
                        .store(stream.node_id(), Ordering::Relaxed);
                    debug!(
                        rate = format.sample_rate,
                        channels = format.channels,
                        sample_format = format.sample_format.label(),
                        "playback format negotiated"
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
            process_sink(stream, user_data, &quit_on_drop);
        })
        .register()
    {
        Ok(listener) => listener,
        Err(e) => {
            let err = PipeWireError::stream(format!("register playback callbacks: {e}"));
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
        Direction::Output,
        config.target.node_id(),
        FLAGS,
        &mut params,
    ) {
        let err = PipeWireError::stream(format!("connect playback stream: {e}"));
        shared.set(err.clone());
        ready.send(Err(err));
        return;
    }

    main_loop.run();
}

fn process_sink(
    stream: &pw::stream::Stream,
    user_data: &mut SinkUserData,
    _main_loop: &MainLoopRc,
) {
    let stats = &user_data.stats;
    stats.process_calls.fetch_add(1, Ordering::Relaxed);
    let Some(format) = user_data.format else {
        return;
    };

    let Some(mut buffer) = stream.dequeue_buffer() else {
        stats.errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else {
        stats.errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Some(slice) = data.data() else {
        stats.errors.fetch_add(1, Ordering::Relaxed);
        return;
    };

    let bytes_per_sample = format.sample_format.bytes_per_sample();
    let stride = format.frame_stride();
    if stride == 0 || slice.is_empty() {
        return;
    }
    let total_samples = slice.len() / bytes_per_sample;
    let total_bytes = slice.len();

    // Silence first; any samples we cannot take from the queue stay zero.
    slice.fill(0);

    let mut written = 0usize;
    match user_data.queue.try_lock() {
        Ok(mut queue) => {
            while written < total_samples {
                let Some(sample) = queue.pop_one() else {
                    break;
                };
                let start = written * bytes_per_sample;
                let end = start + bytes_per_sample;
                match format.sample_format {
                    SampleFormat::S16Le => {
                        slice[start..end].copy_from_slice(&sample.to_le_bytes());
                    }
                    SampleFormat::F32Le => {
                        let f = sample as f32 / 32_768.0;
                        slice[start..end].copy_from_slice(&f.to_le_bytes());
                    }
                }
                written += 1;
            }
        }
        Err(TryLockError::WouldBlock) => {
            stats.lock_failures.fetch_add(1, Ordering::Relaxed);
        }
        Err(TryLockError::Poisoned(_)) => {
            stats.lock_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    let missing = total_samples.saturating_sub(written);
    if missing > 0 {
        stats.underruns.fetch_add(1, Ordering::Relaxed);
        stats
            .silence_samples
            .fetch_add(missing as u64, Ordering::Relaxed);
    }
    stats
        .played_samples
        .fetch_add(written as u64, Ordering::Relaxed);

    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = stride as i32;
    // The buffer is fully written: real samples followed by silence.
    *chunk.size_mut() = total_bytes as u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_snapshot_without_a_daemon() {
        let counters = SinkCounters::default();
        counters.played_samples.store(96_000, Ordering::Relaxed);
        counters.underruns.store(3, Ordering::Relaxed);
        counters.silence_samples.store(4_096, Ordering::Relaxed);
        counters.process_calls.store(12, Ordering::Relaxed);
        counters.lock_failures.store(1, Ordering::Relaxed);
        counters.errors.store(2, Ordering::Relaxed);

        let stats = counters.snapshot(480);
        assert_eq!(stats.queued_samples, 480);
        assert_eq!(stats.played_samples, 96_000);
        assert_eq!(stats.underruns, 3);
        assert_eq!(stats.silence_samples, 4_096);
        assert_eq!(stats.process_calls, 12);
        assert_eq!(stats.lock_failures, 1);
        assert_eq!(stats.errors, 2);
    }
}
