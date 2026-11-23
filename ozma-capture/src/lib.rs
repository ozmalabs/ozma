// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-capture` — capture-source discovery, real V4L2 MMAP capture, and the
//! deterministic fake path.
//!
//! This crate contains no `unsafe` code. V4L2 discovery reads `/dev` and sysfs
//! without opening a device; opening a source uses the safe [`v4l`] crate to
//! query capabilities, negotiate a single-plane format, and stream MMAP
//! buffers. A dedicated worker thread owns the blocking device/stream pair so
//! the async [`ozma_media::CaptureSource`] remains responsive. Tests use
//! [`FakeCaptureProvider`] for CI; the real capture test is opt-in because it
//! needs `/dev/video*` hardware.

#![forbid(unsafe_code)]

pub mod fake;
pub mod provider;
pub mod v4l2;

pub use fake::FakeCaptureProvider;
pub use provider::{CaptureProvider, CaptureRegistry, CaptureScan};
pub use v4l2::{
    choose_resolution, discover_v4l2, fourcc_to_pixel_format, h264_access_unit_contains_idr,
    is_capture_like_name, keyframe_for, monotonic_pts, next_sequence, parse_video_index,
    preferred_fourcc, timestamp_micros, V4l2Capture, V4l2Node, V4l2Provider, DEFAULT_FRAME_TIMEOUT,
    DEFAULT_MAX_HEIGHT, DEFAULT_MAX_WIDTH, PREFERRED_FOURCCS,
};

/// Re-export of the capture vocabulary callers need alongside this crate.
pub use ozma_protocol::{CaptureCapability, CaptureKind};

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_media::{CaptureSource, MediaError};
    use ozma_protocol::{codes, ApiError};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct BrokenProvider {
        calls: Arc<AtomicUsize>,
    }

    impl CaptureProvider for BrokenProvider {
        fn kind(&self) -> CaptureKind {
            CaptureKind::Kms
        }

        fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Err(ApiError::new(
                codes::CAPTURE_UNAVAILABLE,
                "KMS discovery failed",
            ))
        }

        fn open(
            &self,
            _capability: &CaptureCapability,
        ) -> Result<Box<dyn CaptureSource>, ApiError> {
            Err(ApiError::new(codes::NOT_FOUND, "nothing to open"))
        }
    }

    #[tokio::test]
    async fn fake_provider_is_a_real_registry_path() {
        let mut registry = CaptureRegistry::new();
        registry.register(
            FakeCaptureProvider::new()
                .with_frame_limit(2)
                .with_fps(1_000),
        );

        let capabilities = registry.enumerate().unwrap();
        assert_eq!(capabilities.len(), 1);
        assert_eq!(capabilities[0].id, "fake0");
        assert_eq!(capabilities[0].kind, CaptureKind::Fake);

        let mut source = registry.open(&capabilities[0]).unwrap();
        let first = source.next_frame().await.unwrap();
        let second = source.next_frame().await.unwrap();
        assert!(!first.is_empty());
        assert_eq!(first.sequence(), 0);
        assert_eq!(second.sequence(), 1);
        assert!(second.pts_micros() > first.pts_micros());
        assert_eq!(
            source.next_frame().await.unwrap_err(),
            MediaError::EndOfStream
        );
    }

    #[test]
    fn open_id_rejects_unknown_sources() {
        let registry = CaptureRegistry::new().with(FakeCaptureProvider::new());
        let error = registry.open_id("missing").err().unwrap();
        assert_eq!(error.code, codes::NOT_FOUND);
    }

    #[test]
    fn scan_surfaces_provider_errors_without_hiding_other_sources() {
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = CaptureRegistry::new()
            .with(BrokenProvider {
                calls: Arc::clone(&calls),
            })
            .with(FakeCaptureProvider::new());

        let scans = registry.scan();
        assert_eq!(scans.len(), 2);
        assert_eq!(scans[0].kind, CaptureKind::Kms);
        assert!(scans[0].error.is_some());
        assert!(scans[1].is_ok());
        assert_eq!(scans[1].capabilities.len(), 1);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // The all-or-nothing view reports the failure instead of dropping it.
        let error = registry.enumerate().unwrap_err();
        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
    }

    #[test]
    fn system_registry_starts_with_v4l2_discovery() {
        let registry = CaptureRegistry::system();
        assert_eq!(registry.provider_count(), 1);
        // Discovery on the build host is allowed to return any number of
        // devices, including none; it must not error for a normal host.
        assert!(registry.enumerate().is_ok());
    }
}
