// SPDX-License-Identifier: AGPL-3.0-only
//! FunctionFS vendor-function model and ConfigFS behavior.
//!
//! These tests use a temporary ConfigFS-shaped root and regular marker files.
//! They never mount FunctionFS, open an endpoint, or spawn a daemon.

use std::fs;
use std::path::PathBuf;

use ozma_usb::{
    ConfigFsUsbController, FunctionFsConfig, FunctionFsDaemon, FunctionFsDaemonSpec,
    FunctionFsEndpoint, FunctionFsReadiness, FunctionFsStatus, FunctionFsStrings,
    MissingFunctionFsDaemon, ObservedFunctionFsDaemon, UsbFunction, UsbFunctionKind,
    UsbGadgetConfig, UsbGadgetController,
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    controller: ConfigFsUsbController,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("usb_gadget");
        fs::create_dir(&root).unwrap();
        let controller = ConfigFsUsbController::new(&root);
        Self {
            _temp: temp,
            root,
            controller,
        }
    }

    fn gadget(&self) -> PathBuf {
        self.root.join("ozma-test")
    }
}

fn vendor_config(instance: &str) -> FunctionFsConfig {
    FunctionFsConfig::new(instance, 0x1209, 0x0001)
        .with_interface(0xff, 0x42, 0x01)
        .with_endpoint(FunctionFsEndpoint::bulk_out(1, 512))
        .with_endpoint(FunctionFsEndpoint::bulk_in(2, 512))
        .with_strings(FunctionFsStrings {
            manufacturer: "Ozma Labs".to_owned(),
            product: "Ozma Vendor".to_owned(),
            serial_number: "OZMA-VENDOR-1".to_owned(),
            configuration: "Vendor Configuration".to_owned(),
        })
}

#[test]
fn functionfs_config_validates_and_generates_descriptors() {
    let config = vendor_config("vendor0");
    assert!(config.validate().is_ok());
    assert_eq!(config.configfs_name(), "ffs.vendor0");
    assert_eq!(config.endpoints.len(), 2);

    let descriptors = config.descriptors();
    assert_eq!(descriptors.device.len(), 18);
    assert_eq!(descriptors.device[0], 18);
    assert_eq!(descriptors.device[1], 0x01);
    assert_eq!(&descriptors.device[8..10], &0x1209u16.to_le_bytes());
    assert_eq!(&descriptors.device[10..12], &0x0001u16.to_le_bytes());
    assert_eq!(descriptors.device[4], 0xff);
    assert_eq!(descriptors.device[5], 0x42);
    assert_eq!(descriptors.device[6], 0x01);
    assert_eq!(descriptors.endpoint_count(), 2);
    assert_eq!(
        &descriptors.configuration[2..4],
        &(9u16 + 9 + 14).to_le_bytes()
    );
    // Interface + endpoint descriptors.
    assert_eq!(descriptors.configuration[13], 2);
    assert_eq!(descriptors.configuration[20], 0x01);
    assert_eq!(descriptors.configuration[27], 0x82);
    assert_eq!(descriptors.strings.len(), 5);
    assert_eq!(descriptors.strings[0], vec![4, 0x03, 0x09, 0x04]);
}

#[test]
fn functionfs_endpoint_and_string_bounds_are_typed() {
    for endpoint in [
        FunctionFsEndpoint {
            address: 0x00,
            transfer: ozma_usb::FunctionFsTransfer::Bulk,
            max_packet_size: 64,
            interval: 0,
        },
        FunctionFsEndpoint {
            address: 0x10,
            transfer: ozma_usb::FunctionFsTransfer::Bulk,
            max_packet_size: 64,
            interval: 0,
        },
        FunctionFsEndpoint {
            address: 0x01,
            transfer: ozma_usb::FunctionFsTransfer::Bulk,
            max_packet_size: 513,
            interval: 0,
        },
        FunctionFsEndpoint {
            address: 0x02,
            transfer: ozma_usb::FunctionFsTransfer::Isochronous,
            max_packet_size: 1024,
            interval: 0,
        },
    ] {
        let error = vendor_config("vendor0")
            .with_endpoint(endpoint)
            .validate()
            .unwrap_err();
        assert_eq!(error.kind(), "invalid-config");
    }

    let too_many = (1..=16).fold(vendor_config("vendor0"), |config, address| {
        config.with_endpoint(FunctionFsEndpoint::bulk_in(address, 64))
    });
    assert_eq!(too_many.validate().unwrap_err().kind(), "invalid-config");

    let duplicate = FunctionFsConfig::new("vendor0", 0x1209, 1)
        .with_endpoint(FunctionFsEndpoint::bulk_in(1, 64))
        .with_endpoint(FunctionFsEndpoint::bulk_in(1, 64));
    let error = duplicate.validate().unwrap_err();
    assert!(error.to_string().contains("duplicate FunctionFS endpoint"));

    let long_string = "x".repeat(127);
    let config = vendor_config("vendor0").with_strings(FunctionFsStrings {
        manufacturer: long_string,
        ..FunctionFsStrings::default()
    });
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-config");

    let relative_daemon =
        vendor_config("vendor0").with_daemon(FunctionFsDaemonSpec::new("relative-daemon"));
    assert_eq!(
        relative_daemon.validate().unwrap_err().kind(),
        "invalid-config"
    );
}

#[test]
fn functionfs_readiness_is_explicit_and_never_assumed() {
    let temp = tempfile::tempdir().unwrap();
    let daemon = temp.path().join("ffs-daemon");
    let marker = temp.path().join("ffs-ready");

    let unconfigured = vendor_config("vendor0");
    assert_eq!(
        unconfigured.readiness(),
        FunctionFsReadiness::DaemonNotConfigured
    );
    assert!(!unconfigured.status().ready);
    assert_eq!(
        unconfigured.require_ready().unwrap_err().kind(),
        "unsupported-functionfs"
    );

    let missing = unconfigured
        .clone()
        .with_daemon(FunctionFsDaemonSpec::new(&daemon));
    assert_eq!(missing.readiness(), FunctionFsReadiness::DaemonMissing);
    assert_eq!(
        missing.status().readiness,
        FunctionFsReadiness::DaemonMissing
    );

    fs::write(&daemon, b"#!/bin/sh\n").unwrap();
    let not_ready = missing
        .clone()
        .with_daemon(FunctionFsDaemonSpec::new(&daemon));
    assert_eq!(not_ready.readiness(), FunctionFsReadiness::DaemonNotReady);
    assert_eq!(
        not_ready.require_ready().unwrap_err().kind(),
        "unsupported-functionfs"
    );

    fs::write(&marker, b"ready\n").unwrap();
    let ready = unconfigured.with_daemon(
        FunctionFsDaemonSpec::new(&daemon)
            .with_arg("--control")
            .with_ready_marker(&marker),
    );
    assert_eq!(ready.readiness(), FunctionFsReadiness::Ready);
    assert!(ready.require_ready().is_ok());
    assert!(ready.status().ready);
    assert_eq!(
        ready.status().daemon_path.as_deref(),
        Some(daemon.as_path())
    );

    assert_eq!(
        MissingFunctionFsDaemon.status(&ready).readiness,
        FunctionFsReadiness::DaemonMissing
    );
    assert!(!MissingFunctionFsDaemon.status(&ready).ready);
    assert_eq!(
        ObservedFunctionFsDaemon.status(&ready).readiness,
        FunctionFsReadiness::Ready
    );
}

#[test]
fn functionfs_configfs_creation_and_status_in_a_temp_root() {
    let fixture = Fixture::new();
    let config = UsbGadgetConfig::new("ozma-test", "dummy_udc.0")
        .with_function(UsbFunction::vendor_with_config(vendor_config("vendor0")));
    let status = fixture.controller.create_gadget(&config).unwrap();
    assert!(fixture.gadget().join("functions/ffs.vendor0").is_dir());
    assert!(
        fs::symlink_metadata(fixture.gadget().join("configs/c.1/ffs.vendor0"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let function = status.function("ffs.vendor0").unwrap();
    assert_eq!(function.kind, UsbFunctionKind::Vendor);
    assert!(function.requires_userspace);
    let observed = function.function_fs.as_ref().unwrap();
    assert_eq!(observed.readiness, FunctionFsReadiness::Unknown);
    assert!(!observed.ready);

    let model = vendor_config("vendor0");
    let status = fixture
        .controller
        .function_fs_status("ozma-test", &model)
        .unwrap();
    assert_eq!(status.readiness, FunctionFsReadiness::DaemonNotConfigured);
    assert_eq!(status.endpoints, 2);
    assert!(status.descriptor_bytes > 0);

    let temp = tempfile::tempdir().unwrap();
    let daemon = temp.path().join("ffs-daemon");
    fs::write(&daemon, b"#!/bin/sh\n").unwrap();
    let marker = temp.path().join("ready");
    fs::write(&marker, b"1\n").unwrap();
    let ready = vendor_config("vendor0")
        .with_daemon(FunctionFsDaemonSpec::new(&daemon).with_ready_marker(&marker));
    let status = fixture
        .controller
        .function_fs_status("ozma-test", &ready)
        .unwrap();
    assert_eq!(status.readiness, FunctionFsReadiness::Ready);
    assert!(status.ready);
    assert!(status.daemon_path.is_some());

    let missing = FunctionFsStatus::missing("vendor0");
    assert_eq!(missing.readiness, FunctionFsReadiness::DaemonMissing);
    assert!(!missing.ready);

    assert_eq!(
        fixture
            .controller
            .function_fs_status("ozma-test", &vendor_config("absent"))
            .unwrap_err()
            .kind(),
        "missing-function"
    );
}

#[test]
fn fake_controller_exposes_functionfs_readiness_and_audio_absence() {
    let controller = ozma_usb::FakeUsbGadgetController::new();
    let config = UsbGadgetConfig::new("ozma", "dummy_udc.0")
        .with_function(UsbFunction::vendor_with_config(vendor_config("vendor0")))
        .with_function(UsbFunction::uac2("audio", 2, 1, 48_000, 24));
    let status = controller.create_gadget(&config).unwrap();

    let vendor = status.function("ffs.vendor0").unwrap();
    assert_eq!(vendor.function_fs.as_ref().unwrap().endpoints, 2);
    assert!(!vendor.function_fs.as_ref().unwrap().ready);
    let audio = status.function("uac2.audio").unwrap();
    assert!(!audio.uac2.as_ref().unwrap().is_bridged());
}
