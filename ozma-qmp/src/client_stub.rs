// SPDX-License-Identifier: AGPL-3.0-only
//! Non-Unix stub for [`QmpClient`] and [`SharedQmpClient`].

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::QmpConfig;
use crate::error::QmpError;
use crate::input_event::QmpInputEvent;

/// VM run status returned by `query-status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QmpStatus {
    pub running: bool,
    pub status: String,
}

fn unsupported() -> QmpError {
    QmpError::Protocol("QMP Unix sockets are only supported on Unix".to_owned())
}

/// Stub QMP client on non-Unix platforms.
#[derive(Clone, Debug)]
pub struct QmpClient {
    config: QmpConfig,
}

impl QmpClient {
    pub fn new(config: QmpConfig) -> Self {
        Self { config }
    }

    pub async fn connect(config: QmpConfig) -> Result<Self, QmpError> {
        let _ = config;
        Err(unsupported())
    }

    pub fn config(&self) -> &QmpConfig {
        &self.config
    }

    pub fn is_dual(&self) -> bool {
        self.config.is_dual()
    }

    pub fn is_connected(&self) -> bool {
        false
    }

    pub fn control_connected(&self) -> bool {
        false
    }

    pub fn input_connected(&self) -> bool {
        false
    }

    pub async fn connect_control(&self) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn connect_input(&self) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn reconnect(&self, _max_attempts: u32) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn disconnect(&self) {}

    pub async fn send_input_events(&self, _events: &[QmpInputEvent]) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn query_status(&self) -> Result<QmpStatus, QmpError> {
        Err(unsupported())
    }

    pub async fn system_powerdown(&self) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn system_reset(&self) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn stop(&self) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn quit(&self) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn cont(&self) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn screendump(&self, _path: &Path) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn add_client(&self, _protocol: &str, _fdname: &str) -> Result<(), QmpError> {
        Err(unsupported())
    }
}

/// Stub shared QMP client on non-Unix platforms.
#[derive(Clone, Debug)]
pub struct SharedQmpClient {
    client: QmpClient,
}

impl SharedQmpClient {
    pub fn new(config: QmpConfig) -> Self {
        Self {
            client: QmpClient::new(config),
        }
    }

    pub async fn connect(config: QmpConfig) -> Result<Self, QmpError> {
        let _ = config;
        Err(unsupported())
    }

    pub fn from_client(client: QmpClient) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &QmpClient {
        &self.client
    }

    pub fn config(&self) -> &QmpConfig {
        self.client.config()
    }

    pub async fn is_connected(&self) -> bool {
        false
    }

    pub fn is_connected_now(&self) -> bool {
        false
    }

    pub async fn ensure_connected(&self) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn reconnect(&self, _max_attempts: u32) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn disconnect(&self) {}

    pub async fn query_status(&self) -> Result<QmpStatus, QmpError> {
        Err(unsupported())
    }

    pub async fn system_powerdown(&self) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn system_reset(&self) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn stop(&self) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn quit(&self) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn cont(&self) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn screendump(&self, _path: &Path) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn send_input_events(&self, _events: &[QmpInputEvent]) -> Result<(), QmpError> {
        Err(unsupported())
    }

    pub async fn add_client(&self, _protocol: &str, _fdname: &str) -> Result<(), QmpError> {
        Err(unsupported())
    }
}
