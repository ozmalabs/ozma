// SPDX-License-Identifier: AGPL-3.0-only
//! A bounded, cancellable capture -> encode pipeline for one track.
//!
//! Each pipeline owns one [`CaptureSource`] and one [`Encoder`], and runs them
//! in two Tokio tasks connected by a small bounded queue. Failures are
//! propagated to the owner through [`RunningPipeline::wait`]; they are never
//! swallowed or converted into a silent drop.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ozma_protocol::ApiError;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::cancel::CancelToken;
use crate::error::MediaError;
use crate::frame::{EncodedFrame, Frame};
use crate::traits::{CaptureSource, Encoder};

/// How long a cancelled pipeline waits for a source's `stop` hook.
const STOP_TIMEOUT: Duration = Duration::from_millis(250);

/// Cumulative counters for one running track.
///
/// `frames_encoded` counts access units produced by the encoder even if the
/// consumer queue was full and the frame was dropped. `frames_dropped` covers
/// both capture-side and output-side drops. `last_pts_micros` is the PTS of
/// the most recently captured frame.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineMetrics {
    /// Frames read from the capture source.
    pub frames_captured: u64,
    /// Access units produced by the encoder.
    pub frames_encoded: u64,
    /// Frames discarded because a bounded queue was full.
    pub frames_dropped: u64,
    /// Counted source or encoder failures.
    pub errors: u64,
    /// PTS of the last captured frame, in microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pts_micros: Option<u64>,
}

/// Queue sizing and overflow policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipelineConfig {
    /// Frames buffered between capture and encode. Must be non-zero.
    pub capture_queue: usize,
    /// Encoded frames buffered for the consumer. Must be non-zero.
    pub output_queue: usize,
    /// Drop the newest frame when a queue is full instead of applying
    /// backpressure. This keeps latency bounded on slow encoders.
    pub drop_on_full: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            capture_queue: 2,
            output_queue: 8,
            drop_on_full: true,
        }
    }
}

impl PipelineConfig {
    /// Default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set both queue capacities.
    pub fn with_queues(mut self, capture_queue: usize, output_queue: usize) -> Self {
        self.capture_queue = capture_queue;
        self.output_queue = output_queue;
        self
    }

    /// Block instead of dropping when a queue is full.
    pub fn with_backpressure(mut self) -> Self {
        self.drop_on_full = false;
        self
    }
}

const NO_PTS: u64 = u64::MAX;

#[derive(Debug)]
struct Counters {
    captured: AtomicU64,
    encoded: AtomicU64,
    dropped: AtomicU64,
    errors: AtomicU64,
    last_pts: AtomicU64,
}

impl Default for Counters {
    fn default() -> Self {
        Self {
            captured: AtomicU64::new(0),
            encoded: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            last_pts: AtomicU64::new(NO_PTS),
        }
    }
}

impl Counters {
    fn snapshot(&self) -> PipelineMetrics {
        let last = self.last_pts.load(Ordering::Relaxed);
        PipelineMetrics {
            frames_captured: self.captured.load(Ordering::Relaxed),
            frames_encoded: self.encoded.load(Ordering::Relaxed),
            frames_dropped: self.dropped.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            last_pts_micros: (last != NO_PTS).then_some(last),
        }
    }
}

/// A running single-track media pipeline.
///
/// Drop the handle to stop the track, or call [`RunningPipeline::cancel`] and
/// [`RunningPipeline::wait`] for a deterministic shutdown.
#[derive(Debug)]
pub struct RunningPipeline {
    frames: mpsc::Receiver<EncodedFrame>,
    cancel: CancelToken,
    counters: Arc<Counters>,
    capture_task: JoinHandle<Result<(), MediaError>>,
    encode_task: JoinHandle<Result<(), MediaError>>,
}

impl RunningPipeline {
    /// Wait for the next encoded frame, or `None` once the track has stopped.
    pub async fn recv(&mut self) -> Option<EncodedFrame> {
        self.frames.recv().await
    }

    /// Borrow the encoded-frame receiver for select loops and consumers.
    pub fn frames(&mut self) -> &mut mpsc::Receiver<EncodedFrame> {
        &mut self.frames
    }

    /// Snapshot the live counters.
    pub fn metrics(&self) -> PipelineMetrics {
        self.counters.snapshot()
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Wait for both stages to stop and return the final counters.
    ///
    /// This drops the output receiver, so any buffered encoded frames are
    /// discarded and the encoder stops promptly. Errors from either stage are
    /// returned to the caller.
    pub async fn wait(self) -> Result<PipelineMetrics, MediaError> {
        let Self {
            frames,
            cancel,
            counters,
            capture_task,
            encode_task,
        } = self;
        drop(frames);
        let _ = cancel;
        let (capture, encode) = tokio::join!(capture_task, encode_task);
        join_stage(capture)??;
        join_stage(encode)??;
        Ok(counters.snapshot())
    }

    /// Cancel and wait in one step.
    pub async fn cancel_and_wait(self) -> Result<PipelineMetrics, MediaError> {
        self.cancel();
        self.wait().await
    }
}

fn join_stage(
    result: Result<Result<(), MediaError>, tokio::task::JoinError>,
) -> Result<Result<(), MediaError>, MediaError> {
    match result {
        Ok(inner) => Ok(inner),
        Err(error) => Err(MediaError::Api(ApiError::internal(format!(
            "media pipeline task failed: {error}"
        )))),
    }
}

/// A capture -> encode pipeline for one media track.
#[derive(Debug)]
pub struct Pipeline;

impl Pipeline {
    /// Validate the formats and start both stages.
    ///
    /// The source format must exactly match the encoder's declared input
    /// format; a mismatch is rejected before any device I/O starts.
    ///
    /// This spawns tasks and therefore must be called from a Tokio runtime.
    pub fn start<S, E>(
        source: S,
        encoder: E,
        config: PipelineConfig,
    ) -> Result<RunningPipeline, MediaError>
    where
        S: CaptureSource + 'static,
        E: Encoder + 'static,
    {
        if config.capture_queue == 0 || config.output_queue == 0 {
            return Err(MediaError::Api(ApiError::invalid(
                "pipeline queue capacities must be non-zero",
            )));
        }

        let source_format = source.format();
        let input_format = encoder.input_format();
        if !source_format.feeds(&input_format) {
            return Err(MediaError::FormatMismatch {
                expected: input_format.to_string(),
                actual: source_format.to_string(),
            });
        }

        let cancel = CancelToken::new();
        let counters = Arc::new(Counters::default());
        let (frame_tx, frame_rx) = mpsc::channel(config.capture_queue);
        let (output_tx, output_rx) = mpsc::channel(config.output_queue);

        let capture_task = tokio::spawn(run_capture(
            source,
            frame_tx,
            cancel.clone(),
            Arc::clone(&counters),
            config.drop_on_full,
        ));
        let encode_task = tokio::spawn(run_encoder(
            encoder,
            frame_rx,
            output_tx,
            cancel.clone(),
            Arc::clone(&counters),
            config.drop_on_full,
        ));

        Ok(RunningPipeline {
            frames: output_rx,
            cancel,
            counters,
            capture_task,
            encode_task,
        })
    }
}

/// Alias used when a session owns several pipelines, one per track.
pub type TrackPipeline = Pipeline;

async fn run_capture<S: CaptureSource>(
    mut source: S,
    frames: mpsc::Sender<Frame>,
    cancel: CancelToken,
    counters: Arc<Counters>,
    drop_on_full: bool,
) -> Result<(), MediaError> {
    let source_id = source.id().to_owned();
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            next = source.next_frame() => next,
        };

        let frame = match next {
            Ok(frame) => frame,
            Err(MediaError::EndOfStream) | Err(MediaError::Closed { .. }) => break,
            Err(MediaError::Cancelled) => break,
            Err(error) => {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(source = %source_id, error = %error, "capture source failed");
                cancel.cancel();
                stop_source(&mut source).await;
                return Err(error);
            }
        };

        counters.captured.fetch_add(1, Ordering::Relaxed);
        counters
            .last_pts
            .store(frame.pts_micros(), Ordering::Relaxed);

        match frames.try_send(frame) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(frame)) if drop_on_full => {
                counters.dropped.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(source = %source_id, "dropping frame: encoder queue full");
                let _ = frame;
            }
            Err(mpsc::error::TrySendError::Full(frame)) => {
                let sent = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => false,
                    result = frames.send(frame) => result.is_ok(),
                };
                if !sent {
                    break;
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }
    }

    stop_source(&mut source).await;
    Ok(())
}

async fn run_encoder<E: Encoder>(
    mut encoder: E,
    mut frames: mpsc::Receiver<Frame>,
    output: mpsc::Sender<EncodedFrame>,
    cancel: CancelToken,
    counters: Arc<Counters>,
    drop_on_full: bool,
) -> Result<(), MediaError> {
    let encoder_id = encoder.id().to_owned();

    loop {
        let maybe_frame = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            frame = frames.recv() => frame,
        };

        let Some(frame) = maybe_frame else {
            // Capture ended cleanly. Give the encoder a chance to drain.
            let flushed = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                result = encoder.flush() => result,
            };
            match flushed {
                Ok(encoded) => {
                    for frame in encoded {
                        counters.encoded.fetch_add(1, Ordering::Relaxed);
                        if !forward(&output, frame, drop_on_full, &cancel, &counters).await {
                            cancel.cancel();
                            return Ok(());
                        }
                    }
                }
                Err(error) => {
                    counters.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(encoder = %encoder_id, error = %error, "encoder flush failed");
                    cancel.cancel();
                    return Err(error);
                }
            }
            break;
        };

        let encoded = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            result = encoder.encode(frame) => result,
        };

        match encoded {
            Ok(frame) => {
                counters.encoded.fetch_add(1, Ordering::Relaxed);
                if !forward(&output, frame, drop_on_full, &cancel, &counters).await {
                    // The consumer is gone; stop capture as well.
                    cancel.cancel();
                    return Ok(());
                }
            }
            Err(error) => {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(encoder = %encoder_id, error = %error, "encoder failed");
                cancel.cancel();
                return Err(error);
            }
        }
    }

    Ok(())
}

/// Forward one encoded frame. Returns `false` when the consumer disappeared
/// or the pipeline was cancelled.
async fn forward(
    output: &mpsc::Sender<EncodedFrame>,
    frame: EncodedFrame,
    drop_on_full: bool,
    cancel: &CancelToken,
    counters: &Counters,
) -> bool {
    match output.try_send(frame) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(frame)) if drop_on_full => {
            counters.dropped.fetch_add(1, Ordering::Relaxed);
            tracing::debug!("dropping encoded frame: consumer queue full");
            let _ = frame;
            true
        }
        Err(mpsc::error::TrySendError::Full(frame)) => tokio::select! {
            biased;
            _ = cancel.cancelled() => false,
            result = output.send(frame) => result.is_ok(),
        },
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

async fn stop_source<S: CaptureSource>(source: &mut S) {
    match tokio::time::timeout(STOP_TIMEOUT, source.stop()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::debug!(error = %error, "capture source stop failed"),
        Err(_) => tracing::warn!("capture source stop timed out"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeCapture, FakeEncoder};
    use crate::frame::FrameFormat;
    use ozma_protocol::MediaFormat;

    fn h264_encoder(input: FrameFormat) -> FakeEncoder {
        FakeEncoder::new(
            input,
            MediaFormat::new(ozma_protocol::Codec::H264, 320, 240, 30, 2_000),
        )
    }

    #[tokio::test]
    async fn fake_pipeline_produces_frames() {
        let source = FakeCapture::default().with_fps(1_000).with_frame_limit(8);
        let encoder = h264_encoder(FrameFormat::rgb24(320, 240));
        let config = PipelineConfig::new().with_queues(4, 16);
        let mut pipeline = Pipeline::start(source, encoder, config).unwrap();

        let mut frames = Vec::new();
        for _ in 0..8 {
            let frame = tokio::time::timeout(Duration::from_secs(2), pipeline.recv())
                .await
                .expect("pipeline should produce a frame")
                .expect("pipeline should not close early");
            assert!(!frame.is_empty());
            assert_eq!(frame.format().codec, ozma_protocol::Codec::H264);
            frames.push(frame);
        }

        assert_eq!(frames[0].sequence(), 0);
        assert!(frames[0].is_keyframe());
        for pair in frames.windows(2) {
            assert!(pair[1].pts_micros() > pair[0].pts_micros());
            assert_eq!(pair[1].sequence(), pair[0].sequence() + 1);
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(2), pipeline.recv())
                .await
                .expect("receiver should close")
                .is_none()
        );

        let metrics = pipeline.wait().await.expect("clean pipeline");
        assert_eq!(metrics.frames_captured, 8);
        assert_eq!(metrics.frames_encoded, 8);
        assert_eq!(metrics.frames_dropped, 0);
        assert_eq!(metrics.errors, 0);
        assert_eq!(metrics.last_pts_micros, Some(frames[7].pts_micros()));
    }

    #[tokio::test]
    async fn format_mismatch_is_rejected_before_start() {
        let source = FakeCapture::default();
        let encoder = FakeEncoder::h264(FrameFormat::rgb24(640, 480), 2_000);
        let error = Pipeline::start(source, encoder, PipelineConfig::default()).unwrap_err();
        match error {
            MediaError::FormatMismatch { expected, actual } => {
                assert!(expected.contains("640x480"));
                assert!(actual.contains("320x240"));
            }
            other => panic!("expected format mismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancellation_stops_cleanly() {
        let source = FakeCapture::default().with_fps(1_000);
        let encoder = h264_encoder(FrameFormat::rgb24(320, 240));
        let config = PipelineConfig::new().with_queues(2, 8);
        let mut pipeline = Pipeline::start(source, encoder, config).unwrap();

        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(2), pipeline.recv())
                .await
                .expect("pipeline should produce a frame")
                .expect("pipeline should not close early");
        }
        assert!(!pipeline.is_cancelled());
        let metrics = tokio::time::timeout(Duration::from_secs(2), pipeline.cancel_and_wait())
            .await
            .expect("cancellation must finish promptly")
            .expect("cancellation is a clean stop");
        assert!(metrics.frames_captured >= 3);
        assert!(metrics.errors == 0);
    }

    #[tokio::test]
    async fn zero_capacity_is_rejected() {
        let source = FakeCapture::default();
        let encoder = h264_encoder(FrameFormat::rgb24(320, 240));
        let error =
            Pipeline::start(source, encoder, PipelineConfig::new().with_queues(0, 4)).unwrap_err();
        assert!(matches!(error, MediaError::Api(_)));
    }
}
