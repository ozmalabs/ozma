// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! `ozma-node` library crate — the supervisor, platform detection, discovery,
//! registration, API, and audio bridge used by the binary and tests.

pub mod agent_metrics;
pub mod aida64;
pub mod api;
pub mod audio_runtime;
pub mod backend;
pub mod backup;
pub mod browser_scene;
pub mod client_capture;
pub mod config;
pub mod controller_compat;
pub mod desktop_compat;
pub mod display_broker;
pub mod display_compat;
pub mod display_control;
pub mod display_outputs;
pub mod hardware;
pub mod hls;
pub mod host_surfaces;
pub mod input_select;
pub mod keyboard_layout;
pub mod mdns;
pub mod mesh_service;
pub mod multi_hls;
pub mod multi_webrtc;
pub mod platform;
pub mod registration;
pub mod rpa;
pub mod rtsp_runtime;
pub mod scene_replay;
pub mod seats;
pub mod shutdown;
pub mod software;
pub mod thermal;
pub mod transport;
pub mod usb_audio;
pub mod usb_uac2;
pub mod video;
pub mod vnc_server;
pub mod webrtc;

pub use backend::SessionBackend;
pub use client_capture::{ClientCaptureHub, ClientCaptureSource};
pub use config::NodeConfig;
