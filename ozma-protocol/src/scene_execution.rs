// SPDX-License-Identifier: AGPL-3.0-only
//! Select a representation for a scene subtree using estimated presentation cost.

use std::collections::HashSet;
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

/// A representation request for one complete scene revision. Resource IDs are
/// checked before a switch is made, so a client never exposes a half-ready
/// revision.
#[derive(Clone, Debug)]
pub struct RepresentationRequest {
    pub revision: u64,
    pub candidates: Vec<Candidate>,
    pub required_resources: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentationStatus {
    Presented {
        revision: u64,
        representation: Representation,
    },
    WaitingForResources {
        revision: u64,
    },
    IgnoredStale,
}

/// Chooses representations only at revision boundaries. While a richer
/// representation is waiting for resources, the last presented representation
/// remains active. Video is the universal fallback for a new revision.
pub struct AdaptivePresenter {
    current: Representation,
    revision: u64,
    pending: Option<RepresentationRequest>,
}

impl AdaptivePresenter {
    pub fn new(fallback: Representation) -> Self {
        Self {
            current: fallback,
            revision: 0,
            pending: None,
        }
    }

    pub fn current(&self) -> Representation {
        self.current
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn submit(
        &mut self,
        request: RepresentationRequest,
        resources: &HashSet<u64>,
        budget: ExecutionBudget,
    ) -> PresentationStatus {
        if request.revision <= self.revision
            || self
                .pending
                .as_ref()
                .is_some_and(|pending| request.revision <= pending.revision)
        {
            return PresentationStatus::IgnoredStale;
        }
        self.pending = Some(request);
        self.try_present(resources, budget)
    }

    pub fn resources_arrived(
        &mut self,
        resources: &HashSet<u64>,
        budget: ExecutionBudget,
    ) -> Option<PresentationStatus> {
        self.pending.as_ref()?;
        Some(self.try_present(resources, budget))
    }

    fn try_present(
        &mut self,
        resources: &HashSet<u64>,
        budget: ExecutionBudget,
    ) -> PresentationStatus {
        let request = self.pending.as_ref().expect("pending request");
        let ready = request
            .required_resources
            .iter()
            .all(|id| resources.contains(id));
        let mut candidates = request.candidates.clone();
        if !ready {
            candidates.retain(|candidate| candidate.representation == Representation::Video);
        }
        let Some(representation) = select_representation(&candidates, Some(self.current), budget)
        else {
            return PresentationStatus::WaitingForResources {
                revision: request.revision,
            };
        };
        let revision = request.revision;
        // Video may present immediately while the richer representation is
        // still loading. Keep the request so the same revision can switch at
        // a later presentation boundary when its resources arrive.
        if !ready && representation == Representation::Video {
            if self.revision == revision && self.current == representation {
                return PresentationStatus::WaitingForResources { revision };
            }
            self.revision = revision;
            self.current = representation;
            return PresentationStatus::Presented {
                revision,
                representation,
            };
        }
        self.pending = None;
        self.revision = revision;
        self.current = representation;
        PresentationStatus::Presented {
            revision,
            representation,
        }
    }
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

    fn budget() -> ExecutionBudget {
        ExecutionBudget {
            bytes_per_second: 1_000_000,
            presentation_deadline: Duration::from_millis(16),
            minimum_switch_saving: Duration::from_millis(3),
        }
    }

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

    #[test]
    fn keeps_video_until_image_is_cached_then_switches_at_revision_boundary() {
        let mut presenter = AdaptivePresenter::new(Representation::Video);
        let request = RepresentationRequest {
            revision: 1,
            candidates: vec![
                Candidate {
                    representation: Representation::Image,
                    supported: true,
                    transfer_bytes: 0,
                    processing: Duration::from_millis(1),
                },
                Candidate {
                    representation: Representation::Video,
                    supported: true,
                    transfer_bytes: 0,
                    processing: Duration::from_millis(12),
                },
            ],
            required_resources: vec![7],
        };
        let empty = HashSet::new();
        assert_eq!(
            presenter.submit(request, &empty, budget()),
            PresentationStatus::Presented {
                revision: 1,
                representation: Representation::Video
            }
        );
        assert_eq!(presenter.current(), Representation::Video);
        let mut cached = HashSet::new();
        cached.insert(7);
        assert_eq!(
            presenter.resources_arrived(&cached, budget()),
            Some(PresentationStatus::Presented {
                revision: 1,
                representation: Representation::Image
            })
        );
        assert_eq!(presenter.current(), Representation::Image);
    }

    #[test]
    fn unready_drawing_keeps_video_and_does_not_reannounce_it() {
        let mut presenter = AdaptivePresenter::new(Representation::Video);
        let request = RepresentationRequest {
            revision: 1,
            candidates: vec![
                Candidate {
                    representation: Representation::Drawing,
                    supported: true,
                    transfer_bytes: 0,
                    processing: Duration::from_millis(1),
                },
                Candidate {
                    representation: Representation::Video,
                    supported: true,
                    transfer_bytes: 0,
                    processing: Duration::from_millis(12),
                },
            ],
            required_resources: vec![8],
        };
        assert_eq!(
            presenter.submit(request, &HashSet::new(), budget()),
            PresentationStatus::Presented {
                revision: 1,
                representation: Representation::Video
            },
        );
        assert_eq!(
            presenter.resources_arrived(&HashSet::from([7]), budget()),
            Some(PresentationStatus::WaitingForResources { revision: 1 }),
        );
        assert_eq!(
            presenter.resources_arrived(&HashSet::from([8]), budget()),
            Some(PresentationStatus::Presented {
                revision: 1,
                representation: Representation::Drawing
            }),
        );
    }

    #[test]
    fn newest_revision_supersedes_an_unready_revision() {
        let mut presenter = AdaptivePresenter::new(Representation::Video);
        let request = |revision, id| RepresentationRequest {
            revision,
            candidates: vec![Candidate {
                representation: Representation::Image,
                supported: true,
                transfer_bytes: 0,
                processing: Duration::from_millis(1),
            }],
            required_resources: vec![id],
        };
        let empty = HashSet::new();
        assert!(matches!(
            presenter.submit(request(1, 7), &empty, budget()),
            PresentationStatus::WaitingForResources { .. }
        ));
        assert!(matches!(
            presenter.submit(request(2, 8), &empty, budget()),
            PresentationStatus::WaitingForResources { revision: 2 }
        ));
        let mut cached = HashSet::new();
        cached.insert(7);
        assert_eq!(
            presenter.resources_arrived(&cached, budget()),
            Some(PresentationStatus::WaitingForResources { revision: 2 })
        );
        cached.insert(8);
        assert_eq!(
            presenter.resources_arrived(&cached, budget()),
            Some(PresentationStatus::Presented {
                revision: 2,
                representation: Representation::Image
            })
        );
    }
}
