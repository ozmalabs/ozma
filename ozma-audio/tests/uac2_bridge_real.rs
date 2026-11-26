#![cfg(target_os = "linux")]

// SPDX-License-Identifier: AGPL-3.0-only
//! Opt-in real ALSA → UAC2 capture-packetiser test.
//!
//! The default suite uses the in-memory fakes in `ozma-usb`. This test is
//! ignored and environment-gated because it opens a real ALSA capture device:
//!
//! ```text
//! OZMA_AUDIO_TEST_DEVICE=default \
//!   cargo test -p ozma-audio --test uac2_bridge_real -- --ignored
//! ```
//!
//! It does not open a USB endpoint or a real UDC; the UAC2 side remains the
//! injected in-memory endpoint.

use ozma_audio::{AlsaCapture, AlsaCaptureConfig, AudioSource};
use ozma_usb::{
    FakeUac2AudioSource, FakeUac2Endpoint, Uac2Bridge, Uac2BridgeConfig, Uac2BridgeInputs,
    Uac2Endpoint, Uac2PcmFormat, Uac2SampleSize, Uac2StreamPlan,
};

#[tokio::test]
#[ignore = "requires a real ALSA capture device; set OZMA_AUDIO_TEST_DEVICE"]
async fn real_alsa_capture_feeds_the_uac2_capture_packetiser() {
    let Ok(device) = std::env::var("OZMA_AUDIO_TEST_DEVICE") else {
        eprintln!("skipping: OZMA_AUDIO_TEST_DEVICE is not set");
        return;
    };

    let mut capture = AlsaCapture::open(AlsaCaptureConfig::for_device(device))
        .expect("open configured ALSA capture device");
    let frame = capture.next_frame().await.expect("first 20 ms frame");
    capture.stop().await.expect("stop ALSA capture");
    assert!(!frame.is_empty());
    assert_eq!(frame.sample_rate, 48_000);
    assert_eq!(frame.channels, 2);
    let expected_frames = frame.frames_per_channel();
    assert!(expected_frames > 0);

    let format = Uac2PcmFormat::new(frame.sample_rate, frame.channels, Uac2SampleSize::S16)
        .expect("valid UAC2 format");
    let mut pcm = Vec::with_capacity(frame.samples.len() * 2);
    for sample in &frame.samples {
        pcm.extend_from_slice(&sample.to_le_bytes());
    }

    let endpoint = FakeUac2Endpoint::in_endpoint(1024).expect("capture endpoint");
    let handle = endpoint.handle();
    let config = Uac2BridgeConfig::new("audio").with_capture(Uac2StreamPlan {
        format,
        endpoint: Uac2Endpoint::capture(1024).expect("capture descriptor"),
    });
    let source = FakeUac2AudioSource::with_pcm(format, pcm).expect("finite PCM source");
    let mut bridge = Uac2Bridge::new(
        config,
        Uac2BridgeInputs {
            capture_endpoint: Some(Box::new(endpoint)),
            capture_source: Some(Box::new(source)),
            ..Uac2BridgeInputs::default()
        },
    )
    .expect("valid bridge");

    bridge.start().expect("start bridge");
    let mut frames = 0usize;
    while frames < expected_frames {
        let transfer = bridge.poll_capture().expect("packetise capture frame");
        assert!(transfer.packets > 0, "packetiser produced no packet");
        frames += transfer.bytes / format.frame_bytes();
    }
    assert_eq!(frames, expected_frames);
    assert!(!handle.written_packets().is_empty());
    assert_eq!(bridge.counters().capture_frames_out, expected_frames as u64);
}
