// SPDX-License-Identifier: AGPL-3.0-only
//! Node-level wiring proof for the single shared QMP control client.
//!
//! The fake server accepts exactly one connection. Power status, a capture
//! `screendump`, and an input `input-send-event` all go through the same
//! [`ozma_qmp::SharedQmpClient`]. Any second control client would be recorded
//! and fail the assertions.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ozma_capture::{CaptureRegistry, QmpDisplayConfig};
use ozma_input::{InputSink, QmpSink};
use ozma_media::FrameFormat;
use ozma_power::{PowerBackend, PowerState, QmpPower};
use ozma_protocol::input::{InputEvent, KeyModifiers};
use ozma_qmp::{QmpConfig, SharedQmpClient};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::task::JoinHandle;

const GREETING: &[u8] =
    br#"{"QMP":{"version":{"qemu":{"major":11,"minor":1,"micro":3},"package":"ozma-node-shared-fake"},"capabilities":[]}}"#;

const PIXELS_2X2: [u8; 12] = [
    0, 1, 2, 3, 4, 5, //
    6, 7, 8, 9, 10, 11,
];

fn ppm_2x2() -> Vec<u8> {
    let mut data = b"P6\n2 2\n255\n".to_vec();
    data.extend_from_slice(&PIXELS_2X2);
    data
}

struct SingleConnectionServer {
    root: PathBuf,
    path: PathBuf,
    handle: JoinHandle<()>,
    connections: Arc<AtomicUsize>,
    commands: UnboundedReceiver<Value>,
}

impl SingleConnectionServer {
    async fn spawn() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ozma-node-shared-qmp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create temp dir");
        let path = root.join("qmp.sock");
        let listener = UnixListener::bind(&path).expect("bind fake QMP socket");
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
                    // The node must never open a second control client.
                    drop(stream);
                    continue;
                }
                let sender = sender.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve(stream, sender).await {
                        eprintln!("shared node QMP server connection failed: {error}");
                    }
                });
            }
        });

        Self {
            root,
            path,
            handle,
            connections,
            commands,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    async fn recv_command_named(&mut self, name: &str) -> Value {
        loop {
            let command = tokio::time::timeout(Duration::from_secs(5), self.commands.recv())
                .await
                .expect("timed out waiting for a QMP command")
                .expect("QMP command channel closed");
            if command.get("execute").and_then(Value::as_str) == Some(name) {
                return command;
            }
        }
    }
}

impl Drop for SingleConnectionServer {
    fn drop(&mut self) {
        self.handle.abort();
        let _ = std::fs::remove_dir_all(&self.root);
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
        let command: Value =
            serde_json::from_str(line.trim()).expect("client sent invalid JSON to fake QMP");
        let name = command
            .get("execute")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let _ = commands.send(command.clone());

        match name.as_str() {
            "qmp_capabilities" | "system_powerdown" | "system_reset" | "stop" | "cont"
            | "input-send-event" | "add_client" | "getfd" | "quit" => {
                write_half.write_all(b"{\"return\":{}}\n").await?;
            }
            "query-status" => {
                write_half
                    .write_all(b"{\"return\":{\"status\":\"running\",\"running\":true}}\n")
                    .await?;
            }
            "screendump" => {
                let filename = command["arguments"]["filename"]
                    .as_str()
                    .expect("screendump filename");
                tokio::fs::write(filename, ppm_2x2()).await?;
                write_half.write_all(b"{\"return\":{}}\n").await?;
            }
            other => {
                let response = json!({
                    "error": {
                        "class": "CommandNotFound",
                        "desc": format!("unknown command {other}"),
                    }
                });
                write_half
                    .write_all(&serde_json::to_vec(&response).expect("encode fake response"))
                    .await?;
                write_half.write_all(b"\n").await?;
            }
        }
        write_half.flush().await?;
    }
}

#[tokio::test]
async fn power_capture_and_input_share_one_control_connection() {
    let mut server = SingleConnectionServer::spawn().await;
    let shared = SharedQmpClient::connect(
        QmpConfig::new(server.path()).with_connect_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();
    assert_eq!(server.connections(), 1);

    // Power role.
    let mut power = QmpPower::with_shared_client(shared.clone());
    assert_eq!(power.query_status().await.unwrap(), PowerState::running());

    // Capture role, through the same registry shape used by HLS and snapshots.
    let display = QmpDisplayConfig::new(server.path())
        .with_max_fps(240)
        .with_screendump_dir(server.root());
    let registry = CaptureRegistry::new().with_qmp_display_shared(display, shared.clone());
    let capabilities = registry.enumerate().unwrap();
    assert_eq!(capabilities.len(), 1);
    let mut source = registry.open(&capabilities[0]).unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), source.next_frame())
        .await
        .expect("shared screendump timed out")
        .expect("shared screendump failed");
    assert_eq!(frame.format(), FrameFormat::rgb24(2, 2));
    assert_eq!(frame.data(), &PIXELS_2X2[..]);
    source.stop().await.unwrap();

    // Input role, through the shared control client (no dedicated socket).
    let (sink, handle) = QmpSink::spawn_with_shared(shared.clone());
    sink.submit_report(&InputEvent::Key {
        code: 0x04,
        down: true,
        modifiers: KeyModifiers::default(),
    })
    .unwrap();
    server.recv_command_named("input-send-event").await;

    drop(sink);
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("shared input task should exit")
        .unwrap();

    // The capture source did not disconnect the shared client, and no second
    // control client was attempted.
    assert!(shared.is_connected().await);
    assert_eq!(server.connections(), 1);
}
