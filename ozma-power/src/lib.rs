// SPDX-License-Identifier: AGPL-3.0-only
//! VM power backends for Ozma nodes.
//!
//! This crate replaces `softnode/libvirt_power.py` and the power methods of
//! `softnode/qmp_client.py`. Every backend implements [`PowerBackend`], so the
//! upcoming VM profile and the controller compatibility HTTP surface can select
//! libvirt or QMP without branching.
//!
//! * [`LibvirtPower`] shells out to `virsh` with an argv vector (no shell),
//!   exactly like the Python fallback path.
//! * [`QmpPower`] wraps [`ozma_qmp::QmpClient`] and shares its handshake,
//!   timeouts, and reconnect policy. A node can instead pass its one
//!   [`ozma_qmp::SharedQmpClient`] via [`QmpPower::with_shared_client`], so
//!   power serializes with QMP capture, snapshots, and input on the single
//!   control socket.
//!
//! # Status vocabulary
//!
//! [`PowerState`] serializes as `{"status": "..."}`. Backend output is
//! normalized to the stable Python names: `running`, `blocked`, `paused`,
//! `shutdown`, `shutoff`, `crashed`, `pmsuspended`, `unknown`.
//!
//! # Example
//!
//! ```no_run
//! use ozma_power::{LibvirtPower, PowerBackend};
//!
//! # async fn example() -> Result<(), ozma_power::PowerError> {
//! let mut power = LibvirtPower::new("my-vm");
//! power.connect().await?;
//!
//! let state = power.query_status().await?;
//! println!("state: {state}");
//!
//! power.cont().await?;           // resume paused, or start shutoff
//! power.system_powerdown().await?;
//! # Ok(())
//! # }
//! ```
//!
//! A QMP backend uses the same trait:
//!
//! ```no_run
//! use ozma_power::{PowerBackend, QmpPower};
//! use ozma_qmp::QmpConfig;
//!
//! # async fn example() -> Result<(), ozma_power::PowerError> {
//! let mut power = QmpPower::connect(QmpConfig::new("/run/ozma/vm-ctrl.qmp")).await?;
//! assert!(power.query_status().await?.is_running());
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

mod action;
mod error;
mod libvirt;
mod qmp;
mod runner;
mod state;
mod unavailable;

use async_trait::async_trait;

pub use action::PowerAction;
pub use error::PowerError;
pub use libvirt::{LibvirtPower, DEFAULT_LIBVIRT_URI};
pub use qmp::QmpPower;
pub use runner::{CommandOutput, CommandRunner, TokioCommandRunner};
pub use state::{PowerOutcome, PowerState, PowerStatus};
pub use unavailable::UnavailablePower;

/// VM power control shared by every backend.
///
/// Implementations are `Send + Sync` and object safe, so a node can hold a
/// `Box<dyn PowerBackend>` selected from configuration.
#[async_trait]
pub trait PowerBackend: Send + Sync {
    /// Current VM state.
    async fn query_status(&mut self) -> Result<PowerState, PowerError>;

    /// Resume a paused VM, or start a stopped one.
    async fn cont(&mut self) -> Result<(), PowerError>;

    /// Pause a running VM without powering it off.
    ///
    /// QMP backends send QEMU's `stop` (monitor pause); libvirt backends run
    /// `virsh suspend`. This is deliberately separate from
    /// [`PowerBackend::stop`], which terminates the VM. Backends that cannot
    /// pause return [`PowerError::Unsupported`].
    async fn pause(&mut self) -> Result<(), PowerError> {
        Err(PowerError::Unsupported("pause".to_string()))
    }

    /// Resume a paused VM.
    ///
    /// This is the explicit pause counterpart. The default implementation uses
    /// [`PowerBackend::cont`]; backends with a distinct resume command (libvirt
    /// `virsh resume`) may override it.
    async fn resume(&mut self) -> Result<(), PowerError> {
        self.cont().await
    }

    /// Request an orderly ACPI shutdown.
    ///
    /// This is a request to the guest, not a host-side power cut; QEMU keeps
    /// running until the guest handles it. Use [`PowerBackend::stop`] for a
    /// hard power-off.
    async fn system_powerdown(&mut self) -> Result<(), PowerError>;

    /// Request an orderly ACPI shutdown and report what the backend observed.
    ///
    /// Backends that can read the domain state back should override this so a
    /// request the guest has not handled yet is reported as
    /// [`PowerOutcome::pending`] instead of implying the VM is off. The default
    /// implementation preserves backends with no readback: it calls
    /// [`PowerBackend::system_powerdown`] and returns
    /// [`PowerOutcome::accepted`].
    async fn system_powerdown_outcome(&mut self) -> Result<PowerOutcome, PowerError> {
        self.system_powerdown().await?;
        Ok(PowerOutcome::accepted())
    }

    /// Reset the VM.
    async fn system_reset(&mut self) -> Result<(), PowerError>;

    /// Force the VM off (destroy, not a guest-requested shutdown).
    ///
    /// This is the hard power-off used by `/power/force-off`, `/power/stop`,
    /// and `PowerAction::Stop`. [`LibvirtPower`] runs `virsh destroy`;
    /// [`QmpPower`] sends QEMU's `quit`, which terminates the QEMU process
    /// without letting the guest flush. It is distinct from
    /// [`PowerBackend::system_powerdown`], which only requests ACPI shutdown.
    async fn stop(&mut self) -> Result<(), PowerError>;
}
