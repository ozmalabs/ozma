// SPDX-License-Identifier: AGPL-3.0-only
//! Deterministic fake QMP server for `ozma-power` tests.

#![allow(dead_code)]

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

const GREETING: &[u8] =
    br#"{"QMP":{"version":{"qemu":{"major":8,"minor":0,"micro":0},"package":"ozma-power-fake"},"capabilities":[]}}"#;

/// Behavior of [`FakeQmpServer`].
#[derive(Debug, Clone)]
pub struct FakeQmpOptions {
    /// Status string returned by `query-status`.
    pub status: String,
    /// `running` flag returned by `query-status`.
    pub running: bool,
    /// Return a QMP error for this command.
    pub error_on: Option<String>,
    /// Close the connection instead of replying to this command.
    pub close_on: Option<String>,
}

impl Default for FakeQmpOptions {
    fn default() -> Self {
        Self {
            status: "running".to_string(),
            running: true,
            error_on: None,
            close_on: None,
        }
    }
}

/// A minimal QMP server over a temporary Unix socket.
///
/// Every received command (including `qmp_capabilities`) is forwarded to the
/// command channel so tests can assert exact wire commands.
pub struct FakeQmpServer {
    _dir: TempDir,
    path: PathBuf,
    handle: JoinHandle<()>,
    commands: UnboundedReceiver<Value>,
}

impl FakeQmpServer {
    pub async fn spawn() -> Self {
        Self::spawn_with(FakeQmpOptions::default()).await
    }

    pub async fn spawn_with(options: FakeQmpOptions) -> Self {
        let dir = tempfile::tempdir().expect("tempdir for fake QMP server");
        let path = dir.path().join("qmp.sock");
        let listener = UnixListener::bind(&path).expect("bind fake QMP socket");
        let (sender, commands) = unbounded_channel();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                if serve(stream, options.clone(), sender.clone())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            _dir: dir,
            path,
            handle,
            commands,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Receive the next command, including `qmp_capabilities`.
    pub async fn recv_command(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(2), self.commands.recv())
            .await
            .expect("timed out waiting for a fake QMP command")
            .expect("fake QMP command channel closed")
    }

    /// Receive commands until `name` arrives, discarding anything before it.
    pub async fn recv_command_named(&mut self, name: &str) -> Value {
        loop {
            let command = self.recv_command().await;
            if command.get("execute").and_then(Value::as_str) == Some(name) {
                return command;
            }
        }
    }
}

impl Drop for FakeQmpServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve(
    stream: UnixStream,
    options: FakeQmpOptions,
    commands: UnboundedSender<Value>,
) -> io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    write_half.write_all(GREETING).await?;
    write_half.write_all(b"\n").await?;

    let Some(capabilities) = read_json(&mut reader).await? else {
        return Ok(());
    };
    let _ = commands.send(capabilities);
    write_half.write_all(b"{\"return\":{}}\n").await?;

    loop {
        let Some(command) = read_json(&mut reader).await? else {
            return Ok(());
        };
        let name = command
            .get("execute")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let _ = commands.send(command);

        if options.close_on.as_deref() == Some(name.as_str()) {
            return Ok(());
        }

        if options.error_on.as_deref() == Some(name.as_str()) {
            write_half
                .write_all(
                    b"{\"error\":{\"class\":\"GenericError\",\"desc\":\"injected failure\"}}\n",
                )
                .await?;
            continue;
        }

        let response = match name.as_str() {
            "query-status" => json!({
                "return": {"status": options.status, "running": options.running}
            }),
            "system_powerdown" | "system_reset" | "stop" | "cont" | "quit" => json!({"return": {}}),
            _ => json!({
                "error": {
                    "class": "CommandNotFound",
                    "desc": format!("unknown command {name}")
                }
            }),
        };
        write_half
            .write_all(&serde_json::to_vec(&response).expect("encode fake response"))
            .await?;
        write_half.write_all(b"\n").await?;
    }
}

async fn read_json(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> io::Result<Option<Value>> {
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).await?;
    if bytes == 0 {
        return Ok(None);
    }
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        serde_json::from_str(line).expect("client sent invalid JSON to fake QMP server"),
    ))
}
