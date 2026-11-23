// SPDX-License-Identifier: AGPL-3.0-only
//! A small playback engine that decouples producers from sinks.
//!
//! Decode and transport should never wait for a display or a sound card.
//! [`PlaybackEngine`] owns a [`VideoSink`] and an optional [`AudioSink`] on
//! dedicated threads and accepts decoded frames through bounded queues:
//!
//! * video is **lossy**: when its queue is full, the oldest frame is dropped and
//!   counted, so control and input keep moving while a slow renderer catches up;
//! * audio is **bounded but not silently lossy**: a full audio queue rejects the
//!   new block with [`PlaybackError::QueueFull`], and the engine counts the drop
//!   so the client can report it.
//!
//! The engine is synchronous and cheap to call: `play_video` and `play_audio`
//! only take a short queue lock. A client on an async runtime can call them
//! directly or wrap the whole engine in a blocking task.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use ozma_decode::{DecodedAudio, DecodedVideoFrame};
use tracing::warn;

use crate::audio::AudioSink;
use crate::error::PlaybackError;
use crate::video::VideoSink;

/// Queue sizes for [`PlaybackEngine`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    /// Frames buffered for the video sink. Two frames absorb one slow present
    /// without adding visible latency.
    pub video_capacity: usize,
    /// Decoded audio blocks buffered for the audio sink. Eight 20 ms blocks is
    /// 160 ms.
    pub audio_capacity: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            video_capacity: 2,
            audio_capacity: 8,
        }
    }
}

impl EngineConfig {
    fn normalized(self) -> Self {
        Self {
            video_capacity: self.video_capacity.max(1),
            audio_capacity: self.audio_capacity.max(1),
        }
    }
}

/// Counters for one [`PlaybackEngine`].
///
/// `video_presented` and `audio_played` count blocks a sink accepted.
/// `video_dropped` counts frames evicted because the video queue was full.
/// `audio_dropped` counts blocks rejected because the audio queue was full.
/// `errors` counts sink failures. No counter wraps; they saturate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaybackStats {
    /// Video frames accepted by the video sink.
    pub video_presented: u64,
    /// Video frames evicted before presentation.
    pub video_dropped: u64,
    /// Audio blocks accepted by the audio sink.
    pub audio_played: u64,
    /// Audio blocks rejected because the queue was full.
    pub audio_dropped: u64,
    /// Sink calls that returned an error.
    pub errors: u64,
}

impl PlaybackStats {
    /// A zeroed counter set.
    pub const fn new() -> Self {
        Self {
            video_presented: 0,
            video_dropped: 0,
            audio_played: 0,
            audio_dropped: 0,
            errors: 0,
        }
    }

    /// True when nothing has been presented, dropped, or failed.
    pub const fn is_empty(&self) -> bool {
        self.video_presented == 0
            && self.video_dropped == 0
            && self.audio_played == 0
            && self.audio_dropped == 0
            && self.errors == 0
    }

    /// Total blocks dropped (video and audio).
    pub const fn drops(&self) -> u64 {
        self.video_dropped.saturating_add(self.audio_dropped)
    }
}

#[derive(Default)]
struct Counters {
    video_presented: AtomicU64,
    video_dropped: AtomicU64,
    audio_played: AtomicU64,
    audio_dropped: AtomicU64,
    errors: AtomicU64,
}

impl Counters {
    fn increment(counter: &AtomicU64) {
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            Some(value.saturating_add(1))
        });
    }

    fn snapshot(&self) -> PlaybackStats {
        PlaybackStats {
            video_presented: self.video_presented.load(Ordering::Relaxed),
            video_dropped: self.video_dropped.load(Ordering::Relaxed),
            audio_played: self.audio_played.load(Ordering::Relaxed),
            audio_dropped: self.audio_dropped.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

struct QueueState<T> {
    items: VecDeque<T>,
    capacity: usize,
    closed: bool,
}

/// A bounded queue with an explicit overflow policy at each call site.
struct Queue<T> {
    state: Mutex<QueueState<T>>,
    ready: Condvar,
}

impl<T> Queue<T> {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(QueueState {
                items: VecDeque::new(),
                capacity: capacity.max(1),
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    /// Push, evicting the oldest item when full. Returns whether an item was
    /// evicted, or the item back if the queue is closed.
    fn push_drop_oldest(&self, item: T) -> Result<bool, T> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.closed {
            return Err(item);
        }
        let dropped = if state.items.len() >= state.capacity {
            state.items.pop_front();
            true
        } else {
            false
        };
        state.items.push_back(item);
        self.ready.notify_one();
        Ok(dropped)
    }

    /// Push, rejecting the item when full.
    fn push_reject(&self, item: T) -> Result<(), T> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.closed || state.items.len() >= state.capacity {
            return Err(item);
        }
        state.items.push_back(item);
        self.ready.notify_one();
        Ok(())
    }

    /// Pop the oldest item, waiting while the queue is empty and open.
    fn pop(&self) -> Option<T> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        loop {
            if let Some(item) = state.items.pop_front() {
                return Some(item);
            }
            if state.closed {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.closed = true;
        self.ready.notify_all();
    }

    fn capacity(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .capacity
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        // Dropping the engine without an explicit stop must still wake a worker.
        self.close();
    }
}

/// Owns playback sinks and feeds them from bounded queues on worker threads.
///
/// Construct with [`PlaybackEngine::new`] or [`PlaybackEngine::with_config`].
/// Both open the sinks (audio may fail with a device error) and start one
/// worker thread per sink. Call [`PlaybackEngine::stop`] for a joined shutdown;
/// dropping the engine also stops it, ignoring any sink error.
pub struct PlaybackEngine {
    video: Option<Arc<Queue<DecodedVideoFrame>>>,
    audio: Option<Arc<Queue<DecodedAudio>>>,
    counters: Arc<Counters>,
    video_worker: Option<JoinHandle<Result<(), PlaybackError>>>,
    audio_worker: Option<JoinHandle<Result<(), PlaybackError>>>,
    config: EngineConfig,
    stopped: bool,
}

impl std::fmt::Debug for PlaybackEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaybackEngine")
            .field("config", &self.config)
            .field("stopped", &self.stopped)
            .field("stats", &self.stats())
            .finish()
    }
}

impl PlaybackEngine {
    /// Start an engine with default queue sizes.
    pub fn new(
        video: Box<dyn VideoSink>,
        audio: Option<Box<dyn AudioSink>>,
    ) -> Result<Self, PlaybackError> {
        Self::with_config(video, audio, EngineConfig::default())
    }

    /// Start an engine with explicit queue sizes.
    pub fn with_config(
        mut video: Box<dyn VideoSink>,
        mut audio: Option<Box<dyn AudioSink>>,
        config: EngineConfig,
    ) -> Result<Self, PlaybackError> {
        let config = config.normalized();
        video.open()?;
        if let Some(audio) = audio.as_mut() {
            audio.open()?;
        }

        let counters = Arc::new(Counters::default());
        let video_queue = Arc::new(Queue::new(config.video_capacity));

        let video_counters = counters.clone();
        let video_queue_for_worker = video_queue.clone();
        let video_worker = std::thread::Builder::new()
            .name("ozma-playback-video".to_owned())
            .spawn(move || -> Result<(), PlaybackError> {
                let mut sink = video;
                while let Some(frame) = video_queue_for_worker.pop() {
                    match sink.present(&frame) {
                        Ok(()) => {
                            Counters::increment(&video_counters.video_presented);
                        }
                        Err(error) => {
                            Counters::increment(&video_counters.errors);
                            warn!(
                                code = error.code(),
                                %error,
                                "video sink rejected a frame"
                            );
                        }
                    }
                }
                let flush = sink.flush();
                let stop = sink.stop();
                flush.and(stop)
            })
            .map_err(|error| {
                PlaybackError::worker(format!("could not start the video worker: {error}"))
            })?;

        let (audio_queue, audio_worker) = if let Some(mut sink) = audio.take() {
            let queue = Arc::new(Queue::new(config.audio_capacity));
            let audio_counters = counters.clone();
            let audio_queue_for_worker = queue.clone();
            let worker = std::thread::Builder::new()
                .name("ozma-playback-audio".to_owned())
                .spawn(move || -> Result<(), PlaybackError> {
                    while let Some(block) = audio_queue_for_worker.pop() {
                        match sink.play(&block) {
                            Ok(()) => {
                                Counters::increment(&audio_counters.audio_played);
                            }
                            Err(error) => {
                                Counters::increment(&audio_counters.errors);
                                warn!(
                                    code = error.code(),
                                    %error,
                                    "audio sink rejected a block"
                                );
                            }
                        }
                    }
                    let flush = sink.flush();
                    let stop = sink.stop();
                    flush.and(stop)
                })
                .map_err(|error| {
                    PlaybackError::worker(format!("could not start the audio worker: {error}"))
                })?;
            (Some(queue), Some(worker))
        } else {
            (None, None)
        };

        Ok(Self {
            video: Some(video_queue),
            audio: audio_queue,
            counters,
            video_worker: Some(video_worker),
            audio_worker,
            config,
            stopped: false,
        })
    }

    /// Queue a video frame for presentation.
    ///
    /// Never blocks on the sink. When the bounded queue is full, the oldest
    /// frame is evicted and counted in [`PlaybackStats::video_dropped`].
    pub fn play_video(&self, frame: DecodedVideoFrame) -> Result<(), PlaybackError> {
        if self.stopped {
            return Err(PlaybackError::Stopped);
        }
        let Some(queue) = self.video.as_ref() else {
            return Err(PlaybackError::Stopped);
        };
        match queue.push_drop_oldest(frame) {
            Ok(true) => {
                Counters::increment(&self.counters.video_dropped);
                Ok(())
            }
            Ok(false) => Ok(()),
            Err(_) => Err(PlaybackError::Stopped),
        }
    }

    /// Queue a decoded audio block for playback.
    ///
    /// Never blocks on the sink. A full queue rejects the block with
    /// [`PlaybackError::QueueFull`] and counts it in
    /// [`PlaybackStats::audio_dropped`].
    pub fn play_audio(&self, audio: DecodedAudio) -> Result<(), PlaybackError> {
        if self.stopped {
            return Err(PlaybackError::Stopped);
        }
        let Some(queue) = self.audio.as_ref() else {
            return Err(PlaybackError::Stopped);
        };
        match queue.push_reject(audio) {
            Ok(()) => Ok(()),
            Err(_) => {
                Counters::increment(&self.counters.audio_dropped);
                Err(PlaybackError::QueueFull {
                    capacity: queue.capacity(),
                })
            }
        }
    }

    /// Stop accepting frames, drain the bounded queues, and join both workers.
    ///
    /// Repeated calls return `Ok(())`. The first sink error encountered while
    /// stopping is returned after both workers have been joined.
    pub fn stop(&mut self) -> Result<(), PlaybackError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;

        if let Some(queue) = self.video.as_ref() {
            queue.close();
        }
        if let Some(queue) = self.audio.as_ref() {
            queue.close();
        }

        let mut first_error = None;
        if let Some(worker) = self.video_worker.take() {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => first_error = Some(error),
                Err(_) => {
                    first_error = Some(PlaybackError::worker(
                        "the video worker panicked while stopping",
                    ))
                }
            }
        }
        if let Some(worker) = self.audio_worker.take() {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    first_error.get_or_insert(error);
                }
                Err(_) => {
                    first_error.get_or_insert_with(|| {
                        PlaybackError::worker("the audio worker panicked while stopping")
                    });
                }
            }
        }

        self.video.take();
        self.audio.take();
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Current counters.
    pub fn stats(&self) -> PlaybackStats {
        self.counters.snapshot()
    }

    /// True until [`PlaybackEngine::stop`] is called.
    pub fn is_running(&self) -> bool {
        !self.stopped
    }

    /// The queue configuration in force.
    pub fn config(&self) -> EngineConfig {
        self.config
    }
}

impl Drop for PlaybackEngine {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::NullAudioSink;
    use crate::video::NullVideoSink;
    use ozma_decode::{DecodeMeta, I420Planes};
    use std::time::{Duration, Instant};

    fn frame(sequence: u64) -> DecodedVideoFrame {
        DecodedVideoFrame::new(
            DecodeMeta::new(sequence * 1_000, sequence, sequence == 0),
            2,
            2,
            I420Planes::new(vec![16; 4], vec![128; 1], vec![128; 1]),
        )
        .unwrap()
    }

    fn audio(sequence: u64) -> DecodedAudio {
        DecodedAudio {
            pts_micros: sequence * 20_000,
            sequence,
            sample_rate: crate::audio::PLAYBACK_SAMPLE_RATE,
            channels: crate::audio::PLAYBACK_CHANNELS,
            samples: vec![1, -1, 2, -2],
        }
    }

    #[test]
    fn null_sinks_receive_queued_frames_and_stop_cleanly() {
        let mut engine = PlaybackEngine::new(
            Box::new(NullVideoSink::new()),
            Some(Box::new(NullAudioSink::new())),
        )
        .unwrap();
        assert!(engine.is_running());
        engine.play_video(frame(0)).unwrap();
        engine.play_audio(audio(0)).unwrap();
        engine.stop().unwrap();
        assert!(!engine.is_running());

        let stats = engine.stats();
        assert_eq!(stats.video_presented, 1);
        assert_eq!(stats.audio_played, 1);
        assert_eq!(stats.video_dropped, 0);
        assert_eq!(stats.audio_dropped, 0);
        assert_eq!(stats.errors, 0);

        engine.stop().unwrap();
        assert_eq!(
            engine.play_video(frame(1)).unwrap_err(),
            PlaybackError::Stopped
        );
        assert_eq!(
            engine.play_audio(audio(1)).unwrap_err(),
            PlaybackError::Stopped
        );
    }

    #[test]
    fn video_queue_drops_the_oldest_frame_instead_of_blocking() {
        struct GateState {
            entered: bool,
            released: bool,
        }

        struct Gate {
            state: Mutex<GateState>,
            changed: Condvar,
        }

        impl Gate {
            fn new() -> Self {
                Self {
                    state: Mutex::new(GateState {
                        entered: false,
                        released: false,
                    }),
                    changed: Condvar::new(),
                }
            }

            fn enter_and_wait(&self) {
                let mut state = self.state.lock().unwrap();
                state.entered = true;
                self.changed.notify_all();
                while !state.released {
                    state = self.changed.wait(state).unwrap();
                }
            }

            fn wait_until_entered(&self) {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut state = self.state.lock().unwrap();
                while !state.entered {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    assert!(!remaining.is_zero(), "worker never entered the sink");
                    let (next, _) = self.changed.wait_timeout(state, remaining).unwrap();
                    state = next;
                }
            }

            fn release(&self) {
                let mut state = self.state.lock().unwrap();
                state.released = true;
                self.changed.notify_all();
            }
        }

        struct GatedVideoSink {
            gate: Arc<Gate>,
            presented: Arc<AtomicU64>,
        }

        impl VideoSink for GatedVideoSink {
            fn present(&mut self, _frame: &DecodedVideoFrame) -> Result<(), PlaybackError> {
                self.gate.enter_and_wait();
                self.presented.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }

            fn flush(&mut self) -> Result<(), PlaybackError> {
                Ok(())
            }

            fn stop(&mut self) -> Result<(), PlaybackError> {
                Ok(())
            }

            fn is_open(&self) -> bool {
                true
            }
        }

        let gate = Arc::new(Gate::new());
        let presented = Arc::new(AtomicU64::new(0));
        let sink = GatedVideoSink {
            gate: gate.clone(),
            presented: presented.clone(),
        };
        let config = EngineConfig {
            video_capacity: 1,
            audio_capacity: 1,
        };
        let mut engine = PlaybackEngine::with_config(Box::new(sink), None, config).unwrap();

        engine.play_video(frame(0)).unwrap();
        // The worker now holds frame 0 inside `present`.
        gate.wait_until_entered();

        engine.play_video(frame(1)).unwrap();
        // Queue capacity is one, so frame 2 evicts frame 1.
        engine.play_video(frame(2)).unwrap();
        assert_eq!(engine.stats().video_dropped, 1);
        assert_eq!(engine.stats().video_presented, 0);

        gate.release();
        engine.stop().unwrap();
        assert_eq!(engine.stats().video_presented, 2);
        assert_eq!(engine.stats().video_dropped, 1);
        assert_eq!(presented.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn audio_queue_rejects_new_blocks_and_counts_them() {
        struct StalledGate {
            state: Mutex<(bool, bool)>,
            changed: Condvar,
        }

        impl StalledGate {
            fn new() -> Self {
                Self {
                    state: Mutex::new((false, false)),
                    changed: Condvar::new(),
                }
            }

            fn enter_and_wait(&self) {
                let mut state = self.state.lock().unwrap();
                state.0 = true;
                self.changed.notify_all();
                while !state.1 {
                    state = self.changed.wait(state).unwrap();
                }
            }

            fn wait_until_entered(&self) {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut state = self.state.lock().unwrap();
                while !state.0 {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    assert!(!remaining.is_zero(), "audio worker never entered the sink");
                    let (next, _) = self.changed.wait_timeout(state, remaining).unwrap();
                    state = next;
                }
            }

            fn release(&self) {
                let mut state = self.state.lock().unwrap();
                state.1 = true;
                self.changed.notify_all();
            }
        }

        struct StalledAudioSink {
            gate: Arc<StalledGate>,
            played: Arc<AtomicU64>,
        }

        impl AudioSink for StalledAudioSink {
            fn open(&mut self) -> Result<(), PlaybackError> {
                Ok(())
            }

            fn play(&mut self, _audio: &DecodedAudio) -> Result<(), PlaybackError> {
                self.gate.enter_and_wait();
                self.played.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }

            fn flush(&mut self) -> Result<(), PlaybackError> {
                Ok(())
            }

            fn stop(&mut self) -> Result<(), PlaybackError> {
                Ok(())
            }

            fn is_open(&self) -> bool {
                true
            }
        }

        let gate = Arc::new(StalledGate::new());
        let played = Arc::new(AtomicU64::new(0));
        let sink = StalledAudioSink {
            gate: gate.clone(),
            played: played.clone(),
        };
        let config = EngineConfig {
            video_capacity: 1,
            audio_capacity: 1,
        };
        let mut engine = PlaybackEngine::with_config(
            Box::new(NullVideoSink::new()),
            Some(Box::new(sink)),
            config,
        )
        .unwrap();

        engine.play_audio(audio(0)).unwrap();
        // The worker now holds block 0 inside `play`, so the queue is empty.
        gate.wait_until_entered();
        engine.play_audio(audio(1)).unwrap();
        let error = engine.play_audio(audio(2)).unwrap_err();
        assert!(matches!(error, PlaybackError::QueueFull { capacity: 1 }));
        assert_eq!(engine.stats().audio_dropped, 1);

        gate.release();
        engine.stop().unwrap();
        assert_eq!(engine.stats().audio_played, 2);
        assert_eq!(played.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn engine_debug_and_empty_stats_are_safe() {
        let engine = PlaybackEngine::new(Box::new(NullVideoSink::new()), None).unwrap();
        assert!(engine.stats().is_empty());
        assert_eq!(engine.stats().drops(), 0);
        assert!(format!("{engine:?}").contains("PlaybackEngine"));
    }
}
