// SPDX-License-Identifier: AGPL-3.0-only
//! Live `AudioRouter` checks against the host PipeWire/PulseAudio server.
//!
//! These are `#[ignore]`d and double-gated with `OZMA_AUDIO_LIVE=1` because
//! they load and unload a real `module-null-sink`. The sink name is unique per
//! process and is removed before the test returns, so no graph state is left
//! behind:
//!
//! ```text
//! OZMA_AUDIO_LIVE=1 cargo test -p ozma-audio --test router_live -- --ignored --nocapture
//! ```
//!
//! The test never touches the default sink/source and never plays audio.

use ozma_audio::{AudioRouter, AudioSinkSpec, AudioTarget, PipeWireAudioRouter, SweepRequest};

fn live() -> bool {
    std::env::var("OZMA_AUDIO_LIVE").as_deref() == Ok("1")
}

fn sink_listed(name: &str) -> bool {
    let output = std::process::Command::new("pactl")
        .args(["list", "short", "sinks"])
        .output()
        .expect("run `pactl list short sinks`");
    assert!(output.status.success(), "pactl list short sinks failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.split('\t').nth(1).map(str::trim) == Some(name))
}

#[tokio::test]
#[ignore = "requires a live PipeWire/PulseAudio server; set OZMA_AUDIO_LIVE=1"]
async fn ensure_sink_reuses_then_remove_unloads_live() {
    if !live() {
        eprintln!("skipped: set OZMA_AUDIO_LIVE=1 to run against the live audio server");
        return;
    }

    let name = format!("ozma-live-{}", std::process::id());
    let spec = AudioSinkSpec::new(name.clone(), format!("OzmaLive{}", std::process::id()))
        .expect("valid sink spec");
    let router = PipeWireAudioRouter::production();

    // A previous aborted run must not make this test ambiguous.
    router.remove_sink(&name).await.expect("clean start");

    let created = router.ensure_sink(spec.clone()).await.expect("create sink");
    assert!(created.created, "first ensure_sink must create the module");
    assert_eq!(created.name, name);
    assert_eq!(
        created.monitor_target,
        AudioTarget::monitor_name(&name),
        "the monitor target must be `<sink>.monitor`"
    );
    assert!(sink_listed(&name), "pactl must report the new sink");

    let reused = router.ensure_sink(spec).await.expect("reuse sink");
    assert!(!reused.created, "second ensure_sink must reuse the sink");

    router.remove_sink(&name).await.expect("remove sink");
    assert!(!sink_listed(&name), "remove_sink must unload the module");

    // Unknown sinks are a documented no-op.
    router
        .remove_sink(&name)
        .await
        .expect("second remove is a no-op");
}

/// Read-only graph enumeration against the live server.
#[tokio::test]
#[ignore = "requires a live PipeWire/PulseAudio server; set OZMA_AUDIO_LIVE=1"]
async fn graph_lists_the_live_server_read_only() {
    if !live() {
        eprintln!("skipped: set OZMA_AUDIO_LIVE=1 to run against the live audio server");
        return;
    }

    let router = PipeWireAudioRouter::production();
    let graph = router.graph().await.expect("live graph snapshot");
    assert!(!graph.nodes.is_empty(), "the live server must report nodes");
    assert!(
        graph
            .nodes
            .iter()
            .any(|node| node.media_class == "Audio/Sink"),
        "at least one sink is expected"
    );
    assert!(
        graph
            .nodes
            .iter()
            .any(|node| node.media_class == "Audio/Source"),
        "at least one source is expected"
    );
    assert!(!graph.links_available);
    assert!(graph.links.is_empty());
    println!(
        "live graph: {} nodes, {} streams, default sink {:?}, default source {:?}",
        graph.nodes.len(),
        graph.streams.len(),
        graph.default_sink,
        graph.default_source
    );
}

/// A generated sweep, played to a disposable null sink and captured from its
/// monitor. It never opens a microphone and never changes the default devices.
#[tokio::test]
#[ignore = "requires a live PipeWire/PulseAudio server; set OZMA_AUDIO_LIVE=1"]
async fn sweep_measures_a_disposable_null_sink_monitor_live() {
    if !live() {
        eprintln!("skipped: set OZMA_AUDIO_LIVE=1 to run against the live audio server");
        return;
    }

    let name = format!("ozma-sweep-live-{}", std::process::id());
    let spec = AudioSinkSpec::new(name.clone(), format!("OzmaSweepLive{}", std::process::id()))
        .expect("valid sink spec")
        .with_channels(2)
        .expect("valid channels")
        .with_rate(48_000)
        .expect("valid rate");
    let router = PipeWireAudioRouter::production();

    router.remove_sink(&name).await.expect("clean start");
    router.ensure_sink(spec).await.expect("create sweep sink");

    let mut request = SweepRequest::new(format!("{name}.monitor"), name.clone());
    request.start_hz = 100.0;
    request.end_hz = 8_000.0;
    request.duration_secs = 1.0;
    request.tail_secs = 0.3;
    request.start_delay_ms = 250;
    request.amplitude = 0.5;

    let outcome = router.sweep(request).await.expect("live measured sweep");
    let measurement = &outcome.profile.measurement;
    assert!(
        measurement.captured_frames > 48_000,
        "captured {} frames",
        measurement.captured_frames
    );
    assert!(
        measurement.peak_dbfs > -40.0,
        "sweep monitor peak was {:.1} dBFS",
        measurement.peak_dbfs
    );
    assert!(
        measurement.rms_dbfs > -40.0,
        "sweep monitor RMS was {:.1} dBFS",
        measurement.rms_dbfs
    );
    let analysis = outcome
        .profile
        .analysis
        .as_ref()
        .expect("band analysis from captured sweep");
    assert!(
        analysis.bands_measured >= 5,
        "only {} of 10 bands were measurable",
        analysis.bands_measured
    );
    for band in &analysis.bands {
        assert!(band.freq.is_finite());
        assert!(band.measured_db.is_finite());
        assert!(band.suggested_gain_db.is_finite());
        assert!(band.suggested_gain_db.abs() <= ozma_audio::MAX_SUGGESTED_GAIN_DB as f32);
        assert!((0.0..=1.0).contains(&band.confidence));
    }
    assert_eq!(outcome.profile.band_count, outcome.profile.bands.len());
    assert!(!outcome.profile.active);
    println!(
        "live sweep: {} frames, peak {:.1} dBFS, rms {:.1} dBFS, noise {:.1} dBFS, \
         rt60 {:?}, bands {}/10 (skipped {}), mean confidence {:.2}",
        measurement.captured_frames,
        measurement.peak_dbfs,
        measurement.rms_dbfs,
        measurement.noise_floor_dbfs,
        measurement.rt60_secs,
        analysis.bands_measured,
        analysis.bands_skipped,
        analysis.mean_confidence
    );
    for band in &analysis.bands {
        println!(
            "  {:>7.1} Hz: measured {:>+6.1} dB, target {:>+5.1} dB, \
             suggested {:>+5.1} dB, confidence {:.2}",
            band.freq, band.measured_db, band.target_db, band.suggested_gain_db, band.confidence
        );
    }

    // The same measured profile must round-trip through the opt-in store.
    let profile_root =
        std::env::temp_dir().join(format!("ozma-live-profile-{}", std::process::id()));
    let store = ozma_audio::RoomProfileStore::new(&profile_root);
    store.store(&outcome.profile).expect("store live profile");
    assert_eq!(
        store.load().expect("load live profile").as_ref(),
        Some(&outcome.profile),
        "stored profile must match the measured profile"
    );
    assert!(store.clear().expect("clear live profile"));
    let _ = std::fs::remove_dir_all(&profile_root);

    router.remove_sink(&name).await.expect("remove sweep sink");
}
