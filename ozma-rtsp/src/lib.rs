// SPDX-License-Identifier: AGPL-3.0-only
//! RTSP 1.0 server and interleaved RTP transport for Ozma.
//!
//! This is the compatibility adapter from `docs/node-hermes-parity.md` §9 (and
//! the M2 RTSP acceptance work): an H.264 access-unit source is packetised by
//! [`ozma_rtp`] and delivered either as RTP over TCP with `$` interleaving or as
//! plain RTP/UDP. It has no dependency on Ozma's native QUIC transport.
//!
//! | Piece | Role |
//! |---|---|
//! | [`RtspServer`] | accept loop, per-connection request handling, media tasks |
//! | [`RtspConfig`] | MTU, payload type, timeout, UDP enablement |
//! | [`EncodedVideoSource`] | pull-based source of [`ozma_media::EncodedFrame`] |
//! | [`ChannelVideoSource`] | bridge from an encoder or pipeline channel |
//! | [`VecVideoSource`] | finite in-memory source for tests and demos |
//! | [`parse_request`] / [`RtspRequest`] | standalone request parser and CSeq/Session helpers |
//! | [`h264_sdp`] | one-track H.264 SDP |
//! | [`parse_transport`] | TCP-interleaved and UDP `Transport:` negotiation |
//!
//! # Example
//!
//! ```no_run
//! use std::time::Duration;
//! use ozma_rtsp::{ChannelVideoSource, RtspConfig, RtspServer};
//! use tokio::net::TcpListener;
//!
//! # async fn run(frames: Vec<ozma_media::EncodedFrame>) -> std::io::Result<()> {
//! let listener = TcpListener::bind("0.0.0.0:8554").await?;
//! let frames = std::sync::Arc::new(frames);
//! let server = RtspServer::new(RtspConfig::default()).with_source_factory({
//!     let frames = frames.clone();
//!     move || Box::new(ozma_rtsp::VecVideoSource::new((*frames).clone()))
//! });
//! server.serve(listener).await
//! # }
//! ```
//!
//! # Supported RTSP surface
//!
//! * `OPTIONS`, `DESCRIBE`, `SETUP`, `PLAY`, `PAUSE`, `TEARDOWN`,
//!   `GET_PARAMETER`, `SET_PARAMETER`; unknown methods get `501`.
//! * `CSeq` echoed, `Session` returned with `timeout=`, `Content-Length`
//!   honoured on requests and emitted on responses with bodies.
//! * `RTP/AVP/TCP;unicast;interleaved=0-1` (default channels 0/1).
//! * `RTP/AVP;unicast;client_port=N-M` (and `RTP/AVP/UDP`) sends RTP/UDP to the
//!   client's address and reports `server_port=S1-S2`.
//! * One H.264 track in SDP with `packetization-mode=1` and a relative
//!   `trackID=0` control URL.
//!
//! # Session and media notes
//!
//! * `DESCRIBE` creates a session, `SETUP` selects the transport, `PLAY`
//!   starts a fresh [`EncodedVideoSource`] from the factory, `PAUSE` stops it,
//!   and `TEARDOWN` removes it. Dropping the TCP connection cleans everything
//!   up. Sessions idle past [`RtspConfig::session_timeout`] are closed.
//!   There is no resume or seek: a `PLAY` after `PAUSE` asks the factory for a
//!   new source.
//! * Each `PLAY` gets its own packetizer, sequence number, SSRC, and timestamp
//!   clock. RTP timestamps come from the frame PTS in 90 kHz units, with a
//!   sequence/fps fallback for encoders that leave PTS at zero.
//! * `PLAY` starts consuming the factory's source immediately. The source
//!   should begin with a keyframe (SPS/PPS/IDR) or the client shows nothing
//!   until the next IDR; the packetizer repeats cached SPS/PPS before later
//!   IDRs but cannot synthesise a keyframe.
//! * RTP/RTCP is sent on the requested interleaved RTP channel. RTCP is not
//!   generated yet, so the RTCP channel stays silent.
//!
//! # Deliberate limits (remaining adapter work)
//!
//! * **No HTTPS pairing, GameStream/ENet control, or `x-nv-*` negotiation.**
//!   Moonlight/Hestia pairing, app list/launch, AES-GCM RTSP encryption, and
//!   the control channel remain in `controller/gaming/moonlight_*.py` until a
//!   native pairing adapter is written.
//! * **H.264 only, one track.** Audio, HEVC, multi-track SDP, and `a=control`
//!   stream IDs beyond `trackID=0` are future work.
//! * **No RTCP.** Sender/receiver reports, NACK/PLI/FIR, keyframe requests, and
//!   RTP-Info `rtptime` are not implemented. The server cannot react to loss.
//! * **No SRTP or TURN.** RTP/UDP is plaintext and assumes a trusted LAN or
//!   tunnel (`ssh -L`, WireGuard), matching the documented adapter intent.
//! * **No RTSP authentication or TLS.** Bind behind a trusted network or
//!   tunnel and add the pairing/auth adapter before exposing this server
//!   publicly. `rtsps://` URIs are handled as ordinary RTSP.
//! * **No pacing/jitter buffer.** Frames are sent as the source provides them.
//!   A real-time encoder is expected upstream; a burst source can overrun a
//!   slow client's socket. TCP writes also backpressure the media task.
//! * **UDP RTCP port is reserved but silent.** The client's RTCP address is
//!   known and the server RTCP port is advertised, ready for a later report
//!   implementation.
//! * **No HLS/LL-HLS packaging.** `ozma-rtp` is transport-neutral and can feed
//!   a future fMP4/TS packager, but segmenting, playlists, and HTTP delivery
//!   are not part of this crate.
//! * **Single client per RTSP connection.** Sessions are scoped to the
//!   connection; a shared cross-connection session broker belongs to M3.
//!
//! The crate is `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

pub mod error;
pub mod request;
pub mod sdp;
pub mod server;
pub mod session;
pub mod source;

pub use error::RtspError;
pub use request::{
    parse_request, parse_session_header, Headers, Method, RtspRequest, RtspResponse, MAX_BODY_BYTES,
};
pub use sdp::{h264_sdp, SdpConfig};
pub use server::{RtspConfig, RtspServer, SourceFactory};
pub use session::{parse_transport, SessionState, TransportRequest};
pub use source::{ChannelVideoSource, EncodedVideoSource, VecVideoSource};
