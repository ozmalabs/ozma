// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end decode tests: real OpenH264 encode -> decode, malformed input,
//! headless conversion, and a libopus roundtrip.
//!
//! Everything here runs without hardware, a display, or a network.

use ozma_decode::{
    decode_codes, AudioDecoder, DecodeMeta, DecodedVideoFrame, FrameBuffer, H264Decoder,
    I420Planes, VideoDecoder,
};
use ozma_encode::H264SoftwareEncoder;
use ozma_media::{EncodedFrame, Encoder, Frame, FrameFormat};
use ozma_protocol::{Codec, MediaFormat};

/// A deterministic colour ramp with enough structure that a decoder that
/// returned a blank frame would be obvious in the assertions.
fn synthetic_rgb(width: u32, height: u32) -> Vec<u8> {
    let width = width.max(1);
    let height = height.max(1);
    let mut data = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            data.push((x * 255 / width) as u8);
            data.push((y * 255 / height) as u8);
            data.push(((x ^ y) & 0xff) as u8);
        }
    }
    data
}

async fn encode_frames(width: u32, height: u32, count: u64) -> Vec<EncodedFrame> {
    let format = FrameFormat::rgb24(width, height);
    let request = MediaFormat::new(Codec::H264, width, height, 30, 2_000);
    let mut encoder = H264SoftwareEncoder::new(format, request).expect("software encoder");
    let mut frames = Vec::new();
    for sequence in 0..count {
        let pts = 100_000 + sequence * 33_333;
        // The first frame is forced to a keyframe by the encoder.
        let frame = Frame::with_data(
            format,
            pts,
            sequence,
            sequence == 0,
            synthetic_rgb(width, height),
        )
        .expect("synthetic frame");
        frames.push(encoder.encode(frame).await.expect("encode"));
    }
    frames
}

fn video_meta(encoded: &EncodedFrame) -> DecodeMeta {
    DecodeMeta::new(
        encoded.pts_micros(),
        encoded.sequence(),
        encoded.is_keyframe(),
    )
}

#[tokio::test]
async fn encodes_and_decodes_frames_with_metadata_intact() {
    let encoded = encode_frames(160, 120, 2).await;
    assert!(encoded[0].is_keyframe(), "first encoded frame is an IDR");

    let mut decoder = H264Decoder::new().expect("OpenH264 decoder");
    assert!(decoder.is_open());
    assert_eq!(decoder.dimensions(), None);
    assert_eq!(decoder.format().codec, Codec::H264);

    for (index, access_unit) in encoded.iter().enumerate() {
        let frame = decoder
            .decode(access_unit.data(), video_meta(access_unit))
            .await
            .unwrap_or_else(|error| panic!("frame {index} failed: {error}"))
            .unwrap_or_else(|| panic!("frame {index} produced no picture"));

        assert_eq!(frame.width, 160);
        assert_eq!(frame.height, 120);
        assert_eq!(frame.pts_micros, access_unit.pts_micros());
        assert_eq!(frame.sequence, access_unit.sequence());
        assert_eq!(frame.keyframe, access_unit.is_keyframe());
        assert_eq!(frame.yuv.y.len(), 160 * 120);
        assert_eq!(frame.yuv.u.len(), 80 * 60);
        assert_eq!(frame.yuv.v.len(), 80 * 60);
        assert!(frame.yuv_len() > 0);
        assert!(
            frame.yuv.y.iter().any(|value| *value != 0),
            "luma should not be all zero"
        );
        assert!(
            frame.yuv.u.iter().any(|value| *value != 128),
            "chroma should not be flat"
        );
    }

    assert_eq!(decoder.dimensions(), Some((160, 120)));
    assert_eq!(decoder.stats().frames_decoded, 2);
    assert_eq!(decoder.stats().errors, 0);
    assert!(decoder.flush().await.expect("flush").is_empty());
}

#[tokio::test]
async fn empty_and_malformed_units_are_structured_errors() {
    let mut decoder = H264Decoder::new().expect("OpenH264 decoder");
    let meta = DecodeMeta::new(0, 0, true);

    let empty = decoder.decode(&[], meta).await.unwrap_err();
    assert_eq!(empty.code(), decode_codes::EMPTY_ACCESS_UNIT);

    let not_annex_b = decoder
        .decode(b"this is not an h264 access unit", meta)
        .await
        .unwrap_err();
    assert_eq!(not_annex_b.code(), decode_codes::MALFORMED_ACCESS_UNIT);

    let truncated = decoder.decode(&[0, 0, 1], meta).await.unwrap_err();
    assert_eq!(truncated.code(), decode_codes::MALFORMED_ACCESS_UNIT);
    assert_eq!(
        truncated.to_api_error().code,
        decode_codes::MALFORMED_ACCESS_UNIT
    );

    assert_eq!(decoder.stats().errors, 3);
    assert_eq!(decoder.stats().frames_decoded, 0);

    // A failed unit must not poison the decoder: a real keyframe still works.
    let encoded = encode_frames(96, 96, 1).await;
    let frame = decoder
        .decode(encoded[0].data(), video_meta(&encoded[0]))
        .await
        .unwrap()
        .expect("recovery after malformed input");
    assert_eq!(frame.width, 96);
    assert_eq!(decoder.stats().frames_decoded, 1);
}

#[test]
fn rgba_and_ppm_conversions_are_headless_and_exact() {
    let frame = DecodedVideoFrame::new(
        DecodeMeta::new(11, 5, true),
        4,
        2,
        I420Planes::new(vec![81; 8], vec![90; 2], vec![240; 2]),
    )
    .unwrap();

    let rgba = frame.to_rgba();
    assert_eq!(rgba.len(), 4 * 2 * 4);
    assert!(rgba.chunks_exact(4).all(|pixel| pixel[3] == 255));

    let ppm = frame.to_ppm();
    let header = b"P6\n4 2\n255\n";
    assert!(ppm.starts_with(header));
    assert_eq!(ppm.len(), header.len() + 4 * 2 * 3);

    let path = std::env::temp_dir().join(format!(
        "ozma-decode-{}-{}.ppm",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    frame.write_ppm(&path).expect("write PPM");
    let written = std::fs::read(&path).expect("read PPM");
    let _ = std::fs::remove_file(&path);
    assert_eq!(written, ppm);
}

#[test]
fn frame_buffer_bounds_memory_and_counts_drops() {
    fn tiny(sequence: u64) -> DecodedVideoFrame {
        DecodedVideoFrame::new(
            DecodeMeta::new(sequence, sequence, sequence == 0),
            2,
            2,
            I420Planes::new(vec![16; 4], vec![128; 1], vec![128; 1]),
        )
        .unwrap()
    }

    let mut buffer = FrameBuffer::new(2);
    assert!(buffer.push(tiny(0)).is_none());
    assert!(buffer.push(tiny(1)).is_none());
    let evicted = buffer.push(tiny(2)).expect("oldest frame is evicted");
    assert_eq!(evicted.sequence, 0);
    assert_eq!(buffer.len(), 2);
    assert_eq!(buffer.stats().frames_decoded, 3);
    assert_eq!(buffer.stats().frames_dropped, 1);
    assert_eq!(buffer.peek().unwrap().sequence, 1);
    assert_eq!(buffer.peek_newest().unwrap().sequence, 2);
}

#[tokio::test]
async fn stop_is_idempotent_and_closes_the_decoder() {
    let mut decoder = H264Decoder::new().expect("OpenH264 decoder");
    decoder.stop().await.expect("first stop");
    decoder.stop().await.expect("second stop");
    assert!(!decoder.is_open());

    let error = decoder
        .decode(&[0, 0, 1, 0x65], DecodeMeta::default())
        .await
        .unwrap_err();
    assert_eq!(error.code(), decode_codes::CLOSED);
    assert!(decoder.flush().await.is_err());
}

#[tokio::test]
async fn opus_encode_decode_roundtrip_preserves_metadata() {
    use ozma_audio::{
        AudioEncoder, AudioFrame, EncoderConfig, OpusEncoder, CHANNELS, FRAME_SAMPLES, SAMPLE_RATE,
    };

    let mut encoder = OpusEncoder::new(EncoderConfig::opus()).expect("Opus encoder");
    let frame =
        AudioFrame::silent(33_000, 9, SAMPLE_RATE, CHANNELS, FRAME_SAMPLES).expect("silent frame");
    let packet = encoder.encode(frame).await.expect("Opus encode");

    let mut decoder = AudioDecoder::new().expect("Opus decoder");
    let decoded = decoder.decode(&packet).expect("Opus decode");
    assert_eq!(decoded.pts_micros, 33_000);
    assert_eq!(decoded.sequence, 9);
    assert_eq!(decoded.sample_rate, SAMPLE_RATE);
    assert_eq!(decoded.channels, CHANNELS);
    assert_eq!(
        decoded.pcm_i16().len(),
        FRAME_SAMPLES * usize::from(CHANNELS)
    );
    assert_eq!(decoded.frames_per_channel(), FRAME_SAMPLES);
    assert_eq!(decoded.duration_micros(), 20_000);

    // The raw-packet convenience path must produce the same PCM block.
    let from_bytes = decoder
        .decode_bytes(&packet.data, 33_000, 9)
        .expect("Opus decode from bytes");
    assert_eq!(from_bytes, decoded);
}

#[test]
fn video_decoder_trait_is_object_safe() {
    let _decoder: Box<dyn VideoDecoder> = Box::new(H264Decoder::new().expect("decoder"));
}
