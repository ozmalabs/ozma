// SPDX-License-Identifier: AGPL-3.0-only
//! In-memory fake controller contract.

use std::fs;

use ozma_usb::{
    FakeUsbGadgetController, MassStorageImage, MediaState, UsbFunction, UsbGadgetConfig,
    UsbGadgetController, VirtualMedia,
};

fn config() -> UsbGadgetConfig {
    UsbGadgetConfig::new("fake", "dummy_udc.0")
        .with_function(UsbFunction::keyboard("kbd"))
        .with_function(UsbFunction::mass_storage("usb0", None))
}

#[test]
fn fake_create_bind_and_status() {
    let controller = FakeUsbGadgetController::new();
    let status = controller.create_gadget(&config()).unwrap();
    assert!(!status.bound);
    assert_eq!(status.functions.len(), 2);
    assert_eq!(controller.list_gadgets().unwrap(), vec!["fake".to_owned()]);

    let bound = controller.bind("fake", "dummy_udc.0").unwrap();
    assert!(bound.bound);
    assert!(controller.is_bound("fake"));

    let error = controller.bind("fake", "dummy_udc.1").unwrap_err();
    assert_eq!(error.kind(), "already-bound");

    let unbound = controller.unbind("fake").unwrap();
    assert!(!unbound.bound);
    assert!(!controller.is_bound("fake"));
}

#[test]
fn fake_attach_detach_and_read_only() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("media.img");
    fs::write(&path, vec![0u8; 1024]).unwrap();

    let controller = FakeUsbGadgetController::new();
    controller.create_gadget(&config()).unwrap();

    assert_eq!(
        controller.media_state("fake", "usb0").unwrap(),
        MediaState::Detached
    );

    let media = VirtualMedia::disk("usb0", &path);
    let attached = controller.attach_media("fake", &media).unwrap();
    assert!(attached.is_attached());
    assert_eq!(
        controller.media_state("fake", "mass_storage.usb0").unwrap(),
        attached
    );

    let read_only = VirtualMedia::disk("usb0", &path)
        .with_read_only(true)
        .unwrap();
    let state = controller.attach_media("fake", &read_only).unwrap();
    match state {
        MediaState::Attached { read_only, .. } => assert!(read_only),
        other => panic!("unexpected state: {other:?}"),
    }

    assert_eq!(
        controller.detach_media("fake", "usb0").unwrap(),
        MediaState::Detached
    );
    assert!(!controller
        .media_state("fake", "usb0")
        .unwrap()
        .is_attached());
}

#[test]
fn fake_missing_gadget_and_function_are_typed() {
    let controller = FakeUsbGadgetController::new();
    assert_eq!(
        controller.status("missing").unwrap_err().kind(),
        "missing-gadget"
    );
    assert_eq!(
        controller.remove_gadget("missing").unwrap_err().kind(),
        "missing-gadget"
    );

    controller.create_gadget(&config()).unwrap();
    assert_eq!(
        controller.create_gadget(&config()).unwrap_err().kind(),
        "gadget-exists"
    );

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("media.img");
    fs::write(&path, b"data").unwrap();
    let media = VirtualMedia::disk("absent", path);
    assert_eq!(
        controller.attach_media("fake", &media).unwrap_err().kind(),
        "missing-function"
    );

    assert_eq!(
        controller
            .detach_media("fake", "absent")
            .unwrap_err()
            .kind(),
        "missing-function"
    );
}

#[test]
fn fake_reports_capabilities_and_initial_media() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("media.img");
    fs::write(&path, b"data").unwrap();

    let controller = FakeUsbGadgetController::new();
    let config = UsbGadgetConfig::new("fake", "dummy_udc.0").with_function(
        UsbFunction::mass_storage("usb0", Some(MassStorageImage::disk(&path))),
    );
    let status = controller.create_gadget(&config).unwrap();
    let media = status.function("usb0").unwrap().media.as_ref().unwrap();
    match media {
        MediaState::Attached { read_only, .. } => assert!(!read_only),
        other => panic!("unexpected state: {other:?}"),
    }
    assert!(controller.capabilities().gadget);

    let unavailable = FakeUsbGadgetController::unavailable();
    assert!(!unavailable.capabilities().gadget);
    assert!(!unavailable.capabilities().virtual_media);
}

#[test]
fn fake_removes_a_gadget() {
    let controller = FakeUsbGadgetController::new();
    controller.create_gadget(&config()).unwrap();
    controller.remove_gadget("fake").unwrap();
    assert!(controller.list_gadgets().unwrap().is_empty());
}
