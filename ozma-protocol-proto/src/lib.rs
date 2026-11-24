// SPDX-License-Identifier: AGPL-3.0-only
//! Generated protobuf and JSON types for the Ozma protocol.
//!
//! The `.proto` files under `proto/ozma/v1` are the source of truth. This crate
//! compiles them at build time with `prost-build` and generates proto3 JSON
//! serde implementations with `pbjson-build`.
//!
//! All messages and enums live in [`v1`] and use the `ozma.v1` protobuf
//! package.

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/ozma.v1.rs"));
    include!(concat!(env!("OUT_DIR"), "/ozma.v1.serde.rs"));
}

/// `FileDescriptorSet` for every `ozma.v1` proto compiled by this crate.
///
/// The descriptor set is emitted by `build.rs` and can be used for reflection,
/// gRPC-style tooling, or validating generated bindings.
pub const FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/ozma_v1_descriptor.bin"));
