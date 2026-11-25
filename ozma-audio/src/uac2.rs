// SPDX-License-Identifier: AGPL-3.0-only
//! Real ALSA and PipeWire adapters for the UAC2 isochronous bridge.
//!
//! The bridge core in `ozma-usb` deliberately knows nothing about sound
//! servers. This module supplies the two traits it needs:
//!
//! * [`AlsaUac2Sink`] consumes host→device (USB OUT) playback PCM into an ALSA
//!   playback PCM, or a [`PipeWireUac2Sink`] when the graph is the better
//!   target.
//! * [`AlsaUac2Source`] pulls device→host (USB IN) capture PCM from an ALSA
//!   capture PCM, or a [`PipeWireUac2Source`] when a graph source is already
//!   open.
//!
//! Both directions use the bridge's [`Uac2PcmFormat`] verbatim. The ALSA
//! device is asked for that exact rate, channel count, and packed sample width;
//! a nearest-match negotiation is rejected as [`UsbError::Uac2Format`] instead
//! of silently resampling or reinterpreting samples. PipeWire only delivers
//! interleaved `i16`, so the PipeWire adapters accept `S16` formats and reject
//! anything wider.
//!
//! # Non-blocking and bounded
//!
//! ALSA PCMs are opened in non-blocking mode. `EAGAIN` is reported as a zero
//! byte transfer, which the bridge already treats as backpressure (sink) or an
//! underrun (source). Short writes and reads are returned as-is and counted in
//! the adapter statistics handle. Errors never turn into a silent success.
//!
//! The [`Uac2PcmWriter`]/[`Uac2PcmReader`] traits and their factories are
//! public so default tests can exercise every adapter path with deterministic
//! fake devices. Real ALSA access is confined to `AlsaPcmFactory` and the
//! env-gated integration test.

use std::sync::{Arc, Mutex, MutexGuard};

use alsa::{
    pcm::{Access, Format, Frames, HwParams, PCM},
    Direction, ValueOr,
};
use ozma_usb::{
    Uac2AudioSink, Uac2AudioSource, Uac2PcmFormat, Uac2SampleSize, UsbError, UAC2_CAPTURE_ENDPOINT,
    UAC2_PLAYBACK_ENDPOINT,
};
use serde::{Deserialize, Serialize};

/// Default ALSA period in frames (about 21 ms at 48 kHz).
pub const UAC2_DEFAULT_PERIOD_FRAMES: u32 = 1_024;
/// Default ALSA ring length in periods.
pub const UAC2_DEFAULT_BUFFER_PERIODS: u32 = 4;

/// Configuration for an ALSA-backed UAC2 sink or source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlsaUac2Config {
    /// ALSA PCM name, for example `default`, `hw:1,0`, or `plug:default`.
    pub device: String,
    /// Exact interleaved PCM format the bridge requires.
    pub format: Uac2PcmFormat,
    /// ALSA period size in frames.
    pub period_frames: u32,
    /// ALSA ring buffer length in periods.
    pub buffer_periods: u32,
}

impl AlsaUac2Config {
    /// Config for an exact format.
    pub fn new(device: impl Into<String>, format: Uac2PcmFormat) -> Self {
        Self {
            device: device.into(),
            format,
            period_frames: UAC2_DEFAULT_PERIOD_FRAMES,
            buffer_periods: UAC2_DEFAULT_BUFFER_PERIODS,
        }
    }

    /// Config for a device at the canonical 48 kHz stereo S16 format.
    pub fn for_device(device: impl Into<String>) -> Self {
        Self::new(
            device,
            Uac2PcmFormat {
                sample_rate: 48_000,
                channels: 2,
                sample_size: Uac2SampleSize::S16,
            },
        )
    }

    /// Validate the device name, format, and period sizes.
    pub fn validate(&self) -> Result<(), UsbError> {
        self.format.validate()?;
        if self.device.trim().is_empty() {
            return Err(UsbError::Uac2Endpoint {
                address: UAC2_PLAYBACK_ENDPOINT,
                operation: "open",
                reason: "ALSA device name must not be empty".to_owned(),
            });
        }
        if self.period_frames == 0 || self.buffer_periods == 0 {
            return Err(UsbError::Uac2Format {
                reason: "ALSA period and buffer periods must be non-zero".to_owned(),
            });
        }
        Ok(())
    }
}

/// Counters maintained by a UAC2 playback sink adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2SinkStats {
    /// Calls into the underlying writer.
    pub write_calls: u64,
    /// PCM bytes accepted by the device.
    pub bytes_written: u64,
    /// Whole interleaved frames accepted by the device.
    pub frames_written: u64,
    /// Writes where the device accepted fewer bytes than offered.
    pub short_writes: u64,
    /// Writes that made no progress (`EAGAIN`, full queue).
    pub backpressure: u64,
    /// Device/format errors observed by the adapter.
    pub device_errors: u64,
    /// Successful flushes.
    pub flushes: u64,
}

/// Counters maintained by a UAC2 capture source adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2SourceStats {
    /// Calls into the underlying reader.
    pub read_calls: u64,
    /// PCM bytes produced by the device.
    pub bytes_read: u64,
    /// Whole interleaved frames produced by the device.
    pub frames_read: u64,
    /// Reads where the device produced fewer bytes than requested.
    pub short_reads: u64,
    /// Reads that made no progress (`EAGAIN`, no PipeWire frame).
    pub underruns: u64,
    /// Device/format errors observed by the adapter.
    pub device_errors: u64,
    /// Successful flushes.
    pub flushes: u64,
}

/// Shared snapshot handle for a [`AlsaUac2Sink`].
#[derive(Clone, Debug, Default)]
pub struct Uac2SinkHandle {
    stats: Arc<Mutex<Uac2SinkStats>>,
}

impl Uac2SinkHandle {
    /// Current counters.
    pub fn stats(&self) -> Uac2SinkStats {
        *lock(&self.stats)
    }
}

/// Shared snapshot handle for a [`AlsaUac2Source`].
#[derive(Clone, Debug, Default)]
pub struct Uac2SourceHandle {
    stats: Arc<Mutex<Uac2SourceStats>>,
}

impl Uac2SourceHandle {
    /// Current counters.
    pub fn stats(&self) -> Uac2SourceStats {
        *lock(&self.stats)
    }
}

/// Byte-oriented playback device used by [`AlsaUac2Sink`].
///
/// A returned length must be a whole number of interleaved frames. Returning
/// `Ok(0)` is backpressure, not success.
pub trait Uac2PcmWriter: Send {
    /// Write interleaved PCM, returning the accepted byte count.
    fn write(&mut self, pcm: &[u8]) -> Result<usize, UsbError>;
    /// Flush/reset the device. Must be idempotent.
    fn flush(&mut self) -> Result<(), UsbError>;
    /// Human-readable device label for errors and logs.
    fn label(&self) -> &str;
}

/// Byte-oriented capture device used by [`AlsaUac2Source`].
pub trait Uac2PcmReader: Send {
    /// Read up to `pcm.len()` bytes, returning the produced count.
    fn read(&mut self, pcm: &mut [u8]) -> Result<usize, UsbError>;
    /// Flush/reset the device. Must be idempotent.
    fn flush(&mut self) -> Result<(), UsbError>;
    /// Human-readable device label for errors and logs.
    fn label(&self) -> &str;
}

/// Factory for ALSA playback PCMs.
pub trait Uac2PcmWriterFactory: Send + Sync {
    /// Open a playback device.
    fn open(&self, config: &AlsaUac2Config) -> Result<Box<dyn Uac2PcmWriter>, UsbError>;
}

/// Factory for ALSA capture PCMs.
pub trait Uac2PcmReaderFactory: Send + Sync {
    /// Open a capture device.
    fn open(&self, config: &AlsaUac2Config) -> Result<Box<dyn Uac2PcmReader>, UsbError>;
}

/// The production ALSA factory.
#[derive(Clone, Copy, Debug, Default)]
pub struct AlsaPcmFactory;

impl Uac2PcmWriterFactory for AlsaPcmFactory {
    fn open(&self, config: &AlsaUac2Config) -> Result<Box<dyn Uac2PcmWriter>, UsbError> {
        AlsaPcmWriter::open(config).map(|writer| Box::new(writer) as Box<dyn Uac2PcmWriter>)
    }
}

impl Uac2PcmReaderFactory for AlsaPcmFactory {
    fn open(&self, config: &AlsaUac2Config) -> Result<Box<dyn Uac2PcmReader>, UsbError> {
        AlsaPcmReader::open(config).map(|reader| Box::new(reader) as Box<dyn Uac2PcmReader>)
    }
}

/// ALSA playback adapter implementing [`Uac2AudioSink`].
pub struct AlsaUac2Sink {
    format: Uac2PcmFormat,
    io: Box<dyn Uac2PcmWriter>,
    stats: Arc<Mutex<Uac2SinkStats>>,
}

impl std::fmt::Debug for AlsaUac2Sink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AlsaUac2Sink")
            .field("format", &self.format)
            .field("device", &self.io.label())
            .field("stats", &self.handle().stats())
            .finish()
    }
}

impl AlsaUac2Sink {
    /// Open a real ALSA playback device with [`AlsaPcmFactory`].
    pub fn open(config: AlsaUac2Config) -> Result<Self, UsbError> {
        Self::open_with(config, &AlsaPcmFactory)
    }

    /// Open with an injected factory. Default tests use a fake factory so no
    /// ALSA device is touched.
    pub fn open_with(
        config: AlsaUac2Config,
        factory: &dyn Uac2PcmWriterFactory,
    ) -> Result<Self, UsbError> {
        config.validate()?;
        let io = factory.open(&config)?;
        Ok(Self::with_io(config.format, io))
    }

    /// Wrap an already-open writer. Used by tests and by embedders that own
    /// their own PCM handle.
    pub fn with_io(format: Uac2PcmFormat, io: Box<dyn Uac2PcmWriter>) -> Self {
        Self {
            format,
            io,
            stats: Arc::new(Mutex::new(Uac2SinkStats::default())),
        }
    }

    /// Shared counters.
    pub fn handle(&self) -> Uac2SinkHandle {
        Uac2SinkHandle {
            stats: Arc::clone(&self.stats),
        }
    }

    /// Device label behind this sink.
    pub fn device(&self) -> &str {
        self.io.label()
    }
}

impl Uac2AudioSink for AlsaUac2Sink {
    fn format(&self) -> Uac2PcmFormat {
        self.format
    }

    fn write(&mut self, pcm: &[u8]) -> Result<usize, UsbError> {
        if pcm.is_empty() {
            return Ok(0);
        }
        let frame_bytes = self.format.frame_bytes();
        if pcm.len() % frame_bytes != 0 {
            let error = UsbError::Uac2Format {
                reason: format!(
                    "playback buffer of {} bytes is not frame-aligned ({frame_bytes} bytes/frame)",
                    pcm.len()
                ),
            };
            lock(&self.stats).device_errors += 1;
            return Err(error);
        }

        let result = {
            let mut stats = lock(&self.stats);
            stats.write_calls += 1;
            self.io.write(pcm)
        };
        match result {
            Ok(written) => {
                let mut stats = lock(&self.stats);
                if written > pcm.len() || written % frame_bytes != 0 {
                    stats.device_errors += 1;
                    return Err(UsbError::Uac2Format {
                        reason: format!(
                            "playback device accepted {written} bytes, not whole {frame_bytes}-byte frames"
                        ),
                    });
                }
                if written == 0 {
                    stats.backpressure += 1;
                } else {
                    stats.bytes_written += written as u64;
                    stats.frames_written += (written / frame_bytes) as u64;
                    if written < pcm.len() {
                        stats.short_writes += 1;
                    }
                }
                Ok(written)
            }
            Err(error) => {
                lock(&self.stats).device_errors += 1;
                Err(error)
            }
        }
    }

    fn flush(&mut self) -> Result<(), UsbError> {
        match self.io.flush() {
            Ok(()) => {
                lock(&self.stats).flushes += 1;
                Ok(())
            }
            Err(error) => {
                lock(&self.stats).device_errors += 1;
                Err(error)
            }
        }
    }
}

/// ALSA capture adapter implementing [`Uac2AudioSource`].
pub struct AlsaUac2Source {
    format: Uac2PcmFormat,
    io: Box<dyn Uac2PcmReader>,
    stats: Arc<Mutex<Uac2SourceStats>>,
}

impl std::fmt::Debug for AlsaUac2Source {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AlsaUac2Source")
            .field("format", &self.format)
            .field("device", &self.io.label())
            .field("stats", &self.handle().stats())
            .finish()
    }
}

impl AlsaUac2Source {
    /// Open a real ALSA capture device with [`AlsaPcmFactory`].
    pub fn open(config: AlsaUac2Config) -> Result<Self, UsbError> {
        Self::open_with(config, &AlsaPcmFactory)
    }

    /// Open with an injected factory.
    pub fn open_with(
        config: AlsaUac2Config,
        factory: &dyn Uac2PcmReaderFactory,
    ) -> Result<Self, UsbError> {
        config.validate()?;
        let io = factory.open(&config)?;
        Ok(Self::with_io(config.format, io))
    }

    /// Wrap an already-open reader.
    pub fn with_io(format: Uac2PcmFormat, io: Box<dyn Uac2PcmReader>) -> Self {
        Self {
            format,
            io,
            stats: Arc::new(Mutex::new(Uac2SourceStats::default())),
        }
    }

    /// Shared counters.
    pub fn handle(&self) -> Uac2SourceHandle {
        Uac2SourceHandle {
            stats: Arc::clone(&self.stats),
        }
    }

    /// Device label behind this source.
    pub fn device(&self) -> &str {
        self.io.label()
    }
}

impl Uac2AudioSource for AlsaUac2Source {
    fn format(&self) -> Uac2PcmFormat {
        self.format
    }

    fn read(&mut self, pcm: &mut [u8]) -> Result<usize, UsbError> {
        if pcm.is_empty() {
            return Ok(0);
        }
        let frame_bytes = self.format.frame_bytes();
        if pcm.len() % frame_bytes != 0 {
            let error = UsbError::Uac2Format {
                reason: format!(
                    "capture request of {} bytes is not frame-aligned ({frame_bytes} bytes/frame)",
                    pcm.len()
                ),
            };
            lock(&self.stats).device_errors += 1;
            return Err(error);
        }

        let result = {
            let mut stats = lock(&self.stats);
            stats.read_calls += 1;
            self.io.read(pcm)
        };
        match result {
            Ok(read) => {
                let mut stats = lock(&self.stats);
                if read > pcm.len() || read % frame_bytes != 0 {
                    stats.device_errors += 1;
                    return Err(UsbError::Uac2Format {
                        reason: format!(
                            "capture device produced {read} bytes, not whole {frame_bytes}-byte frames"
                        ),
                    });
                }
                if read == 0 {
                    stats.underruns += 1;
                } else {
                    stats.bytes_read += read as u64;
                    stats.frames_read += (read / frame_bytes) as u64;
                    if read < pcm.len() {
                        stats.short_reads += 1;
                    }
                }
                Ok(read)
            }
            Err(error) => {
                lock(&self.stats).device_errors += 1;
                Err(error)
            }
        }
    }

    fn flush(&mut self) -> Result<(), UsbError> {
        match self.io.flush() {
            Ok(()) => {
                lock(&self.stats).flushes += 1;
                Ok(())
            }
            Err(error) => {
                lock(&self.stats).device_errors += 1;
                Err(error)
            }
        }
    }
}

/// Native PipeWire playback adapter implementing [`Uac2AudioSink`].
#[cfg(target_os = "linux")]
pub struct PipeWireUac2Sink {
    format: Uac2PcmFormat,
    sink: ozma_pipewire::PipeWireAudioSink,
    stats: Arc<Mutex<Uac2SinkStats>>,
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for PipeWireUac2Sink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PipeWireUac2Sink")
            .field("format", &self.format)
            .field("stats", &self.handle().stats())
            .finish()
    }
}

#[cfg(target_os = "linux")]
impl PipeWireUac2Sink {
    /// Open a PipeWire playback stream for an `S16` UAC2 format.
    pub async fn open(
        format: Uac2PcmFormat,
        config: ozma_pipewire::SinkConfig,
    ) -> Result<Self, UsbError> {
        let sink = ozma_pipewire::PipeWireAudioSink::new(config)
            .await
            .map_err(|error| pipewire_error(UAC2_PLAYBACK_ENDPOINT, "open", error))?;
        Self::from_sink(format, sink)
    }

    /// Wrap an already-open PipeWire sink.
    pub fn from_sink(
        format: Uac2PcmFormat,
        sink: ozma_pipewire::PipeWireAudioSink,
    ) -> Result<Self, UsbError> {
        validate_pipewire_format(format)?;
        let negotiated = sink.negotiated_format();
        if negotiated.sample_rate != format.sample_rate
            || negotiated.channels != u16::from(format.channels)
        {
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "PipeWire negotiated {} Hz / {} ch, but the bridge requires {} Hz / {} ch",
                    negotiated.sample_rate,
                    negotiated.channels,
                    format.sample_rate,
                    format.channels
                ),
            });
        }
        Ok(Self {
            format,
            sink,
            stats: Arc::new(Mutex::new(Uac2SinkStats::default())),
        })
    }

    /// Shared counters.
    pub fn handle(&self) -> Uac2SinkHandle {
        Uac2SinkHandle {
            stats: Arc::clone(&self.stats),
        }
    }
}

#[cfg(target_os = "linux")]
impl Uac2AudioSink for PipeWireUac2Sink {
    fn format(&self) -> Uac2PcmFormat {
        self.format
    }

    fn write(&mut self, pcm: &[u8]) -> Result<usize, UsbError> {
        if pcm.is_empty() {
            return Ok(0);
        }
        let frame_bytes = self.format.frame_bytes();
        if pcm.len() % frame_bytes != 0 {
            lock(&self.stats).device_errors += 1;
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "playback buffer of {} bytes is not frame-aligned ({frame_bytes} bytes/frame)",
                    pcm.len()
                ),
            });
        }
        if let Some(error) = self.sink.take_error() {
            lock(&self.stats).device_errors += 1;
            return Err(pipewire_error(UAC2_PLAYBACK_ENDPOINT, "write", error));
        }

        let channels = usize::from(self.format.channels);
        let available_samples = self
            .sink
            .queue_capacity()
            .saturating_sub(self.sink.queued_samples());
        let available_frames = available_samples / channels;
        let offered_frames = pcm.len() / frame_bytes;
        let frames = available_frames.min(offered_frames);
        let mut stats = lock(&self.stats);
        stats.write_calls += 1;
        if frames == 0 {
            stats.backpressure += 1;
            return Ok(0);
        }

        let bytes = frames * frame_bytes;
        let samples: Vec<i16> = pcm[..bytes]
            .chunks_exact(2)
            .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
            .collect();
        match self.sink.write_all(&samples) {
            Ok(()) => {
                stats.bytes_written += bytes as u64;
                stats.frames_written += frames as u64;
                if bytes < pcm.len() {
                    stats.short_writes += 1;
                }
                Ok(bytes)
            }
            Err(ozma_pipewire::PipeWireError::QueueFull { .. }) => {
                stats.backpressure += 1;
                Ok(0)
            }
            Err(error) => {
                stats.device_errors += 1;
                Err(pipewire_error(UAC2_PLAYBACK_ENDPOINT, "write", error))
            }
        }
    }

    fn flush(&mut self) -> Result<(), UsbError> {
        match self.sink.flush() {
            Ok(()) => {
                lock(&self.stats).flushes += 1;
                Ok(())
            }
            Err(error) => {
                lock(&self.stats).device_errors += 1;
                Err(pipewire_error(UAC2_PLAYBACK_ENDPOINT, "flush", error))
            }
        }
    }
}

/// Native PipeWire capture adapter implementing [`Uac2AudioSource`].
#[cfg(target_os = "linux")]
pub struct PipeWireUac2Source {
    format: Uac2PcmFormat,
    source: ozma_pipewire::PipeWireAudioSource,
    pending: Vec<u8>,
    stats: Arc<Mutex<Uac2SourceStats>>,
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for PipeWireUac2Source {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PipeWireUac2Source")
            .field("format", &self.format)
            .field("pending_bytes", &self.pending.len())
            .field("stats", &self.handle().stats())
            .finish()
    }
}

#[cfg(target_os = "linux")]
impl PipeWireUac2Source {
    /// Wrap an already-open PipeWire capture stream.
    ///
    /// `PipeWireAudioSource::new` is async; construct it in the async caller and
    /// hand it here. Reads are then non-blocking via `try_next_frame`.
    pub fn from_source(
        format: Uac2PcmFormat,
        source: ozma_pipewire::PipeWireAudioSource,
    ) -> Result<Self, UsbError> {
        validate_pipewire_format(format)?;
        let negotiated = source.negotiated_format();
        if negotiated.sample_rate != format.sample_rate
            || negotiated.channels != u16::from(format.channels)
        {
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "PipeWire negotiated {} Hz / {} ch, but the bridge requires {} Hz / {} ch",
                    negotiated.sample_rate,
                    negotiated.channels,
                    format.sample_rate,
                    format.channels
                ),
            });
        }
        Ok(Self {
            format,
            source,
            pending: Vec::new(),
            stats: Arc::new(Mutex::new(Uac2SourceStats::default())),
        })
    }

    /// Shared counters.
    pub fn handle(&self) -> Uac2SourceHandle {
        Uac2SourceHandle {
            stats: Arc::clone(&self.stats),
        }
    }

    /// Native PipeWire capture counters, including frames dropped by its own
    /// bounded queue.
    pub fn pipewire_stats(&self) -> ozma_pipewire::CaptureStats {
        self.source.stats()
    }
}

#[cfg(target_os = "linux")]
impl Uac2AudioSource for PipeWireUac2Source {
    fn format(&self) -> Uac2PcmFormat {
        self.format
    }

    fn read(&mut self, pcm: &mut [u8]) -> Result<usize, UsbError> {
        if pcm.is_empty() {
            return Ok(0);
        }
        let frame_bytes = self.format.frame_bytes();
        if pcm.len() % frame_bytes != 0 {
            lock(&self.stats).device_errors += 1;
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "capture request of {} bytes is not frame-aligned ({frame_bytes} bytes/frame)",
                    pcm.len()
                ),
            });
        }

        if self.pending.is_empty() {
            match self.source.try_next_frame() {
                Ok(Some(frame)) => {
                    if frame.sample_rate != self.format.sample_rate
                        || frame.channels != u16::from(self.format.channels)
                    {
                        lock(&self.stats).device_errors += 1;
                        return Err(UsbError::Uac2Format {
                            reason: format!(
                                "PipeWire frame is {} Hz / {} ch, but the bridge requires {} Hz / {} ch",
                                frame.sample_rate,
                                frame.channels,
                                self.format.sample_rate,
                                self.format.channels
                            ),
                        });
                    }
                    self.pending.reserve(frame.samples.len() * 2);
                    for sample in frame.samples {
                        self.pending.extend_from_slice(&sample.to_le_bytes());
                    }
                }
                Ok(None) => {
                    let mut stats = lock(&self.stats);
                    stats.read_calls += 1;
                    stats.underruns += 1;
                    return Ok(0);
                }
                Err(error) => {
                    lock(&self.stats).device_errors += 1;
                    return Err(pipewire_error(UAC2_CAPTURE_ENDPOINT, "read", error));
                }
            }
        }

        let frames = (pcm.len().min(self.pending.len())) / frame_bytes;
        let mut stats = lock(&self.stats);
        stats.read_calls += 1;
        if frames == 0 {
            stats.underruns += 1;
            return Ok(0);
        }
        let read = frames * frame_bytes;
        pcm[..read].copy_from_slice(&self.pending[..read]);
        self.pending.drain(..read);
        stats.bytes_read += read as u64;
        stats.frames_read += frames as u64;
        if read < pcm.len() {
            stats.short_reads += 1;
        }
        Ok(read)
    }

    fn flush(&mut self) -> Result<(), UsbError> {
        self.pending.clear();
        lock(&self.stats).flushes += 1;
        Ok(())
    }
}

/// ALSA non-blocking playback handle.
struct AlsaPcmWriter {
    pcm: PCM,
    device: String,
    frame_bytes: usize,
}

impl AlsaPcmWriter {
    fn open(config: &AlsaUac2Config) -> Result<Self, UsbError> {
        let pcm = open_alsa_pcm(config, Direction::Playback)?;
        Ok(Self {
            pcm,
            device: config.device.clone(),
            frame_bytes: config.format.frame_bytes(),
        })
    }
}

impl Uac2PcmWriter for AlsaPcmWriter {
    fn write(&mut self, pcm: &[u8]) -> Result<usize, UsbError> {
        if pcm.is_empty() {
            return Ok(0);
        }
        if pcm.len() % self.frame_bytes != 0 {
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "playback buffer of {} bytes is not frame-aligned ({} bytes/frame)",
                    pcm.len(),
                    self.frame_bytes
                ),
            });
        }
        let io = self.pcm.io_bytes();
        match io.writei(pcm) {
            Ok(frames) => Ok(frames * self.frame_bytes),
            Err(error) if is_would_block(&error) => Ok(0),
            Err(error) => {
                let message = error.to_string();
                if self.pcm.try_recover(error, true).is_ok() {
                    Ok(0)
                } else {
                    Err(UsbError::Uac2Endpoint {
                        address: UAC2_PLAYBACK_ENDPOINT,
                        operation: "write",
                        reason: format!("ALSA device {:?}: {message}", self.device),
                    })
                }
            }
        }
    }

    fn flush(&mut self) -> Result<(), UsbError> {
        self.pcm.prepare().map_err(|error| UsbError::Uac2Endpoint {
            address: UAC2_PLAYBACK_ENDPOINT,
            operation: "flush",
            reason: format!("ALSA device {:?}: {error}", self.device),
        })
    }

    fn label(&self) -> &str {
        &self.device
    }
}

/// ALSA non-blocking capture handle.
struct AlsaPcmReader {
    pcm: PCM,
    device: String,
    frame_bytes: usize,
}

impl AlsaPcmReader {
    fn open(config: &AlsaUac2Config) -> Result<Self, UsbError> {
        let pcm = open_alsa_pcm(config, Direction::Capture)?;
        Ok(Self {
            pcm,
            device: config.device.clone(),
            frame_bytes: config.format.frame_bytes(),
        })
    }
}

impl Uac2PcmReader for AlsaPcmReader {
    fn read(&mut self, pcm: &mut [u8]) -> Result<usize, UsbError> {
        if pcm.is_empty() {
            return Ok(0);
        }
        if pcm.len() % self.frame_bytes != 0 {
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "capture request of {} bytes is not frame-aligned ({} bytes/frame)",
                    pcm.len(),
                    self.frame_bytes
                ),
            });
        }
        let io = self.pcm.io_bytes();
        match io.readi(pcm) {
            Ok(frames) => Ok(frames * self.frame_bytes),
            Err(error) if is_would_block(&error) => Ok(0),
            Err(error) => {
                let message = error.to_string();
                if self.pcm.try_recover(error, true).is_ok() {
                    Ok(0)
                } else {
                    Err(UsbError::Uac2Endpoint {
                        address: UAC2_CAPTURE_ENDPOINT,
                        operation: "read",
                        reason: format!("ALSA device {:?}: {message}", self.device),
                    })
                }
            }
        }
    }

    fn flush(&mut self) -> Result<(), UsbError> {
        self.pcm.prepare().map_err(|error| UsbError::Uac2Endpoint {
            address: UAC2_CAPTURE_ENDPOINT,
            operation: "flush",
            reason: format!("ALSA device {:?}: {error}", self.device),
        })
    }

    fn label(&self) -> &str {
        &self.device
    }
}

/// Open and configure an ALSA PCM at exactly the requested format.
fn open_alsa_pcm(config: &AlsaUac2Config, direction: Direction) -> Result<PCM, UsbError> {
    config.validate()?;
    let address = match direction {
        Direction::Playback => UAC2_PLAYBACK_ENDPOINT,
        Direction::Capture => UAC2_CAPTURE_ENDPOINT,
    };
    let device = &config.device;
    let format = config.format;
    let pcm = PCM::new(device, direction, true).map_err(|error| UsbError::Uac2Endpoint {
        address,
        operation: "open",
        reason: format!("cannot open ALSA device {device:?}: {error}"),
    })?;

    let configure = || -> Result<(), UsbError> {
        let hwp = HwParams::any(&pcm).map_err(|error| UsbError::Uac2Endpoint {
            address,
            operation: "configure",
            reason: format!("ALSA device {device:?} has no hardware parameters: {error}"),
        })?;
        hwp.set_channels(u32::from(format.channels))
            .map_err(|error| parameter_error(address, device, "channels", error))?;
        hwp.set_rate(format.sample_rate, ValueOr::Nearest)
            .map_err(|error| parameter_error(address, device, "sample rate", error))?;
        hwp.set_format(alsa_format(format.sample_size))
            .map_err(|error| parameter_error(address, device, "sample format", error))?;
        hwp.set_access(Access::RWInterleaved)
            .map_err(|error| parameter_error(address, device, "interleaved access", error))?;
        hwp.set_period_size(config.period_frames as Frames, ValueOr::Nearest)
            .map_err(|error| parameter_error(address, device, "period size", error))?;
        hwp.set_buffer_size((config.period_frames * config.buffer_periods) as Frames)
            .map_err(|error| parameter_error(address, device, "buffer size", error))?;
        pcm.hw_params(&hwp)
            .map_err(|error| parameter_error(address, device, "hardware parameters", error))?;
        // Let the first read/write start the stream. Starting an empty
        // playback buffer is rejected with EPIPE by some drivers (snd-aloop),
        // and the bridge is a steady 1 ms pump rather than a one-shot player.
        let swp = pcm
            .sw_params_current()
            .map_err(|error| parameter_error(address, device, "software parameters", error))?;
        swp.set_start_threshold(1)
            .map_err(|error| parameter_error(address, device, "start threshold", error))?;
        pcm.sw_params(&swp)
            .map_err(|error| parameter_error(address, device, "software parameters", error))?;
        pcm.prepare()
            .map_err(|error| parameter_error(address, device, "prepare", error))?;
        Ok(())
    };
    configure()?;

    let current = pcm
        .hw_params_current()
        .map_err(|error| UsbError::Uac2Endpoint {
            address,
            operation: "configure",
            reason: format!("cannot read negotiated parameters for {device:?}: {error}"),
        })?;
    let rate = current
        .get_rate()
        .map_err(|error| parameter_error(address, device, "negotiated sample rate", error))?;
    let channels = current
        .get_channels()
        .map_err(|error| parameter_error(address, device, "negotiated channels", error))?;
    let sample_format = current
        .get_format()
        .map_err(|error| parameter_error(address, device, "negotiated sample format", error))?;
    if rate != format.sample_rate
        || channels != u32::from(format.channels)
        || sample_format != alsa_format(format.sample_size)
    {
        return Err(UsbError::Uac2Format {
            reason: format!(
                "ALSA device {device:?} negotiated {rate} Hz / {channels} ch / {sample_format:?}, \
                 but the bridge requires {} Hz / {} ch / {:?}; resampling is not implicit",
                format.sample_rate,
                format.channels,
                alsa_format(format.sample_size)
            ),
        });
    }
    drop(current);

    Ok(pcm)
}

fn alsa_format(size: Uac2SampleSize) -> Format {
    match size {
        Uac2SampleSize::S16 => Format::S16LE,
        // UAC2 S24 is packed in three bytes, matching ALSA's S24_3LE.
        Uac2SampleSize::S24 => Format::S243LE,
        Uac2SampleSize::S32 => Format::S32LE,
    }
}

fn parameter_error(address: u8, device: &str, parameter: &str, error: alsa::Error) -> UsbError {
    UsbError::Uac2Endpoint {
        address,
        operation: "configure",
        reason: format!("ALSA device {device:?} cannot provide {parameter}: {error}"),
    }
}

fn is_would_block(error: &alsa::Error) -> bool {
    std::io::Error::from_raw_os_error(error.errno()).kind() == std::io::ErrorKind::WouldBlock
}

#[cfg(target_os = "linux")]
fn validate_pipewire_format(format: Uac2PcmFormat) -> Result<(), UsbError> {
    format.validate()?;
    if format.sample_size != Uac2SampleSize::S16 {
        return Err(UsbError::Uac2Format {
            reason: format!(
                "PipeWire streams carry interleaved S16; {} is not available for this adapter",
                format.sample_size.bits()
            ),
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn pipewire_error(
    address: u8,
    operation: &'static str,
    error: ozma_pipewire::PipeWireError,
) -> UsbError {
    UsbError::Uac2Endpoint {
        address,
        operation,
        reason: format!("PipeWire {operation} failed: {error}"),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_usb::{Uac2AudioSink, Uac2AudioSource};

    #[derive(Debug)]
    struct FakeWriter {
        label: String,
        max_bytes: Option<usize>,
        fail_after: Option<usize>,
        writes: Vec<u8>,
        flushes: usize,
    }

    impl FakeWriter {
        fn new(max_bytes: Option<usize>) -> Self {
            Self {
                label: "fake-playback".to_owned(),
                max_bytes,
                fail_after: None,
                writes: Vec::new(),
                flushes: 0,
            }
        }
    }

    impl Uac2PcmWriter for FakeWriter {
        fn write(&mut self, pcm: &[u8]) -> Result<usize, UsbError> {
            if let Some(limit) = self.fail_after {
                if self.writes.len() >= limit {
                    return Err(UsbError::Uac2Endpoint {
                        address: UAC2_PLAYBACK_ENDPOINT,
                        operation: "write",
                        reason: "fake device disappeared".to_owned(),
                    });
                }
            }
            let accepted = self
                .max_bytes
                .map(|max| max.min(pcm.len()))
                .unwrap_or(pcm.len());
            self.writes.extend_from_slice(&pcm[..accepted]);
            Ok(accepted)
        }

        fn flush(&mut self) -> Result<(), UsbError> {
            self.flushes += 1;
            Ok(())
        }

        fn label(&self) -> &str {
            &self.label
        }
    }

    #[derive(Debug)]
    struct FakeReader {
        label: String,
        data: Vec<u8>,
        offset: usize,
        max_bytes: Option<usize>,
        flushes: usize,
    }

    impl FakeReader {
        fn new(data: Vec<u8>, max_bytes: Option<usize>) -> Self {
            Self {
                label: "fake-capture".to_owned(),
                data,
                offset: 0,
                max_bytes,
                flushes: 0,
            }
        }

        fn remaining(&self) -> usize {
            self.data.len().saturating_sub(self.offset)
        }
    }

    impl Uac2PcmReader for FakeReader {
        fn read(&mut self, pcm: &mut [u8]) -> Result<usize, UsbError> {
            let available = self.remaining().min(pcm.len());
            let read = self
                .max_bytes
                .map(|max| max.min(available))
                .unwrap_or(available);
            pcm[..read].copy_from_slice(&self.data[self.offset..self.offset + read]);
            self.offset += read;
            Ok(read)
        }

        fn flush(&mut self) -> Result<(), UsbError> {
            self.flushes += 1;
            Ok(())
        }

        fn label(&self) -> &str {
            &self.label
        }
    }

    fn format() -> Uac2PcmFormat {
        Uac2PcmFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_size: Uac2SampleSize::S16,
        }
    }

    #[test]
    fn sink_counts_short_writes_and_reports_backpressure() {
        let format = format();
        let mut sink = AlsaUac2Sink::with_io(
            format,
            Box::new(FakeWriter::new(Some(format.frame_bytes() * 2))),
        );
        let pcm = vec![0x11u8; format.frame_bytes() * 5];
        assert_eq!(sink.write(&pcm).unwrap(), format.frame_bytes() * 2);
        assert_eq!(sink.write(&pcm).unwrap(), format.frame_bytes() * 2);
        let stats = sink.handle().stats();
        assert_eq!(stats.write_calls, 2);
        assert_eq!(stats.short_writes, 2);
        assert_eq!(stats.frames_written, 4);
        assert_eq!(stats.backpressure, 0);
    }

    #[test]
    fn sink_zero_progress_is_backpressure_not_success() {
        let format = format();
        let mut sink = AlsaUac2Sink::with_io(format, Box::new(FakeWriter::new(Some(0))));
        assert_eq!(sink.write(&vec![0u8; format.frame_bytes()]).unwrap(), 0);
        assert_eq!(sink.handle().stats().backpressure, 1);
        assert_eq!(sink.handle().stats().bytes_written, 0);
    }

    #[test]
    fn sink_device_error_is_typed_and_counted() {
        let format = format();
        let mut writer = FakeWriter::new(None);
        writer.fail_after = Some(0);
        let mut sink = AlsaUac2Sink::with_io(format, Box::new(writer));
        let error = sink.write(&vec![0u8; format.frame_bytes()]).unwrap_err();
        assert!(matches!(error, UsbError::Uac2Endpoint { .. }));
        assert_eq!(sink.handle().stats().device_errors, 1);
    }

    #[test]
    fn sink_rejects_ragged_frames() {
        let format = format();
        let mut sink = AlsaUac2Sink::with_io(format, Box::new(FakeWriter::new(None)));
        let error = sink.write(&[0u8; 3]).unwrap_err();
        assert!(matches!(error, UsbError::Uac2Format { .. }));
        assert_eq!(sink.handle().stats().device_errors, 1);
    }

    #[test]
    fn source_returns_partial_reads_and_flushes() {
        let format = format();
        let data = vec![0x5Au8; format.frame_bytes() * 4];
        let mut source = AlsaUac2Source::with_io(
            format,
            Box::new(FakeReader::new(
                data.clone(),
                Some(format.frame_bytes() * 2),
            )),
        );
        let mut buffer = vec![0u8; format.frame_bytes() * 4];
        assert_eq!(source.read(&mut buffer).unwrap(), format.frame_bytes() * 2);
        assert_eq!(source.read(&mut buffer).unwrap(), format.frame_bytes() * 2);
        assert_eq!(source.read(&mut buffer).unwrap(), 0);
        let stats = source.handle().stats();
        assert_eq!(stats.read_calls, 3);
        assert_eq!(stats.short_reads, 2);
        assert_eq!(stats.underruns, 1);
        assert_eq!(stats.frames_read, 4);
        source.flush().unwrap();
        assert_eq!(source.handle().stats().flushes, 1);
    }

    #[test]
    fn default_config_is_48k_stereo_s16() {
        let config = AlsaUac2Config::for_device("default");
        assert_eq!(config.format, format());
        config.validate().unwrap();
        assert!(AlsaUac2Config::new(
            "x",
            Uac2PcmFormat {
                sample_rate: 0,
                channels: 2,
                sample_size: Uac2SampleSize::S16,
            }
        )
        .validate()
        .is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pipewire_adapters_reject_wide_samples() {
        let mut wide = format();
        wide.sample_size = Uac2SampleSize::S24;
        assert!(matches!(
            validate_pipewire_format(wide),
            Err(UsbError::Uac2Format { .. })
        ));
        assert!(validate_pipewire_format(format()).is_ok());
    }

    #[test]
    fn factory_missing_device_is_a_typed_open_error() {
        struct MissingFactory;
        impl Uac2PcmWriterFactory for MissingFactory {
            fn open(&self, config: &AlsaUac2Config) -> Result<Box<dyn Uac2PcmWriter>, UsbError> {
                Err(UsbError::Uac2Endpoint {
                    address: UAC2_PLAYBACK_ENDPOINT,
                    operation: "open",
                    reason: format!("missing {}", config.device),
                })
            }
        }
        let error = AlsaUac2Sink::open_with(
            AlsaUac2Config::for_device("ozma-no-such-device"),
            &MissingFactory,
        )
        .unwrap_err();
        assert!(matches!(error, UsbError::Uac2Endpoint { .. }));
    }
}
