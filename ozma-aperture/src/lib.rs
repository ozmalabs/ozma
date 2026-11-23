// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-aperture` — the pure capability and security negotiation model.
//!
//! An **aperture** is the set of admissible values per dimension. The
//! **effective aperture** is the meet (intersection) of the endpoint's
//! [`physiology`](Aperture) and every [`Frame`] currently in force. A frame has
//! [`Provenance`] — chosen, forced, or negotiated — and provenance determines
//! the [`Recourse`] available when a request falls outside the effective
//! aperture.
//!
//! The crate is deliberately pure: no filesystem I/O, no async runtime, no
//! platform code, and no authentication. It folds verified facts into a
//! decision and produces a serializable [`Receipt`] that explains the result.
//!
//! # Restriction orders
//!
//! Numeric dimensions take the minimum maximum. Sets take the intersection.
//! Booleans are ANDed (`false` is more restrictive). Ordered dimensions use an
//! explicit per-dimension order where a higher rank is more restrictive; see
//! [`Dimension::restriction_order`]. Unknown dimensions and unknown ordered
//! tokens only match an equal value and otherwise produce an empty dimension,
//! so newer policy cannot silently widen an older node.
//!
//! # Receipt hashes
//!
//! [`Receipt::input_hash`] is FNV-1a 64-bit over canonical JSON (stable
//! `BTreeMap`/`BTreeSet` ordering) of the physiology, frames, request, and
//! policy version. It is an evidence/diff hash, not a cryptographic MAC.
//!
//! # Example
//!
//! ```
//! use ozma_aperture::{
//!     evaluate, Aperture, Dimension, Evaluation, Frame, Provenance, Value,
//! };
//!
//! let physiology = Aperture::new()
//!     .with(Dimension::Width, Value::number(3840))
//!     .with(Dimension::Fps, Value::number(60));
//! let forced = Frame::new(
//!     "usb2-bandwidth",
//!     Provenance::Forced,
//!     Aperture::new().with(Dimension::Width, Value::number(1920)),
//! );
//! let request = Aperture::new()
//!     .with(Dimension::Width, Value::number(2560));
//!
//! let evaluation = evaluate(&physiology, &[forced], &request).expect("not empty");
//! match evaluation {
//!     Evaluation::Residual(residual) => {
//!         assert_eq!(residual.recourse.to_string(), "escalate");
//!         assert_eq!(residual.limiting[0].frame, "usb2-bandwidth");
//!     }
//!     Evaluation::Admitted(_) => panic!("expected a residual"),
//! }
//! ```

#![forbid(unsafe_code)]

mod dimension;
mod error;
mod evaluate;
mod frame;
mod policy;

pub use dimension::{Aperture, Dimension, Value};
pub use error::ApertureError;
pub use evaluate::{
    evaluate, evaluate_with_policy_version, input_hash, Admission, Evaluation, LimitingFrame,
    Receipt, Residual, ResidualEntry, UNVERSIONED_POLICY,
};
pub use frame::{Frame, Proposal, Provenance, Recourse};
pub use policy::PolicyDocument;
