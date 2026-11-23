// SPDX-License-Identifier: AGPL-3.0-only
//! Live native playback for Ozma sessions.
//!
//! Connects to a prepared session, reassembles/decodes H.264, decodes Opus,
//! and feeds `ozma-playback` sinks. Build with `--features window` for a
//! native window; otherwise use PPM output or the null sink.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use ozma_client::{
    fetch_transport_info, load_certificate, ClientCaptureOptions, ClientEvent, NativeSessionClient,
    VideoAssembler,
};
use ozma_decode::{AudioDecoder, DecodeMeta, H264Decoder, VideoDecoder};
use ozma_playback::{
    AlsaAudioSink, AlsaAudioSinkConfig, AudioSink, NullVideoSink, PlaybackEngine, PpmVideoSink,
    VideoSink,
};
use ozma_protocol::{ApiError, ChannelKind, SessionOffer};
use tokio::time::timeout;

#[derive(Debug, Parser)]
#[command(name = "ozma-play", about = "Play a live Ozma session")]
struct Cli {
    /// SessionOffer JSON file or inline JSON.
    #[arg(long)]
    offer: String,
    /// DER/PEM certificate file; otherwise fetched via --node.
    #[arg(long)]
    cert: Option<PathBuf>,
    /// Node API URL for GET /api/v1/transport.
    #[arg(long, env = "OZMA_NODE_URL")]
    node: Option<String>,
    /// Override offer token.
    #[arg(long, env = "OZMA_SESSION_TOKEN")]
    token: Option<String>,
    #[arg(long, default_value = "localhost")]
    server_name: String,
    /// Seconds to play; 0 runs until the connection closes or Ctrl-C.
    #[arg(long, default_value_t = 10)]
    seconds: u64,
    /// Play Opus audio through the default ALSA device.
    #[arg(long)]
    audio: bool,
    /// Open a native window (requires --features window).
    #[arg(long)]
    window: bool,
    /// Write decoded frames here instead of a window/null sink.
    #[arg(long)]
    frame_dir: Option<PathBuf>,
    /// Publish this client's camera. Optionally name a source; a bare flag uses
    /// the first available source.
    #[arg(long, value_name = "SOURCE", num_args = 0..=1, default_missing_value = "")]
    camera: Option<String>,
    /// Publish this client's microphone. The session offer must include a mic
    /// channel, which means the session was created with `audio.mic = true`.
    #[arg(long, value_name = "SOURCE", num_args = 0..=1, default_missing_value = "")]
    mic: Option<String>,
}

fn api_error(code: &str, message: impl Into<String>) -> ApiError {
    ApiError::new(code, message)
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match run(Cli::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error [{}]: {}", error.code, error.message);
            if let Some(remediation) = &error.remediation {
                eprintln!("  remediation: {remediation}");
            }
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), ApiError> {
    let mut offer: SessionOffer = {
        let text = if cli.offer.trim_start().starts_with('{') {
            cli.offer.clone()
        } else {
            std::fs::read_to_string(&cli.offer).map_err(|error| {
                api_error(
                    ozma_client::codes::CLIENT_CONFIG,
                    format!("cannot read offer '{}': {error}", cli.offer),
                )
            })?
        };
        serde_json::from_str(&text).map_err(|error| {
            api_error(
                ozma_client::codes::CLIENT_CONFIG,
                format!("invalid offer JSON: {error}"),
            )
        })?
    };
    if let Some(token) = cli.token {
        offer.token = token;
    }
    let certificate = match cli.cert {
        Some(path) => load_certificate(&path)?,
        None => {
            let node = cli.node.as_deref().ok_or_else(|| {
                api_error(
                    ozma_client::codes::CLIENT_CONFIG,
                    "either --cert or --node is required",
                )
            })?;
            fetch_transport_info(node, None, false)
                .await?
                .certificate_der
        }
    };
    let mut client = NativeSessionClient::connect(&offer, &certificate).await?;

    let capture_requested = cli.camera.is_some() || cli.mic.is_some();
    if capture_requested {
        if cli.mic.is_some()
            && client
                .channel_map()
                .channel_for_kind(ChannelKind::Mic)
                .is_none()
        {
            return Err(api_error(
                ozma_client::codes::CLIENT_CONFIG,
                "the session offer has no microphone channel",
            )
            .with_remediation(
                "Create the session with `audio.mic = true`, then reconnect and retry.",
            ));
        }
        let options = ClientCaptureOptions {
            camera: cli.camera.as_ref().map(|source| {
                if source.is_empty() {
                    "default".to_owned()
                } else {
                    source.clone()
                }
            }),
            mic: cli.mic.as_ref().map(|source| {
                if source.is_empty() {
                    "default".to_owned()
                } else {
                    source.clone()
                }
            }),
            width: 1_280,
            height: 720,
            fps: 30,
            bitrate_kbps: 4_000,
        };
        client.start_capture(options).await?;
    }

    let video: Box<dyn VideoSink> = if cli.window {
        #[cfg(feature = "window")]
        {
            Box::new(
                ozma_playback::WindowVideoSink::new("Ozma session", 1280, 720).map_err(
                    |error| api_error(ozma_client::codes::CLIENT_PROTOCOL, error.to_string()),
                )?,
            )
        }
        #[cfg(not(feature = "window"))]
        {
            return Err(api_error(
                ozma_client::codes::CLIENT_CONFIG,
                "this binary was built without the `window` feature",
            ));
        }
    } else if let Some(directory) = cli.frame_dir {
        Box::new(
            PpmVideoSink::new(directory)
                .map_err(|error| api_error(ozma_client::codes::CLIENT_CONFIG, error.to_string()))?,
        )
    } else {
        Box::new(NullVideoSink::new())
    };
    let audio: Option<Box<dyn AudioSink>> = if cli.audio {
        #[cfg(target_os = "linux")]
        {
            match ozma_playback::PipeWireAudioSink::default_sink().await {
                Ok(sink) => Some(Box::new(sink)),
                Err(error) => {
                    tracing::warn!(error = %error, "PipeWire playback unavailable; trying ALSA");
                    Some(Box::new(
                        AlsaAudioSink::connect(AlsaAudioSinkConfig::default()).map_err(
                            |error| api_error(ozma_client::codes::CLIENT_CONFIG, error.to_string()),
                        )?,
                    ))
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Some(Box::new(
                AlsaAudioSink::connect(AlsaAudioSinkConfig::default()).map_err(|error| {
                    api_error(ozma_client::codes::CLIENT_CONFIG, error.to_string())
                })?,
            ))
        }
    } else {
        None
    };
    let mut engine = PlaybackEngine::new(video, audio)
        .map_err(|error| api_error(ozma_client::codes::CLIENT_PROTOCOL, error.to_string()))?;
    let mut video_decoder = H264Decoder::new()
        .map_err(|error| api_error(ozma_client::codes::CLIENT_PROTOCOL, error.to_string()))?;
    let mut audio_decoder = AudioDecoder::new().ok();
    let mut assembler = VideoAssembler::default();
    let started = Instant::now();
    let limit = (cli.seconds > 0).then(|| Duration::from_secs(cli.seconds));
    let mut audio_sequence = 0u64;

    loop {
        if limit.is_some_and(|limit| started.elapsed() >= limit) {
            break;
        }
        let wait = limit
            .map(|limit| limit.saturating_sub(started.elapsed()))
            .unwrap_or(Duration::from_secs(3600));
        let event = match timeout(wait, client.recv_event()).await {
            Ok(event) => event?,
            Err(_) => break,
        };
        match event {
            ClientEvent::Media(packet) if packet.kind == ChannelKind::Video => {
                if let Some(frame) =
                    assembler
                        .push_or_complete(&packet.payload)
                        .map_err(|error| {
                            api_error(ozma_client::codes::CLIENT_PROTOCOL, error.to_string())
                        })?
                {
                    let meta = DecodeMeta::new(0, frame.sequence, frame.keyframe);
                    if let Some(decoded) =
                        video_decoder
                            .decode(&frame.data, meta)
                            .await
                            .map_err(|error| {
                                api_error(ozma_client::codes::CLIENT_PROTOCOL, error.to_string())
                            })?
                    {
                        let _ = engine.play_video(decoded);
                    } else {
                        for decoded in video_decoder.flush().await.map_err(|error| {
                            api_error(ozma_client::codes::CLIENT_PROTOCOL, error.to_string())
                        })? {
                            let _ = engine.play_video(decoded);
                        }
                    }
                }
            }
            ClientEvent::Media(packet) if packet.kind == ChannelKind::Audio => {
                if let Some(decoder) = audio_decoder.as_mut() {
                    let pts = audio_sequence.saturating_mul(20_000);
                    if let Ok(decoded) = decoder.decode_bytes(&packet.payload, pts, audio_sequence)
                    {
                        let _ = engine.play_audio(decoded);
                    }
                    audio_sequence += 1;
                }
            }
            ClientEvent::Closed { .. } | ClientEvent::Bye { .. } => break,
            ClientEvent::Error { error } => return Err(error),
            _ => {}
        }
    }

    if capture_requested {
        let _ = client.stop_capture().await;
    }
    let capture_state = client.capture_state();
    let _ = client.release_all().await;
    let _ = client.close().await;
    engine
        .stop()
        .map_err(|error| api_error(ozma_client::codes::CLIENT_PROTOCOL, error.to_string()))?;
    let stats = engine.stats();
    println!(
        "played: video {} (dropped {}), audio {} (dropped {}), errors {}",
        stats.video_presented,
        stats.video_dropped,
        stats.audio_played,
        stats.audio_dropped,
        stats.errors
    );
    if let Some(state) = &capture_state {
        println!(
            "captured: video {} ({} frames, {} bytes, {} dropped), mic {} ({} packets, {} bytes), errors {}",
            state.video_codec.as_deref().unwrap_or("off"),
            state.frames_sent,
            state.bytes_sent,
            state.dropped,
            if state.audio { "on" } else { "off" },
            state.mic_packets,
            state.mic_bytes,
            state.errors
        );
    }
    Ok(())
}
