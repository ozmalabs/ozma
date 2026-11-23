// SPDX-License-Identifier: AGPL-3.0-only
//! Diagnostics and runtime metrics shared by the API, UI, and reports.

use serde::{Deserialize, Serialize};

/// Result of one readiness or diagnostic check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Skip,
}

/// A single check with an actionable explanation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticCheck {
    pub id: String,
    pub status: CheckStatus,
    pub message: String,
    #[serde(default)]
    pub remediation: Option<String>,
}

impl DiagnosticCheck {
    pub fn ok(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: CheckStatus::Ok,
            message: message.into(),
            remediation: None,
        }
    }

    pub fn fail(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: CheckStatus::Fail,
            message: message.into(),
            remediation: None,
        }
    }

    pub fn warn(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: CheckStatus::Warn,
            message: message.into(),
            remediation: None,
        }
    }

    pub fn skip(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: CheckStatus::Skip,
            message: message.into(),
            remediation: None,
        }
    }

    pub fn with_remediation(mut self, remediation: impl Into<String>) -> Self {
        self.remediation = Some(remediation.into());
        self
    }
}

/// A complete readiness/diagnostic report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticReport {
    pub ok: bool,
    pub generated_at: u64,
    #[serde(default)]
    pub checks: Vec<DiagnosticCheck>,
}

impl DiagnosticReport {
    pub fn new(checks: Vec<DiagnosticCheck>, generated_at: u64) -> Self {
        let ok = !checks.iter().any(|check| check.status == CheckStatus::Fail);
        Self {
            ok,
            generated_at,
            checks,
        }
    }
}

/// Live counters exposed by `/api/v1/metrics` and the client quality panel.
/// Values are intentionally simple and cumulative where appropriate.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeMetrics {
    pub sessions: u32,
    pub active_streams: u32,
    pub fps: f32,
    pub width: u32,
    pub height: u32,
    pub bitrate_kbps: u32,
    pub frames_dropped: u64,
    pub capture_to_encode_us: u64,
    pub encode_us: u64,
    pub rtt_ms: f32,
    pub jitter_ms: f32,
    pub loss_percent: f32,
}

impl RuntimeMetrics {
    pub fn has_video(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_check_makes_the_report_not_ok() {
        let report = DiagnosticReport::new(
            vec![
                DiagnosticCheck::ok("capture", "frame seen"),
                DiagnosticCheck::fail("encoder", "probe failed"),
            ],
            123,
        );
        assert!(!report.ok);
    }
}
