// SPDX-License-Identifier: AGPL-3.0-only
//! Minimal fixture mapper for Xpra window updates.
//!
//! Xpra's packet framing and pixel encodings are handled by a future adapter.
//! This module maps already-decoded geometry, raster, scroll, and removal
//! events into Ozma's scene representation.

use std::collections::BTreeMap;

use crate::scene::{ImagePlacement, SceneCommit};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XpraUpdate {
    Geometry {
        window_id: u32,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    Raster {
        window_id: u32,
        resource_id: u64,
    },
    /// Fixture-level placement delta. Xpra region-copy scroll packets
    /// require a raster backing store before they can be mapped.
    Move {
        window_id: u32,
        dx: i32,
        dy: i32,
    },
    Remove {
        window_id: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateError {
    UnknownWindow,
    InvalidGeometry,
    RevisionOverflow,
}

#[derive(Clone, Debug)]
struct Window {
    placement: ImagePlacement,
    has_raster: bool,
}

/// Maps decoded Xpra-like fixture events to complete Ozma image overlay commits.
pub struct XpraSceneMapper {
    revision: u64,
    windows: BTreeMap<u32, Window>,
}

impl Default for XpraSceneMapper {
    fn default() -> Self {
        Self {
            revision: 0,
            windows: BTreeMap::new(),
        }
    }
}

impl XpraSceneMapper {
    pub fn apply(&mut self, update: XpraUpdate) -> Result<SceneCommit, UpdateError> {
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(UpdateError::RevisionOverflow)?;
        match update {
            XpraUpdate::Geometry {
                window_id,
                x,
                y,
                width,
                height,
            } => {
                if width == 0 || height == 0 {
                    return Err(UpdateError::InvalidGeometry);
                }
                let window = self.windows.entry(window_id).or_insert_with(|| Window {
                    placement: ImagePlacement {
                        node_id: window_id,
                        resource_id: 0,
                        x,
                        y,
                        width,
                        height,
                    },
                    has_raster: false,
                });
                window.placement.x = x;
                window.placement.y = y;
                window.placement.width = width;
                window.placement.height = height;
            }
            XpraUpdate::Raster {
                window_id,
                resource_id,
            } => {
                let window = self
                    .windows
                    .get_mut(&window_id)
                    .ok_or(UpdateError::UnknownWindow)?;
                window.placement.resource_id = resource_id;
                window.has_raster = true;
            }
            XpraUpdate::Move { window_id, dx, dy } => {
                let window = self
                    .windows
                    .get_mut(&window_id)
                    .ok_or(UpdateError::UnknownWindow)?;
                let x = window
                    .placement
                    .x
                    .checked_add(dx)
                    .ok_or(UpdateError::InvalidGeometry)?;
                let y = window
                    .placement
                    .y
                    .checked_add(dy)
                    .ok_or(UpdateError::InvalidGeometry)?;
                window.placement.x = x;
                window.placement.y = y;
            }
            XpraUpdate::Remove { window_id } => {
                self.windows
                    .remove(&window_id)
                    .ok_or(UpdateError::UnknownWindow)?;
            }
        }
        self.revision = revision;
        Ok(SceneCommit {
            revision: self.revision,
            images: self
                .windows
                .values()
                .filter(|w| w.has_raster)
                .map(|w| w.placement.clone())
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_raster_scroll_and_remove_map_to_scene_commits() {
        let mut mapper = XpraSceneMapper::default();
        let geometry = mapper
            .apply(XpraUpdate::Geometry {
                window_id: 7,
                x: 20,
                y: 30,
                width: 640,
                height: 480,
            })
            .unwrap();
        assert!(geometry.images.is_empty());

        let raster = mapper
            .apply(XpraUpdate::Raster {
                window_id: 7,
                resource_id: 99,
            })
            .unwrap();
        assert_eq!(raster.images[0].resource_id, 99);
        let scrolled = mapper
            .apply(XpraUpdate::Move {
                window_id: 7,
                dx: 0,
                dy: -100,
            })
            .unwrap();
        assert_eq!((scrolled.images[0].x, scrolled.images[0].y), (20, -70));

        let removed = mapper.apply(XpraUpdate::Remove { window_id: 7 }).unwrap();
        assert!(removed.images.is_empty());
        assert_eq!(removed.revision, 4);
    }

    #[test]
    fn rejects_invalid_or_unknown_updates() {
        let mut mapper = XpraSceneMapper::default();
        assert_eq!(
            mapper.apply(XpraUpdate::Move {
                window_id: 1,
                dx: 1,
                dy: 1
            }),
            Err(UpdateError::UnknownWindow)
        );
        assert_eq!(
            mapper.apply(XpraUpdate::Geometry {
                window_id: 1,
                x: 0,
                y: 0,
                width: 0,
                height: 1
            }),
            Err(UpdateError::InvalidGeometry)
        );
    }
}
