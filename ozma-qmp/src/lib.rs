// SPDX-License-Identifier: AGPL-3.0-only
//! QMP (QEMU Machine Protocol) client and HID translation for Ozma nodes.
//!
//! This crate replaces the Python `softnode/qmp_client.py` and
//! `softnode/hid_to_qmp.py` modules. It has no platform bindings beyond Unix
//! sockets and is intended to be consumed by:
//!
//! * `ozma-input` — [`QmpInputEvent`] plus [`KeyboardReportState`] /
//!   [`MouseReportState`] / [`RelativeMouseReportState`] for a QMP input sink.
//! * `ozma-power` — [`QmpClient::system_powerdown`],
//!   [`QmpClient::system_reset`], [`QmpClient::stop`], [`QmpClient::cont`],
//!   [`QmpClient::query_status`].
//! * `ozma-capture` — [`QmpClient::screendump`] and
//!   [`QmpClient::add_client`] for D-Bus display handoff.
//!
//! # Socket modes
//!
//! QEMU can expose one or two QMP sockets:
//!
//! ```text
//! -qmp unix:/run/ozma/vm-ctrl.qmp,server,nowait
//! -qmp unix:/run/ozma/vm-input.qmp,server,nowait
//! ```
//!
//! QEMU accepts **one client per chardev**, so every node role that uses the
//! control socket must share it. [`SharedQmpClient`] is a cloneable wrapper
//! that serializes power, capture, snapshot, and input commands through one
//! control connection. With [`QmpConfig::dual`] the dedicated input socket is
//! used only for fire-and-forget `input-send-event` writes (a background task
//! drains QEMU's replies); the control socket remains the shared client. With
//! [`QmpConfig::new`] both roles share the control socket, through either the
//! shared wrapper or a single [`QmpClient`].
//!
//! # Example
//!
//! ```no_run
//! use ozma_qmp::{hid_to_qcode, QmpClient, QmpConfig, QmpInputEvent};
//!
//! # async fn example() -> Result<(), ozma_qmp::QmpError> {
//! let client = QmpClient::connect(QmpConfig::dual(
//!     "/run/ozma/vm-ctrl.qmp",
//!     "/run/ozma/vm-input.qmp",
//! )).await?;
//!
//! let status = client.query_status().await?;
//! assert!(status.running);
//!
//! let qcode = hid_to_qcode(0x04).expect("HID usage 0x04 is 'a'");
//! client
//!     .send_input_events(&[
//!         QmpInputEvent::key(qcode, true),
//!         QmpInputEvent::key(qcode, false),
//!     ])
//!     .await?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod backoff;
pub mod client;
pub mod config;
pub mod error;
pub mod hid;
pub mod input_event;
pub mod shared;

pub use backoff::{backoff_delay, Backoff, BACKOFF_FACTOR, BACKOFF_INITIAL, BACKOFF_MAX};
pub use client::{QmpClient, QmpStatus};
pub use config::{QmpConfig, DEFAULT_COMMAND_TIMEOUT, DEFAULT_CONNECT_TIMEOUT};
pub use error::QmpError;
pub use hid::{
    hid_to_qcode, KeyboardReportState, MouseReportState, RelativeMouseReportState, HID_TO_QCODE,
};
pub use input_event::{QmpInputEvent, QmpKeyValue};
pub use shared::SharedQmpClient;
