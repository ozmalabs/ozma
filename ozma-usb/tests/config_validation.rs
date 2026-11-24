// SPDX-License-Identifier: AGPL-3.0-only
//! Configuration and function model validation.

use ozma_usb::{
    is_safe_identifier, HidProtocol, MassStorageImage, UsbFunction, UsbFunctionKind,
    UsbGadgetConfig, KEYBOARD_REPORT_DESCRIPTOR, MAX_POWER_MA, MOUSE_REPORT_DESCRIPTOR,
};

fn valid_config() -> UsbGadgetConfig {
    UsbGadgetConfig::new("ozma-test", "dummy_udc.0")
        .with_function(UsbFunction::keyboard("kbd"))
        .with_function(UsbFunction::mouse("mouse"))
        .with_function(UsbFunction::mass_storage(
            "usb0",
            Some(MassStorageImage::disk("/srv/media.img")),
        ))
}

#[test]
fn valid_gadget_passes_validation() {
    let config = valid_config();
    assert!(config.validate().is_ok());
    assert_eq!(config.name, "ozma-test");
    assert_eq!(config.id_vendor, 0x1d6b);
    assert_eq!(config.functions.len(), 3);
}

#[test]
fn gadget_needs_a_function() {
    let config = UsbGadgetConfig::new("ozma", "dummy_udc.0");
    let error = config.validate().unwrap_err();
    assert_eq!(error.kind(), "invalid-config");
    assert!(error.to_string().contains("at least one function"));
}

#[test]
fn gadget_name_rejects_traversal_spaces_and_empty_names() {
    for name in ["", "../ozma", "ozma/usb", "ozma test", "ozma.usb", "."] {
        let error = UsbGadgetConfig::new(name, "dummy_udc.0")
            .with_function(UsbFunction::keyboard("kbd"))
            .validate()
            .unwrap_err();
        assert_eq!(error.kind(), "invalid-config", "name {name:?}");
    }
}

#[test]
fn identifiers_accept_expected_configfs_names() {
    for name in ["ozma", "ozma-node", "node_1", "kbd0"] {
        assert!(is_safe_identifier(name), "{name:?}");
    }
    for name in ["", "-", "a b", "a/b", "a.b", "é"] {
        assert!(!is_safe_identifier(name), "{name:?}");
    }
}

#[test]
fn gadget_rejects_empty_udc_and_zero_ids() {
    let config = UsbGadgetConfig::new("ozma", "").with_function(UsbFunction::keyboard("kbd"));
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-config");

    let config = valid_config().with_identity(0, 0x0104);
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-config");

    let config = valid_config().with_identity(0x1d6b, 0);
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-config");
}

#[test]
fn gadget_rejects_excessive_power_and_duplicate_functions() {
    let config = valid_config().with_max_power_ma(MAX_POWER_MA + 1);
    assert_eq!(config.validate().unwrap_err().kind(), "invalid-config");

    let config = UsbGadgetConfig::new("ozma", "dummy_udc.0")
        .with_function(UsbFunction::keyboard("kbd"))
        .with_function(UsbFunction::keyboard("kbd"));
    let error = config.validate().unwrap_err();
    assert!(error.to_string().contains("duplicate ConfigFS function"));
}

#[test]
fn standard_hid_descriptors_and_kinds() {
    let keyboard = UsbFunction::keyboard("kbd");
    assert_eq!(keyboard.kind(), UsbFunctionKind::Hid);
    assert_eq!(keyboard.configfs_name(), "hid.kbd");
    assert_eq!(keyboard.instance(), "kbd");
    match &keyboard {
        UsbFunction::Hid {
            protocol,
            report_length,
            report_descriptor,
            ..
        } => {
            assert_eq!(*protocol, HidProtocol::Keyboard);
            assert_eq!(*report_length, 8);
            assert_eq!(report_descriptor.as_slice(), &KEYBOARD_REPORT_DESCRIPTOR);
        }
        other => panic!("unexpected function: {other:?}"),
    }
    assert!(keyboard.validate().is_ok());

    let mouse = UsbFunction::mouse("mouse");
    match &mouse {
        UsbFunction::Hid {
            report_length,
            report_descriptor,
            ..
        } => {
            assert_eq!(*report_length, 6);
            assert_eq!(report_descriptor.as_slice(), &MOUSE_REPORT_DESCRIPTOR);
        }
        other => panic!("unexpected function: {other:?}"),
    }
    assert!(mouse.validate().is_ok());
}

#[test]
fn hid_function_validation_rejects_wrong_report_shapes() {
    let keyboard = UsbFunction::keyboard("kbd");
    let UsbFunction::Hid {
        protocol,
        subclass,
        report_length: _,
        report_descriptor,
        instance,
    } = keyboard
    else {
        unreachable!()
    };
    let wrong_length = UsbFunction::Hid {
        instance,
        protocol,
        subclass,
        report_length: 7,
        report_descriptor: report_descriptor.clone(),
    };
    assert_eq!(
        wrong_length.validate().unwrap_err().kind(),
        "invalid-config"
    );

    let empty_descriptor = UsbFunction::Hid {
        instance: "kbd".to_owned(),
        protocol: HidProtocol::None,
        subclass: 0,
        report_length: 8,
        report_descriptor: Vec::new(),
    };
    assert_eq!(
        empty_descriptor.validate().unwrap_err().kind(),
        "invalid-config"
    );
}

#[test]
fn function_names_are_kind_prefixed() {
    assert_eq!(
        UsbFunction::mass_storage("usb0", None).configfs_name(),
        "mass_storage.usb0"
    );
    assert_eq!(
        UsbFunction::uac2("audio", 2, 1, 48_000, 16).configfs_name(),
        "uac2.audio"
    );
    assert_eq!(
        UsbFunction::vendor("vendor0", 0x1234, 0x5678).configfs_name(),
        "ffs.vendor0"
    );
    assert!(UsbFunction::vendor("vendor0", 0x1234, 0x5678).requires_userspace());
    assert!(!UsbFunction::keyboard("kbd").requires_userspace());
    assert_eq!(
        UsbFunctionKind::from_configfs_name("mass_storage.usb0"),
        Some(UsbFunctionKind::MassStorage)
    );
    assert_eq!(
        UsbFunctionKind::from_configfs_name("ffs.vendor0"),
        Some(UsbFunctionKind::Vendor)
    );
    assert_eq!(UsbFunctionKind::from_configfs_name("misc.thing"), None);
}

#[test]
fn uac2_and_vendor_validation() {
    let no_channels = UsbFunction::uac2("audio", 0, 0, 48_000, 16);
    assert_eq!(no_channels.validate().unwrap_err().kind(), "invalid-config");

    let bad_rate = UsbFunction::uac2("audio", 2, 0, 1_000, 16);
    assert_eq!(bad_rate.validate().unwrap_err().kind(), "invalid-config");

    let bad_depth = UsbFunction::uac2("audio", 2, 0, 48_000, 20);
    assert_eq!(bad_depth.validate().unwrap_err().kind(), "invalid-config");

    let valid = UsbFunction::uac2("audio", 2, 1, 48_000, 24);
    assert!(valid.validate().is_ok());

    let zero_vendor = UsbFunction::vendor("vendor0", 0, 0x1234);
    assert_eq!(zero_vendor.validate().unwrap_err().kind(), "invalid-config");
}

#[test]
fn mass_storage_function_accepts_an_empty_lun() {
    let function = UsbFunction::mass_storage("usb0", None);
    assert!(function.validate().is_ok());
    assert_eq!(function.configfs_name(), "mass_storage.usb0");
}
