// SPDX-License-Identifier: AGPL-3.0-only
//! A retained failure for a backend that could not be connected at startup.
//!
//! The node keeps running when a power backend cannot connect (so capture,
//! input, and the API stay available), but it must not turn that failure into
//! `not-configured`. [`UnavailablePower`] is attached in place of the real
//! backend and replays the original typed reason from every operation.

use async_trait::async_trait;

use crate::error::PowerError;
use crate::state::PowerOutcome;
use crate::{PowerBackend, PowerState};

/// A power backend that failed to connect, retaining the real reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailablePower {
    backend: &'static str,
    kind: &'static str,
    reason: String,
}

impl UnavailablePower {
    /// Retain `error` from `backend` for later `/power/*` calls.
    pub fn new(backend: &'static str, error: &PowerError) -> Self {
        Self {
            backend,
            kind: error.kind(),
            reason: error.to_string(),
        }
    }

    /// Backend name (`libvirt`, `qmp`).
    pub fn backend(&self) -> &'static str {
        self.backend
    }

    /// Original machine-readable error kind (`domain-not-found`, ...).
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// Original human-readable failure.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// The typed error replayed by every backend operation.
    pub fn error(&self) -> PowerError {
        PowerError::Unavailable {
            backend: self.backend,
            kind: self.kind,
            reason: self.reason.clone(),
        }
    }
}

#[async_trait]
impl PowerBackend for UnavailablePower {
    async fn query_status(&mut self) -> Result<PowerState, PowerError> {
        Err(self.error())
    }

    async fn cont(&mut self) -> Result<(), PowerError> {
        Err(self.error())
    }

    async fn pause(&mut self) -> Result<(), PowerError> {
        Err(self.error())
    }

    async fn resume(&mut self) -> Result<(), PowerError> {
        Err(self.error())
    }

    async fn system_powerdown(&mut self) -> Result<(), PowerError> {
        Err(self.error())
    }

    async fn system_powerdown_outcome(&mut self) -> Result<PowerOutcome, PowerError> {
        Err(self.error())
    }

    async fn system_reset(&mut self) -> Result<(), PowerError> {
        Err(self.error())
    }

    async fn stop(&mut self) -> Result<(), PowerError> {
        Err(self.error())
    }
}
