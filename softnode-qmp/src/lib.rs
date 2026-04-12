// SPDX-License-Identifier: AGPL-3.0-only
//! QMP client and HID→QMP translator for ozma softnode.
//!
//! # Modules
//! - [`hid_to_qmp`] — HID Usage ID → QMP qcode table, keyboard/mouse report state machines
//! - [`qmp_client`] — async QMP Unix-socket clients (input, control, unified)

pub mod hid_to_qmp;
pub mod qmp_client;

pub use hid_to_qmp::{KeyboardReportState, MouseReportState};
pub use qmp_client::{QmpClient, QmpControlClient, QmpInputClient};
