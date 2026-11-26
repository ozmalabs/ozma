// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Unix-socket signaling server and the shared H.264 video pipeline.
//!
//! One encoder reads the shared-memory framebuffer and feeds a single
//! `TrackLocalStaticSample`; every peer connection attaches that track, so N
//! peers do not multiply encoder work. Frame queues are bounded (depth 2) and
//! a full queue drops the newest frame instead of growing. Encoding is idle
//! while no peer is connected.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_credential_type::RTCIceCredentialType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::encoder::BgraEncoder;
use crate::sdp::{
    append_ice_candidates, candidate_type_counts, offered_payload_type, remote_ice_candidates,
};
use crate::shm::{read_frame, FrameSpec};
use crate::signaling::{
    answer_response, bitrate_ack, candidate_ack, error_ack, error_response, parse_message,
    ClientMessage, IceKind,
};

/// Shared-memory and frame pacing configuration.
#[derive(Clone, Debug)]
pub struct VideoConfig {
    pub shm_path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

/// ICE and signaling configuration.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub socket_path: PathBuf,
    pub video: VideoConfig,
    pub ice_servers: Vec<RTCIceServer>,
    pub gather_timeout: Duration,
    pub max_sessions: usize,
}

/// Counters surfaced at shutdown and in periodic logs.
#[derive(Debug, Default)]
struct Stats {
    frames_read: AtomicU64,
    frames_dropped_full: AtomicU64,
    frames_encoded: AtomicU64,
    encode_errors: AtomicU64,
    samples_written: AtomicU64,
    samples_dropped_full: AtomicU64,
    bitrate_changes: AtomicU64,
}

/// The shared video source: one encoder, one track, many peer connections.
pub struct VideoStream {
    track: Arc<TrackLocalStaticSample>,
    bitrate_kbps: Arc<AtomicU32>,
    peers: Arc<AtomicUsize>,
    request_keyframe: Arc<AtomicBool>,
    shutdown_flag: Arc<AtomicBool>,
    shutdown: Arc<Notify>,
    stats: Arc<Stats>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

/// RAII peer counter: dropping the guard decrements the active peer count.
pub struct PeerGuard {
    stream: Arc<VideoStream>,
}

impl Drop for PeerGuard {
    fn drop(&mut self) {
        self.stream
            .peers
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                Some(value.saturating_sub(1))
            })
            .ok();
    }
}

impl VideoStream {
    /// Build the track and start the shared encoder, reader, writer, and stats
    /// tasks. The encoder is constructed here so an unusable OpenH264 build is
    /// a startup error rather than a silent no-video stream.
    pub fn start(config: VideoConfig) -> Result<Arc<Self>, String> {
        let spec = FrameSpec::new(config.width, config.height).map_err(|e| e.to_string())?;
        let encoder =
            BgraEncoder::new(config.width, config.height, config.fps, config.bitrate_kbps)
                .map_err(|e| e.to_string())?;

        let track = Arc::new(TrackLocalStaticSample::new(
            h264_capability(),
            "video".to_owned(),
            "ozma".to_owned(),
        ));

        let bitrate_kbps = Arc::new(AtomicU32::new(config.bitrate_kbps));
        let peers = Arc::new(AtomicUsize::new(0));
        let request_keyframe = Arc::new(AtomicBool::new(true));
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(Notify::new());
        let stats = Arc::new(Stats::default());

        let sample_duration = Duration::from_secs_f64(1.0 / f64::from(config.fps.max(1)));
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(2);
        let (sample_tx, mut sample_rx) = mpsc::channel::<Vec<u8>>(2);

        // Encoder thread: CPU work stays off the async runtime threads.
        let encoder_track = track.clone();
        let encoder_bitrate = bitrate_kbps.clone();
        let encoder_peers = peers.clone();
        let encoder_request_keyframe = request_keyframe.clone();
        let encoder_shutdown = shutdown_flag.clone();
        let encoder_stats = stats.clone();
        let coded_width = encoder.coded_width();
        let coded_height = encoder.coded_height();
        std::thread::Builder::new()
            .name("ozma-h264-encode".to_owned())
            .spawn(move || {
                run_encoder(
                    encoder,
                    frame_rx,
                    sample_tx,
                    encoder_bitrate,
                    encoder_peers,
                    encoder_request_keyframe,
                    encoder_shutdown,
                    encoder_stats,
                    coded_width,
                    coded_height,
                );
            })
            .map_err(|error| format!("could not start the encoder thread: {error}"))?;

        // RTP writer task: packetizes into the shared track.
        let writer_track: Arc<TrackLocalStaticSample> = encoder_track;
        let writer_shutdown = shutdown.clone();
        let writer_flag = shutdown_flag.clone();
        let writer_stats = stats.clone();
        let writer = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = writer_shutdown.notified() => break,
                    next = sample_rx.recv() => {
                        let Some(data) = next else { break };
                        if writer_flag.load(Ordering::SeqCst) {
                            break;
                        }
                        let sample = Sample {
                            data: data.into(),
                            duration: sample_duration,
                            ..Default::default()
                        };
                        if let Err(error) = writer_track.write_sample(&sample).await {
                            // A failed binding usually means the peer closed;
                            // keep the shared track for every other peer.
                            debug!(error = %error, "RTP sample write failed");
                            writer_stats.samples_dropped_full.fetch_add(1, Ordering::Relaxed);
                        } else {
                            writer_stats.samples_written.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Shared-memory reader task.
        let reader_shutdown = shutdown.clone();
        let reader_flag = shutdown_flag.clone();
        let reader_peers = peers.clone();
        let reader_stats = stats.clone();
        let shm_path = config.shm_path.clone();
        let fps = config.fps.max(1);
        let reader = tokio::spawn(async move {
            let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_error: Option<String> = None;
            loop {
                tokio::select! {
                    _ = reader_shutdown.notified() => break,
                    _ = ticker.tick() => {}
                }
                if reader_flag.load(Ordering::SeqCst) {
                    break;
                }
                if reader_peers.load(Ordering::SeqCst) == 0 {
                    continue;
                }
                match read_frame(&shm_path, spec).await {
                    Ok(frame) => {
                        reader_stats.frames_read.fetch_add(1, Ordering::Relaxed);
                        match frame_tx.try_send(frame) {
                            Ok(()) => {}
                            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                                reader_stats
                                    .frames_dropped_full
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                        }
                    }
                    Err(error) => {
                        let message = error.to_string();
                        if last_error.as_deref() != Some(message.as_str()) {
                            warn!(error = %message, "shared-memory frame unavailable");
                            last_error = Some(message);
                        }
                    }
                }
            }
        });

        // Periodic bounded logging while peers are connected.
        let stats_task_stats = stats.clone();
        let stats_task_peers = peers.clone();
        let stats_task_bitrate = bitrate_kbps.clone();
        let stats_task_shutdown = shutdown.clone();
        let stats_task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(10));
            loop {
                tokio::select! {
                    _ = stats_task_shutdown.notified() => break,
                    _ = ticker.tick() => {}
                }
                let peers = stats_task_peers.load(Ordering::Relaxed);
                if peers == 0 {
                    continue;
                }
                info!(
                    peers,
                    bitrate_kbps = stats_task_bitrate.load(Ordering::Relaxed),
                    frames_read = stats_task_stats.frames_read.load(Ordering::Relaxed),
                    frames_encoded = stats_task_stats.frames_encoded.load(Ordering::Relaxed),
                    frames_dropped_full =
                        stats_task_stats.frames_dropped_full.load(Ordering::Relaxed),
                    encode_errors = stats_task_stats.encode_errors.load(Ordering::Relaxed),
                    samples_written = stats_task_stats.samples_written.load(Ordering::Relaxed),
                    samples_dropped = stats_task_stats
                        .samples_dropped_full
                        .load(Ordering::Relaxed),
                    "native WebRTC video pipeline"
                );
            }
        });

        Ok(Arc::new(Self {
            track,
            bitrate_kbps,
            peers,
            request_keyframe,
            shutdown_flag,
            shutdown,
            stats,
            tasks: Mutex::new(vec![writer, reader, stats_task]),
        }))
    }

    /// Register a peer and request an IDR so its first sample is decodable.
    pub fn peer_guard(self: &Arc<Self>) -> PeerGuard {
        self.peers.fetch_add(1, Ordering::SeqCst);
        self.request_keyframe.store(true, Ordering::SeqCst);
        PeerGuard {
            stream: self.clone(),
        }
    }

    /// Apply a requested bps bitrate; returns the clamped value.
    pub fn set_bitrate_bps(&self, requested_bps: u64) -> u32 {
        let (clamped, kbps) = crate::signaling::bitrate_kbps_for(requested_bps);
        let previous = self.bitrate_kbps.swap(kbps, Ordering::SeqCst);
        if previous != kbps {
            self.stats.bitrate_changes.fetch_add(1, Ordering::Relaxed);
            self.request_keyframe.store(true, Ordering::SeqCst);
            info!(bitrate_bps = clamped, "video bitrate change requested");
        }
        clamped
    }

    pub fn stats_snapshot(&self) -> String {
        format!(
            "frames_read={} frames_encoded={} frames_dropped_full={} encode_errors={} samples_written={} samples_dropped={}",
            self.stats.frames_read.load(Ordering::Relaxed),
            self.stats.frames_encoded.load(Ordering::Relaxed),
            self.stats.frames_dropped_full.load(Ordering::Relaxed),
            self.stats.encode_errors.load(Ordering::Relaxed),
            self.stats.samples_written.load(Ordering::Relaxed),
            self.stats.samples_dropped_full.load(Ordering::Relaxed),
        )
    }

    /// Stop the pipeline tasks within a bounded time.
    pub async fn shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        self.shutdown.notify_waiters();
        let tasks: Vec<JoinHandle<()>> = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
            std::mem::take(&mut *tasks)
        };
        for task in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_encoder(
    mut encoder: BgraEncoder,
    frame_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    sample_tx: mpsc::Sender<Vec<u8>>,
    bitrate_kbps: Arc<AtomicU32>,
    peers: Arc<AtomicUsize>,
    request_keyframe: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<Stats>,
    coded_width: u32,
    coded_height: u32,
) {
    info!(
        coded_width,
        coded_height, "H.264 encoder ready (OpenH264 baseline, real-time screen content)"
    );
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let frame = match frame_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(frame) => frame,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if peers.load(Ordering::SeqCst) == 0 {
            continue;
        }

        let wanted = bitrate_kbps.load(Ordering::SeqCst);
        match encoder.set_bitrate(wanted) {
            Ok(true) => info!(
                bitrate_kbps = encoder.bitrate_kbps(),
                "encoder rebuilt for new bitrate"
            ),
            Ok(false) => {}
            Err(error) => {
                stats.encode_errors.fetch_add(1, Ordering::Relaxed);
                warn!(error = %error, "could not apply bitrate; keeping the previous encoder");
            }
        }

        let force = request_keyframe.swap(false, Ordering::SeqCst);
        match encoder.encode(&frame, force) {
            Ok(access_unit) => {
                stats.frames_encoded.fetch_add(1, Ordering::Relaxed);
                // Bounded handoff: if the RTP writer is behind, drop the
                // newest sample rather than buffering without limit.
                if let Err(mpsc::error::TrySendError::Full(_)) = sample_tx.try_send(access_unit) {
                    stats.samples_dropped_full.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(error) => {
                stats.encode_errors.fetch_add(1, Ordering::Relaxed);
                warn!(error = %error, "H.264 encode failed; frame skipped");
            }
        }
    }
    debug!("encoder thread stopped");
}

struct SessionEntry {
    pc: Arc<RTCPeerConnection>,
    _peer: PeerGuard,
}

/// The signaling server and its global session table.
pub struct NativeHelper {
    config: ServerConfig,
    video: Arc<VideoStream>,
    sessions: Arc<Mutex<HashMap<String, SessionEntry>>>,
}

/// The single H.264 capability this helper sends.
///
/// The fmtp string must match both the media-engine registration and the
/// track, otherwise `webrtc-rs`'s fuzzy codec search cannot bind them.
fn h264_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"
            .to_owned(),
        rtcp_feedback: vec![],
    }
}

/// Opus is registered only so an offered audio m-line can be answered
/// `inactive` with a valid mid/rtpmap. Native audio capture is not implemented.
/// True for an IPv6 link-local candidate (`fe80::/10`).
///
/// `webrtc-rs` can emit these even when the host refuses to bind the
/// link-local socket (the log shows `could not listen udp fe80...`). A peer
/// cannot route a link-local address without the original scope id, so trying
/// such a pair only delays ICE. They are omitted from the answer; loopback,
/// ULA, and global IPv6 candidates are kept.
fn is_link_local_candidate(candidate: &str) -> bool {
    candidate
        .split_whitespace()
        .nth(4)
        .is_some_and(|address| address.to_ascii_lowercase().starts_with("fe80:"))
}

fn opus_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: "audio/OPUS".to_owned(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

impl NativeHelper {
    /// Build the shared video pipeline.
    pub fn new(config: ServerConfig) -> Result<Arc<Self>, String> {
        let video = VideoStream::start(config.video.clone())?;
        Ok(Arc::new(Self {
            config,
            video,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }))
    }

    /// Serve until `shutdown` fires. Creates the socket before returning.
    pub async fn run(self: Arc<Self>, shutdown: Arc<Notify>) -> Result<(), String> {
        if let Some(parent) = self.config.socket_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    format!(
                        "could not create signaling directory `{}`: {error}",
                        parent.display()
                    )
                })?;
            }
        }
        let _ = std::fs::remove_file(&self.config.socket_path);
        let listener = UnixListener::bind(&self.config.socket_path).map_err(|error| {
            format!(
                "could not bind signaling socket `{}`: {error}",
                self.config.socket_path.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &self.config.socket_path,
                std::fs::Permissions::from_mode(0o777),
            );
        }
        info!(
            socket = %self.config.socket_path.display(),
            width = self.config.video.width,
            height = self.config.video.height,
            fps = self.config.video.fps,
            "native WebRTC helper ready"
        );

        loop {
            tokio::select! {
                _ = shutdown.notified() => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _addr)) => {
                            let helper = self.clone();
                            tokio::spawn(async move {
                                helper.handle_connection(stream).await;
                            });
                        }
                        Err(error) => {
                            warn!(error = %error, "signaling accept failed");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                }
            }
        }

        info!("shutting down native WebRTC helper");
        self.close_all_sessions().await;
        self.video.shutdown().await;
        info!(stats = %self.video.stats_snapshot(), "final pipeline counters");
        let _ = std::fs::remove_file(&self.config.socket_path);
        Ok(())
    }

    async fn handle_connection(self: Arc<Self>, stream: UnixStream) {
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let mut owned: Vec<String> = Vec::new();

        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => {
                    debug!(error = %error, "signaling read failed");
                    break;
                }
            };
            if line.trim().is_empty() {
                continue;
            }

            let response = match parse_message(&line) {
                Ok(ClientMessage::Offer { sdp }) => match self.create_session(&sdp).await {
                    Ok((answer, session)) => {
                        owned.push(session.clone());
                        answer_response(&answer, &session)
                    }
                    Err(error) => {
                        warn!(error = %error, "offer rejected");
                        error_response(&error)
                    }
                },
                Ok(ClientMessage::AddIceCandidate {
                    session,
                    candidate,
                    sdp_mline_index,
                }) => match self
                    .add_ice_candidate(session.as_deref(), &candidate, sdp_mline_index)
                    .await
                {
                    Ok(remote_candidates) => candidate_ack(session.as_deref(), remote_candidates),
                    Err(error) => error_ack(&error),
                },
                Ok(ClientMessage::Bitrate { bitrate_bps }) => {
                    let clamped = self.video.set_bitrate_bps(bitrate_bps);
                    bitrate_ack(clamped)
                }
                Err(error) => error_response(&error.to_string()),
            };

            if writer.write_all(response.as_bytes()).await.is_err()
                || writer.write_all(b"\n").await.is_err()
                || writer.flush().await.is_err()
            {
                break;
            }
        }

        for session in owned {
            self.close_session(&session).await;
        }
    }

    async fn create_session(&self, offer_sdp: &str) -> Result<(String, String), String> {
        let session_count = self
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len();
        if session_count >= self.config.max_sessions {
            return Err(format!(
                "too many WebRTC sessions (bound {})",
                self.config.max_sessions
            ));
        }

        let session = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();

        // Reuse the peer's offered H.264 payload type. A fresh media engine per
        // answer keeps the sender's packetizer on the same PT the answer
        // advertises, which GStreamer receivers require.
        let offered_pt = offered_payload_type(offer_sdp, "video", "H264").unwrap_or(96);
        let offered_audio_pt = offered_payload_type(offer_sdp, "audio", "OPUS");
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: h264_capability(),
                    payload_type: offered_pt,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )
            .map_err(|error| {
                format!("could not register H.264 payload type {offered_pt}: {error}")
            })?;
        if let Some(audio_pt) = offered_audio_pt {
            media_engine
                .register_codec(
                    RTCRtpCodecParameters {
                        capability: opus_capability(),
                        payload_type: audio_pt,
                        ..Default::default()
                    },
                    RTPCodecType::Audio,
                )
                .map_err(|error| {
                    format!("could not register Opus payload type {audio_pt}: {error}")
                })?;
        }
        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)
            .map_err(|error| format!("could not install WebRTC interceptors: {error}"))?;
        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        let mut configuration = RTCConfiguration::default();
        configuration.ice_servers = self.config.ice_servers.clone();
        let pc = Arc::new(
            api.new_peer_connection(configuration)
                .await
                .map_err(|error| format!("could not create peer connection: {error}"))?,
        );

        let candidates: Arc<tokio::sync::Mutex<Vec<(u16, String)>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let candidate_sink = candidates.clone();
        pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
            let candidate_sink = candidate_sink.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    // `to_json` is the only stable way to get the candidate
                    // line in webrtc 0.11. It reports m-line 0, which is the
                    // video m-line for this helper's single-track answer.
                    if let Ok(init) = candidate.to_json() {
                        if !init.candidate.is_empty() {
                            candidate_sink
                                .lock()
                                .await
                                .push((init.sdp_mline_index.unwrap_or(0), init.candidate));
                        }
                    }
                }
            })
        }));

        // Terminal-state cleanup removes the session even when the peer
        // disappears without closing the signaling socket.
        let sessions = self.sessions.clone();
        let session_for_state = session.clone();
        pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            let sessions = sessions.clone();
            let session = session_for_state.clone();
            Box::pin(async move {
                if matches!(
                    state,
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                ) {
                    let entry = sessions
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&session);
                    if let Some(entry) = entry {
                        let _ = entry.pc.close().await;
                        info!(session = %session, state = ?state, "WebRTC session ended");
                    }
                }
            })
        }));

        pc.add_track(self.video.track.clone())
            .await
            .map_err(|error| format!("could not attach the video track: {error}"))?;

        // An offered Opus m-line gets a real transceiver so the answer carries
        // its mid and is answered `inactive`. Without this webrtc-rs emits a
        // bare rejected m-line with no mid, which breaks a BUNDLE answer for
        // GStreamer peers that offered audio.
        let audio_transceiver = match offered_audio_pt {
            Some(_) => Some(
                pc.add_transceiver_from_kind(RTPCodecType::Audio, None)
                    .await
                    .map_err(|error| format!("could not add the audio m-line: {error}"))?,
            ),
            None => None,
        };

        let remote_candidates = remote_ice_candidates(offer_sdp).len();
        let offer = RTCSessionDescription::offer(offer_sdp.to_owned())
            .map_err(|error| format!("could not parse the offer SDP: {error}"))?;
        pc.set_remote_description(offer)
            .await
            .map_err(|error| format!("could not apply the offer: {error}"))?;
        if let Some(audio) = audio_transceiver {
            // Video-only helper: never send or receive native audio.
            audio
                .set_direction(RTCRtpTransceiverDirection::Inactive)
                .await;
        }

        // Register the gather-complete waiter before installing the local
        // description, which is what starts gathering.
        let mut gathered = pc.gathering_complete_promise().await;
        let answer = pc
            .create_answer(None)
            .await
            .map_err(|error| format!("could not create the answer: {error}"))?;
        if answer.sdp_type != RTCSdpType::Answer {
            return Err("create_answer did not return an answer description".to_owned());
        }
        pc.set_local_description(answer)
            .await
            .map_err(|error| format!("could not install the answer: {error}"))?;

        if tokio::time::timeout(self.config.gather_timeout, gathered.recv())
            .await
            .is_err()
        {
            warn!(
                session = %session,
                "ICE gathering did not complete before the answer was sent"
            );
        }

        let local = pc
            .local_description()
            .await
            .ok_or_else(|| "the peer connection has no local description".to_owned())?;
        let mut candidates = candidates.lock().await.clone();
        let gathered = candidates.len();
        candidates.retain(|(_, candidate)| !is_link_local_candidate(candidate));
        if candidates.len() != gathered {
            debug!(
                omitted = gathered - candidates.len(),
                "omitted unroutable IPv6 link-local ICE candidates"
            );
        }
        let answer_sdp = append_ice_candidates(&local.sdp, &candidates);
        let type_counts = candidate_type_counts(
            &candidates
                .iter()
                .map(|(index, candidate)| (*index as usize, candidate.clone()))
                .collect::<Vec<_>>(),
        );
        let type_summary = if type_counts.is_empty() {
            "none".to_owned()
        } else {
            type_counts
                .iter()
                .map(|(name, count)| format!("{name}={count}"))
                .collect::<Vec<_>>()
                .join(",")
        };

        let peer = self.video.peer_guard();
        self.sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                session.clone(),
                SessionEntry {
                    pc: pc.clone(),
                    _peer: peer,
                },
            );
        info!(
            session = %session,
            width = self.config.video.width,
            height = self.config.video.height,
            fps = self.config.video.fps,
            ice_candidates = candidates.len(),
            ice_types = %type_summary,
            remote_candidates,
            "WebRTC session answered"
        );
        Ok((answer_sdp, session))
    }

    async fn add_ice_candidate(
        &self,
        session: Option<&str>,
        candidate: &str,
        sdp_mline_index: u32,
    ) -> Result<usize, String> {
        let targets: Vec<(String, Arc<RTCPeerConnection>)> = {
            let sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            match session {
                Some(id) => match sessions.get(id) {
                    Some(entry) => vec![(id.to_owned(), entry.pc.clone())],
                    None => return Err("unknown WebRTC session".to_owned()),
                },
                None => sessions
                    .iter()
                    .map(|(id, entry)| (id.clone(), entry.pc.clone()))
                    .collect(),
            }
        };
        if targets.is_empty() {
            return Err("no live WebRTC session".to_owned());
        }
        let index =
            u16::try_from(sdp_mline_index).map_err(|_| "sdpMLineIndex is too large".to_owned())?;
        for (id, pc) in &targets {
            pc.add_ice_candidate(RTCIceCandidateInit {
                candidate: candidate.to_owned(),
                sdp_mid: None,
                sdp_mline_index: Some(index),
                username_fragment: None,
            })
            .await
            .map_err(|error| format!("session `{id}` rejected the candidate: {error}"))?;
        }
        Ok(targets.len())
    }

    async fn close_session(&self, session: &str) {
        let entry = self
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(session);
        if let Some(entry) = entry {
            let _ = entry.pc.close().await;
        }
    }

    async fn close_all_sessions(&self) {
        let entries: Vec<SessionEntry> = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            sessions.drain().map(|(_, entry)| entry).collect()
        };
        for entry in entries {
            let _ = entry.pc.close().await;
        }
    }
}

/// Build the ICE server list from the CLI's normalised addresses.
pub fn ice_servers(
    stun_server: Option<&str>,
    turn_server: Option<&str>,
    turn_user: Option<&str>,
    turn_password: Option<&str>,
) -> Result<Vec<RTCIceServer>, String> {
    let mut servers = Vec::new();
    if let Some(stun) = stun_server {
        servers.push(RTCIceServer {
            urls: vec![crate::signaling::normalize_ice_server(stun, IceKind::Stun)
                .map_err(|e| e.to_string())?],
            ..Default::default()
        });
    }
    if let Some(turn) = turn_server {
        servers.push(RTCIceServer {
            urls: vec![crate::signaling::normalize_ice_server(turn, IceKind::Turn)
                .map_err(|e| e.to_string())?],
            username: turn_user.unwrap_or_default().to_owned(),
            credential: turn_password.unwrap_or_default().to_owned(),
            credential_type: RTCIceCredentialType::Password,
        });
    }
    Ok(servers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_eight_hex_characters() {
        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn ice_server_list_is_built_only_from_configured_values() {
        assert!(ice_servers(None, None, None, None).unwrap().is_empty());
        let servers = ice_servers(Some("stun.example.org"), None, None, None).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].urls, vec!["stun:stun.example.org:3478"]);

        let servers = ice_servers(
            None,
            Some("turn.example.org:3478"),
            Some("user"),
            Some("secret"),
        )
        .unwrap();
        assert_eq!(servers[0].urls, vec!["turn:turn.example.org:3478"]);
        assert_eq!(servers[0].username, "user");
        assert_eq!(servers[0].credential, "secret");
        assert!(matches!(
            servers[0].credential_type,
            RTCIceCredentialType::Password
        ));
    }

    #[test]
    fn link_local_candidates_are_identified_without_matching_ula_or_ipv4() {
        assert!(is_link_local_candidate(
            "candidate:1 1 UDP 1 fe80::ae51:5ab:7037:c407 5000 typ host"
        ));
        assert!(is_link_local_candidate(
            "candidate:1 1 UDP 1 FE80::1 5000 typ host"
        ));
        assert!(!is_link_local_candidate(
            "candidate:1 1 UDP 1 fded:1d3f:768d:4e91::1 5000 typ host"
        ));
        assert!(!is_link_local_candidate(
            "candidate:1 1 UDP 1 192.0.2.1 5000 typ host"
        ));
        assert!(!is_link_local_candidate("candidate:1 1 UDP 1"));
    }

    #[test]
    fn invalid_ice_server_is_a_startup_error() {
        assert!(ice_servers(Some("turn:host:3478"), None, None, None).is_err());
    }
}
