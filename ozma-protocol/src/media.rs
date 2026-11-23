// SPDX-License-Identifier: AGPL-3.0-only
//! Media vocabulary shared by capture, encode, transport, and clients.

use serde::{Deserialize, Serialize};

/// Codecs the protocol can negotiate. `MJPEG`, `Opus`, `AAC`, and PCM are
/// included because they are useful fallbacks/audio formats, not because every
/// node must support every one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    Mjpeg,
    Opus,
    Aac,
    #[serde(rename = "pcm_s16le")]
    PcmS16Le,
}

impl Codec {
    pub fn is_video(self) -> bool {
        matches!(self, Codec::H264 | Codec::Hevc | Codec::Av1 | Codec::Mjpeg)
    }

    pub fn is_audio(self) -> bool {
        matches!(self, Codec::Opus | Codec::Aac | Codec::PcmS16Le)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
            Codec::Mjpeg => "mjpeg",
            Codec::Opus => "opus",
            Codec::Aac => "aac",
            Codec::PcmS16Le => "pcm_s16le",
        }
    }
}

/// Colour/HDR state of a stream. The protocol uses this instead of a bare
/// `hdr: bool` so that metadata can be negotiated without changing the shape
/// of every message.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorFormat {
    #[default]
    Sdr,
    Hdr10,
    Hdr10Plus,
    DolbyVision,
}

impl ColorFormat {
    pub fn is_hdr(&self) -> bool {
        !matches!(self, ColorFormat::Sdr)
    }
}

/// A video format request or capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFormat {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    #[serde(default)]
    pub color: ColorFormat,
    #[serde(default)]
    pub hdr: bool,
}

impl MediaFormat {
    pub fn new(codec: Codec, width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Self {
        Self {
            codec,
            width,
            height,
            fps,
            bitrate_kbps,
            color: ColorFormat::Sdr,
            hdr: false,
        }
    }

    pub fn label(&self) -> String {
        format!(
            "{}x{}@{} {} {}kbps",
            self.width,
            self.height,
            self.fps,
            self.codec.as_str(),
            self.bitrate_kbps
        )
    }
}

/// An audio format request or capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFormat {
    pub codec: Codec,
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate_kbps: u32,
}

impl AudioFormat {
    pub fn opus() -> Self {
        Self {
            codec: Codec::Opus,
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 128,
        }
    }

    pub fn aac() -> Self {
        Self {
            codec: Codec::Aac,
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 128,
        }
    }
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self::opus()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_names_are_stable() {
        assert_eq!(serde_json::to_string(&Codec::H264).unwrap(), "\"h264\"");
        assert_eq!(
            serde_json::to_string(&Codec::PcmS16Le).unwrap(),
            "\"pcm_s16le\""
        );
    }

    #[test]
    fn media_format_roundtrips() {
        let format = MediaFormat::new(Codec::Hevc, 2560, 1440, 120, 20_000);
        let json = serde_json::to_string(&format).unwrap();
        let back: MediaFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(back, format);
    }
}
