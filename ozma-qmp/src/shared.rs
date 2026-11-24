// SPDX-License-Identifier: AGPL-3.0-only
//! A cloneable QMP control client shared by several roles.
//!
//! QEMU accepts **one** client per `-qmp` chardev. A real node needs power,
//! capture, snapshots, and (in single-socket mode) input to use the same
//! control connection, so this wrapper serializes every command through one
//! [`QmpClient`] and one command mutex. Clones share both the connection and the
//! mutex; there is never a concurrent reader on the socket.
//!
//! The dedicated input-socket mode remains available through
//! [`crate::QmpClient`]: a shared control client never claims the input role
//! when `--qmp-input-socket` is configured.
//!
//! # Commands and reconnects
//!
//! A command that finds the control socket disconnected makes one reconnect
//! attempt while holding the command mutex, then runs the command. This keeps
//! lazy startup working (QEMU may come up after the node) without allowing two
//! callers to race two connections. If the reconnect still fails, the typed
//! [`QmpError`] is returned to the caller.

use std::os::fd::RawFd;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::{QmpClient, QmpConfig, QmpError, QmpInputEvent, QmpStatus};

/// A cloneable handle to one serialized QMP control connection.
///
/// All command methods take the same async mutex, so concurrent calls from
/// power, HLS, snapshots, and input are ordered. The underlying
/// [`QmpClient`] also serializes its control reads; this outer lock makes the
/// "one owner per socket" contract explicit and covers reconnect plus command
/// as a single critical section.
#[derive(Debug, Clone)]
pub struct SharedQmpClient {
    client: QmpClient,
    commands: Arc<Mutex<()>>,
}

impl SharedQmpClient {
    /// Create a disconnected shared client.
    ///
    /// Commands will make one reconnect attempt when they find the client
    /// disconnected, so this is safe even if QEMU is not listening yet.
    pub fn new(config: QmpConfig) -> Self {
        Self {
            client: QmpClient::new(config),
            commands: Arc::new(Mutex::new(())),
        }
    }

    /// Connect once and wrap the resulting client.
    pub async fn connect(config: QmpConfig) -> Result<Self, QmpError> {
        Ok(Self::from_client(QmpClient::connect(config).await?))
    }

    /// Share an already-created client.
    ///
    /// The client's existing control connection (if any) is reused. Use this
    /// when another crate already owns the connection lifecycle.
    pub fn from_client(client: QmpClient) -> Self {
        Self {
            client,
            commands: Arc::new(Mutex::new(())),
        }
    }

    /// The underlying client, for read-only state inspection.
    ///
    /// Command methods on this wrapper should still be used so the shared
    /// serialization lock is respected.
    pub fn client(&self) -> &QmpClient {
        &self.client
    }

    /// Configuration this shared client was built with.
    pub fn config(&self) -> &QmpConfig {
        self.client.config()
    }

    /// True when the client can serve its role.
    ///
    /// This is an async method to match the rest of the shared surface, but it
    /// only reads the underlying connection state and never blocks on I/O.
    pub async fn is_connected(&self) -> bool {
        self.client.is_connected()
    }

    /// Synchronous connection snapshot for display/status helpers.
    pub fn is_connected_now(&self) -> bool {
        self.client.is_connected()
    }

    /// Connect the control socket if needed, while holding the command mutex.
    ///
    /// Returns the reconnect error when no connection could be established.
    pub async fn ensure_connected(&self) -> Result<(), QmpError> {
        let _guard = self.commands.lock().await;
        self.ensure_connected_locked().await
    }

    /// Reconnect missing sockets with the client's backoff schedule.
    pub async fn reconnect(&self, max_attempts: u32) -> Result<(), QmpError> {
        let _guard = self.commands.lock().await;
        self.client.reconnect(max_attempts).await
    }

    /// Close the shared control connection.
    ///
    /// A later command makes one lazy reconnect attempt.
    pub async fn disconnect(&self) {
        let _guard = self.commands.lock().await;
        self.client.disconnect().await;
    }

    /// Run `query-status` and parse the result.
    pub async fn query_status(&self) -> Result<QmpStatus, QmpError> {
        let _guard = self.commands.lock().await;
        self.ensure_connected_locked().await?;
        self.client.query_status().await
    }

    /// Ask the guest to power down (`system_powerdown`).
    pub async fn system_powerdown(&self) -> Result<(), QmpError> {
        let _guard = self.commands.lock().await;
        self.ensure_connected_locked().await?;
        self.client.system_powerdown().await
    }

    /// Reset the VM (`system_reset`).
    pub async fn system_reset(&self) -> Result<(), QmpError> {
        let _guard = self.commands.lock().await;
        self.ensure_connected_locked().await?;
        self.client.system_reset().await
    }

    /// Pause the VM (`stop`).
    ///
    /// This only pauses emulation. Use [`SharedQmpClient::quit`] for a hard
    /// power-off or [`SharedQmpClient::system_powerdown`] for ACPI shutdown.
    pub async fn stop(&self) -> Result<(), QmpError> {
        let _guard = self.commands.lock().await;
        self.ensure_connected_locked().await?;
        self.client.stop().await
    }

    /// Terminate the QEMU process (`quit`).
    ///
    /// This is the hard power-off for the QMP backend: the QEMU process exits,
    /// so the shared control connection cannot serve further commands until a
    /// new QEMU is started and a command reconnects.
    pub async fn quit(&self) -> Result<(), QmpError> {
        let _guard = self.commands.lock().await;
        self.ensure_connected_locked().await?;
        self.client.quit().await
    }

    /// Resume the VM (`cont`).
    pub async fn cont(&self) -> Result<(), QmpError> {
        let _guard = self.commands.lock().await;
        self.ensure_connected_locked().await?;
        self.client.cont().await
    }

    /// Write a PPM screendump to `path` (`screendump`).
    pub async fn screendump(&self, path: &Path) -> Result<(), QmpError> {
        let _guard = self.commands.lock().await;
        self.ensure_connected_locked().await?;
        self.client.screendump(path).await
    }

    /// Send `input-send-event` through the shared control connection.
    ///
    /// An empty event list is a successful no-op, matching [`QmpClient`].
    pub async fn send_input_events(&self, events: &[QmpInputEvent]) -> Result<(), QmpError> {
        if events.is_empty() {
            return Ok(());
        }
        let _guard = self.commands.lock().await;
        self.ensure_connected_locked().await?;
        self.client.send_input_events(events).await
    }

    /// Add a QMP client on a file descriptor (`add_client`).
    pub async fn add_client(&self, protocol: &str, fdname: &str) -> Result<(), QmpError> {
        let _guard = self.commands.lock().await;
        self.ensure_connected_locked().await?;
        self.client.add_client(protocol, fdname).await
    }

    /// Pass `fd` to QEMU under `fdname` using SCM_RIGHTS (`getfd`).
    pub async fn getfd(&self, fdname: &str, fd: RawFd) -> Result<(), QmpError> {
        let _guard = self.commands.lock().await;
        self.ensure_connected_locked().await?;
        self.client.getfd(fdname, fd).await
    }

    /// Pass `fd` and immediately attach a client to it (`getfd` + `add_client`).
    ///
    /// Both commands run while holding the shared command lock: `getfd` names
    /// a descriptor in the QMP monitor and `add_client` consumes that name, so
    /// an interleaved command or lazy reconnect must not land between them.
    pub async fn getfd_and_add_client(
        &self,
        fdname: &str,
        fd: RawFd,
        protocol: &str,
    ) -> Result<(), QmpError> {
        let _guard = self.commands.lock().await;
        self.ensure_connected_locked().await?;
        self.client.getfd(fdname, fd).await?;
        self.client.add_client(protocol, fdname).await
    }

    async fn ensure_connected_locked(&self) -> Result<(), QmpError> {
        if self.client.is_connected() {
            return Ok(());
        }
        self.client.reconnect(1).await
    }
}
