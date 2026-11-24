// SPDX-License-Identifier: AGPL-3.0-only
//! QMP power backend.
//!
//! This is a thin power-specific facade over [`ozma_qmp::QmpClient`]; the
//! client owns the socket, handshake, timeouts, and reconnect/backoff policy.
//! The node normally supplies a [`ozma_qmp::SharedQmpClient`] so power,
//! capture, snapshots, and input serialize on the one control connection.

use async_trait::async_trait;
use ozma_qmp::{QmpClient, QmpConfig, QmpStatus, SharedQmpClient};
use tracing::info;

use crate::error::PowerError;
use crate::state::{PowerState, PowerStatus};
use crate::PowerBackend;

#[derive(Debug, Clone)]
enum QmpHandle {
    Owned(QmpClient),
    Shared(SharedQmpClient),
}

/// Power control for one QEMU VM through its QMP control socket.
#[derive(Debug, Clone)]
pub struct QmpPower {
    client: QmpHandle,
}

impl QmpPower {
    /// Connect to a QMP socket and complete the greeting/capabilities
    /// handshake.
    pub async fn connect(config: QmpConfig) -> Result<Self, PowerError> {
        let client = QmpClient::connect(config).await?;
        info!("QMP power: connected");
        Ok(Self {
            client: QmpHandle::Owned(client),
        })
    }

    /// Wrap an existing owned client, connected or not.
    ///
    /// This remains the dedicated/single-role path. For a node that also
    /// captures or injects input over the same control socket, prefer
    /// [`QmpPower::with_shared_client`].
    pub fn with_client(client: QmpClient) -> Self {
        Self {
            client: QmpHandle::Owned(client),
        }
    }

    /// Use the node's single shared control client.
    ///
    /// Power commands lock the shared command mutex, so they cannot race a
    /// screendump or input write on the same QMP chardev. A command makes at
    /// most one lazy reconnect of that same connection if it was closed.
    pub fn with_shared_client(client: SharedQmpClient) -> Self {
        Self {
            client: QmpHandle::Shared(client),
        }
    }

    /// Underlying QMP client, for read-only state inspection.
    pub fn client(&self) -> &QmpClient {
        match &self.client {
            QmpHandle::Owned(client) => client,
            QmpHandle::Shared(shared) => shared.client(),
        }
    }

    /// The shared client this backend uses, if any.
    pub fn shared_client(&self) -> Option<&SharedQmpClient> {
        match &self.client {
            QmpHandle::Owned(_) => None,
            QmpHandle::Shared(shared) => Some(shared),
        }
    }

    /// True when the underlying control socket is connected.
    pub fn is_connected(&self) -> bool {
        match &self.client {
            QmpHandle::Owned(client) => client.is_connected(),
            QmpHandle::Shared(shared) => shared.is_connected_now(),
        }
    }

    async fn status(&self) -> Result<QmpStatus, PowerError> {
        match &self.client {
            QmpHandle::Owned(client) => Ok(client.query_status().await?),
            QmpHandle::Shared(shared) => Ok(shared.query_status().await?),
        }
    }

    async fn cont_command(&self) -> Result<(), PowerError> {
        match &self.client {
            QmpHandle::Owned(client) => Ok(client.cont().await?),
            QmpHandle::Shared(shared) => Ok(shared.cont().await?),
        }
    }

    async fn pause_command(&self) -> Result<(), PowerError> {
        match &self.client {
            QmpHandle::Owned(client) => Ok(client.stop().await?),
            QmpHandle::Shared(shared) => Ok(shared.stop().await?),
        }
    }

    async fn powerdown_command(&self) -> Result<(), PowerError> {
        match &self.client {
            QmpHandle::Owned(client) => Ok(client.system_powerdown().await?),
            QmpHandle::Shared(shared) => Ok(shared.system_powerdown().await?),
        }
    }

    async fn reset_command(&self) -> Result<(), PowerError> {
        match &self.client {
            QmpHandle::Owned(client) => Ok(client.system_reset().await?),
            QmpHandle::Shared(shared) => Ok(shared.system_reset().await?),
        }
    }

    async fn stop_command(&self) -> Result<(), PowerError> {
        match &self.client {
            QmpHandle::Owned(client) => Ok(client.quit().await?),
            QmpHandle::Shared(shared) => Ok(shared.quit().await?),
        }
    }
}

fn state_from_qmp(status: QmpStatus) -> PowerState {
    let state = PowerState::from_qmp(&status.status);
    // QEMU has occasionally reported a running guest with an empty/unknown
    // status string; trust the explicit `running` flag in that case.
    if status.running && state.kind() == PowerStatus::Unknown {
        PowerState::running()
    } else {
        state
    }
}

#[async_trait]
impl PowerBackend for QmpPower {
    async fn query_status(&mut self) -> Result<PowerState, PowerError> {
        let status = self.status().await?;
        Ok(state_from_qmp(status))
    }

    async fn cont(&mut self) -> Result<(), PowerError> {
        self.cont_command().await
    }

    /// Pause the VM with QMP `stop`.
    ///
    /// QEMU's `stop` is a monitor pause, not a power-off. The control socket
    /// stays connected and [`PowerBackend::query_status`] reports `paused`.
    async fn pause(&mut self) -> Result<(), PowerError> {
        self.pause_command().await
    }

    async fn resume(&mut self) -> Result<(), PowerError> {
        self.cont_command().await
    }

    async fn system_powerdown(&mut self) -> Result<(), PowerError> {
        self.powerdown_command().await
    }

    async fn system_reset(&mut self) -> Result<(), PowerError> {
        self.reset_command().await
    }

    /// Force the VM off by terminating QEMU (`quit`).
    ///
    /// QEMU's `quit` command exits the emulator process, matching the trait's
    /// destroy/force-off contract. It is not an ACPI request: the guest gets no
    /// chance to flush or shut down. Use [`QmpPower::system_powerdown`] for an
    /// orderly guest-requested shutdown.
    ///
    /// After a successful call the control connection is gone; the node must
    /// start a new QEMU (or reconnect to the restarted server) before querying
    /// or sending another power command.
    async fn stop(&mut self) -> Result<(), PowerError> {
        self.stop_command().await
    }
}
