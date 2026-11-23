// SPDX-License-Identifier: AGPL-3.0-only
//! Measured path selection.
//!
//! [`TransportDirector`] is deliberately policy-driven rather than tied to a
//! socket. Agents feed it the candidates the node can currently offer together
//! with fresh measurements; it filters out paths that cannot carry the session
//! and returns the best one with a human-readable reason and a structured list
//! of rejections. That reason is meant to be surfaced in diagnostics, so users
//! see "relay because direct UDP lost 18% of packets" instead of a black
//! screen.
//!
//! Default policy (`Tiered`): lower-tier transports win only when every
//! higher-tier transport is not viable. The tier order is loopback, QUIC, raw
//! UDP, TCP-interleaved, Unix/SSH, relay, HLS. Within a tier the lowest RTT
//! wins, then jitter, loss, and finally higher throughput. [`SelectionStrategy::LowestLatency`]
//! instead ranks every viable path by RTT, with the tier as a tie-break; use it
//! when the node measured all paths on the same network and latency matters
//! more than the transport's features.

use std::cmp::Ordering;

use ozma_protocol::{Result, TransportDescriptor, TransportMode, TransportPreference};

use crate::channel::ChannelMap;
use crate::error::transport_unavailable;

/// A point-in-time measurement of a candidate path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinkQuality {
    /// Smoothed round-trip time.
    pub rtt_ms: f32,
    /// Mean absolute RTT variation.
    pub jitter_ms: f32,
    /// Packet loss as a percentage in `0.0..=100.0`.
    pub loss_percent: f32,
    /// Measured or advertised throughput.
    pub throughput_kbps: u32,
}

impl LinkQuality {
    /// Construct a measurement.
    pub const fn new(
        rtt_ms: f32,
        jitter_ms: f32,
        loss_percent: f32,
        throughput_kbps: u32,
    ) -> Self {
        Self {
            rtt_ms,
            jitter_ms,
            loss_percent,
            throughput_kbps,
        }
    }

    /// An idealised local measurement, useful for loopback and tests.
    pub const fn ideal() -> Self {
        Self {
            rtt_ms: 0.0,
            jitter_ms: 0.0,
            loss_percent: 0.0,
            throughput_kbps: u32::MAX,
        }
    }

    fn measurements_are_valid(&self) -> bool {
        self.rtt_ms.is_finite()
            && self.rtt_ms >= 0.0
            && self.jitter_ms.is_finite()
            && self.jitter_ms >= 0.0
            && self.loss_percent.is_finite()
            && (0.0..=100.0).contains(&self.loss_percent)
    }
}

/// One offered transport plus its current measurement.
#[derive(Clone, Debug, PartialEq)]
pub struct TransportCandidate {
    /// The endpoint and channel layout the node can offer.
    pub descriptor: TransportDescriptor,
    /// Fresh measurement for that endpoint.
    pub quality: LinkQuality,
}

impl TransportCandidate {
    /// Pair a descriptor with a measurement.
    pub fn new(descriptor: TransportDescriptor, quality: LinkQuality) -> Self {
        Self {
            descriptor,
            quality,
        }
    }

    /// Shorthand for the candidate's mode.
    pub fn mode(&self) -> TransportMode {
        self.descriptor.mode
    }
}

/// How [`TransportDirector`] ranks viable candidates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SelectionStrategy {
    /// Prefer the lowest transport tier first, then latency within it.
    #[default]
    Tiered,
    /// Prefer the lowest RTT across every viable transport, using the tier as
    /// a tie-break.
    LowestLatency,
}

/// Thresholds a candidate must meet to be viable.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransportPolicy {
    /// Reject paths with a higher smoothed RTT.
    pub max_rtt_ms: f32,
    /// Reject paths with a higher jitter.
    pub max_jitter_ms: f32,
    /// Reject paths with a higher loss percentage.
    pub max_loss_percent: f32,
    /// Reject paths with less throughput.
    pub min_throughput_kbps: u32,
    /// Candidate ranking strategy.
    pub strategy: SelectionStrategy,
}

impl Default for TransportPolicy {
    fn default() -> Self {
        Self {
            max_rtt_ms: 150.0,
            max_jitter_ms: 50.0,
            max_loss_percent: 5.0,
            min_throughput_kbps: 1500,
            strategy: SelectionStrategy::Tiered,
        }
    }
}

impl TransportPolicy {
    /// A policy suitable for an interactive LAN session.
    pub fn interactive() -> Self {
        Self::default()
    }

    /// A permissive policy for degraded WAN paths.
    pub fn permissive() -> Self {
        Self {
            max_rtt_ms: 400.0,
            max_jitter_ms: 150.0,
            max_loss_percent: 20.0,
            min_throughput_kbps: 500,
            strategy: SelectionStrategy::Tiered,
        }
    }

    /// A strict policy for high-bitrate video.
    pub fn strict() -> Self {
        Self {
            max_rtt_ms: 80.0,
            max_jitter_ms: 20.0,
            max_loss_percent: 1.0,
            min_throughput_kbps: 20_000,
            strategy: SelectionStrategy::Tiered,
        }
    }

    pub fn with_max_rtt_ms(mut self, value: f32) -> Self {
        self.max_rtt_ms = value;
        self
    }

    pub fn with_max_jitter_ms(mut self, value: f32) -> Self {
        self.max_jitter_ms = value;
        self
    }

    pub fn with_max_loss_percent(mut self, value: f32) -> Self {
        self.max_loss_percent = value;
        self
    }

    pub fn with_min_throughput_kbps(mut self, value: u32) -> Self {
        self.min_throughput_kbps = value;
        self
    }

    pub fn with_strategy(mut self, strategy: SelectionStrategy) -> Self {
        self.strategy = strategy;
        self
    }
}

/// The tier order used by [`SelectionStrategy::Tiered`].
///
/// Lower is better. Loopback is first because it cannot leave the process.
pub const fn mode_rank(mode: TransportMode) -> u8 {
    match mode {
        TransportMode::Loopback => 0,
        TransportMode::Quic => 1,
        TransportMode::Udp => 2,
        TransportMode::TcpInterleaved => 3,
        TransportMode::UnixSocket | TransportMode::Ssh => 4,
        TransportMode::Relay => 5,
        TransportMode::Hls => 6,
    }
}

/// Why a candidate was not viable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectionReason {
    /// Measurement contained NaN, a negative value, or invalid loss.
    InvalidMeasurement,
    /// The descriptor carries no channels.
    NoChannels,
    /// Channel descriptors are malformed (for example duplicate ids).
    InvalidChannels,
    /// Smoothed RTT exceeded the policy.
    ExcessiveRtt,
    /// Jitter exceeded the policy.
    ExcessiveJitter,
    /// Loss exceeded the policy.
    ExcessiveLoss,
    /// Throughput was below the policy.
    InsufficientThroughput,
}

impl RejectionReason {
    /// Stable label for diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidMeasurement => "invalid measurement",
            Self::NoChannels => "no channels",
            Self::InvalidChannels => "invalid channels",
            Self::ExcessiveRtt => "excessive rtt",
            Self::ExcessiveJitter => "excessive jitter",
            Self::ExcessiveLoss => "excessive loss",
            Self::InsufficientThroughput => "insufficient throughput",
        }
    }
}

/// A structured record of one rejected candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct PathRejection {
    /// Transport mode that was rejected.
    pub mode: TransportMode,
    /// Endpoint, when the descriptor had one.
    pub endpoint: Option<String>,
    /// Machine-readable reason.
    pub reason: RejectionReason,
    /// Human-readable measurement/threshold comparison.
    pub detail: String,
}

/// The chosen path, its measurement, and why it won.
#[derive(Clone, Debug, PartialEq)]
pub struct PathSelection {
    /// Descriptor to put in the client's [`ozma_protocol::SessionOffer`].
    pub descriptor: TransportDescriptor,
    /// Measurement that justified the choice.
    pub quality: LinkQuality,
    /// Documented explanation suitable for logs or a diagnostics card.
    pub reason: String,
    /// Every other candidate and why it lost.
    pub rejected: Vec<PathRejection>,
}

/// Chooses among measured transport candidates.
#[derive(Clone, Debug, Default)]
pub struct TransportDirector {
    policy: TransportPolicy,
    candidates: Vec<TransportCandidate>,
}

impl TransportDirector {
    /// Create a director with an explicit policy.
    pub fn new(policy: TransportPolicy) -> Self {
        Self {
            policy,
            candidates: Vec::new(),
        }
    }

    /// The policy in use.
    pub fn policy(&self) -> &TransportPolicy {
        &self.policy
    }

    /// The candidates currently known to the director.
    pub fn candidates(&self) -> &[TransportCandidate] {
        &self.candidates
    }

    /// Add or replace a measurement for an endpoint.
    ///
    /// Two descriptors for the same mode and endpoint are treated as the same
    /// path; the newer measurement replaces the older one.
    pub fn add_candidate(&mut self, candidate: TransportCandidate) -> &mut Self {
        if let Some(existing) = self.candidates.iter_mut().find(|existing| {
            existing.descriptor.mode == candidate.descriptor.mode
                && existing.descriptor.endpoint == candidate.descriptor.endpoint
        }) {
            *existing = candidate;
        } else {
            self.candidates.push(candidate);
        }
        self
    }

    /// Remove every candidate.
    pub fn clear(&mut self) {
        self.candidates.clear();
    }

    /// Select from the accumulated candidates.
    pub fn select(&self) -> Result<PathSelection> {
        Self::select_from(&self.candidates, &self.policy)
    }

    /// Select while honouring a session's fixed transport preference.
    ///
    /// `Auto` uses the policy normally. Any other preference restricts the
    /// candidates to that mode; if no such path is viable, the result is
    /// `transport.unavailable` rather than silently choosing another mode.
    pub fn select_for(&self, preference: TransportPreference) -> Result<PathSelection> {
        let wanted = match preference {
            TransportPreference::Auto => return self.select(),
            TransportPreference::Quic => Some(TransportMode::Quic),
            TransportPreference::Udp => Some(TransportMode::Udp),
            TransportPreference::TcpInterleaved => Some(TransportMode::TcpInterleaved),
            TransportPreference::Relay => Some(TransportMode::Relay),
            TransportPreference::Loopback => Some(TransportMode::Loopback),
        };
        let filtered: Vec<_> = self
            .candidates
            .iter()
            .filter(|candidate| Some(candidate.descriptor.mode) == wanted)
            .cloned()
            .collect();
        let mut selection = Self::select_from(&filtered, &self.policy)?;
        selection.reason = format!(
            "session requires {}; {}",
            preference_label(preference),
            selection.reason
        );
        Ok(selection)
    }

    /// Select from an arbitrary candidate slice.
    pub fn select_from(
        candidates: &[TransportCandidate],
        policy: &TransportPolicy,
    ) -> Result<PathSelection> {
        let mut viable: Vec<&TransportCandidate> = Vec::new();
        let mut rejected = Vec::new();

        for candidate in candidates {
            match evaluate(candidate, policy) {
                Ok(()) => viable.push(candidate),
                Err((reason, detail)) => rejected.push(PathRejection {
                    mode: candidate.descriptor.mode,
                    endpoint: candidate.descriptor.endpoint.clone(),
                    reason,
                    detail,
                }),
            }
        }

        if viable.is_empty() {
            let detail = if candidates.is_empty() {
                "no transport candidates were measured".to_string()
            } else {
                rejected
                    .iter()
                    .map(|rejection| {
                        format!(
                            "{}: {} ({})",
                            mode_label(rejection.mode),
                            rejection.reason.as_str(),
                            rejection.detail
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            return Err(transport_unavailable("no viable transport path")
                .with_details(detail)
                .with_remediation(
                    "Check the network, enable the relay, or relax the transport policy.",
                ));
        }

        viable.sort_by(|a, b| compare(a, b, policy.strategy));
        let chosen = viable[0];
        let rejected_here: Vec<PathRejection> = rejected
            .into_iter()
            .filter(|rejection| {
                // A path with the same endpoint/mode as the chosen one is not a
                // rejection, it is the measurement that won.
                !(rejection.mode == chosen.descriptor.mode
                    && rejection.endpoint == chosen.descriptor.endpoint)
            })
            .collect();

        let endpoint = chosen
            .descriptor
            .endpoint
            .as_deref()
            .unwrap_or("(unspecified endpoint)");
        let strategy_reason = match policy.strategy {
            SelectionStrategy::Tiered => {
                "it is the highest-priority viable transport and has the lowest rtt at that tier"
            }
            SelectionStrategy::LowestLatency => {
                "it has the lowest measured rtt among all viable transports"
            }
        };
        let mut reason = format!(
            "selected {} at {} because {}; rtt {:.1} ms, jitter {:.1} ms, loss {:.2}%, throughput {} kbps",
            mode_label(chosen.descriptor.mode),
            endpoint,
            strategy_reason,
            chosen.quality.rtt_ms,
            chosen.quality.jitter_ms,
            chosen.quality.loss_percent,
            chosen.quality.throughput_kbps,
        );
        if !rejected_here.is_empty() {
            let rejected_text = rejected_here
                .iter()
                .map(|rejection| {
                    format!(
                        "{} ({})",
                        mode_label(rejection.mode),
                        rejection.reason.as_str()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            reason.push_str(&format!("; rejected: {rejected_text}"));
        }

        Ok(PathSelection {
            descriptor: chosen.descriptor.clone(),
            quality: chosen.quality,
            reason,
            rejected: rejected_here,
        })
    }
}

fn evaluate(
    candidate: &TransportCandidate,
    policy: &TransportPolicy,
) -> std::result::Result<(), (RejectionReason, String)> {
    let quality = candidate.quality;
    if !quality.measurements_are_valid() {
        return Err((
            RejectionReason::InvalidMeasurement,
            format!(
                "rtt={} jitter={} loss={}",
                quality.rtt_ms, quality.jitter_ms, quality.loss_percent
            ),
        ));
    }
    if candidate.descriptor.channels.is_empty() {
        return Err((RejectionReason::NoChannels, "channel list is empty".into()));
    }
    if let Err(error) = ChannelMap::new(candidate.descriptor.channels.iter().cloned()) {
        return Err((RejectionReason::InvalidChannels, error.message));
    }
    if quality.rtt_ms > policy.max_rtt_ms {
        return Err((
            RejectionReason::ExcessiveRtt,
            format!("{:.1} ms > {:.1} ms", quality.rtt_ms, policy.max_rtt_ms),
        ));
    }
    if quality.jitter_ms > policy.max_jitter_ms {
        return Err((
            RejectionReason::ExcessiveJitter,
            format!("{:.1} ms > {:.1} ms", quality.jitter_ms, policy.max_jitter_ms),
        ));
    }
    if quality.loss_percent > policy.max_loss_percent {
        return Err((
            RejectionReason::ExcessiveLoss,
            format!(
                "{:.2}% > {:.2}%",
                quality.loss_percent, policy.max_loss_percent
            ),
        ));
    }
    if quality.throughput_kbps < policy.min_throughput_kbps {
        return Err((
            RejectionReason::InsufficientThroughput,
            format!(
                "{} kbps < {} kbps",
                quality.throughput_kbps, policy.min_throughput_kbps
            ),
        ));
    }
    Ok(())
}

fn compare(a: &TransportCandidate, b: &TransportCandidate, strategy: SelectionStrategy) -> Ordering {
    let latency = a
        .quality
        .rtt_ms
        .total_cmp(&b.quality.rtt_ms)
        .then(a.quality.jitter_ms.total_cmp(&b.quality.jitter_ms));
    let tier = mode_rank(a.descriptor.mode).cmp(&mode_rank(b.descriptor.mode));
    let primary = match strategy {
        SelectionStrategy::Tiered => tier.then(latency),
        SelectionStrategy::LowestLatency => latency.then(tier),
    };
    primary
        .then(a.quality.loss_percent.total_cmp(&b.quality.loss_percent))
        .then(b.quality.throughput_kbps.cmp(&a.quality.throughput_kbps))
        .then_with(|| a.descriptor.endpoint.cmp(&b.descriptor.endpoint))
}

fn mode_label(mode: TransportMode) -> &'static str {
    match mode {
        TransportMode::Quic => "quic",
        TransportMode::Udp => "udp",
        TransportMode::TcpInterleaved => "tcp_interleaved",
        TransportMode::UnixSocket => "unix_socket",
        TransportMode::Ssh => "ssh",
        TransportMode::Hls => "hls",
        TransportMode::Relay => "relay",
        TransportMode::Loopback => "loopback",
    }
}

fn preference_label(preference: TransportPreference) -> &'static str {
    match preference {
        TransportPreference::Auto => "auto",
        TransportPreference::Quic => "quic",
        TransportPreference::Udp => "udp",
        TransportPreference::TcpInterleaved => "tcp_interleaved",
        TransportPreference::Relay => "relay",
        TransportPreference::Loopback => "loopback",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::offer::TransportDescriptorBuilder;
    use ozma_protocol::codes;

    fn candidate(
        descriptor: TransportDescriptor,
        rtt_ms: f32,
        jitter_ms: f32,
        loss_percent: f32,
        throughput_kbps: u32,
    ) -> TransportCandidate {
        TransportCandidate::new(
            descriptor,
            LinkQuality::new(rtt_ms, jitter_ms, loss_percent, throughput_kbps),
        )
    }

    fn quic(endpoint: &str) -> TransportDescriptor {
        TransportDescriptorBuilder::quic(endpoint).build()
    }

    fn udp(endpoint: &str) -> TransportDescriptor {
        TransportDescriptorBuilder::udp(endpoint).build()
    }

    fn tcp(endpoint: &str) -> TransportDescriptor {
        TransportDescriptorBuilder::tcp_interleaved(endpoint).build()
    }

    #[test]
    fn tiered_policy_prefers_quic_even_when_udp_is_faster() {
        let candidates = vec![
            candidate(quic("10.0.0.1:4433"), 40.0, 2.0, 0.0, 50_000),
            candidate(udp("10.0.0.1:5004"), 8.0, 1.0, 0.0, 80_000),
        ];
        let selection = TransportDirector::select_from(&candidates, &TransportPolicy::default())
            .expect("a viable path exists");
        assert_eq!(selection.descriptor.mode, TransportMode::Quic);
        assert!(selection.reason.contains("highest-priority"));
    }

    #[test]
    fn lowest_latency_strategy_picks_the_fastest_viable_path() {
        let candidates = vec![
            candidate(quic("10.0.0.1:4433"), 40.0, 2.0, 0.0, 50_000),
            candidate(udp("10.0.0.1:5004"), 8.0, 1.0, 0.0, 80_000),
        ];
        let policy = TransportPolicy::default().with_strategy(SelectionStrategy::LowestLatency);
        let selection =
            TransportDirector::select_from(&candidates, &policy).expect("a viable path exists");
        assert_eq!(selection.descriptor.mode, TransportMode::Udp);
        assert!(selection.reason.contains("lowest measured rtt"));
    }

    #[test]
    fn lowest_latency_viable_path_wins_within_a_tier() {
        let candidates = vec![
            candidate(quic("10.0.0.1:4433"), 31.0, 3.0, 0.1, 30_000),
            candidate(quic("10.0.0.2:4433"), 12.0, 1.0, 0.0, 40_000),
        ];
        let selection = TransportDirector::select_from(&candidates, &TransportPolicy::default())
            .expect("a viable path exists");
        assert_eq!(
            selection.descriptor.endpoint.as_deref(),
            Some("10.0.0.2:4433")
        );
        assert_eq!(selection.quality.rtt_ms, 12.0);
    }

    #[test]
    fn a_lossy_path_falls_back_to_the_next_tier() {
        let candidates = vec![
            candidate(quic("10.0.0.1:4433"), 5.0, 1.0, 12.5, 90_000),
            candidate(udp("10.0.0.1:5004"), 9.0, 2.0, 0.5, 70_000),
        ];
        let selection = TransportDirector::select_from(&candidates, &TransportPolicy::default())
            .expect("udp remains viable");
        assert_eq!(selection.descriptor.mode, TransportMode::Udp);
        assert_eq!(selection.rejected.len(), 1);
        assert_eq!(selection.rejected[0].mode, TransportMode::Quic);
        assert_eq!(
            selection.rejected[0].reason,
            RejectionReason::ExcessiveLoss
        );
        assert!(selection.reason.contains("rejected: quic"));
    }

    #[test]
    fn a_low_latency_but_unusable_path_never_wins() {
        let candidates = vec![
            candidate(quic("10.0.0.1:4433"), 1.0, 0.0, 55.0, 90_000),
            candidate(tcp("10.0.0.1:8554"), 30.0, 4.0, 0.0, 20_000),
        ];
        let selection = TransportDirector::select_from(&candidates, &TransportPolicy::strict())
            .expect("tcp remains viable");
        assert_eq!(selection.descriptor.mode, TransportMode::TcpInterleaved);
    }

    #[test]
    fn no_viable_path_returns_transport_unavailable() {
        let candidates = vec![
            candidate(quic("10.0.0.1:4433"), 300.0, 80.0, 30.0, 100),
            candidate(tcp("10.0.0.1:8554"), 200.0, 60.0, 20.0, 200),
        ];
        let error = TransportDirector::select_from(&candidates, &TransportPolicy::default())
            .expect_err("nothing is viable");
        assert_eq!(error.code, codes::TRANSPORT_UNAVAILABLE);
        assert!(error.retryable);
        assert!(error.remediation.is_some());
    }

    #[test]
    fn empty_candidate_set_is_transport_unavailable() {
        let error = TransportDirector::select_from(&[], &TransportPolicy::default())
            .expect_err("no candidates");
        assert_eq!(error.code, codes::TRANSPORT_UNAVAILABLE);
    }

    #[test]
    fn a_fixed_preference_restricts_selection() {
        let mut director = TransportDirector::default();
        director
            .add_candidate(candidate(
                quic("10.0.0.1:4433"),
                10.0,
                1.0,
                0.0,
                50_000,
            ))
            .add_candidate(candidate(
                udp("10.0.0.1:5004"),
                20.0,
                2.0,
                0.0,
                40_000,
            ));
        let selection = director
            .select_for(TransportPreference::Udp)
            .expect("udp was measured");
        assert_eq!(selection.descriptor.mode, TransportMode::Udp);
        assert!(selection.reason.contains("session requires udp"));

        let error = director
            .select_for(TransportPreference::Relay)
            .expect_err("no relay candidate");
        assert_eq!(error.code, codes::TRANSPORT_UNAVAILABLE);
    }

    #[test]
    fn adding_the_same_endpoint_replaces_its_measurement() {
        let mut director = TransportDirector::default();
        director.add_candidate(candidate(
            quic("10.0.0.1:4433"),
            100.0,
            5.0,
            1.0,
            10_000,
        ));
        director.add_candidate(candidate(
            quic("10.0.0.1:4433"),
            8.0,
            1.0,
            0.0,
            60_000,
        ));
        assert_eq!(director.candidates().len(), 1);
        assert_eq!(director.candidates()[0].quality.rtt_ms, 8.0);
        assert_eq!(
            director.select().unwrap().quality.rtt_ms,
            8.0
        );
    }

    #[test]
    fn invalid_measurements_are_rejected_as_a_unit() {
        let candidates = vec![candidate(
            quic("10.0.0.1:4433"),
            f32::NAN,
            1.0,
            0.0,
            50_000,
        )];
        let error = TransportDirector::select_from(&candidates, &TransportPolicy::default())
            .expect_err("nan is not viable");
        assert_eq!(error.code, codes::TRANSPORT_UNAVAILABLE);
    }
}
