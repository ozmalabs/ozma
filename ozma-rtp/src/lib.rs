// SPDX-License-Identifier: AGPL-3.0-only
//! H.264 RTP packetization and depacketization for Ozma.
//!
//! This crate is the reusable packetiser described in
//! `docs/node-hermes-parity.md` §8: it has no RTSP, SDP, or QUIC dependency and
//! can feed raw RTP/UDP, RTSP interleaved framing, or a future datagram
//! transport.
//!
//! | Type | Role |
//! |---|---|
//! | [`H264Packetizer`] | Annex-B access unit → RTP packets (single NAL, STAP-A, FU-A) |
//! | [`H264Depacketizer`] | RTP packets → Annex-B access unit, tolerating reordering |
//! | [`RtpHeader`] | validated RTP header with payload range |
//! | [`PacketizerConfig`] / [`DepacketizerConfig`] | MTU, payload type, and buffer limits |
//! | [`PacketizerStats`] / [`DepacketizerStats`] | packet, byte, keyframe, and loss counters |
//!
//! # Packetization policy
//!
//! * A NAL that fits in one packet becomes a single-NAL packet.
//! * A larger NAL is split into FU-A fragments sized from the MTU.
//! * SPS/PPS NAL units found in an access unit are aggregated into one STAP-A
//!   packet at the front when both fit. The packetizer also caches the most
//!   recent SPS/PPS and inserts any missing cached set before an IDR access
//!   unit, so a client that joins between keyframes can decode the next one.
//!   Set [`PacketizerConfig::repeat_parameter_sets`] to `false` for strict
//!   byte-for-byte round-trips.
//! * The marker bit is set on the last packet of every access unit.
//! * The RTP timestamp is supplied by the caller in 90 kHz units;
//!   [`micros_to_rtp_timestamp`] and [`RtpTimestampClock`] are provided as
//!   convenience helpers.
//!
//! # Example
//!
//! ```
//! use ozma_rtp::{H264Depacketizer, H264Packetizer};
//!
//! let access_unit = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce,
//!                    0, 0, 0, 1, 0x65, 0x88, 0x84];
//! let mut packetizer = H264Packetizer::new(0x1234_5678, 1200).unwrap();
//! let packets = packetizer.packetize(&access_unit, 90_000, true).unwrap();
//!
//! let mut depacketizer = H264Depacketizer::new();
//! let mut unit = None;
//! for packet in &packets {
//!     if let Some(complete) = depacketizer.push(packet).unwrap() {
//!         unit = Some(complete);
//!     }
//! }
//! let unit = unit.expect("marker closes the access unit");
//! assert!(unit.keyframe);
//! assert_eq!(unit.timestamp, 90_000);
//! ```
//!
//! # Deliberate limits
//!
//! * **H.264 only.** HEVC, AV1, VP8/9, and audio payloads are out of scope.
//! * **Only single-NAL, STAP-A, and FU-A.** STAP-B, MTAP, FU-B, and
//!   interleaved mode are rejected with a clear error; `packetization-mode=1`
//!   clients do not need them.
//! * **No FEC or RTCP.** Reed-Solomon FEC, receiver reports, NACK/PLI, and
//!   pacing belong to a later transport layer.
//! * **No jitter buffer/timers.** Reordering is handled by sequence number and
//!   timestamp while packets arrive. A gap that never fills is counted by
//!   [`H264Depacketizer::flush_incomplete`]; the caller decides when to call
//!   it. Reordering where the marker packet arrives before *all* earlier
//!   packets is only recovered if the earlier packets arrive before finalising
//!   (the normal case on a real link).
//! * **SSRC/sequence are local state.** SSRC collision detection and RTCP
//!   sender reports are the caller's responsibility.
//!
//! The crate is `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

pub mod depacketizer;
pub mod error;
pub mod header;
pub mod nal;
pub mod packetizer;

pub use depacketizer::{
    DepacketizedAccessUnit, DepacketizerConfig, DepacketizerStats, H264Depacketizer,
    DEFAULT_MAX_ACCESS_UNIT_BYTES, DEFAULT_MAX_PENDING_UNITS, DEFAULT_MAX_SEQUENCE_SPAN,
};
pub use error::RtpError;
pub use header::{RtpHeader, RTP_HEADER_LEN};
pub use nal::{
    annex_b_is_keyframe, is_idr, is_parameter_set, nal_type, push_annex_b, split_annex_b,
    NAL_TYPE_IDR, NAL_TYPE_PPS, NAL_TYPE_SPS,
};
pub use packetizer::{
    micros_to_rtp_timestamp, nals_to_annex_b, H264Packetizer, PacketizerConfig, PacketizerStats,
    RtpTimestampClock, DEFAULT_MAX_ACCESS_UNIT_BYTES as DEFAULT_PACKETIZER_MAX_ACCESS_UNIT_BYTES,
    DEFAULT_MTU, DEFAULT_PAYLOAD_TYPE, MIN_MTU,
};
