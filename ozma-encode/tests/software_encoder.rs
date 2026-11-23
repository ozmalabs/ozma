// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end tests for the real software H.264 encoder.
//!
//! These tests use only synthetic frames; no capture device or hardware
//! encoder is required. OpenH264 builds and initialises in a few seconds.

use jpeg_encoder::{ColorType, Encoder as JpegEncoder};
use ozma_encode::{
    annex_b_is_keyframe, annex_b_nal_types, EncoderProbe, EncoderRegistry, H264SoftwareEncoder,
    SelectedEncoder, SoftwareProbe, ENCODE_FAILED,
};
use ozma_media::{
    Encoder, FakeCapture, Frame, FrameFormat, MediaError, Pipeline, PipelineConfig, PixelFormat,
};
use ozma_protocol::{codes, Codec, EncodeCapability, EncoderBackend, MediaFormat};

fn request(width: u32, height: u32) -> MediaFormat {
    MediaFormat::new(Codec::H264, width, height, 30, 1_000)
}

fn pattern(width: u32, height: u32, phase: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            out.push((((x * 4) + phase) & 0xFF) as u8);
            out.push((((y * 4) + phase * 3) & 0xFF) as u8);
            out.push((((x + y) * 2 + phase * 7) & 0xFF) as u8);
            out.push(0xFF);
        }
    }
    out
}

fn rgb_frame(format: FrameFormat, sequence: u64) -> Frame {
    let pts = sequence * 33_333;
    let (width, height) = (format.width, format.height);
    let rgba = pattern(width, height, sequence as u32);
    // Drop the alpha byte to produce packed RGB24.
    let mut packed = Vec::with_capacity((width * height * 3) as usize);
    for chunk in rgba.chunks_exact(4) {
        packed.extend_from_slice(&chunk[..3]);
    }
    Frame::with_data(format, pts, sequence, sequence == 0, packed).unwrap()
}

fn pattern_rgb(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            out.push(((x * 4) & 0xFF) as u8);
            out.push(((y * 4) & 0xFF) as u8);
            out.push((((x + y) * 2) & 0xFF) as u8);
        }
    }
    out
}

fn nv12_frame(format: FrameFormat, sequence: u64) -> Frame {
    let (width, height) = (format.width, format.height);
    let pixels = (width * height) as usize;
    let mut data = vec![0u8; pixels + pixels / 2];
    for y in 0..height {
        for x in 0..width {
            data[(y * width + x) as usize] = ((x + y + sequence as u32 * 3) & 0xFF) as u8;
        }
    }
    for index in 0..pixels / 4 {
        let offset = pixels + index * 2;
        data[offset] = ((index + sequence as usize) & 0xFF) as u8;
        data[offset + 1] =
            ((255usize.wrapping_sub(index).wrapping_add(sequence as usize)) & 0xFF) as u8;
    }
    Frame::with_data(format, sequence * 33_333, sequence, sequence == 0, data).unwrap()
}

fn yuyv_frame(format: FrameFormat, sequence: u64) -> Frame {
    let (width, height) = (format.width, format.height);
    let mut data = Vec::with_capacity((width * height * 2) as usize);
    for y in 0..height {
        for x in 0..width {
            data.push(((x + y + sequence as u32 * 5) & 0xFF) as u8);
            if x % 2 == 0 {
                data.push((((x / 2) + sequence as u32) & 0xFF) as u8);
            } else {
                data.push(((200 - (x / 2) + sequence as u32) & 0xFF) as u8);
            }
        }
    }
    Frame::with_data(format, sequence * 33_333, sequence, sequence == 0, data).unwrap()
}

fn bgra_frame(format: FrameFormat, sequence: u64) -> Frame {
    Frame::with_data(
        format,
        sequence * 33_333,
        sequence,
        sequence == 0,
        pattern(format.width, format.height, sequence as u32),
    )
    .unwrap()
}

fn assert_annex_b(data: &[u8]) -> Vec<u8> {
    assert!(
        data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1]),
        "access unit does not start with an Annex-B start code: {:02x?}",
        &data[..data.len().min(16)]
    );
    let types = annex_b_nal_types(data);
    assert!(!types.is_empty(), "no NAL units found");
    types
}

async fn encode_sequence(format: FrameFormat, make: fn(FrameFormat, u64) -> Frame) {
    let mut encoder = H264SoftwareEncoder::new(format, request(format.width, format.height))
        .expect("construct software encoder");
    assert_eq!(encoder.id(), "openh264-software-h264");
    assert_eq!(encoder.input_format(), format);
    assert_eq!(encoder.output_format().codec, Codec::H264);

    let mut previous_pts = None;
    for sequence in 0u64..3 {
        let frame = make(format, sequence);
        let expected_pts = frame.pts_micros();
        let encoded = encoder.encode(frame).await.expect("encode frame");
        assert!(!encoded.is_empty());
        assert_eq!(encoded.pts_micros(), expected_pts, "PTS must be preserved");
        assert_eq!(encoded.sequence(), sequence, "sequence must be preserved");
        let types = assert_annex_b(encoded.data());
        if sequence == 0 {
            assert!(encoded.is_keyframe(), "first frame must be a keyframe");
            assert!(
                types.contains(&5),
                "first frame must contain an IDR: {types:?}"
            );
            assert!(
                types.contains(&7),
                "first frame must contain SPS: {types:?}"
            );
        }
        assert_eq!(
            encoded.is_keyframe(),
            types.contains(&5),
            "keyframe flag must match the IDR NAL scan"
        );
        if let Some(previous) = previous_pts {
            assert!(encoded.pts_micros() > previous, "PTS must increase");
        }
        previous_pts = Some(encoded.pts_micros());
    }
    assert_eq!(encoder.encoded_count(), 3);
    assert!(encoder.flush().await.unwrap().is_empty());
}

#[tokio::test]
async fn rgb24_encodes_to_annex_b_with_preserved_timing() {
    encode_sequence(FrameFormat::rgb24(64, 64), rgb_frame).await;
}

#[tokio::test]
async fn nv12_encodes_to_annex_b_with_preserved_timing() {
    encode_sequence(FrameFormat::new(PixelFormat::Nv12, 64, 64), nv12_frame).await;
}

#[tokio::test]
async fn yuyv_encodes_to_annex_b_with_preserved_timing() {
    encode_sequence(FrameFormat::new(PixelFormat::Yuyv, 64, 64), yuyv_frame).await;
}

#[tokio::test]
async fn bgra_encodes_to_annex_b_with_preserved_timing() {
    encode_sequence(FrameFormat::new(PixelFormat::Bgra, 64, 64), bgra_frame).await;
}

#[tokio::test]
async fn keyframe_request_produces_an_idr_on_the_next_frame() {
    let format = FrameFormat::rgb24(64, 64);
    let mut encoder = H264SoftwareEncoder::new(format, request(64, 64)).unwrap();

    let first = encoder.encode(rgb_frame(format, 0)).await.unwrap();
    assert!(first.is_keyframe());
    let second = encoder.encode(rgb_frame(format, 1)).await.unwrap();
    assert!(!second.is_keyframe(), "second frame should be a P frame");
    assert!(!annex_b_is_keyframe(second.data()));

    encoder.request_keyframe().await.unwrap();
    let third = encoder.encode(rgb_frame(format, 2)).await.unwrap();
    assert!(third.is_keyframe(), "requested keyframe must be an IDR");
    let types = assert_annex_b(third.data());
    assert!(
        types.contains(&5),
        "forced frame must contain IDR: {types:?}"
    );
    assert!(types.contains(&7), "forced frame must carry SPS: {types:?}");
}

#[tokio::test]
async fn configured_gop_emits_keyframes_on_cadence() {
    let format = FrameFormat::rgb24(64, 64);
    let mut encoder = H264SoftwareEncoder::new(format, request(64, 64))
        .unwrap()
        .with_keyframe_interval(2);
    assert_eq!(encoder.keyframe_interval(), 2);

    let flags: Vec<bool> = {
        let mut flags = Vec::new();
        for sequence in 0u64..4 {
            flags.push(
                encoder
                    .encode(rgb_frame(format, sequence))
                    .await
                    .unwrap()
                    .is_keyframe(),
            );
        }
        flags
    };
    assert_eq!(flags, vec![true, false, true, false]);
}

#[tokio::test]
async fn malformed_mjpeg_is_rejected_with_a_structured_error() {
    let format = FrameFormat::new(PixelFormat::Mjpeg, 64, 64);
    let mut encoder = H264SoftwareEncoder::new(format, request(64, 64)).unwrap();
    let frame = Frame::with_data(format, 0, 0, true, b"this is not a JPEG".to_vec()).unwrap();

    let error = encoder.encode(frame).await.unwrap_err();
    match error {
        MediaError::Api(api) => {
            assert_eq!(api.code, ENCODE_FAILED);
            assert!(api.remediation.is_some());
            assert!(api.message.to_ascii_lowercase().contains("mjpeg"));
        }
        other => panic!("expected a structured encoder error, got {other:?}"),
    }
}

#[tokio::test]
async fn valid_mjpeg_is_decoded_and_reencoded() {
    let width = 64u16;
    let height = 64u16;
    let rgb = pattern_rgb(u32::from(width), u32::from(height));
    let mut jpeg = Vec::new();
    JpegEncoder::new(&mut jpeg, 90)
        .encode(&rgb, width, height, ColorType::Rgb)
        .unwrap();

    let format = FrameFormat::new(PixelFormat::Mjpeg, u32::from(width), u32::from(height));
    let mut encoder =
        H264SoftwareEncoder::new(format, request(format.width, format.height)).unwrap();
    let frame = Frame::with_data(format, 5_000, 7, true, jpeg).unwrap();
    let encoded = encoder.encode(frame).await.unwrap();
    assert_eq!(encoded.pts_micros(), 5_000);
    assert_eq!(encoded.sequence(), 7);
    assert!(encoded.is_keyframe());
    let types = assert_annex_b(encoded.data());
    assert!(
        types.contains(&5),
        "MJPEG input should produce an IDR: {types:?}"
    );
}

#[tokio::test]
async fn h264_passthrough_forwards_annex_b_and_detects_idr() {
    let format = FrameFormat::new(PixelFormat::H264, 64, 64);
    let mut encoder = H264SoftwareEncoder::new(format, request(64, 64)).unwrap();
    assert_eq!(encoder.id(), "h264-passthrough");
    assert!(encoder.is_passthrough());

    let idr = vec![
        0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1E, // SPS
        0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80, // PPS
        0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, // IDR
    ];
    let frame = Frame::with_data(format, 1_000, 0, false, idr.clone()).unwrap();
    let encoded = encoder.encode(frame).await.unwrap();
    assert_eq!(
        encoded.data(),
        idr.as_slice(),
        "passthrough must not rewrite bytes"
    );
    assert!(encoded.is_keyframe(), "IDR scan must mark the access unit");
    assert_eq!(encoded.pts_micros(), 1_000);
    assert_eq!(encoded.sequence(), 0);

    // A skip/delta frame is forwarded but not marked as a keyframe, even when
    // the incoming capture flag is wrong. Use a three-byte start code to cover
    // both Annex-B forms.
    let delta = vec![0, 0, 1, 0x41, 0x9A, 0x00];
    let frame = Frame::with_data(format, 2_000, 1, true, delta.clone()).unwrap();
    let encoded = encoder.encode(frame).await.unwrap();
    assert_eq!(encoded.data(), delta.as_slice());
    assert!(
        !encoded.is_keyframe(),
        "P frame must not be marked as a keyframe"
    );
}

#[tokio::test]
async fn h264_passthrough_rejects_non_annex_b_and_keyframe_requests() {
    let format = FrameFormat::new(PixelFormat::H264, 64, 64);
    let mut encoder = H264SoftwareEncoder::new(format, request(64, 64)).unwrap();

    let frame = Frame::with_data(format, 0, 0, true, b"AVCC length-prefixed".to_vec()).unwrap();
    let error = encoder.encode(frame).await.unwrap_err();
    assert!(matches!(
        error,
        MediaError::Api(ref api) if api.code == ENCODE_FAILED
    ));

    let error = encoder.request_keyframe().await.unwrap_err();
    assert!(matches!(
        error,
        MediaError::Api(ref api) if api.code == ENCODE_FAILED
    ));
}

#[tokio::test]
async fn unsupported_inputs_and_hdr_are_rejected_at_construction() {
    let hdr = MediaFormat {
        codec: Codec::H264,
        width: 64,
        height: 64,
        fps: 30,
        bitrate_kbps: 1_000,
        color: ozma_protocol::ColorFormat::Hdr10,
        hdr: true,
    };
    let error = H264SoftwareEncoder::new(FrameFormat::rgb24(64, 64), hdr).unwrap_err();
    assert_eq!(error.code, codes::ENCODER_UNAVAILABLE);

    let p010 = FrameFormat::new(PixelFormat::P010, 64, 64);
    let error = H264SoftwareEncoder::new(p010, request(64, 64)).unwrap_err();
    assert_eq!(error.code, codes::ENCODER_UNAVAILABLE);

    let av1 = MediaFormat::new(Codec::Av1, 64, 64, 30, 1_000);
    let error = H264SoftwareEncoder::new(FrameFormat::rgb24(64, 64), av1).unwrap_err();
    assert_eq!(error.code, codes::ENCODER_UNAVAILABLE);
}

#[tokio::test]
async fn registry_selects_and_builds_a_real_software_encoder() {
    let mut registry = EncoderRegistry::with_software_h264();
    registry.probe_all(&SoftwareProbe::new()).await;
    let probe = registry
        .probe_result(Codec::H264, EncoderBackend::Software)
        .expect("software probe recorded");
    assert!(probe.is_ok(), "software probe failed: {:?}", probe.error);

    let request = request(64, 64);
    let selection = registry.select(&request).unwrap();
    assert_eq!(selection.backend(), EncoderBackend::Software);
    assert!(!selection.is_hardware());
    assert_eq!(
        selection.reason,
        ozma_encode::SelectionReason::OnlyCandidate
    );

    let format = FrameFormat::rgb24(64, 64);
    let concrete = selection.software_encoder(format).unwrap();
    assert_eq!(concrete.id(), "openh264-software-h264");

    let mut encoder = selection.build_encoder(format).unwrap();
    assert_eq!(encoder.id(), "openh264-software-h264");
    let encoded = encoder.encode(rgb_frame(format, 0)).await.unwrap();
    assert!(encoded.is_keyframe());
    assert_annex_b(encoded.data());
}

#[tokio::test]
async fn selected_encoder_drives_the_media_pipeline() {
    let registry = EncoderRegistry::with_software_h264();
    let selection = registry.select(&request(64, 64)).unwrap();
    let format = FrameFormat::rgb24(64, 64);
    let encoder = selection.build_encoder(format).unwrap();

    let source = FakeCapture::new(format).with_frame_limit(3).with_fps(1_000);
    let mut pipeline = Pipeline::start(source, encoder, PipelineConfig::default()).unwrap();

    let first = pipeline.recv().await.expect("first encoded frame");
    assert!(first.is_keyframe());
    assert_annex_b(first.data());
    let second = pipeline.recv().await.expect("second encoded frame");
    assert!(second.pts_micros() > first.pts_micros());

    let metrics = pipeline.wait().await.unwrap();
    assert_eq!(metrics.frames_captured, 3);
    assert_eq!(metrics.frames_encoded, 3);
    assert_eq!(metrics.errors, 0);
}

#[tokio::test]
async fn fake_selection_still_builds_the_ci_encoder() {
    let capability = EncodeCapability {
        codec: Codec::H264,
        backend: EncoderBackend::Fake,
        hardware: false,
        max_width: 1920,
        max_height: 1080,
        max_fps: 60,
        hdr: false,
        profiles: Vec::new(),
    };
    let registry = EncoderRegistry::from_capabilities([capability]);
    let selection = registry.select(&request(64, 64)).unwrap();
    let format = FrameFormat::rgb24(64, 64);
    let mut encoder = selection.build_encoder(format).unwrap();
    assert!(encoder.id().contains("fake"));
    let encoded = encoder.encode(rgb_frame(format, 0)).await.unwrap();
    assert!(encoded.is_keyframe());
}

#[tokio::test]
async fn software_probe_verifies_a_real_idr_access_unit() {
    let capability = H264SoftwareEncoder::capability();
    let result = SoftwareProbe::new()
        .with_checked_at(1_234)
        .probe(&capability)
        .await;
    assert!(result.is_ok(), "probe failed: {:?}", result.error);
    assert_eq!(result.checked_at, 1_234);
    assert_eq!(result.codec(), Codec::H264);
    assert_eq!(result.backend(), EncoderBackend::Software);
}

#[tokio::test]
async fn software_probe_reports_that_it_cannot_validate_hardware() {
    let capability = EncodeCapability {
        codec: Codec::H264,
        backend: EncoderBackend::Vaapi,
        hardware: true,
        max_width: 1920,
        max_height: 1080,
        max_fps: 60,
        hdr: false,
        profiles: Vec::new(),
    };
    let result = SoftwareProbe::new().probe(&capability).await;
    assert!(!result.is_ok());
    let error = result.error.expect("structured failure");
    assert_eq!(error.code, codes::ENCODER_PROBE_FAILED);
    assert!(error.remediation.is_some());
}

#[tokio::test]
async fn hardware_selection_never_builds_a_fake_or_software_encoder() {
    let capability = EncodeCapability {
        codec: Codec::H264,
        backend: EncoderBackend::Vaapi,
        hardware: true,
        max_width: 1920,
        max_height: 1080,
        max_fps: 60,
        hdr: false,
        profiles: Vec::new(),
    };
    let registry = EncoderRegistry::from_capabilities([capability]);
    let selection = registry.select(&request(64, 64)).unwrap();
    assert_eq!(selection.backend(), EncoderBackend::Vaapi);
    match selection.build_encoder(FrameFormat::rgb24(64, 64)) {
        Ok(SelectedEncoder::Vaapi(encoder)) => {
            // A real VAAPI encoder was built; it must identify itself as such
            // and never as the fake or software backend.
            assert_eq!(encoder.id(), "vaapi-h264");
        }
        Ok(other) => panic!("hardware selection built {other:?}, not a VAAPI encoder"),
        Err(error) => {
            // Hosts without a render node or ffmpeg must fail loudly with a
            // remediation instead of silently substituting software.
            assert_eq!(error.code, codes::ENCODER_UNAVAILABLE);
            assert!(error.remediation.is_some());
        }
    }
}
