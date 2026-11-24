// SPDX-License-Identifier: AGPL-3.0-only
//! Safe FunctionFS lifecycle tests.
//!
//! The default suite stages a temporary ConfigFS-shaped root, a temporary
//! mountpoint, a fake mounter, and a fake UDC name. It never mounts
//! FunctionFS, writes a real UDC, or requires root.

use std::fs;
use std::path::PathBuf;

use ozma_usb::{
    CommandFunctionFsMounter, FakeFunctionFsMounter, FunctionFsConfig, FunctionFsDaemonSpec,
    FunctionFsEndpoint, FunctionFsLifecycle, FunctionFsLifecycleConfig, FunctionFsLifecyclePhase,
    FunctionFsMounter, ManualFunctionFsLifecycleClock,
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    mountpoint: PathBuf,
    marker: PathBuf,
    function: FunctionFsConfig,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("usb_gadget");
        fs::create_dir(&root).unwrap();
        let mountpoint = temp.path().join("dev/ffs-vendor0");
        let program = temp.path().join("ffs-daemon");
        fs::write(&program, b"#!/bin/sh\nexit 0\n").unwrap();
        let marker = temp.path().join("ffs-ready");
        let function = FunctionFsConfig::new("vendor0", 0x1209, 0x0001)
            .with_interface(0xff, 0x42, 0x01)
            .with_endpoint(FunctionFsEndpoint::bulk_out(1, 512))
            .with_endpoint(FunctionFsEndpoint::bulk_in(2, 512))
            .with_daemon(FunctionFsDaemonSpec::new(&program).with_ready_marker(&marker));
        Self {
            _temp: temp,
            root,
            mountpoint,
            marker,
            function,
        }
    }

    fn config(&self, udc: &str) -> FunctionFsLifecycleConfig {
        FunctionFsLifecycleConfig::new("ozma-test", &self.root, udc, self.function.clone())
            .with_mountpoint(&self.mountpoint)
            .with_readiness(100, 10)
    }

    fn lifecycle(&self, udc: &str) -> FunctionFsLifecycle {
        FunctionFsLifecycle::new(self.config(udc)).unwrap()
    }

    fn gadget(&self) -> PathBuf {
        self.root.join("ozma-test")
    }

    fn function_dir(&self) -> PathBuf {
        self.gadget().join("functions/ffs.vendor0")
    }

    fn config_link(&self) -> PathBuf {
        self.gadget().join("configs/c.1/ffs.vendor0")
    }

    fn ready(&self) {
        fs::write(&self.marker, b"ready\n").unwrap();
    }

    fn staged_mount(&self) {
        fs::create_dir_all(&self.mountpoint).unwrap();
        fs::write(self.mountpoint.join("ep0"), b"").unwrap();
    }
}

fn fake() -> FakeFunctionFsMounter {
    FakeFunctionFsMounter::new().with_endpoints([1, 2])
}

#[test]
fn prepare_creates_the_function_and_config_link_without_binding() {
    let fixture = Fixture::new();
    let mut lifecycle = fixture.lifecycle("dummy_udc.0");
    let status = lifecycle.prepare().unwrap();

    assert_eq!(status.phase, FunctionFsLifecyclePhase::Prepared);
    assert_eq!(status.function, "ffs.vendor0");
    assert!(fixture.function_dir().is_dir());
    assert!(fs::symlink_metadata(fixture.config_link())
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!status.bound);
    assert!(!status.mounted);
    assert!(!status.ep0_written);
    assert!(!fixture.gadget().join("UDC").exists());

    lifecycle.cleanup().unwrap();
    assert!(!fixture.gadget().exists());
    assert_eq!(lifecycle.phase(), FunctionFsLifecyclePhase::TornDown);
}

#[test]
fn mountpoint_validation_and_fake_mounter_are_explicit() {
    let fixture = Fixture::new();

    let mut missing = fixture.lifecycle("dummy_udc.0");
    missing.prepare().unwrap();
    assert_eq!(
        missing.mount().unwrap_err().kind(),
        "missing-functionfs-mount"
    );

    // An existing directory without ep0 is not a mount either.
    fs::create_dir_all(&fixture.mountpoint).unwrap();
    assert_eq!(
        missing.mount().unwrap_err().kind(),
        "missing-functionfs-ep0"
    );
    missing.cleanup().unwrap();

    let mount = fake();
    let mut lifecycle = FunctionFsLifecycle::new(fixture.config("dummy_udc.0"))
        .unwrap()
        .with_mounter(mount.clone());
    lifecycle.prepare().unwrap();
    let status = lifecycle.mount().unwrap();
    assert_eq!(status.phase, FunctionFsLifecyclePhase::Mounted);
    assert!(status.mounted);
    assert_eq!(mount.mounts(), 1);
    assert!(fixture.mountpoint.join("ep0").is_file());
    assert!(fixture.mountpoint.join("ep1").is_file());
    assert!(fixture.mountpoint.join("ep2").is_file());

    lifecycle.cleanup().unwrap();
    assert_eq!(mount.unmounts(), 1);
    assert!(!fixture.mountpoint.exists());
    assert!(!fixture.gadget().exists());
}

#[test]
fn descriptors_readiness_bind_unbind_and_cleanup() {
    let fixture = Fixture::new();
    let mount = fake();
    let mut lifecycle = FunctionFsLifecycle::new(fixture.config("dummy_udc.0"))
        .unwrap()
        .with_mounter(mount.clone())
        .with_clock(ManualFunctionFsLifecycleClock::new(0));
    lifecycle.prepare().unwrap();
    lifecycle.mount().unwrap();
    let status = lifecycle.write_descriptors().unwrap();
    assert_eq!(status.phase, FunctionFsLifecyclePhase::DescriptorsWritten);
    assert!(status.ep0_written);
    assert!(status.strings_written);
    assert_eq!(
        status.function_fs.readiness,
        ozma_usb::FunctionFsReadiness::DaemonNotReady
    );

    let descriptors = fixture.function.ep0_descriptors().unwrap();
    let ep0 = fs::read(fixture.mountpoint.join("ep0")).unwrap();
    assert_eq!(
        ep0.len(),
        descriptors.descriptor_blob().unwrap().len() + descriptors.strings().len()
    );
    assert_eq!(&ep0[..4], &3u32.to_le_bytes()[..]);

    // Binding before readiness is refused and does not touch UDC.
    assert_eq!(
        lifecycle.bind().unwrap_err().kind(),
        "invalid-functionfs-state"
    );
    assert!(!fixture.gadget().join("UDC").exists());

    fixture.ready();
    let status = lifecycle.wait_for_ready().unwrap();
    assert_eq!(status.phase, FunctionFsLifecyclePhase::Ready);
    assert!(status.function_fs.ready);

    let status = lifecycle.bind().unwrap();
    assert_eq!(status.phase, FunctionFsLifecyclePhase::Bound);
    assert!(status.bound);
    assert_eq!(
        fs::read_to_string(fixture.gadget().join("UDC")).unwrap(),
        "dummy_udc.0"
    );

    let status = lifecycle.unbind().unwrap();
    assert_eq!(status.phase, FunctionFsLifecyclePhase::Ready);
    assert!(!status.bound);
    assert_eq!(
        fs::read_to_string(fixture.gadget().join("UDC")).unwrap(),
        ""
    );

    lifecycle.cleanup().unwrap();
    assert!(!fixture.gadget().exists());
    assert!(!fixture.mountpoint.exists());
    assert_eq!(mount.unmounts(), 1);
    assert_eq!(lifecycle.phase(), FunctionFsLifecyclePhase::TornDown);

    // Cleanup is idempotent.
    lifecycle.cleanup().unwrap();
}

#[test]
fn readiness_timeout_is_typed_and_never_binds() {
    let fixture = Fixture::new();
    let mount = fake();
    let mut lifecycle = FunctionFsLifecycle::new(fixture.config("dummy_udc.0"))
        .unwrap()
        .with_mounter(mount.clone())
        .with_clock(ManualFunctionFsLifecycleClock::new(0));
    lifecycle.prepare().unwrap();
    lifecycle.mount().unwrap();
    lifecycle.write_descriptors().unwrap();

    let error = lifecycle.wait_for_ready().unwrap_err();
    assert_eq!(error.kind(), "functionfs-readiness-timeout");
    assert_eq!(
        lifecycle.phase(),
        FunctionFsLifecyclePhase::DescriptorsWritten
    );
    assert!(!fixture.gadget().join("UDC").exists());
    assert_eq!(
        lifecycle.bind().unwrap_err().kind(),
        "invalid-functionfs-state"
    );

    lifecycle.cleanup().unwrap();
}

#[test]
fn bind_failure_rolls_back_the_gadget_and_owned_mount() {
    let fixture = Fixture::new();
    let mount = fake();
    let mut lifecycle = FunctionFsLifecycle::new(fixture.config("dummy_udc.0"))
        .unwrap()
        .with_mounter(mount.clone())
        .with_clock(ManualFunctionFsLifecycleClock::new(0));
    lifecycle.prepare().unwrap();
    lifecycle.mount().unwrap();
    lifecycle.write_descriptors().unwrap();
    fixture.ready();
    lifecycle.wait_for_ready().unwrap();

    // Make the ConfigFS UDC attribute unwritable by replacing it with a
    // directory, which is how a fake UDC binding failure is staged.
    fs::create_dir(fixture.gadget().join("UDC")).unwrap();
    let error = lifecycle.bind().unwrap_err();
    assert_eq!(error.kind(), "configfs-io");

    assert_eq!(lifecycle.phase(), FunctionFsLifecyclePhase::Failed);
    assert!(!fixture.gadget().exists(), "rollback removes the gadget");
    assert!(
        !fixture.mountpoint.exists(),
        "rollback unmounts owned mounts"
    );
    assert_eq!(mount.unmounts(), 1);
    assert!(!lifecycle.status().bound);

    lifecycle.cleanup().unwrap();
    assert_eq!(lifecycle.phase(), FunctionFsLifecyclePhase::TornDown);
}

#[test]
fn cleanup_leaves_a_caller_supplied_mount_in_place() {
    let fixture = Fixture::new();
    fixture.staged_mount();
    let mut lifecycle = fixture.lifecycle("dummy_udc.0");
    lifecycle.prepare().unwrap();
    lifecycle.mount().unwrap();
    assert!(lifecycle.status().mounted);

    lifecycle.cleanup().unwrap();
    assert!(!fixture.gadget().exists());
    assert!(
        fixture.mountpoint.join("ep0").is_file(),
        "a mount the lifecycle did not create is caller-owned"
    );
}

#[test]
fn missing_mount_tool_is_a_typed_error() {
    let fixture = Fixture::new();
    let mut lifecycle = FunctionFsLifecycle::new(fixture.config("dummy_udc.0"))
        .unwrap()
        .with_mounter(CommandFunctionFsMounter::with_programs(
            "ozma-no-such-mount-program",
            "ozma-no-such-umount-program",
        ));
    lifecycle.prepare().unwrap();
    let error = lifecycle.mount().unwrap_err();
    assert_eq!(error.kind(), "missing-command");
    lifecycle.cleanup().unwrap();
}

#[test]
fn configuration_validation_rejects_relative_and_inconsistent_paths() {
    let fixture = Fixture::new();

    let relative = fixture
        .config("dummy_udc.0")
        .with_mountpoint("relative/mount");
    assert_eq!(
        FunctionFsLifecycle::new(relative).unwrap_err().kind(),
        "invalid-config"
    );

    let zero_timeout = fixture.config("dummy_udc.0").with_readiness(0, 10);
    assert_eq!(
        FunctionFsLifecycle::new(zero_timeout).unwrap_err().kind(),
        "invalid-config"
    );

    let slow_poll = fixture.config("dummy_udc.0").with_readiness(50, 100);
    assert_eq!(
        FunctionFsLifecycle::new(slow_poll).unwrap_err().kind(),
        "invalid-config"
    );

    let mut bad_udc = fixture.config("dummy_udc.0");
    bad_udc.udc = "../escape".to_owned();
    assert_eq!(
        FunctionFsLifecycle::new(bad_udc).unwrap_err().kind(),
        "invalid-config"
    );

    // The fake mounter's name is observable without a real command.
    assert_eq!(fake().name(), "fake");
    let command = CommandFunctionFsMounter::new();
    assert_eq!(command.name(), "command");
}
