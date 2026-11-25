// SPDX-License-Identifier: AGPL-3.0-only
//! Ordered FunctionFS handoff tests.
//!
//! The fake controller and fake mounter exercise the real phase order, rollback,
//! and single-unbind shutdown without ConfigFS, a real mount, or root.

use std::path::PathBuf;

use ozma_usb::{
    FakeFunctionFsMounter, FakeUsbGadgetController, FunctionFsHandoff, FunctionFsHandoffPhase,
    UsbFunction, UsbGadgetConfig, UsbGadgetController,
};

struct Fixture {
    _temp: tempfile::TempDir,
    mountpoint: PathBuf,
    controller: std::sync::Arc<FakeUsbGadgetController>,
    mounter: FakeFunctionFsMounter,
    gadget: UsbGadgetConfig,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        Self {
            mountpoint: temp.path().join("dev/ffs-vendor0"),
            _temp: temp,
            controller: std::sync::Arc::new(FakeUsbGadgetController::new()),
            mounter: FakeFunctionFsMounter::new().with_endpoints([1, 2]),
            gadget: UsbGadgetConfig::new("ozma-handoff", "dummy_udc.0")
                .with_function(UsbFunction::vendor("vendor0", 0x1d6b, 0x0106)),
        }
    }

    fn handoff(&self) -> FunctionFsHandoff {
        FunctionFsHandoff::new(
            self.controller.clone(),
            "ozma-handoff",
            "vendor0",
            "dummy_udc.0",
            &self.mountpoint,
        )
        .unwrap()
        .with_mounter(self.mounter.clone())
    }

    fn mounted(&self) -> FunctionFsHandoff {
        let mut handoff = self.handoff();
        handoff.prepare(&self.gadget).unwrap();
        handoff.mount().unwrap();
        handoff
    }
}

#[test]
fn orders_prepare_mount_daemon_bind_and_unbinds_once_on_shutdown() {
    let fixture = Fixture::new();
    let mut handoff = fixture.handoff();
    assert_eq!(handoff.phase(), FunctionFsHandoffPhase::Idle);

    // Bind is impossible before the function exists, is mounted, and has a
    // daemon that wrote ep0.
    assert_eq!(handoff.bind().unwrap_err().kind(), "functionfs-lifecycle");

    let status = handoff.prepare(&fixture.gadget).unwrap();
    assert_eq!(status.phase, FunctionFsHandoffPhase::Prepared);
    assert!(!status.bound);
    assert!(!status.mounted);
    assert!(!fixture.controller.is_bound("ozma-handoff"));
    assert_eq!(handoff.bind().unwrap_err().kind(), "functionfs-lifecycle");

    let status = handoff.mount().unwrap();
    assert_eq!(status.phase, FunctionFsHandoffPhase::Mounted);
    assert!(status.mounted);
    assert!(status.mounted_by_us);
    assert_eq!(fixture.mounter.mounts(), 1);
    assert!(fixture.mountpoint.join("ep0").is_file());
    assert_eq!(
        handoff.bind().unwrap_err().kind(),
        "functionfs-lifecycle",
        "the daemon must write descriptors before the UDC bind"
    );

    let status = handoff.daemon_started().unwrap();
    assert_eq!(status.phase, FunctionFsHandoffPhase::DaemonStarted);
    let status = handoff.bind().unwrap();
    assert_eq!(status.phase, FunctionFsHandoffPhase::Bound);
    assert!(status.bound);
    assert!(fixture.controller.is_bound("ozma-handoff"));
    assert_eq!(fixture.controller.bind_calls(), 1);
    assert_eq!(fixture.controller.unbind_calls(), 0);

    // Deterministic shutdown: one unbind, then unmount and remove.
    handoff.unbind_once().unwrap();
    assert_eq!(fixture.controller.unbind_calls(), 1);
    // A repeated call is a no-op, not a second UDC write.
    handoff.unbind_once().unwrap();
    assert_eq!(fixture.controller.unbind_calls(), 1);
    handoff.cleanup().unwrap();
    assert_eq!(handoff.phase(), FunctionFsHandoffPhase::TornDown);
    assert!(!fixture.mountpoint.exists());
    assert_eq!(fixture.mounter.unmounts(), 1);
    assert!(
        fixture
            .controller
            .status("ozma-handoff")
            .unwrap_err()
            .kind()
            == "missing-gadget"
    );
    assert_eq!(handoff.unbinds(), 1);
    // Cleanup is idempotent and must not add another unbind.
    handoff.cleanup().unwrap();
    assert_eq!(fixture.controller.unbind_calls(), 1);
}

#[test]
fn mount_failure_rolls_back_the_created_function() {
    let fixture = Fixture::new();
    let mut handoff = fixture.handoff();
    fixture.mounter.set_fail_mount(true);
    handoff.prepare(&fixture.gadget).unwrap();
    let error = handoff.mount().unwrap_err();
    assert_eq!(error.kind(), "functionfs-lifecycle");
    assert_eq!(handoff.phase(), FunctionFsHandoffPhase::Failed);
    assert!(
        fixture
            .controller
            .status("ozma-handoff")
            .unwrap_err()
            .kind()
            == "missing-gadget"
    );
    handoff.cleanup().unwrap();
}

#[test]
fn bind_failure_unbinds_removes_the_function_and_unmounts() {
    let fixture = Fixture::new();
    let mut handoff = fixture.mounted();
    handoff.daemon_started().unwrap();
    fixture.controller.set_fail_bind(true);
    let error = handoff.bind().unwrap_err();
    assert_eq!(error.kind(), "invalid-config");
    assert_eq!(handoff.phase(), FunctionFsHandoffPhase::Failed);
    assert!(
        fixture
            .controller
            .status("ozma-handoff")
            .unwrap_err()
            .kind()
            == "missing-gadget"
    );
    assert!(!fixture.mountpoint.exists());
    assert_eq!(fixture.mounter.unmounts(), 1);
    handoff.cleanup().unwrap();
}

#[test]
fn prepare_failure_is_typed_and_leaves_the_existing_gadget_alone() {
    let fixture = Fixture::new();
    fixture.controller.create_gadget(&fixture.gadget).unwrap();
    let mut handoff = fixture.handoff();
    let error = handoff.prepare(&fixture.gadget).unwrap_err();
    assert_eq!(error.kind(), "gadget-exists");
    assert_eq!(handoff.phase(), FunctionFsHandoffPhase::Failed);
    assert!(fixture.controller.status("ozma-handoff").is_ok());
    fixture.controller.remove_gadget("ozma-handoff").unwrap();
}

#[test]
fn missing_mount_without_a_mounter_is_typed_and_rolls_back() {
    let fixture = Fixture::new();
    let mut handoff = FunctionFsHandoff::new(
        fixture.controller.clone(),
        "ozma-handoff",
        "vendor0",
        "dummy_udc.0",
        &fixture.mountpoint,
    )
    .unwrap();
    handoff.prepare(&fixture.gadget).unwrap();
    let error = handoff.mount().unwrap_err();
    assert_eq!(error.kind(), "missing-functionfs-mount");
    assert_eq!(handoff.phase(), FunctionFsHandoffPhase::Failed);
    assert!(
        fixture
            .controller
            .status("ozma-handoff")
            .unwrap_err()
            .kind()
            == "missing-gadget"
    );
}

#[test]
fn configuration_mismatches_are_rejected_before_filesystem_work() {
    let fixture = Fixture::new();
    let mut other = UsbGadgetConfig::new("other-gadget", "dummy_udc.0")
        .with_function(UsbFunction::vendor("vendor0", 0x1d6b, 0x0106));
    let mut handoff = fixture.handoff();
    assert_eq!(
        handoff.prepare(&other).unwrap_err().kind(),
        "invalid-config"
    );
    other.udc = "other_udc.1".to_owned();
    assert_eq!(
        handoff.prepare(&other).unwrap_err().kind(),
        "invalid-config"
    );
    assert!(fixture.controller.list_gadgets().unwrap().is_empty());
}

#[test]
fn caller_supplied_mount_is_not_unmounted_by_cleanup() {
    let fixture = Fixture::new();
    // Stage a mount without the fake mounter so the handoff does not own it.
    std::fs::create_dir_all(&fixture.mountpoint).unwrap();
    std::fs::write(fixture.mountpoint.join("ep0"), b"").unwrap();
    let mut handoff = fixture.handoff();
    handoff.prepare(&fixture.gadget).unwrap();
    let status = handoff.mount().unwrap();
    assert!(status.mounted);
    assert!(!status.mounted_by_us);
    handoff.daemon_started().unwrap();
    handoff.bind().unwrap();
    handoff.unbind_once().unwrap();
    handoff.cleanup().unwrap();
    assert!(
        fixture.mountpoint.join("ep0").is_file(),
        "a caller-owned mount stays in place"
    );
    assert_eq!(fixture.mounter.unmounts(), 0);
}
