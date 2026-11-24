// SPDX-License-Identifier: AGPL-3.0-only
//! Test fixtures: a fake QMP server and an SCM_RIGHTS `getfd` server.

#![allow(dead_code)]

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const GREETING: &[u8] =
    br#"{"QMP":{"version":{"qemu":{"major":8,"minor":0,"micro":0},"package":"ozma-fake"},"capabilities":[]}}"#;

/// Behavior switches for [`FakeQmpServer`].
#[derive(Debug, Clone, Default)]
pub struct FakeQmpOptions {
    /// Delay a response to this command.
    pub delay: Option<(String, Duration)>,
    /// Send an asynchronous `{"event": ...}` before this command's response.
    pub event_before: Option<String>,
    /// Return a QMP error for this command.
    pub error_on: Option<String>,
    /// Close the connection instead of replying to this command.
    pub close_on: Option<String>,
    /// Send a non-QMP greeting and close.
    pub bad_greeting: bool,
}

/// A minimal QMP server over a temporary Unix socket.
///
/// Connections are served sequentially. Every received command (including the
/// initial `qmp_capabilities`) is forwarded to the command channel.
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

    /// Return true when no command is currently queued.
    pub fn try_recv_command(&mut self) -> Option<Value> {
        self.commands.try_recv().ok()
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

    if options.bad_greeting {
        write_half.write_all(b"{\"not_qmp\":true}\n").await?;
        write_half.flush().await?;
        return Ok(());
    }

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

        if let Some((delayed, delay)) = &options.delay {
            if delayed == &name {
                tokio::time::sleep(*delay).await;
            }
        }

        if options.event_before.as_deref() == Some(name.as_str()) {
            write_half
                .write_all(b"{\"event\":\"RESUME\",\"data\":{}}\n")
                .await?;
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
                "return": {"status": "running", "running": true}
            }),
            "screendump" | "system_powerdown" | "system_reset" | "stop" | "cont"
            | "input-send-event" | "add_client" | "getfd" | "quit" => json!({"return": {}}),
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

/// A one-connection server that receives a `getfd` command with SCM_RIGHTS.
pub struct GetFdServer {
    _dir: TempDir,
    path: PathBuf,
    handle: JoinHandle<()>,
    received: oneshot::Receiver<RawFd>,
}

impl GetFdServer {
    pub async fn spawn() -> Self {
        let dir = tempfile::tempdir().expect("tempdir for getfd server");
        let path = dir.path().join("qmp.sock");
        let listener = UnixListener::bind(&path).expect("bind getfd socket");
        let (sender, received) = oneshot::channel();

        let handle = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            match serve_getfd(stream).await {
                Ok(fd) => {
                    let _ = sender.send(fd);
                }
                Err(error) => {
                    eprintln!("getfd test server failed: {error}");
                }
            }
        });

        Self {
            _dir: dir,
            path,
            handle,
            received,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Wait for the descriptor received over SCM_RIGHTS.
    pub async fn recv(&mut self) -> RawFd {
        tokio::time::timeout(Duration::from_secs(2), &mut self.received)
            .await
            .expect("timed out waiting for SCM_RIGHTS descriptor")
            .expect("getfd server closed without sending a descriptor")
    }
}

impl Drop for GetFdServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve_getfd(stream: UnixStream) -> io::Result<RawFd> {
    write_all(&stream, GREETING).await?;
    write_all(&stream, b"\n").await?;

    let (line, _) = recv_line(&stream).await?;
    let capabilities: Value = serde_json::from_str(line.trim()).expect("invalid capabilities JSON");
    assert_eq!(capabilities["execute"], "qmp_capabilities");
    write_all(&stream, b"{\"return\":{}}\n").await?;

    let (line, mut fds) = recv_line(&stream).await?;
    let command: Value = serde_json::from_str(line.trim()).expect("invalid getfd JSON");
    assert_eq!(command["execute"], "getfd");
    assert_eq!(command["arguments"]["fdname"], "ozma-test");
    write_all(&stream, b"{\"return\":{}}\n").await?;

    if fds.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "getfd arrived without SCM_RIGHTS",
        ));
    }
    Ok(fds.remove(0))
}

async fn write_all(stream: &UnixStream, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        stream.writable().await?;
        match stream.try_write(data) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "write returned zero bytes",
                ))
            }
            Ok(written) => data = &data[written..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

async fn recv_line(stream: &UnixStream) -> io::Result<(String, Vec<RawFd>)> {
    let mut buffer = [0u8; 4096];
    let mut control = nix::cmsg_space!([RawFd; 4]);

    loop {
        stream.readable().await?;
        let mut iov = [io::IoSliceMut::new(&mut buffer)];
        match nix::sys::socket::recvmsg::<()>(
            stream.as_raw_fd(),
            &mut iov,
            Some(&mut control),
            nix::sys::socket::MsgFlags::empty(),
        ) {
            Ok(message) => {
                let bytes = message.bytes;
                let mut fds = Vec::new();
                for control in message
                    .cmsgs()
                    .map_err(|error| io::Error::from_raw_os_error(error as i32))?
                {
                    if let nix::sys::socket::ControlMessageOwned::ScmRights(mut received) = control
                    {
                        fds.append(&mut received);
                    }
                }
                let text = String::from_utf8_lossy(&buffer[..bytes]).into_owned();
                return Ok((text, fds));
            }
            Err(nix::errno::Errno::EAGAIN) => {
                // Tokio's readiness flag is not cleared by the raw recvmsg;
                // yield so the peer task can make progress before retrying.
                tokio::task::yield_now().await;
                continue;
            }
            Err(error) => return Err(io::Error::from_raw_os_error(error as i32)),
        }
    }
}
