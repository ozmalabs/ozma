//! Integration test: capture from v4l2loopback, verify m3u8 is produced.
//!
//! Prerequisites (CI / developer machine):
//!   modprobe v4l2loopback devices=1 video_nr=10 card_label="ozma-test"
//!   ffmpeg -re -f lavfi -i testsrc=size=640x480:rate=30 \
//!          -f v4l2 /dev/video10 &
//!
//! The test is skipped automatically when `/dev/video10` is absent.

use std::{path::PathBuf, time::Duration};

use ozma_node::capture::{CaptureDevice, EncoderConfig, MediaCapture};

#[tokio::test]
async fn test_hls_manifest_produced() {
    let dev_path = "/dev/video10";
    if !std::path::Path::new(dev_path).exists() {
        eprintln!("SKIP: {dev_path} not present (load v4l2loopback to run this test)");
        return;
    }

    let out_dir = PathBuf::from("/tmp/ozma-test-stream");
    let _ = std::fs::remove_dir_all(&out_dir);

    let dev = CaptureDevice::probe(dev_path).expect("probe v4l2loopback device");
    let enc = EncoderConfig::software_h264();
    let mut mc = MediaCapture::new(dev, enc, out_dir.clone());

    mc.start().await.expect("start capture");

    // Wait up to 10 s for the manifest to appear.
    let manifest = out_dir.join("stream.m3u8");
    let mut found = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if manifest.exists() {
            found = true;
            break;
        }
    }

    mc.stop().await;

    assert!(found, "stream.m3u8 was not produced within 10 s");

    let content = std::fs::read_to_string(&manifest).expect("read manifest");
    assert!(content.contains("#EXTM3U"), "manifest missing #EXTM3U header");
}
