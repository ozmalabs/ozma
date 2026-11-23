// SPDX-License-Identifier: AGPL-3.0-only
//! QMP power backend.
//!
//! This is a thin power-specific facade over [`ozma_qmp::QmpClient`]; the
//! client owns the socket, handshake, timeouts, and reconnect/backoff policy.

use async_trait::async_trait;
use ozma_qmp::{QmpClient, QmpConfig, QmpStatus};
use tracing::info;

use crate::error::PowerError;
use crate::state::{PowerState, PowerStatus};
use crate::PowerBackend;

/// Power control for one QEMU VM through its QMP control socket.
#[derive(Debug, Clone)]
pub struct QmpPower {
    client: QmpClient,
}

impl QmpPower {
    /// Connect to a QMP socket and complete the greeting/capabilities
    /// handshake.
    pub async fn connect(config: QmpConfig) -> Result<Self, PowerError> {
        let client = QmpClient::connect(config).await?;
        info!("QMP power: connected");
        Ok(Self { client })
    }

    /// Wrap an existing client, connected or not.
    ///
    /// This is useful when the node already owns a QMP client for input or
    /// capture and wants to share the connection for power.
    pub fn with_client(client: QmpClient) -> Self {
        Self { client }
    }

    /// Underlying QMP client.
    pub fn client(&self) -> &QmpClient {
        &self.client
    }

    /// True when the underlying control socket is connected.
    pub fn is_connected(&self) -> bool {
        self.client.is_connected()
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
        let status = self.client.query_status().await?;
        Ok(state_from_qmp(status))
    }

    async fn cont(&mut self) -> Result<(), PowerError> {
        self.client.cont().await?;
        Ok(())
    }

    async fn system_powerdown(&mut self) -> Result<(), PowerError> {
        self.client.system_powerdown().await?;
        Ok(())
    }

    async fn system_reset(&mut self) -> Result<(), PowerError> {
        self.client.system_reset().await?;
        Ok(())
    }

    /// Delegate to QEMU's `stop` command.
    ///
    /// QEMU's `stop` **pauses** the guest; it does not force a power cut. The
    /// Python QMP power backend has the same limitation (it exposes no `stop`
    /// method at all, so `force-off` falls back to ACPI powerdown). Use
    /// [`crate::LibvirtPower`] for a real `destroy`, or extend `ozma-qmp` with
    /// QEMU's `quit` command before treating this as a hard power-off.
    async fn stop(&mut self) -> Result<(), PowerError> {
        self.client.stop().await?;
        Ok(())
    }
}
