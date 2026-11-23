// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! `ozma-node` library crate — the supervisor, platform detection, discovery,
//! registration, API, and audio bridge used by the binary and tests.

pub mod api;
pub mod backend;
pub mod config;
pub mod mdns;
pub mod platform;
pub mod registration;
pub mod usb_audio;

pub use backend::SessionBackend;
pub use config::NodeConfig;
