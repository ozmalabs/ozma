// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end `/power/*` route tests against a fake QMP server.
//!
//! The fake behaves like a disposable QEMU monitor: it answers the commands
//! the node sends, and on `quit` it replies once and then stops accepting
//! connections (the QEMU process has exited). The routes must report HTTP 200
//! for the action, and a typed 502 afterwards instead of a fabricated state.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use ozma_node::controller_compat::{
    power_router, CompatStats, ConnectionInfo, ConnectionState, SINK_QMP,
};
use ozma_power::{PowerBackend, QmpPower};
use ozma_qmp::{QmpConfig, SharedQmpClient};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tower::ServiceExt;

const GREETING: &[u8] =
    br#"{"QMP":{"version":{"qemu":{"major":11,"minor":1,"micro":3},"package":"ozma-power-fake"},"capabilities":[]}}"#;

struct FakeQemu {
    root: PathBuf,
    path: PathBuf,
    handle: Option<JoinHandle<()>>,
    commands: UnboundedReceiver<Value>,
    exited: Arc<AtomicBool>,
}

impl FakeQemu {
    async fn spawn() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ozma-node-power-qmp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create fake QEMU temp dir");
        let path = root.join("qmp.sock");
        let listener = UnixListener::bind(&path).expect("bind fake QMP socket");
        let exited = Arc::new(AtomicBool::new(false));
        let exited_flag = Arc::clone(&exited);
        let (sender, commands) = unbounded_channel();

        let handle = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let quit = serve(stream, sender.clone()).await.unwrap_or(false);
                if quit {
                    exited_flag.store(true, Ordering::SeqCst);
                    // Dropping the listener leaves the socket path behind, but
                    // no process accepts on it; a reconnect now fails.
                    break;
                }
            }
        });

        Self {
            root,
            path,
            handle: Some(handle),
            commands,
            exited,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn has_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    async fn recv_command_named(&mut self, name: &str) -> Value {
        loop {
            let command = tokio::time::timeout(Duration::from_secs(2), self.commands.recv())
                .await
                .expect("timed out waiting for a fake QMP command")
                .expect("fake QMP command channel closed");
            if command.get("execute").and_then(Value::as_str) == Some(name) {
                return command;
            }
        }
    }

    async fn wait_for_exit(mut self) {
        if let Some(handle) = self.handle.take() {
            tokio::time::timeout(Duration::from_secs(2), handle)
                .await
                .expect("fake QEMU did not exit after quit")
                .expect("fake QEMU task panicked");
        }
        assert!(self.has_exited(), "fake QEMU did not record quit");
    }
}

impl Drop for FakeQemu {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Serve one fake QMP connection. Returns `true` when the client sent `quit`.
async fn serve(stream: UnixStream, commands: UnboundedSender<Value>) -> io::Result<bool> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    write_half.write_all(GREETING).await?;
    write_half.write_all(b"\n").await?;

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(false);
        }
        let command: Value = serde_json::from_str(line.trim())
            .expect("node sent invalid JSON to the fake QMP server");
        let name = command
            .get("execute")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let _ = commands.send(command);

        let (response, quit) = match name.as_str() {
            "qmp_capabilities" | "system_powerdown" | "system_reset" | "stop" | "cont"
            | "screendump" | "input-send-event" | "add_client" | "getfd" => {
                (json!({"return": {}}), false)
            }
            "query-status" => (
                json!({"return": {"status": "running", "running": true}}),
                false,
            ),
            // QEMU acknowledges quit and then exits. Everything after this is
            // a new process (or, here, a refused connection).
            "quit" => (json!({"return": {}}), true),
            other => (
                json!({
                    "error": {
                        "class": "CommandNotFound",
                        "desc": format!("unknown command {other}"),
                    }
                }),
                false,
            ),
        };
        write_half
            .write_all(&serde_json::to_vec(&response).expect("encode fake response"))
            .await?;
        write_half.write_all(b"\n").await?;
        write_half.flush().await?;

        if quit {
            return Ok(true);
        }
    }
}

async fn call(app: &Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

#[tokio::test]
async fn force_off_sends_quit_and_the_vm_is_unavailable_afterward() {
    let mut qemu = FakeQemu::spawn().await;
    let shared = SharedQmpClient::connect(
        QmpConfig::new(qemu.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();
    qemu.recv_command_named("qmp_capabilities").await;

    let power: ozma_node::controller_compat::SharedPower = Arc::new(tokio::sync::Mutex::new(
        Box::new(QmpPower::with_shared_client(shared.clone())) as Box<dyn PowerBackend>,
    ));
    let state = ConnectionState::new(
        ConnectionInfo::new("vm-a._ozma._udp.local.", "vm", 7382, 7384, 7331, SINK_QMP),
        CompatStats::default(),
    )
    .with_power(Some(power));
    let app = power_router(state);

    // ACPI `off` must not terminate QEMU.
    let (status, json) = call(&app, "POST", "/power/off").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["action"], "off");
    assert_eq!(
        qemu.recv_command_named("system_powerdown").await,
        json!({"execute": "system_powerdown"})
    );
    assert!(!qemu.has_exited());
    assert!(shared.is_connected().await);

    // Reset and resume remain real QMP commands over the same connection.
    let (status, _) = call(&app, "POST", "/power/reset").await;
    assert_eq!(status, StatusCode::OK);
    qemu.recv_command_named("system_reset").await;

    let (status, _) = call(&app, "POST", "/power/on").await;
    assert_eq!(status, StatusCode::OK);
    qemu.recv_command_named("cont").await;

    let (status, json) = call(&app, "GET", "/power/state").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["status"], "running");
    qemu.recv_command_named("query-status").await;

    // Hard stop: the alias maps to force-off and must send QEMU `quit`.
    let (status, json) = call(&app, "POST", "/power/stop").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"], true);
    assert_eq!(json["action"], "force-off");
    assert_eq!(
        qemu.recv_command_named("quit").await,
        json!({"execute": "quit"})
    );
    assert!(qemu.has_exited());

    // The QMP server is gone, so the node must report a typed unavailable
    // state rather than fabricating "shutoff".
    let (status, json) = call(&app, "GET", "/power/state").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(json["status"], "unknown");
    assert_eq!(json["available"], false);
    assert_eq!(json["code"], "qmp-connect");

    // A repeated force-off is also a typed error, not a second success.
    let (status, json) = call(&app, "POST", "/power/stop").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(json["ok"], false);
    assert_eq!(json["code"], "qmp-connect");

    qemu.wait_for_exit().await;
}
