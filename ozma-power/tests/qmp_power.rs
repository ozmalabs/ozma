// SPDX-License-Identifier: AGPL-3.0-only
//! QMP power backend tests against a local fake QMP server.
//!
//! Real-QEMU coverage is opt-in via `OZMA_TEST_QMP_SOCKET` and `#[ignore]`.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{FakeQmpOptions, FakeQmpServer};
use ozma_power::{PowerBackend, PowerError, PowerState, QmpPower};
use ozma_qmp::{QmpClient, QmpConfig, QmpError};
use serde_json::json;

fn config(server: &FakeQmpServer) -> QmpConfig {
    QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2))
}

async fn connected(options: FakeQmpOptions) -> (QmpPower, FakeQmpServer) {
    let server = FakeQmpServer::spawn_with(options).await;
    let power = QmpPower::connect(config(&server)).await.unwrap();
    (power, server)
}

#[tokio::test]
async fn connect_handshakes_and_queries_running() {
    let (mut power, mut server) = connected(FakeQmpOptions::default()).await;

    assert!(power.is_connected());
    assert_eq!(power.query_status().await.unwrap(), PowerState::running());

    assert_eq!(
        server.recv_command().await,
        json!({"execute": "qmp_capabilities"})
    );
    assert_eq!(
        server.recv_command().await,
        json!({"execute": "query-status"})
    );
}

#[tokio::test]
async fn query_status_maps_qmp_states_to_the_stable_vocabulary() {
    let cases = [
        ("running", true, "running"),
        ("paused", false, "paused"),
        ("shutdown", false, "shutdown"),
        ("suspended", false, "pmsuspended"),
        ("blocked", false, "blocked"),
        ("prelaunch", false, "unknown"),
        ("io-error", false, "unknown"),
        ("", false, "unknown"),
    ];

    for (status, running, expected) in cases {
        let (mut power, mut server) = connected(FakeQmpOptions {
            status: status.to_string(),
            running,
            ..FakeQmpOptions::default()
        })
        .await;

        let state = power.query_status().await.unwrap();
        assert_eq!(state.status, expected, "QMP status was {status:?}");
        assert_eq!(state.is_running(), expected == "running");
        server.recv_command_named("query-status").await;
    }
}

#[tokio::test]
async fn query_status_trusts_running_when_the_status_string_is_unknown() {
    let (mut power, _server) = connected(FakeQmpOptions {
        status: "prelaunch".to_string(),
        running: true,
        ..FakeQmpOptions::default()
    })
    .await;

    assert_eq!(power.query_status().await.unwrap(), PowerState::running());
}

#[tokio::test]
async fn power_commands_are_sent_in_order() {
    let (mut power, mut server) = connected(FakeQmpOptions::default()).await;

    power.system_powerdown().await.unwrap();
    power.system_reset().await.unwrap();
    power.cont().await.unwrap();
    power.stop().await.unwrap();

    assert_eq!(
        server.recv_command().await,
        json!({"execute": "qmp_capabilities"})
    );
    for expected in ["system_powerdown", "system_reset", "cont", "stop"] {
        assert_eq!(
            server.recv_command().await,
            json!({"execute": expected}),
            "unexpected wire command for {expected}"
        );
    }
}

#[tokio::test]
async fn qmp_error_is_typed_and_leaves_the_client_connected() {
    let (mut power, mut server) = connected(FakeQmpOptions {
        error_on: Some("system_reset".to_string()),
        ..FakeQmpOptions::default()
    })
    .await;

    let error = power.system_reset().await.unwrap_err();
    match &error {
        PowerError::Qmp(QmpError::Command { command, message }) => {
            assert_eq!(command, "system_reset");
            assert!(message.contains("injected failure"));
        }
        other => panic!("expected Qmp(Command), got {other:?}"),
    }
    assert!(error.to_string().contains("system_reset"));

    server.recv_command_named("system_reset").await;

    // A rejected command does not tear down the control socket.
    assert!(power.is_connected());
    assert_eq!(power.query_status().await.unwrap(), PowerState::running());
    server.recv_command_named("query-status").await;
}

#[tokio::test]
async fn not_connected_maps_to_the_backend_level_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.sock");
    let mut power = QmpPower::with_client(QmpClient::new(QmpConfig::new(missing)));

    assert!(!power.is_connected());
    assert!(matches!(
        power.query_status().await.unwrap_err(),
        PowerError::NotConnected
    ));
    assert!(matches!(
        power.cont().await.unwrap_err(),
        PowerError::NotConnected
    ));
    assert!(matches!(
        power.system_powerdown().await.unwrap_err(),
        PowerError::NotConnected
    ));
    assert!(matches!(
        power.system_reset().await.unwrap_err(),
        PowerError::NotConnected
    ));
    assert!(matches!(
        power.stop().await.unwrap_err(),
        PowerError::NotConnected
    ));
}

#[tokio::test]
async fn connect_to_a_missing_socket_is_a_qmp_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.sock");

    let error = QmpPower::connect(QmpConfig::new(missing))
        .await
        .unwrap_err();
    assert!(matches!(error, PowerError::Qmp(QmpError::Connect { .. })));
}

#[tokio::test]
#[ignore = "requires a real QEMU QMP socket via OZMA_TEST_QMP_SOCKET"]
async fn real_qemu_query_status() {
    let socket = std::env::var("OZMA_TEST_QMP_SOCKET")
        .map(PathBuf::from)
        .expect("set OZMA_TEST_QMP_SOCKET to a QEMU QMP socket");

    let mut power = QmpPower::connect(QmpConfig::new(socket)).await.unwrap();
    let state = power.query_status().await.unwrap();
    assert!(
        matches!(
            state.status.as_str(),
            "running"
                | "blocked"
                | "paused"
                | "shutdown"
                | "shutoff"
                | "crashed"
                | "pmsuspended"
                | "unknown"
        ),
        "unexpected status: {state}"
    );
}
