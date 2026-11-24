// SPDX-License-Identifier: AGPL-3.0-only
//! Ports of `tests/unit/softnode/test_qmp_client.py` plus handshake, timeout,
//! event-skipping, and SCM_RIGHTS coverage.

mod common;

use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

use common::{FakeQmpOptions, FakeQmpServer, GetFdServer};
use ozma_qmp::{
    backoff_delay, Backoff, QmpClient, QmpConfig, QmpError, QmpInputEvent, QmpStatus,
    BACKOFF_FACTOR, BACKOFF_INITIAL, BACKOFF_MAX,
};
use serde_json::json;

fn missing_socket(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    dir.path().join(format!("{name}.sock"))
}

// ---------------------------------------------------------------------------
// Disconnected behavior (ported from TestQMPInputClient/TestQMPControlClient)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn initial_state_is_disconnected() {
    let dir = tempfile::tempdir().unwrap();
    let config = QmpConfig::dual(missing_socket(&dir, "ctrl"), missing_socket(&dir, "input"));
    let client = QmpClient::new(config);

    assert!(client.is_dual());
    assert!(!client.is_connected());
    assert!(!client.control_connected());
    assert!(!client.input_connected());
}

#[tokio::test]
async fn send_when_disconnected_is_not_connected() {
    let dir = tempfile::tempdir().unwrap();
    let client = QmpClient::new(QmpConfig::new(missing_socket(&dir, "ctrl")));

    let error = client
        .send_input_events(&[QmpInputEvent::key("a", true)])
        .await
        .unwrap_err();
    assert!(matches!(error, QmpError::NotConnected));
}

#[tokio::test]
async fn empty_events_are_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let client = QmpClient::new(QmpConfig::new(missing_socket(&dir, "ctrl")));

    // Like Python, an empty list is a successful no-op even while
    // disconnected: there is nothing to write.
    assert!(client.send_input_events(&[]).await.is_ok());
}

#[tokio::test]
async fn control_commands_when_disconnected() {
    let dir = tempfile::tempdir().unwrap();
    let client = QmpClient::new(QmpConfig::new(missing_socket(&dir, "ctrl")));

    assert!(matches!(
        client.query_status().await.unwrap_err(),
        QmpError::NotConnected
    ));
    assert!(matches!(
        client
            .screendump(&dir.path().join("shot.ppm"))
            .await
            .unwrap_err(),
        QmpError::NotConnected
    ));
    assert!(matches!(
        client.system_powerdown().await.unwrap_err(),
        QmpError::NotConnected
    ));
    assert!(matches!(
        client.system_reset().await.unwrap_err(),
        QmpError::NotConnected
    ));
    assert!(matches!(
        client.stop().await.unwrap_err(),
        QmpError::NotConnected
    ));
    assert!(matches!(
        client.cont().await.unwrap_err(),
        QmpError::NotConnected
    ));
    assert!(matches!(
        client.quit().await.unwrap_err(),
        QmpError::NotConnected
    ));
    assert!(matches!(
        client.add_client("dbus", "dbus-fd").await.unwrap_err(),
        QmpError::NotConnected
    ));
}

#[tokio::test]
async fn single_mode_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let client = QmpClient::new(QmpConfig::new(missing_socket(&dir, "ctrl")));
    assert!(!client.is_dual());
    assert!(!client.is_connected());
}

// ---------------------------------------------------------------------------
// Connect / handshake
// ---------------------------------------------------------------------------

#[tokio::test]
async fn connect_completes_handshake() {
    let mut server = FakeQmpServer::spawn().await;
    let config = QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2));

    let client = QmpClient::connect(config).await.unwrap();
    assert!(client.control_connected());
    assert!(client.is_connected());
    assert!(!client.input_connected());

    let capabilities = server.recv_command().await;
    assert_eq!(capabilities["execute"], "qmp_capabilities");
}

#[tokio::test]
async fn connect_rejects_bad_greeting() {
    let server = FakeQmpServer::spawn_with(FakeQmpOptions {
        bad_greeting: true,
        ..FakeQmpOptions::default()
    })
    .await;

    let error = QmpClient::connect(
        QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap_err();
    assert!(matches!(&error, QmpError::Handshake(_)), "{error:?}");
}

#[tokio::test]
async fn connect_to_missing_socket_is_typed() {
    let dir = tempfile::tempdir().unwrap();
    let error = QmpClient::connect(QmpConfig::new(missing_socket(&dir, "missing")))
        .await
        .unwrap_err();
    assert!(matches!(&error, QmpError::Connect { .. }), "{error:?}");
}

#[tokio::test]
async fn dual_mode_connected_requires_both_sockets() {
    let control = FakeQmpServer::spawn().await;
    let input = FakeQmpServer::spawn().await;
    let client = QmpClient::new(QmpConfig::dual(control.path(), input.path()));

    assert!(!client.is_connected());
    client.connect_control().await.unwrap();
    assert!(client.control_connected());
    assert!(!client.input_connected());
    assert!(!client.is_connected());

    client.connect_input().await.unwrap();
    assert!(client.input_connected());
    assert!(client.is_connected());
}

#[tokio::test]
async fn single_mode_connected_needs_only_control() {
    let server = FakeQmpServer::spawn().await;
    let client = QmpClient::new(QmpConfig::new(server.path()));

    assert!(!client.is_connected());
    client.connect_control().await.unwrap();
    assert!(client.control_connected());
    assert!(client.is_connected());
}

// ---------------------------------------------------------------------------
// Control commands
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_status_skips_async_events() {
    let mut server = FakeQmpServer::spawn_with(FakeQmpOptions {
        event_before: Some("query-status".to_string()),
        ..FakeQmpOptions::default()
    })
    .await;
    let client = QmpClient::connect(
        QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();

    let status = client.query_status().await.unwrap();
    assert_eq!(
        status,
        QmpStatus {
            running: true,
            status: "running".to_string(),
        }
    );

    let command = server.recv_command_named("query-status").await;
    assert_eq!(command["execute"], "query-status");
}

#[tokio::test]
async fn power_and_capture_commands() {
    let mut server = FakeQmpServer::spawn().await;
    let client = QmpClient::connect(
        QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();
    let screenshot = server.path().parent().unwrap().join("shot.ppm");

    client.system_powerdown().await.unwrap();
    client.system_reset().await.unwrap();
    client.stop().await.unwrap();
    client.cont().await.unwrap();
    client.screendump(&screenshot).await.unwrap();
    client.add_client("dbus", "dbus-fd").await.unwrap();

    for name in [
        "system_powerdown",
        "system_reset",
        "stop",
        "cont",
        "screendump",
        "add_client",
    ] {
        let command = server.recv_command_named(name).await;
        assert_eq!(command["execute"], name);
        match name {
            "screendump" => {
                assert_eq!(
                    command["arguments"]["filename"],
                    screenshot.to_string_lossy().as_ref()
                );
            }
            "add_client" => {
                assert_eq!(command["arguments"]["protocol"], "dbus");
                assert_eq!(command["arguments"]["fdname"], "dbus-fd");
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn qmp_error_is_typed_and_keeps_connection() {
    let mut server = FakeQmpServer::spawn_with(FakeQmpOptions {
        error_on: Some("query-status".to_string()),
        ..FakeQmpOptions::default()
    })
    .await;
    let client = QmpClient::connect(
        QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();

    let error = client.query_status().await.unwrap_err();
    match error {
        QmpError::Command { command, message } => {
            assert_eq!(command, "query-status");
            assert!(message.contains("GenericError"), "{message}");
        }
        other => panic!("expected QmpError::Command, got {other:?}"),
    }

    // A QMP-level error does not tear down the socket.
    assert!(client.control_connected());
    assert!(client.is_connected());
    let _ = server.recv_command_named("query-status").await;
}

#[tokio::test]
async fn command_timeout_is_typed_and_drops_connection() {
    let server = FakeQmpServer::spawn_with(FakeQmpOptions {
        delay: Some(("query-status".to_string(), Duration::from_secs(5))),
        ..FakeQmpOptions::default()
    })
    .await;
    let config = QmpConfig::new(server.path())
        .with_connect_timeout(Duration::from_secs(2))
        .with_command_timeout(Duration::from_millis(50));
    let client = QmpClient::connect(config).await.unwrap();

    let started = Instant::now();
    let error = client.query_status().await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2));
    match error {
        QmpError::Timeout {
            command,
            timeout: applied,
        } => {
            assert_eq!(command, "query-status");
            assert_eq!(applied, Duration::from_millis(50));
        }
        other => panic!("expected QmpError::Timeout, got {other:?}"),
    }
    assert!(!client.control_connected());
}

#[tokio::test]
async fn closed_connection_is_typed() {
    let mut server = FakeQmpServer::spawn_with(FakeQmpOptions {
        close_on: Some("query-status".to_string()),
        ..FakeQmpOptions::default()
    })
    .await;
    let client = QmpClient::connect(
        QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();

    let error = client.query_status().await.unwrap_err();
    assert!(matches!(&error, QmpError::ConnectionClosed), "{error:?}");
    assert!(!client.control_connected());

    let _ = server.recv_command_named("query-status").await;
}

// ---------------------------------------------------------------------------
// Input routing
// ---------------------------------------------------------------------------

fn expected_key_events() -> serde_json::Value {
    json!([{
        "type": "key",
        "data": {"down": true, "key": {"type": "qcode", "data": "a"}}
    }])
}

#[tokio::test]
async fn send_input_dual_mode_uses_input_socket() {
    let control = FakeQmpServer::spawn().await;
    let mut input = FakeQmpServer::spawn().await;
    let client = QmpClient::connect(
        QmpConfig::dual(control.path(), input.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();

    assert!(client.is_connected());
    client
        .send_input_events(&[QmpInputEvent::key("a", true)])
        .await
        .unwrap();

    let command = input.recv_command_named("input-send-event").await;
    assert_eq!(command["arguments"]["events"], expected_key_events());
}

#[tokio::test]
async fn dual_input_drain_prevents_socket_buffer_buildup() {
    let control = FakeQmpServer::spawn().await;
    let input = FakeQmpServer::spawn().await;
    let client = QmpClient::connect(
        QmpConfig::dual(control.path(), input.path())
            .with_connect_timeout(Duration::from_secs(2))
            .with_command_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();

    // Each command is ~35 KiB; twenty of them exceed a typical Unix socket
    // buffer. If the input reader were not draining QEMU's replies, one of
    // these writes would block and eventually time out.
    let events = vec![QmpInputEvent::key("a", false); 500];
    for _ in 0..20 {
        client.send_input_events(&events).await.unwrap();
    }

    // The control path is independent and still healthy.
    assert_eq!(client.query_status().await.unwrap().status, "running");
}

#[tokio::test]
async fn send_input_single_mode_falls_back_to_control() {
    let mut server = FakeQmpServer::spawn().await;
    let client = QmpClient::connect(
        QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();

    assert!(!client.is_dual());
    client
        .send_input_events(&[QmpInputEvent::key("a", true)])
        .await
        .unwrap();

    let command = server.recv_command_named("input-send-event").await;
    assert_eq!(command["arguments"]["events"], expected_key_events());
}

#[tokio::test]
async fn send_input_reports_immediate_qmp_error_in_single_mode() {
    let mut server = FakeQmpServer::spawn_with(FakeQmpOptions {
        error_on: Some("input-send-event".to_string()),
        ..FakeQmpOptions::default()
    })
    .await;
    let client = QmpClient::connect(
        QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();

    let error = client
        .send_input_events(&[QmpInputEvent::key("a", true)])
        .await
        .unwrap_err();
    assert!(matches!(&error, QmpError::Command { .. }), "{error:?}");
    let _ = server.recv_command_named("input-send-event").await;
}

// ---------------------------------------------------------------------------
// quit (hard QEMU exit)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn quit_sends_quit_and_invalidates_the_control_connection() {
    let mut server = FakeQmpServer::spawn().await;
    let client = QmpClient::connect(
        QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();
    server.recv_command_named("qmp_capabilities").await;

    client.quit().await.unwrap();
    assert_eq!(
        server.recv_command_named("quit").await,
        json!({"execute": "quit"})
    );

    // QEMU is expected to be gone; the socket must not be advertised as
    // reusable, and a second quit cannot silently succeed.
    assert!(!client.control_connected());
    assert!(!client.is_connected());
    assert!(matches!(
        client.quit().await.unwrap_err(),
        QmpError::NotConnected
    ));
}

#[tokio::test]
async fn quit_succeeds_when_qemu_exits_without_a_reply() {
    let server = FakeQmpServer::spawn_with(FakeQmpOptions {
        close_on: Some("quit".to_string()),
        ..FakeQmpOptions::default()
    })
    .await;
    let client = QmpClient::connect(
        QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();

    // The fake closes after reading quit. The command was delivered, so a
    // missing reply is the expected "QEMU exited" outcome, not an error.
    client.quit().await.unwrap();
    assert!(!client.control_connected());
}

#[tokio::test]
async fn quit_returns_a_typed_error_when_qmp_refuses_and_keeps_the_connection() {
    let mut server = FakeQmpServer::spawn_with(FakeQmpOptions {
        error_on: Some("quit".to_string()),
        ..FakeQmpOptions::default()
    })
    .await;
    let client = QmpClient::connect(
        QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();

    let error = client.quit().await.unwrap_err();
    match &error {
        QmpError::Command { command, message } => {
            assert_eq!(command, "quit");
            assert!(message.contains("injected failure"));
        }
        other => panic!("expected QmpError::Command, got {other:?}"),
    }
    server.recv_command_named("quit").await;

    // A refusal means QEMU is still alive, so the connection remains usable.
    assert!(client.is_connected());
    assert_eq!(client.query_status().await.unwrap().status, "running");
}

// ---------------------------------------------------------------------------
// Backoff
// ---------------------------------------------------------------------------

#[test]
fn backoff_constants_match_python() {
    assert_eq!(BACKOFF_INITIAL, Duration::from_millis(500));
    assert_eq!(BACKOFF_MAX, Duration::from_secs(5));
    assert_eq!(BACKOFF_FACTOR, 2.0);
}

#[test]
fn backoff_caps_at_max() {
    let mut delay = BACKOFF_INITIAL;
    for _ in 0..10 {
        delay = std::cmp::min(
            Duration::from_secs_f32(delay.as_secs_f32() * BACKOFF_FACTOR),
            BACKOFF_MAX,
        );
    }
    assert_eq!(delay, BACKOFF_MAX);

    let mut backoff = Backoff::new(BACKOFF_INITIAL, BACKOFF_MAX, BACKOFF_FACTOR);
    assert_eq!(backoff.next_delay(), Duration::from_millis(500));
    assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    assert_eq!(backoff.next_delay(), Duration::from_secs(2));
    assert_eq!(backoff.next_delay(), Duration::from_secs(4));
    assert_eq!(backoff.next_delay(), Duration::from_secs(5));
    for _ in 0..10 {
        assert_eq!(backoff.next_delay(), BACKOFF_MAX);
    }
    assert_eq!(
        backoff_delay(u32::MAX, BACKOFF_INITIAL, BACKOFF_MAX, BACKOFF_FACTOR),
        BACKOFF_MAX
    );
}

// ---------------------------------------------------------------------------
// getfd / SCM_RIGHTS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn getfd_passes_descriptor_with_scm_rights() {
    let mut server = GetFdServer::spawn().await;
    let client = QmpClient::connect(
        QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();

    let file = tempfile::tempfile().unwrap();
    client.getfd("ozma-test", file.as_raw_fd()).await.unwrap();

    let received = server.recv().await;
    assert!(received >= 0, "server received an invalid descriptor");
    nix::unistd::close(received).unwrap();
}

// ---------------------------------------------------------------------------
// Real QEMU (opt-in)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a real QEMU QMP socket in OZMA_QMP_SOCKET"]
async fn real_qemu_query_status() {
    let path =
        std::env::var("OZMA_QMP_SOCKET").expect("set OZMA_QMP_SOCKET to run the real-QEMU test");
    let client = QmpClient::connect(QmpConfig::new(Path::new(&path)))
        .await
        .unwrap();
    let status = client.query_status().await.unwrap();
    assert!(!status.status.is_empty());
}
