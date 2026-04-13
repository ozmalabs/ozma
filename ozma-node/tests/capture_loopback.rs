//! Integration test: capture from a v4l2loopback device and verify that
//! ffmpeg produces a valid HLS manifest.
//!
//! Prerequisites (CI or local):
//!   sudo modprobe v4l2loopback devices=1 video_nr=10 card_label="ozma-test"
//!
//! Skipped until the capture pipeline and v4l_enum module are wired into
//! the ozma-node library crate.

#[tokio::test]
async fn test_hls_produced_from_loopback() {
    eprintln!("SKIP: capture module not yet integrated into ozma-node lib");
}
