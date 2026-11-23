//! Native PipeWire video tests.
//!
//! The real capture test is gated by `OZMA_TEST_PIPEWIRE_VIDEO=1` or a local
//! `pw-cli`/`PIPEWIRE_REMOTE`.  When the graph has no `Video/Source` node (no
//! camera, no screen-cast portal session) the test skips cleanly instead of
//! pretending to have captured a frame.

#![cfg(target_os = "linux")]

use std::{
    env, process,
    time::{Duration, Instant},
};

use ozma_pipewire::{
    probe_video_sources, probe_video_status, PipeWireError, PipeWireVideoSource, StreamTarget,
    VideoCaptureConfig, VideoProbeConfig,
};

fn forced() -> bool {
    matches!(
        env::var("OZMA_TEST_PIPEWIRE_VIDEO").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn has_pw_cli() -> bool {
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|dir| dir.join("pw-cli").is_file()))
        .unwrap_or(false)
}

fn enabled() -> bool {
    forced() || env::var_os("PIPEWIRE_REMOTE").is_some() || has_pw_cli()
}

fn report_or_skip(what: &str, error: &dyn std::fmt::Display) -> bool {
    if forced() {
        panic!("{what} failed while OZMA_TEST_PIPEWIRE_VIDEO is set: {error}");
    }
    eprintln!("skipping {what}: {error}");
    false
}

fn unique_bogus_remote() -> String {
    format!(
        "ozma-test-no-such-video-remote-{}-{}",
        process::id(),
        Instant::now().elapsed().as_nanos()
    )
}

#[test]
fn public_video_handles_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<PipeWireVideoSource>();
    assert_send::<PipeWireError>();
    assert_send::<ozma_pipewire::VideoFrame>();
    assert_send::<ozma_pipewire::VideoNode>();
    assert_send::<ozma_pipewire::VideoStatus>();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn video_probe_bogus_remote_is_bounded_and_typed() {
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        probe_video_sources_with_bogus_remote(),
    )
    .await
    .expect("video probe must not hang on a missing daemon");

    let error = result.expect_err("bogus remote unexpectedly reachable");
    assert!(
        matches!(
            error,
            PipeWireError::Unavailable { .. } | PipeWireError::ProbeTimeout { .. }
        ),
        "unexpected error for missing daemon: {error} ({})",
        error.code()
    );
    assert!(!error.remediation().is_empty());

    let status = tokio::time::timeout(
        Duration::from_secs(8),
        probe_video_status(VideoProbeConfig {
            remote: Some(unique_bogus_remote()),
            timeout: Duration::from_millis(1200),
            stop_timeout: Duration::from_millis(1200),
        }),
    )
    .await
    .expect("status probe must not hang on a missing daemon");
    assert!(!status.reachable);
    assert!(!status.has_video());
    let recorded = status.error.expect("unreachable status carries an error");
    assert!(
        recorded.code == "pipewire.unavailable" || recorded.code == "pipewire.probe_timeout",
        "unexpected status code: {}",
        recorded.code
    );
    assert!(!recorded.remediation.is_empty());
}

async fn probe_video_sources_with_bogus_remote(
) -> Result<Vec<ozma_pipewire::VideoNode>, PipeWireError> {
    ozma_pipewire::probe_video_sources_with(VideoProbeConfig {
        remote: Some(unique_bogus_remote()),
        timeout: Duration::from_millis(1200),
        stop_timeout: Duration::from_millis(1200),
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bogus_remote_video_source_fails_bounded() {
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        PipeWireVideoSource::new(VideoCaptureConfig {
            remote: Some(unique_bogus_remote()),
            startup_timeout: Duration::from_millis(1200),
            stop_timeout: Duration::from_millis(1200),
            ..VideoCaptureConfig::default()
        }),
    )
    .await
    .expect("video stream startup must not hang without a daemon");

    match result {
        Ok(_) => panic!("video capture unexpectedly opened a bogus remote"),
        Err(error) => assert!(
            matches!(
                error,
                PipeWireError::Unavailable { .. }
                    | PipeWireError::StartTimeout { .. }
                    | PipeWireError::Disconnected { .. }
                    | PipeWireError::Stream { .. }
                    | PipeWireError::VideoFormatUnsupported { .. }
                    | PipeWireError::CompressedVideoUnsupported { .. }
            ),
            "unexpected video capture error: {error}"
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_video_stream_delivers_monotonic_frames() {
    if !enabled() {
        eprintln!(
            "skipping real PipeWire video test (no OZMA_TEST_PIPEWIRE_VIDEO, PIPEWIRE_REMOTE, or pw-cli)"
        );
        return;
    }

    let sources = match probe_video_sources(Duration::from_secs(3)).await {
        Ok(sources) => sources,
        Err(error) => {
            report_or_skip("real PipeWire video probe", &error);
            return;
        }
    };
    let Some(node) = sources.first() else {
        eprintln!("skipping real video capture: the graph has no Video/Source node");
        return;
    };
    println!(
        "found video source id={} name={} description={} path={:?}",
        node.id, node.name, node.description, node.path
    );

    let mut source = match PipeWireVideoSource::new(VideoCaptureConfig {
        target: StreamTarget::Object(node.name.clone()),
        stream_name: "ozma-test-video".to_string(),
        width: 1280,
        height: 720,
        fps: 30,
        queue_capacity: 4,
        startup_timeout: Duration::from_secs(4),
        stop_timeout: Duration::from_secs(2),
        ..VideoCaptureConfig::default()
    })
    .await
    {
        Ok(source) => source,
        Err(error) => {
            report_or_skip("video stream startup", &error);
            return;
        }
    };

    let format = source.negotiated_format();
    println!("video negotiated {}", format.summary());
    assert!(format.width > 0 && format.height > 0);
    assert_eq!(
        format.stride,
        format.format.row_bytes(format.width).unwrap()
    );

    let deadline = Instant::now() + Duration::from_secs(8);
    let first = loop {
        match source.try_next_frame() {
            Ok(Some(frame)) => break frame,
            Ok(None) => {}
            Err(error) => {
                report_or_skip("video next_frame", &error);
                return;
            }
        }
        if Instant::now() >= deadline {
            let stats = source.stats();
            let error = source.take_error();
            report_or_skip(
                "video next_frame",
                &format!("source produced no frame within 8s (stats={stats:?}, error={error:?})"),
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(first.format, format.format);
    assert_eq!(first.width, format.width);
    assert_eq!(first.height, format.height);
    assert_eq!(first.stride, format.stride);
    assert!(!first.is_empty());
    assert!(first.is_complete());
    println!(
        "captured sequence={} pts={}us {} bytes",
        first.sequence,
        first.pts_micros,
        first.len()
    );

    let second = tokio::time::timeout(Duration::from_secs(8), source.next_frame())
        .await
        .expect("second frame is bounded")
        .expect("second frame");
    assert_eq!(second.sequence, first.sequence + 1);
    assert!(second.pts_micros >= first.pts_micros);
    assert_eq!(second.format, first.format);

    let stats = source.stats();
    println!(
        "video stats: frames={} dropped={} errors={} format={:?} {}x{}",
        stats.frames, stats.dropped_frames, stats.errors, stats.format, stats.width, stats.height
    );
    assert!(stats.frames >= 1);
    assert_eq!(stats.format, Some(format.format));
    source.stop();
}
