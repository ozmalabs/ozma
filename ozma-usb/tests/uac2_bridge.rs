// SPDX-License-Identifier: AGPL-3.0-only
//! UAC2 isochronous ↔ PCM bridge core.
//!
//! Every test uses in-memory endpoints and fake audio paths. No USB device,
//! UDC, ALSA PCM, or PipeWire stream is opened.

use ozma_usb::{
    FakeUac2AudioSink, FakeUac2AudioSinkHandle, FakeUac2AudioSource, FakeUac2Endpoint,
    FakeUac2EndpointHandle, Uac2AudioSink, Uac2AudioSource, Uac2Bridge, Uac2BridgeConfig,
    Uac2BridgeCounters, Uac2BridgeInputs, Uac2BridgeStatus, Uac2Config, Uac2Direction,
    Uac2Endpoint, Uac2EndpointIo, Uac2Packetizer, Uac2PcmFormat, Uac2SampleSize, Uac2StreamPlan,
    Uac2UnderrunPolicy, UAC2_MAX_PACKET_BYTES,
};

fn pcm(rate: u32, channels: u8, size: Uac2SampleSize) -> Uac2PcmFormat {
    Uac2PcmFormat::new(rate, channels, size).unwrap()
}

fn max_packet(format: Uac2PcmFormat) -> u16 {
    let bytes = Uac2Packetizer::new(format, 1).unwrap().max_packet_bytes();
    u16::try_from(bytes)
        .unwrap()
        .min(UAC2_MAX_PACKET_BYTES as u16)
}

fn playback_plan(format: Uac2PcmFormat) -> Uac2StreamPlan {
    Uac2StreamPlan {
        format,
        endpoint: Uac2Endpoint::playback(max_packet(format)).unwrap(),
    }
}

fn capture_plan(format: Uac2PcmFormat) -> Uac2StreamPlan {
    Uac2StreamPlan {
        format,
        endpoint: Uac2Endpoint::capture(max_packet(format)).unwrap(),
    }
}

struct PlaybackFixture {
    bridge: Uac2Bridge,
    endpoint: FakeUac2EndpointHandle,
    sink: FakeUac2AudioSinkHandle,
}

fn playback_bridge(
    usb: Uac2PcmFormat,
    sink_format: Uac2PcmFormat,
    ring_capacity: usize,
) -> PlaybackFixture {
    let endpoint = FakeUac2Endpoint::out(max_packet(usb)).unwrap();
    let endpoint_handle = endpoint.handle();
    let sink = FakeUac2AudioSink::new(sink_format);
    let sink_handle = sink.handle();
    let config = Uac2BridgeConfig::new("audio")
        .with_playback(playback_plan(usb))
        .with_ring_capacity_bytes(ring_capacity);
    let inputs = Uac2BridgeInputs {
        playback_endpoint: Some(Box::new(endpoint)),
        playback_sink: Some(Box::new(sink)),
        ..Uac2BridgeInputs::default()
    };
    PlaybackFixture {
        bridge: Uac2Bridge::new(config, inputs).unwrap(),
        endpoint: endpoint_handle,
        sink: sink_handle,
    }
}

struct CaptureFixture {
    bridge: Uac2Bridge,
    endpoint: FakeUac2EndpointHandle,
}

fn capture_bridge(
    usb: Uac2PcmFormat,
    _source_format: Uac2PcmFormat,
    source: FakeUac2AudioSource,
    policy: Uac2UnderrunPolicy,
) -> CaptureFixture {
    let endpoint = FakeUac2Endpoint::in_endpoint(max_packet(usb)).unwrap();
    let endpoint_handle = endpoint.handle();
    let config = Uac2BridgeConfig::new("audio")
        .with_capture(capture_plan(usb))
        .with_underrun_policy(policy);
    let inputs = Uac2BridgeInputs {
        capture_endpoint: Some(Box::new(endpoint)),
        capture_source: Some(Box::new(source)),
        ..Uac2BridgeInputs::default()
    };
    CaptureFixture {
        bridge: Uac2Bridge::new(config, inputs).unwrap(),
        endpoint: endpoint_handle,
    }
}

#[test]
fn playback_out_packets_reach_the_sink_and_only_then_report_running() {
    let format = pcm(48_000, 2, Uac2SampleSize::S16);
    let mut fixture = playback_bridge(format, format, 4096);

    // Configured, but nothing has moved yet.
    let report = fixture.bridge.report();
    assert_eq!(report.status, Uac2BridgeStatus::Configured);
    assert!(!report.is_live());
    assert!(!report.counters.has_data());

    fixture.bridge.start().unwrap();
    assert_eq!(
        fixture.bridge.report().status,
        Uac2BridgeStatus::Configured,
        "starting alone must not claim live audio"
    );

    let packet: Vec<u8> = (0..24).collect();
    fixture.endpoint.queue_host_packet(packet.clone()).unwrap();
    let transfer = fixture.bridge.poll_playback().unwrap();
    assert_eq!(transfer.packets, 1);
    assert_eq!(transfer.bytes, 24);
    assert_eq!(fixture.sink.written(), packet);

    let report = fixture.bridge.report();
    assert_eq!(report.status, Uac2BridgeStatus::Running);
    assert!(report.is_live());
    assert_eq!(report.counters.playback_packets_in, 1);
    assert_eq!(report.counters.playback_bytes_in, 24);
    assert_eq!(report.counters.playback_frames_in, 6);
    assert_eq!(report.counters.sink_bytes, 24);
    assert_eq!(report.counters.sink_frames, 6);

    let status = fixture
        .bridge
        .status("audio", &Uac2Config::headset(2, 1, 48_000, 16));
    assert_eq!(status.bridge, Uac2BridgeStatus::Running);
    assert!(status.is_bridged());
    assert!(status.is_live());
    assert_eq!(
        status.bridge_report.as_ref().unwrap().counters,
        report.counters
    );
}

#[test]
fn playback_converts_24_bit_usb_to_16_bit_sink() {
    let usb = pcm(48_000, 2, Uac2SampleSize::S24);
    let sink = pcm(48_000, 2, Uac2SampleSize::S16);
    let mut fixture = playback_bridge(usb, sink, 4096);
    fixture.bridge.start().unwrap();

    // Two stereo frames: L=0x100000, R=0x200000, L=0x300000, R=0x400000.
    let packet = vec![
        0x00, 0x00, 0x10, 0x00, 0x00, 0x20, 0x00, 0x00, 0x30, 0x00, 0x00, 0x40,
    ];
    fixture.endpoint.queue_host_packet(packet).unwrap();
    fixture.bridge.poll_playback().unwrap();
    assert_eq!(
        fixture.sink.written(),
        vec![0x00, 0x10, 0x00, 0x20, 0x00, 0x30, 0x00, 0x40]
    );
    assert_eq!(fixture.bridge.counters().playback_frames_in, 2);
    assert_eq!(fixture.bridge.counters().sink_frames, 2);
}

#[test]
fn capture_packetizer_is_sample_accurate_for_44100() {
    let format = pcm(44_100, 2, Uac2SampleSize::S16);
    let source = FakeUac2AudioSource::silence(format);
    let mut fixture = capture_bridge(format, format, source, Uac2UnderrunPolicy::PadSilence);
    fixture.bridge.start().unwrap();

    let mut frames = 0usize;
    let mut saw_five = false;
    let mut saw_six = false;
    for _ in 0..8_000 {
        let transfer = fixture.bridge.poll_capture().unwrap();
        assert_eq!(transfer.packets, 1);
        let packet_frames = transfer.bytes / format.frame_bytes();
        match packet_frames {
            5 => saw_five = true,
            6 => saw_six = true,
            other => panic!("unexpected 44.1 kHz packet of {other} frames"),
        }
        frames += packet_frames;
    }
    assert_eq!(frames, 44_100, "one second must be exactly 44 100 frames");
    assert!(saw_five && saw_six, "44.1 kHz alternates 5 and 6 frames");
    assert_eq!(fixture.endpoint.written_packets().len(), 8_000);
    assert_eq!(fixture.bridge.counters().capture_packets_out, 8_000);
    assert_eq!(fixture.bridge.counters().capture_frames_out, 44_100);
    assert_eq!(fixture.bridge.report().status, Uac2BridgeStatus::Running);
}

#[test]
fn capture_upmixes_a_mono_source_to_stereo_usb_packets() {
    let usb = pcm(48_000, 2, Uac2SampleSize::S16);
    let source_format = pcm(48_000, 1, Uac2SampleSize::S16);
    let mono = vec![
        0x10u8, 0x00, 0x20, 0x00, 0x30, 0x00, 0x40, 0x00, 0x50, 0x00, 0x60, 0x00,
    ];
    let source = FakeUac2AudioSource::with_pcm(source_format, mono).unwrap();
    let mut fixture = capture_bridge(usb, source_format, source, Uac2UnderrunPolicy::PadSilence);
    fixture.bridge.start().unwrap();

    let transfer = fixture.bridge.poll_capture().unwrap();
    assert_eq!(
        transfer.bytes, 24,
        "six mono frames become six stereo frames"
    );
    assert!(!transfer.short);
    let packets = fixture.endpoint.written_packets();
    assert_eq!(
        packets[0],
        vec![
            0x10, 0x00, 0x10, 0x00, 0x20, 0x00, 0x20, 0x00, 0x30, 0x00, 0x30, 0x00, 0x40, 0x00,
            0x40, 0x00, 0x50, 0x00, 0x50, 0x00, 0x60, 0x00, 0x60, 0x00,
        ]
    );
    assert_eq!(fixture.bridge.counters().capture_frames_out, 6);
}

#[test]
fn short_capture_reads_pad_to_the_expected_packet_size() {
    let format = pcm(48_000, 2, Uac2SampleSize::S16);
    let partial = vec![
        0x11u8, 0x11, 0x22, 0x22, 0x33, 0x33, 0x44, 0x44, 0x55, 0x55, 0x66, 0x66,
    ];
    let source = FakeUac2AudioSource::with_pcm(format, partial.clone()).unwrap();
    let mut fixture = capture_bridge(format, format, source, Uac2UnderrunPolicy::PadSilence);
    fixture.bridge.start().unwrap();

    let transfer = fixture.bridge.poll_capture().unwrap();
    assert!(transfer.short);
    assert!(!transfer.underrun);
    assert_eq!(transfer.bytes, 24, "48 kHz stereo S16 packet is six frames");
    let packets = fixture.endpoint.written_packets();
    assert_eq!(packets.len(), 1);
    assert_eq!(&packets[0][..12], &partial[..]);
    assert!(packets[0][12..].iter().all(|byte| *byte == 0));
    let counters = fixture.bridge.counters();
    assert_eq!(counters.capture_short_packets, 1);
    assert_eq!(counters.capture_padded_bytes, 12);
    assert_eq!(counters.capture_underruns, 0);

    // The finite source is empty now; padding still emits a full silent packet.
    let transfer = fixture.bridge.poll_capture().unwrap();
    assert!(transfer.short);
    assert!(transfer.underrun);
    let packets = fixture.endpoint.written_packets();
    assert_eq!(packets.len(), 2);
    assert!(packets[1].iter().all(|byte| *byte == 0));
    let counters = fixture.bridge.counters();
    assert_eq!(counters.capture_short_packets, 2);
    assert_eq!(counters.capture_padded_bytes, 36);
    assert_eq!(counters.capture_underruns, 1);
}

#[test]
fn skip_policy_emits_a_partial_last_packet_then_no_packet() {
    let format = pcm(48_000, 2, Uac2SampleSize::S16);
    let partial = vec![
        0x10u8, 0x00, 0x20, 0x00, 0x30, 0x00, 0x40, 0x00, 0x50, 0x00, 0x60, 0x00,
    ];
    let source = FakeUac2AudioSource::with_pcm(format, partial.clone()).unwrap();
    let mut fixture = capture_bridge(format, format, source, Uac2UnderrunPolicy::SkipPacket);
    fixture.bridge.start().unwrap();

    let transfer = fixture.bridge.poll_capture().unwrap();
    assert!(transfer.short);
    assert_eq!(
        transfer.bytes, 12,
        "only the available three frames are sent"
    );
    assert_eq!(fixture.endpoint.written_packets(), vec![partial]);
    assert_eq!(fixture.bridge.counters().capture_padded_bytes, 0);

    let transfer = fixture.bridge.poll_capture().unwrap();
    assert_eq!(transfer.packets, 0);
    assert!(transfer.underrun);
    assert_eq!(fixture.endpoint.written_packets().len(), 1);
    assert_eq!(fixture.bridge.counters().capture_underruns, 1);
}

#[test]
fn playback_ring_drops_oldest_frames_and_counts_backpressure() {
    let format = pcm(48_000, 2, Uac2SampleSize::S16);
    // Four stereo S16 frames exactly.
    // The sink never accepts data, so the ring stays full after each push.
    let endpoint = FakeUac2Endpoint::out(max_packet(format)).unwrap();
    let endpoint_handle = endpoint.handle();
    let sink = FakeUac2AudioSink::new(format).with_max_write(0);
    let sink_handle = sink.handle();
    let config = Uac2BridgeConfig::new("audio")
        .with_playback(playback_plan(format))
        .with_ring_capacity_bytes(16);
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

    endpoint_handle
        .queue_host_packet((0..24).collect::<Vec<u8>>())
        .unwrap();
    let transfer = bridge.poll_playback().unwrap();
    assert_eq!(
        transfer.dropped_bytes, 8,
        "six frames pushed into a four-frame ring"
    );
    assert!(bridge.counters().playback_overflow_events >= 1);
    assert!(bridge.counters().sink_backpressure >= 1);
    assert_eq!(sink_handle.written_bytes(), 0);

    endpoint_handle
        .queue_host_packet((100..124).collect::<Vec<u8>>())
        .unwrap();
    bridge.poll_playback().unwrap();
    let counters = bridge.counters();
    assert!(counters.playback_dropped_frames >= 2);
    assert!(counters.playback_dropped_bytes >= 8);
    assert!(counters.playback_overflow_events >= 2);
    assert!(counters.sink_backpressure >= 2);
    assert_eq!(sink_handle.written_bytes(), 0);

    // A sink that accepts one frame per call observes partial backpressure.
    let endpoint = FakeUac2Endpoint::out(max_packet(format)).unwrap();
    let endpoint_handle = endpoint.handle();
    let sink = FakeUac2AudioSink::new(format).with_max_write(4);
    let sink_handle = sink.handle();
    let config = Uac2BridgeConfig::new("audio")
        .with_playback(playback_plan(format))
        .with_ring_capacity_bytes(4096);
    let mut partial = Uac2Bridge::new(
        config,
        Uac2BridgeInputs {
            playback_endpoint: Some(Box::new(endpoint)),
            playback_sink: Some(Box::new(sink)),
            ..Uac2BridgeInputs::default()
        },
    )
    .unwrap();
    endpoint_handle
        .queue_host_packet((0..24).collect::<Vec<u8>>())
        .unwrap();
    partial.start().unwrap();
    partial.poll_playback().unwrap();
    assert_eq!(sink_handle.written_bytes(), 4);
    assert_eq!(partial.counters().sink_backpressure, 1);
}

#[test]
fn flush_clears_buffers_and_stop_is_a_typed_terminal_state() {
    let format = pcm(48_000, 2, Uac2SampleSize::S16);
    let mut fixture = playback_bridge(format, format, 4096);
    fixture.bridge.start().unwrap();
    fixture
        .endpoint
        .queue_host_packet((0..24).collect::<Vec<u8>>())
        .unwrap();
    fixture.bridge.poll_playback().unwrap();
    assert_eq!(fixture.bridge.report().status, Uac2BridgeStatus::Running);

    fixture.bridge.flush().unwrap();
    assert!(!fixture.bridge.has_moved_data());
    assert_eq!(
        fixture.bridge.report().status,
        Uac2BridgeStatus::Configured,
        "flush removes live-audio claim until data moves again"
    );
    // Counters are cumulative.
    assert_eq!(fixture.bridge.counters().playback_bytes_in, 24);

    fixture.bridge.stop().unwrap();
    assert!(!fixture.bridge.is_running());
    let error = fixture.bridge.poll_playback().unwrap_err();
    assert_eq!(error.kind(), "uac2-not-running");

    fixture.bridge.start().unwrap();
    assert_eq!(fixture.bridge.report().status, Uac2BridgeStatus::Configured);
    fixture
        .endpoint
        .queue_host_packet((50..74).collect::<Vec<u8>>())
        .unwrap();
    fixture.bridge.poll_playback().unwrap();
    assert_eq!(fixture.bridge.report().status, Uac2BridgeStatus::Running);
}

#[test]
fn stop_after_data_reports_stopped_without_claiming_live() {
    let format = pcm(48_000, 2, Uac2SampleSize::S16);
    let mut fixture = playback_bridge(format, format, 4096);
    fixture.bridge.start().unwrap();
    fixture
        .endpoint
        .queue_host_packet((0..24).collect::<Vec<u8>>())
        .unwrap();
    fixture.bridge.poll_playback().unwrap();
    assert_eq!(fixture.bridge.report().status, Uac2BridgeStatus::Running);

    fixture.bridge.stop().unwrap();
    let report = fixture.bridge.report();
    assert_eq!(report.status, Uac2BridgeStatus::Stopped);
    assert!(!report.status.is_live());
    assert!(report.counters.has_data());
    assert!(fixture.bridge.has_moved_data());
}

#[test]
fn capture_flush_resets_the_packet_accumulator() {
    let format = pcm(44_100, 2, Uac2SampleSize::S16);
    let source = FakeUac2AudioSource::silence(format);
    let mut fixture = capture_bridge(format, format, source, Uac2UnderrunPolicy::PadSilence);
    fixture.bridge.start().unwrap();
    let first = fixture.bridge.poll_capture().unwrap();
    assert_eq!(first.bytes, 20, "first 44.1 kHz packet is five frames");
    fixture.bridge.flush().unwrap();
    let second = fixture.bridge.poll_capture().unwrap();
    assert_eq!(second.bytes, 20, "flush restarts at the accumulator origin");
}

#[test]
fn invalid_formats_and_missing_paths_are_typed() {
    let format = pcm(48_000, 2, Uac2SampleSize::S16);

    let no_direction = Uac2BridgeConfig::new("audio");
    assert_eq!(
        no_direction.validate().unwrap_err().kind(),
        "uac2-missing-path"
    );

    let missing_endpoint = Uac2BridgeConfig::new("audio").with_playback(playback_plan(format));
    let error = Uac2Bridge::new(missing_endpoint, Uac2BridgeInputs::default()).unwrap_err();
    assert_eq!(error.kind(), "uac2-missing-path");
    assert!(error.to_string().contains("playback endpoint"));

    let endpoint = FakeUac2Endpoint::out(max_packet(format)).unwrap();
    let sink = FakeUac2AudioSink::new(format);
    let missing_audio = Uac2BridgeConfig::new("audio").with_playback(playback_plan(format));
    let error = Uac2Bridge::new(
        missing_audio,
        Uac2BridgeInputs {
            playback_endpoint: Some(Box::new(endpoint)),
            ..Uac2BridgeInputs::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.kind(), "uac2-missing-path");
    assert!(error.to_string().contains("playback audio"));

    let wrong_rate = Uac2BridgeConfig::new("audio").with_playback(playback_plan(format));
    let endpoint = FakeUac2Endpoint::out(max_packet(format)).unwrap();
    let error = Uac2Bridge::new(
        wrong_rate,
        Uac2BridgeInputs {
            playback_endpoint: Some(Box::new(endpoint)),
            playback_sink: Some(Box::new(FakeUac2AudioSink::new(pcm(
                44_100,
                2,
                Uac2SampleSize::S16,
            )))),
            ..Uac2BridgeInputs::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.kind(), "uac2-format");
    assert!(error.to_string().contains("resampling"));

    let wrong_endpoint = Uac2BridgeConfig::new("audio").with_playback(playback_plan(format));
    let in_endpoint = FakeUac2Endpoint::in_endpoint(max_packet(format)).unwrap();
    let error = Uac2Bridge::new(
        wrong_endpoint,
        Uac2BridgeInputs {
            playback_endpoint: Some(Box::new(in_endpoint)),
            playback_sink: Some(Box::new(sink)),
            ..Uac2BridgeInputs::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.kind(), "uac2-endpoint");

    let zero_ring = Uac2BridgeConfig::new("audio")
        .with_playback(playback_plan(format))
        .with_ring_capacity_bytes(0);
    assert_eq!(zero_ring.validate().unwrap_err().kind(), "uac2-overflow");
}

#[test]
fn status_is_unsupported_without_endpoint_or_audio_paths() {
    let config = Uac2Config::headset(2, 1, 48_000, 24);
    let plain = ozma_usb::Uac2Status::from_config("audio", &config);
    assert_eq!(plain.bridge, Uac2BridgeStatus::Unsupported);
    assert!(!plain.is_bridged());
    assert!(!plain.is_live());
    assert!(plain.bridge_report.is_none());

    let unavailable = plain.with_bridge_unavailable("no UAC2 endpoint or audio path injected");
    assert_eq!(unavailable.bridge, Uac2BridgeStatus::Unsupported);
    assert!(!unavailable.is_live());
    let report = unavailable.bridge_report.as_ref().unwrap();
    assert_eq!(report.status, Uac2BridgeStatus::Unsupported);
    assert_eq!(
        report.reason.as_deref(),
        Some("no UAC2 endpoint or audio path injected")
    );
    assert!(!report.is_live());
    let json = serde_json::to_value(&unavailable).unwrap();
    assert_eq!(json["bridge"], "unsupported");
    assert_eq!(json["bridge_report"]["status"], "unsupported");
}

#[test]
fn started_but_idle_bridge_stays_configured() {
    let format = pcm(48_000, 2, Uac2SampleSize::S16);
    let mut fixture = playback_bridge(format, format, 4096);
    let status_config = Uac2Config::speaker(2, 48_000, 16);
    fixture.bridge.start().unwrap();

    // No queued OUT packet: the endpoint returns a zero-length packet.
    let transfer = fixture.bridge.poll_playback().unwrap();
    assert_eq!(transfer.packets, 0);
    let report = fixture.bridge.report();
    assert_eq!(report.status, Uac2BridgeStatus::Configured);
    assert!(!report.is_live());
    assert!(!report.counters.has_data());
    assert_eq!(report.counters.empty_packets, 1);

    let status = fixture.bridge.status("audio", &status_config);
    assert_eq!(status.bridge, Uac2BridgeStatus::Configured);
    assert!(!status.is_live());
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["bridge"], "configured");
    assert_eq!(json["bridge_report"]["status"], "configured");
    assert_eq!(json["bridge_report"]["counters"]["empty_packets"], 1);
}

#[test]
fn uac2_config_derives_both_plans_and_reports_format_and_packet_size() {
    let uac2 = Uac2Config::headset(2, 1, 48_000, 24);
    let bridge_config = Uac2BridgeConfig::from_uac2("audio", &uac2).unwrap();
    let playback = bridge_config.playback.unwrap();
    let capture = bridge_config.capture.unwrap();
    assert_eq!(playback.format.channels, 2);
    assert_eq!(playback.format.sample_size, Uac2SampleSize::S24);
    assert_eq!(playback.endpoint.address, 0x01);
    assert_eq!(playback.endpoint.direction, Uac2Direction::Out);
    assert_eq!(capture.format.channels, 1);
    assert_eq!(capture.endpoint.address, 0x82);
    assert_eq!(capture.endpoint.direction, Uac2Direction::In);

    let playback_endpoint = FakeUac2Endpoint::out(playback.endpoint.max_packet_size).unwrap();
    let playback_endpoint_handle = playback_endpoint.handle();
    let playback_sink = FakeUac2AudioSink::new(playback.format);
    let playback_sink_handle = playback_sink.handle();
    let capture_endpoint = FakeUac2Endpoint::in_endpoint(capture.endpoint.max_packet_size).unwrap();
    let capture_endpoint_handle = capture_endpoint.handle();
    let capture_source = FakeUac2AudioSource::silence(capture.format);

    let mut bridge = Uac2Bridge::new(
        bridge_config.clone(),
        Uac2BridgeInputs {
            playback_endpoint: Some(Box::new(playback_endpoint)),
            playback_sink: Some(Box::new(playback_sink)),
            capture_endpoint: Some(Box::new(capture_endpoint)),
            capture_source: Some(Box::new(capture_source)),
        },
    )
    .unwrap();
    bridge.start().unwrap();

    let packet = vec![0u8; playback.format.frame_bytes() * 6];
    playback_endpoint_handle.queue_host_packet(packet).unwrap();
    bridge.pump().unwrap();

    let status = bridge.status("audio", &uac2);
    assert_eq!(status.bridge, Uac2BridgeStatus::Running);
    let report = status.bridge_report.as_ref().unwrap();
    assert_eq!(report.playback.unwrap().format.channels, 2);
    assert_eq!(report.playback.unwrap().maximum_packet_bytes, 36);
    assert_eq!(report.capture.unwrap().format.channels, 1);
    assert_eq!(report.capture.unwrap().maximum_packet_bytes, 18);
    assert_eq!(playback_sink_handle.written_bytes(), 36);
    assert_eq!(capture_endpoint_handle.written_packets().len(), 1);

    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["bridge"], "running");
    assert_eq!(json["bridge_report"]["playback"]["endpoint"], 1);
    assert_eq!(json["bridge_report"]["capture"]["endpoint"], 130);
    assert_eq!(
        json["bridge_report"]["playback"]["format"]["sample_size"],
        "s24"
    );
}

#[test]
fn counters_are_serializable_and_report_no_data_before_traffic() {
    let format = pcm(48_000, 2, Uac2SampleSize::S16);
    let fixture = playback_bridge(format, format, 4096);
    let counters = fixture.bridge.counters();
    assert_eq!(counters, Uac2BridgeCounters::default());
    assert!(!counters.has_data());
    let json = serde_json::to_value(counters).unwrap();
    assert_eq!(json["playback_bytes_in"], 0);
}

#[test]
fn fake_paths_are_object_safe_and_typed_on_wrong_direction() {
    let format = pcm(48_000, 1, Uac2SampleSize::S16);
    let _sink: Box<dyn Uac2AudioSink> = Box::new(FakeUac2AudioSink::new(format));
    let _source: Box<dyn Uac2AudioSource> = Box::new(FakeUac2AudioSource::silence(format));

    let mut endpoint = FakeUac2Endpoint::in_endpoint(64).unwrap();
    let mut buffer = [0u8; 8];
    assert_eq!(
        endpoint.read_packet(&mut buffer).unwrap_err().kind(),
        "uac2-endpoint"
    );
    let mut out = FakeUac2Endpoint::out(64).unwrap();
    assert_eq!(
        out.write_packet(&[1, 2]).unwrap_err().kind(),
        "uac2-endpoint"
    );
    let descriptor = Uac2Endpoint::playback(64).unwrap();
    assert_eq!(out.endpoint(), descriptor);
}
