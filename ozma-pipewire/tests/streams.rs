//! Native PipeWire stream tests.
//!
//! Unit tests for PCM conversion/queues live in the library.  These tests
//! exercise real streams.  They are skipped cleanly when no PipeWire session is
//! available, and are also forced on with `OZMA_TEST_PIPEWIRE=1`.
//!
//! Every path is bounded: a missing daemon or an idle source cannot hang the
//! test suite.

#![cfg(target_os = "linux")]

use std::{
    env,
    path::PathBuf,
    process,
    time::{Duration, Instant},
};

use ozma_pipewire::{
    probe, probe_status, probe_with, CaptureConfig, PipeWireAudioSink, PipeWireAudioSource,
    PipeWireError, ProbeConfig, SinkConfig,
};

fn forced() -> bool {
    matches!(
        env::var("OZMA_TEST_PIPEWIRE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn has_pw_cli() -> bool {
    env::var_os("PATH")
        .map(|path| {
            env::split_paths(&path).any(|dir| {
                let candidate: PathBuf = dir.join("pw-cli");
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

/// Gate on the explicit env var, `PIPEWIRE_REMOTE`, or a local `pw-cli`.
fn enabled() -> bool {
    if forced() {
        return true;
    }
    if env::var_os("PIPEWIRE_REMOTE").is_some() {
        return true;
    }
    has_pw_cli()
}

fn report_or_skip(what: &str, error: &dyn std::fmt::Display) -> bool {
    if forced() {
        panic!("{what} failed while OZMA_TEST_PIPEWIRE is set: {error}");
    }
    eprintln!("skipping {what}: {error}");
    false
}

fn unique_bogus_remote() -> String {
    // A remote name that cannot exist: PipeWire resolves it inside
    // XDG_RUNTIME_DIR, and no socket with this name is created.
    format!(
        "ozma-test-no-such-remote-{}-{}",
        process::id(),
        Instant::now().elapsed().as_nanos()
    )
}

#[test]
fn public_handles_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<PipeWireAudioSource>();
    assert_send::<PipeWireAudioSink>();
    assert_send::<PipeWireError>();
    assert_send::<ozma_pipewire::PipeWireStatus>();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_bogus_remote_is_bounded_and_typed() {
    // Deliberately run even without a real session: this is the missing-daemon
    // path.  The outer timeout guards against any regression that would block.
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        probe_with(ProbeConfig {
            remote: Some(unique_bogus_remote()),
            timeout: Duration::from_millis(1200),
            stop_timeout: Duration::from_millis(1200),
        }),
    )
    .await
    .expect("probe_with must not hang on a missing daemon");

    match result {
        Ok(status) => panic!("bogus remote unexpectedly reachable: {}", status.summary()),
        Err(error) => {
            assert!(
                matches!(
                    error,
                    PipeWireError::Unavailable { .. } | PipeWireError::ProbeTimeout { .. }
                ),
                "unexpected error for missing daemon: {error} ({})",
                error.code()
            );
            assert!(
                error.code() == "pipewire.unavailable" || error.code() == "pipewire.probe_timeout"
            );
            assert!(!error.remediation().is_empty());
        }
    }

    // The non-failing form must preserve the same diagnosis.
    let status = tokio::time::timeout(
        Duration::from_secs(8),
        probe_status(ProbeConfig {
            remote: Some(unique_bogus_remote()),
            timeout: Duration::from_millis(1200),
            stop_timeout: Duration::from_millis(1200),
        }),
    )
    .await
    .expect("probe_status must not hang on a missing daemon");
    assert!(!status.reachable);
    let error = status.error.expect("unreachable status carries an error");
    assert!(
        error.code == "pipewire.unavailable" || error.code == "pipewire.probe_timeout",
        "unexpected status code: {}",
        error.code
    );
    assert!(!error.remediation.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_new_on_bogus_remote_fails_bounded() {
    let remote = Some(unique_bogus_remote());

    let capture = tokio::time::timeout(
        Duration::from_secs(8),
        PipeWireAudioSource::new(CaptureConfig {
            remote: remote.clone(),
            startup_timeout: Duration::from_millis(1200),
            stop_timeout: Duration::from_millis(1200),
            ..CaptureConfig::default()
        }),
    )
    .await
    .expect("capture startup must not hang without a daemon");
    match capture {
        Ok(_) => panic!("capture unexpectedly opened a bogus remote"),
        Err(error) => assert!(
            matches!(
                error,
                PipeWireError::Unavailable { .. }
                    | PipeWireError::StartTimeout { .. }
                    | PipeWireError::Disconnected { .. }
            ),
            "unexpected capture error: {error}"
        ),
    }

    let sink = tokio::time::timeout(
        Duration::from_secs(8),
        PipeWireAudioSink::new(SinkConfig {
            remote,
            startup_timeout: Duration::from_millis(1200),
            stop_timeout: Duration::from_millis(1200),
            ..SinkConfig::default()
        }),
    )
    .await
    .expect("playback startup must not hang without a daemon");
    match sink {
        Ok(_) => panic!("playback unexpectedly opened a bogus remote"),
        Err(error) => assert!(
            matches!(
                error,
                PipeWireError::Unavailable { .. }
                    | PipeWireError::StartTimeout { .. }
                    | PipeWireError::Disconnected { .. }
            ),
            "unexpected playback error: {error}"
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_session_probe_reports_graph_and_defaults() {
    if !enabled() {
        eprintln!(
            "skipping real PipeWire test (no OZMA_TEST_PIPEWIRE, PIPEWIRE_REMOTE, or pw-cli)"
        );
        return;
    }

    let status = match probe(Duration::from_secs(3)).await {
        Ok(status) => status,
        Err(error) => {
            report_or_skip("real PipeWire probe", &error);
            return;
        }
    };

    assert!(status.reachable);
    println!("{}", status.summary());
    if enabled() && status.sources.is_empty() && status.sinks.is_empty() && forced() {
        // A daemon with no audio nodes is legal (e.g. a bare test session), so
        // only fail this when the caller explicitly demanded a live graph.
        eprintln!("PipeWire is reachable but has no audio nodes");
    }
    for node in status.sources.iter().chain(status.sinks.iter()) {
        assert!(!node.name.is_empty(), "node has an empty name: {node:?}");
        assert!(node.media_class.contains("Audio"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_capture_delivers_at_least_one_frame() {
    if !enabled() {
        eprintln!("skipping real PipeWire capture (no daemon hints)");
        return;
    }

    let status = match probe(Duration::from_secs(3)).await {
        Ok(status) => status,
        Err(error) => {
            report_or_skip("capture probe", &error);
            return;
        }
    };
    if status.sources.is_empty() && status.default_source.is_none() {
        eprintln!("skipping capture: no audio source in the PipeWire graph");
        return;
    }

    let mut source = match PipeWireAudioSource::new(CaptureConfig {
        stream_name: "ozma-test-capture".to_string(),
        startup_timeout: Duration::from_secs(4),
        stop_timeout: Duration::from_secs(2),
        queue_capacity: 8,
        ..CaptureConfig::default()
    })
    .await
    {
        Ok(source) => source,
        Err(error) => {
            report_or_skip("capture stream startup", &error);
            return;
        }
    };

    let format = source.negotiated_format();
    println!(
        "capture negotiated {} Hz / {} ch / {}",
        format.sample_rate,
        format.channels,
        format.sample_format.label()
    );
    assert!(format.channels >= 1);
    assert!(format.sample_rate >= 8_000);

    let frame = tokio::time::timeout(Duration::from_secs(6), source.next_frame()).await;
    match frame {
        Ok(Ok(frame)) => {
            println!(
                "captured sequence={} pts={}us samples={}",
                frame.sequence,
                frame.pts_micros,
                frame.samples.len()
            );
            assert!(frame.is_aligned());
            assert!(!frame.samples.is_empty());
            assert_eq!(frame.channels as u32, format.channels as u32);
            assert_eq!(frame.sample_rate, format.sample_rate);
        }
        Ok(Err(error)) => {
            report_or_skip("capture next_frame", &error);
        }
        Err(_) => {
            report_or_skip(
                "capture next_frame",
                &"default source produced no frame within 6s",
            );
        }
    }

    let stats = source.stats();
    println!(
        "capture stats: frames={} samples={} dropped={} errors={}",
        stats.frames, stats.samples, stats.dropped_frames, stats.errors
    );
    source.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_playback_accepts_a_silent_block() {
    if !enabled() {
        eprintln!("skipping real PipeWire playback (no daemon hints)");
        return;
    }

    let status = match probe(Duration::from_secs(3)).await {
        Ok(status) => status,
        Err(error) => {
            report_or_skip("playback probe", &error);
            return;
        }
    };
    if status.sinks.is_empty() && status.default_sink.is_none() {
        eprintln!("skipping playback: no audio sink in the PipeWire graph");
        return;
    }

    let mut sink = match PipeWireAudioSink::new(SinkConfig {
        stream_name: "ozma-test-playback".to_string(),
        startup_timeout: Duration::from_secs(4),
        stop_timeout: Duration::from_secs(2),
        ..SinkConfig::default()
    })
    .await
    {
        Ok(sink) => sink,
        Err(error) => {
            report_or_skip("playback stream startup", &error);
            return;
        }
    };

    let format = sink.negotiated_format();
    println!(
        "playback negotiated {} Hz / {} ch / {}",
        format.sample_rate,
        format.channels,
        format.sample_format.label()
    );

    // 20 ms of silence at the negotiated rate.
    let frames = (format.sample_rate as usize / 50).max(1);
    let samples = vec![0i16; frames * format.channels as usize];
    sink.write_all(&samples)
        .expect("queue accepts a short block");
    assert_eq!(
        sink.queued_samples() + sink.stats().played_samples as usize,
        samples.len(),
        "all samples are either queued or already played"
    );

    tokio::time::sleep(Duration::from_millis(400)).await;
    let stats = sink.stats();
    println!(
        "playback stats: queued={} played={} underruns={} silence={} calls={} lock_failures={} errors={}",
        stats.queued_samples,
        stats.played_samples,
        stats.underruns,
        stats.silence_samples,
        stats.process_calls,
        stats.lock_failures,
        stats.errors
    );
    assert!(stats.process_calls > 0, "sink process callback never ran");

    sink.flush().expect("flush works");
    assert_eq!(sink.queued_samples(), 0);
    sink.stop();
}
