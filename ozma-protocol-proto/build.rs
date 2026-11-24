// SPDX-License-Identifier: AGPL-3.0-only
//! Build the Ozma protocol bindings from `proto/ozma/v1`.
//!
//! The generated Rust is written to `OUT_DIR` and included by `src/lib.rs`.
//! A single `FileDescriptorSet` is also written to
//! `OUT_DIR/ozma_v1_descriptor.bin` and re-exported by the crate for reflection
//! and tooling. pbjson serde implementations are generated from that
//! descriptor set.

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let proto_root = manifest_dir.join("..").join("proto");
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let descriptor_path = out_dir.join("ozma_v1_descriptor.bin");

    println!("cargo:rerun-if-changed={}", proto_root.display());
    println!("cargo:rerun-if-env-changed=PROTOC");

    let mut proto_files = Vec::new();
    collect_proto_files(&proto_root, &mut proto_files)?;
    if proto_files.is_empty() {
        return Err(format!("no .proto files found under {}", proto_root.display()).into());
    }
    for file in &proto_files {
        println!("cargo:rerun-if-changed={}", file.display());
    }

    // Prefer an explicitly configured protoc. Otherwise use the vendored binary
    // so the build does not depend on a system installation.
    if env::var_os("PROTOC").is_none() {
        let protoc = protoc_bin_vendored::protoc_bin_path()?;
        env::set_var("PROTOC", protoc);
    }

    let includes = [proto_root.as_path()];

    prost_build::Config::new()
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(&proto_files, &includes)?;

    let descriptor_set = read_descriptor_set(&descriptor_path)?;
    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_set)?
        .build(&[".ozma.v1"])?;

    Ok(())
}

fn collect_proto_files(
    directory: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut entries = std::fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_proto_files(&path, files)?;
        } else if path.extension() == Some(OsStr::new("proto")) {
            files.push(path);
        }
    }

    Ok(())
}

fn read_descriptor_set(path: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    std::fs::read(path).map_err(|error| {
        format!(
            "failed to read generated descriptor set {}: {error}",
            path.display()
        )
        .into()
    })
}
