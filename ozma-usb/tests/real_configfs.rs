// SPDX-License-Identifier: AGPL-3.0-only
//! Opt-in validation against the host's real ConfigFS gadget tree.
//!
//! This test is ignored by default and additionally requires
//! `OZMA_USB_CONFIGFS_TEST=1`, root access, a mounted ConfigFS root, and a free
//! UDC (`OZMA_USB_UDC`). It never attaches media, mounts anything, or writes a
//! block device. The default suite must pass with no privileges.

use std::env;
use std::fs;
use std::path::Path;

use ozma_usb::{
    ConfigFsUsbController, UsbFunction, UsbGadgetConfig, UsbGadgetController, DEFAULT_CONFIGFS_ROOT,
};

#[test]
#[ignore = "requires root, configfs, and a spare UDC; set OZMA_USB_CONFIGFS_TEST=1"]
fn real_configfs_round_trip() {
    if env::var("OZMA_USB_CONFIGFS_TEST").as_deref() != Ok("1") {
        eprintln!("skipping: OZMA_USB_CONFIGFS_TEST is not 1");
        return;
    }
    let root =
        env::var("OZMA_USB_CONFIGFS_ROOT").unwrap_or_else(|_| DEFAULT_CONFIGFS_ROOT.to_owned());
    let root = Path::new(&root);
    if !ConfigFsUsbController::probe(root) {
        eprintln!("skipping: ConfigFS root {} is absent", root.display());
        return;
    }
    let Ok(udc) = env::var("OZMA_USB_UDC") else {
        eprintln!("skipping: set OZMA_USB_UDC to a free UDC (for example dummy_udc.0)");
        return;
    };

    let name = format!("ozma-usb-test-{}", std::process::id());
    let controller = ConfigFsUsbController::new(root);
    let config = UsbGadgetConfig::new(&name, &udc)
        .with_function(UsbFunction::keyboard("kbd"))
        .with_function(UsbFunction::mouse("mouse"));

    let status = controller.create_and_bind(&config).unwrap();
    assert!(status.bound);
    assert!(fs::metadata(root.join(&name)).unwrap().is_dir());

    let status = controller.status(&name).unwrap();
    assert_eq!(status.name, name);
    assert!(status.function("hid.kbd").is_some());

    controller.remove_gadget(&name).unwrap();
    assert!(!root.join(&name).exists());
}
