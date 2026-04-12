// SPDX-License-Identifier: AGPL-3.0-only
//! Async QMP (QEMU Machine Protocol) clients — dual-socket design.
//!
//! Two independent clients for two independent QMP sockets:
//!
//! - [`QmpInputClient`] — dedicated to `input-send-event` (keyboard + mouse).
//!   Fire-and-forget writes. No response reading. No locks. No races.
//!   This is the high-frequency path (~100+ events/sec).
//!
//! - [`QmpControlClient`] — power, status, USB attach/detach, screendump.
//!   Request/response with proper serialisation. Low frequency (<1/sec).
//!
//! - [`QmpClient`] — unified wrapper; uses dual sockets when both paths are
//!   given, falls back to single-socket (legacy) mode otherwise.
//!
//! QEMU command line for dual sockets:
//! ```text
//! -qmp unix:/tmp/vm-ctrl.qmp,server,nowait
//! -qmp unix:/tmp/vm-input.qmp,server,nowait
//! ```

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::hid_to_qmp::QmpInputEvent;

const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
const BACKOFF_FACTOR: f64 = 2.0;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Shared connection state
// ---------------------------------------------------------------------------

struct InputConn {
    writer: tokio::net::unix::OwnedWriteHalf,
}

struct ControlConn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

// ---------------------------------------------------------------------------
// QmpInputClient
// ---------------------------------------------------------------------------

/// Dedicated QMP client for `input-send-event` only.
///
/// Writes keyboard/mouse events and never reads responses. This eliminates
/// the reader/writer race condition that plagued the single-socket design.
/// QEMU sends `{"return":{}}` for each event but we don't consume them —
/// they accumulate in the kernel socket buffer and get discarded on close.
#[derive(Clone)]
pub struct QmpInputClient {
    socket_path: String,
    conn: Arc<RwLock<Option<InputConn>>>,
}

impl QmpInputClient {
    pub fn new(socket_path: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
            conn: Arc::new(RwLock::new(None)),
        }
    }

    /// Spawn the background reconnect loop.
    pub async fn start(&self) {
        let client = self.clone();
        tokio::spawn(async move {
            client.connect_loop().await;
        });
    }

    /// Stop the client and close the connection.
    pub async fn stop(&self) {
        let mut guard = self.conn.write().await;
        *guard = None;
    }

    /// Returns `true` if currently connected.
    pub async fn connected(&self) -> bool {
        self.conn.read().await.is_some()
    }

    /// Send a list of QMP input events. Returns `false` if not connected or
    /// on write error (reconnect is triggered automatically).
    pub async fn send_input_events(&self, events: &[QmpInputEvent]) -> bool {
        if events.is_empty() {
            return false;
        }

        let events_json: Vec<Value> = events
            .iter()
            .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
            .collect();

        let cmd = json!({
            "execute": "input-send-event",
            "arguments": { "events": events_json }
        });
        let mut payload = serde_json::to_vec(&cmd).unwrap();
        payload.push(b'\n');

        let mut guard = self.conn.write().await;
        if let Some(conn) = guard.as_mut() {
            match conn.writer.write_all(&payload).await {
                Ok(_) => return true,
                Err(e) => {
                    debug!("QMP input write error: {}", e);
                    *guard = None;
                }
            }
        }

        // Trigger reconnect
        let client = self.clone();
        tokio::spawn(async move {
            client.connect_loop().await;
        });
        false
    }

    async fn connect_loop(&self) {
        let mut backoff = BACKOFF_INITIAL;
        loop {
            if self.try_connect().await {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = std::cmp::min(
                Duration::from_secs_f64(backoff.as_secs_f64() * BACKOFF_FACTOR),
                BACKOFF_MAX,
            );
        }
    }

    async fn try_connect(&self) -> bool {
        if !Path::new(&self.socket_path).exists() {
            return false;
        }
        match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.socket_path)).await {
            Ok(Ok(stream)) => {
                let (read_half, write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);

                // Read greeting
                let mut line = String::new();
                if tokio::time::timeout(CONNECT_TIMEOUT, reader.read_line(&mut line))
                    .await
                    .is_err()
                    || !line.contains("QMP")
                {
                    return false;
                }

                // Send capabilities negotiation
                let cap_cmd = json!({"execute": "qmp_capabilities"});
                let mut cap_bytes = serde_json::to_vec(&cap_cmd).unwrap();
                cap_bytes.push(b'\n');

                // We need a temporary mutable writer for the handshake
                // Re-connect the stream for the handshake then split again
                // Instead, use a single write before splitting permanently
                // We already split — use write_half directly
                let mut writer = write_half;
                if writer.write_all(&cap_bytes).await.is_err() {
                    return false;
                }

                // Read capabilities response
                line.clear();
                if tokio::time::timeout(CONNECT_TIMEOUT, reader.read_line(&mut line))
                    .await
                    .is_err()
                {
                    return false;
                }

                // Drain responses in background
                let conn_arc = self.conn.clone();
                let path = self.socket_path.clone();
                tokio::spawn(async move {
                    drain_reader(reader, conn_arc, path).await;
                });

                let mut guard = self.conn.write().await;
                *guard = Some(InputConn { writer });
                info!("QMP input connected: {}", self.socket_path);
                true
            }
            Ok(Err(e)) => {
                debug!("QMP input connect failed: {}", e);
                false
            }
            Err(_) => {
                debug!("QMP input connect timed out");
                false
            }
        }
    }
}

/// Silently drain all responses from the reader until EOF, then mark
/// the connection as disconnected.
async fn drain_reader(
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    conn: Arc<RwLock<Option<InputConn>>>,
    path: String,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    let mut guard = conn.write().await;
    if guard.is_some() {
        *guard = None;
        info!("QMP input disconnected ({}), will reconnect", path);
    }
}

// ---------------------------------------------------------------------------
// QmpControlClient
// ---------------------------------------------------------------------------

/// QMP client for control commands: power, status, USB, screendump.
///
/// Uses proper request/response serialisation: one command at a time,
/// wait for the response before sending the next. Low frequency path.
#[derive(Clone)]
pub struct QmpControlClient {
    socket_path: String,
    conn: Arc<Mutex<Option<ControlConn>>>,
}

impl QmpControlClient {
    pub fn new(socket_path: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
            conn: Arc::new(Mutex::new(None)),
        }
    }

    /// Spawn the background reconnect loop.
    pub async fn start(&self) {
        let client = self.clone();
        tokio::spawn(async move {
            client.connect_loop().await;
        });
    }

    /// Stop the client and close the connection.
    pub async fn stop(&self) {
        let mut guard = self.conn.lock().await;
        *guard = None;
    }

    /// Returns `true` if currently connected.
    pub async fn connected(&self) -> bool {
        self.conn.lock().await.is_some()
    }

    /// Send a raw QMP command and return the response, or `None` on failure.
    pub async fn send_command(&self, cmd: Value) -> Option<Value> {
        let mut payload = serde_json::to_vec(&cmd).ok()?;
        payload.push(b'\n');

        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut()?;

        // Write
        if let Err(e) = conn.writer.write_all(&payload).await {
            warn!("QMP control write error: {}", e);
            *guard = None;
            self.spawn_reconnect();
            return None;
        }

        // Read response — skip async events (they have an "event" key)
        let mut line = String::new();
        for _ in 0..10 {
            line.clear();
            match tokio::time::timeout(COMMAND_TIMEOUT, conn.reader.read_line(&mut line)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {
                    *guard = None;
                    self.spawn_reconnect();
                    return None;
                }
                Ok(Ok(_)) => {
                    if let Ok(resp) = serde_json::from_str::<Value>(&line) {
                        if resp.get("return").is_some() || resp.get("error").is_some() {
                            return Some(resp);
                        }
                        // Async event — skip
                    }
                }
            }
        }
        None
    }

    fn spawn_reconnect(&self) {
        let client = self.clone();
        tokio::spawn(async move {
            client.connect_loop().await;
        });
    }

    // ── Convenience methods ───────────────────────────────────────────────

    pub async fn system_powerdown(&self) -> bool {
        self.send_command(json!({"execute": "system_powerdown"}))
            .await
            .map(|r| r.get("return").is_some())
            .unwrap_or(false)
    }

    pub async fn system_reset(&self) -> bool {
        self.send_command(json!({"execute": "system_reset"}))
            .await
            .map(|r| r.get("return").is_some())
            .unwrap_or(false)
    }

    pub async fn pause(&self) -> bool {
        self.send_command(json!({"execute": "stop"}))
            .await
            .map(|r| r.get("return").is_some())
            .unwrap_or(false)
    }

    pub async fn cont(&self) -> bool {
        self.send_command(json!({"execute": "cont"}))
            .await
            .map(|r| r.get("return").is_some())
            .unwrap_or(false)
    }

    pub async fn query_status(&self) -> Option<Value> {
        let resp = self
            .send_command(json!({"execute": "query-status"}))
            .await?;
        resp.get("return").cloned()
    }

    pub async fn screendump(&self, output_path: &str) -> bool {
        self.send_command(json!({
            "execute": "screendump",
            "arguments": { "filename": output_path }
        }))
        .await
        .map(|r| r.get("return").is_some())
        .unwrap_or(false)
    }

    pub async fn attach_usb_storage(
        &self,
        image_path: &str,
        drive_id: &str,
        readonly: bool,
    ) -> bool {
        let resp = self
            .send_command(json!({
                "execute": "blockdev-add",
                "arguments": {
                    "driver": "file",
                    "node-name": format!("{drive_id}-file"),
                    "filename": image_path,
                    "read-only": readonly
                }
            }))
            .await;
        if resp.as_ref().and_then(|r| r.get("error")).is_some() {
            return false;
        }
        self.send_command(json!({
            "execute": "device_add",
            "arguments": {
                "driver": "usb-storage",
                "id": drive_id,
                "drive": format!("{drive_id}-file"),
                "removable": true
            }
        }))
        .await
        .map(|r| r.get("return").is_some())
        .unwrap_or(false)
    }

    pub async fn detach_usb_storage(&self, drive_id: &str) -> bool {
        self.send_command(json!({
            "execute": "device_del",
            "arguments": { "id": drive_id }
        }))
        .await;
        self.send_command(json!({
            "execute": "blockdev-del",
            "arguments": { "node-name": format!("{drive_id}-file") }
        }))
        .await;
        true
    }

    async fn connect_loop(&self) {
        let mut backoff = BACKOFF_INITIAL;
        loop {
            if self.try_connect().await {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = std::cmp::min(
                Duration::from_secs_f64(backoff.as_secs_f64() * BACKOFF_FACTOR),
                BACKOFF_MAX,
            );
        }
    }

    async fn try_connect(&self) -> bool {
        if !Path::new(&self.socket_path).exists() {
            return false;
        }
        match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.socket_path)).await {
            Ok(Ok(stream)) => {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);

                // Read greeting
                let mut line = String::new();
                if tokio::time::timeout(CONNECT_TIMEOUT, reader.read_line(&mut line))
                    .await
                    .is_err()
                    || !line.contains("QMP")
                {
                    return false;
                }

                // Capabilities negotiation
                let cap_cmd = json!({"execute": "qmp_capabilities"});
                let mut cap_bytes = serde_json::to_vec(&cap_cmd).unwrap();
                cap_bytes.push(b'\n');
                if write_half.write_all(&cap_bytes).await.is_err() {
                    return false;
                }

                line.clear();
                match tokio::time::timeout(CONNECT_TIMEOUT, reader.read_line(&mut line)).await {
                    Ok(Ok(_)) => {}
                    _ => return false,
                }
                if let Ok(resp) = serde_json::from_str::<Value>(&line) {
                    if resp.get("return").is_none() {
                        return false;
                    }
                } else {
                    return false;
                }

                let mut guard = self.conn.lock().await;
                *guard = Some(ControlConn {
                    reader,
                    writer: write_half,
                });
                info!("QMP control connected: {}", self.socket_path);
                true
            }
            Ok(Err(e)) => {
                debug!("QMP control connect failed: {}", e);
                false
            }
            Err(_) => {
                debug!("QMP control connect timed out");
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// QmpClient — unified wrapper
// ---------------------------------------------------------------------------

/// Unified QMP client — wraps input + control on separate sockets.
///
/// If two socket paths are given, uses dedicated sockets (recommended).
/// If only one path is given, falls back to single-socket mode (legacy).
pub struct QmpClient {
    ctrl: QmpControlClient,
    input: Option<QmpInputClient>,
    dual: bool,
}

impl QmpClient {
    /// Create a dual-socket client (recommended).
    pub fn new_dual(ctrl_path: impl Into<String>, input_path: impl Into<String>) -> Self {
        Self {
            ctrl: QmpControlClient::new(ctrl_path),
            input: Some(QmpInputClient::new(input_path)),
            dual: true,
        }
    }

    /// Create a single-socket client (legacy mode).
    pub fn new_single(socket_path: impl Into<String>) -> Self {
        Self {
            ctrl: QmpControlClient::new(socket_path),
            input: None,
            dual: false,
        }
    }

    pub async fn start(&self) {
        self.ctrl.start().await;
        if let Some(inp) = &self.input {
            inp.start().await;
        }
    }

    pub async fn stop(&self) {
        self.ctrl.stop().await;
        if let Some(inp) = &self.input {
            inp.stop().await;
        }
    }

    pub async fn connected(&self) -> bool {
        if self.dual {
            self.ctrl.connected().await
                && self.input.as_ref().map(|i| async { i.connected().await }).is_some()
        } else {
            self.ctrl.connected().await
        }
    }

    /// Send input events via the dedicated input socket (or control socket in
    /// legacy mode).
    pub async fn send_input_events(&self, events: &[QmpInputEvent]) -> bool {
        if let Some(inp) = &self.input {
            return inp.send_input_events(events).await;
        }
        // Legacy: route through control client
        let events_json: Vec<Value> = events
            .iter()
            .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
            .collect();
        self.ctrl
            .send_command(json!({
                "execute": "input-send-event",
                "arguments": { "events": events_json }
            }))
            .await
            .is_some()
    }

    // Delegate control methods
    pub async fn system_powerdown(&self) -> bool {
        self.ctrl.system_powerdown().await
    }
    pub async fn system_reset(&self) -> bool {
        self.ctrl.system_reset().await
    }
    pub async fn cont(&self) -> bool {
        self.ctrl.cont().await
    }
    pub async fn query_status(&self) -> Option<Value> {
        self.ctrl.query_status().await
    }
    pub async fn screendump(&self, output_path: &str) -> bool {
        self.ctrl.screendump(output_path).await
    }
    pub async fn attach_usb_storage(
        &self,
        image_path: &str,
        drive_id: &str,
        readonly: bool,
    ) -> bool {
        self.ctrl.attach_usb_storage(image_path, drive_id, readonly).await
    }
    pub async fn detach_usb_storage(&self, drive_id: &str) -> bool {
        self.ctrl.detach_usb_storage(drive_id).await
    }
}
