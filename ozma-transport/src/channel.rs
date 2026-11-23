// SPDX-License-Identifier: AGPL-3.0-only
//! Typed channel ids and the negotiated channel map.
//!
//! Both peers of a session must agree on which channel carries which kind of
//! data, whether it is reliable, and how large a single payload may be. The map
//! is derived from [`ozma_protocol::ChannelDescriptor`]s, normally those in the
//! [`ozma_protocol::TransportDescriptor`] returned in a session offer.

use std::collections::BTreeMap;
use std::fmt;

use ozma_protocol::{
    codes, ApiError, ChannelDescriptor, ChannelKind, ChannelPriority, TrackId, TransportDescriptor,
};

use crate::error::{channel_unknown, payload_too_large};

/// The wire id of a logical channel.
///
/// A newtype keeps channel ids from being confused with other small integers
/// (track indexes, stream ids, priorities) at call sites.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelId(u8);

impl ChannelId {
    /// Control channel: session events, input, clipboard, cursor.
    pub const CONTROL: Self = Self(0);
    /// Input channel: keyboard, mouse, gamepad.
    pub const INPUT: Self = Self(1);
    /// First video track. Additional tracks are allocated sequentially.
    pub const VIDEO: Self = Self(2);
    /// Audio channel.
    pub const AUDIO: Self = Self(3);
    /// Microphone/return-audio channel.
    pub const MIC: Self = Self(4);
    /// Telemetry and reports.
    pub const TELEMETRY: Self = Self(5);
    /// Reliable bulk data: files, USB/IP, firmware.
    pub const BULK: Self = Self(6);

    /// Construct a channel id from its wire value.
    pub const fn new(raw: u8) -> Self {
        Self(raw)
    }

    /// The channel id for video track `index`, where `0` is the first track.
    ///
    /// This matches [`ChannelMap::for_tracks`], which allocates video channels
    /// from id 2 in track order.
    pub const fn video_track(index: u8) -> Self {
        Self(2u8.saturating_add(index))
    }

    /// The raw wire value.
    pub const fn raw(self) -> u8 {
        self.0
    }
}

impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<ChannelId> for u8 {
    fn from(value: ChannelId) -> Self {
        value.0
    }
}

impl From<u8> for ChannelId {
    fn from(value: u8) -> Self {
        Self(value)
    }
}

/// The validated set of channels a transport carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelMap {
    channels: BTreeMap<u8, ChannelDescriptor>,
}

impl ChannelMap {
    /// Build a map from descriptors, rejecting duplicate ids and zero-sized
    /// payload limits. An empty map is legal at construction time but cannot
    /// carry a session; [`crate::offer::SessionOfferBuilder`] rejects it.
    pub fn new(descriptors: impl IntoIterator<Item = ChannelDescriptor>) -> Result<Self, ApiError> {
        let mut channels = BTreeMap::new();
        for descriptor in descriptors {
            if descriptor.max_payload == 0 {
                return Err(ApiError::new(
                    codes::INVALID_REQUEST,
                    format!("channel {} has a zero max_payload", descriptor.id),
                ));
            }
            let id = descriptor.id;
            if channels.insert(id, descriptor).is_some() {
                return Err(ApiError::new(
                    codes::INVALID_REQUEST,
                    "duplicate channel id in transport descriptor",
                )
                .with_details(format!("channel id {id}")));
            }
        }
        Ok(Self { channels })
    }

    /// Build from a transport descriptor's channel list.
    pub fn from_descriptor(descriptor: &TransportDescriptor) -> Result<Self, ApiError> {
        Self::new(descriptor.channels.iter().cloned())
    }

    /// The conventional single-track session map used by the node and native
    /// clients: control, input, one video track, audio, mic, telemetry, bulk.
    pub fn default_session() -> Self {
        Self::for_tracks(&[TrackId::new("video0")], true, true)
            .expect("default channel layout is always valid")
    }

    /// Allocate channels for a session with `track_ids` video tracks.
    ///
    /// Ids 0 and 1 are reserved for control and input; video tracks follow from
    /// id 2 in the supplied order; audio, mic, telemetry and bulk follow.
    pub fn for_tracks(track_ids: &[TrackId], audio: bool, mic: bool) -> Result<Self, ApiError> {
        let mut descriptors = Vec::new();
        descriptors.push(ChannelDescriptor {
            id: ChannelId::CONTROL.raw(),
            kind: ChannelKind::Control,
            priority: ChannelPriority::Realtime,
            reliable: true,
            track_id: None,
            max_payload: u16::MAX,
        });
        descriptors.push(ChannelDescriptor {
            id: ChannelId::INPUT.raw(),
            kind: ChannelKind::Input,
            priority: ChannelPriority::Realtime,
            reliable: true,
            track_id: None,
            max_payload: 1200,
        });

        let mut next_id = ChannelId::VIDEO.raw();
        for track_id in track_ids {
            descriptors.push(ChannelDescriptor {
                id: next_id,
                kind: ChannelKind::Video,
                priority: ChannelPriority::Normal,
                reliable: false,
                track_id: Some(track_id.clone()),
                max_payload: 1200,
            });
            next_id = next_id.checked_add(1).ok_or_else(|| {
                ApiError::new(
                    codes::INVALID_REQUEST,
                    "too many tracks for the channel id space",
                )
            })?;
        }
        if audio {
            descriptors.push(ChannelDescriptor {
                id: next_id,
                kind: ChannelKind::Audio,
                priority: ChannelPriority::High,
                reliable: false,
                track_id: None,
                max_payload: 1200,
            });
            next_id = next_id.checked_add(1).ok_or_else(|| {
                ApiError::new(
                    codes::INVALID_REQUEST,
                    "too many channels for the channel id space",
                )
            })?;
        }
        if mic {
            descriptors.push(ChannelDescriptor {
                id: next_id,
                kind: ChannelKind::Mic,
                priority: ChannelPriority::High,
                reliable: false,
                track_id: None,
                max_payload: 1200,
            });
            next_id = next_id.checked_add(1).ok_or_else(|| {
                ApiError::new(
                    codes::INVALID_REQUEST,
                    "too many channels for the channel id space",
                )
            })?;
        }
        descriptors.push(ChannelDescriptor {
            id: next_id,
            kind: ChannelKind::Telemetry,
            priority: ChannelPriority::Low,
            reliable: true,
            track_id: None,
            max_payload: u16::MAX,
        });
        next_id = next_id.checked_add(1).ok_or_else(|| {
            ApiError::new(
                codes::INVALID_REQUEST,
                "too many channels for the channel id space",
            )
        })?;
        descriptors.push(ChannelDescriptor {
            id: next_id,
            kind: ChannelKind::Bulk,
            priority: ChannelPriority::Bulk,
            reliable: true,
            track_id: None,
            max_payload: u16::MAX,
        });
        next_id = next_id.checked_add(1).ok_or_else(|| {
            ApiError::new(
                codes::INVALID_REQUEST,
                "too many channels for the channel id space",
            )
        })?;

        // Browser -> node encoded camera video. It is appended last so adding
        // it never shifts the id of control, input, video, audio, mic,
        // telemetry, or bulk channels used by existing clients.
        descriptors.push(ChannelDescriptor {
            id: next_id,
            kind: ChannelKind::Camera,
            priority: ChannelPriority::Normal,
            reliable: false,
            track_id: Some(TrackId::new("camera0")),
            max_payload: 1200,
        });

        Self::new(descriptors)
    }

    /// Look up a descriptor, if this map carries the channel.
    pub fn get(&self, channel: ChannelId) -> Option<&ChannelDescriptor> {
        self.channels.get(&channel.raw())
    }

    /// The first channel of `kind`, in channel-id order.
    ///
    /// Channel ids are negotiated, so callers should resolve by kind instead
    /// of assuming the conventional layout.
    pub fn channel_for_kind(&self, kind: ChannelKind) -> Option<ChannelId> {
        self.channels
            .values()
            .find(|descriptor| descriptor.kind == kind)
            .map(|descriptor| ChannelId::new(descriptor.id))
    }

    /// Every channel of `kind`, in channel-id order.
    pub fn channels_for_kind(&self, kind: ChannelKind) -> Vec<ChannelId> {
        self.channels
            .values()
            .filter(|descriptor| descriptor.kind == kind)
            .map(|descriptor| ChannelId::new(descriptor.id))
            .collect()
    }

    /// The descriptor for the first channel of `kind`.
    pub fn descriptor_for_kind(&self, kind: ChannelKind) -> Option<&ChannelDescriptor> {
        self.channels
            .values()
            .find(|descriptor| descriptor.kind == kind)
    }

    /// Look up a descriptor or return a typed `invalid_request` error.
    pub fn require(&self, channel: ChannelId) -> Result<&ChannelDescriptor, ApiError> {
        self.get(channel).ok_or_else(|| channel_unknown(channel))
    }

    /// The number of channels.
    pub fn len(&self) -> usize {
        self.channels.len()
    }

    /// True when no channels are carried.
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    /// Iterate descriptors in channel-id order.
    pub fn iter(&self) -> impl Iterator<Item = &ChannelDescriptor> {
        self.channels.values()
    }

    /// Clone the descriptors in channel-id order.
    pub fn descriptors(&self) -> Vec<ChannelDescriptor> {
        self.channels.values().cloned().collect()
    }

    /// True when the channel is reliable and must use a stream, not a
    /// datagram.
    pub fn is_reliable(&self, channel: ChannelId) -> Result<bool, ApiError> {
        Ok(self.require(channel)?.reliable)
    }

    /// Validate a payload against the channel's negotiated maximum.
    pub fn check_payload(&self, channel: ChannelId, len: usize) -> Result<(), ApiError> {
        let descriptor = self.require(channel)?;
        if len > descriptor.max_payload as usize {
            return Err(payload_too_large(
                channel,
                len,
                descriptor.max_payload as usize,
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_matches_the_protocol_channels() {
        let map = ChannelMap::default_session();
        assert_eq!(map.len(), 8);
        assert_eq!(
            map.require(ChannelId::CONTROL).unwrap().kind,
            ChannelKind::Control
        );
        assert!(map.require(ChannelId::CONTROL).unwrap().reliable);
        assert_eq!(
            map.require(ChannelId::VIDEO).unwrap().track_id,
            Some(TrackId::new("video0"))
        );
        assert!(!map.require(ChannelId::VIDEO).unwrap().reliable);
        assert_eq!(
            map.require(ChannelId::AUDIO).unwrap().priority,
            ChannelPriority::High
        );
        assert!(map.require(ChannelId::TELEMETRY).unwrap().reliable);

        // The browser camera channel is appended after the existing
        // single-track layout: control 0, input 1, video 2, audio 3, mic 4,
        // telemetry 5, bulk 6, camera 7.
        let camera = map
            .channel_for_kind(ChannelKind::Camera)
            .expect("camera channel");
        assert_eq!(camera, ChannelId::new(7));
        let descriptor = map.require(camera).unwrap();
        assert_eq!(descriptor.track_id, Some(TrackId::new("camera0")));
        assert!(!descriptor.reliable);
        assert_eq!(descriptor.max_payload, 1200);
        assert_eq!(map.channels_for_kind(ChannelKind::Camera), vec![camera]);
    }

    #[test]
    fn camera_is_appended_without_shifting_existing_ids() {
        let map = ChannelMap::for_tracks(&[TrackId::new("video0")], false, false).unwrap();
        // control 0, input 1, video 2, telemetry 3, bulk 4, camera 5.
        assert_eq!(
            map.channel_for_kind(ChannelKind::Telemetry),
            Some(ChannelId::new(3))
        );
        assert_eq!(
            map.channel_for_kind(ChannelKind::Bulk),
            Some(ChannelId::new(4))
        );
        assert_eq!(
            map.channel_for_kind(ChannelKind::Camera),
            Some(ChannelId::new(5))
        );
    }

    #[test]
    fn multi_track_layout_allocates_sequential_video_ids() {
        let map = ChannelMap::for_tracks(
            &[TrackId::new("video0"), TrackId::new("video1")],
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            map.require(ChannelId::video_track(0)).unwrap().kind,
            ChannelKind::Video
        );
        assert_eq!(
            map.require(ChannelId::video_track(1)).unwrap().track_id,
            Some(TrackId::new("video1"))
        );
        // No audio/mic channels: telemetry follows the second video channel.
        assert_eq!(
            map.require(ChannelId::video_track(2)).unwrap().kind,
            ChannelKind::Telemetry
        );
    }

    #[test]
    fn duplicate_channel_ids_are_rejected() {
        let descriptor = ChannelDescriptor {
            id: 1,
            kind: ChannelKind::Input,
            priority: ChannelPriority::Realtime,
            reliable: true,
            track_id: None,
            max_payload: 1200,
        };
        let error = ChannelMap::new([descriptor.clone(), descriptor]).unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);
    }

    #[test]
    fn payload_limits_are_enforced() {
        let map = ChannelMap::default_session();
        assert!(map.check_payload(ChannelId::VIDEO, 1200).is_ok());
        let error = map.check_payload(ChannelId::VIDEO, 1201).unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);
        assert!(map.check_payload(ChannelId::new(99), 1).is_err());
    }
}
