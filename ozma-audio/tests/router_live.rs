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

use ozma_audio::{AudioRouter, AudioSinkSpec, AudioTarget, PipeWireAudioRouter};

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
