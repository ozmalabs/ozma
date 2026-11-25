// SPDX-License-Identifier: AGPL-3.0-only
//! Live room-EQ insertion checks against the host PipeWire server.
//!
//! These are `#[ignore]`d and double-gated with `OZMA_AUDIO_LIVE=1`. They create
//! a disposable null sink, load a real `libpipewire-module-filter-chain` inside a
//! short-lived `pipewire -c` client targeting that sink, then remove both. The
//! default sink/source are asserted unchanged and no stream is moved.
//!
//! ```text
//! OZMA_AUDIO_LIVE=1 cargo test -p ozma-audio --test eq_live -- --ignored --nocapture
//! ```

use std::process::Command;

use ozma_audio::{
    next_eq_sink_name, AudioRouter, AudioSinkSpec, PipeWireAudioRouter, RoomEqSpec, SweepBand,
};

fn live() -> bool {
    std::env::var("OZMA_AUDIO_LIVE").as_deref() == Ok("1")
}

fn pactl(args: &[&str]) -> String {
    let output = Command::new("pactl")
        .args(args)
        .output()
        .expect("run pactl");
    assert!(
        output.status.success(),
        "pactl {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn sink_listed(name: &str) -> bool {
    pactl(&["list", "short", "sinks"])
        .lines()
        .any(|line| line.split('\t').nth(1).map(str::trim) == Some(name))
}

fn default_sink() -> String {
    pactl(&["info"])
        .lines()
        .find_map(|line| line.strip_prefix("Default Sink: "))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn band(freq: f64, gain: f64, q: f64, band_type: &str) -> SweepBand {
    SweepBand {
        freq,
        gain,
        q,
        band_type: band_type.to_owned(),
    }
}

#[tokio::test]
#[ignore = "requires a live PipeWire server; set OZMA_AUDIO_LIVE=1"]
async fn filter_chain_applies_to_a_disposable_sink_and_removes_cleanly() {
    if !live() {
        eprintln!("skipped: set OZMA_AUDIO_LIVE=1 to run against the live audio server");
        return;
    }

    let name = format!("ozma-eq-live-{}", std::process::id());
    let spec = AudioSinkSpec::new(name.clone(), format!("OzmaEqLive{}", std::process::id()))
        .expect("valid sink spec")
        .with_channels(2)
        .expect("valid channels")
        .with_rate(48_000)
        .expect("valid rate");
    let router = PipeWireAudioRouter::production();
    let before_default = default_sink();

    router.remove_sink(&name).await.expect("clean start");
    router.ensure_sink(spec).await.expect("create target sink");

    let eq_sink = next_eq_sink_name("live-profile");
    let eq_spec = RoomEqSpec {
        profile_id: "live-profile".to_owned(),
        target_sink: name.clone(),
        eq_sink: eq_sink.clone(),
        bands: vec![
            band(63.0, 3.0, 1.0, "low_shelf"),
            band(1_000.0, -2.5, 1.4, "peaking"),
            band(8_000.0, -2.0, 0.7, "high_shelf"),
        ],
        rate: 48_000,
        channels: 2,
    };

    let application = router
        .apply_room_eq(eq_spec)
        .await
        .expect("load the live filter chain");
    assert_eq!(application.target_sink, name);
    assert_eq!(application.eq_sink, eq_sink);
    assert_eq!(application.active_bands, 3);
    assert!(application.module_id > 0);
    assert!(
        sink_listed(&eq_sink),
        "the filter-chain sink must be visible to pactl"
    );
    assert!(sink_listed(&name), "the target sink must remain");
    assert_eq!(
        default_sink(),
        before_default,
        "default sink must not change"
    );

    let links = Command::new("pw-link")
        .arg("-l")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    assert!(
        links.contains("output.filter-chain"),
        "the smart filter output node must exist; links:\n{links}"
    );
    assert!(
        links.contains(&format!("{name}:playback")),
        "the smart filter must be linked into the target sink; links:\n{links}"
    );

    println!(
        "live EQ: sink {eq_sink} -> target {name}, module {} ({} bands), default sink {before_default}",
        application.module_id, application.active_bands
    );

    let removed = router
        .remove_room_eq()
        .await
        .expect("remove the filter chain");
    assert_eq!(
        removed.as_ref().map(|app| app.eq_sink.as_str()),
        Some(eq_sink.as_str())
    );
    for _ in 0..40 {
        if !sink_listed(&eq_sink) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        !sink_listed(&eq_sink),
        "remove_room_eq must unload the EQ sink"
    );
    assert!(sink_listed(&name), "the target sink must remain");
    assert_eq!(
        default_sink(),
        before_default,
        "default sink must not change"
    );

    router.remove_sink(&name).await.expect("remove target sink");
    assert!(!sink_listed(&name));
}

#[tokio::test]
#[ignore = "requires a live PipeWire server; set OZMA_AUDIO_LIVE=1"]
async fn filter_chain_refuses_a_missing_target_without_leaking_a_sink() {
    if !live() {
        eprintln!("skipped: set OZMA_AUDIO_LIVE=1 to run against the live audio server");
        return;
    }

    let router = PipeWireAudioRouter::production();
    let eq_sink = next_eq_sink_name("missing-target");
    let error = router
        .apply_room_eq(RoomEqSpec {
            profile_id: "missing-target".to_owned(),
            target_sink: format!("ozma-absent-{}", std::process::id()),
            eq_sink: eq_sink.clone(),
            bands: vec![band(1_000.0, -2.0, 1.4, "peaking")],
            rate: 48_000,
            channels: 2,
        })
        .await
        .expect_err("a missing target must be refused");
    assert!(matches!(error, ozma_audio::AudioError::Refused { .. }));
    assert!(!sink_listed(&eq_sink), "no EQ sink may be left behind");
}
