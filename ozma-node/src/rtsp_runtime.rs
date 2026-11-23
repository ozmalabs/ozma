// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! RTSP compatibility listener.
//!
//! One default capture/encode pipeline feeds every RTSP viewer via a bounded
//! broadcast. This is the LAN/`ssh -L`/Moonlight-compatible media path; the
//! native QUIC session remains the primary transport.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use ozma_media::{
    CaptureSource, EncodedFrame, FakeCapture, FakeEncoder, Frame, FrameFormat, MediaError, Pipeline,
    PipelineConfig,
};
use ozma_rtsp::{EncodedVideoSource, RtspConfig, RtspServer};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::backend::SessionBackend;

enum RTSPSource {
    Real(Box<dyn CaptureSource>),
    Fake(FakeCapture),
}

impl RTSPSource {
    fn format(&self) -> FrameFormat {
        match self {
            Self::Real(source) => source.format(),
            Self::Fake(source) => source.format(),
        }
    }
}

#[async_trait]
impl CaptureSource for RTSPSource {
    fn id(&self) -> &str {
        match self {
            Self::Real(source) => source.id(),
            Self::Fake(source) => source.id(),
        }
    }

    fn format(&self) -> FrameFormat {
        RTSPSource::format(self)
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        match self {
            Self::Real(source) => source.next_frame().await,
            Self::Fake(source) => source.next_frame().await,
        }
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        match self {
            Self::Real(source) => source.stop().await,
            Self::Fake(source) => source.stop().await,
        }
    }
}

struct BroadcastSource {
    initial: Option<EncodedFrame>,
    receiver: broadcast::Receiver<EncodedFrame>,
}

#[async_trait]
impl EncodedVideoSource for BroadcastSource {
    async fn next_frame(&mut self) -> Option<EncodedFrame> {
        if let Some(frame) = self.initial.take() {
            return Some(frame);
        }
        loop {
            match self.receiver.recv().await {
                Ok(frame) => return Some(frame),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

/// Start the RTSP listener and its shared default media pipeline.
pub async fn run(port: u16, backend: Arc<SessionBackend>) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("binding RTSP listener on port {port}"))?;

    let source = match backend.open_capture(None) {
        Ok(source) => RTSPSource::Real(source),
        Err(error) => {
            warn!(error = %error, "No real capture for RTSP; using fake source");
            RTSPSource::Fake(FakeCapture::default().with_fps(30))
        }
    };
    let input = source.format();
    let encoder = match backend.build_encoder(input) {
        Ok(encoder) => encoder,
        Err(error) => {
            warn!(error = %error, "No encoder for RTSP; using fake source");
            ozma_encode::SelectedEncoder::Fake(FakeEncoder::h264(input, 2_000))
        }
    };
    let mut pipeline = Pipeline::start(source, encoder, PipelineConfig::default())?;
    let (frames, _) = broadcast::channel::<EncodedFrame>(16);
    let keyframe = Arc::new(Mutex::new(None::<EncodedFrame>));
    let publisher = frames.clone();
    let keyframe_writer = keyframe.clone();
    tokio::spawn(async move {
        while let Some(frame) = pipeline.recv().await {
            if frame.is_keyframe() {
                *keyframe_writer.lock().expect("keyframe lock") = Some(frame.clone());
            }
            let _ = publisher.send(frame);
        }
    });

    let keyframe_reader = keyframe.clone();
    let server = RtspServer::new(RtspConfig::default()).with_source_factory(move || {
        let initial = keyframe_reader.lock().expect("keyframe lock").clone();
        Box::new(BroadcastSource {
            initial,
            receiver: frames.subscribe(),
        })
    });
    info!(port, "RTSP compatibility listener ready");
    server.serve(listener).await.context("serving RTSP")?;
    Ok(())
}
