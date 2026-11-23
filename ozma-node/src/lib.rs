// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! `ozma-node` library crate — the supervisor, platform detection, discovery,
//! registration, API, and audio bridge used by the binary and tests.

pub mod api;
pub mod audio_runtime;
pub mod backend;
pub mod browser_scene;
pub mod client_capture;
pub mod config;
pub mod controller_compat;
pub mod display_compat;
pub mod hls;
pub mod input_select;
pub mod mdns;
pub mod platform;
pub mod registration;
pub mod rtsp_runtime;
pub mod scene_replay;
pub mod transport;
pub mod usb_audio;
pub mod webrtc;

pub use backend::SessionBackend;
pub use client_capture::{ClientCaptureHub, ClientCaptureSource};
pub use config::NodeConfig;
