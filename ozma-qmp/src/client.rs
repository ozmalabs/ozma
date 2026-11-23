// SPDX-License-Identifier: AGPL-3.0-only
//! Dual-socket QMP client.
//!
//! Control commands are serialized and wait for their response (skipping
//! asynchronous `{"event": ...}` messages). The optional dedicated input
//! socket is fire-and-forget: writes never wait for QEMU's `{"return":{}}`, and
//! a background task drains those responses so the socket buffer cannot grow
//! without bound.

use std::io::IoSlice;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, info};

use crate::backoff::Backoff;
use crate::config::QmpConfig;
use crate::error::QmpError;
use crate::input_event::QmpInputEvent;

/// Maximum number of unsolicited events skipped before a command response.
const MAX_ASYNC_EVENTS: usize = 64;

/// Parsed `query-status` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QmpStatus {
    /// Whether the VM is currently running.
    pub running: bool,
    /// QEMU's status string (`running`, `paused`, `shutdown`, ...).
    pub status: String,
}

struct ControlConnection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl std::fmt::Debug for ControlConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlConnection").finish_non_exhaustive()
    }
}

struct InputConnection {
    writer: OwnedWriteHalf,
    drain: JoinHandle<()>,
}

impl std::fmt::Debug for InputConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputConnection").finish_non_exhaustive()
    }
}

impl Drop for InputConnection {
    fn drop(&mut self) {
        self.drain.abort();
    }
}

/// A QMP client with a required control socket and an optional input socket.
///
/// Cloning the client shares the underlying connections.
#[derive(Debug, Clone)]
pub struct QmpClient {
    config: QmpConfig,
    control: Arc<Mutex<Option<ControlConnection>>>,
    input: Arc<Mutex<Option<InputConnection>>>,
    control_connected: Arc<AtomicBool>,
    input_connected: Arc<AtomicBool>,
    input_generation: Arc<AtomicU64>,
}

impl QmpClient {
    /// Create a disconnected client.
    ///
    /// Use [`QmpClient::connect`] or [`QmpClient::connect_control`] /
    /// [`QmpClient::connect_input`] to open the sockets.
    pub fn new(config: QmpConfig) -> Self {
        Self {
            config,
            control: Arc::new(Mutex::new(None)),
            input: Arc::new(Mutex::new(None)),
            control_connected: Arc::new(AtomicBool::new(false)),
            input_connected: Arc::new(AtomicBool::new(false)),
            input_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Connect the required control socket and, if configured, the input
    /// socket. Both handshakes must succeed.
    pub async fn connect(config: QmpConfig) -> Result<Self, QmpError> {
        config.validate()?;
        let client = Self::new(config);
        client.ensure_connected_once().await?;
        Ok(client)
    }

    /// Configuration this client was built with.
    pub fn config(&self) -> &QmpConfig {
        &self.config
    }

    /// True when a dedicated input socket is configured.
    pub fn is_dual(&self) -> bool {
        self.config.is_dual()
    }

    /// True when the client can serve its role: the control socket in single
    /// mode, both sockets in dual mode. This mirrors Python's
    /// `QMPClient.connected`.
    pub fn is_connected(&self) -> bool {
        if self.config.is_dual() {
            self.control_connected() && self.input_connected()
        } else {
            self.control_connected()
        }
    }

    /// True when the control socket is connected.
    pub fn control_connected(&self) -> bool {
        self.control_connected.load(Ordering::SeqCst)
    }

    /// True when the dedicated input socket is connected. Always false in
    /// single-socket mode.
    pub fn input_connected(&self) -> bool {
        self.input_connected.load(Ordering::SeqCst)
    }

    /// Open (or replace) the control socket and complete the QMP handshake.
    pub async fn connect_control(&self) -> Result<(), QmpError> {
        self.config.validate()?;
        let (reader, writer) = open_socket(
            &self.config.control_path,
            self.config.connect_timeout,
            "control",
        )
        .await?;

        let mut guard = self.control.lock().await;
        *guard = Some(ControlConnection { reader, writer });
        self.control_connected.store(true, Ordering::SeqCst);
        info!(path = %self.config.control_path.display(), "QMP control connected");
        Ok(())
    }

    /// Open (or replace) the dedicated input socket and start its background
    /// response drain. Returns [`QmpError::NoInputSocket`] in single mode.
    pub async fn connect_input(&self) -> Result<(), QmpError> {
        self.config.validate()?;
        let path = self
            .config
            .input_path
            .as_ref()
            .ok_or(QmpError::NoInputSocket)?;

        let (reader, writer) = open_socket(path, self.config.connect_timeout, "input").await?;

        let generation = self
            .input_generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        let connected = self.input_connected.clone();
        let generations = self.input_generation.clone();
        let drain = tokio::spawn(async move {
            let mut reader = reader;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            if generations.load(Ordering::SeqCst) == generation {
                connected.store(false, Ordering::SeqCst);
            }
            debug!("QMP input drain finished");
        });

        let mut guard = self.input.lock().await;
        *guard = Some(InputConnection { writer, drain });
        self.input_connected.store(true, Ordering::SeqCst);
        info!(path = %path.display(), "QMP input connected");
        Ok(())
    }

    /// Reconnect missing sockets with exponential backoff.
    ///
    /// `max_attempts` is the number of connection rounds; use `u32::MAX` for
    /// an effectively unbounded retry loop in a supervisor task. Returns the
    /// last error if every round fails.
    pub async fn reconnect(&self, max_attempts: u32) -> Result<(), QmpError> {
        if self.is_connected() {
            return Ok(());
        }

        let rounds = max_attempts.max(1);
        let mut backoff = Backoff::from_config(&self.config);
        let mut last = QmpError::NotConnected;

        for attempt in 0..rounds {
            match self.ensure_connected_once().await {
                Ok(()) => return Ok(()),
                Err(error) => last = error,
            }
            if attempt + 1 < rounds {
                let delay = backoff.next_delay();
                debug!(?delay, attempt, "QMP reconnect backoff");
                tokio::time::sleep(delay).await;
            }
        }

        Err(last)
    }

    /// Close both sockets and stop the input drain.
    pub async fn disconnect(&self) {
        self.input_generation.fetch_add(1, Ordering::SeqCst);
        self.control_connected.store(false, Ordering::SeqCst);
        self.input_connected.store(false, Ordering::SeqCst);

        let mut control = self.control.lock().await;
        *control = None;
        drop(control);

        let mut input = self.input.lock().await;
        *input = None;
    }

    async fn ensure_connected_once(&self) -> Result<(), QmpError> {
        if !self.control_connected() {
            self.connect_control().await?;
        }
        if self.config.is_dual() && !self.input_connected() {
            self.connect_input().await?;
        }
        Ok(())
    }

    /// Send `input-send-event`.
    ///
    /// With a dedicated input socket the write is fire-and-forget and only
    /// reports local socket errors. In single-socket mode the command travels
    /// over the control connection and waits for QEMU's response. Empty event
    /// lists are a successful no-op.
    pub async fn send_input_events(&self, events: &[QmpInputEvent]) -> Result<(), QmpError> {
        if events.is_empty() {
            return Ok(());
        }

        let events: Value = serde_json::to_value(events)
            .map_err(|error| QmpError::Protocol(format!("cannot encode input events: {error}")))?;
        let arguments = Some(json!({ "events": events }));

        if self.config.is_dual() {
            let payload = encode_command("input-send-event", arguments)?;
            let mut guard = self.input.lock().await;
            let write = {
                let Some(connection) = guard.as_mut() else {
                    self.input_connected.store(false, Ordering::SeqCst);
                    return Err(QmpError::NotConnected);
                };
                timeout(
                    self.config.command_timeout,
                    connection.writer.write_all(&payload),
                )
                .await
            };

            match write {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => {
                    *guard = None;
                    self.input_connected.store(false, Ordering::SeqCst);
                    Err(QmpError::Io(error))
                }
                Err(_) => {
                    *guard = None;
                    self.input_connected.store(false, Ordering::SeqCst);
                    Err(QmpError::Timeout {
                        command: "input-send-event".to_string(),
                        timeout: self.config.command_timeout,
                    })
                }
            }
        } else {
            self.control_command("input-send-event", arguments)
                .await
                .map(|_| ())
        }
    }

    /// Run `query-status` and parse the result.
    pub async fn query_status(&self) -> Result<QmpStatus, QmpError> {
        let value = self.control_command("query-status", None).await?;
        serde_json::from_value(value)
            .map_err(|error| QmpError::Protocol(format!("invalid query-status response: {error}")))
    }

    /// Ask the guest to power down (`system_powerdown`).
    pub async fn system_powerdown(&self) -> Result<(), QmpError> {
        self.control_command("system_powerdown", None)
            .await
            .map(|_| ())
    }

    /// Reset the VM (`system_reset`).
    pub async fn system_reset(&self) -> Result<(), QmpError> {
        self.control_command("system_reset", None).await.map(|_| ())
    }

    /// Pause the VM (`stop`).
    pub async fn stop(&self) -> Result<(), QmpError> {
        self.control_command("stop", None).await.map(|_| ())
    }

    /// Resume the VM (`cont`).
    pub async fn cont(&self) -> Result<(), QmpError> {
        self.control_command("cont", None).await.map(|_| ())
    }

    /// Write a PPM screendump to `path` (`screendump`).
    pub async fn screendump(&self, path: &Path) -> Result<(), QmpError> {
        let filename = path
            .to_str()
            .ok_or_else(|| QmpError::InvalidPath(path.to_path_buf()))?;
        self.control_command("screendump", Some(json!({ "filename": filename })))
            .await
            .map(|_| ())
    }

    /// Add a QMP client on a file descriptor (`add_client`), used to hand a
    /// D-Bus or other socket to QEMU.
    pub async fn add_client(&self, protocol: &str, fdname: &str) -> Result<(), QmpError> {
        self.control_command(
            "add_client",
            Some(json!({ "protocol": protocol, "fdname": fdname })),
        )
        .await
        .map(|_| ())
    }

    /// Pass `fd` to QEMU under `fdname` using SCM_RIGHTS (`getfd`).
    ///
    /// The descriptor must remain open until QEMU has received it; the caller
    /// keeps ownership and may close it after this call returns.
    pub async fn getfd(&self, fdname: &str, fd: RawFd) -> Result<(), QmpError> {
        if fd < 0 {
            return Err(QmpError::Fd(format!("invalid file descriptor {fd}")));
        }
        let payload = encode_command("getfd", Some(json!({ "fdname": fdname })))?;

        let mut guard = self.control.lock().await;
        let Some(mut connection) = guard.take() else {
            self.control_connected.store(false, Ordering::SeqCst);
            return Err(QmpError::NotConnected);
        };

        let result = async {
            let iov = [IoSlice::new(&payload)];
            let fds = [fd];
            let cmsgs = [ControlMessage::ScmRights(&fds)];
            let sent = sendmsg::<()>(
                connection.writer.as_ref().as_raw_fd(),
                &iov,
                &cmsgs,
                MsgFlags::empty(),
                None,
            )
            .map_err(|error| QmpError::Fd(format!("sendmsg(SCM_RIGHTS) failed: {error}")))?;

            if sent != payload.len() {
                return Err(QmpError::Fd(format!(
                    "short sendmsg: wrote {sent} of {} bytes",
                    payload.len()
                )));
            }

            read_response(&mut connection.reader, "getfd", self.config.command_timeout).await
        }
        .await;

        match result {
            Ok(_) => {
                *guard = Some(connection);
                self.control_connected.store(true, Ordering::SeqCst);
                Ok(())
            }
            Err(error) => {
                if matches!(error, QmpError::Command { .. }) {
                    *guard = Some(connection);
                    self.control_connected.store(true, Ordering::SeqCst);
                } else {
                    self.control_connected.store(false, Ordering::SeqCst);
                }
                Err(error)
            }
        }
    }

    async fn control_command(
        &self,
        command: &str,
        arguments: Option<Value>,
    ) -> Result<Value, QmpError> {
        let payload = encode_command(command, arguments)?;

        let mut guard = self.control.lock().await;
        let Some(mut connection) = guard.take() else {
            self.control_connected.store(false, Ordering::SeqCst);
            return Err(QmpError::NotConnected);
        };

        let result = execute_control(
            &mut connection,
            &payload,
            command,
            self.config.command_timeout,
        )
        .await;

        match result {
            Ok(value) => {
                *guard = Some(connection);
                self.control_connected.store(true, Ordering::SeqCst);
                Ok(value)
            }
            Err(error) => {
                if matches!(error, QmpError::Command { .. }) {
                    *guard = Some(connection);
                    self.control_connected.store(true, Ordering::SeqCst);
                } else {
                    self.control_connected.store(false, Ordering::SeqCst);
                }
                Err(error)
            }
        }
    }
}

fn encode_command(command: &str, arguments: Option<Value>) -> Result<Vec<u8>, QmpError> {
    let mut value = json!({ "execute": command });
    if let Some(arguments) = arguments {
        value["arguments"] = arguments;
    }
    let mut payload = serde_json::to_vec(&value)
        .map_err(|error| QmpError::Protocol(format!("cannot encode QMP command: {error}")))?;
    payload.push(b'\n');
    Ok(payload)
}

async fn execute_control(
    connection: &mut ControlConnection,
    payload: &[u8],
    command: &str,
    command_timeout: Duration,
) -> Result<Value, QmpError> {
    timeout(command_timeout, connection.writer.write_all(payload))
        .await
        .map_err(|_| QmpError::Timeout {
            command: command.to_string(),
            timeout: command_timeout,
        })?
        .map_err(QmpError::Io)?;

    read_response(&mut connection.reader, command, command_timeout).await
}

async fn read_response(
    reader: &mut BufReader<OwnedReadHalf>,
    command: &str,
    read_timeout: Duration,
) -> Result<Value, QmpError> {
    let mut line = String::new();
    let mut skipped_events = 0usize;

    loop {
        line.clear();
        let read = timeout(read_timeout, reader.read_line(&mut line))
            .await
            .map_err(|_| QmpError::Timeout {
                command: command.to_string(),
                timeout: read_timeout,
            })?;
        let bytes = read.map_err(QmpError::Io)?;
        if bytes == 0 {
            return Err(QmpError::ConnectionClosed);
        }

        let text = line.trim();
        if text.is_empty() {
            continue;
        }

        let value: Value = serde_json::from_str(text).map_err(|error| {
            QmpError::Protocol(format!("invalid JSON from QMP during {command}: {error}"))
        })?;

        if let Some(error) = value.get("error") {
            return Err(QmpError::Command {
                command: command.to_string(),
                message: error.to_string(),
            });
        }
        if let Some(returned) = value.get("return") {
            return Ok(returned.clone());
        }
        if value.get("event").is_some() {
            skipped_events += 1;
            if skipped_events > MAX_ASYNC_EVENTS {
                return Err(QmpError::Protocol(format!(
                    "more than {MAX_ASYNC_EVENTS} async events before the {command} response"
                )));
            }
            continue;
        }

        return Err(QmpError::Protocol(format!(
            "unexpected QMP message during {command}: {text}"
        )));
    }
}

async fn open_socket(
    path: &Path,
    handshake_timeout: Duration,
    role: &str,
) -> Result<(BufReader<OwnedReadHalf>, OwnedWriteHalf), QmpError> {
    let stream = timeout(handshake_timeout, UnixStream::connect(path))
        .await
        .map_err(|_| QmpError::Timeout {
            command: format!("connect {role} socket"),
            timeout: handshake_timeout,
        })?
        .map_err(|source| QmpError::Connect {
            path: path.to_path_buf(),
            source,
        })?;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut greeting = String::new();
    let bytes = timeout(handshake_timeout, reader.read_line(&mut greeting))
        .await
        .map_err(|_| QmpError::Timeout {
            command: format!("{role} greeting"),
            timeout: handshake_timeout,
        })?
        .map_err(QmpError::Io)?;
    if bytes == 0 {
        return Err(QmpError::Handshake(
            "connection closed before the QMP greeting".to_string(),
        ));
    }

    let greeting_text = greeting.trim().to_string();
    let greeting: Value = serde_json::from_str(&greeting_text).map_err(|error| {
        QmpError::Handshake(format!("invalid QMP greeting on {role} socket: {error}"))
    })?;
    if greeting.get("QMP").is_none() {
        return Err(QmpError::Handshake(format!(
            "unexpected QMP greeting on {role} socket: {greeting_text}"
        )));
    }

    let capabilities = encode_command("qmp_capabilities", None)?;
    timeout(handshake_timeout, write_half.write_all(&capabilities))
        .await
        .map_err(|_| QmpError::Timeout {
            command: "qmp_capabilities".to_string(),
            timeout: handshake_timeout,
        })?
        .map_err(QmpError::Io)?;

    read_response(&mut reader, "qmp_capabilities", handshake_timeout).await?;
    Ok((reader, write_half))
}
