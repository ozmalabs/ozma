//! Integration tests for ozma-pipewire.
//!
//! The PipeWire tests require a running PipeWire daemon and are therefore
//! gated behind `#[cfg(target_os = "linux")]`.  The CPAL snapshot test
//! runs on all platforms.

#[cfg(target_os = "linux")]
mod pipewire_tests {
    use ozma_pipewire::build_router;

    /// Smoke-test: build a router and take a snapshot.
    ///
    /// Requires a running PipeWire session.  Skip gracefully if PipeWire
    /// is not available (CI without a display server).
    #[tokio::test]
    async fn test_snapshot_linux() {
        let router = match build_router().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Skipping PipeWire test (no daemon?): {e}");
                return;
            }
        };

        let snap = router.snapshot().await.expect("snapshot failed");
        println!("nodes={}, links={}", snap.nodes.len(), snap.links.len());
    }

    /// Verify that subscribing to events doesn't panic.
    #[tokio::test]
    async fn test_subscribe_linux() {
        let router = match build_router().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Skipping PipeWire test (no daemon?): {e}");
                return;
            }
        };

        let mut rx = router.subscribe();
        // No events expected in a short window, but the channel must be live.
        assert!(rx.try_recv().is_err());
    }

    /// Verify that create_link returns a meaningful error when ports are absent.
    #[tokio::test]
    async fn test_create_link_missing_nodes() {
        let router = match build_router().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Skipping PipeWire test (no daemon?): {e}");
                return;
            }
        };

        let result = router
            .create_link(ozma_pipewire::LinkRequest {
                output_node_id: 99999,
                input_node_id: 99998,
            })
            .await;

        assert!(result.is_err(), "expected error for non-existent nodes");
    }
}

#[cfg(not(target_os = "linux"))]
mod cpal_tests {
    use ozma_pipewire::build_router;

    #[tokio::test]
    async fn test_snapshot_cpal() {
        let router = build_router().await.expect("CPAL router init failed");
        let snap = router.snapshot().await.expect("snapshot failed");
        println!("CPAL nodes={}", snap.nodes.len());
        // Links are always empty on CPAL.
        assert!(snap.links.is_empty());
    }

    #[tokio::test]
    async fn test_create_link_unsupported() {
        let router = build_router().await.expect("CPAL router init failed");
        let err = router
            .create_link(ozma_pipewire::LinkRequest {
                output_node_id: 0,
                input_node_id: 1,
            })
            .await;
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("not supported"), "unexpected error: {msg}");
    }
}
