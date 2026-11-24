// SPDX-License-Identifier: AGPL-3.0-only
//! QMP power backend tests against a local fake QMP server.
//!
//! Real-QEMU coverage is opt-in via `OZMA_TEST_QMP_SOCKET` and `#[ignore]`.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{FakeQmpOptions, FakeQmpServer};
use ozma_power::{PowerBackend, PowerError, PowerState, QmpPower};
use ozma_qmp::{QmpClient, QmpConfig, QmpError, SharedQmpClient};
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
    for expected in ["system_powerdown", "system_reset", "cont", "quit"] {
        assert_eq!(
            server.recv_command().await,
            json!({"execute": expected}),
            "unexpected wire command for {expected}"
        );
    }
}

#[tokio::test]
async fn pause_and_resume_send_qmp_stop_and_cont() {
    let (mut power, mut server) = connected(FakeQmpOptions::default()).await;

    power.pause().await.unwrap();
    power.resume().await.unwrap();

    assert_eq!(
        server.recv_command_named("stop").await,
        json!({"execute": "stop"})
    );
    assert_eq!(
        server.recv_command_named("cont").await,
        json!({"execute": "cont"})
    );
    // Pause is the monitor stop, not `quit`; the control socket stays usable.
    assert!(power.is_connected());
    assert_eq!(power.query_status().await.unwrap(), PowerState::running());
    server.recv_command_named("query-status").await;
}

#[tokio::test]
async fn force_off_sends_quit_and_leaves_power_unavailable() {
    let (mut power, mut server) = connected(FakeQmpOptions::default()).await;

    power.stop().await.unwrap();

    // Hard power-off is QEMU `quit`, not the pause command.
    assert_eq!(
        server.recv_command_named("quit").await,
        json!({"execute": "quit"})
    );
    assert!(!power.is_connected());

    // The QEMU process is expected to be gone, so a later state query is a
    // typed backend error rather than a fabricated `shutoff`.
    assert!(matches!(
        power.query_status().await.unwrap_err(),
        PowerError::NotConnected
    ));
}

#[tokio::test]
async fn acpi_powerdown_keeps_qemu_running_and_connected() {
    let (mut power, mut server) = connected(FakeQmpOptions::default()).await;

    power.system_powerdown().await.unwrap();

    assert_eq!(
        server.recv_command_named("system_powerdown").await,
        json!({"execute": "system_powerdown"})
    );
    assert!(power.is_connected());
    assert_eq!(power.query_status().await.unwrap(), PowerState::running());
}

#[tokio::test]
async fn force_off_treats_a_closed_socket_as_a_successful_qemu_exit() {
    let mut server = FakeQmpServer::spawn_with(FakeQmpOptions {
        close_on: Some("quit".to_string()),
        ..FakeQmpOptions::default()
    })
    .await;
    let mut power = QmpPower::connect(config(&server)).await.unwrap();

    power.stop().await.unwrap();
    assert_eq!(
        server.recv_command_named("quit").await,
        json!({"execute": "quit"})
    );
    assert!(!power.is_connected());
}

#[tokio::test]
async fn force_off_refused_by_qmp_is_a_typed_error_and_keeps_the_connection() {
    let (mut power, mut server) = connected(FakeQmpOptions {
        error_on: Some("quit".to_string()),
        ..FakeQmpOptions::default()
    })
    .await;

    let error = power.stop().await.unwrap_err();
    match &error {
        PowerError::Qmp(QmpError::Command { command, .. }) => assert_eq!(command, "quit"),
        other => panic!("expected Qmp(Command), got {other:?}"),
    }
    assert_eq!(error.kind(), "qmp-command");
    server.recv_command_named("quit").await;

    // QEMU refused, so it is still running and the backend can retry.
    assert!(power.is_connected());
    assert_eq!(power.query_status().await.unwrap(), PowerState::running());
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
async fn shared_client_power_commands_use_the_supplied_connection() {
    let mut server = FakeQmpServer::spawn().await;
    let shared = SharedQmpClient::connect(config(&server)).await.unwrap();
    let mut power = QmpPower::with_shared_client(shared.clone());

    assert!(power.is_connected());
    assert!(power.shared_client().is_some());
    assert_eq!(power.query_status().await.unwrap(), PowerState::running());
    power.cont().await.unwrap();

    assert_eq!(
        server.recv_command().await,
        json!({"execute": "qmp_capabilities"})
    );
    assert_eq!(
        server.recv_command().await,
        json!({"execute": "query-status"})
    );
    assert_eq!(server.recv_command().await, json!({"execute": "cont"}));

    // Power did not take ownership of the shared connection.
    assert!(shared.is_connected().await);
}

#[tokio::test]
async fn shared_client_force_off_sends_quit_and_disconnects() {
    let mut server = FakeQmpServer::spawn().await;
    let shared = SharedQmpClient::connect(config(&server)).await.unwrap();
    let mut power = QmpPower::with_shared_client(shared.clone());

    power.stop().await.unwrap();

    assert_eq!(
        server.recv_command_named("quit").await,
        json!({"execute": "quit"})
    );
    assert!(!power.is_connected());
    assert!(!shared.is_connected().await);
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
