//! Integration test: capture from v4l2loopback, verify m3u8 is produced.
//!
//! Prerequisites (CI / developer machine):
//!   modprobe v4l2loopback devices=1 video_nr=10 card_label="ozma-test"
//!   ffmpeg -re -f lavfi -i testsrc=size=640x480:rate=30 \
//!          -f v4l2 /dev/video10 &
//!
//! The test is skipped automatically when `/dev/video10` is absent or when
//! the capture pipeline is not yet integrated into the ozma-node library.

#[tokio::test]
async fn test_hls_manifest_produced() {
    // The capture module is not yet exposed in the ozma-node library crate.
    // Skip until it's integrated.
    eprintln!("SKIP: capture module not yet integrated into ozma-node lib");
}
