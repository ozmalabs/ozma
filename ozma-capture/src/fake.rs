// SPDX-License-Identifier: AGPL-3.0-only
//! A registry-compatible fake capture provider.
//!
//! This is the deterministic source used by node, session, and API tests. It
//! wraps [`ozma_media::FakeCapture`] so tests exercise the same provider and
//! registry path as a real device, without touching V4L2.

use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};
use ozma_media::{CaptureSource, FakeCapture, FrameFormat, PixelFormat};

use crate::provider::CaptureProvider;

/// A provider that exposes exactly one synthetic capture source.
#[derive(Clone, Debug)]
pub struct FakeCaptureProvider {
    capability: CaptureCapability,
    format: FrameFormat,
    frame_limit: Option<u64>,
    fps: u32,
}

impl Default for FakeCaptureProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeCaptureProvider {
    /// One 320x240 RGB24 source at 30 fps with unlimited frames.
    pub fn new() -> Self {
        Self {
            capability: CaptureCapability {
                id: "fake0".to_owned(),
                name: "Fake capture".to_owned(),
                kind: CaptureKind::Fake,
                formats: vec![PixelFormat::Rgb24.as_str().to_owned()],
                max_width: 320,
                max_height: 240,
                max_fps: 30,
                compressed: false,
                hdr: false,
                has_audio: false,
            },
            format: FrameFormat::rgb24(320, 240),
            frame_limit: None,
            fps: 30,
        }
    }

    /// Override the advertised id and name.
    pub fn with_identity(mut self, id: impl Into<String>, name: impl Into<String>) -> Self {
        self.capability.id = id.into();
        self.capability.name = name.into();
        self
    }

    /// Set the synthetic frame format and matching capability geometry.
    pub fn with_format(mut self, format: FrameFormat) -> Self {
        self.format = format;
        self.capability.formats = vec![format.pixel_format.as_str().to_owned()];
        self.capability.max_width = format.width;
        self.capability.max_height = format.height;
        self
    }

    /// Limit the number of frames produced by opened sources.
    pub fn with_frame_limit(mut self, frames: u64) -> Self {
        self.frame_limit = Some(frames);
        self
    }

    /// Set the pacing rate of opened sources.
    pub fn with_fps(mut self, fps: u32) -> Self {
        self.fps = fps.max(1);
        self.capability.max_fps = self.fps;
        self
    }

    /// The capability this provider advertises.
    pub fn capability(&self) -> &CaptureCapability {
        &self.capability
    }
}

impl CaptureProvider for FakeCaptureProvider {
    fn kind(&self) -> CaptureKind {
        CaptureKind::Fake
    }

    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        Ok(vec![self.capability.clone()])
    }

    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        if capability.kind != CaptureKind::Fake || capability.id != self.capability.id {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                format!("Fake capture source `{}` is not available.", capability.id),
            )
            .with_details(capability.id.clone()));
        }

        let mut source = FakeCapture::new(self.format)
            .with_id(self.capability.id.clone())
            .with_fps(self.fps);
        if let Some(limit) = self.frame_limit {
            source = source.with_frame_limit(limit);
        }
        Ok(Box::new(source))
    }
}
