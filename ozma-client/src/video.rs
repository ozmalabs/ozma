// SPDX-License-Identifier: AGPL-3.0-only
//! Video reassembly moved to `ozma-client-core` so the browser client uses
//! exactly the same reassembler as the native client. Re-exported here to keep
//! existing `ozma_client::video` paths working.

pub use ozma_client_core::video::*;
