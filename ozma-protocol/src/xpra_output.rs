// SPDX-License-Identifier: AGPL-3.0-only
//! Encode scene image placements and encoded video frames as bounded outbound
//! Xpra records.
//!
//! Images are whole-window inline PNG draws. Video is a standard Xpra
//! `window-draw` whose encoding names a compressed codec, plus the video-only
//! options Xpra defines (`frame`, `type`, `csc`). The adapter forwards encoded
//! frames unchanged:
//!
//! - `h264` is one Annex B access unit, including start codes.
//! - `vp8` and `vp9` are one compressed frame without a container.
//! - `jpeg` is a complete JFIF file, sent as a plain picture.
//!
//! H.264, VP8 and VP9 payloads are assumed to be 8-bit YUV 4:2:0 in the studio
//! range (`YUV420P`). The adapter does not transcode; a client that did not
//! advertise the matching codec and `full_csc_modes` entry receives no video.

use std::collections::BTreeMap;

use serde_json::Value as JsonValue;

use crate::scene::{ImagePlacement, SceneCommit};

const RECORD_MAGIC: u8 = b'P';
const RENCODEPLUS: u8 = 0x10;
const MAX_RECORD: usize = 65_535;
/// Xpra permits larger records once capabilities have been exchanged. Video
/// access units commonly exceed the 64 KiB picture-record bound.
const MAX_VIDEO_RECORD: usize = 16 * 1024 * 1024;
const MAX_WINDOWS: usize = 256;
const MAX_DIMENSION: u32 = 16_384;

/// Window identifier the development adapter reserves for a full-display
/// video stream. Image placements may not reuse it.
pub const FULL_DISPLAY_WINDOW_ID: u32 = u32::MAX;

/// Compressed picture encodings this adapter can forward, in preference order.
pub const VIDEO_CODECS: [VideoCodec; 4] = [
    VideoCodec::H264,
    VideoCodec::Vp8,
    VideoCodec::Vp9,
    VideoCodec::Jpeg,
];

/// Every picture encoding the server advertises in `encoding-set`.
pub const SERVER_ENCODINGS: [&str; 5] = ["h264", "vp8", "vp9", "jpeg", "png"];

/// Exact Xpra encoding name for a compressed payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Vp8,
    Vp9,
    Jpeg,
}

impl VideoCodec {
    pub fn xpra_name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Vp8 => "vp8",
            Self::Vp9 => "vp9",
            Self::Jpeg => "jpeg",
        }
    }

    pub fn from_xpra_name(name: &str) -> Option<Self> {
        VIDEO_CODECS
            .into_iter()
            .find(|codec| codec.xpra_name() == name)
    }

    /// Whether the payload is a video stream access unit that carries the
    /// `frame`/`type`/`csc` options. JPEG is a complete picture.
    pub fn is_stream(self) -> bool {
        !matches!(self, Self::Jpeg)
    }
}

/// What one client hello permits this viewer to receive. Unknown hello fields
/// are ignored. `png: true` is the baseline for scene image placements; video
/// is opt-in and requires the codec to appear in `encoding.core`/`options` and
/// in `encoding.full_csc_modes` with `YUV420P`. `video` is ordered by
/// preference: H.264 first, then VP8, VP9, and JPEG.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XpraClientEncodings {
    pub png: bool,
    pub video: Vec<VideoCodec>,
}

impl Default for XpraClientEncodings {
    fn default() -> Self {
        Self {
            png: true,
            video: Vec::new(),
        }
    }
}

impl XpraClientEncodings {
    /// Parse the capabilities map of a client `hello`. Both nested
    /// `encoding.core` and flat `encoding.core` keys are accepted, as are
    /// unknown capabilities.
    pub fn from_hello(hello: &JsonValue) -> Self {
        let caps = if hello.get(0).and_then(JsonValue::as_str) == Some("hello") {
            hello.get(1).unwrap_or(hello)
        } else {
            hello
        };
        let png =
            advertised(caps, "encoding.core", "png") || advertised(caps, "encoding.options", "png");
        let mut video = Vec::new();
        for codec in [VideoCodec::H264, VideoCodec::Vp8, VideoCodec::Vp9] {
            if (advertised(caps, "encoding.core", codec.xpra_name())
                || advertised(caps, "encoding.options", codec.xpra_name()))
                && supports_csc(caps, codec)
            {
                video.push(codec);
            }
        }
        if advertised(caps, "encoding.core", "jpeg") || advertised(caps, "encoding.options", "jpeg")
        {
            video.push(VideoCodec::Jpeg);
        }
        Self { png, video }
    }

    pub fn supports_video(&self, codec: VideoCodec) -> bool {
        self.video.contains(&codec)
    }

    /// The first advertised codec, preferred H.264.
    pub fn preferred_video(&self) -> Option<VideoCodec> {
        self.video.first().copied()
    }
}

/// One encoded video frame ready for the Xpra draw options. The draw sequence
/// and stream frame index are allocated by [`XpraOutputEncoder`].
///
/// [`XpraOutputEncoder::encode_video`] maps it to
/// `["window-draw", node_id, 0, 0, width, height, codec, data, sequence, 0,
/// options]`. Streaming codecs carry `{"frame": index, "type": "IDR"|"I"|"P",
/// "csc": "YUV420P"}`; JPEG is a complete picture with empty options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XpraVideoFrame {
    pub node_id: u32,
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// One framed outbound record and, for draws, the acknowledgement identity the
/// caller must wait for before sending more data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedRecord {
    pub bytes: Vec<u8>,
    pub draw: Option<DrawAck>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrawAck {
    pub window_id: u32,
    pub sequence: i64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OutputError {
    #[error("scene resource {0} is missing")]
    MissingResource(u64),
    #[error("scene resource {0} does not have a valid PNG header")]
    InvalidResource(u64),
    #[error("the client did not advertise PNG")]
    PngUnsupported,
    #[error("scene revision must increase")]
    StaleRevision,
    #[error("Xpra record exceeds the configured payload limit")]
    RecordTooLarge,
    #[error("scene has duplicate window IDs or unsupported geometry")]
    InvalidPlacement,
    #[error("Xpra draw sequence exhausted")]
    SequenceExhausted,
    #[error("the client did not advertise the {0} video encoding")]
    VideoUnsupported(&'static str),
    #[error("an Xpra video frame must start with a keyframe")]
    VideoKeyframeRequired,
    #[error("Xpra video frame has invalid dimensions or an empty payload")]
    InvalidVideoFrame,
}

/// Per-viewer state for full-window PNG and video draws. Callers supply
/// immutable resource bytes from the session cache and deliver returned
/// records in order.
#[derive(Default)]
pub struct XpraOutputEncoder {
    revision: u64,
    draw_sequence: i64,
    windows: BTreeMap<u32, ImagePlacement>,
    video_windows: BTreeMap<u32, VideoWindow>,
    encodings: XpraClientEncodings,
}

#[derive(Clone, Copy)]
struct VideoWindow {
    width: u32,
    height: u32,
    next_frame: i64,
}

impl XpraOutputEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a per-viewer encoder from the encodings parsed out of the
    /// client's hello.
    pub fn with_encodings(encodings: XpraClientEncodings) -> Self {
        Self {
            encodings,
            ..Self::default()
        }
    }

    pub fn set_encodings(&mut self, encodings: XpraClientEncodings) {
        self.encodings = encodings;
    }

    pub fn encodings(&self) -> &XpraClientEncodings {
        &self.encodings
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn encode_commit(
        &mut self,
        commit: &SceneCommit,
        resources: &BTreeMap<u64, Vec<u8>>,
    ) -> Result<Vec<EncodedRecord>, OutputError> {
        if commit.revision <= self.revision {
            return Err(OutputError::StaleRevision);
        }
        if !self.encodings.png && !commit.images.is_empty() {
            return Err(OutputError::PngUnsupported);
        }
        if commit.images.len() > MAX_WINDOWS {
            return Err(OutputError::InvalidPlacement);
        }

        let mut next = BTreeMap::new();
        let mut records = Vec::new();
        let mut sequence = self.draw_sequence;
        for image in &commit.images {
            if !(1..=MAX_DIMENSION).contains(&image.width)
                || !(1..=MAX_DIMENSION).contains(&image.height)
                || image.node_id == 0
                || image.node_id == FULL_DISPLAY_WINDOW_ID
                || self.video_windows.contains_key(&image.node_id)
                || next.insert(image.node_id, image.clone()).is_some()
            {
                return Err(OutputError::InvalidPlacement);
            }
            let old = self.windows.get(&image.node_id);
            let wid = Value::Int(i64::from(image.node_id));
            let x = Value::Int(i64::from(image.x));
            let y = Value::Int(i64::from(image.y));
            let width = Value::Int(i64::from(image.width));
            let height = Value::Int(i64::from(image.height));
            if old.is_none() {
                records.push(plain(record_bytes(&[
                    Value::Text("window-create"),
                    wid,
                    x,
                    y,
                    width,
                    height,
                    Value::Map(&[]),
                    Value::Map(&[]),
                ])?));
            } else if old.is_some_and(|old| {
                (old.x, old.y, old.width, old.height)
                    != (image.x, image.y, image.width, image.height)
            }) {
                let counter =
                    i64::try_from(commit.revision).map_err(|_| OutputError::SequenceExhausted)?;
                records.push(plain(record_bytes(&[
                    Value::Text("window-move-resize"),
                    wid,
                    x,
                    y,
                    width,
                    height,
                    Value::Int(counter),
                ])?));
            }
            if old.is_none_or(|old| {
                (old.resource_id, old.width, old.height)
                    != (image.resource_id, image.width, image.height)
            }) {
                let png = resources
                    .get(&image.resource_id)
                    .ok_or(OutputError::MissingResource(image.resource_id))?;
                if png.len() > MAX_RECORD {
                    return Err(OutputError::RecordTooLarge);
                }
                let (png_width, png_height) =
                    png_dimensions(png).ok_or(OutputError::InvalidResource(image.resource_id))?;
                let size = [
                    Value::Int(i64::from(png_width)),
                    Value::Int(i64::from(png_height)),
                ];
                let scale = [("scaled_size", Value::List(&size))];
                let options = if (png_width, png_height) == (image.width, image.height) {
                    Value::Map(&[])
                } else {
                    Value::Map(&scale)
                };
                sequence = sequence
                    .checked_add(1)
                    .ok_or(OutputError::SequenceExhausted)?;
                records.push(EncodedRecord {
                    bytes: record_bytes(&[
                        Value::Text("window-draw"),
                        wid,
                        Value::Int(0),
                        Value::Int(0),
                        width,
                        height,
                        Value::Text("png"),
                        Value::Bytes(png),
                        Value::Int(sequence),
                        Value::Int(0),
                        options,
                    ])?,
                    draw: Some(DrawAck {
                        window_id: image.node_id,
                        sequence,
                    }),
                });
            }
        }
        for old in self.windows.keys().filter(|id| !next.contains_key(id)) {
            records.push(plain(record_bytes(&[
                Value::Text("window-destroy"),
                Value::Int(i64::from(*old)),
            ])?));
        }
        self.windows = next;
        self.draw_sequence = sequence;
        self.revision = commit.revision;
        Ok(records)
    }

    /// Encode one already-compressed frame as a `window-draw` for this viewer.
    /// A new window is created on first use, and a change of encoded dimensions
    /// emits `window-move-resize` and restarts the stream. Only a codec the
    /// client advertised is accepted.
    pub fn encode_video(
        &mut self,
        frame: &XpraVideoFrame,
    ) -> Result<Vec<EncodedRecord>, OutputError> {
        if !self.encodings.supports_video(frame.codec) {
            return Err(OutputError::VideoUnsupported(frame.codec.xpra_name()));
        }
        if !(1..=MAX_DIMENSION).contains(&frame.width)
            || !(1..=MAX_DIMENSION).contains(&frame.height)
            || frame.data.is_empty()
        {
            return Err(OutputError::InvalidVideoFrame);
        }
        if self.windows.contains_key(&frame.node_id) || frame.node_id == 0 {
            return Err(OutputError::InvalidPlacement);
        }

        let wid = Value::Int(i64::from(frame.node_id));
        let x = Value::Int(0);
        let y = Value::Int(0);
        let width = Value::Int(i64::from(frame.width));
        let height = Value::Int(i64::from(frame.height));
        let old = self.video_windows.get(&frame.node_id).copied();
        let stream = frame.codec.is_stream();
        let restart = old.is_none_or(|old| (old.width, old.height) != (frame.width, frame.height));
        if stream && restart && !frame.keyframe {
            return Err(OutputError::VideoKeyframeRequired);
        }
        let frame_index = if !stream {
            0
        } else if restart || frame.keyframe {
            0
        } else {
            old.map_or(0, |old| old.next_frame)
        };
        if stream && frame_index == 0 && !frame.keyframe {
            return Err(OutputError::VideoKeyframeRequired);
        }

        let mut records = Vec::new();
        if old.is_none() {
            records.push(plain(record_bytes(&[
                Value::Text("window-create"),
                wid,
                x,
                y,
                width,
                height,
                Value::Map(&[]),
                Value::Map(&[]),
            ])?));
        } else if restart {
            records.push(plain(record_bytes(&[
                Value::Text("window-move-resize"),
                wid,
                x,
                y,
                width,
                height,
                Value::Int(0),
            ])?));
        }

        let sequence = self
            .draw_sequence
            .checked_add(1)
            .ok_or(OutputError::SequenceExhausted)?;
        let frame_number = Value::Int(frame_index);
        let frame_type = Value::Text(if frame.keyframe || frame_index == 0 {
            if frame.codec == VideoCodec::H264 {
                "IDR"
            } else {
                "I"
            }
        } else {
            "P"
        });
        let csc = Value::Text("YUV420P");
        let video_options = [("frame", frame_number), ("type", frame_type), ("csc", csc)];
        let options = if frame.codec.is_stream() {
            Value::Map(&video_options)
        } else {
            Value::Map(&[])
        };
        let bytes = frame_with_limit(
            &[
                Value::Text("window-draw"),
                wid,
                x,
                y,
                width,
                height,
                Value::Text(frame.codec.xpra_name()),
                Value::Bytes(&frame.data),
                Value::Int(sequence),
                Value::Int(0),
                options,
            ],
            MAX_VIDEO_RECORD,
        )?;
        records.push(EncodedRecord {
            bytes,
            draw: Some(DrawAck {
                window_id: frame.node_id,
                sequence,
            }),
        });

        let next_frame = frame_index
            .checked_add(1)
            .ok_or(OutputError::SequenceExhausted)?;
        self.draw_sequence = sequence;
        self.video_windows.insert(
            frame.node_id,
            VideoWindow {
                width: frame.width,
                height: frame.height,
                next_frame,
            },
        );
        Ok(records)
    }
}

fn plain(bytes: Vec<u8>) -> EncodedRecord {
    EncodedRecord { bytes, draw: None }
}

fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if !png.starts_with(b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR") {
        return None;
    }
    let width = u32::from_be_bytes(png.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(png.get(20..24)?.try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

/// Look up a dotted capability name, accepting either the literal flat key or
/// the nested form. Unknown or malformed values are treated as absent.
fn capability<'a>(caps: &'a JsonValue, path: &str) -> Option<&'a JsonValue> {
    if let Some(value) = caps.get(path) {
        return Some(value);
    }
    let (head, tail) = path.split_once('.')?;
    capability(caps.get(head)?, tail)
}

fn advertised(caps: &JsonValue, path: &str, name: &str) -> bool {
    capability(caps, path).is_some_and(|value| list_contains(value, name))
}

fn list_contains(value: &JsonValue, name: &str) -> bool {
    match value {
        JsonValue::Array(items) => items.iter().any(|item| item.as_str() == Some(name)),
        JsonValue::Object(map) => map.get("").is_some_and(|value| list_contains(value, name)),
        _ => false,
    }
}

fn supports_csc(caps: &JsonValue, codec: VideoCodec) -> bool {
    capability(caps, "encoding.full_csc_modes")
        .and_then(|modes| modes.get(codec.xpra_name()))
        .is_some_and(|formats| list_contains(formats, "YUV420P"))
}

#[derive(Clone, Copy)]
enum Value<'a> {
    Int(i64),
    Text(&'a str),
    Bytes(&'a [u8]),
    Map(&'a [(&'a str, Value<'a>)]),
    List(&'a [Value<'a>]),
}

fn record_bytes(values: &[Value<'_>]) -> Result<Vec<u8>, OutputError> {
    frame_with_limit(values, MAX_RECORD)
}

fn frame_with_limit(values: &[Value<'_>], limit: usize) -> Result<Vec<u8>, OutputError> {
    let mut out = vec![RECORD_MAGIC, RENCODEPLUS, 0, 0, 0, 0, 0, 0];
    encode(Value::List(values), &mut out);
    let len = out.len() - 8;
    if len > limit {
        return Err(OutputError::RecordTooLarge);
    }
    out[4..8].copy_from_slice(&(len as u32).to_be_bytes());
    Ok(out)
}

fn encode(value: Value<'_>, out: &mut Vec<u8>) {
    match value {
        Value::Int(value) if (0..=43).contains(&value) => out.push(value as u8),
        Value::Int(value) if (-32..=-1).contains(&value) => out.push((69 - value) as u8),
        Value::Int(value) => {
            out.push(65);
            out.extend_from_slice(&value.to_be_bytes());
        }
        Value::Text(value) => sized(value.as_bytes(), b':', out),
        Value::Bytes(value) => sized(value, b'/', out),
        Value::Map(entries) => {
            out.push(102 + entries.len() as u8);
            for &(key, value) in entries {
                encode(Value::Text(key), out);
                encode(value, out);
            }
        }
        Value::List(items) => {
            out.push(192 + items.len() as u8);
            for &item in items {
                encode(item, out);
            }
        }
    }
}

fn sized(data: &[u8], marker: u8, out: &mut Vec<u8>) {
    out.extend_from_slice(data.len().to_string().as_bytes());
    out.push(marker);
    out.extend_from_slice(data);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{ImagePlacement, SceneCommit};

    #[test]
    fn emits_create_and_draw_then_only_move_for_placement_change() {
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR\0\0\0\x01\0\0\0\x01";
        let resources = BTreeMap::from([(7, png.to_vec())]);
        let mut encoder = XpraOutputEncoder::new();
        let commit = |revision, x| SceneCommit {
            revision,
            images: vec![ImagePlacement {
                node_id: 3,
                resource_id: 7,
                x,
                y: 4,
                width: 10,
                height: 8,
            }],
        };
        let first = encoder.encode_commit(&commit(1, 2), &resources).unwrap();
        assert_eq!(first.len(), 2);
        let second = encoder.encode_commit(&commit(2, 9), &resources).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].bytes[0], RECORD_MAGIC);
    }

    #[test]
    fn records_round_trip_through_the_inbound_decoder() {
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR\0\0\0\x01\0\0\0\x01";
        let resources = BTreeMap::from([(7, png.to_vec())]);
        let mut encoder = XpraOutputEncoder::new();
        let records = encoder
            .encode_commit(
                &SceneCommit {
                    revision: 1,
                    images: vec![ImagePlacement {
                        node_id: 3,
                        resource_id: 7,
                        x: 2,
                        y: 4,
                        width: 10,
                        height: 8,
                    }],
                },
                &resources,
            )
            .unwrap();
        let mut mapper = crate::xpra::XpraSceneMapper::default();
        let mut saw_draw = false;
        for record in records {
            let packet = crate::xpra::decode_framed_rencodeplus(&record.bytes).unwrap();
            let name = packet[0].as_str().unwrap();
            if name == "window-draw" {
                assert_eq!(packet[8], 1);
                assert_eq!(packet[9], 0);
                assert_eq!(packet[10]["scaled_size"], serde_json::json!([1, 1]));
                saw_draw = true;
            }
            crate::xpra::decode_packet(&packet, &mut mapper, 7).unwrap();
        }
        assert!(saw_draw);
    }

    #[test]
    fn invalid_revision_does_not_change_state() {
        let mut encoder = XpraOutputEncoder::new();
        let missing = SceneCommit {
            revision: 1,
            images: vec![ImagePlacement {
                node_id: 1,
                resource_id: 44,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
        };
        assert_eq!(
            encoder.encode_commit(&missing, &BTreeMap::new()),
            Err(OutputError::MissingResource(44))
        );
        assert_eq!(encoder.revision, 0);
    }

    #[test]
    fn parses_client_video_capabilities_and_ignores_unknown_fields() {
        let hello = serde_json::json!([
            "hello",
            {
                "version": "1.2.3",
                "uuid": "1b4e28ba2fa1411d8f2c8b6b0e2a2f3c",
                "encoding": {
                    "options": ["stream", "vp8", "h264", "png"],
                    "core": ["h264", "png"],
                    "full_csc_modes": {"h264": ["YUV420P", "NV12"]}
                },
                "window": {"enabled": true},
                "unknown-future-field": {"nested": [1, 2, 3]}
            }
        ]);
        let encodings = XpraClientEncodings::from_hello(&hello);
        assert!(encodings.png);
        assert_eq!(encodings.video, vec![VideoCodec::H264]);
    }

    #[test]
    fn accepts_flat_capability_keys_and_empty_map_key() {
        let hello = serde_json::json!({
            "encoding.core": {"": ["vp9", "png"]},
            "encoding.full_csc_modes": {"vp9": {"": ["YUV420P"]}}
        });
        let encodings = XpraClientEncodings::from_hello(&hello);
        assert!(encodings.png);
        assert_eq!(encodings.preferred_video(), Some(VideoCodec::Vp9));
    }

    #[test]
    fn video_requires_csc_support_or_falls_back_to_jpeg() {
        let no_csc = serde_json::json!({
            "encoding": {"core": ["h264", "png"]}
        });
        let encodings = XpraClientEncodings::from_hello(&no_csc);
        assert!(encodings.png);
        assert!(encodings.video.is_empty());

        let jpeg = serde_json::json!({
            "encoding": {"core": ["jpeg", "png"]}
        });
        assert_eq!(
            XpraClientEncodings::from_hello(&jpeg).preferred_video(),
            Some(VideoCodec::Jpeg)
        );
    }

    #[test]
    fn encodes_video_draw_with_xpra_video_options() {
        let hello = serde_json::json!({
            "encoding": {
                "options": ["stream", "h264", "png"],
                "core": ["h264", "png"],
                "full_csc_modes": {"h264": ["YUV420P"]}
            }
        });
        let mut encoder =
            XpraOutputEncoder::with_encodings(XpraClientEncodings::from_hello(&hello));
        let payload = [0, 0, 0, 1, 0x67, 0x42];
        let records = encoder
            .encode_video(&XpraVideoFrame {
                node_id: FULL_DISPLAY_WINDOW_ID,
                codec: VideoCodec::H264,
                width: 64,
                height: 48,
                keyframe: true,
                data: payload.to_vec(),
            })
            .unwrap();
        assert_eq!(records.len(), 2);
        let create = crate::xpra::decode_framed_rencodeplus(&records[0].bytes).unwrap();
        assert_eq!(create[0], "window-create");
        assert_eq!(create[1], u32::MAX);
        assert!(records[0].draw.is_none());

        let draw = crate::xpra::decode_framed_rencodeplus(&records[1].bytes).unwrap();
        assert_eq!(draw[0], "window-draw");
        assert_eq!(draw[1], u32::MAX);
        assert_eq!((draw[4].as_u64(), draw[5].as_u64()), (Some(64), Some(48)));
        assert_eq!(draw[6], "h264");
        assert_eq!(draw[7], serde_json::json!(payload));
        assert_eq!(draw[8], 1);
        assert_eq!(draw[10]["frame"], 0);
        assert_eq!(draw[10]["type"], "IDR");
        assert_eq!(draw[10]["csc"], "YUV420P");
        assert_eq!(
            records[1].draw,
            Some(DrawAck {
                window_id: FULL_DISPLAY_WINDOW_ID,
                sequence: 1
            })
        );
    }

    #[test]
    fn video_frame_indices_advance_and_restart_on_keyframe() {
        let hello = serde_json::json!({
            "encoding": {
                "core": ["h264", "png"],
                "full_csc_modes": {"h264": ["YUV420P"]}
            }
        });
        let mut encoder =
            XpraOutputEncoder::with_encodings(XpraClientEncodings::from_hello(&hello));
        let frame = |keyframe| XpraVideoFrame {
            node_id: 9,
            codec: VideoCodec::H264,
            width: 16,
            height: 16,
            keyframe,
            data: vec![0, 0, 1],
        };
        encoder.encode_video(&frame(true)).unwrap();
        let second = encoder.encode_video(&frame(false)).unwrap();
        assert_eq!(second.len(), 1);
        let draw = crate::xpra::decode_framed_rencodeplus(&second[0].bytes).unwrap();
        assert_eq!(draw[10]["frame"], 1);
        assert_eq!(draw[10]["type"], "P");
        let restart = encoder.encode_video(&frame(true)).unwrap();
        let draw = crate::xpra::decode_framed_rencodeplus(&restart[0].bytes).unwrap();
        assert_eq!(draw[10]["frame"], 0);
        assert_eq!(draw[10]["type"], "IDR");
    }

    #[test]
    fn video_is_rejected_without_client_support() {
        let mut encoder = XpraOutputEncoder::new();
        let frame = XpraVideoFrame {
            node_id: 9,
            codec: VideoCodec::H264,
            width: 16,
            height: 16,
            keyframe: true,
            data: vec![0, 0, 1],
        };
        assert_eq!(
            encoder.encode_video(&frame),
            Err(OutputError::VideoUnsupported("h264"))
        );

        let mut png_only = XpraOutputEncoder::with_encodings(XpraClientEncodings {
            png: true,
            video: Vec::new(),
        });
        assert_eq!(
            png_only.encode_video(&frame),
            Err(OutputError::VideoUnsupported("h264"))
        );
    }

    #[test]
    fn falls_back_to_a_lower_video_codec_with_csc_support() {
        let hello = serde_json::json!({
            "encoding": {
                "core": ["h264", "vp8", "png"],
                "full_csc_modes": {"vp8": ["YUV420P"]}
            }
        });
        let encodings = XpraClientEncodings::from_hello(&hello);
        assert_eq!(encodings.video, vec![VideoCodec::Vp8]);
    }

    #[test]
    fn jpeg_picture_does_not_start_a_video_stream() {
        let hello = serde_json::json!({
            "encoding": {"core": ["jpeg", "png"]}
        });
        let mut encoder =
            XpraOutputEncoder::with_encodings(XpraClientEncodings::from_hello(&hello));
        let records = encoder
            .encode_video(&XpraVideoFrame {
                node_id: 9,
                codec: VideoCodec::Jpeg,
                width: 16,
                height: 16,
                keyframe: false,
                data: vec![0xff, 0xd8, 0xff, 0xd9],
            })
            .unwrap();
        let draw = crate::xpra::decode_framed_rencodeplus(&records[1].bytes).unwrap();
        assert_eq!(draw[6], "jpeg");
        assert_eq!(draw[10], serde_json::json!({}));
    }

    #[test]
    fn video_requires_a_keyframe_to_start_a_stream() {
        let hello = serde_json::json!({
            "encoding": {
                "core": ["h264", "png"],
                "full_csc_modes": {"h264": ["YUV420P"]}
            }
        });
        let mut encoder =
            XpraOutputEncoder::with_encodings(XpraClientEncodings::from_hello(&hello));
        let result = encoder.encode_video(&XpraVideoFrame {
            node_id: 9,
            codec: VideoCodec::H264,
            width: 16,
            height: 16,
            keyframe: false,
            data: vec![0, 0, 1],
        });
        assert_eq!(result, Err(OutputError::VideoKeyframeRequired));
    }

    #[test]
    fn png_only_client_rejects_image_commit() {
        let mut encoder = XpraOutputEncoder::with_encodings(XpraClientEncodings {
            png: false,
            video: vec![VideoCodec::H264],
        });
        let commit = SceneCommit {
            revision: 1,
            images: vec![ImagePlacement {
                node_id: 1,
                resource_id: 1,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
        };
        assert_eq!(
            encoder.encode_commit(&commit, &BTreeMap::new()),
            Err(OutputError::PngUnsupported)
        );
    }
}
