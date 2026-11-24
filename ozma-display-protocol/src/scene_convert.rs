// SPDX-License-Identifier: AGPL-3.0-only
//! Conversions between the domain scene model and `ozma.v1.SceneSnapshot`.
//!
//! The domain model in [`ozma_protocol::scene`] describes a full-display video
//! surface plus an ordered list of image placements. The protobuf snapshot is a
//! general node tree. To carry the domain model without ambiguity, nodes use
//! this mapping:
//!
//! | Node | Protobuf id | Parent | Content |
//! |---|---|---|---|
//! | Full-display video | [`VIDEO_NODE_ID`] (`0`) | none (root) | `VideoSurface { id: 1, channel }` |
//! | Image placement `node_id` | `1 + u64::from(node_id)` | `Some(VIDEO_NODE_ID)` | `ResourceRef { id: resource_id }` |
//!
//! The snapshot root list is always `[VIDEO_NODE_ID]`. Image z-order matches
//! the position in [`SceneCommit::images`], so the placement list round-trips in
//! order.
//!
//! Reverse conversion ignores the video node (and any other non-image content)
//! because the domain model keeps the video surface outside the image list. A
//! protobuf image node id that cannot be represented as a `u32` placement id,
//! or a duplicate image node id, is reported as a [`SceneConvertError`].
//!
//! The mapping is intentionally simple rather than derived from display
//! geometry: a snapshot produced here is lossless for the domain model, but it
//! is not a general-purpose scene authoring format. Snapshots from other
//! producers that use arbitrary trees or non-image content lose that
//! structure on the way back into [`SceneCommit`].

use std::collections::HashSet;

use ozma_protocol::scene::{ImagePlacement, SceneCommit};

use crate::proto::{self, scene_node};

/// Protobuf node id reserved for the full-display video surface.
pub const VIDEO_NODE_ID: u64 = 0;

/// `VideoSurface.id` emitted for the full-display video node.
pub const VIDEO_SURFACE_ID: u32 = 1;

/// Offset applied to a domain [`ImagePlacement::node_id`] to obtain its
/// protobuf node id. The offset keeps image nodes away from
/// [`VIDEO_NODE_ID`].
pub const IMAGE_NODE_ID_OFFSET: u64 = 1;

/// Scale factor in parts per million used for emitted image transforms.
pub const IMAGE_SCALE_PPM: i32 = 1_000_000;

/// Opacity in parts per million used for emitted image transforms.
pub const IMAGE_OPACITY_PPM: u32 = 1_000_000;

/// Errors returned when a scene value cannot be mapped across the domain and
/// protobuf representations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SceneConvertError {
    /// The domain commit contains two placements with the same `node_id`, so
    /// the protobuf tree would contain two nodes with the same id.
    #[error("duplicate image node id {node_id} in scene commit")]
    DuplicateImageNodeId {
        /// Duplicated domain node id.
        node_id: u32,
    },

    /// A protobuf image node id does not map back to a `u32` placement id.
    #[error("scene node id {node_id} does not map to a u32 image node id")]
    ImageNodeIdOverflow {
        /// The out-of-range protobuf node id.
        node_id: u64,
    },
}

/// Encodes a domain scene commit as a protobuf [`proto::SceneSnapshot`].
///
/// `video_channel` is the logical channel that carries the full-display video;
/// it is stored on the video surface node.
///
/// # Errors
///
/// Returns [`SceneConvertError::DuplicateImageNodeId`] when two placements
/// share a `node_id`; such a commit would produce an ambiguous node tree.
pub fn scene_commit_to_proto(
    commit: &SceneCommit,
    video_channel: u8,
) -> Result<proto::SceneSnapshot, SceneConvertError> {
    let mut seen = HashSet::with_capacity(commit.images.len());
    let mut nodes = Vec::with_capacity(commit.images.len() + 1);

    nodes.push(proto::SceneNode {
        id: VIDEO_NODE_ID,
        content: Some(scene_node::Content::Video(proto::VideoSurface {
            id: VIDEO_SURFACE_ID,
            channel: u32::from(video_channel),
            ..Default::default()
        })),
        ..Default::default()
    });

    for (index, image) in commit.images.iter().enumerate() {
        if !seen.insert(image.node_id) {
            return Err(SceneConvertError::DuplicateImageNodeId {
                node_id: image.node_id,
            });
        }

        nodes.push(proto::SceneNode {
            id: IMAGE_NODE_ID_OFFSET + u64::from(image.node_id),
            parent: Some(VIDEO_NODE_ID),
            transform: Some(proto::Transform {
                translation_x: image.x,
                translation_y: image.y,
                width: image.width,
                height: image.height,
                scale_x_ppm: IMAGE_SCALE_PPM,
                scale_y_ppm: IMAGE_SCALE_PPM,
                opacity_ppm: Some(IMAGE_OPACITY_PPM),
                ..Default::default()
            }),
            // Placement order is the paint order. Saturating is preferable to
            // failing on an impossible list longer than `i32::MAX` entries.
            z: i32::try_from(index).unwrap_or(i32::MAX),
            content: Some(scene_node::Content::Image(proto::ResourceRef {
                id: image.resource_id,
                ..Default::default()
            })),
            ..Default::default()
        });
    }

    Ok(proto::SceneSnapshot {
        revision: commit.revision,
        nodes,
        roots: vec![VIDEO_NODE_ID],
        base_revision: None,
        definition_id: None,
    })
}

/// Decodes a protobuf [`proto::SceneSnapshot`] into a domain scene commit.
///
/// The video node and any non-image content are ignored. Image nodes become
/// [`ImagePlacement`] entries in snapshot node order.
///
/// # Errors
///
/// Returns [`SceneConvertError::ImageNodeIdOverflow`] when an image node id
/// does not fit the domain model's `u32` node id after removing
/// [`IMAGE_NODE_ID_OFFSET`], and
/// [`SceneConvertError::DuplicateImageNodeId`] when two image nodes map to the
/// same domain node id.
pub fn scene_commit_from_proto(
    snapshot: &proto::SceneSnapshot,
) -> Result<SceneCommit, SceneConvertError> {
    let mut images = Vec::new();
    let mut seen = HashSet::new();

    for node in &snapshot.nodes {
        // The full-display video surface is carried by `SceneSnapshot`'s
        // video node; the domain commit keeps it outside the image list.
        if node.id == VIDEO_NODE_ID {
            continue;
        }

        let Some(scene_node::Content::Image(image)) = &node.content else {
            continue;
        };

        let node_id = u32::try_from(node.id - IMAGE_NODE_ID_OFFSET)
            .map_err(|_| SceneConvertError::ImageNodeIdOverflow { node_id: node.id })?;
        if !seen.insert(node_id) {
            return Err(SceneConvertError::DuplicateImageNodeId { node_id });
        }

        let transform = node.transform.clone().unwrap_or_default();
        images.push(ImagePlacement {
            node_id,
            resource_id: image.id,
            x: transform.translation_x,
            y: transform.translation_y,
            width: transform.width,
            height: transform.height,
        });
    }

    Ok(SceneCommit {
        revision: snapshot.revision,
        images,
    })
}
