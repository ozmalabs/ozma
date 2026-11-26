#![cfg(target_os = "linux")]

// SPDX-License-Identifier: AGPL-3.0-only
//! Round-trip tests for the real audio adapters with in-memory PCM devices.
//!
//! No ALSA or PipeWire device is opened. The fakes implement the same
//! `Uac2PcmWriter`/`Uac2PcmReader` contracts as the production ALSA handles,
//! so the adapter wrapper, the bridge ring, packetisation, counters, and
//! stop/flush paths are all exercised.

use std::sync::{Arc, Mutex};

use ozma_audio::{
    AlsaUac2Config, AlsaUac2Sink, AlsaUac2Source, Uac2PcmReader, Uac2PcmWriter,
    Uac2PcmWriterFactory,
};
use ozma_usb::{
    FakeUac2Endpoint, Uac2Bridge, Uac2BridgeConfig, Uac2BridgeInputs, Uac2Endpoint, Uac2PcmFormat,
    Uac2SampleSize, Uac2StreamPlan, UsbError,
};

#[derive(Debug, Default)]
struct WriterState {
    written: Vec<u8>,
    flushes: usize,
    max_bytes: Option<usize>,
    failing: bool,
}

#[derive(Debug, Clone, Default)]
struct RecordingWriter {
    state: Arc<Mutex<WriterState>>,
}

impl RecordingWriter {
    fn with_max(max_bytes: usize) -> Self {
        let writer = Self::default();
        writer.state.lock().unwrap().max_bytes = Some(max_bytes);
        writer
    }

    fn failing() -> Self {
        let writer = Self::default();
        writer.state.lock().unwrap().failing = true;
        writer
    }

    fn written(&self) -> Vec<u8> {
        self.state.lock().unwrap().written.clone()
    }

    fn flushes(&self) -> usize {
        self.state.lock().unwrap().flushes
    }
}

impl Uac2PcmWriter for RecordingWriter {
    fn write(&mut self, pcm: &[u8]) -> Result<usize, UsbError> {
        let mut state = self.state.lock().unwrap();
        if state.failing {
            return Err(UsbError::Uac2Endpoint {
                address: ozma_usb::UAC2_PLAYBACK_ENDPOINT,
                operation: "write",
                reason: "fake playback device removed".to_owned(),
            });
        }
        let accepted = state
            .max_bytes
            .map(|max| max.min(pcm.len()))
            .unwrap_or(pcm.len());
        state.written.extend_from_slice(&pcm[..accepted]);
        Ok(accepted)
    }

    fn flush(&mut self) -> Result<(), UsbError> {
        self.state.lock().unwrap().flushes += 1;
        Ok(())
    }

    fn label(&self) -> &str {
        "recording-writer"
    }
}

#[derive(Debug, Clone)]
struct RecordingReader {
    state: Arc<Mutex<ReaderState>>,
}

#[derive(Debug, Default)]
struct ReaderState {
    data: Vec<u8>,
    offset: usize,
    max_bytes: Option<usize>,
    flushes: usize,
}

impl RecordingReader {
    fn new(data: Vec<u8>) -> Self {
        let reader = Self {
            state: Arc::new(Mutex::new(ReaderState::default())),
        };
        reader.state.lock().unwrap().data = data;
        reader
    }

    fn with_max(self, max_bytes: usize) -> Self {
        self.state.lock().unwrap().max_bytes = Some(max_bytes);
        self
    }
}

impl Uac2PcmReader for RecordingReader {
    fn read(&mut self, pcm: &mut [u8]) -> Result<usize, UsbError> {
        let mut state = self.state.lock().unwrap();
        let available = (state.data.len() - state.offset).min(pcm.len());
        let read = state
            .max_bytes
            .map(|max| max.min(available))
            .unwrap_or(available);
        pcm[..read].copy_from_slice(&state.data[state.offset..state.offset + read]);
        state.offset += read;
        Ok(read)
    }

    fn flush(&mut self) -> Result<(), UsbError> {
        self.state.lock().unwrap().flushes += 1;
        Ok(())
    }

    fn label(&self) -> &str {
        "recording-reader"
    }
}

fn format(channels: u8) -> Uac2PcmFormat {
    Uac2PcmFormat {
        sample_rate: 48_000,
        channels,
        sample_size: Uac2SampleSize::S16,
    }
}

#[test]
fn playback_round_trip_writes_the_same_frames_to_the_sink() {
    let format = format(2);
    let writer = RecordingWriter::default();
    let written = writer.clone();
    let sink = AlsaUac2Sink::with_io(format, Box::new(writer));
    let sink_handle = sink.handle();

    let endpoint = FakeUac2Endpoint::out(1024).unwrap();
    let endpoint_handle = endpoint.handle();
    let pcm: Vec<u8> = (0..format.frame_bytes() * 8)
        .map(|index| (index % 251) as u8)
        .collect();
    endpoint_handle.queue_host_packet(pcm.clone()).unwrap();

    let config = Uac2BridgeConfig::new("audio").with_playback(Uac2StreamPlan {
        format,
        endpoint: Uac2Endpoint::playback(1024).unwrap(),
    });
    let mut bridge = Uac2Bridge::new(
        config,
        Uac2BridgeInputs {
            playback_endpoint: Some(Box::new(endpoint)),
            playback_sink: Some(Box::new(sink)),
            ..Uac2BridgeInputs::default()
        },
    )
    .unwrap();

    bridge.start().unwrap();
    let transfer = bridge.poll_playback().unwrap();
    assert_eq!(transfer.bytes, pcm.len());
    assert!(bridge.is_running());
    assert_eq!(bridge.report().status.as_str(), "running");
    assert_eq!(written.written(), pcm);
    assert_eq!(sink_handle.stats().frames_written, 8);
    assert_eq!(bridge.counters().sink_bytes, pcm.len() as u64);
    assert!(bridge.has_moved_data());

    bridge.stop().unwrap();
    assert_eq!(written.flushes(), 1);
    assert_eq!(bridge.report().status.as_str(), "stopped");
}

#[test]
fn playback_backpressure_keeps_pcm_in_the_ring_and_counts_it() {
    let format = format(2);
    let writer = RecordingWriter::with_max(0);
    let sink = AlsaUac2Sink::with_io(format, Box::new(writer));
    let endpoint = FakeUac2Endpoint::out(1024).unwrap();
    let handle = endpoint.handle();
    handle
        .queue_host_packet(vec![7u8; format.frame_bytes() * 4])
        .unwrap();

    let config = Uac2BridgeConfig::new("audio").with_playback(Uac2StreamPlan {
        format,
        endpoint: Uac2Endpoint::playback(1024).unwrap(),
    });
    let mut bridge = Uac2Bridge::new(
        config,
        Uac2BridgeInputs {
            playback_endpoint: Some(Box::new(endpoint)),
            playback_sink: Some(Box::new(sink)),
            ..Uac2BridgeInputs::default()
        },
    )
    .unwrap();
    bridge.start().unwrap();
    bridge.poll_playback().unwrap();
    assert!(bridge.counters().sink_backpressure >= 1);
    assert_eq!(bridge.counters().sink_bytes, 0);
    // A blocked sink is not a fake live audio success: the bridge can still be
    // "running" only because a USB packet was read; sink counters stay zero.
    assert_eq!(bridge.counters().sink_frames, 0);
}

#[test]
fn capture_round_trip_packetises_the_source_frames() {
    let usb_format = format(2);
    let pcm: Vec<u8> = (0..usb_format.frame_bytes() * 40)
        .map(|index| (index % 239) as u8)
        .collect();
    let reader = RecordingReader::new(pcm.clone());
    let source = AlsaUac2Source::with_io(usb_format, Box::new(reader));
    let source_handle = source.handle();

    let endpoint = FakeUac2Endpoint::in_endpoint(1024).unwrap();
    let handle = endpoint.handle();
    let config = Uac2BridgeConfig::new("audio").with_capture(Uac2StreamPlan {
        format: usb_format,
        endpoint: Uac2Endpoint::capture(1024).unwrap(),
    });
    let mut bridge = Uac2Bridge::new(
        config,
        Uac2BridgeInputs {
            capture_endpoint: Some(Box::new(endpoint)),
            capture_source: Some(Box::new(source)),
            ..Uac2BridgeInputs::default()
        },
    )
    .unwrap();

    bridge.start().unwrap();
    let mut produced = Vec::new();
    while produced.len() < pcm.len() {
        let transfer = bridge.poll_capture().unwrap();
        assert!(transfer.bytes <= 1024);
        for packet in handle.take_written_packets() {
            produced.extend_from_slice(&packet);
        }
    }
    assert_eq!(&produced[..pcm.len()], pcm.as_slice());
    assert_eq!(source_handle.stats().frames_read, 40);
    assert!(bridge.counters().capture_frames_out >= 40);
    assert_eq!(bridge.report().status.as_str(), "running");

    bridge.stop().unwrap();
    assert_eq!(source_handle.stats().flushes, 1);
}

#[test]
fn capture_partial_reads_are_padded_and_counted() {
    let usb_format = format(2);
    let pcm = vec![0x22u8; usb_format.frame_bytes() * 5];
    let reader = RecordingReader::new(pcm.clone()).with_max(usb_format.frame_bytes() * 2);
    let source = AlsaUac2Source::with_io(usb_format, Box::new(reader));
    let endpoint = FakeUac2Endpoint::in_endpoint(1024).unwrap();
    let handle = endpoint.handle();

    let config = Uac2BridgeConfig::new("audio").with_capture(Uac2StreamPlan {
        format: usb_format,
        endpoint: Uac2Endpoint::capture(1024).unwrap(),
    });
    let mut bridge = Uac2Bridge::new(
        config,
        Uac2BridgeInputs {
            capture_endpoint: Some(Box::new(endpoint)),
            capture_source: Some(Box::new(source)),
            ..Uac2BridgeInputs::default()
        },
    )
    .unwrap();
    bridge.start().unwrap();
    let mut padded = false;
    for _ in 0..16 {
        let transfer = bridge.poll_capture().unwrap();
        if transfer.short {
            padded = true;
            break;
        }
    }
    assert!(padded, "expected a short capture interval");
    assert!(bridge.counters().capture_short_packets >= 1);
    assert!(bridge.counters().capture_padded_bytes >= 1);
    // Padded frames are real packet payloads, never a silent success claim.
    assert!(!handle.written_packets().is_empty());
}

#[test]
fn playback_device_error_is_typed_and_counted_by_the_bridge() {
    let format = format(2);
    let sink = AlsaUac2Sink::with_io(format, Box::new(RecordingWriter::failing()));
    let endpoint = FakeUac2Endpoint::out(1024).unwrap();
    endpoint
        .handle()
        .queue_host_packet(vec![9u8; format.frame_bytes() * 2])
        .unwrap();
    let config = Uac2BridgeConfig::new("audio").with_playback(Uac2StreamPlan {
        format,
        endpoint: Uac2Endpoint::playback(1024).unwrap(),
    });
    let mut bridge = Uac2Bridge::new(
        config,
        Uac2BridgeInputs {
            playback_endpoint: Some(Box::new(endpoint)),
            playback_sink: Some(Box::new(sink)),
            ..Uac2BridgeInputs::default()
        },
    )
    .unwrap();
    bridge.start().unwrap();
    let error = bridge.poll_playback().unwrap_err();
    assert!(matches!(error, UsbError::Uac2Endpoint { .. }));
    assert_eq!(bridge.counters().errors, 1);
    assert_eq!(bridge.counters().sink_bytes, 0);
}

#[test]
fn open_failure_keeps_a_typed_error_and_no_sink() {
    struct MissingFactory;
    impl Uac2PcmWriterFactory for MissingFactory {
        fn open(&self, config: &AlsaUac2Config) -> Result<Box<dyn Uac2PcmWriter>, UsbError> {
            Err(UsbError::Uac2Endpoint {
                address: ozma_usb::UAC2_PLAYBACK_ENDPOINT,
                operation: "open",
                reason: format!("device {} is missing", config.device),
            })
        }
    }
    let error = AlsaUac2Sink::open_with(AlsaUac2Config::for_device("missing"), &MissingFactory)
        .unwrap_err();
    assert!(matches!(error, UsbError::Uac2Endpoint { .. }));
}

#[test]
fn bridge_never_claims_running_without_pcm_movement() {
    let format = format(2);
    let sink = AlsaUac2Sink::with_io(format, Box::new(RecordingWriter::default()));
    let endpoint = FakeUac2Endpoint::out(1024).unwrap();
    let config = Uac2BridgeConfig::new("audio").with_playback(Uac2StreamPlan {
        format,
        endpoint: Uac2Endpoint::playback(1024).unwrap(),
    });
    let mut bridge = Uac2Bridge::new(
        config,
        Uac2BridgeInputs {
            playback_endpoint: Some(Box::new(endpoint)),
            playback_sink: Some(Box::new(sink)),
            ..Uac2BridgeInputs::default()
        },
    )
    .unwrap();
    bridge.start().unwrap();
    let transfer = bridge.poll_playback().unwrap();
    assert_eq!(transfer.bytes, 0);
    assert_eq!(bridge.report().status.as_str(), "configured");
    assert!(!bridge.report().is_live());
}
