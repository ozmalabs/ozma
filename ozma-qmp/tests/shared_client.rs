// SPDX-License-Identifier: AGPL-3.0-only
//! Prove that a [`SharedQmpClient`] uses exactly one QMP control connection
//! even when several callers issue commands concurrently.
//!
//! The fake server accepts the first connection and records every later
//! connection attempt. Any second control client would either block on QEMU's
//! one-client chardev or show up in `connections()`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ozma_qmp::{QmpConfig, QmpError, QmpInputEvent, SharedQmpClient};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::task::JoinHandle;

const GREETING: &[u8] =
    br#"{"QMP":{"version":{"qemu":{"major":11,"minor":1,"micro":3},"package":"ozma-shared-fake"},"capabilities":[]}}"#;

struct SingleConnectionServer {
    _dir: TempDir,
    path: PathBuf,
    handle: JoinHandle<()>,
    connections: Arc<AtomicUsize>,
    commands: UnboundedReceiver<Value>,
}

impl SingleConnectionServer {
    async fn spawn() -> Self {
        let dir = tempfile::tempdir().expect("tempdir for shared QMP server");
        let path = dir.path().join("qmp.sock");
        let listener = UnixListener::bind(&path).expect("bind shared QMP socket");
        let connections = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::clone(&connections);
        let (sender, commands) = unbounded_channel();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let index = accepted.fetch_add(1, Ordering::SeqCst);
                if index > 0 {
                    // A second control client is exactly the bug this test
                    // guards against. Drop it without a greeting so the client
                    // sees a closed handshake rather than silent success.
                    drop(stream);
                    continue;
                }
                let sender = sender.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve(stream, sender).await {
                        eprintln!("shared QMP server connection failed: {error}");
                    }
                });
            }
        });

        Self {
            _dir: dir,
            path,
            handle,
            connections,
            commands,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    async fn recv_command_named(&mut self, name: &str) -> Value {
        loop {
            let command = tokio::time::timeout(Duration::from_secs(5), self.commands.recv())
                .await
                .expect("timed out waiting for a shared QMP command")
                .expect("shared QMP command channel closed");
            if command.get("execute").and_then(Value::as_str) == Some(name) {
                return command;
            }
        }
    }
}

impl Drop for SingleConnectionServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve(
    stream: UnixStream,
    commands: tokio::sync::mpsc::UnboundedSender<Value>,
) -> io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    write_half.write_all(GREETING).await?;
    write_half.write_all(b"\n").await?;

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let command: Value = serde_json::from_str(line.trim())
            .expect("client sent invalid JSON to shared QMP server");
        let name = command
            .get("execute")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let _ = commands.send(command);

        let response = match name.as_str() {
            "qmp_capabilities" | "system_powerdown" | "system_reset" | "stop" | "cont"
            | "screendump" | "input-send-event" | "add_client" | "getfd" => json!({"return": {}}),
            "query-status" => json!({"return": {"status": "running", "running": true}}),
            other => json!({
                "error": {
                    "class": "CommandNotFound",
                    "desc": format!("unknown command {other}"),
                }
            }),
        };
        write_half
            .write_all(&serde_json::to_vec(&response).expect("encode fake response"))
            .await?;
        write_half.write_all(b"\n").await?;
    }
}

#[tokio::test]
async fn shared_clients_issue_commands_over_one_connection() {
    let mut server = SingleConnectionServer::spawn().await;
    let shared = SharedQmpClient::connect(
        QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();
    assert_eq!(server.connections(), 1);

    let first = shared.clone();
    let second = shared.clone();
    let third = shared.clone();
    let fourth = shared.clone();
    let shot = server.path().parent().unwrap().join("shot.ppm");
    let events = [QmpInputEvent::key("a", true)];

    let (status, powerdown, dump, input) = tokio::join!(
        first.query_status(),
        second.system_powerdown(),
        third.screendump(&shot),
        fourth.send_input_events(&events),
    );
    assert!(status.unwrap().running);
    powerdown.unwrap();
    dump.unwrap();
    input.unwrap();

    // Every command reached the one server, in some serialized order.
    for name in [
        "query-status",
        "system_powerdown",
        "screendump",
        "input-send-event",
    ] {
        server.recv_command_named(name).await;
    }

    // A clone still sees the same live connection, and no second control
    // connection was attempted.
    assert!(shared.is_connected().await);
    assert_eq!(server.connections(), 1);
}

#[tokio::test]
async fn disconnected_shared_client_returns_a_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    let shared = SharedQmpClient::new(QmpConfig::new(dir.path().join("missing.sock")));

    let error = shared.query_status().await.unwrap_err();
    assert!(
        matches!(error, QmpError::Connect { .. } | QmpError::Timeout { .. }),
        "expected a typed connection failure, got {error:?}"
    );
    assert!(!shared.is_connected().await);
}
