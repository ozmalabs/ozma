// SPDX-License-Identifier: AGPL-3.0-only
//! UAC2 audio-function descriptor and capability model.
//!
//! Linux's `usb_f_uac2` function takes a small set of ConfigFS attributes
//! (`p_chmask`, `c_chmask`, `p_srate`, `c_srate`, `p_ssize`, `c_ssize`,
//! `req_number`). This module keeps a richer capability model (multiple
//! advertised rates/sizes, microphone/headset/speaker modes, optional volume
//! and mute), generates the corresponding UAC2 descriptor bytes, and projects
//! the primary rate/size onto the ConfigFS attributes the kernel accepts.
//!
//! No ALSA device is opened, no isochronous endpoint is serviced, and no
//! audio is bridged. [`Uac2Status`] always reports the bridge as
//! [`Uac2BridgeStatus::Unsupported`].

use serde::{Deserialize, Serialize};

use crate::error::UsbError;

/// Lowest sample rate accepted by the model.
pub const UAC2_MIN_SAMPLE_RATE: u32 = 8_000;
/// Highest sample rate accepted by the model.
pub const UAC2_MAX_SAMPLE_RATE: u32 = 192_000;
/// Maximum channels per direction.
pub const UAC2_MAX_CHANNELS: u8 = 8;
/// Maximum number of advertised sample rates per direction.
pub const UAC2_MAX_SAMPLE_RATES: usize = 16;
/// Maximum number of advertised sample sizes per direction.
pub const UAC2_MAX_SAMPLE_SIZES: usize = 3;
/// Maximum generated descriptor size in bytes.
pub const UAC2_MAX_DESCRIPTOR_BYTES: usize = 65_535;

/// UAC2 endpoint addresses used by the generated descriptors.
pub const UAC2_PLAYBACK_ENDPOINT: u8 = 0x01;
/// Capture (device-to-host) endpoint address.
pub const UAC2_CAPTURE_ENDPOINT: u8 = 0x82;

/// Role/shape of a UAC2 function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Uac2Mode {
    /// Capture only (`c_chmask` non-zero, `p_chmask` zero).
    Microphone,
    /// Playback and capture (`p_chmask` and `c_chmask` non-zero).
    Headset,
    /// Playback only (`p_chmask` non-zero, `c_chmask` zero).
    Speaker,
}

impl Uac2Mode {
    /// Stability name used in status JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Microphone => "microphone",
            Self::Headset => "headset",
            Self::Speaker => "speaker",
        }
    }

    /// Derive the mode from enabled channel counts.
    pub fn from_channels(playback_channels: u8, capture_channels: u8) -> Option<Self> {
        match (playback_channels, capture_channels) {
            (0, 0) => None,
            (0, _) => Some(Self::Microphone),
            (_, 0) => Some(Self::Speaker),
            (_, _) => Some(Self::Headset),
        }
    }
}

/// Expanded UAC2 audio configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2Config {
    /// Playback channel count (`0` disables playback).
    pub playback_channels: u8,
    /// Capture channel count (`0` disables capture).
    pub capture_channels: u8,
    /// Primary sample rate, written to `p_srate`/`c_srate`.
    pub sample_rate_hz: u32,
    /// Primary sample size in bits, written to `p_ssize`/`c_ssize` (as bytes).
    pub bit_depth: u8,
    /// Advertised sample rates. Contains `sample_rate_hz` first by convention.
    pub sample_rates: Vec<u32>,
    /// Advertised sample sizes in bits. Contains `bit_depth` first by
    /// convention.
    pub sample_sizes: Vec<u8>,
    /// Microphone/headset/speaker classification.
    pub mode: Uac2Mode,
    /// Advertise a volume control in the generated descriptor.
    pub volume: bool,
    /// Advertise a mute control in the generated descriptor.
    pub mute: bool,
}

impl Uac2Config {
    /// A duplex-capable configuration with one rate and one sample size.
    pub fn new(
        playback_channels: u8,
        capture_channels: u8,
        sample_rate_hz: u32,
        bit_depth: u8,
    ) -> Self {
        let mode = Uac2Mode::from_channels(playback_channels, capture_channels)
            .unwrap_or(Uac2Mode::Headset);
        Self {
            playback_channels,
            capture_channels,
            sample_rate_hz,
            bit_depth,
            sample_rates: vec![sample_rate_hz],
            sample_sizes: vec![bit_depth],
            mode,
            volume: false,
            mute: false,
        }
    }

    /// Capture-only microphone mode.
    pub fn microphone(capture_channels: u8, sample_rate_hz: u32, bit_depth: u8) -> Self {
        let mut config = Self::new(0, capture_channels, sample_rate_hz, bit_depth);
        config.mode = Uac2Mode::Microphone;
        config
    }

    /// Playback-only speaker mode.
    pub fn speaker(playback_channels: u8, sample_rate_hz: u32, bit_depth: u8) -> Self {
        let mut config = Self::new(playback_channels, 0, sample_rate_hz, bit_depth);
        config.mode = Uac2Mode::Speaker;
        config
    }

    /// Playback-and-capture headset mode.
    pub fn headset(
        playback_channels: u8,
        capture_channels: u8,
        sample_rate_hz: u32,
        bit_depth: u8,
    ) -> Self {
        let mut config = Self::new(
            playback_channels,
            capture_channels,
            sample_rate_hz,
            bit_depth,
        );
        config.mode = Uac2Mode::Headset;
        config
    }

    /// Replace the advertised sample-rate list (primary rate first).
    pub fn with_sample_rates(mut self, rates: impl IntoIterator<Item = u32>) -> Self {
        self.sample_rates = rates.into_iter().collect();
        self
    }

    /// Replace the advertised sample-size list (primary size first).
    pub fn with_sample_sizes(mut self, sizes: impl IntoIterator<Item = u8>) -> Self {
        self.sample_sizes = sizes.into_iter().collect();
        self
    }

    /// Advertise a volume control.
    pub fn with_volume(mut self, volume: bool) -> Self {
        self.volume = volume;
        self
    }

    /// Advertise a mute control.
    pub fn with_mute(mut self, mute: bool) -> Self {
        self.mute = mute;
        self
    }

    /// Whether playback is enabled.
    pub fn playback_enabled(&self) -> bool {
        self.playback_channels > 0
    }

    /// Whether capture is enabled.
    pub fn capture_enabled(&self) -> bool {
        self.capture_channels > 0
    }

    /// Whether both directions are enabled.
    pub fn is_duplex(&self) -> bool {
        self.playback_enabled() && self.capture_enabled()
    }

    /// ConfigFS `p_chmask`/`c_chmask` value for a channel count.
    pub fn channel_mask(channels: u8) -> u32 {
        if channels == 0 {
            0
        } else {
            (1u32 << channels) - 1
        }
    }

    /// Byte size written to `p_ssize`/`c_ssize`.
    pub fn sample_size_bytes(&self) -> u8 {
        self.bit_depth / 8
    }

    /// Isochronous packet size for one direction at the highest advertised
    /// rate and the primary sample size.
    pub fn endpoint_packet_size(&self, channels: u8) -> u16 {
        if channels == 0 {
            return 0;
        }
        let rate = self
            .sample_rates
            .iter()
            .copied()
            .max()
            .unwrap_or(self.sample_rate_hz);
        // USB high-speed isochronous service interval is one microframe.
        let per_microframe = (u64::from(rate) + 7_999) / 8_000;
        let bytes = per_microframe * u64::from(self.sample_size_bytes()) * u64::from(channels);
        bytes.min(u64::from(u16::MAX)) as u16
    }

    /// Validate channels, rates, sizes, mode consistency, and descriptor
    /// bounds.
    pub fn validate(&self) -> Result<(), UsbError> {
        if !self.playback_enabled() && !self.capture_enabled() {
            return Err(UsbError::InvalidField {
                field: "function.uac2",
                reason: "UAC2 needs at least one playback or capture channel".to_owned(),
            });
        }
        if self.playback_channels > UAC2_MAX_CHANNELS || self.capture_channels > UAC2_MAX_CHANNELS {
            return Err(UsbError::InvalidField {
                field: "function.uac2.channels",
                reason: format!("UAC2 channel count must be 0..={UAC2_MAX_CHANNELS}"),
            });
        }
        let expected = Uac2Mode::from_channels(self.playback_channels, self.capture_channels)
            .expect("at least one channel was checked above");
        if self.mode != expected {
            return Err(UsbError::InvalidField {
                field: "function.uac2.mode",
                reason: format!(
                    "mode {} does not match playback={} capture={} channels",
                    self.mode.as_str(),
                    self.playback_channels,
                    self.capture_channels
                ),
            });
        }
        if !(UAC2_MIN_SAMPLE_RATE..=UAC2_MAX_SAMPLE_RATE).contains(&self.sample_rate_hz) {
            return Err(UsbError::InvalidField {
                field: "function.sample_rate_hz",
                reason: format!(
                    "sample rate must be {UAC2_MIN_SAMPLE_RATE}..={UAC2_MAX_SAMPLE_RATE} Hz"
                ),
            });
        }
        if !matches!(self.bit_depth, 16 | 24 | 32) {
            return Err(UsbError::InvalidField {
                field: "function.bit_depth",
                reason: "sample size must be 16, 24, or 32 bits".to_owned(),
            });
        }
        validate_rates(&self.sample_rates, self.sample_rate_hz)?;
        validate_sizes(&self.sample_sizes, self.bit_depth)?;

        for channels in [self.playback_channels, self.capture_channels] {
            if channels > 0 && self.endpoint_packet_size(channels) > 1024 {
                return Err(UsbError::InvalidField {
                    field: "function.uac2.endpoint",
                    reason: format!(
                        "isochronous packet for {channels} channels at {} Hz exceeds 1024 bytes",
                        self.sample_rate_hz
                    ),
                });
            }
        }

        let descriptors = self.descriptors();
        if descriptors.bytes.is_empty() || descriptors.bytes.len() > UAC2_MAX_DESCRIPTOR_BYTES {
            return Err(UsbError::InvalidField {
                field: "function.uac2.descriptors",
                reason: format!(
                    "generated descriptors must be 1..={UAC2_MAX_DESCRIPTOR_BYTES} bytes"
                ),
            });
        }
        Ok(())
    }

    /// Compact descriptor shape for status.
    pub fn descriptor_summary(&self) -> Uac2DescriptorSummary {
        self.descriptors().summary()
    }

    /// Generate the UAC2 AC/AS interface descriptor bytes.
    pub fn descriptors(&self) -> Uac2DescriptorSet {
        let mut bytes = Vec::new();
        let mut interfaces = 1u8; // Audio Control interface.
        let mut alternate_settings = 0u8;

        // Audio Control interface.
        bytes.extend_from_slice(&[9, 0x04, 0, 0, 0, 0x01, 0x01, 0x20, 0]);

        // Audio Control header; wTotalLength is patched after the AC block.
        let ac_start = bytes.len();
        bytes.extend_from_slice(&[9, 0x24, 0x01, 0x00, 0x02, 0x01, 0x00, 0x00, 0x00]);
        // Clock source: fixed internal clock, ID 0x10.
        bytes.extend_from_slice(&[8, 0x24, 0x0A, 0x10, 0x03, 0x00, 0x00, 0x00]);

        let playback_input = 0x01u8;
        let playback_output = 0x02u8;
        let playback_feature = 0x05u8;
        let capture_input = 0x03u8;
        let capture_output = 0x04u8;
        let capture_feature = 0x06u8;
        let clock = 0x10u8;

        if self.playback_enabled() {
            push_input_terminal(
                &mut bytes,
                playback_input,
                0x0101, // USB streaming
                self.playback_channels,
                clock,
            );
            let playback_source = if self.volume || self.mute {
                push_feature_unit(
                    &mut bytes,
                    playback_feature,
                    playback_input,
                    self.playback_channels,
                    self.volume,
                    self.mute,
                );
                playback_feature
            } else {
                playback_input
            };
            let output_type = match self.mode {
                Uac2Mode::Headset => 0x0401, // headset
                _ => 0x0301,                 // speaker
            };
            push_output_terminal(
                &mut bytes,
                playback_output,
                output_type,
                playback_source,
                clock,
            );
        }

        if self.capture_enabled() {
            push_input_terminal(
                &mut bytes,
                capture_input,
                0x0201, // microphone
                self.capture_channels,
                clock,
            );
            let capture_source = if self.volume || self.mute {
                push_feature_unit(
                    &mut bytes,
                    capture_feature,
                    capture_input,
                    self.capture_channels,
                    self.volume,
                    self.mute,
                );
                capture_feature
            } else {
                capture_input
            };
            push_output_terminal(
                &mut bytes,
                capture_output,
                0x0101, // USB streaming
                capture_source,
                clock,
            );
        }

        let ac_len = (bytes.len() - ac_start) as u16;
        bytes[ac_start + 6..ac_start + 8].copy_from_slice(&ac_len.to_le_bytes());

        let mut next_interface = 1u8;
        let mut playback_endpoint = None;
        let mut capture_endpoint = None;

        if self.playback_enabled() {
            push_streaming_interfaces(
                &mut bytes,
                next_interface,
                playback_input,
                0x01,
                self.playback_channels,
                &self.sample_rates,
                self.bit_depth,
                self.endpoint_packet_size(self.playback_channels),
            );
            playback_endpoint = Some(UAC2_PLAYBACK_ENDPOINT);
            next_interface += 1;
            interfaces += 1;
            alternate_settings = alternate_settings.saturating_add(2);
        }

        if self.capture_enabled() {
            push_streaming_interfaces(
                &mut bytes,
                next_interface,
                capture_output,
                0x82,
                self.capture_channels,
                &self.sample_rates,
                self.bit_depth,
                self.endpoint_packet_size(self.capture_channels),
            );
            capture_endpoint = Some(UAC2_CAPTURE_ENDPOINT);
            interfaces += 1;
            alternate_settings = alternate_settings.saturating_add(2);
        }

        Uac2DescriptorSet {
            bytes,
            interfaces,
            alternate_settings,
            playback_endpoint,
            capture_endpoint,
        }
    }

    /// A bridge is deliberately absent from this crate.
    pub fn require_bridge(&self) -> Result<(), UsbError> {
        self.require_bridge_for("uac2")
    }

    /// A bridge is deliberately absent from this crate, reported for `instance`.
    pub fn require_bridge_for(&self, instance: &str) -> Result<(), UsbError> {
        Err(UsbError::UnsupportedUac2Bridge {
            instance: instance.to_owned(),
        })
    }
}

impl Default for Uac2Config {
    fn default() -> Self {
        Self::headset(2, 1, 48_000, 16)
    }
}

fn validate_rates(rates: &[u32], primary: u32) -> Result<(), UsbError> {
    if rates.is_empty() || rates.len() > UAC2_MAX_SAMPLE_RATES {
        return Err(UsbError::InvalidField {
            field: "function.uac2.sample_rates",
            reason: format!("advertise 1..={UAC2_MAX_SAMPLE_RATES} sample rates"),
        });
    }
    if !rates.contains(&primary) {
        return Err(UsbError::InvalidField {
            field: "function.uac2.sample_rates",
            reason: format!("sample-rate list must contain the primary rate {primary} Hz"),
        });
    }
    let mut seen = std::collections::BTreeSet::new();
    for rate in rates {
        if !(UAC2_MIN_SAMPLE_RATE..=UAC2_MAX_SAMPLE_RATE).contains(rate) {
            return Err(UsbError::InvalidField {
                field: "function.uac2.sample_rates",
                reason: format!(
                    "sample rate {rate} must be {UAC2_MIN_SAMPLE_RATE}..={UAC2_MAX_SAMPLE_RATE} Hz"
                ),
            });
        }
        if !seen.insert(*rate) {
            return Err(UsbError::InvalidField {
                field: "function.uac2.sample_rates",
                reason: format!("duplicate sample rate {rate} Hz"),
            });
        }
    }
    Ok(())
}

fn validate_sizes(sizes: &[u8], primary: u8) -> Result<(), UsbError> {
    if sizes.is_empty() || sizes.len() > UAC2_MAX_SAMPLE_SIZES {
        return Err(UsbError::InvalidField {
            field: "function.uac2.sample_sizes",
            reason: format!("advertise 1..={UAC2_MAX_SAMPLE_SIZES} sample sizes"),
        });
    }
    if !sizes.contains(&primary) {
        return Err(UsbError::InvalidField {
            field: "function.uac2.sample_sizes",
            reason: format!("sample-size list must contain the primary size {primary} bits"),
        });
    }
    let mut seen = std::collections::BTreeSet::new();
    for size in sizes {
        if !matches!(size, 16 | 24 | 32) {
            return Err(UsbError::InvalidField {
                field: "function.uac2.sample_sizes",
                reason: format!("sample size {size} must be 16, 24, or 32 bits"),
            });
        }
        if !seen.insert(*size) {
            return Err(UsbError::InvalidField {
                field: "function.uac2.sample_sizes",
                reason: format!("duplicate sample size {size} bits"),
            });
        }
    }
    Ok(())
}

fn push_input_terminal(
    bytes: &mut Vec<u8>,
    terminal_id: u8,
    terminal_type: u16,
    channels: u8,
    clock: u8,
) {
    let [terminal_lo, terminal_hi] = terminal_type.to_le_bytes();
    bytes.extend_from_slice(&[
        17,
        0x24,
        0x02,
        terminal_id,
        terminal_lo,
        terminal_hi,
        0x00, // bAssocTerminal
        clock,
        channels,
        0x00,
        0x00,
        0x00,
        0x00, // wChannelConfig
        0x00, // iChannelNames
        0x00,
        0x00, // bmControls
        0x00, // iTerminal
    ]);
}

fn push_output_terminal(
    bytes: &mut Vec<u8>,
    terminal_id: u8,
    terminal_type: u16,
    source_id: u8,
    clock: u8,
) {
    let [terminal_lo, terminal_hi] = terminal_type.to_le_bytes();
    bytes.extend_from_slice(&[
        12,
        0x24,
        0x03,
        terminal_id,
        terminal_lo,
        terminal_hi,
        0x00, // bAssocTerminal
        source_id,
        clock,
        0x00,
        0x00, // bmControls
        0x00, // iTerminal
    ]);
}

fn push_feature_unit(
    bytes: &mut Vec<u8>,
    unit_id: u8,
    source_id: u8,
    channels: u8,
    volume: bool,
    mute: bool,
) -> u8 {
    let controls = (u8::from(mute)) | (u8::from(volume) << 1);
    let length = 6 + 4 * (usize::from(channels) + 1);
    bytes.extend_from_slice(&[length as u8, 0x24, 0x06, unit_id, source_id]);
    // Master channel control, then one zero-control entry per channel.
    bytes.extend_from_slice(&[controls, 0x00, 0x00, 0x00]);
    for _ in 0..channels {
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    }
    bytes.push(0x00); // iFeature
    unit_id
}

#[allow(clippy::too_many_arguments)]
fn push_streaming_interfaces(
    bytes: &mut Vec<u8>,
    interface: u8,
    terminal_link: u8,
    endpoint_address: u8,
    channels: u8,
    rates: &[u32],
    bit_depth: u8,
    max_packet_size: u16,
) {
    // Alternate setting 0: zero-bandwidth interface.
    bytes.extend_from_slice(&[9, 0x04, interface, 0, 0, 0x01, 0x02, 0x20, 0]);

    // Alternate setting 1: one isochronous endpoint.
    bytes.extend_from_slice(&[9, 0x04, interface, 1, 1, 0x01, 0x02, 0x20, 0]);

    // Class-specific AS interface descriptor (PCM, type I).
    bytes.extend_from_slice(&[
        16,
        0x24,
        0x01,
        terminal_link,
        0x00, // bmControls
        0x01, // bFormatType: FORMAT_TYPE_I
        0x01,
        0x00,
        0x00,
        0x00, // bmFormats: PCM
        channels,
        0x00,
        0x00,
        0x00,
        0x00, // bmChannelConfig
        0x00, // iChannelNames
    ]);

    // Format type I: rates are 24-bit little-endian values.
    let rate_bytes = 3 * rates.len();
    bytes.push((8 + rate_bytes) as u8);
    bytes.extend_from_slice(&[
        0x24,
        0x02,
        0x01, // bFormatType: FORMAT_TYPE_I
        channels,
        bit_depth / 8,
        bit_depth,
        rates.len() as u8,
    ]);
    for rate in rates {
        let value = rate.to_le_bytes();
        bytes.extend_from_slice(&value[..3]);
    }

    // Standard isochronous endpoint descriptor (asynchronous, data).
    let [packet_lo, packet_hi] = max_packet_size.to_le_bytes();
    bytes.extend_from_slice(&[
        7,
        0x05,
        endpoint_address,
        0x05, // isochronous, asynchronous
        packet_lo,
        packet_hi,
        1, // bInterval: one microframe
    ]);

    // Class-specific AS isochronous endpoint descriptor.
    bytes.extend_from_slice(&[8, 0x25, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00]);
}

/// Generated UAC2 descriptor bytes and their shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uac2DescriptorSet {
    /// Concatenated AC/AS interface descriptors (without the composite
    /// configuration descriptor header, which the kernel owns).
    pub bytes: Vec<u8>,
    /// Number of UAC2 interfaces (audio control plus active streaming).
    pub interfaces: u8,
    /// Number of alternate settings across streaming interfaces.
    pub alternate_settings: u8,
    /// Playback endpoint address, when playback is enabled.
    pub playback_endpoint: Option<u8>,
    /// Capture endpoint address, when capture is enabled.
    pub capture_endpoint: Option<u8>,
}

impl Uac2DescriptorSet {
    /// Compact summary embedded in status.
    pub fn summary(&self) -> Uac2DescriptorSummary {
        Uac2DescriptorSummary {
            descriptor_bytes: self.bytes.len(),
            interfaces: self.interfaces,
            alternate_settings: self.alternate_settings,
            endpoints: u8::from(self.playback_endpoint.is_some())
                + u8::from(self.capture_endpoint.is_some()),
            playback_endpoint: self.playback_endpoint,
            capture_endpoint: self.capture_endpoint,
        }
    }
}

/// Descriptor shape exposed by gadget status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2DescriptorSummary {
    /// Generated descriptor byte count.
    pub descriptor_bytes: usize,
    /// Interface count.
    pub interfaces: u8,
    /// Alternate-setting count.
    pub alternate_settings: u8,
    /// Endpoint count (zero, one, or two).
    pub endpoints: u8,
    /// Playback endpoint address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback_endpoint: Option<u8>,
    /// Capture endpoint address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_endpoint: Option<u8>,
}

/// Host audio bridge state. This crate never establishes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Uac2BridgeStatus {
    /// No ALSA/USB audio bridge is implemented or attached.
    Unsupported,
}

impl Uac2BridgeStatus {
    /// Stable name used by status JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
        }
    }

    /// Whether audio is actually bridged.
    pub fn is_supported(self) -> bool {
        false
    }
}

/// One enabled UAC2 direction in status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2StreamStatus {
    /// Channel count.
    pub channels: u8,
    /// Advertised sample rates in bits.
    pub sample_rates: Vec<u32>,
    /// Advertised sample sizes in bits.
    pub sample_sizes: Vec<u8>,
    /// Rate written to the ConfigFS attribute.
    pub configfs_rate_hz: u32,
    /// Sample size in bytes written to the ConfigFS attribute.
    pub configfs_sample_size: u8,
}

/// UAC2 status embedded in a gadget function snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2Status {
    /// Function instance (`audio`).
    pub instance: String,
    /// Microphone/headset/speaker mode.
    pub mode: Uac2Mode,
    /// Playback stream, when enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback: Option<Uac2StreamStatus>,
    /// Capture stream, when enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<Uac2StreamStatus>,
    /// Volume control is advertised.
    pub volume: bool,
    /// Mute control is advertised.
    pub mute: bool,
    /// Generated descriptor shape.
    pub descriptors: Uac2DescriptorSummary,
    /// Host audio bridge state (always unsupported).
    pub bridge: Uac2BridgeStatus,
}

impl Uac2Status {
    /// Build status from a UAC2 function model.
    pub fn from_config(instance: impl Into<String>, config: &Uac2Config) -> Self {
        let stream = |channels: u8| Uac2StreamStatus {
            channels,
            sample_rates: config.sample_rates.clone(),
            sample_sizes: config.sample_sizes.clone(),
            configfs_rate_hz: config.sample_rate_hz,
            configfs_sample_size: config.sample_size_bytes(),
        };
        Self {
            instance: instance.into(),
            mode: config.mode,
            playback: config
                .playback_enabled()
                .then(|| stream(config.playback_channels)),
            capture: config
                .capture_enabled()
                .then(|| stream(config.capture_channels)),
            volume: config.volume,
            mute: config.mute,
            descriptors: config.descriptors().summary(),
            bridge: Uac2BridgeStatus::Unsupported,
        }
    }

    /// Whether a host audio bridge is available (always false).
    pub fn is_bridged(&self) -> bool {
        self.bridge.is_supported()
    }
}
