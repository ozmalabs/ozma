// SPDX-License-Identifier: AGPL-3.0-only
//! ozma-drivers — standalone binary entry point.
//!
//! In production the driver surfaces are embedded in the controller process.
//! This binary is useful for hardware bring-up and manual testing.

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    #[cfg(feature = "streamdeck")]
    {
        use ozma_drivers::{
            control_surface::{ControlEvent, ControlSurface, ScenarioInfo},
            streamdeck::StreamDeckSurface,
        };
        use tokio::sync::mpsc;

        let mut surfaces = StreamDeckSurface::discover().await;
        if surfaces.is_empty() {
            tracing::warn!("No Stream Deck devices found.");
            return;
        }

        let (tx, mut rx) = mpsc::channel::<ControlEvent>(64);

        for surface in &mut surfaces {
            tracing::info!("Starting: {}", surface.description());
            surface.start(tx.clone()).await.expect("start failed");

            let scenarios = vec![
                ScenarioInfo { id: "s1".into(), name: "Gaming".into(), color: "#ff4400".into() },
                ScenarioInfo { id: "s2".into(), name: "Work".into(),   color: "#0044ff".into() },
                ScenarioInfo { id: "s3".into(), name: "Media".into(),  color: "#00cc44".into() },
            ];
            surface
                .update_scenarios(&scenarios, Some("s1"))
                .await
                .expect("update_scenarios failed");
        }

        tracing::info!("Listening for key events (Ctrl-C to quit)…");
        loop {
            tokio::select! {
                Some(ev) = rx.recv() => {
                    tracing::info!(
                        surface = %ev.surface_id,
                        control = %ev.control_name,
                        payload = %ev.payload,
                        "key event"
                    );
                }
                _ = tokio::signal::ctrl_c() => break,
            }
        }

        for surface in &mut surfaces {
            surface.stop().await.ok();
        }
    }

    #[cfg(not(feature = "streamdeck"))]
    tracing::warn!("Built without streamdeck feature — nothing to do.");
}
