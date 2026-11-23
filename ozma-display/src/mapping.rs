// SPDX-License-Identifier: AGPL-3.0-only
//! Input and topology mapping.
//!
//! Everything here is pure: no I/O, no OS calls, no allocation beyond error
//! details. The node uses it to turn a remote client's display-local pointer
//! position into a host-global absolute position, and back again for edge
//! detection.
//!
//! # Coordinate model
//!
//! * A [`DisplayPlacement`] is the **logical** rectangle the host OS uses in
//!   the desktop topology.
//! * A [`LocalPoint`] is a position on the remote panel in its own **physical**
//!   pixels, before host scaling, with the origin at the panel's top-left in
//!   its unrotated orientation. This is what a client reports from its input
//!   surface.
//! * `scale_percent` converts physical pixels to logical units
//!   (`logical = physical * 100 / scale`). Rotation is applied after scaling,
//!   clockwise, so a 90° display reports swapped logical extents.
//! * A [`GlobalPoint`] is a host logical position, offset by the placement's
//!   `x`/`y`.
//!
//! Integer division makes the forward transform lossy at fractional scale
//! factors. [`global_to_local`] returns the smallest physical pixel that maps
//! back to the same global pixel, so `local -> global -> local -> global` is
//! stable even when `local -> global -> local` is not bit-exact.
//!
//! Relative pointer motion is passed through unchanged and must not be scaled
//! or accelerated: the host compositor applies its own pointer acceleration
//! once.

use ozma_protocol::display::{DisplayId, DisplayMode, DisplayPlacement, DisplayTopology, Rotation};
use ozma_protocol::input::InputEvent;
use ozma_protocol::{ApiError, Result};

use crate::error;

/// A display-local physical-pixel position.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LocalPoint {
    pub x: i32,
    pub y: i32,
}

impl LocalPoint {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A host-topology logical position.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct GlobalPoint {
    pub x: i32,
    pub y: i32,
}

impl GlobalPoint {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A raw relative pointer delta, preserved exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RelativeDelta {
    pub dx: i32,
    pub dy: i32,
}

/// Logical size of a mode at a scale and rotation, as the host topology sees
/// it. Rotation swaps the axes at 90°/270°.
pub fn logical_size(mode: &DisplayMode, rotation: Rotation, scale_percent: u16) -> (u32, u32) {
    let scale = u64::from(scale_percent.max(1));
    let width = (u64::from(mode.width) * 100 / scale) as u32;
    let height = (u64::from(mode.height) * 100 / scale) as u32;
    if rotation.swaps_axes() {
        (height, width)
    } else {
        (width, height)
    }
}

/// Unrotated logical size of a placement, recovering the pre-rotation extents.
fn unrotated_size(placement: &DisplayPlacement) -> (i64, i64) {
    if placement.rotation.swaps_axes() {
        (i64::from(placement.height), i64::from(placement.width))
    } else {
        (i64::from(placement.width), i64::from(placement.height))
    }
}

/// Map a display-local physical point to host-global logical coordinates.
///
/// Returns `display.out_of_bounds` when the point is outside the panel.
pub fn local_to_global(placement: &DisplayPlacement, local: LocalPoint) -> Result<GlobalPoint> {
    if local.x < 0 || local.y < 0 {
        return Err(error::out_of_bounds(
            &placement.display_id,
            "local",
            local.x,
            local.y,
            placement.width,
            placement.height,
        ));
    }

    let scale = i64::from(placement.scale_percent.max(1));
    let (width, height) = unrotated_size(placement);

    // physical -> unrotated logical
    let lx = i64::from(local.x) * 100 / scale;
    let ly = i64::from(local.y) * 100 / scale;

    if lx >= width || ly >= height {
        return Err(error::out_of_bounds(
            &placement.display_id,
            "local",
            local.x,
            local.y,
            placement.width,
            placement.height,
        ));
    }

    // unrotated logical -> rotated logical (clockwise)
    let (rx, ry) = match placement.rotation {
        Rotation::Deg0 => (lx, ly),
        Rotation::Deg90 => (height - 1 - ly, lx),
        Rotation::Deg180 => (width - 1 - lx, height - 1 - ly),
        Rotation::Deg270 => (ly, width - 1 - lx),
    };

    Ok(GlobalPoint::new(
        placement.x + rx as i32,
        placement.y + ry as i32,
    ))
}

/// Map host-global logical coordinates into a display's local physical point.
///
/// This is the inverse used for edge crossing: if the global point is not on
/// the display, the cursor has left it and the client must stop injecting.
pub fn global_to_local(placement: &DisplayPlacement, global: GlobalPoint) -> Result<LocalPoint> {
    let rel_x = i64::from(global.x) - i64::from(placement.x);
    let rel_y = i64::from(global.y) - i64::from(placement.y);

    if rel_x < 0
        || rel_y < 0
        || rel_x >= i64::from(placement.width)
        || rel_y >= i64::from(placement.height)
    {
        return Err(error::out_of_bounds(
            &placement.display_id,
            "global",
            global.x,
            global.y,
            placement.width,
            placement.height,
        ));
    }

    let (width, height) = unrotated_size(placement);

    // rotated logical -> unrotated logical
    let (lx, ly) = match placement.rotation {
        Rotation::Deg0 => (rel_x, rel_y),
        Rotation::Deg90 => (rel_y, height - 1 - rel_x),
        Rotation::Deg180 => (width - 1 - rel_x, height - 1 - rel_y),
        Rotation::Deg270 => (width - 1 - rel_y, rel_x),
    };

    // unrotated logical -> physical. Ceiling division picks the first physical
    // pixel whose forward mapping lands on `lx`/`ly`.
    let scale = i64::from(placement.scale_percent.max(1));
    let x = (lx * scale + 99) / 100;
    let y = (ly * scale + 99) / 100;

    let x = i32::try_from(x).unwrap_or(i32::MAX);
    let y = i32::try_from(y).unwrap_or(i32::MAX);
    Ok(LocalPoint::new(x, y))
}

/// Which display contains a global point, if any.
pub fn display_at<'a>(topology: &'a DisplayTopology, global: GlobalPoint) -> Option<&'a DisplayPlacement> {
    topology
        .displays
        .iter()
        .find(|placement| placement.contains(global.x, global.y))
}

/// The display a cursor entered at `global`, if any.
///
/// Callers use this when the client reports that the pointer left its surface:
/// a `Some(id)` different from the active display means the host compositor
/// moved the cursor onto another monitor, so the client must stop injecting.
pub fn edge_crossing(topology: &DisplayTopology, global: GlobalPoint) -> Option<DisplayId> {
    display_at(topology, global).map(|placement| placement.display_id.clone())
}

/// Resolve a display in a topology or return `not_found`.
fn placement_for<'a>(topology: &'a DisplayTopology, id: &DisplayId) -> Result<&'a DisplayPlacement> {
    topology
        .find(id)
        .ok_or_else(|| error::unknown_display(id))
}

/// Map an absolute point for a named display.
pub fn map_absolute(
    topology: &DisplayTopology,
    id: &DisplayId,
    local: LocalPoint,
) -> Result<GlobalPoint> {
    let placement = placement_for(topology, id)?;
    local_to_global(placement, local)
}

/// Inverse of [`map_absolute`], used to decide whether a cursor is still on a
/// display.
pub fn unmap_absolute(
    topology: &DisplayTopology,
    id: &DisplayId,
    global: GlobalPoint,
) -> Result<LocalPoint> {
    let placement = placement_for(topology, id)?;
    global_to_local(placement, global)
}

/// Rewrite an absolute pointer event into host-global coordinates.
///
/// The returned event keeps the display id so the injector can distinguish a
/// virtual display from a physical one if it needs to. Events that are not
/// absolute pointer events are rejected with `invalid_request`; use
/// [`map_relative_input`] for pointer-lock deltas.
pub fn map_absolute_input(
    topology: &DisplayTopology,
    event: &InputEvent,
) -> Result<InputEvent> {
    match event {
        InputEvent::PointerAbsolute {
            display_id: Some(id),
            x,
            y,
        } => {
            let global = map_absolute(topology, id, LocalPoint::new(*x, *y))?;
            Ok(InputEvent::PointerAbsolute {
                display_id: Some(id.clone()),
                x: global.x,
                y: global.y,
            })
        }
        InputEvent::PointerAbsolute {
            display_id: None, ..
        } => Err(ApiError::invalid(
            "An absolute pointer event on a virtual display must name the display",
        )),
        _ => Err(ApiError::invalid(
            "Only PointerAbsolute events are mapped through map_absolute_input",
        )),
    }
}

/// Relative pointer motion passes through unchanged.
///
/// No acceleration, no scale, no rotation: the remote client has already
/// expressed the delta in the host's pointer space and the host applies its
/// own acceleration exactly once.
pub fn relative_passthrough(dx: i32, dy: i32) -> RelativeDelta {
    RelativeDelta { dx, dy }
}

/// Rewrite a relative pointer event. The delta is preserved bit-exactly.
pub fn map_relative_input(event: &InputEvent) -> Result<InputEvent> {
    match event {
        InputEvent::PointerRelative { dx, dy } => {
            let delta = relative_passthrough(*dx, *dy);
            Ok(InputEvent::PointerRelative {
                dx: delta.dx,
                dy: delta.dy,
            })
        }
        _ => Err(ApiError::invalid(
            "Only PointerRelative events pass through map_relative_input",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_protocol::display::DisplayMode;

    fn placement(
        id: &str,
        x: i32,
        y: i32,
        mode: &DisplayMode,
        rotation: Rotation,
        scale: u16,
    ) -> DisplayPlacement {
        let (width, height) = logical_size(mode, rotation, scale);
        DisplayPlacement {
            display_id: DisplayId::new(id),
            x,
            y,
            width,
            height,
            scale_percent: scale,
            rotation,
            primary: x == 0 && y == 0,
        }
    }

    fn panel() -> DisplayMode {
        DisplayMode::new(1920, 1080, 60)
    }

    #[test]
    fn logical_size_scales_and_swaps() {
        let mode = DisplayMode::new(1920, 1080, 60);
        assert_eq!(logical_size(&mode, Rotation::Deg0, 100), (1920, 1080));
        assert_eq!(logical_size(&mode, Rotation::Deg0, 125), (1536, 864));
        assert_eq!(logical_size(&mode, Rotation::Deg0, 150), (1280, 720));
        assert_eq!(logical_size(&mode, Rotation::Deg90, 100), (1080, 1920));
        assert_eq!(logical_size(&mode, Rotation::Deg270, 100), (1080, 1920));
        assert_eq!(logical_size(&mode, Rotation::Deg180, 100), (1920, 1080));
    }

    #[test]
    fn maps_at_100_125_and_150_percent() {
        let mode = panel();
        for (scale, local, expected) in [
            (100, (960, 540), (960, 540)),
            (125, (1250, 500), (1000, 400)),
            (150, (1440, 360), (960, 240)),
        ] {
            let display = placement("remote", 0, 0, &mode, Rotation::Deg0, scale);
            assert_eq!(
                local_to_global(&display, LocalPoint::new(local.0, local.1)).unwrap(),
                GlobalPoint::new(expected.0, expected.1),
                "scale {scale}%"
            );
        }
    }

    #[test]
    fn applies_multi_display_offset() {
        let mode = panel();
        let primary = placement("primary", 0, 0, &mode, Rotation::Deg0, 100);
        let right = placement("right", 1920, 0, &mode, Rotation::Deg0, 100);
        let below = placement("below", 0, 1080, &mode, Rotation::Deg0, 100);
        assert_eq!(
            local_to_global(&right, LocalPoint::new(0, 0)).unwrap(),
            GlobalPoint::new(1920, 0)
        );
        assert_eq!(
            local_to_global(&below, LocalPoint::new(10, 20)).unwrap(),
            GlobalPoint::new(10, 1100)
        );
        assert_eq!(
            local_to_global(&primary, LocalPoint::new(1919, 1079)).unwrap(),
            GlobalPoint::new(1919, 1079)
        );
    }

    #[test]
    fn maps_all_rotations_clockwise() {
        let mode = panel();
        let local = LocalPoint::new(10, 20);

        let d0 = placement("d0", 0, 0, &mode, Rotation::Deg0, 100);
        assert_eq!(
            local_to_global(&d0, local).unwrap(),
            GlobalPoint::new(10, 20)
        );

        let d90 = placement("d90", 0, 0, &mode, Rotation::Deg90, 100);
        assert_eq!(d90.width, 1080);
        assert_eq!(d90.height, 1920);
        assert_eq!(
            local_to_global(&d90, local).unwrap(),
            GlobalPoint::new(1080 - 1 - 20, 10)
        );

        let d180 = placement("d180", 0, 0, &mode, Rotation::Deg180, 100);
        assert_eq!(
            local_to_global(&d180, local).unwrap(),
            GlobalPoint::new(1920 - 1 - 10, 1080 - 1 - 20)
        );

        let d270 = placement("d270", 0, 0, &mode, Rotation::Deg270, 100);
        assert_eq!(
            local_to_global(&d270, local).unwrap(),
            GlobalPoint::new(20, 1920 - 1 - 10)
        );
    }

    #[test]
    fn round_trips_all_rotations_and_scales() {
        let mode = panel();
        for rotation in [
            Rotation::Deg0,
            Rotation::Deg90,
            Rotation::Deg180,
            Rotation::Deg270,
        ] {
            for scale in [100u16, 125, 150] {
                let display = placement("remote", 100, 50, &mode, rotation, scale);
                // Multiples that survive integer scaling exactly at all three
                // factors (LCM of 5 and 3 is 15; use 30).
                for (x, y) in [(0, 0), (30, 60), (300, 450)] {
                    let global = local_to_global(&display, LocalPoint::new(x, y)).unwrap();
                    let back = global_to_local(&display, global).unwrap();
                    assert_eq!(
                        back,
                        LocalPoint::new(x, y),
                        "rotation {rotation:?} scale {scale} point ({x},{y})"
                    );
                }
            }
        }
    }

    #[test]
    fn inverse_is_stable_even_when_not_bit_exact() {
        let mode = panel();
        let display = placement("remote", 0, 0, &mode, Rotation::Deg90, 125);
        for x in 0..300 {
            for y in 0..100 {
                let global = local_to_global(&display, LocalPoint::new(x, y)).unwrap();
                let back = global_to_local(&display, global).unwrap();
                assert_eq!(local_to_global(&display, back).unwrap(), global);
            }
        }
    }

    #[test]
    fn rejects_local_out_of_bounds() {
        let mode = panel();
        let display = placement("remote", 0, 0, &mode, Rotation::Deg0, 100);
        for point in [(-1, 0), (0, -1), (1920, 0), (0, 1080)] {
            let err = local_to_global(&display, LocalPoint::new(point.0, point.1)).unwrap_err();
            assert_eq!(err.code, crate::error::DISPLAY_OUT_OF_BOUNDS);
        }
    }

    #[test]
    fn rejects_global_out_of_bounds_for_edge_crossing() {
        let mode = panel();
        let display = placement("remote", 1920, 0, &mode, Rotation::Deg0, 100);
        let err = global_to_local(&display, GlobalPoint::new(1919, 500)).unwrap_err();
        assert_eq!(err.code, crate::error::DISPLAY_OUT_OF_BOUNDS);
        let err = global_to_local(&display, GlobalPoint::new(3840, 500)).unwrap_err();
        assert_eq!(err.code, crate::error::DISPLAY_OUT_OF_BOUNDS);
        assert!(global_to_local(&display, GlobalPoint::new(1920, 0)).is_ok());
    }

    #[test]
    fn rejects_unknown_display() {
        let mode = panel();
        let topology = DisplayTopology::new(vec![placement(
            "primary",
            0,
            0,
            &mode,
            Rotation::Deg0,
            100,
        )]);
        let err = map_absolute(
            &topology,
            &DisplayId::new("missing"),
            LocalPoint::new(1, 1),
        )
        .unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::NOT_FOUND);
        assert_eq!(
            err.details["display_id"],
            serde_json::Value::String("missing".into())
        );
    }

    #[test]
    fn hit_tests_display_at_global_point() {
        let mode = panel();
        let topology = DisplayTopology::new(vec![
            placement("left", 0, 0, &mode, Rotation::Deg0, 100),
            placement("right", 1920, 0, &mode, Rotation::Deg0, 100),
        ]);
        assert_eq!(
            display_at(&topology, GlobalPoint::new(10, 10)).map(|d| d.display_id.clone()),
            Some(DisplayId::new("left"))
        );
        assert_eq!(
            display_at(&topology, GlobalPoint::new(1930, 10)).map(|d| d.display_id.clone()),
            Some(DisplayId::new("right"))
        );
        assert!(display_at(&topology, GlobalPoint::new(5000, 10)).is_none());
        assert_eq!(
            edge_crossing(&topology, GlobalPoint::new(1920, 0)),
            Some(DisplayId::new("right"))
        );
        assert_eq!(edge_crossing(&topology, GlobalPoint::new(0, 0)), Some(DisplayId::new("left")));
        assert_eq!(edge_crossing(&topology, GlobalPoint::new(5000, 0)), None);
    }

    #[test]
    fn relative_motion_is_not_accelerated_or_scaled() {
        let event = InputEvent::PointerRelative { dx: -17, dy: 33 };
        assert_eq!(map_relative_input(&event).unwrap(), event);
        assert_eq!(relative_passthrough(-17, 33).dx, -17);
        assert_eq!(relative_passthrough(-17, 33).dy, 33);
        // A 150% display must not multiply the delta.
        let display = placement(
            "remote",
            0,
            0,
            &panel(),
            Rotation::Deg90,
            150,
        );
        assert!(display.scale_percent == 150);
        assert_eq!(
            map_relative_input(&InputEvent::PointerRelative { dx: 3, dy: 4 }).unwrap(),
            InputEvent::PointerRelative { dx: 3, dy: 4 }
        );
    }

    #[test]
    fn maps_absolute_events_with_display_id() {
        let mode = panel();
        let topology = DisplayTopology::new(vec![
            placement("left", 0, 0, &mode, Rotation::Deg0, 100),
            placement("right", 1920, 0, &mode, Rotation::Deg0, 100),
        ]);
        let event = InputEvent::PointerAbsolute {
            display_id: Some(DisplayId::new("right")),
            x: 640,
            y: 360,
        };
        let mapped = map_absolute_input(&topology, &event).unwrap();
        assert_eq!(
            mapped,
            InputEvent::PointerAbsolute {
                display_id: Some(DisplayId::new("right")),
                x: 2560,
                y: 360,
            }
        );

        let missing = InputEvent::PointerAbsolute {
            display_id: None,
            x: 1,
            y: 2,
        };
        assert!(map_absolute_input(&topology, &missing).is_err());
        assert!(map_relative_input(&missing).is_err());
    }
}
