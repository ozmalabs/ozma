//! Integration test: capture from a v4l2loopback device and verify that
//! ffmpeg produces a valid HLS manifest.
//!
//! Prerequisites (CI or local):
//!   sudo modprobe v4l2loopback devices=1 video_nr=10 card_label="ozma-test"
//!   # Feed a test signal into the loopback device, e.g.:
//!   #   ffmpeg -re -f lavfi -i testsrc=size=1280x720:rate=25 -f v4l2 /dev/video10
//!
//! The test is skipped automatically if `/dev/video10` does not exist.

use std::path::PathBuf;
use std::time::Duration;

use ozma_node::{capture::{EncoderConfig, MediaCapture}, v4l_enum};

const LOOPBACK_DEV: &str = "/dev/video10";
const TIMEOUT_SECS: u64 = 15;

#[tokio::test]
async fn test_hls_produced_from_loopback() {
    if !std::path::Path::new(LOOPBACK_DEV).exists() {
        eprintln!("SKIP: {} not found — load v4l2loopback first", LOOPBACK_DEV);
        return;
    }

    let devices = v4l_enum::enumerate();
    let dev = devices
        .into_iter()
        .find(|d| d.path == std::path::Path::new(LOOPBACK_DEV))
        .expect("v4l2loopback device not found by enumerator");

    let tmp = tempfile::tempdir().expect("tempdir");
    let hls_dir = PathBuf::from(tmp.path());

    let enc = EncoderConfig::software_h264();
    let mut mc = MediaCapture::new(dev, enc, hls_dir.clone())
        .with_fps(25)
        .with_hls(1.0, 4);

    mc.start().await.expect("start capture");

    // Poll until the manifest appears or we time out.
    let manifest = hls_dir.join("stream.m3u8");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(TIMEOUT_SECS);
    loop {
        let ready = manifest.exists()
            && std::fs::metadata(&manifest)
                .map(|m| m.len() > 0)
                .unwrap_or(false);
        if ready {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            mc.stop().await;
            panic!("stream.m3u8 not produced within {}s", TIMEOUT_SECS);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    mc.stop().await;

    let content = std::fs::read_to_string(&manifest).expect("read manifest");
    assert!(content.contains("#EXTM3U"), "manifest missing #EXTM3U header");
    assert!(content.contains(".ts"), "manifest contains no .ts segments");
}
