// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Portable UAC2 status types for platforms without the Linux ALSA bridge.

use std::sync::{Arc, Mutex};

use ozma_usb::{Uac2BridgeReport, UsbError};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2BridgeError {
    pub kind: String,
    pub message: String,
}

impl Uac2BridgeError {
    pub fn from_usb(error: &UsbError) -> Self {
        Self {
            kind: error.kind().to_owned(),
            message: error.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
struct SharedState {
    report: Uac2BridgeReport,
    last_error: Option<Uac2BridgeError>,
}

#[derive(Clone, Debug)]
pub struct Uac2BridgeShared(Arc<Mutex<SharedState>>);

impl Uac2BridgeShared {
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self::from_report(Uac2BridgeReport::unsupported(reason))
    }

    pub fn from_report(report: Uac2BridgeReport) -> Self {
        let report = normalize_report(report);
        Self(Arc::new(Mutex::new(SharedState {
            report,
            last_error: None,
        })))
    }

    pub fn from_report_with_error(
        mut report: Uac2BridgeReport,
        error: Uac2BridgeError,
    ) -> Self {
        report = normalize_report(report);
        if report.status == ozma_usb::Uac2BridgeStatus::Running {
            report.status = ozma_usb::Uac2BridgeStatus::Configured;
        }
        Self(Arc::new(Mutex::new(SharedState {
            report,
            last_error: Some(error),
        })))
    }

    pub fn report(&self) -> Uac2BridgeReport {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).report.clone()
    }

    pub fn last_error(&self) -> Option<Uac2BridgeError> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .last_error
            .clone()
    }
}

fn normalize_report(mut report: Uac2BridgeReport) -> Uac2BridgeReport {
    if report.status.is_live() && !report.counters.has_data() {
        report.status = ozma_usb::Uac2BridgeStatus::Configured;
    }
    report
}
