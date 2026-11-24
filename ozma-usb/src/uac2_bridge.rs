// SPDX-License-Identifier: AGPL-3.0-only
//! Transport-independent UAC2 isochronous ↔ PCM bridge core.
//!
//! The bridge sits between two injectable halves:
//!
//! * **Playback** (host → device): a USB isochronous **OUT** endpoint
//!   ([`UAC2_PLAYBACK_ENDPOINT`]) is drained into a bounded PCM ring buffer,
//!   converted to the audio sink's format, and written to a [`Uac2AudioSink`].
//! * **Capture** (device → host): a [`Uac2AudioSource`] is packetised into
//!   sample-accurate USB isochronous **IN** packets
//!   ([`UAC2_CAPTURE_ENDPOINT`]).
//!
//! No USB device, ALSA device, PipeWire stream, `unsafe`, or real UDC is
//! touched here. Endpoints are [`Uac2EndpointIo`] trait objects and audio is a
//! [`Uac2AudioSink`]/[`Uac2AudioSource`] pair, so the whole core is exercised
//! with the in-memory fakes in this module.
//!
//! # Format rules
//!
//! * PCM is interleaved signed little-endian, 16/24/32-bit, 1–8 channels,
//!   8–192 kHz.
//! * Bit-depth and channel-count conversion is explicit in
//!   [`convert_pcm`]: wider/narrower samples are scaled, mono is replicated,
//!   many-to-one is averaged, and other channel-count changes keep the first
//!   `min(source, target)` channels and zero-fill or truncate the rest.
//! * **No resampling is implicit.** A sample-rate mismatch is a typed
//!   [`UsbError::Uac2Format`] error. There is deliberately no linear or
//!   sinc resampler in this core.
//!
//! # Packet sizing
//!
//! USB high-speed isochronous service intervals are expressed in
//! microframes. [`Uac2Packetizer`] accumulates `sample_rate * interval` and
//! divides by 8000, so for example 44.1 kHz alternates 5- and 6-frame packets
//! and sums to exactly 44 100 frames per second. Packet sizes are therefore
//! sample-accurate over any window instead of being rounded to a fixed
//! payload.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::error::UsbError;
use crate::uac2::{
    Uac2BridgeStatus, Uac2Config, Uac2Status, UAC2_CAPTURE_ENDPOINT, UAC2_MAX_CHANNELS,
    UAC2_MAX_SAMPLE_RATE, UAC2_MIN_SAMPLE_RATE, UAC2_PLAYBACK_ENDPOINT,
};

/// Microframes per second for USB high-speed isochronous endpoints.
pub const UAC2_MICROFRAMES_PER_SECOND: u32 = 8_000;
/// Maximum payload of one high-speed isochronous endpoint packet.
pub const UAC2_MAX_PACKET_BYTES: usize = 1024;
/// Default playback ring capacity (about 170 ms of 48 kHz stereo S24).
pub const UAC2_DEFAULT_RING_BYTES: usize = 64 * 1024;
/// Largest supported `bInterval` exponent (16 microframes).
pub const UAC2_MAX_PACKET_INTERVAL: u8 = 16;

/// Direction of a UAC2 isochronous endpoint from the device's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Uac2Direction {
    /// Host-to-device, device reads (USB OUT).
    Out,
    /// Device-to-host, device writes (USB IN).
    In,
}

impl Uac2Direction {
    /// Stable name used in status JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Out => "out",
            Self::In => "in",
        }
    }

    /// Whether the USB address direction bit is set.
    pub fn is_in(self) -> bool {
        matches!(self, Self::In)
    }
}

/// Sample width of one interleaved PCM channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Uac2SampleSize {
    /// Signed 16-bit little-endian.
    S16,
    /// Signed 24-bit little-endian, packed in three bytes.
    S24,
    /// Signed 32-bit little-endian.
    S32,
}

impl Uac2SampleSize {
    /// Convert a UAC2 descriptor bit depth (16/24/32) into a sample size.
    pub fn from_bits(bits: u8) -> Option<Self> {
        match bits {
            16 => Some(Self::S16),
            24 => Some(Self::S24),
            32 => Some(Self::S32),
            _ => None,
        }
    }

    /// Bit depth.
    pub fn bits(self) -> u8 {
        match self {
            Self::S16 => 16,
            Self::S24 => 24,
            Self::S32 => 32,
        }
    }

    /// Packed bytes per sample.
    pub fn bytes(self) -> usize {
        usize::from(self.bits() / 8)
    }
}

/// A validated interleaved PCM format for one UAC2 direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2PcmFormat {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u8,
    /// Sample width.
    pub sample_size: Uac2SampleSize,
}

impl Uac2PcmFormat {
    /// Build and validate a PCM format.
    pub fn new(
        sample_rate: u32,
        channels: u8,
        sample_size: Uac2SampleSize,
    ) -> Result<Self, UsbError> {
        let format = Self {
            sample_rate,
            channels,
            sample_size,
        };
        format.validate()?;
        Ok(format)
    }

    /// Build the format of one direction of a [`Uac2Config`].
    pub fn from_uac2(config: &Uac2Config, channels: u8) -> Result<Self, UsbError> {
        let sample_size =
            Uac2SampleSize::from_bits(config.bit_depth).ok_or_else(|| UsbError::Uac2Format {
                reason: format!(
                    "UAC2 sample size must be 16, 24, or 32 bits, not {}",
                    config.bit_depth
                ),
            })?;
        Self::new(config.sample_rate_hz, channels, sample_size)
    }

    /// Validate the rate, channel count, and sample size.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.channels == 0 || self.channels > UAC2_MAX_CHANNELS {
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "channel count must be 1..={UAC2_MAX_CHANNELS}, not {}",
                    self.channels
                ),
            });
        }
        if !(UAC2_MIN_SAMPLE_RATE..=UAC2_MAX_SAMPLE_RATE).contains(&self.sample_rate) {
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "sample rate must be {UAC2_MIN_SAMPLE_RATE}..={UAC2_MAX_SAMPLE_RATE} Hz, not {}",
                    self.sample_rate
                ),
            });
        }
        Ok(())
    }

    /// Packed bytes per sample.
    pub fn bytes_per_sample(&self) -> usize {
        self.sample_size.bytes()
    }

    /// Packed bytes in one interleaved frame.
    pub fn frame_bytes(&self) -> usize {
        usize::from(self.channels) * self.bytes_per_sample()
    }

    /// Byte count for `frames` interleaved frames.
    pub fn frames_to_bytes(&self, frames: usize) -> Result<usize, UsbError> {
        frames
            .checked_mul(self.frame_bytes())
            .ok_or(UsbError::Uac2Overflow {
                buffer: "PCM frame conversion",
                limit: usize::MAX,
                requested: frames,
            })
    }

    /// Whole frames represented by `bytes`, rejecting ragged buffers.
    pub fn bytes_to_frames(&self, bytes: usize) -> Result<usize, UsbError> {
        if bytes % self.frame_bytes() != 0 {
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "{bytes} bytes is not a whole number of {}-channel {} frames ({} bytes/frame)",
                    self.channels,
                    self.sample_size.bits(),
                    self.frame_bytes()
                ),
            });
        }
        Ok(bytes / self.frame_bytes())
    }
}

/// One UAC2 isochronous endpoint descriptor used by the bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2Endpoint {
    /// USB endpoint address, including the IN bit.
    pub address: u8,
    /// Transfer direction.
    pub direction: Uac2Direction,
    /// `wMaxPacketSize` for the endpoint.
    pub max_packet_size: u16,
    /// `bInterval` exponent in microframes.
    pub interval: u8,
}

impl Uac2Endpoint {
    /// Build and validate an endpoint descriptor.
    pub fn new(
        address: u8,
        direction: Uac2Direction,
        max_packet_size: u16,
        interval: u8,
    ) -> Result<Self, UsbError> {
        let endpoint = Self {
            address,
            direction,
            max_packet_size,
            interval,
        };
        endpoint.validate()?;
        Ok(endpoint)
    }

    /// Playback OUT endpoint for a packet size.
    pub fn playback(max_packet_size: u16) -> Result<Self, UsbError> {
        Self::new(
            UAC2_PLAYBACK_ENDPOINT,
            Uac2Direction::Out,
            max_packet_size,
            1,
        )
    }

    /// Capture IN endpoint for a packet size.
    pub fn capture(max_packet_size: u16) -> Result<Self, UsbError> {
        Self::new(UAC2_CAPTURE_ENDPOINT, Uac2Direction::In, max_packet_size, 1)
    }

    /// Whether this is a device-to-host endpoint.
    pub fn is_in(&self) -> bool {
        self.direction.is_in()
    }

    /// Validate the address/direction pairing and packet bounds.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.address & 0x0f == 0 {
            return Err(UsbError::Uac2Endpoint {
                address: self.address,
                operation: "configure",
                reason: "endpoint number must be 1..=15".to_owned(),
            });
        }
        let address_is_in = self.address & 0x80 != 0;
        if address_is_in != self.is_in() {
            return Err(UsbError::Uac2Endpoint {
                address: self.address,
                operation: "configure",
                reason: format!(
                    "address direction bit says {}, but the endpoint direction is {}",
                    if address_is_in { "IN" } else { "OUT" },
                    self.direction.as_str()
                ),
            });
        }
        if self.max_packet_size == 0 || usize::from(self.max_packet_size) > UAC2_MAX_PACKET_BYTES {
            return Err(UsbError::Uac2Endpoint {
                address: self.address,
                operation: "configure",
                reason: format!(
                    "wMaxPacketSize must be 1..={UAC2_MAX_PACKET_BYTES}, not {}",
                    self.max_packet_size
                ),
            });
        }
        if self.interval == 0 || self.interval > UAC2_MAX_PACKET_INTERVAL {
            return Err(UsbError::Uac2Endpoint {
                address: self.address,
                operation: "configure",
                reason: format!(
                    "bInterval must be 1..={UAC2_MAX_PACKET_INTERVAL}, not {}",
                    self.interval
                ),
            });
        }
        Ok(())
    }
}

/// Sample-accurate packet size for the next isochronous service interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2PacketSpec {
    /// Interleaved sample frames in this packet.
    pub frames: usize,
    /// Payload bytes in this packet.
    pub bytes: usize,
}

/// Sample-accurate isochronous packet sizing.
///
/// The accumulator keeps the fractional frame count across service intervals.
/// It never rounds to a single fixed packet size, so long-run packet payload
/// sums exactly match the sample rate.
#[derive(Clone, Debug)]
pub struct Uac2Packetizer {
    format: Uac2PcmFormat,
    interval: u8,
    accumulator: u32,
    packets: u64,
}

impl Uac2Packetizer {
    /// Create a packetizer for one format and `bInterval` exponent.
    pub fn new(format: Uac2PcmFormat, interval: u8) -> Result<Self, UsbError> {
        format.validate()?;
        if interval == 0 || interval > UAC2_MAX_PACKET_INTERVAL {
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "packet interval must be 1..={UAC2_MAX_PACKET_INTERVAL} microframes, not {interval}"
                ),
            });
        }
        Ok(Self {
            format,
            interval,
            accumulator: 0,
            packets: 0,
        })
    }

    /// Format being packetised.
    pub fn format(&self) -> Uac2PcmFormat {
        self.format
    }

    /// Service interval in microframes.
    pub fn interval(&self) -> u8 {
        self.interval
    }

    /// Fractional sample-frame accumulator, in units of 1/8000 frame.
    pub fn accumulator(&self) -> u32 {
        self.accumulator
    }

    /// Number of packets produced so far.
    pub fn packets(&self) -> u64 {
        self.packets
    }

    /// Size of the next packet without advancing the accumulator.
    pub fn peek_next(&self) -> Uac2PacketSpec {
        let total = u64::from(self.accumulator)
            + u64::from(self.format.sample_rate) * u64::from(self.interval);
        let frames =
            usize::try_from(total / u64::from(UAC2_MICROFRAMES_PER_SECOND)).unwrap_or(usize::MAX);
        let bytes = frames
            .saturating_mul(self.format.frame_bytes())
            .min(usize::MAX);
        Uac2PacketSpec { frames, bytes }
    }

    /// Return the next packet size and advance to the following interval.
    pub fn advance(&mut self) -> Uac2PacketSpec {
        let spec = self.peek_next();
        let total = u64::from(self.accumulator)
            + u64::from(self.format.sample_rate) * u64::from(self.interval);
        self.accumulator = (total % u64::from(UAC2_MICROFRAMES_PER_SECOND)) as u32;
        self.packets = self.packets.saturating_add(1);
        spec
    }

    /// Largest packet this format can produce for the configured interval.
    pub fn max_packet_bytes(&self) -> usize {
        let product = u64::from(self.format.sample_rate) * u64::from(self.interval);
        let max_frames = product.div_ceil(u64::from(UAC2_MICROFRAMES_PER_SECOND)) as usize;
        max_frames.saturating_mul(self.format.frame_bytes())
    }

    /// Reset the accumulator and packet counter.
    pub fn reset(&mut self) {
        self.accumulator = 0;
        self.packets = 0;
    }
}

/// Convert interleaved PCM between two formats without resampling.
///
/// Sample rates must match. Bit-depth conversion scales to the full signed
/// range, mono is replicated to every target channel, many-to-one is averaged,
/// and every other channel-count change keeps the first `min(source, target)`
/// channels and zero-fills or drops the rest.
pub fn convert_pcm(
    input: &[u8],
    from: Uac2PcmFormat,
    to: Uac2PcmFormat,
) -> Result<Vec<u8>, UsbError> {
    from.validate()?;
    to.validate()?;
    if from.sample_rate != to.sample_rate {
        return Err(UsbError::Uac2Format {
            reason: format!(
                "cannot convert {} Hz to {} Hz: resampling is not implemented; configure both sides to the same rate",
                from.sample_rate, to.sample_rate
            ),
        });
    }
    from.bytes_to_frames(input.len())?;
    if from == to {
        return Ok(input.to_vec());
    }

    let source_channels = usize::from(from.channels);
    let target_channels = usize::from(to.channels);
    let source_frame_bytes = from.frame_bytes();
    let target_sample_bytes = to.bytes_per_sample();
    let frames = input.len() / source_frame_bytes;
    let mut output = Vec::with_capacity(frames * to.frame_bytes());
    let mut full = vec![0i32; source_channels];

    for frame in 0..frames {
        let base = frame * source_frame_bytes;
        for (channel, slot) in full.iter_mut().enumerate() {
            let offset = base + channel * from.bytes_per_sample();
            *slot = decode_full(&input[offset..], from.sample_size);
        }
        let downmix = if target_channels == 1 && source_channels > 1 {
            let sum: i64 = full.iter().map(|value| i64::from(*value)).sum();
            Some((sum / source_channels as i64) as i32)
        } else {
            None
        };
        for channel in 0..target_channels {
            let value = match (source_channels, target_channels) {
                (source, target) if source == target => full[channel],
                (1, _) => full[0],
                (_, 1) => downmix.unwrap_or(0),
                _ => {
                    if channel < source_channels {
                        full[channel]
                    } else {
                        0
                    }
                }
            };
            encode_full(value, to.sample_size, target_sample_bytes, &mut output);
        }
    }
    Ok(output)
}

fn decode_full(bytes: &[u8], size: Uac2SampleSize) -> i32 {
    match size {
        Uac2SampleSize::S16 => {
            let value = i16::from_le_bytes([bytes[0], bytes[1]]);
            i32::from(value) << 16
        }
        Uac2SampleSize::S24 => {
            let value =
                (i32::from(bytes[0]) | (i32::from(bytes[1]) << 8) | (i32::from(bytes[2]) << 16))
                    << 8
                    >> 8;
            value << 8
        }
        Uac2SampleSize::S32 => i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
    }
}

fn encode_full(value: i32, size: Uac2SampleSize, sample_bytes: usize, output: &mut Vec<u8>) {
    debug_assert_eq!(sample_bytes, size.bytes());
    match size {
        Uac2SampleSize::S16 => output.extend_from_slice(&((value >> 16) as i16).to_le_bytes()),
        Uac2SampleSize::S24 => {
            let bytes = (value >> 8).to_le_bytes();
            output.extend_from_slice(&bytes[..3]);
        }
        Uac2SampleSize::S32 => output.extend_from_slice(&value.to_le_bytes()),
    }
}

/// One isochronous endpoint handle injected into the bridge.
pub trait Uac2EndpointIo {
    /// Descriptor represented by this handle.
    fn endpoint(&self) -> Uac2Endpoint;

    /// Read one OUT packet into `buffer`, returning the number of bytes.
    ///
    /// A zero-length packet is valid and means "no payload this interval".
    fn read_packet(&mut self, buffer: &mut [u8]) -> Result<usize, UsbError>;

    /// Write one IN packet.
    fn write_packet(&mut self, data: &[u8]) -> Result<usize, UsbError>;

    /// Flush pending IN packets.
    fn flush(&mut self) -> Result<(), UsbError> {
        Ok(())
    }
}

/// An audio sink fed by host playback (USB OUT) PCM.
pub trait Uac2AudioSink: Send {
    /// Native format expected by [`Uac2AudioSink::write`].
    fn format(&self) -> Uac2PcmFormat;

    /// Write interleaved PCM, returning the number of bytes consumed.
    ///
    /// A short write applies backpressure; zero means no progress this cycle.
    fn write(&mut self, pcm: &[u8]) -> Result<usize, UsbError>;

    /// Flush the sink. Must be idempotent.
    fn flush(&mut self) -> Result<(), UsbError> {
        Ok(())
    }
}

/// An audio source packetised toward the host (USB IN).
pub trait Uac2AudioSource: Send {
    /// Native format produced by [`Uac2AudioSource::read`].
    fn format(&self) -> Uac2PcmFormat;

    /// Read up to `pcm.len()` bytes, returning the number of bytes produced.
    fn read(&mut self, pcm: &mut [u8]) -> Result<usize, UsbError>;

    /// Flush source state. Must be idempotent.
    fn flush(&mut self) -> Result<(), UsbError> {
        Ok(())
    }
}

/// Bounded frame-aligned PCM ring buffer.
#[derive(Debug)]
struct PcmRing {
    storage: Vec<u8>,
    head: usize,
    len: usize,
    frame_bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RingPush {
    written: usize,
    dropped: usize,
    overflowed: bool,
}

impl PcmRing {
    fn new(capacity_bytes: usize, frame_bytes: usize) -> Result<Self, UsbError> {
        if frame_bytes == 0 {
            return Err(UsbError::Uac2Format {
                reason: "PCM frame size must be non-zero".to_owned(),
            });
        }
        let capacity = capacity_bytes / frame_bytes * frame_bytes;
        if capacity < frame_bytes {
            return Err(UsbError::Uac2Overflow {
                buffer: "playback ring",
                limit: capacity_bytes,
                requested: frame_bytes,
            });
        }
        Ok(Self {
            storage: vec![0; capacity],
            head: 0,
            len: 0,
            frame_bytes,
        })
    }

    fn capacity(&self) -> usize {
        self.storage.len()
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn available(&self) -> usize {
        self.len
    }

    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    fn push(&mut self, data: &[u8]) -> Result<RingPush, UsbError> {
        if data.len() % self.frame_bytes != 0 {
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "PCM buffer of {} bytes is not frame-aligned ({} bytes/frame)",
                    data.len(),
                    self.frame_bytes
                ),
            });
        }
        if data.is_empty() {
            return Ok(RingPush::default());
        }

        let capacity_frames = self.capacity() / self.frame_bytes;
        let mut data = data;
        let mut frames = data.len() / self.frame_bytes;
        let mut dropped = 0usize;

        if frames > capacity_frames {
            let excess = frames - capacity_frames;
            dropped += excess * self.frame_bytes;
            data = &data[excess * self.frame_bytes..];
            frames = capacity_frames;
        }

        let used_frames = self.len / self.frame_bytes;
        let free_frames = capacity_frames.saturating_sub(used_frames);
        if frames > free_frames {
            let discard = frames - free_frames;
            self.consume(discard * self.frame_bytes);
            dropped += discard * self.frame_bytes;
        }

        let tail = (self.head + self.len) % self.capacity();
        self.storage[tail..tail + data.len()].copy_from_slice(data);
        self.len += data.len();
        Ok(RingPush {
            written: data.len(),
            dropped,
            overflowed: dropped > 0,
        })
    }

    /// Contiguous readable bytes, capped at `max_bytes` and frame-aligned.
    fn peek(&self, max_bytes: usize) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        let contiguous = self.capacity() - self.head;
        let wanted = self.len.min(contiguous).min(max_bytes);
        let wanted = wanted / self.frame_bytes * self.frame_bytes;
        &self.storage[self.head..self.head + wanted]
    }

    fn consume(&mut self, bytes: usize) {
        let bytes = bytes.min(self.len) / self.frame_bytes * self.frame_bytes;
        self.head = (self.head + bytes) % self.capacity();
        self.len -= bytes;
        if self.len == 0 {
            self.head = 0;
        }
    }
}

/// In-memory isochronous endpoint for tests.
///
/// The endpoint is shared with a [`FakeUac2EndpointHandle`] so a test can queue
/// host packets and inspect device packets after the endpoint has been moved
/// into a [`Uac2Bridge`].
#[derive(Debug, Clone)]
pub struct FakeUac2Endpoint {
    endpoint: Uac2Endpoint,
    state: Arc<Mutex<FakeEndpointState>>,
}

#[derive(Debug, Default)]
struct FakeEndpointState {
    host_packets: VecDeque<Vec<u8>>,
    written_packets: Vec<Vec<u8>>,
    flushes: u64,
}

/// Shared handle to a [`FakeUac2Endpoint`].
#[derive(Debug, Clone)]
pub struct FakeUac2EndpointHandle {
    endpoint: Uac2Endpoint,
    state: Arc<Mutex<FakeEndpointState>>,
}

impl FakeUac2Endpoint {
    /// Empty fake endpoint for a descriptor.
    pub fn new(endpoint: Uac2Endpoint) -> Self {
        Self {
            endpoint,
            state: Arc::new(Mutex::new(FakeEndpointState::default())),
        }
    }

    /// Playback OUT endpoint helper.
    pub fn out(max_packet_size: u16) -> Result<Self, UsbError> {
        Ok(Self::new(Uac2Endpoint::playback(max_packet_size)?))
    }

    /// Capture IN endpoint helper.
    pub fn in_endpoint(max_packet_size: u16) -> Result<Self, UsbError> {
        Ok(Self::new(Uac2Endpoint::capture(max_packet_size)?))
    }

    /// Shared handle for queueing and inspection.
    pub fn handle(&self) -> FakeUac2EndpointHandle {
        FakeUac2EndpointHandle {
            endpoint: self.endpoint,
            state: Arc::clone(&self.state),
        }
    }

    /// Queue a host-to-device packet for a subsequent read.
    pub fn queue_host_packet(&mut self, data: impl Into<Vec<u8>>) -> Result<(), UsbError> {
        self.handle().queue_host_packet(data)
    }

    /// Packets the bridge wrote to an IN endpoint.
    pub fn written_packets(&self) -> Vec<Vec<u8>> {
        self.handle().written_packets()
    }

    /// Drain packets the bridge wrote.
    pub fn take_written_packets(&mut self) -> Vec<Vec<u8>> {
        self.handle().take_written_packets()
    }

    /// Number of queued host packets not yet read.
    pub fn pending_host_packets(&self) -> usize {
        self.handle().pending_host_packets()
    }

    /// Flush operations observed.
    pub fn flushes(&self) -> u64 {
        self.handle().flushes()
    }
}

impl FakeUac2EndpointHandle {
    /// Endpoint descriptor.
    pub fn endpoint(&self) -> Uac2Endpoint {
        self.endpoint
    }

    /// Queue a host-to-device packet for a subsequent read.
    pub fn queue_host_packet(&self, data: impl Into<Vec<u8>>) -> Result<(), UsbError> {
        if !matches!(self.endpoint.direction, Uac2Direction::Out) {
            return Err(UsbError::Uac2Endpoint {
                address: self.endpoint.address,
                operation: "queue",
                reason: "only OUT endpoints accept host packets".to_owned(),
            });
        }
        let data = data.into();
        if data.len() > usize::from(self.endpoint.max_packet_size) {
            return Err(UsbError::Uac2Overflow {
                buffer: "fake endpoint packet",
                limit: usize::from(self.endpoint.max_packet_size),
                requested: data.len(),
            });
        }
        self.state
            .lock()
            .map_err(|_| UsbError::Uac2Endpoint {
                address: self.endpoint.address,
                operation: "queue",
                reason: "fake endpoint state lock poisoned".to_owned(),
            })?
            .host_packets
            .push_back(data);
        Ok(())
    }

    /// Packets the bridge wrote to an IN endpoint.
    pub fn written_packets(&self) -> Vec<Vec<u8>> {
        self.state
            .lock()
            .map(|state| state.written_packets.clone())
            .unwrap_or_default()
    }

    /// Drain packets the bridge wrote.
    pub fn take_written_packets(&self) -> Vec<Vec<u8>> {
        self.state
            .lock()
            .map(|mut state| std::mem::take(&mut state.written_packets))
            .unwrap_or_default()
    }

    /// Number of queued host packets not yet read.
    pub fn pending_host_packets(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.host_packets.len())
            .unwrap_or(0)
    }

    /// Flush operations observed.
    pub fn flushes(&self) -> u64 {
        self.state.lock().map(|state| state.flushes).unwrap_or(0)
    }
}

impl Uac2EndpointIo for FakeUac2Endpoint {
    fn endpoint(&self) -> Uac2Endpoint {
        self.endpoint
    }

    fn read_packet(&mut self, buffer: &mut [u8]) -> Result<usize, UsbError> {
        if self.endpoint.is_in() {
            return Err(UsbError::Uac2Endpoint {
                address: self.endpoint.address,
                operation: "read",
                reason: "IN endpoints cannot be read".to_owned(),
            });
        }
        let mut state = self.state.lock().map_err(|_| UsbError::Uac2Endpoint {
            address: self.endpoint.address,
            operation: "read",
            reason: "fake endpoint state lock poisoned".to_owned(),
        })?;
        let Some(packet) = state.host_packets.pop_front() else {
            return Ok(0);
        };
        if packet.len() > buffer.len() {
            return Err(UsbError::Uac2Overflow {
                buffer: "endpoint read",
                limit: buffer.len(),
                requested: packet.len(),
            });
        }
        buffer[..packet.len()].copy_from_slice(&packet);
        Ok(packet.len())
    }

    fn write_packet(&mut self, data: &[u8]) -> Result<usize, UsbError> {
        if !self.endpoint.is_in() {
            return Err(UsbError::Uac2Endpoint {
                address: self.endpoint.address,
                operation: "write",
                reason: "OUT endpoints cannot be written".to_owned(),
            });
        }
        if data.len() > usize::from(self.endpoint.max_packet_size) {
            return Err(UsbError::Uac2Overflow {
                buffer: "fake endpoint packet",
                limit: usize::from(self.endpoint.max_packet_size),
                requested: data.len(),
            });
        }
        self.state
            .lock()
            .map_err(|_| UsbError::Uac2Endpoint {
                address: self.endpoint.address,
                operation: "write",
                reason: "fake endpoint state lock poisoned".to_owned(),
            })?
            .written_packets
            .push(data.to_vec());
        Ok(data.len())
    }

    fn flush(&mut self) -> Result<(), UsbError> {
        let mut state = self.state.lock().map_err(|_| UsbError::Uac2Endpoint {
            address: self.endpoint.address,
            operation: "flush",
            reason: "fake endpoint state lock poisoned".to_owned(),
        })?;
        state.flushes = state.flushes.saturating_add(1);
        Ok(())
    }
}

/// In-memory audio sink with optional per-call backpressure.
///
/// A [`FakeUac2AudioSinkHandle`] observes accepted PCM after the sink is moved
/// into a bridge.
#[derive(Debug, Clone)]
pub struct FakeUac2AudioSink {
    state: Arc<Mutex<FakeSinkState>>,
}

#[derive(Debug)]
struct FakeSinkState {
    format: Uac2PcmFormat,
    written: Vec<u8>,
    max_write: Option<usize>,
    write_calls: u64,
    flushes: u64,
}

/// Shared handle to a [`FakeUac2AudioSink`].
#[derive(Debug, Clone)]
pub struct FakeUac2AudioSinkHandle {
    state: Arc<Mutex<FakeSinkState>>,
}

impl FakeUac2AudioSink {
    /// A sink for `format` that accepts everything.
    pub fn new(format: Uac2PcmFormat) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeSinkState {
                format,
                written: Vec::new(),
                max_write: None,
                write_calls: 0,
                flushes: 0,
            })),
        }
    }

    /// Accept at most `bytes` (rounded down to a frame) per `write` call.
    pub fn with_max_write(self, bytes: usize) -> Self {
        if let Ok(mut state) = self.state.lock() {
            state.max_write = Some(bytes);
        }
        self
    }

    /// Shared handle for inspection.
    pub fn handle(&self) -> FakeUac2AudioSinkHandle {
        FakeUac2AudioSinkHandle {
            state: Arc::clone(&self.state),
        }
    }

    /// All PCM accepted so far.
    pub fn written(&self) -> Vec<u8> {
        self.handle().written()
    }

    /// Drain accepted PCM.
    pub fn take_written(&mut self) -> Vec<u8> {
        self.handle().take_written()
    }

    /// Number of accepted bytes.
    pub fn written_bytes(&self) -> usize {
        self.handle().written().len()
    }

    /// Number of `write` calls.
    pub fn write_calls(&self) -> u64 {
        self.handle().write_calls()
    }

    /// Number of `flush` calls.
    pub fn flushes(&self) -> u64 {
        self.handle().flushes()
    }
}

impl FakeUac2AudioSinkHandle {
    /// Sink format.
    pub fn format(&self) -> Uac2PcmFormat {
        self.state
            .lock()
            .map(|state| state.format)
            .unwrap_or(Uac2PcmFormat {
                sample_rate: 48_000,
                channels: 1,
                sample_size: Uac2SampleSize::S16,
            })
    }

    /// All PCM accepted so far.
    pub fn written(&self) -> Vec<u8> {
        self.state
            .lock()
            .map(|state| state.written.clone())
            .unwrap_or_default()
    }

    /// Drain accepted PCM.
    pub fn take_written(&self) -> Vec<u8> {
        self.state
            .lock()
            .map(|mut state| std::mem::take(&mut state.written))
            .unwrap_or_default()
    }

    /// Number of accepted bytes.
    pub fn written_bytes(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.written.len())
            .unwrap_or(0)
    }

    /// Number of `write` calls.
    pub fn write_calls(&self) -> u64 {
        self.state
            .lock()
            .map(|state| state.write_calls)
            .unwrap_or(0)
    }

    /// Number of `flush` calls.
    pub fn flushes(&self) -> u64 {
        self.state.lock().map(|state| state.flushes).unwrap_or(0)
    }
}

impl Uac2AudioSink for FakeUac2AudioSink {
    fn format(&self) -> Uac2PcmFormat {
        self.handle().format()
    }

    fn write(&mut self, pcm: &[u8]) -> Result<usize, UsbError> {
        let mut state = self.state.lock().map_err(|_| UsbError::Uac2Endpoint {
            address: 0,
            operation: "sink write",
            reason: "fake sink state lock poisoned".to_owned(),
        })?;
        state.write_calls = state.write_calls.saturating_add(1);
        if pcm.len() % state.format.frame_bytes() != 0 {
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "sink received {} bytes, not whole {}-byte frames",
                    pcm.len(),
                    state.format.frame_bytes()
                ),
            });
        }
        let limit = state.max_write.unwrap_or(pcm.len()).min(pcm.len())
            / state.format.frame_bytes()
            * state.format.frame_bytes();
        state.written.extend_from_slice(&pcm[..limit]);
        Ok(limit)
    }

    fn flush(&mut self) -> Result<(), UsbError> {
        let mut state = self.state.lock().map_err(|_| UsbError::Uac2Endpoint {
            address: 0,
            operation: "sink flush",
            reason: "fake sink state lock poisoned".to_owned(),
        })?;
        state.flushes = state.flushes.saturating_add(1);
        Ok(())
    }
}

/// In-memory audio source: infinite silence or a finite PCM buffer.
#[derive(Debug, Clone)]
pub struct FakeUac2AudioSource {
    state: Arc<Mutex<FakeSourceState>>,
}

#[derive(Debug)]
struct FakeSourceState {
    format: Uac2PcmFormat,
    data: Vec<u8>,
    position: usize,
    infinite_silence: bool,
    max_read_bytes: Option<usize>,
    read_calls: u64,
    flushes: u64,
}

/// Shared handle to a [`FakeUac2AudioSource`].
#[derive(Debug, Clone)]
pub struct FakeUac2AudioSourceHandle {
    state: Arc<Mutex<FakeSourceState>>,
}

impl FakeUac2AudioSource {
    /// An endless silence source.
    pub fn silence(format: Uac2PcmFormat) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeSourceState {
                format,
                data: Vec::new(),
                position: 0,
                infinite_silence: true,
                max_read_bytes: None,
                read_calls: 0,
                flushes: 0,
            })),
        }
    }

    /// A finite source that returns exactly `pcm`, then zero bytes.
    pub fn with_pcm(format: Uac2PcmFormat, pcm: impl Into<Vec<u8>>) -> Result<Self, UsbError> {
        format.validate()?;
        let pcm = pcm.into();
        format.bytes_to_frames(pcm.len())?;
        Ok(Self {
            state: Arc::new(Mutex::new(FakeSourceState {
                format,
                data: pcm,
                position: 0,
                infinite_silence: false,
                max_read_bytes: None,
                read_calls: 0,
                flushes: 0,
            })),
        })
    }

    /// Limit bytes returned by a single `read` call, simulating short reads.
    pub fn with_max_read_bytes(self, bytes: usize) -> Self {
        if let Ok(mut state) = self.state.lock() {
            state.max_read_bytes = Some(bytes);
        }
        self
    }

    /// Shared handle for inspection.
    pub fn handle(&self) -> FakeUac2AudioSourceHandle {
        FakeUac2AudioSourceHandle {
            state: Arc::clone(&self.state),
        }
    }

    /// Bytes remaining in a finite source.
    pub fn remaining_bytes(&self) -> usize {
        self.handle().remaining_bytes()
    }

    /// Number of `read` calls.
    pub fn read_calls(&self) -> u64 {
        self.handle().read_calls()
    }

    /// Number of `flush` calls.
    pub fn flushes(&self) -> u64 {
        self.handle().flushes()
    }
}

impl FakeUac2AudioSourceHandle {
    /// Source format.
    pub fn format(&self) -> Uac2PcmFormat {
        self.state
            .lock()
            .map(|state| state.format)
            .unwrap_or(Uac2PcmFormat {
                sample_rate: 48_000,
                channels: 1,
                sample_size: Uac2SampleSize::S16,
            })
    }

    /// Bytes remaining in a finite source (`usize::MAX` for silence).
    pub fn remaining_bytes(&self) -> usize {
        self.state
            .lock()
            .map(|state| {
                if state.infinite_silence {
                    usize::MAX
                } else {
                    state.data.len().saturating_sub(state.position)
                }
            })
            .unwrap_or(0)
    }

    /// Number of `read` calls.
    pub fn read_calls(&self) -> u64 {
        self.state.lock().map(|state| state.read_calls).unwrap_or(0)
    }

    /// Number of `flush` calls.
    pub fn flushes(&self) -> u64 {
        self.state.lock().map(|state| state.flushes).unwrap_or(0)
    }
}

impl Uac2AudioSource for FakeUac2AudioSource {
    fn format(&self) -> Uac2PcmFormat {
        self.handle().format()
    }

    fn read(&mut self, pcm: &mut [u8]) -> Result<usize, UsbError> {
        let mut state = self.state.lock().map_err(|_| UsbError::Uac2Endpoint {
            address: 0,
            operation: "source read",
            reason: "fake source state lock poisoned".to_owned(),
        })?;
        state.read_calls = state.read_calls.saturating_add(1);
        let requested = state
            .max_read_bytes
            .map(|limit| limit.min(pcm.len()))
            .unwrap_or(pcm.len());
        let requested = requested / state.format.frame_bytes() * state.format.frame_bytes();
        if state.infinite_silence {
            pcm[..requested].fill(0);
            return Ok(requested);
        }
        let available = state.data.len().saturating_sub(state.position);
        let count = available.min(requested);
        pcm[..count].copy_from_slice(&state.data[state.position..state.position + count]);
        state.position += count;
        Ok(count)
    }

    fn flush(&mut self) -> Result<(), UsbError> {
        let mut state = self.state.lock().map_err(|_| UsbError::Uac2Endpoint {
            address: 0,
            operation: "source flush",
            reason: "fake source state lock poisoned".to_owned(),
        })?;
        state.flushes = state.flushes.saturating_add(1);
        Ok(())
    }
}

/// What to do when the capture source has fewer frames than the interval asks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Uac2UnderrunPolicy {
    /// Write a full-size packet and zero-fill the missing frames.
    #[default]
    PadSilence,
    /// Write only the frames that were available, or skip the packet when the
    /// source returned nothing.
    SkipPacket,
}

/// One direction of a [`Uac2BridgeConfig`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2StreamPlan {
    /// PCM format on the USB side.
    pub format: Uac2PcmFormat,
    /// Endpoint descriptor expected for this direction.
    pub endpoint: Uac2Endpoint,
}

/// Bridge configuration, independent of any concrete USB or audio backend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2BridgeConfig {
    /// UAC2 function instance.
    pub instance: String,
    /// Playback (host → device) plan, when bridged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback: Option<Uac2StreamPlan>,
    /// Capture (device → host) plan, when bridged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<Uac2StreamPlan>,
    /// Playback ring capacity in bytes, rounded down to whole sink frames.
    pub ring_capacity_bytes: usize,
    /// Capture underrun behaviour.
    pub underrun_policy: Uac2UnderrunPolicy,
}

impl Uac2BridgeConfig {
    /// A duplex-capable bridge config with no directions yet.
    pub fn new(instance: impl Into<String>) -> Self {
        Self {
            instance: instance.into(),
            playback: None,
            capture: None,
            ring_capacity_bytes: UAC2_DEFAULT_RING_BYTES,
            underrun_policy: Uac2UnderrunPolicy::PadSilence,
        }
    }

    /// Derive plans from a validated UAC2 descriptor model.
    ///
    /// Endpoint packet sizes come from the largest advertised rate, matching
    /// the generated descriptor's `wMaxPacketSize`.
    pub fn from_uac2(instance: impl Into<String>, config: &Uac2Config) -> Result<Self, UsbError> {
        config.validate()?;
        let mut bridge = Self::new(instance);
        if config.playback_enabled() {
            let format = Uac2PcmFormat::from_uac2(config, config.playback_channels)?;
            let endpoint =
                Uac2Endpoint::playback(config.endpoint_packet_size(config.playback_channels))?;
            bridge.playback = Some(Uac2StreamPlan { format, endpoint });
        }
        if config.capture_enabled() {
            let format = Uac2PcmFormat::from_uac2(config, config.capture_channels)?;
            let endpoint =
                Uac2Endpoint::capture(config.endpoint_packet_size(config.capture_channels))?;
            bridge.capture = Some(Uac2StreamPlan { format, endpoint });
        }
        Ok(bridge)
    }

    /// Add or replace the playback plan.
    pub fn with_playback(mut self, plan: Uac2StreamPlan) -> Self {
        self.playback = Some(plan);
        self
    }

    /// Add or replace the capture plan.
    pub fn with_capture(mut self, plan: Uac2StreamPlan) -> Self {
        self.capture = Some(plan);
        self
    }

    /// Set playback ring capacity in bytes.
    pub fn with_ring_capacity_bytes(mut self, bytes: usize) -> Self {
        self.ring_capacity_bytes = bytes;
        self
    }

    /// Set the capture underrun policy.
    pub fn with_underrun_policy(mut self, policy: Uac2UnderrunPolicy) -> Self {
        self.underrun_policy = policy;
        self
    }

    /// Validate the instance, plans, and ring size.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.instance.trim().is_empty() {
            return Err(UsbError::InvalidField {
                field: "uac2.bridge.instance",
                reason: "bridge instance must not be empty".to_owned(),
            });
        }
        if self.playback.is_none() && self.capture.is_none() {
            return Err(UsbError::Uac2MissingPath {
                instance: self.instance.clone(),
                path: "playback/capture",
            });
        }
        if self.ring_capacity_bytes == 0 {
            return Err(UsbError::Uac2Overflow {
                buffer: "playback ring",
                limit: 0,
                requested: 1,
            });
        }
        if let Some(plan) = self.playback {
            validate_plan(&self.instance, "playback", plan)?;
        }
        if let Some(plan) = self.capture {
            validate_plan(&self.instance, "capture", plan)?;
        }
        Ok(())
    }
}

fn validate_plan(instance: &str, path: &'static str, plan: Uac2StreamPlan) -> Result<(), UsbError> {
    plan.format.validate()?;
    plan.endpoint.validate()?;
    let expected_direction = if path == "playback" {
        Uac2Direction::Out
    } else {
        Uac2Direction::In
    };
    if plan.endpoint.direction != expected_direction {
        return Err(UsbError::Uac2MissingPath {
            instance: instance.to_owned(),
            path,
        });
    }
    let packetizer = Uac2Packetizer::new(plan.format, plan.endpoint.interval)?;
    if packetizer.max_packet_bytes() > usize::from(plan.endpoint.max_packet_size) {
        return Err(UsbError::Uac2Endpoint {
            address: plan.endpoint.address,
            operation: "configure",
            reason: format!(
                "{} packet can reach {} bytes but wMaxPacketSize is {}",
                path,
                packetizer.max_packet_bytes(),
                plan.endpoint.max_packet_size
            ),
        });
    }
    Ok(())
}

/// Endpoints and audio path injected into a [`Uac2Bridge`].
#[derive(Default)]
pub struct Uac2BridgeInputs {
    /// Playback OUT endpoint (host → device).
    pub playback_endpoint: Option<Box<dyn Uac2EndpointIo>>,
    /// Playback audio sink.
    pub playback_sink: Option<Box<dyn Uac2AudioSink>>,
    /// Capture IN endpoint (device → host).
    pub capture_endpoint: Option<Box<dyn Uac2EndpointIo>>,
    /// Capture audio source.
    pub capture_source: Option<Box<dyn Uac2AudioSource>>,
}

impl std::fmt::Debug for Uac2BridgeInputs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Uac2BridgeInputs")
            .field("playback_endpoint", &self.playback_endpoint.is_some())
            .field("playback_sink", &self.playback_sink.is_some())
            .field("capture_endpoint", &self.capture_endpoint.is_some())
            .field("capture_source", &self.capture_source.is_some())
            .finish()
    }
}

/// Counters reported by a running bridge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2BridgeCounters {
    /// USB OUT packets read for playback.
    pub playback_packets_in: u64,
    /// USB OUT payload bytes read for playback.
    pub playback_bytes_in: u64,
    /// Sample frames read from USB OUT.
    pub playback_frames_in: u64,
    /// Playback PCM bytes dropped because the ring was full.
    pub playback_dropped_bytes: u64,
    /// Whole playback frames dropped.
    pub playback_dropped_frames: u64,
    /// Playback ring overflow events.
    pub playback_overflow_events: u64,
    /// Sink write calls that made no progress (backpressure).
    pub sink_backpressure: u64,
    /// Bytes accepted by the audio sink.
    pub sink_bytes: u64,
    /// Frames accepted by the audio sink.
    pub sink_frames: u64,
    /// Capture packets written to USB IN.
    pub capture_packets_out: u64,
    /// Capture payload bytes written to USB IN.
    pub capture_bytes_out: u64,
    /// Sample frames written to USB IN.
    pub capture_frames_out: u64,
    /// Capture intervals where the source produced fewer frames than asked.
    pub capture_short_packets: u64,
    /// Zero bytes added to pad short capture packets.
    pub capture_padded_bytes: u64,
    /// Capture intervals where the source returned no frames.
    pub capture_underruns: u64,
    /// Zero-length USB OUT packets observed.
    pub empty_packets: u64,
    /// Bridge/endpoint/audio errors observed.
    pub errors: u64,
}

impl Uac2BridgeCounters {
    /// Total PCM bytes that moved in either direction.
    pub fn data_bytes(self) -> u64 {
        self.playback_bytes_in
            .saturating_add(self.capture_bytes_out)
            .saturating_add(self.sink_bytes)
    }

    /// Whether any PCM has moved since the counters were created.
    pub fn has_data(self) -> bool {
        self.data_bytes() > 0
    }
}

/// Per-direction detail in a bridge status report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2BridgeStreamReport {
    /// Endpoint address.
    pub endpoint: u8,
    /// Endpoint direction.
    pub direction: Uac2Direction,
    /// USB-side PCM format.
    pub format: Uac2PcmFormat,
    /// `bInterval` exponent.
    pub packet_interval: u8,
    /// Endpoint `wMaxPacketSize`.
    pub max_packet_size: u16,
    /// Largest packet the sample-accurate packetizer can emit.
    pub maximum_packet_bytes: usize,
}

/// Bridge status detail embedded in [`Uac2Status`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2BridgeReport {
    /// Configured/running/stopped/unsupported state.
    pub status: Uac2BridgeStatus,
    /// Playback stream, when bridged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback: Option<Uac2BridgeStreamReport>,
    /// Capture stream, when bridged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<Uac2BridgeStreamReport>,
    /// Cumulative counters.
    pub counters: Uac2BridgeCounters,
    /// Directions that could not be bridged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_paths: Vec<String>,
    /// Typed reason for an unsupported bridge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Uac2BridgeReport {
    /// A report that never claims live audio.
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self {
            status: Uac2BridgeStatus::Unsupported,
            playback: None,
            capture: None,
            counters: Uac2BridgeCounters::default(),
            missing_paths: Vec::new(),
            reason: Some(reason.into()),
        }
    }

    /// Whether PCM actually moved in this report.
    pub fn is_live(&self) -> bool {
        self.status.is_live() && self.counters.has_data()
    }
}

/// Result of servicing one bridge direction once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2Transfer {
    /// Packets transferred (zero or one).
    pub packets: usize,
    /// Payload bytes transferred.
    pub bytes: usize,
    /// Playback bytes dropped during this call.
    pub dropped_bytes: u64,
    /// Capture produced fewer frames than the interval asked.
    pub short: bool,
    /// Capture produced no frames this interval.
    pub underrun: bool,
}

/// Result of one duplex [`Uac2Bridge::pump`] cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2PumpOutcome {
    /// Playback service result, when the direction is configured.
    pub playback: Option<Uac2Transfer>,
    /// Capture service result, when the direction is configured.
    pub capture: Option<Uac2Transfer>,
}

struct PlaybackPath {
    plan: Uac2StreamPlan,
    endpoint: Box<dyn Uac2EndpointIo>,
    sink: Box<dyn Uac2AudioSink>,
    sink_format: Uac2PcmFormat,
    ring: PcmRing,
    packetizer: Uac2Packetizer,
}

struct CapturePath {
    plan: Uac2StreamPlan,
    endpoint: Box<dyn Uac2EndpointIo>,
    source: Box<dyn Uac2AudioSource>,
    source_format: Uac2PcmFormat,
    packetizer: Uac2Packetizer,
}

/// Transport-independent UAC2 isochronous ↔ PCM bridge.
pub struct Uac2Bridge {
    instance: String,
    playback: Option<PlaybackPath>,
    capture: Option<CapturePath>,
    counters: Uac2BridgeCounters,
    underrun_policy: Uac2UnderrunPolicy,
    running: bool,
    stopped: bool,
    moved_since_start: bool,
}

impl std::fmt::Debug for Uac2Bridge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Uac2Bridge")
            .field("instance", &self.instance)
            .field("playback", &self.playback.is_some())
            .field("capture", &self.capture.is_some())
            .field("running", &self.running)
            .field("stopped", &self.stopped)
            .field("counters", &self.counters)
            .finish()
    }
}

impl Uac2Bridge {
    /// Build a bridge from a configuration and injected endpoint/audio paths.
    ///
    /// Every configured direction must have both an endpoint and an audio
    /// path. A missing path is a typed [`UsbError::Uac2MissingPath`]; this
    /// constructor never creates a silent fake.
    pub fn new(config: Uac2BridgeConfig, inputs: Uac2BridgeInputs) -> Result<Self, UsbError> {
        config.validate()?;
        let instance = config.instance.clone();
        let Uac2BridgeInputs {
            playback_endpoint,
            playback_sink,
            capture_endpoint,
            capture_source,
        } = inputs;

        let playback = match config.playback {
            Some(plan) => {
                let endpoint = playback_endpoint.ok_or_else(|| UsbError::Uac2MissingPath {
                    instance: instance.clone(),
                    path: "playback endpoint",
                })?;
                let sink = playback_sink.ok_or_else(|| UsbError::Uac2MissingPath {
                    instance: instance.clone(),
                    path: "playback audio",
                })?;
                let packetizer = Uac2Packetizer::new(plan.format, plan.endpoint.interval)?;
                let descriptor = endpoint.endpoint();
                validate_endpoint(
                    &instance,
                    "playback",
                    plan.endpoint,
                    descriptor,
                    &packetizer,
                )?;
                let sink_format = sink.format();
                sink_format.validate()?;
                if sink_format.sample_rate != plan.format.sample_rate {
                    return Err(UsbError::Uac2Format {
                        reason: format!(
                            "playback USB rate {} Hz does not match sink rate {} Hz; resampling is not implicit",
                            plan.format.sample_rate, sink_format.sample_rate
                        ),
                    });
                }
                let ring = PcmRing::new(config.ring_capacity_bytes, sink_format.frame_bytes())?;
                Some(PlaybackPath {
                    plan,
                    endpoint,
                    sink,
                    sink_format,
                    ring,
                    packetizer,
                })
            }
            None => {
                if playback_endpoint.is_some() || playback_sink.is_some() {
                    return Err(UsbError::Uac2Endpoint {
                        address: UAC2_PLAYBACK_ENDPOINT,
                        operation: "configure",
                        reason: "playback endpoint/audio supplied without a playback plan"
                            .to_owned(),
                    });
                }
                None
            }
        };

        let capture = match config.capture {
            Some(plan) => {
                let endpoint = capture_endpoint.ok_or_else(|| UsbError::Uac2MissingPath {
                    instance: instance.clone(),
                    path: "capture endpoint",
                })?;
                let source = capture_source.ok_or_else(|| UsbError::Uac2MissingPath {
                    instance: instance.clone(),
                    path: "capture audio",
                })?;
                let packetizer = Uac2Packetizer::new(plan.format, plan.endpoint.interval)?;
                let descriptor = endpoint.endpoint();
                validate_endpoint(&instance, "capture", plan.endpoint, descriptor, &packetizer)?;
                let source_format = source.format();
                source_format.validate()?;
                if source_format.sample_rate != plan.format.sample_rate {
                    return Err(UsbError::Uac2Format {
                        reason: format!(
                            "capture USB rate {} Hz does not match source rate {} Hz; resampling is not implicit",
                            plan.format.sample_rate, source_format.sample_rate
                        ),
                    });
                }
                Some(CapturePath {
                    plan,
                    endpoint,
                    source,
                    source_format,
                    packetizer,
                })
            }
            None => {
                if capture_endpoint.is_some() || capture_source.is_some() {
                    return Err(UsbError::Uac2Endpoint {
                        address: UAC2_CAPTURE_ENDPOINT,
                        operation: "configure",
                        reason: "capture endpoint/audio supplied without a capture plan".to_owned(),
                    });
                }
                None
            }
        };

        Ok(Self {
            instance,
            playback,
            capture,
            counters: Uac2BridgeCounters::default(),
            underrun_policy: config.underrun_policy,
            running: false,
            stopped: false,
            moved_since_start: false,
        })
    }

    /// Function instance.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// Whether a playback path is wired.
    pub fn has_playback(&self) -> bool {
        self.playback.is_some()
    }

    /// Whether a capture path is wired.
    pub fn has_capture(&self) -> bool {
        self.capture.is_some()
    }

    /// Whether [`Uac2Bridge::start`] has been called and no stop is pending.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Whether PCM has moved since the last start or flush.
    pub fn has_moved_data(&self) -> bool {
        self.moved_since_start
    }

    /// Cumulative counters.
    pub fn counters(&self) -> Uac2BridgeCounters {
        self.counters
    }

    /// Start servicing endpoints. Idempotent.
    pub fn start(&mut self) -> Result<(), UsbError> {
        if self.running {
            return Ok(());
        }
        self.running = true;
        self.stopped = false;
        self.moved_since_start = false;
        if let Some(capture) = &mut self.capture {
            capture.packetizer.reset();
        }
        Ok(())
    }

    /// Stop servicing and flush audio/endpoint state.
    ///
    /// Buffered playback PCM is retained until [`Uac2Bridge::flush`] so a
    /// subsequent start can choose to drain or discard it.
    pub fn stop(&mut self) -> Result<(), UsbError> {
        self.running = false;
        self.stopped = true;
        let mut result = Ok(());
        if let Some(playback) = &mut self.playback {
            result = result.and(playback.sink.flush());
            result = result.and(playback.endpoint.flush());
        }
        if let Some(capture) = &mut self.capture {
            result = result.and(capture.source.flush());
            result = result.and(capture.endpoint.flush());
        }
        result
    }

    /// Drop buffered playback PCM, reset capture packetisation, and flush I/O.
    ///
    /// Counters are cumulative and are not reset. The report stops claiming
    /// live audio until a new transfer succeeds.
    pub fn flush(&mut self) -> Result<(), UsbError> {
        self.moved_since_start = false;
        let mut result = Ok(());
        if let Some(playback) = &mut self.playback {
            let dropped = playback.ring.available();
            playback.ring.clear();
            playback.packetizer.reset();
            self.counters.playback_dropped_bytes = self
                .counters
                .playback_dropped_bytes
                .saturating_add(dropped as u64);
            self.counters.playback_dropped_frames = self
                .counters
                .playback_dropped_frames
                .saturating_add((dropped / playback.sink_format.frame_bytes()) as u64);
            result = result.and(playback.sink.flush());
            result = result.and(playback.endpoint.flush());
        }
        if let Some(capture) = &mut self.capture {
            capture.packetizer.reset();
            result = result.and(capture.source.flush());
            result = result.and(capture.endpoint.flush());
        }
        result
    }

    /// Read and deliver at most one USB OUT playback packet.
    pub fn poll_playback(&mut self) -> Result<Uac2Transfer, UsbError> {
        self.ensure_running()?;
        match self.poll_playback_impl() {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                self.counters.errors = self.counters.errors.saturating_add(1);
                Err(error)
            }
        }
    }

    /// Produce at most one USB IN capture packet.
    pub fn poll_capture(&mut self) -> Result<Uac2Transfer, UsbError> {
        self.ensure_running()?;
        match self.poll_capture_impl() {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                self.counters.errors = self.counters.errors.saturating_add(1);
                Err(error)
            }
        }
    }

    /// Service both configured directions once.
    pub fn pump(&mut self) -> Result<Uac2PumpOutcome, UsbError> {
        self.ensure_running()?;
        match self.pump_impl() {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                self.counters.errors = self.counters.errors.saturating_add(1);
                Err(error)
            }
        }
    }

    fn pump_impl(&mut self) -> Result<Uac2PumpOutcome, UsbError> {
        let playback = if self.playback.is_some() {
            Some(self.poll_playback_impl()?)
        } else {
            None
        };
        let capture = if self.capture.is_some() {
            Some(self.poll_capture_impl()?)
        } else {
            None
        };
        Ok(Uac2PumpOutcome { playback, capture })
    }

    /// Current status report. `Running` requires a transfer or sink write since
    /// the last start/flush.
    pub fn report(&self) -> Uac2BridgeReport {
        let configured = self.playback.is_some() || self.capture.is_some();
        let status = if !configured {
            Uac2BridgeStatus::Unsupported
        } else if self.running {
            if self.moved_since_start {
                Uac2BridgeStatus::Running
            } else {
                Uac2BridgeStatus::Configured
            }
        } else if self.stopped && self.moved_since_start {
            Uac2BridgeStatus::Stopped
        } else {
            Uac2BridgeStatus::Configured
        };
        Uac2BridgeReport {
            status,
            playback: self.playback.as_ref().map(stream_report),
            capture: self.capture.as_ref().map(stream_report),
            counters: self.counters,
            missing_paths: Vec::new(),
            reason: (!configured).then(|| "no playback or capture path configured".to_owned()),
        }
    }

    /// Build a [`Uac2Status`] carrying this bridge report.
    pub fn status(&self, instance: impl Into<String>, config: &Uac2Config) -> Uac2Status {
        Uac2Status::from_config(instance, config).with_bridge_report(self.report())
    }

    fn ensure_running(&self) -> Result<(), UsbError> {
        if self.running {
            Ok(())
        } else {
            Err(UsbError::Uac2NotRunning {
                instance: self.instance.clone(),
            })
        }
    }

    fn poll_playback_impl(&mut self) -> Result<Uac2Transfer, UsbError> {
        let Some(playback) = &mut self.playback else {
            return Err(UsbError::Uac2MissingPath {
                instance: self.instance.clone(),
                path: "playback",
            });
        };
        let max_packet = usize::from(playback.plan.endpoint.max_packet_size);
        let mut packet = vec![0u8; max_packet];
        let read = playback.endpoint.read_packet(&mut packet)?;
        if read > max_packet {
            return Err(UsbError::Uac2Overflow {
                buffer: "endpoint read",
                limit: max_packet,
                requested: read,
            });
        }
        if read == 0 {
            self.counters.empty_packets = self.counters.empty_packets.saturating_add(1);
            let drained = drain_playback(playback, &mut self.counters)?;
            if drained > 0 {
                self.moved_since_start = true;
            }
            return Ok(Uac2Transfer::default());
        }

        let usb_frames = playback.plan.format.bytes_to_frames(read)?;
        let pcm = convert_pcm(&packet[..read], playback.plan.format, playback.sink_format)?;
        self.counters.playback_packets_in = self.counters.playback_packets_in.saturating_add(1);
        self.counters.playback_bytes_in =
            self.counters.playback_bytes_in.saturating_add(read as u64);
        self.counters.playback_frames_in = self
            .counters
            .playback_frames_in
            .saturating_add(usb_frames as u64);

        let pushed = playback.ring.push(&pcm)?;
        if pushed.overflowed {
            self.counters.playback_overflow_events =
                self.counters.playback_overflow_events.saturating_add(1);
        }
        self.counters.playback_dropped_bytes = self
            .counters
            .playback_dropped_bytes
            .saturating_add(pushed.dropped as u64);
        self.counters.playback_dropped_frames = self
            .counters
            .playback_dropped_frames
            .saturating_add((pushed.dropped / playback.sink_format.frame_bytes()) as u64);

        let drained = drain_playback(playback, &mut self.counters)?;

        if read > 0 || pushed.written > 0 || drained > 0 {
            self.moved_since_start = true;
        }
        Ok(Uac2Transfer {
            packets: 1,
            bytes: read,
            dropped_bytes: pushed.dropped as u64,
            short: false,
            underrun: false,
        })
    }

    fn poll_capture_impl(&mut self) -> Result<Uac2Transfer, UsbError> {
        let Some(capture) = &mut self.capture else {
            return Err(UsbError::Uac2MissingPath {
                instance: self.instance.clone(),
                path: "capture",
            });
        };
        let spec = capture.packetizer.peek_next();
        if spec.bytes == 0 {
            capture.packetizer.advance();
            return Ok(Uac2Transfer::default());
        }
        let source_bytes = capture.source_format.frames_to_bytes(spec.frames)?;
        let mut source = vec![0u8; source_bytes];
        let read = capture.source.read(&mut source)?;
        if read > source_bytes {
            return Err(UsbError::Uac2Overflow {
                buffer: "capture source read",
                limit: source_bytes,
                requested: read,
            });
        }
        let available_frames = capture.source_format.bytes_to_frames(read)?;
        let mut pcm = convert_pcm(&source[..read], capture.source_format, capture.plan.format)?;
        let short = available_frames < spec.frames;
        let mut underrun = false;
        if short {
            self.counters.capture_short_packets =
                self.counters.capture_short_packets.saturating_add(1);
            if available_frames == 0 {
                underrun = true;
                self.counters.capture_underruns = self.counters.capture_underruns.saturating_add(1);
            }
            match self.underrun_policy {
                Uac2UnderrunPolicy::PadSilence => {
                    if pcm.len() < spec.bytes {
                        let padded = spec.bytes - pcm.len();
                        pcm.resize(spec.bytes, 0);
                        self.counters.capture_padded_bytes = self
                            .counters
                            .capture_padded_bytes
                            .saturating_add(padded as u64);
                    }
                }
                Uac2UnderrunPolicy::SkipPacket => {
                    if pcm.is_empty() {
                        capture.packetizer.advance();
                        return Ok(Uac2Transfer {
                            packets: 0,
                            bytes: 0,
                            dropped_bytes: 0,
                            short: true,
                            underrun: true,
                        });
                    }
                }
            }
        }

        let written = capture.endpoint.write_packet(&pcm)?;
        if written != pcm.len() {
            return Err(UsbError::Uac2Endpoint {
                address: capture.plan.endpoint.address,
                operation: "write",
                reason: format!("short isochronous write: {written} of {} bytes", pcm.len()),
            });
        }
        capture.packetizer.advance();
        self.counters.capture_packets_out = self.counters.capture_packets_out.saturating_add(1);
        self.counters.capture_bytes_out = self
            .counters
            .capture_bytes_out
            .saturating_add(pcm.len() as u64);
        self.counters.capture_frames_out = self
            .counters
            .capture_frames_out
            .saturating_add((pcm.len() / capture.plan.format.frame_bytes()) as u64);
        if !pcm.is_empty() {
            self.moved_since_start = true;
        }
        Ok(Uac2Transfer {
            packets: 1,
            bytes: pcm.len(),
            dropped_bytes: 0,
            short,
            underrun,
        })
    }
}

fn drain_playback(
    playback: &mut PlaybackPath,
    counters: &mut Uac2BridgeCounters,
) -> Result<usize, UsbError> {
    let mut drained = 0usize;
    while !playback.ring.is_empty() {
        let chunk_len = playback.ring.available();
        let chunk = playback.ring.peek(chunk_len);
        let offered = chunk.len();
        let written = playback.sink.write(chunk)?;
        if written > offered {
            return Err(UsbError::Uac2Endpoint {
                address: playback.plan.endpoint.address,
                operation: "sink write",
                reason: format!("sink accepted {written} of {offered} offered bytes"),
            });
        }
        if written == 0 {
            counters.sink_backpressure = counters.sink_backpressure.saturating_add(1);
            break;
        }
        if written % playback.sink_format.frame_bytes() != 0 {
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "sink accepted {written} bytes, not whole {}-byte frames",
                    playback.sink_format.frame_bytes()
                ),
            });
        }
        playback.ring.consume(written);
        counters.sink_bytes = counters.sink_bytes.saturating_add(written as u64);
        counters.sink_frames = counters
            .sink_frames
            .saturating_add((written / playback.sink_format.frame_bytes()) as u64);
        drained = drained.saturating_add(written);
        if written < offered {
            counters.sink_backpressure = counters.sink_backpressure.saturating_add(1);
            break;
        }
    }
    Ok(drained)
}

fn validate_endpoint(
    instance: &str,
    path: &'static str,
    plan: Uac2Endpoint,
    descriptor: Uac2Endpoint,
    packetizer: &Uac2Packetizer,
) -> Result<(), UsbError> {
    descriptor.validate()?;
    if descriptor.address != plan.address || descriptor.direction != plan.direction {
        return Err(UsbError::Uac2Endpoint {
            address: descriptor.address,
            operation: "configure",
            reason: format!(
                "{path} endpoint 0x{:02x} {} does not match plan 0x{:02x} {}",
                descriptor.address,
                descriptor.direction.as_str(),
                plan.address,
                plan.direction.as_str()
            ),
        });
    }
    if descriptor.max_packet_size < plan.max_packet_size {
        return Err(UsbError::Uac2Endpoint {
            address: descriptor.address,
            operation: "configure",
            reason: format!(
                "{path} endpoint wMaxPacketSize {} is smaller than the plan's {}",
                descriptor.max_packet_size, plan.max_packet_size
            ),
        });
    }
    if packetizer.max_packet_bytes() > usize::from(descriptor.max_packet_size) {
        return Err(UsbError::Uac2Endpoint {
            address: descriptor.address,
            operation: "configure",
            reason: format!(
                "{instance} {path} packet can reach {} bytes but wMaxPacketSize is {}",
                packetizer.max_packet_bytes(),
                descriptor.max_packet_size
            ),
        });
    }
    Ok(())
}

fn stream_report(path: &impl StreamPlanView) -> Uac2BridgeStreamReport {
    let plan = path.plan();
    Uac2BridgeStreamReport {
        endpoint: plan.endpoint.address,
        direction: plan.endpoint.direction,
        format: plan.format,
        packet_interval: plan.endpoint.interval,
        max_packet_size: plan.endpoint.max_packet_size,
        maximum_packet_bytes: path.packetizer().max_packet_bytes(),
    }
}

trait StreamPlanView {
    fn plan(&self) -> Uac2StreamPlan;
    fn packetizer(&self) -> &Uac2Packetizer;
}

impl StreamPlanView for PlaybackPath {
    fn plan(&self) -> Uac2StreamPlan {
        self.plan
    }

    fn packetizer(&self) -> &Uac2Packetizer {
        &self.packetizer
    }
}

impl StreamPlanView for CapturePath {
    fn plan(&self) -> Uac2StreamPlan {
        self.plan
    }

    fn packetizer(&self) -> &Uac2Packetizer {
        &self.packetizer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(rate: u32, channels: u8, size: Uac2SampleSize) -> Uac2PcmFormat {
        Uac2PcmFormat::new(rate, channels, size).unwrap()
    }

    #[test]
    fn packetizer_is_sample_accurate_for_44100() {
        let mut packetizer =
            Uac2Packetizer::new(format(44_100, 2, Uac2SampleSize::S16), 1).unwrap();
        let mut frames = 0usize;
        for _ in 0..8_000 {
            frames += packetizer.advance().frames;
        }
        assert_eq!(frames, 44_100);
        assert_eq!(packetizer.accumulator(), 0);
    }

    #[test]
    fn packetizer_sizes_common_rates() {
        for rate in [8_000u32, 16_000, 48_000, 96_000, 192_000] {
            let mut packetizer =
                Uac2Packetizer::new(format(rate, 2, Uac2SampleSize::S16), 1).unwrap();
            let spec = packetizer.advance();
            assert_eq!(spec.frames, (rate / 8_000) as usize);
            assert_eq!(spec.bytes, spec.frames * 4);
        }
    }

    #[test]
    fn packetizer_max_packet_covers_eight_channels_32_bit() {
        let packetizer = Uac2Packetizer::new(format(192_000, 8, Uac2SampleSize::S32), 1).unwrap();
        assert_eq!(packetizer.max_packet_bytes(), 24 * 8 * 4);
        assert!(packetizer.max_packet_bytes() <= UAC2_MAX_PACKET_BYTES);
    }

    #[test]
    fn conversion_scales_bit_depths_both_ways() {
        let s16 = format(48_000, 2, Uac2SampleSize::S16);
        let s24 = format(48_000, 2, Uac2SampleSize::S24);
        let s32 = format(48_000, 2, Uac2SampleSize::S32);
        let input = [0x00, 0x40, 0x00, 0xc0]; // 16384, -16384 stereo
        let wide = convert_pcm(&input, s16, s32).unwrap();
        assert_eq!(&wide[..4], &(16_384i32 << 16).to_le_bytes());
        let back = convert_pcm(&wide, s32, s16).unwrap();
        assert_eq!(back, input);
        let packed = convert_pcm(&input, s16, s24).unwrap();
        assert_eq!(packed.len(), 6);
        let back = convert_pcm(&packed, s24, s16).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn conversion_replicates_mono_and_averages_downmix() {
        let mono = format(48_000, 1, Uac2SampleSize::S16);
        let stereo = format(48_000, 2, Uac2SampleSize::S16);
        let mono_bytes = [0x10, 0x00];
        let stereo_bytes = convert_pcm(&mono_bytes, mono, stereo).unwrap();
        assert_eq!(stereo_bytes, [0x10, 0x00, 0x10, 0x00]);
        let down = convert_pcm(&[0x10, 0x00, 0x20, 0x00], stereo, mono).unwrap();
        assert_eq!(down, [0x18, 0x00]);
    }

    #[test]
    fn conversion_rejects_resampling() {
        let error = convert_pcm(
            &[0; 4],
            format(48_000, 2, Uac2SampleSize::S16),
            format(44_100, 2, Uac2SampleSize::S16),
        )
        .unwrap_err();
        assert_eq!(error.kind(), "uac2-format");
    }

    #[test]
    fn invalid_formats_are_typed() {
        assert_eq!(
            Uac2PcmFormat::new(48_000, 0, Uac2SampleSize::S16)
                .unwrap_err()
                .kind(),
            "uac2-format"
        );
        assert_eq!(
            Uac2PcmFormat::new(48_000, 9, Uac2SampleSize::S16)
                .unwrap_err()
                .kind(),
            "uac2-format"
        );
        assert_eq!(
            Uac2PcmFormat::new(7_999, 2, Uac2SampleSize::S16)
                .unwrap_err()
                .kind(),
            "uac2-format"
        );
        assert_eq!(
            Uac2PcmFormat::new(192_001, 2, Uac2SampleSize::S16)
                .unwrap_err()
                .kind(),
            "uac2-format"
        );
        assert_eq!(
            Uac2Endpoint::new(0x01, Uac2Direction::In, 64, 1)
                .unwrap_err()
                .kind(),
            "uac2-endpoint"
        );
        assert_eq!(
            Uac2Endpoint::new(0x01, Uac2Direction::Out, 1025, 1)
                .unwrap_err()
                .kind(),
            "uac2-endpoint"
        );
    }
}
