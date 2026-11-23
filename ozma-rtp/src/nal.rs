// SPDX-License-Identifier: AGPL-3.0-only
//! H.264 NAL unit helpers and Annex-B parsing.
//!
//! The media pipeline in `ozma-encode` emits Annex-B access units; RTP carries
//! the same NAL units without start codes. These helpers are deliberately
//! small and allocation-light: the packetizer borrows NAL slices from the
//! input, while the depacketizer rebuilds an Annex-B buffer.

use crate::RtpError;

/// NAL unit type for a sequence parameter set.
pub const NAL_TYPE_SPS: u8 = 7;
/// NAL unit type for a picture parameter set.
pub const NAL_TYPE_PPS: u8 = 8;
/// NAL unit type for a coded slice of an IDR picture.
pub const NAL_TYPE_IDR: u8 = 5;
/// NAL unit type for an access unit delimiter.
pub const NAL_TYPE_AUD: u8 = 9;
/// NAL unit type for a supplemental enhancement information message.
pub const NAL_TYPE_SEI: u8 = 6;

/// RTP payload type for a single NAL unit (RFC 6184 §5.6).
pub const RTP_SINGLE_NAL_MAX: u8 = 23;
/// RTP payload type for STAP-A aggregation (RFC 6184 §5.7.1).
pub const RTP_STAP_A: u8 = 24;
/// RTP payload type for FU-A fragmentation (RFC 6184 §5.8).
pub const RTP_FU_A: u8 = 28;

/// Annex-B start code used by the encoder.
pub const ANNEX_B_START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Return the NAL unit type (low five bits of the first byte).
///
/// Returns `None` for an empty NAL unit.
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|byte| byte & 0x1f)
}

/// Whether a NAL unit is an IDR slice.
pub fn is_idr(nal: &[u8]) -> bool {
    nal_type(nal) == Some(NAL_TYPE_IDR)
}

/// Whether a NAL unit is a sequence or picture parameter set.
pub fn is_parameter_set(nal: &[u8]) -> bool {
    matches!(nal_type(nal), Some(NAL_TYPE_SPS) | Some(NAL_TYPE_PPS))
}

/// Whether a NAL unit is a parameter set or an access-unit delimiter.
pub fn is_metadata_nal(nal: &[u8]) -> bool {
    matches!(
        nal_type(nal),
        Some(NAL_TYPE_SPS) | Some(NAL_TYPE_PPS) | Some(NAL_TYPE_AUD)
    )
}

/// Split an Annex-B byte stream into NAL units without start codes.
///
/// Both three-byte (`00 00 01`) and four-byte (`00 00 00 01`) start codes are
/// accepted, including mixed streams. Leading zero bytes before the first
/// start code are ignored, and zero padding between a NAL unit and the next
/// start code is trimmed.
///
/// The returned slices borrow from `data`.
pub fn split_annex_b(data: &[u8]) -> Result<Vec<&[u8]>, RtpError> {
    let mut nals = Vec::new();
    let mut position = 0usize;
    let mut nal_start: Option<usize> = None;
    let mut saw_start_code = false;

    while position + 3 <= data.len() {
        if data[position] == 0 && data[position + 1] == 0 && data[position + 2] == 1 {
            saw_start_code = true;
            if let Some(start) = nal_start {
                let mut end = position;
                // Absorb the leading zero byte of a four-byte start code.
                while end > start && data[end - 1] == 0 {
                    end -= 1;
                }
                if end > start {
                    nals.push(&data[start..end]);
                }
            }
            position += 3;
            nal_start = Some(position);
        } else {
            position += 1;
        }
    }

    if !saw_start_code {
        return Err(RtpError::NotAnnexB);
    }

    if let Some(start) = nal_start {
        if start < data.len() {
            nals.push(&data[start..]);
        }
    }

    if nals.is_empty() {
        return Err(RtpError::EmptyAccessUnit);
    }
    Ok(nals)
}

/// Append a NAL unit to an Annex-B buffer.
pub fn push_annex_b(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&ANNEX_B_START_CODE);
    out.extend_from_slice(nal);
}

/// Whether an Annex-B access unit contains an IDR slice.
///
/// A stream without start codes yields `false`; callers that need strict
/// validation should use [`split_annex_b`].
pub fn annex_b_is_keyframe(data: &[u8]) -> bool {
    split_annex_b(data)
        .map(|nals| nals.iter().any(|nal| is_idr(nal)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_mixed_start_codes_and_trims_padding() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 3, 0, 0, 0, 0, 1, 0x65, 4,
        ];
        let nals = split_annex_b(&data).unwrap();
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x67, 1]);
        assert_eq!(nals[1], &[0x68, 2, 3]);
        assert_eq!(nals[2], &[0x65, 4]);
    }

    #[test]
    fn rejects_input_without_start_code() {
        assert_eq!(split_annex_b(b"not h264"), Err(RtpError::NotAnnexB));
    }

    #[test]
    fn finds_idr_only_when_present() {
        let key = [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x65, 2];
        let delta = [0, 0, 0, 1, 0x41, 2];
        assert!(annex_b_is_keyframe(&key));
        assert!(!annex_b_is_keyframe(&delta));
        assert!(!annex_b_is_keyframe(b"garbage"));
    }
}
