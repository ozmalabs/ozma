// SPDX-License-Identifier: AGPL-3.0-only
//! ConfigFS controller behavior against a temporary root.
//!
//! These tests never touch `/sys/kernel/config/usb_gadget`, bind a UDC, mount
//! a filesystem, or write a block device. The real-gadget test is separate,
//! `#[ignore]`, and environment-gated.

use std::fs;
use std::path::{Path, PathBuf};

use ozma_usb::{
    ConfigFsUsbController, MassStorageImage, MediaMode, MediaState, UsbError, UsbFunction,
    UsbFunctionKind, UsbGadgetConfig, UsbGadgetController, VirtualMedia,
    KEYBOARD_REPORT_DESCRIPTOR,
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    image: PathBuf,
    controller: ConfigFsUsbController,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("usb_gadget");
        fs::create_dir(&root).unwrap();
        let image = temp.path().join("media.img");
        fs::write(&image, vec![0xA5; 4096]).unwrap();
        let controller = ConfigFsUsbController::new(&root);
        Self {
            _temp: temp,
            root,
            image,
            controller,
        }
    }

    fn config(&self) -> UsbGadgetConfig {
        UsbGadgetConfig::new("ozma-test", "dummy_udc.0")
            .with_function(UsbFunction::keyboard("kbd"))
            .with_function(UsbFunction::mouse("mouse"))
            .with_function(UsbFunction::mass_storage(
                "usb0",
                Some(MassStorageImage::disk(&self.image)),
            ))
    }

    fn gadget(&self) -> PathBuf {
        self.root.join("ozma-test")
    }

    fn read(&self, path: impl AsRef<Path>) -> String {
        fs::read_to_string(path).unwrap().trim().to_owned()
    }
}

#[test]
fn create_writes_descriptors_strings_config_and_functions() {
    let fixture = Fixture::new();
    let status = fixture.controller.create_gadget(&fixture.config()).unwrap();

    assert!(status.present);
    assert!(!status.bound);
    assert_eq!(status.functions.len(), 3);

    let gadget = fixture.gadget();
    assert_eq!(fixture.read(gadget.join("idVendor")), "0x1d6b");
    assert_eq!(fixture.read(gadget.join("idProduct")), "0x0104");
    assert_eq!(fixture.read(gadget.join("bcdDevice")), "0x0100");
    assert_eq!(fixture.read(gadget.join("bcdUSB")), "0x0200");

    assert_eq!(
        fixture.read(gadget.join("strings/0x409/manufacturer")),
        "Ozma Labs"
    );
    assert_eq!(
        fixture.read(gadget.join("strings/0x409/product")),
        "Ozma USB Gadget"
    );
    assert_eq!(
        fixture.read(gadget.join("configs/c.1/strings/0x409/configuration")),
        "Ozma USB Configuration"
    );
    assert_eq!(fixture.read(gadget.join("configs/c.1/MaxPower")), "500");

    assert_eq!(fixture.read(gadget.join("functions/hid.kbd/protocol")), "1");
    assert_eq!(fixture.read(gadget.join("functions/hid.kbd/subclass")), "1");
    assert_eq!(
        fixture.read(gadget.join("functions/hid.kbd/report_length")),
        "8"
    );
    assert_eq!(
        fs::read(gadget.join("functions/hid.kbd/report_desc")).unwrap(),
        KEYBOARD_REPORT_DESCRIPTOR
    );
    assert_eq!(
        fixture.read(gadget.join("functions/hid.mouse/report_length")),
        "6"
    );

    let lun = gadget.join("functions/mass_storage.usb0/lun.0");
    assert_eq!(
        fixture.read(&lun.join("file")),
        fixture.image.to_string_lossy()
    );
    assert_eq!(fixture.read(&lun.join("ro")), "0");
    assert_eq!(fixture.read(&lun.join("cdrom")), "0");
    assert_eq!(fixture.read(&lun.join("removable")), "1");
    assert_eq!(
        fixture.read(gadget.join("functions/mass_storage.usb0/stall")),
        "1"
    );

    for function in ["hid.kbd", "hid.mouse", "mass_storage.usb0"] {
        let link = gadget.join("configs/c.1").join(function);
        let metadata = fs::symlink_metadata(&link).unwrap();
        assert!(metadata.file_type().is_symlink(), "{function}");
        assert_eq!(
            fs::read_link(&link).unwrap(),
            gadget.join("functions").join(function)
        );
    }

    let media = status
        .function("mass_storage.usb0")
        .unwrap()
        .media
        .as_ref()
        .unwrap();
    assert!(media.is_attached());
    assert_eq!(
        status.function("hid.kbd").unwrap().kind,
        UsbFunctionKind::Hid
    );
    assert!(status.function("hid.kbd").unwrap().linked);
}

#[test]
fn bind_unbind_and_list() {
    let fixture = Fixture::new();
    fixture.controller.create_gadget(&fixture.config()).unwrap();

    assert_eq!(
        fixture.controller.list_gadgets().unwrap(),
        vec!["ozma-test".to_owned()]
    );

    let bound = fixture.controller.bind("ozma-test", "dummy_udc.0").unwrap();
    assert!(bound.bound);
    assert_eq!(bound.udc.as_deref(), Some("dummy_udc.0"));
    assert_eq!(fixture.read(fixture.gadget().join("UDC")), "dummy_udc.0");

    // Binding the same UDC is idempotent.
    let again = fixture.controller.bind("ozma-test", "dummy_udc.0").unwrap();
    assert!(again.bound);

    let error = fixture
        .controller
        .bind("ozma-test", "dummy_udc.1")
        .unwrap_err();
    assert_eq!(error.kind(), "already-bound");

    let unbound = fixture.controller.unbind("ozma-test").unwrap();
    assert!(!unbound.bound);
    assert_eq!(fixture.read(fixture.gadget().join("UDC")), "");

    // Unbinding again is idempotent.
    assert!(!fixture.controller.unbind("ozma-test").unwrap().bound);
}

#[test]
fn remove_gadget_removes_the_tree_and_links() {
    let fixture = Fixture::new();
    fixture.controller.create_gadget(&fixture.config()).unwrap();
    fixture.controller.bind("ozma-test", "dummy_udc.0").unwrap();

    fixture.controller.remove_gadget("ozma-test").unwrap();
    assert!(!fixture.gadget().exists());
    assert!(fixture.controller.list_gadgets().unwrap().is_empty());
    assert_eq!(
        fixture
            .controller
            .remove_gadget("ozma-test")
            .unwrap_err()
            .kind(),
        "missing-gadget"
    );
}

#[test]
fn attach_detach_and_status_for_a_writable_disk() {
    let fixture = Fixture::new();
    let config = UsbGadgetConfig::new("ozma-test", "dummy_udc.0")
        .with_function(UsbFunction::mass_storage("usb0", None));
    fixture.controller.create_gadget(&config).unwrap();

    let lun = fixture.gadget().join("functions/mass_storage.usb0/lun.0");
    assert_eq!(fixture.read(lun.join("file")), "");

    let media = VirtualMedia::disk("usb0", &fixture.image)
        .with_size(4096)
        .unwrap();
    let state = fixture
        .controller
        .attach_media("ozma-test", &media)
        .unwrap();
    assert!(state.is_attached());
    match &state {
        MediaState::Attached {
            path,
            mode,
            read_only,
            size_bytes,
        } => {
            assert_eq!(path, &fixture.image);
            assert_eq!(*mode, MediaMode::Disk);
            assert!(!read_only);
            assert_eq!(*size_bytes, Some(4096));
        }
        other => panic!("unexpected state: {other:?}"),
    }
    assert_eq!(
        fixture.read(lun.join("file")),
        fixture.image.to_string_lossy()
    );
    assert_eq!(fixture.read(lun.join("ro")), "0");
    assert_eq!(fixture.read(lun.join("cdrom")), "0");

    let observed = fixture
        .controller
        .media_state("ozma-test", "mass_storage.usb0")
        .unwrap();
    assert_eq!(observed, state);

    let detached = fixture
        .controller
        .detach_media("ozma-test", "usb0")
        .unwrap();
    assert_eq!(detached, MediaState::Detached);
    assert_eq!(fixture.read(lun.join("file")), "");
    assert!(!fixture
        .controller
        .media_state("ozma-test", "usb0")
        .unwrap()
        .is_attached());
}

#[test]
fn read_only_and_cdrom_flags_are_written_before_the_file() {
    let fixture = Fixture::new();
    let config = UsbGadgetConfig::new("ozma-test", "dummy_udc.0")
        .with_function(UsbFunction::mass_storage("usb0", None));
    fixture.controller.create_gadget(&config).unwrap();
    let lun = fixture.gadget().join("functions/mass_storage.usb0/lun.0");

    let read_only = VirtualMedia::disk("usb0", &fixture.image)
        .with_read_only(true)
        .unwrap();
    let state = fixture
        .controller
        .attach_media("ozma-test", &read_only)
        .unwrap();
    assert!(state.is_attached());
    assert_eq!(fixture.read(lun.join("ro")), "1");
    assert_eq!(fixture.read(lun.join("cdrom")), "0");

    let iso = fixture._temp.path().join("installer.iso");
    fs::write(&iso, vec![0x11; 2048]).unwrap();
    let cdrom = VirtualMedia::cdrom("usb0", &iso);
    let state = fixture
        .controller
        .attach_media("ozma-test", &cdrom)
        .unwrap();
    match state {
        MediaState::Attached {
            mode, read_only, ..
        } => {
            assert_eq!(mode, MediaMode::CdRom);
            assert!(read_only);
        }
        other => panic!("unexpected state: {other:?}"),
    }
    assert_eq!(fixture.read(lun.join("ro")), "1");
    assert_eq!(fixture.read(lun.join("cdrom")), "1");
    assert_eq!(fixture.read(lun.join("file")), iso.to_string_lossy());
}

#[test]
fn missing_root_gadget_function_and_duplicate_are_typed() {
    let temp = tempfile::tempdir().unwrap();
    let missing_root = ConfigFsUsbController::new(temp.path().join("absent"));
    assert_eq!(
        missing_root.list_gadgets().unwrap_err().kind(),
        "missing-configfs-root"
    );
    let config =
        UsbGadgetConfig::new("ozma", "dummy_udc.0").with_function(UsbFunction::keyboard("kbd"));
    assert_eq!(
        missing_root.create_gadget(&config).unwrap_err().kind(),
        "missing-configfs-root"
    );

    let not_a_directory = temp.path().join("file");
    fs::write(&not_a_directory, b"x").unwrap();
    let controller = ConfigFsUsbController::new(&not_a_directory);
    assert_eq!(
        controller.list_gadgets().unwrap_err().kind(),
        "configfs-root-not-directory"
    );

    let fixture = Fixture::new();
    assert_eq!(
        fixture.controller.status("missing").unwrap_err().kind(),
        "missing-gadget"
    );
    assert_eq!(
        fixture
            .controller
            .bind("missing", "dummy_udc.0")
            .unwrap_err()
            .kind(),
        "missing-gadget"
    );

    fixture.controller.create_gadget(&fixture.config()).unwrap();
    assert_eq!(
        fixture
            .controller
            .create_gadget(&fixture.config())
            .unwrap_err()
            .kind(),
        "gadget-exists"
    );

    let no_storage = UsbGadgetConfig::new("keyboard-only", "dummy_udc.0")
        .with_function(UsbFunction::keyboard("kbd"));
    fixture.controller.create_gadget(&no_storage).unwrap();
    let media = VirtualMedia::disk("usb0", &fixture.image);
    let error = fixture
        .controller
        .attach_media("keyboard-only", &media)
        .unwrap_err();
    assert_eq!(error.kind(), "missing-function");
    assert!(matches!(error, UsbError::MissingFunction { .. }));
}

#[test]
fn invalid_configuration_does_not_create_a_directory() {
    let fixture = Fixture::new();
    let config = UsbGadgetConfig::new("../escape", "dummy_udc.0")
        .with_function(UsbFunction::keyboard("kbd"));
    assert_eq!(
        fixture
            .controller
            .create_gadget(&config)
            .unwrap_err()
            .kind(),
        "invalid-config"
    );
    assert!(fixture.controller.list_gadgets().unwrap().is_empty());
}

#[test]
fn attaching_a_missing_image_is_typed_and_leaves_the_lun_empty() {
    let fixture = Fixture::new();
    let config = UsbGadgetConfig::new("ozma-test", "dummy_udc.0")
        .with_function(UsbFunction::mass_storage("usb0", None));
    fixture.controller.create_gadget(&config).unwrap();
    let lun = fixture.gadget().join("functions/mass_storage.usb0/lun.0");

    let missing = fixture._temp.path().join("missing.img");
    let media = VirtualMedia::disk("usb0", missing);
    let error = fixture
        .controller
        .attach_media("ozma-test", &media)
        .unwrap_err();
    assert_eq!(error.kind(), "media-not-found");
    assert_eq!(fixture.read(lun.join("file")), "");
}

#[test]
fn uac2_and_vendor_functions_are_created() {
    let fixture = Fixture::new();
    let config = UsbGadgetConfig::new("ozma-test", "dummy_udc.0")
        .with_function(UsbFunction::uac2("audio", 2, 1, 48_000, 24))
        .with_function(UsbFunction::vendor("vendor0", 0x1234, 0x5678));
    let status = fixture.controller.create_gadget(&config).unwrap();

    let uac2 = fixture.gadget().join("functions/uac2.audio");
    assert_eq!(fixture.read(uac2.join("p_chmask")), "3");
    assert_eq!(fixture.read(uac2.join("c_chmask")), "1");
    assert_eq!(fixture.read(uac2.join("p_srate")), "48000");
    assert_eq!(fixture.read(uac2.join("p_ssize")), "3");
    assert_eq!(fixture.read(uac2.join("req_number")), "2");

    assert!(fixture.gadget().join("functions/ffs.vendor0").is_dir());
    let vendor = status.function("ffs.vendor0").unwrap();
    assert_eq!(vendor.kind, UsbFunctionKind::Vendor);
    assert!(vendor.requires_userspace);
}

#[test]
fn failed_create_rolls_back_the_partial_gadget() {
    let fixture = Fixture::new();
    let missing = fixture._temp.path().join("missing.img");
    let config = UsbGadgetConfig::new("rollback", "dummy_udc.0")
        .with_function(UsbFunction::keyboard("kbd"))
        .with_function(UsbFunction::mass_storage(
            "usb0",
            Some(MassStorageImage::disk(missing)),
        ));

    let error = fixture.controller.create_gadget(&config).unwrap_err();
    assert_eq!(error.kind(), "media-not-found");
    assert!(!fixture.root.join("rollback").exists());
    assert!(fixture.controller.list_gadgets().unwrap().is_empty());
}

#[test]
fn capabilities_reflect_the_injected_root() {
    let fixture = Fixture::new();
    let capabilities = fixture.controller.capabilities();
    assert!(capabilities.configfs);
    assert!(capabilities.gadget);
    assert!(capabilities.virtual_media);
    assert!(capabilities.mass_storage);
    assert_eq!(
        capabilities.protocol_capability().gadget,
        capabilities.gadget
    );

    let missing = ConfigFsUsbController::new(fixture._temp.path().join("absent"));
    let capabilities = missing.capabilities();
    assert!(!capabilities.configfs);
    assert!(!capabilities.gadget);
}

#[test]
fn status_reports_detached_mass_storage_before_media_is_attached() {
    let fixture = Fixture::new();
    let config = UsbGadgetConfig::new("ozma-test", "dummy_udc.0")
        .with_function(UsbFunction::mass_storage("usb0", None));
    let status = fixture.controller.create_gadget(&config).unwrap();
    let function = status.function("usb0").unwrap();
    assert_eq!(function.kind, UsbFunctionKind::MassStorage);
    assert_eq!(function.media, Some(MediaState::Detached));
}
