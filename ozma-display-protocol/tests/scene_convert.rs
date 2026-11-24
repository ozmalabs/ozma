// SPDX-License-Identifier: AGPL-3.0-only
//! Round-trip and error tests for domain scene <-> protobuf snapshot conversion.

use ozma_display_protocol::proto::{self, scene_node};
use ozma_display_protocol::scene_convert::{
    scene_commit_from_proto, scene_commit_to_proto, SceneConvertError, IMAGE_NODE_ID_OFFSET,
    IMAGE_OPACITY_PPM, IMAGE_SCALE_PPM, VIDEO_NODE_ID, VIDEO_SURFACE_ID,
};
use ozma_protocol::scene::{ImagePlacement, SceneCommit};

fn sample_commit() -> SceneCommit {
    SceneCommit {
        revision: 9,
        images: vec![
            ImagePlacement {
                node_id: 1,
                resource_id: 0x0123_4567_89ab_cdef,
                x: -10,
                y: 20,
                width: 640,
                height: 480,
            },
            ImagePlacement {
                node_id: 2,
                resource_id: 42,
                x: 25,
                y: -35,
                width: 64,
                height: 48,
            },
            ImagePlacement {
                node_id: 7,
                resource_id: u64::MAX,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
        ],
    }
}

fn image_node(id: u64, resource_id: u64) -> proto::SceneNode {
    proto::SceneNode {
        id,
        content: Some(scene_node::Content::Image(proto::ResourceRef {
            id: resource_id,
            ..Default::default()
        })),
        ..Default::default()
    }
}

fn video_node(channel: u32) -> proto::SceneNode {
    proto::SceneNode {
        id: VIDEO_NODE_ID,
        content: Some(scene_node::Content::Video(proto::VideoSurface {
            id: VIDEO_SURFACE_ID,
            channel,
            ..Default::default()
        })),
        ..Default::default()
    }
}

#[test]
fn domain_commit_roundtrips_through_a_proto_snapshot() {
    let commit = sample_commit();
    let snapshot = scene_commit_to_proto(&commit, 3).expect("encode commit");

    assert_eq!(snapshot.revision, commit.revision);
    assert_eq!(snapshot.roots, vec![VIDEO_NODE_ID]);
    assert_eq!(snapshot.nodes.len(), commit.images.len() + 1);

    let video = &snapshot.nodes[0];
    assert_eq!(video.id, VIDEO_NODE_ID);
    assert_eq!(video.parent, None);
    match &video.content {
        Some(scene_node::Content::Video(surface)) => {
            assert_eq!(surface.id, VIDEO_SURFACE_ID);
            assert_eq!(surface.channel, 3);
        }
        other => panic!("first node is not video: {other:?}"),
    }

    for (index, (node, image)) in snapshot.nodes[1..].iter().zip(&commit.images).enumerate() {
        assert_eq!(node.id, IMAGE_NODE_ID_OFFSET + u64::from(image.node_id));
        assert_eq!(node.parent, Some(VIDEO_NODE_ID));
        assert_eq!(node.z, index as i32);

        let transform = node.transform.as_ref().expect("image transform");
        assert_eq!(transform.translation_x, image.x);
        assert_eq!(transform.translation_y, image.y);
        assert_eq!(transform.width, image.width);
        assert_eq!(transform.height, image.height);
        assert_eq!(transform.scale_x_ppm, IMAGE_SCALE_PPM);
        assert_eq!(transform.scale_y_ppm, IMAGE_SCALE_PPM);
        assert_eq!(transform.opacity_ppm, Some(IMAGE_OPACITY_PPM));

        match &node.content {
            Some(scene_node::Content::Image(resource)) => {
                assert_eq!(resource.id, image.resource_id);
            }
            other => panic!("image node has unexpected content: {other:?}"),
        }
    }

    let decoded = scene_commit_from_proto(&snapshot).expect("decode commit");
    assert_eq!(decoded, commit);
    assert_eq!(decoded.revision, commit.revision);
    assert_eq!(decoded.images.len(), commit.images.len());
    for (actual, expected) in decoded.images.iter().zip(&commit.images) {
        assert_eq!(actual.node_id, expected.node_id);
        assert_eq!(actual.resource_id, expected.resource_id);
        assert_eq!(actual.x, expected.x);
        assert_eq!(actual.y, expected.y);
        assert_eq!(actual.width, expected.width);
        assert_eq!(actual.height, expected.height);
    }
}

#[test]
fn reverse_conversion_ignores_the_video_node_and_non_image_content() {
    let snapshot = proto::SceneSnapshot {
        revision: 4,
        nodes: vec![
            video_node(2),
            image_node(IMAGE_NODE_ID_OFFSET, 100),
            proto::SceneNode {
                id: 2,
                z: 5,
                content: Some(scene_node::Content::Text(proto::TextContent {
                    text: "not an image".to_string(),
                    ..Default::default()
                })),
                ..Default::default()
            },
        ],
        roots: vec![VIDEO_NODE_ID],
        ..Default::default()
    };

    let commit = scene_commit_from_proto(&snapshot).expect("decode snapshot");
    assert_eq!(
        commit,
        SceneCommit {
            revision: 4,
            images: vec![ImagePlacement {
                node_id: 0,
                resource_id: 100,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }],
        }
    );
}

#[test]
fn video_only_snapshot_decodes_to_an_empty_image_list() {
    let snapshot = proto::SceneSnapshot {
        revision: 0,
        nodes: vec![video_node(1)],
        roots: vec![VIDEO_NODE_ID],
        ..Default::default()
    };

    assert_eq!(
        scene_commit_from_proto(&snapshot).expect("decode video-only snapshot"),
        SceneCommit {
            revision: 0,
            images: Vec::new(),
        }
    );
}

#[test]
fn duplicate_image_node_ids_are_rejected_in_both_directions() {
    let mut commit = sample_commit();
    commit.images[1].node_id = commit.images[0].node_id;
    assert_eq!(
        scene_commit_to_proto(&commit, 1),
        Err(SceneConvertError::DuplicateImageNodeId { node_id: 1 })
    );

    let snapshot = proto::SceneSnapshot {
        revision: 1,
        nodes: vec![
            image_node(IMAGE_NODE_ID_OFFSET, 10),
            image_node(IMAGE_NODE_ID_OFFSET, 11),
        ],
        roots: vec![VIDEO_NODE_ID],
        ..Default::default()
    };
    assert_eq!(
        scene_commit_from_proto(&snapshot),
        Err(SceneConvertError::DuplicateImageNodeId { node_id: 0 })
    );
}

#[test]
fn image_node_ids_that_do_not_fit_u32_are_rejected() {
    let overflowing_id = u64::from(u32::MAX) + IMAGE_NODE_ID_OFFSET + 1;
    let snapshot = proto::SceneSnapshot {
        revision: 1,
        nodes: vec![image_node(overflowing_id, 10)],
        roots: vec![VIDEO_NODE_ID],
        ..Default::default()
    };

    assert_eq!(
        scene_commit_from_proto(&snapshot),
        Err(SceneConvertError::ImageNodeIdOverflow {
            node_id: overflowing_id
        })
    );
}

#[test]
fn maximum_domain_node_id_roundtrips() {
    let commit = SceneCommit {
        revision: 1,
        images: vec![ImagePlacement {
            node_id: u32::MAX,
            resource_id: 1,
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        }],
    };

    let snapshot = scene_commit_to_proto(&commit, 0).expect("encode max node id");
    assert_eq!(
        snapshot.nodes[1].id,
        IMAGE_NODE_ID_OFFSET + u64::from(u32::MAX)
    );
    assert_eq!(
        scene_commit_from_proto(&snapshot).expect("decode max node id"),
        commit
    );
}
