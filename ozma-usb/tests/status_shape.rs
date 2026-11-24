// SPDX-License-Identifier: AGPL-3.0-only
//! JSON shapes consumed by the node status/registration surfaces later.

use ozma_usb::{
    FakeUsbGadgetController, MassStorageImage, UsbFunction, UsbGadgetCapabilities, UsbGadgetConfig,
    UsbGadgetController,
};

#[test]
fn capability_shape_projects_onto_protocol() {
    let capabilities = UsbGadgetCapabilities::available();
    let json = serde_json::to_value(&capabilities).unwrap();
    assert_eq!(json["gadget"], true);
    assert_eq!(json["virtual_media"], true);
    assert_eq!(json["mass_storage"], true);
    assert_eq!(json["usbip"], false);
    assert!(json["functions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|kind| kind == "hid"));
    assert!(json["functions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|kind| kind == "mass_storage"));

    let protocol = capabilities.with_usbip(true).protocol_capability();
    assert!(protocol.gadget);
    assert!(protocol.usbip);
    assert!(protocol.virtual_media);
}

#[test]
fn detached_and_attached_media_shapes() {
    let detached = serde_json::to_value(ozma_usb::MediaState::Detached).unwrap();
    assert_eq!(detached["state"], "detached");

    let image = MassStorageImage::disk("/srv/media.img")
        .with_read_only(true)
        .unwrap()
        .with_size(4096)
        .unwrap();
    let attached = serde_json::to_value(image.attached_state()).unwrap();
    assert_eq!(attached["state"], "attached");
    assert_eq!(attached["mode"], "disk");
    assert_eq!(attached["read_only"], true);
    assert_eq!(attached["size_bytes"], 4096);
    assert_eq!(attached["path"], "/srv/media.img");
}

#[test]
fn gadget_status_shape_is_stable() {
    let controller = FakeUsbGadgetController::new();
    let config = UsbGadgetConfig::new("ozma", "dummy_udc.0")
        .with_function(UsbFunction::keyboard("kbd"))
        .with_function(UsbFunction::mass_storage("usb0", None));
    let status = controller.create_gadget(&config).unwrap();
    let json = serde_json::to_value(&status).unwrap();

    assert_eq!(json["name"], "ozma");
    assert_eq!(json["present"], true);
    assert_eq!(json["bound"], false);
    assert!(json.get("udc").is_none(), "unbound status omits UDC");

    let functions = json["functions"].as_array().unwrap();
    assert_eq!(functions.len(), 2);
    let keyboard = functions
        .iter()
        .find(|function| function["name"] == "hid.kbd")
        .unwrap();
    assert_eq!(keyboard["kind"], "hid");
    assert_eq!(keyboard["linked"], true);
    assert!(keyboard.get("media").is_none());

    let storage = functions
        .iter()
        .find(|function| function["name"] == "mass_storage.usb0")
        .unwrap();
    assert_eq!(storage["kind"], "mass_storage");
    assert_eq!(storage["media"]["state"], "detached");
}
