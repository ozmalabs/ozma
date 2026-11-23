// SPDX-License-Identifier: AGPL-3.0-only
//! Native display negotiation and the baseline scene snapshot.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

pub const SCENE_VERSION: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayCapabilities {
    pub scene_versions: Vec<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "profile", rename_all = "snake_case")]
pub enum DisplayProfile {
    Video,
    Scene {
        version: u16,
        initial_scene: SceneSnapshot,
    },
}

/// A self-contained descriptor of what to present. Revision zero is the
/// initial snapshot of each new connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneSnapshot {
    pub revision: u64,
    pub full_display: VideoSurface,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoSurface {
    pub id: u32,
    pub channel: u8,
}

impl DisplayCapabilities {
    pub fn select(&self, video_channel: u8) -> DisplayProfile {
        if self.scene_versions.contains(&SCENE_VERSION) {
            DisplayProfile::Scene {
                version: SCENE_VERSION,
                initial_scene: SceneSnapshot {
                    revision: 0,
                    full_display: VideoSurface {
                        id: 1,
                        channel: video_channel,
                    },
                },
            }
        } else {
            DisplayProfile::Video
        }
    }
}

/// An immutable, session-local resource identifier followed by an encoded PNG.
pub struct ImageResource<'a> {
    pub id: u64,
    pub png: &'a [u8],
}

impl<'a> ImageResource<'a> {
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = self.id.to_be_bytes().to_vec();
        payload.extend_from_slice(self.png);
        payload
    }

    pub fn decode(payload: &'a [u8]) -> Option<Self> {
        let id = u64::from_be_bytes(payload.get(..8)?.try_into().ok()?);
        let png = payload.get(8..)?;
        png.starts_with(b"\x89PNG\r\n\x1a\n")
            .then_some(Self { id, png })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePlacement {
    pub node_id: u32,
    pub resource_id: u64,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Complete ordered image overlay list for a revision. The full-display
/// video surface continues independently below these placements.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneCommit {
    pub revision: u64,
    pub images: Vec<ImagePlacement>,
}

impl SceneCommit {
    fn valid_placements(&self) -> bool {
        let mut nodes = HashSet::new();
        self.images
            .iter()
            .all(|image| image.width > 0 && image.height > 0 && nodes.insert(image.node_id))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitStatus {
    Presented,
    WaitingForResources,
    IgnoredStale,
    InvalidPlacements,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceError {
    InvalidPayload,
    ConflictingId,
    CacheFull,
}

/// Receiver state for one negotiated connection. Create a new receiver from
/// the next snapshot on reconnect. Resource identifiers belong to that
/// connection and are immutable for its lifetime.
pub struct SceneReceiver {
    surface: VideoSurface,
    presented: SceneCommit,
    pending: Option<SceneCommit>,
    resources: HashMap<u64, Vec<u8>>,
    remaining_bytes: usize,
}

impl SceneReceiver {
    pub fn new(snapshot: SceneSnapshot, cache_budget_bytes: usize) -> Self {
        Self {
            surface: snapshot.full_display,
            presented: SceneCommit {
                revision: snapshot.revision,
                images: Vec::new(),
            },
            pending: None,
            resources: HashMap::new(),
            remaining_bytes: cache_budget_bytes,
        }
    }

    pub fn video_surface(&self) -> &VideoSurface {
        &self.surface
    }

    pub fn presented(&self) -> &SceneCommit {
        &self.presented
    }

    pub fn image(&self, id: u64) -> Option<&[u8]> {
        self.resources.get(&id).map(Vec::as_slice)
    }

    pub fn receive_commit(&mut self, commit: SceneCommit) -> CommitStatus {
        let latest = self.pending.as_ref().unwrap_or(&self.presented).revision;
        if commit.revision <= latest {
            return CommitStatus::IgnoredStale;
        }
        if !commit.valid_placements() {
            return CommitStatus::InvalidPlacements;
        }

        // Commits contain the complete overlay list. The highest valid
        // revision supersedes any previous pending revision.
        self.pending = Some(commit);
        if self.present_if_ready() {
            CommitStatus::Presented
        } else {
            CommitStatus::WaitingForResources
        }
    }

    /// Returns true when the received resource made a pending commit visible.
    pub fn receive_resource(&mut self, payload: &[u8]) -> Result<bool, ResourceError> {
        let resource = ImageResource::decode(payload).ok_or(ResourceError::InvalidPayload)?;
        if let Some(existing) = self.image(resource.id) {
            return if existing == resource.png {
                Ok(false)
            } else {
                Err(ResourceError::ConflictingId)
            };
        }

        self.remaining_bytes = self
            .remaining_bytes
            .checked_sub(resource.png.len())
            .ok_or(ResourceError::CacheFull)?;
        self.resources.insert(resource.id, resource.png.to_vec());
        Ok(self.present_if_ready())
    }

    fn present_if_ready(&mut self) -> bool {
        if let Some(commit) = &self.pending {
            if commit
                .images
                .iter()
                .all(|image| self.resources.contains_key(&image.resource_id))
            {
                self.presented = self.pending.take().expect("pending revision was checked");
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfixture";

    fn snapshot() -> SceneSnapshot {
        SceneSnapshot {
            revision: 0,
            full_display: VideoSurface { id: 1, channel: 2 },
        }
    }

    fn commit(revision: u64, resource_id: u64) -> SceneCommit {
        SceneCommit {
            revision,
            images: vec![ImagePlacement {
                node_id: 1,
                resource_id,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
        }
    }

    #[test]
    fn newest_valid_commit_wins_when_resources_arrive_out_of_order() {
        let mut receiver = SceneReceiver::new(snapshot(), PNG.len() * 2);
        assert_eq!(
            receiver.receive_commit(commit(1, 10)),
            CommitStatus::WaitingForResources
        );
        assert_eq!(
            receiver.receive_commit(commit(2, 20)),
            CommitStatus::WaitingForResources
        );
        let mut invalid = commit(3, 30);
        invalid.images[0].width = 0;
        assert_eq!(
            receiver.receive_commit(invalid),
            CommitStatus::InvalidPlacements
        );
        assert_eq!(
            receiver.receive_resource(&ImageResource { id: 10, png: PNG }.encode()),
            Ok(false)
        );
        assert_eq!(receiver.presented().revision, 0);
        assert_eq!(
            receiver.receive_resource(&ImageResource { id: 20, png: PNG }.encode()),
            Ok(true)
        );
        assert_eq!(receiver.presented(), &commit(2, 20));
        assert_eq!(
            receiver.receive_commit(commit(1, 10)),
            CommitStatus::IgnoredStale
        );
    }

    #[test]
    fn cache_is_bounded_and_fresh_on_reconnect() {
        let mut receiver = SceneReceiver::new(snapshot(), PNG.len());
        let image = ImageResource { id: 1, png: PNG }.encode();
        assert_eq!(receiver.receive_resource(&image), Ok(false));
        assert_eq!(receiver.receive_resource(&image), Ok(false));
        let conflict = ImageResource {
            id: 1,
            png: b"\x89PNG\r\n\x1a\nother",
        }
        .encode();
        assert_eq!(
            receiver.receive_resource(&conflict),
            Err(ResourceError::ConflictingId)
        );
        assert_eq!(
            receiver.receive_resource(&ImageResource { id: 2, png: PNG }.encode()),
            Err(ResourceError::CacheFull)
        );
        assert_eq!(receiver.image(1), Some(PNG));
        assert_eq!(
            receiver.receive_commit(commit(1, 1)),
            CommitStatus::Presented
        );
        assert_eq!(receiver.video_surface(), &snapshot().full_display);

        let mut reconnected = SceneReceiver::new(snapshot(), PNG.len());
        assert_eq!(
            reconnected.receive_commit(commit(1, 1)),
            CommitStatus::WaitingForResources
        );
        assert_eq!(reconnected.presented().revision, 0);
        assert_eq!(reconnected.receive_resource(&image), Ok(true));
    }

    #[test]
    fn unknown_scene_version_uses_video_profile() {
        let capabilities = DisplayCapabilities {
            scene_versions: vec![SCENE_VERSION + 1],
        };
        assert_eq!(capabilities.select(2), DisplayProfile::Video);
    }
}
