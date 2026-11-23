// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded replay producer for browser scene JSONL events.
//!
//! The browser fixture emits `resource` and `scene_commit` records. This
//! adapter turns those records into the same `SceneProducer` updates used by a
//! live compositor integration. It deliberately loads a bounded snapshot so a
//! malformed or untrusted replay cannot grow a node's memory without limit.

use std::io::Read;
use std::path::Path;

use base64::Engine;
use ozma_protocol::scene::{ImageResource, SceneCommit};
use serde::Deserialize;

use crate::backend::{SceneProducer, SceneUpdate};

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("could not read scene replay: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid scene replay line {line}: {message}")]
    InvalidLine { line: usize, message: String },
    #[error("scene replay exceeds {limit} byte limit")]
    TooLarge { limit: usize },
}

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type")]
    kind: String,
    id: Option<u64>,
    payload_base64: Option<String>,
    commit: Option<SceneCommit>,
}

/// A finite, immutable scene producer loaded from browser JSONL output.
pub struct JsonlSceneProducer {
    updates: Vec<SceneUpdate>,
}

impl JsonlSceneProducer {
    pub fn from_path(path: impl AsRef<Path>, byte_limit: usize) -> Result<Self, ReplayError> {
        let mut data = Vec::new();
        std::fs::File::open(path)?
            .take((byte_limit as u64).saturating_add(1))
            .read_to_end(&mut data)?;
        Self::from_jsonl(&data, byte_limit)
    }

    pub fn from_jsonl(data: &[u8], byte_limit: usize) -> Result<Self, ReplayError> {
        if data.len() > byte_limit {
            return Err(ReplayError::TooLarge { limit: byte_limit });
        }
        let mut updates = Vec::new();
        let mut pending_resource = None;
        let mut last_revision = 0;
        for (index, line) in data.split(|byte| *byte == b'\n').enumerate() {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let record: Record =
                serde_json::from_slice(line).map_err(|error| ReplayError::InvalidLine {
                    line: index + 1,
                    message: error.to_string(),
                })?;
            match record.kind.as_str() {
                "resource" => {
                    let id = record
                        .id
                        .ok_or_else(|| invalid(index, "resource id is missing"))?;
                    let encoded = record
                        .payload_base64
                        .ok_or_else(|| invalid(index, "resource payload is missing"))?;
                    let payload = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|error| invalid(index, format!("invalid base64: {error}")))?;
                    if pending_resource.is_some() {
                        return Err(invalid(index, "multiple resources precede one commit"));
                    }
                    if payload.len() > u16::MAX as usize {
                        return Err(invalid(
                            index,
                            "resource exceeds the transport payload limit",
                        ));
                    }
                    let image = ImageResource::decode(&payload)
                        .ok_or_else(|| invalid(index, "resource is not a framed PNG"))?;
                    if image.id != id {
                        return Err(invalid(index, "resource id does not match its frame"));
                    }
                    pending_resource = Some(payload);
                }
                "scene_commit" => {
                    let commit = record
                        .commit
                        .ok_or_else(|| invalid(index, "commit is missing"))?;
                    if commit.revision <= last_revision {
                        return Err(invalid(index, "commit revisions must increase"));
                    }
                    last_revision = commit.revision;
                    updates.push(SceneUpdate {
                        resource: pending_resource.take(),
                        commit,
                    });
                }
                "video_frame" => {}
                _ => return Err(invalid(index, "unknown record type")),
            }
        }
        if pending_resource.is_some() {
            return Err(invalid(
                data.split(|byte| *byte == b'\n').count(),
                "resource has no following commit",
            ));
        }
        Ok(Self { updates })
    }

    pub fn len(&self) -> usize {
        self.updates.len()
    }
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }
}

impl SceneProducer for JsonlSceneProducer {
    fn updates(&self, _session: &ozma_protocol::Session) -> Vec<SceneUpdate> {
        self.updates
            .iter()
            .map(|update| SceneUpdate {
                resource: update.resource.clone(),
                commit: update.commit.clone(),
            })
            .collect()
    }
}

fn invalid(line: usize, message: impl Into<String>) -> ReplayError {
    ReplayError::InvalidLine {
        line: line + 1,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &str = "AAAAAAAAAAeJUE5HDQoaCmZpeHR1cmU=";

    fn replay() -> String {
        format!(
            "{{\"type\":\"resource\",\"id\":7,\"payload_base64\":\"{PNG}\"}}\n{{\"type\":\"scene_commit\",\"commit\":{{\"revision\":1,\"images\":[]}}}}\n"
        )
    }

    #[test]
    fn loads_bounded_browser_replay() {
        let producer = JsonlSceneProducer::from_jsonl(replay().as_bytes(), 1024).unwrap();
        assert_eq!(producer.len(), 1);
        assert!(!producer.is_empty());
    }

    #[test]
    fn rejects_uncommitted_resource_and_oversized_input() {
        let data = br#"{"type":"resource","id":7,"payload_base64":"AAAA"}"#;
        assert!(matches!(
            JsonlSceneProducer::from_jsonl(data, 1024),
            Err(ReplayError::InvalidLine { .. })
        ));
        assert!(matches!(
            JsonlSceneProducer::from_jsonl(data, 1),
            Err(ReplayError::TooLarge { .. })
        ));
    }
}
