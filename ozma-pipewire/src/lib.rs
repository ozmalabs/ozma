//! # ozma-pipewire
//!
//! PipeWire audio routing and native audio streams for the Ozma agent.
//!
//! ## Platform support
//!
//! | Platform | Backend           | Link management    |
//! |----------|-------------------|--------------------|
//! | Linux    | PipeWire (native) | ✓                  |
//! | Windows  | CPAL              | ✗ (enumerate only) |
//! | macOS    | CPAL              | ✗ (enumerate only) |
//!
//! ## Graph routing
//!
//! ```rust,no_run
//! use ozma_pipewire::build_router;
//!
//! #[tokio::main]
//! async fn main() {
//!     let router = build_router().await.expect("audio router init failed");
//!     let snap = router.snapshot().await.expect("snapshot failed");
//!     for node in &snap.nodes {
//!         println!("{:?}", node);
//!     }
//! }
//! ```
//!
//! ## Native capture and playback (Linux)
//!
//! [`PipeWireAudioSource`] and [`PipeWireAudioSink`] open their own PipeWire
//! `Stream` on a dedicated OS thread, independent of the graph router.  Both
//! target 48 kHz stereo and accept `S16LE` or `F32LE`; capture always exposes
//! interleaved `i16`.
//!
//! [`PipeWireVideoSource`] is the video equivalent: a raw `Video/Capture`
//! stream that negotiates `BGRA`, `BGRx`, `RGBx`, `I420`, or `NV12` (with
//! `YUYV` as a UVC compatibility fallback) and yields owned [`VideoFrame`]s.
//! [`probe_video_sources`] lists the graph's `Video/Source` nodes without
//! opening a stream.
//!
//! ```rust,no_run
//! use ozma_pipewire::{CaptureConfig, PcmFrame, PipeWireAudioSource};
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut source = PipeWireAudioSource::new(CaptureConfig::default()).await?;
//!     let frame: PcmFrame = tokio::time::timeout(Duration::from_secs(2), source.next_frame()).await??;
//!     println!("{} Hz, {} ch, {} samples", frame.sample_rate, frame.channels, frame.samples.len());
//!     let status = ozma_pipewire::probe(Duration::from_secs(2)).await?;
//!     println!("{}", status.summary());
//!     Ok(())
//! }
//! ```

pub mod error;
pub mod pcm;
pub mod types;

#[cfg(target_os = "linux")]
mod capture;
#[cfg(target_os = "linux")]
mod pipewire_backend;
#[cfg(target_os = "linux")]
mod pw_common;
#[cfg(target_os = "linux")]
mod sink;
#[cfg(target_os = "linux")]
mod status;
#[cfg(target_os = "linux")]
mod video;

#[cfg(not(target_os = "linux"))]
mod cpal_backend;

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::broadcast;

pub use error::PipeWireError;
pub use pcm::{
    convert_to_s16, f32_to_i16, s16_to_f32_le_bytes, s16_to_le_bytes, NegotiatedFormat, PcmFrame,
    PcmQueue, SampleFormat,
};
pub use types::{AudioEvent, AudioLink, AudioNode, GraphSnapshot, LinkRequest};

#[cfg(target_os = "linux")]
pub use capture::{CaptureConfig, CaptureStats, PipeWireAudioSource};
#[cfg(target_os = "linux")]
pub use pw_common::StreamTarget;
#[cfg(target_os = "linux")]
pub use sink::{PipeWireAudioSink, SinkConfig, SinkStats};
#[cfg(target_os = "linux")]
pub use status::{probe, probe_status, probe_with, PipeWireStatus, ProbeConfig, StatusError};
#[cfg(target_os = "linux")]
pub use video::{
    build_buffer_pod, build_video_format_pod, parse_video_format, probe_video_sources,
    probe_video_sources_blocking, probe_video_sources_with, probe_video_status,
    NegotiatedVideoFormat, PipeWireVideoSource, PlaneLayout, VideoCaptureConfig, VideoCaptureStats,
    VideoFrame, VideoNode, VideoPixelFormat, VideoProbeConfig, VideoStatus,
};

/// Core trait implemented by every audio routing backend.
#[async_trait]
pub trait AudioRouter: Send + Sync {
    /// Return a point-in-time snapshot of the audio graph.
    async fn snapshot(&self) -> anyhow::Result<GraphSnapshot>;

    /// Create a link between two nodes.
    ///
    /// Returns the [`AudioLink`] that was created.
    /// Returns an error on platforms that do not support link management.
    async fn create_link(&self, req: LinkRequest) -> anyhow::Result<AudioLink>;

    /// Destroy a previously created link by its ID.
    async fn destroy_link(&self, link_id: u32) -> anyhow::Result<()>;

    /// Subscribe to live graph-change events.
    ///
    /// The returned receiver will receive [`AudioEvent`]s as they occur.
    /// Lagged receivers will see a [`broadcast::error::RecvError::Lagged`] error
    /// and should call [`snapshot`] to resync.
    fn subscribe(&self) -> broadcast::Receiver<AudioEvent>;
}

/// Construct the best available [`AudioRouter`] for the current platform.
///
/// On Linux this returns a `PipeWireRouter`; on other platforms it returns
/// a `CpalRouter`.
pub async fn build_router() -> anyhow::Result<Arc<dyn AudioRouter>> {
    #[cfg(target_os = "linux")]
    {
        let r = pipewire_backend::PipeWireRouter::new().await?;
        return Ok(Arc::new(r));
    }

    #[cfg(not(target_os = "linux"))]
    {
        let r = cpal_backend::CpalRouter::new().await?;
        return Ok(Arc::new(r));
    }
}
