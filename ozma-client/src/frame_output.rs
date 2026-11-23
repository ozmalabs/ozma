// SPDX-License-Identifier: AGPL-3.0-only
//! Protocol-neutral composed frames for display output adapters.
//!
//! Adapters such as VNC, Cast, and other framebuffer protocols consume this
//! type after scene composition. The queue deliberately keeps only the newest
//! frame because an output that is behind should catch up from current state.

/// A changed rectangle in output coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl DamageRect {
    pub fn full(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    pub fn area(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// An atomically composed RGBA8 output. The pixels are complete even when
/// `damage` contains only changed regions, allowing adapters to request a
/// full refresh after reconnect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposedFrame {
    pub revision: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub damage: Vec<DamageRect>,
}

impl ComposedFrame {
    pub fn new(
        revision: u64,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        damage: Vec<DamageRect>,
    ) -> Option<Self> {
        let expected = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if expected == 0
            || rgba.len() != expected
            || damage.iter().any(|rect| {
                rect.width == 0
                    || rect.height == 0
                    || rect
                        .x
                        .checked_add(rect.width)
                        .is_none_or(|right| right > width)
                    || rect
                        .y
                        .checked_add(rect.height)
                        .is_none_or(|bottom| bottom > height)
            })
        {
            return None;
        }
        Some(Self {
            revision,
            width,
            height,
            rgba,
            damage,
        })
    }

    pub fn full_damage(mut self) -> Self {
        self.damage = vec![DamageRect::full(self.width, self.height)];
        self
    }
}

/// A bounded latest-frame handoff for independent output adapters.
#[derive(Debug, Default)]
pub struct LatestFrame {
    frame: Option<ComposedFrame>,
    last_published: Option<u64>,
}

impl LatestFrame {
    pub fn publish(&mut self, frame: ComposedFrame) -> bool {
        if self
            .last_published
            .is_some_and(|revision| frame.revision <= revision)
        {
            return false;
        }
        self.last_published = Some(frame.revision);
        self.frame = Some(frame);
        true
    }

    pub fn take(&mut self) -> Option<ComposedFrame> {
        self.frame.take()
    }

    pub fn revision(&self) -> Option<u64> {
        self.frame.as_ref().map(|frame| frame.revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_buffers_and_damage() {
        assert!(ComposedFrame::new(1, 2, 1, vec![0; 7], vec![]).is_none());
        assert!(ComposedFrame::new(
            1,
            2,
            1,
            vec![0; 8],
            vec![DamageRect {
                x: 1,
                y: 0,
                width: 2,
                height: 1
            }]
        )
        .is_none());
    }

    #[test]
    fn latest_handoff_coalesces_and_rejects_stale_revisions() {
        let frame = |revision| {
            ComposedFrame::new(
                revision,
                1,
                1,
                vec![revision as u8; 4],
                vec![DamageRect::full(1, 1)],
            )
            .unwrap()
        };
        let mut latest = LatestFrame::default();
        assert!(latest.publish(frame(1)));
        assert!(latest.publish(frame(3)));
        assert!(!latest.publish(frame(2)));
        assert_eq!(latest.revision(), Some(3));
        assert_eq!(latest.take().unwrap().rgba, vec![3; 4]);
        assert!(latest.take().is_none());
        assert!(!latest.publish(frame(2)));
    }

    #[test]
    fn full_damage_supports_reconnect_refresh() {
        let frame = ComposedFrame::new(4, 2, 1, vec![0; 8], vec![])
            .unwrap()
            .full_damage();
        assert_eq!(frame.damage, vec![DamageRect::full(2, 1)]);
    }
}
