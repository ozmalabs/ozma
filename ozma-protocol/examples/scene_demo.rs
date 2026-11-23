// SPDX-License-Identifier: AGPL-3.0-only
//! Deterministic presentation fixture with simulated image and video payloads.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Placement {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    Image {
        asset: &'static str,
        placement: Placement,
    },
    Video {
        frame: u64,
        placement: Placement,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Commit {
    generation: u64,
    nodes: BTreeMap<&'static str, Node>,
}

fn present(
    current: &mut Commit,
    candidate: Commit,
    assets: &BTreeMap<&str, &[u8]>,
) -> Result<(), &'static str> {
    if candidate.generation <= current.generation {
        return Err("stale scene generation");
    }
    for node in candidate.nodes.values() {
        if let Node::Image { asset, .. } = node {
            if !assets.contains_key(asset) {
                return Err("missing image resource");
            }
        }
    }
    *current = candidate;
    Ok(())
}

fn main() {
    let assets = BTreeMap::from([("demo-image", &b"fixture image payload"[..])]);
    let image_placement = Placement {
        x: 20,
        y: 40,
        width: 320,
        height: 180,
    };
    let video_placement = Placement {
        x: 400,
        y: 40,
        width: 640,
        height: 360,
    };
    let mut current = Commit {
        generation: 0,
        nodes: BTreeMap::new(),
    };
    let first = Commit {
        generation: 1,
        nodes: BTreeMap::from([
            (
                "image",
                Node::Image {
                    asset: "demo-image",
                    placement: image_placement.clone(),
                },
            ),
            (
                "video",
                Node::Video {
                    frame: 1,
                    placement: video_placement.clone(),
                },
            ),
        ]),
    };
    present(&mut current, first.clone(), &assets).unwrap();

    let mut scrolled = current.clone();
    scrolled.generation += 1;
    scrolled.nodes.insert(
        "image",
        Node::Image {
            asset: "demo-image",
            placement: Placement {
                y: 10,
                ..image_placement
            },
        },
    );
    present(&mut current, scrolled.clone(), &assets).unwrap();
    assert_ne!(current.nodes["image"], first.nodes["image"]);
    assert_eq!(current.nodes["video"], first.nodes["video"]);
    assert_eq!(assets["demo-image"], b"fixture image payload");

    let mut advanced = current.clone();
    advanced.generation += 1;
    advanced.nodes.insert(
        "video",
        Node::Video {
            frame: 2,
            placement: video_placement,
        },
    );
    present(&mut current, advanced.clone(), &assets).unwrap();
    assert_eq!(current.nodes["image"], scrolled.nodes["image"]);
    assert_ne!(current.nodes["video"], scrolled.nodes["video"]);

    let mut incomplete = current.clone();
    incomplete.generation += 1;
    incomplete.nodes.remove("video");
    incomplete.nodes.insert(
        "missing",
        Node::Image {
            asset: "uncached-image",
            placement: Placement {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
        },
    );
    assert_eq!(
        present(&mut current, incomplete, &assets),
        Err("missing image resource")
    );
    assert_eq!(current, advanced);

    let fallback = Commit {
        generation: current.generation + 1,
        nodes: BTreeMap::from([(
            "display",
            Node::Video {
                frame: 1,
                placement: Placement {
                    x: 0,
                    y: 0,
                    width: 1280,
                    height: 720,
                },
            },
        )]),
    };
    present(&mut current, fallback.clone(), &BTreeMap::new()).unwrap();
    assert_eq!(current, fallback);
    println!("Verified cached placement, independent video, atomic rejection, and full-display fallback.");
}
