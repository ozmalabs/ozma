// SPDX-License-Identifier: AGPL-3.0-only
//! Map a bounded subset of Xpra packets into Ozma scene updates.
//!
//! The native adapter accepts uncompressed rencodeplus main records with
//! inline PNG payloads. A peer connection and negotiation are separate.

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
    fn decoded_xpra_packet_arrays_map_supported_operations() {
        let mut mapper = XpraSceneMapper::default();
        let create = serde_json::json!(["window-create", 7, 20, 30, 2, 2, {}, {}]);
        assert!(decode_packet(&create, &mut mapper, 42).unwrap().is_some());
        let draw = serde_json::json!([
            "window-draw",
            7,
            0,
            0,
            2,
            2,
            "png",
            [137, 80, 78, 71, 13, 10, 26, 10, 1],
            1,
            0,
            {}
        ]);
        let (commit, resource) = decode_packet(&draw, &mut mapper, 42).unwrap().unwrap();
        assert_eq!(commit.images[0].resource_id, 42);
        assert_eq!(&resource[..8], &42u64.to_be_bytes());
        let destroy = serde_json::json!(["window-destroy", 7]);
        assert!(decode_packet(&destroy, &mut mapper, 43).unwrap().is_some());
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

    #[test]
    fn validates_xpra_record_header_and_maps_json_payload() {
        let packet = serde_json::json!(["window-create", 7, 20, 30, 2, 2, {}, {}]);
        let payload = serde_json::to_vec(&packet).unwrap();
        let mut framed = vec![0x50, 0, 0, 0];
        framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        framed.extend_from_slice(&payload);
        assert_eq!(decode_framed_json(&framed).unwrap(), packet);

        let mut mapper = XpraSceneMapper::default();
        assert!(decode_framed_packet(&framed, &mut mapper, 1)
            .unwrap()
            .is_some());
        framed[0] = 0;
        assert_eq!(decode_framed_json(&framed), Err(RecordError::InvalidMagic));
    }

    #[test]
    fn rejects_compressed_or_oversized_records_before_json_decode() {
        let mut compressed = vec![0x50, 0, 1, 0, 0, 0, 0, 0];
        assert_eq!(
            decode_framed_json(&compressed),
            Err(RecordError::UnsupportedCompression)
        );
        compressed[2] = 0;
        compressed[4..8].copy_from_slice(&65_536u32.to_be_bytes());
        assert_eq!(
            decode_framed_json(&compressed),
            Err(RecordError::PayloadTooLarge)
        );
    }
}

#[cfg(test)]
mod rencode_tests {
    use super::*;

    #[test]
    fn decodes_raw_packet_array_and_long_strings_or_bytes() {
        let raw = [
            200, 141, b'w', b'i', b'n', b'd', b'o', b'w', b'-', b'c', b'r', b'e', b'a', b't', b'e',
            7, 20, 30, 2, 2, 102, 102,
        ];
        let value = decode_rencodeplus(&raw).unwrap();
        assert_eq!(value[0], "window-create");
        assert_eq!(value[1], 7);
        assert_eq!(
            decode_rencodeplus(&[b'3', b'/', 1, 2, 3]).unwrap(),
            serde_json::json!([1, 2, 3])
        );
        assert_eq!(
            decode_rencodeplus(&[b'3', b':', b'a', b'b', b'c']).unwrap(),
            serde_json::json!("abc")
        );
    }

    #[test]
    fn maps_inline_png_from_native_records() {
        fn frame(packet: &[u8]) -> Vec<u8> {
            let mut header = vec![b'P', 0x10, 0, 0];
            header.extend_from_slice(&(packet.len() as u32).to_be_bytes());
            header.extend_from_slice(packet);
            header
        }

        let mut mapper = XpraSceneMapper::default();
        let mut create = vec![200, 141];
        create.extend_from_slice(b"window-create");
        create.extend_from_slice(&[7, 20, 30, 2, 2, 102, 102]);
        let (commit, _) = map_rencodeplus_record(&frame(&create), &mut mapper, 99)
            .unwrap()
            .unwrap();
        assert!(commit.images.is_empty());

        let mut draw = vec![200, 139];
        draw.extend_from_slice(b"window-draw");
        draw.extend_from_slice(&[7, 0, 0, 2, 2, 131]);
        draw.extend_from_slice(b"png8/\x89PNG\r\n\x1a\n");
        let (commit, resource) = map_rencodeplus_record(&frame(&draw), &mut mapper, 99)
            .unwrap()
            .unwrap();
        assert_eq!(commit.images[0].resource_id, 99);
        assert_eq!(
            crate::scene::ImageResource::decode(&resource).unwrap().id,
            99
        );
    }

    #[test]
    fn framed_rencodeplus_requires_encoder_flag() {
        let raw = [129, b'p'];
        let mut record = vec![0x50, 0x10, 0, 0];
        record.extend_from_slice(&(raw.len() as u32).to_be_bytes());
        record.extend_from_slice(&raw);
        assert_eq!(
            decode_framed_rencodeplus(&record).unwrap(),
            serde_json::json!("p")
        );
        record[1] = 0;
        assert_eq!(
            decode_framed_rencodeplus(&record),
            Err(RecordError::UnsupportedEncoder)
        );
    }
}

/// Convert one already-decoded Xpra packet array. The caller remains
/// responsible for Xpra record framing, rencodeplus, authentication and
/// negotiation. Only complete-window PNG draws are accepted.
pub fn decode_packet(
    packet: &serde_json::Value,
    mapper: &mut XpraSceneMapper,
    next_resource_id: u64,
) -> Result<Option<(SceneCommit, Vec<u8>)>, PacketError> {
    use serde_json::Value;

    let fields = packet.as_array().ok_or(PacketError::Malformed)?;
    let name = fields
        .first()
        .and_then(Value::as_str)
        .ok_or(PacketError::Malformed)?;
    match name {
        "window-create" | "window-move-resize" => {
            let wid = number(fields, 1)?;
            let x = signed(fields, 2)?;
            let y = signed(fields, 3)?;
            let width = dimension(fields, 4)?;
            let height = dimension(fields, 5)?;
            let commit = mapper.apply(XpraUpdate::Geometry {
                window_id: wid,
                x,
                y,
                width,
                height,
            })?;
            Ok(Some((commit, Vec::new())))
        }
        "window-destroy" => {
            let commit = mapper.apply(XpraUpdate::Remove {
                window_id: number(fields, 1)?,
            })?;
            Ok(Some((commit, Vec::new())))
        }
        "window-draw" => {
            let wid = number(fields, 1)?;
            let x = signed(fields, 2)?;
            let y = signed(fields, 3)?;
            let width = dimension(fields, 4)?;
            let height = dimension(fields, 5)?;
            let encoding = fields
                .get(6)
                .and_then(Value::as_str)
                .ok_or(PacketError::Malformed)?;
            if encoding != "png" || x != 0 || y != 0 {
                return Err(PacketError::UnsupportedDraw);
            }
            let window = mapper.windows.get(&wid).ok_or(UpdateError::UnknownWindow)?;
            if (width, height) != (window.placement.width, window.placement.height) {
                return Err(PacketError::UnsupportedDraw);
            }
            let png = fields
                .get(7)
                .and_then(Value::as_array)
                .ok_or(PacketError::Malformed)?;
            if png.len() < 8 || png[..8] != [137, 80, 78, 71, 13, 10, 26, 10] || png.len() > 65_527
            {
                return Err(PacketError::InvalidPng);
            }
            let mut resource = next_resource_id.to_be_bytes().to_vec();
            resource.extend(
                png.iter()
                    .map(|v| {
                        v.as_u64()
                            .and_then(|n| u8::try_from(n).ok())
                            .ok_or(PacketError::Malformed)
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            );
            let commit = mapper.apply(XpraUpdate::Raster {
                window_id: wid,
                resource_id: next_resource_id,
            })?;
            Ok(Some((commit, resource)))
        }
        _ => Ok(None),
    }
}

/// Xpra record header size. The header is magic, flags, compression level,
/// packet index, and a big-endian payload length.
pub const RECORD_HEADER_LEN: usize = 8;
const RECORD_MAGIC: u8 = 0x50;
const MAX_RECORD_PAYLOAD: usize = 65_535;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordError {
    Truncated,
    InvalidMagic,
    UnsupportedCompression,
    UnsupportedPacketIndex,
    PayloadTooLarge,
    LengthMismatch,
    InvalidJson,
    InvalidRencodeplus,
    UnsupportedEncoder,
}

/// Decode one Xpra-framed record carrying a JSON packet array. This is useful
/// for adapters that have already converted rencodeplus into JSON while still
/// preserving framing validation and size limits at the Ozma boundary.
pub fn decode_framed_json(input: &[u8]) -> Result<serde_json::Value, RecordError> {
    if input.len() < RECORD_HEADER_LEN {
        return Err(RecordError::Truncated);
    }
    if input[0] != RECORD_MAGIC {
        return Err(RecordError::InvalidMagic);
    }
    // Compression is handled by the Xpra decoder before this boundary.
    if input[2] != 0 {
        return Err(RecordError::UnsupportedCompression);
    }
    if input[3] != 0 {
        return Err(RecordError::UnsupportedPacketIndex);
    }
    let length = u32::from_be_bytes(input[4..8].try_into().unwrap()) as usize;
    if length > MAX_RECORD_PAYLOAD {
        return Err(RecordError::PayloadTooLarge);
    }
    if input.len() != RECORD_HEADER_LEN + length {
        return Err(RecordError::LengthMismatch);
    }
    serde_json::from_slice(&input[RECORD_HEADER_LEN..]).map_err(|_| RecordError::InvalidJson)
}

/// Decode a framed JSON record and map its packet in one bounded operation.
pub fn decode_framed_packet(
    input: &[u8],
    mapper: &mut XpraSceneMapper,
    next_resource_id: u64,
) -> Result<Option<(SceneCommit, Vec<u8>)>, FramedPacketError> {
    let packet = decode_framed_json(input).map_err(FramedPacketError::Record)?;
    decode_packet(&packet, mapper, next_resource_id).map_err(FramedPacketError::Packet)
}

/// Decode the modern rencodeplus subset used by Xpra packet arrays. Bytes are
/// represented as JSON arrays of octets so the existing packet mapper can
/// consume PNG draw payloads without a second representation.
pub fn decode_rencodeplus(input: &[u8]) -> Result<serde_json::Value, RecordError> {
    if input.len() > MAX_RECORD_PAYLOAD {
        return Err(RecordError::PayloadTooLarge);
    }
    let mut decoder = RencodeDecoder {
        input,
        offset: 0,
        depth: 0,
    };
    let value = decoder
        .value()
        .map_err(|_| RecordError::InvalidRencodeplus)?;
    if decoder.offset != input.len() {
        return Err(RecordError::LengthMismatch);
    }
    Ok(value)
}

/// Decode a full Xpra record whose main payload is uncompressed rencodeplus.
pub fn decode_framed_rencodeplus(input: &[u8]) -> Result<serde_json::Value, RecordError> {
    if input.len() < RECORD_HEADER_LEN || input[0] != RECORD_MAGIC {
        return Err(if input.len() < RECORD_HEADER_LEN {
            RecordError::Truncated
        } else {
            RecordError::InvalidMagic
        });
    }
    if input[1] & 0x10 == 0 || input[1] & !0x18 != 0 {
        return Err(RecordError::UnsupportedEncoder);
    }
    if input[2] != 0 {
        return Err(RecordError::UnsupportedCompression);
    }
    if input[3] != 0 {
        return Err(RecordError::UnsupportedPacketIndex);
    }
    let length = u32::from_be_bytes(input[4..8].try_into().unwrap()) as usize;
    if length > MAX_RECORD_PAYLOAD {
        return Err(RecordError::PayloadTooLarge);
    }
    if input.len() != RECORD_HEADER_LEN + length {
        return Err(RecordError::LengthMismatch);
    }
    decode_rencodeplus(&input[RECORD_HEADER_LEN..])
}

/// Decode and map one native Xpra record.
pub fn map_rencodeplus_record(
    input: &[u8],
    mapper: &mut XpraSceneMapper,
    next_resource_id: u64,
) -> Result<Option<(SceneCommit, Vec<u8>)>, FramedPacketError> {
    let packet = decode_framed_rencodeplus(input)?;
    Ok(decode_packet(&packet, mapper, next_resource_id)?)
}

struct RencodeDecoder<'a> {
    input: &'a [u8],
    offset: usize,
    depth: usize,
}
impl<'a> RencodeDecoder<'a> {
    fn byte(&mut self) -> Result<u8, ()> {
        let b = *self.input.get(self.offset).ok_or(())?;
        self.offset += 1;
        Ok(b)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], ()> {
        let end = self.offset.checked_add(n).ok_or(())?;
        let out = self.input.get(self.offset..end).ok_or(())?;
        self.offset = end;
        Ok(out)
    }
    fn value(&mut self) -> Result<serde_json::Value, ()> {
        if self.depth > 64 {
            return Err(());
        }
        let tag = self.byte()?;
        if tag.is_ascii_digit() {
            let mut digits = vec![tag];
            loop {
                let b = self.byte()?;
                if b == b':' || b == b'/' {
                    let len: usize = std::str::from_utf8(&digits)
                        .map_err(|_| ())?
                        .parse()
                        .map_err(|_| ())?;
                    let data = self.take(len)?;
                    return if b == b':' {
                        Ok(serde_json::Value::String(
                            String::from_utf8(data.to_vec()).map_err(|_| ())?,
                        ))
                    } else {
                        Ok(data
                            .iter()
                            .copied()
                            .map(serde_json::Value::from)
                            .collect::<Vec<_>>()
                            .into())
                    };
                }
                if !b.is_ascii_digit() || digits.len() > 10 {
                    return Err(());
                }
                digits.push(b);
            }
        }
        match tag {
            0..=43 => Ok(serde_json::Value::from(tag)),
            70..=101 => Ok(serde_json::Value::from(-((tag - 69) as i64))),
            62 => Ok(serde_json::Value::from(self.byte()? as i8 as i64)),
            63 => Ok(serde_json::Value::from(
                i16::from_be_bytes(self.take(2)?.try_into().unwrap()) as i64,
            )),
            64 => Ok(serde_json::Value::from(
                i32::from_be_bytes(self.take(4)?.try_into().unwrap()) as i64,
            )),
            65 => Ok(serde_json::Value::from(i64::from_be_bytes(
                self.take(8)?.try_into().unwrap(),
            ))),
            61 => {
                let mut digits = Vec::new();
                loop {
                    let b = self.byte()?;
                    if b == 127 {
                        break;
                    }
                    digits.push(b);
                    if digits.len() > 63 {
                        return Err(());
                    }
                }
                let text = std::str::from_utf8(&digits).map_err(|_| ())?;
                Ok(serde_json::Value::String(text.to_string()))
            }
            67 => Ok(serde_json::Value::Bool(true)),
            68 => Ok(serde_json::Value::Bool(false)),
            69 => Ok(serde_json::Value::Null),
            128..=191 => self.string((tag - 128) as usize),
            192..=255 => self.list((tag - 192) as usize),
            102..=126 => self.map((tag - 102) as usize),
            59 => self.variable_list(),
            60 => self.variable_map(),
            _ => Err(()),
        }
    }
    fn string(&mut self, len: usize) -> Result<serde_json::Value, ()> {
        Ok(serde_json::Value::String(
            String::from_utf8(self.take(len)?.to_vec()).map_err(|_| ())?,
        ))
    }
    fn list(&mut self, len: usize) -> Result<serde_json::Value, ()> {
        self.depth += 1;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(self.value()?);
        }
        self.depth -= 1;
        Ok(out.into())
    }
    fn map(&mut self, len: usize) -> Result<serde_json::Value, ()> {
        self.depth += 1;
        let mut out = serde_json::Map::new();
        for _ in 0..len {
            let key = self.value()?.as_str().ok_or(())?.to_string();
            out.insert(key, self.value()?);
        }
        self.depth -= 1;
        Ok(out.into())
    }
    fn variable_list(&mut self) -> Result<serde_json::Value, ()> {
        self.depth += 1;
        let mut out = Vec::new();
        while *self.input.get(self.offset).ok_or(())? != 127 {
            if out.len() >= 4096 {
                return Err(());
            }
            out.push(self.value()?);
        }
        self.offset += 1;
        self.depth -= 1;
        Ok(out.into())
    }
    fn variable_map(&mut self) -> Result<serde_json::Value, ()> {
        self.depth += 1;
        let mut out = serde_json::Map::new();
        while *self.input.get(self.offset).ok_or(())? != 127 {
            if out.len() >= 4096 {
                return Err(());
            }
            let key = self.value()?.as_str().ok_or(())?.to_string();
            out.insert(key, self.value()?);
        }
        self.offset += 1;
        self.depth -= 1;
        Ok(out.into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FramedPacketError {
    Record(RecordError),
    Packet(PacketError),
}

impl From<RecordError> for FramedPacketError {
    fn from(error: RecordError) -> Self {
        Self::Record(error)
    }
}

impl From<PacketError> for FramedPacketError {
    fn from(error: PacketError) -> Self {
        Self::Packet(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PacketError {
    Malformed,
    UnsupportedDraw,
    InvalidPng,
    Update(UpdateError),
}

impl From<UpdateError> for PacketError {
    fn from(error: UpdateError) -> Self {
        Self::Update(error)
    }
}
fn number(fields: &[serde_json::Value], index: usize) -> Result<u32, PacketError> {
    fields
        .get(index)
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(PacketError::Malformed)
}
fn signed(fields: &[serde_json::Value], index: usize) -> Result<i32, PacketError> {
    fields
        .get(index)
        .and_then(serde_json::Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or(PacketError::Malformed)
}
fn dimension(fields: &[serde_json::Value], index: usize) -> Result<u32, PacketError> {
    fields
        .get(index)
        .and_then(serde_json::Value::as_u64)
        .filter(|n| *n > 0 && *n <= 16_384)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(PacketError::Malformed)
}
