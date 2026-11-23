// SPDX-License-Identifier: AGPL-3.0-only
//! Select a representation for a scene subtree using estimated presentation cost.

use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Representation {
    Video,
    Image,
    Drawing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub representation: Representation,
    /// Whether both ends can use this representation for the whole subtree.
    pub supported: bool,
    /// Bytes that still need to arrive, including any missing resources.
    pub transfer_bytes: u64,
    /// Remaining encode, decode, upload and rendering time.
    pub processing: Duration,
}

impl Candidate {
    fn cost(self, bytes_per_second: u64) -> Duration {
        if self.transfer_bytes == 0 {
            return self.processing;
        }
        if bytes_per_second == 0 {
            return Duration::MAX;
        }
        let transfer =
            Duration::try_from_secs_f64(self.transfer_bytes as f64 / bytes_per_second as f64)
                .unwrap_or(Duration::MAX);
        transfer.saturating_add(self.processing)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ExecutionBudget {
    pub bytes_per_second: u64,
    pub presentation_deadline: Duration,
    pub minimum_switch_saving: Duration,
}

/// Return the lowest cost usable representation, retaining the current one
/// through small fluctuations as long as it can meet the deadline. If every
/// usable representation misses the deadline, select the earliest estimate.
pub fn select_representation(
    candidates: &[Candidate],
    current: Option<Representation>,
    budget: ExecutionBudget,
) -> Option<Representation> {
    let mut ranked = candidates
        .iter()
        .filter(|candidate| candidate.supported)
        .map(|candidate| {
            (
                candidate.representation,
                candidate.cost(budget.bytes_per_second),
            )
        });
    let best = ranked.clone().min_by_key(|(_, cost)| *cost)?;
    let retained = ranked.find(|(representation, _)| Some(*representation) == current);
    match retained {
        Some((representation, cost))
            if cost <= budget.presentation_deadline
                && cost.saturating_sub(best.1) <= budget.minimum_switch_saving =>
        {
            Some(representation)
        }
        _ => Some(best.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compare(image_bytes: u64, image_ms: u64, video_ms: u64) -> Representation {
        select_representation(
            &[
                Candidate {
                    representation: Representation::Image,
                    supported: true,
                    transfer_bytes: image_bytes,
                    processing: Duration::from_millis(image_ms),
                },
                Candidate {
                    representation: Representation::Video,
                    supported: true,
                    transfer_bytes: 0,
                    processing: Duration::from_millis(video_ms),
                },
            ],
            Some(Representation::Video),
            ExecutionBudget {
                bytes_per_second: 1_000_000,
                presentation_deadline: Duration::from_millis(16),
                minimum_switch_saving: Duration::from_millis(3),
            },
        )
        .unwrap()
    }

    #[test]
    fn uses_video_during_upload_and_switches_to_cached_image() {
        assert_eq!(compare(80_000, 2, 12), Representation::Video);
        assert_eq!(compare(0, 2, 12), Representation::Image);
    }

    #[test]
    fn retains_current_on_small_saving_unless_current_misses_deadline() {
        assert_eq!(compare(0, 10, 12), Representation::Video);
        assert_eq!(compare(0, 15, 17), Representation::Image);
    }
}
