// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Live browser scene ingest.
//!
//! [`BrowserSceneHub`] is the thread-safe hand-off between the browser WebSocket
//! endpoint and the native QUIC scene stream. It accepts the same resource,
//! commit, and video-metadata records as the JSONL replay fixture, but it keeps
//! a bounded live history instead of loading one immutable file. The hub is both
//! a [`SceneProducer`] for session transport and a [`BrowserSceneSink`] for the
//! API router.
//!
//! Video records are metadata-only in this adapter. The node's normal capture
//! pipeline remains the video source; a browser video passthrough is not wired
//! here.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use base64::Engine;
use ozma_api::state::{BrowserSceneSink, SceneSinkError};
use ozma_protocol::scene::{ImageResource, SceneCommit};
use ozma_protocol::{Session, SessionId};
use serde::{Deserialize, Serialize};

use crate::backend::{SceneProducer, SceneUpdate};

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Clone, Copy, Debug)]
pub struct BrowserSceneLimits {
    /// Total raw PNG bytes retained by the live resource cache.
    pub cache_budget_bytes: usize,
    /// Maximum framed resource payload, including the eight-byte id prefix.
    /// The native bulk channel uses a `u16` payload length.
    pub max_resource_bytes: usize,
    /// Number of most recent commits retained for new or reconnecting clients.
    pub max_commits: usize,
    /// Number of most recent video metadata records retained.
    pub max_video_frames: usize,
    /// Number of connected session delivery states tracked by revision.
    pub max_tracked_sessions: usize,
}

impl Default for BrowserSceneLimits {
    fn default() -> Self {
        Self {
            cache_budget_bytes: 4 * 1024 * 1024,
            max_resource_bytes: u16::MAX as usize,
            max_commits: 128,
            max_video_frames: 32,
            max_tracked_sessions: 256,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceReport {
    pub newly_cached: bool,
    pub bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitReport {
    pub revision: u64,
    pub images: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoFrameReport {
    pub retained: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoFrameMeta {
    pub surface_id: u32,
    pub frame: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SceneIngestError {
    #[error("invalid resource: {0}")]
    InvalidResource(String),
    #[error("resource {id} was already cached with different bytes")]
    ConflictingResource { id: u64 },
    #[error("resource exceeds the {limit} byte transport limit")]
    ResourceTooLarge { limit: usize },
    #[error("scene resource cache is full ({used} of {budget} bytes)")]
    CacheFull { used: usize, budget: usize },
    #[error("scene revision {revision} is not newer than {latest}")]
    StaleRevision { revision: u64, latest: u64 },
    #[error("invalid placement: {0}")]
    InvalidPlacement(String),
}

impl SceneIngestError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidResource(_) => "invalid_resource",
            Self::ConflictingResource { .. } => "conflicting_resource",
            Self::ResourceTooLarge { .. } => "resource_too_large",
            Self::CacheFull { .. } => "cache_full",
            Self::StaleRevision { .. } => "stale_revision",
            Self::InvalidPlacement(_) => "invalid_placement",
        }
    }
}

impl From<SceneIngestError> for SceneSinkError {
    fn from(error: SceneIngestError) -> Self {
        SceneSinkError::new(error.code(), error.to_string())
    }
}

struct HubState {
    limits: BrowserSceneLimits,
    used_bytes: usize,
    resources: HashMap<u64, Vec<u8>>,
    commits: VecDeque<SceneCommit>,
    video_frames: VecDeque<VideoFrameMeta>,
    last_revision: u64,
    delivered: HashMap<SessionId, HashSet<u64>>,
    session_order: VecDeque<SessionId>,
}

impl HubState {
    fn ensure_session(&mut self, id: &SessionId, reset: bool) {
        if reset {
            if !self.delivered.contains_key(id) {
                self.session_order.push_back(id.clone());
            }
            self.delivered.insert(id.clone(), HashSet::new());
        } else if !self.delivered.contains_key(id) {
            self.session_order.push_back(id.clone());
            self.delivered.insert(id.clone(), HashSet::new());
        }

        while self.delivered.len() > self.limits.max_tracked_sessions {
            let Some(oldest) = self.session_order.pop_front() else {
                break;
            };
            self.delivered.remove(&oldest);
        }
    }
}

/// Bounded, thread-safe live scene producer and browser ingest sink.
#[derive(Clone)]
pub struct BrowserSceneHub {
    state: Arc<Mutex<HubState>>,
}

impl Default for BrowserSceneHub {
    fn default() -> Self {
        Self::with_limits(BrowserSceneLimits::default())
    }
}

impl BrowserSceneHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cache_budget(cache_budget_bytes: usize) -> Self {
        Self::with_limits(BrowserSceneLimits {
            cache_budget_bytes,
            ..BrowserSceneLimits::default()
        })
    }

    pub fn with_limits(limits: BrowserSceneLimits) -> Self {
        let limits = BrowserSceneLimits {
            max_commits: limits.max_commits.max(1),
            max_video_frames: limits.max_video_frames.max(1),
            max_tracked_sessions: limits.max_tracked_sessions.max(1),
            ..limits
        };
        Self {
            state: Arc::new(Mutex::new(HubState {
                limits,
                used_bytes: 0,
                resources: HashMap::new(),
                commits: VecDeque::new(),
                video_frames: VecDeque::new(),
                last_revision: 0,
                delivered: HashMap::new(),
                session_order: VecDeque::new(),
            })),
        }
    }

    pub fn limits(&self) -> BrowserSceneLimits {
        self.lock().limits
    }

    pub fn last_revision(&self) -> u64 {
        self.lock().last_revision
    }

    pub fn cache_bytes(&self) -> usize {
        self.lock().used_bytes
    }

    pub fn resource_count(&self) -> usize {
        self.lock().resources.len()
    }

    pub fn commit_count(&self) -> usize {
        self.lock().commits.len()
    }

    pub fn video_frames(&self) -> Vec<VideoFrameMeta> {
        self.lock().video_frames.iter().cloned().collect()
    }

    pub fn latest_video_frame(&self) -> Option<VideoFrameMeta> {
        self.lock().video_frames.back().cloned()
    }

    /// Cache a PNG by resource id. The raw PNG bytes are stored; the framed
    /// `id || png` payload is reconstructed when the producer sends an update.
    pub fn ingest_resource(&self, id: u64, png: &[u8]) -> Result<ResourceReport, SceneIngestError> {
        let mut state = self.lock();
        if png.len().saturating_add(8) > state.limits.max_resource_bytes {
            return Err(SceneIngestError::ResourceTooLarge {
                limit: state.limits.max_resource_bytes,
            });
        }
        if !png.starts_with(PNG_SIGNATURE) {
            return Err(SceneIngestError::InvalidResource(
                "resource is not a PNG".to_owned(),
            ));
        }

        if let Some(existing) = state.resources.get(&id) {
            return if existing.as_slice() == png {
                Ok(ResourceReport {
                    newly_cached: false,
                    bytes: existing.len(),
                })
            } else {
                Err(SceneIngestError::ConflictingResource { id })
            };
        }

        if state.used_bytes.saturating_add(png.len()) > state.limits.cache_budget_bytes {
            return Err(SceneIngestError::CacheFull {
                used: state.used_bytes,
                budget: state.limits.cache_budget_bytes,
            });
        }

        state.used_bytes += png.len();
        state.resources.insert(id, png.to_vec());
        Ok(ResourceReport {
            newly_cached: true,
            bytes: png.len(),
        })
    }

    /// Decode one framed `ImageResource` payload and cache its PNG.
    pub fn ingest_resource_payload(
        &self,
        payload: &[u8],
    ) -> Result<ResourceReport, SceneIngestError> {
        let image = self.decode_payload(payload)?;
        self.ingest_resource(image.id, image.png)
    }

    /// Decode a framed payload and require its embedded id to match the id in
    /// the browser message.
    pub fn ingest_resource_message(
        &self,
        id: u64,
        payload: &[u8],
    ) -> Result<ResourceReport, SceneIngestError> {
        let image = self.decode_payload(payload)?;
        if image.id != id {
            return Err(SceneIngestError::InvalidResource(format!(
                "resource id {id} does not match its frame {}",
                image.id
            )));
        }
        self.ingest_resource(image.id, image.png)
    }

    fn decode_payload<'a>(&self, payload: &'a [u8]) -> Result<ImageResource<'a>, SceneIngestError> {
        let limit = self.lock().limits.max_resource_bytes;
        if payload.len() > limit {
            return Err(SceneIngestError::ResourceTooLarge { limit });
        }
        ImageResource::decode(payload).ok_or_else(|| {
            SceneIngestError::InvalidResource("resource is not a framed PNG".to_owned())
        })
    }

    /// Publish a complete overlay revision. Revisions must increase. A commit
    /// may reference resources that have not arrived yet; they are delivered
    /// before the waiting revision when they show up.
    pub fn ingest_commit(&self, commit: SceneCommit) -> Result<CommitReport, SceneIngestError> {
        let mut state = self.lock();
        if commit.revision <= state.last_revision {
            return Err(SceneIngestError::StaleRevision {
                revision: commit.revision,
                latest: state.last_revision,
            });
        }
        validate_placements(&commit)?;

        state.last_revision = commit.revision;
        let images = commit.images.len();
        state.commits.push_back(commit);
        while state.commits.len() > state.limits.max_commits {
            state.commits.pop_front();
        }
        Ok(CommitReport {
            revision: state.last_revision,
            images,
        })
    }

    /// Accept browser video metadata as a passthrough event. No frame bytes are
    /// decoded or forwarded through this adapter.
    pub fn ingest_video_frame(
        &self,
        surface_id: u32,
        frame: serde_json::Value,
    ) -> VideoFrameReport {
        let mut state = self.lock();
        while state.video_frames.len() >= state.limits.max_video_frames {
            state.video_frames.pop_front();
        }
        state
            .video_frames
            .push_back(VideoFrameMeta { surface_id, frame });
        VideoFrameReport {
            retained: state.video_frames.len(),
        }
    }

    fn collect(&self, session: &Session, start_revision: u64, reset: bool) -> Vec<SceneUpdate> {
        let mut state = self.lock();
        state.ensure_session(&session.id, reset);

        let HubState {
            commits,
            resources,
            delivered,
            ..
        } = &mut *state;
        let delivered = delivered
            .get_mut(&session.id)
            .expect("session delivery state was just inserted");

        let mut updates = Vec::new();
        for commit in commits.iter() {
            if commit.revision < start_revision {
                continue;
            }

            let mut needed: Vec<u64> = commit
                .images
                .iter()
                .map(|image| image.resource_id)
                .filter(|id| !delivered.contains(id))
                .collect();
            needed.sort_unstable();
            needed.dedup();
            let available: Vec<u64> = needed
                .into_iter()
                .filter(|id| resources.contains_key(id))
                .collect();

            // The equal revision was already sent. Only revisit it to attach
            // resources that arrived after the waiting commit.
            if commit.revision == start_revision && available.is_empty() {
                continue;
            }

            if available.is_empty() {
                // Send the commit even if its resources are still in flight so
                // the receiver can wait for them.
                updates.push(SceneUpdate {
                    resource: None,
                    commit: commit.clone(),
                });
            } else {
                // `SceneUpdate` carries one resource. Multiple missing
                // resources share the same commit; the receiver caches each
                // resource and presents once the last one arrives.
                for id in available {
                    delivered.insert(id);
                    let png = resources.get(&id).expect("available resource was checked");
                    updates.push(SceneUpdate {
                        resource: Some(ImageResource { id, png }.encode()),
                        commit: commit.clone(),
                    });
                }
            }
        }
        updates
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state.lock().expect("browser scene hub lock")
    }
}

fn validate_placements(commit: &SceneCommit) -> Result<(), SceneIngestError> {
    let mut nodes = HashSet::new();
    for image in &commit.images {
        if image.width == 0 || image.height == 0 {
            return Err(SceneIngestError::InvalidPlacement(format!(
                "node {} has zero width or height",
                image.node_id
            )));
        }
        if !nodes.insert(image.node_id) {
            return Err(SceneIngestError::InvalidPlacement(format!(
                "node {} appears more than once",
                image.node_id
            )));
        }
    }
    Ok(())
}

impl SceneProducer for BrowserSceneHub {
    fn updates(&self, session: &Session) -> Vec<SceneUpdate> {
        // A new transport connection has its own receiver and cache. Reset the
        // per-session delivery set so a reconnect receives the retained
        // resources again.
        self.collect(session, 0, true)
    }

    fn updates_since(&self, session: &Session, revision: u64) -> Vec<SceneUpdate> {
        self.collect(session, revision, false)
    }
}

impl BrowserSceneSink for BrowserSceneHub {
    fn ingest_resource(&self, id: u64, payload_base64: &str) -> Result<(), SceneSinkError> {
        let payload = base64::engine::general_purpose::STANDARD
            .decode(payload_base64)
            .map_err(|error| {
                SceneSinkError::new("invalid_resource", format!("invalid base64: {error}"))
            })?;
        BrowserSceneHub::ingest_resource_message(self, id, &payload)
            .map(|_| ())
            .map_err(SceneSinkError::from)
    }

    fn ingest_commit(&self, commit: SceneCommit) -> Result<(), SceneSinkError> {
        BrowserSceneHub::ingest_commit(self, commit)
            .map(|_| ())
            .map_err(SceneSinkError::from)
    }

    fn ingest_video_frame(
        &self,
        surface_id: u32,
        frame: serde_json::Value,
    ) -> Result<(), SceneSinkError> {
        BrowserSceneHub::ingest_video_frame(self, surface_id, frame);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_protocol::scene::ImagePlacement;
    use ozma_protocol::{ClientId, PermissionSet, SessionSpec};

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfixture";
    const OTHER_PNG: &[u8] = b"\x89PNG\r\n\x1a\nother";

    fn session(id: &str) -> Session {
        Session::new(
            SessionId::new(id),
            SessionSpec {
                client_id: ClientId::new("browser"),
                principal: "browser".into(),
                tracks: Vec::new(),
                audio: Default::default(),
                input: Default::default(),
                privacy: Default::default(),
                transport: Default::default(),
                quality: None,
                ttl_seconds: 60,
            },
            PermissionSet::all(),
            0,
        )
    }

    fn placement(node_id: u32, resource_id: u64) -> ImagePlacement {
        ImagePlacement {
            node_id,
            resource_id,
            x: 10,
            y: 20,
            width: 320,
            height: 180,
        }
    }

    fn commit(revision: u64, images: Vec<ImagePlacement>) -> SceneCommit {
        SceneCommit { revision, images }
    }

    #[test]
    fn commit_before_resource_delivers_resource_with_the_waiting_revision() {
        let hub = BrowserSceneHub::new();
        let session = session("s1");
        hub.ingest_commit(commit(1, vec![placement(1, 7)])).unwrap();

        let initial = hub.updates(&session);
        assert_eq!(initial.len(), 1);
        assert!(initial[0].resource.is_none());
        assert_eq!(initial[0].commit.revision, 1);

        // The commit was already sent at revision 1. The late resource is
        // re-delivered with the same revision so the receiver can present it.
        assert!(hub.updates_since(&session, 1).is_empty());
        hub.ingest_resource(7, PNG).unwrap();
        let late = hub.updates_since(&session, 1);
        assert_eq!(late.len(), 1);
        let image = ImageResource::decode(late[0].resource.as_deref().unwrap()).unwrap();
        assert_eq!(image.id, 7);
        assert_eq!(image.png, PNG);
        assert_eq!(late[0].commit.revision, 1);

        // Once delivered to this session it is not repeated.
        assert!(hub.updates_since(&session, 1).is_empty());
    }

    #[test]
    fn resource_before_commit_is_sent_first() {
        let hub = BrowserSceneHub::new();
        let session = session("s1");
        hub.ingest_resource(7, PNG).unwrap();
        hub.ingest_commit(commit(2, vec![placement(1, 7)])).unwrap();

        let updates = hub.updates(&session);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].commit.revision, 2);
        let image = ImageResource::decode(updates[0].resource.as_deref().unwrap()).unwrap();
        assert_eq!(image.id, 7);
        assert!(hub.updates_since(&session, 2).is_empty());
    }

    #[test]
    fn multiple_resources_for_one_commit_are_all_delivered() {
        let hub = BrowserSceneHub::new();
        let session = session("s1");
        hub.ingest_resource(7, PNG).unwrap();
        hub.ingest_resource(8, OTHER_PNG).unwrap();
        hub.ingest_commit(commit(1, vec![placement(1, 7), placement(2, 8)]))
            .unwrap();

        let updates = hub.updates(&session);
        assert_eq!(updates.len(), 2);
        assert!(updates.iter().all(|update| update.commit.revision == 1));
        let ids: HashSet<u64> = updates
            .iter()
            .filter_map(|update| update.resource.as_deref())
            .filter_map(ImageResource::decode)
            .map(|image| image.id)
            .collect();
        assert_eq!(ids, HashSet::from([7, 8]));
    }

    #[test]
    fn cache_budget_and_conflicting_ids_are_enforced() {
        let hub = BrowserSceneHub::with_limits(BrowserSceneLimits {
            cache_budget_bytes: PNG.len(),
            ..BrowserSceneLimits::default()
        });
        assert_eq!(
            hub.ingest_resource(1, PNG).unwrap(),
            ResourceReport {
                newly_cached: true,
                bytes: PNG.len()
            }
        );
        assert_eq!(
            hub.ingest_resource(1, PNG).unwrap(),
            ResourceReport {
                newly_cached: false,
                bytes: PNG.len()
            }
        );
        assert_eq!(
            hub.ingest_resource(1, OTHER_PNG),
            Err(SceneIngestError::ConflictingResource { id: 1 })
        );
        assert_eq!(
            hub.ingest_resource(2, PNG),
            Err(SceneIngestError::CacheFull {
                used: PNG.len(),
                budget: PNG.len()
            })
        );
        assert_eq!(hub.cache_bytes(), PNG.len());
        assert_eq!(hub.resource_count(), 1);
    }

    #[test]
    fn stale_revisions_and_invalid_placements_are_rejected() {
        let hub = BrowserSceneHub::new();
        hub.ingest_commit(commit(2, Vec::new())).unwrap();
        assert_eq!(
            hub.ingest_commit(commit(2, Vec::new())),
            Err(SceneIngestError::StaleRevision {
                revision: 2,
                latest: 2
            })
        );
        assert!(matches!(
            hub.ingest_commit(commit(1, Vec::new())),
            Err(SceneIngestError::StaleRevision { .. })
        ));

        let mut zero = placement(1, 7);
        zero.width = 0;
        assert!(matches!(
            hub.ingest_commit(commit(3, vec![zero])),
            Err(SceneIngestError::InvalidPlacement(_))
        ));
        assert!(matches!(
            hub.ingest_commit(commit(3, vec![placement(1, 7), placement(1, 8)])),
            Err(SceneIngestError::InvalidPlacement(_))
        ));
        assert_eq!(hub.last_revision(), 2);
    }

    #[test]
    fn reconnect_resets_delivery_and_retains_a_bounded_history() {
        let hub = BrowserSceneHub::with_limits(BrowserSceneLimits {
            max_commits: 2,
            ..BrowserSceneLimits::default()
        });
        let session = session("s1");
        hub.ingest_resource(7, PNG).unwrap();
        for revision in 1..=4 {
            hub.ingest_commit(commit(revision, vec![placement(1, 7)]))
                .unwrap();
        }
        assert_eq!(hub.commit_count(), 2);

        let first = hub.updates(&session);
        assert!(first.iter().any(|update| update.resource.is_some()));
        assert_eq!(first.last().unwrap().commit.revision, 4);
        assert!(hub.updates_since(&session, 4).is_empty());

        // A new transport connection for the same session is a fresh receiver.
        let reconnect = hub.updates(&session);
        assert!(reconnect.iter().any(|update| update.resource.is_some()));
        assert_eq!(reconnect.last().unwrap().commit.revision, 4);
    }

    #[test]
    fn video_metadata_is_bounded_and_keeps_the_latest() {
        let hub = BrowserSceneHub::with_limits(BrowserSceneLimits {
            max_video_frames: 2,
            ..BrowserSceneLimits::default()
        });
        for frame in 1..=3 {
            hub.ingest_video_frame(1, serde_json::json!(frame));
        }
        let frames = hub.video_frames();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].frame, serde_json::json!(3));
        assert_eq!(hub.latest_video_frame().unwrap().surface_id, 1);
    }
}
