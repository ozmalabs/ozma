// SPDX-License-Identifier: AGPL-3.0-only
//! Virtual-media model, attach/detach state, and read-only policy.

use std::fs;

use ozma_usb::{MassStorageImage, MediaMode, MediaState, UsbFunction, VirtualMedia};

#[test]
fn disk_and_cdrom_modes() {
    let disk = MassStorageImage::disk("/srv/media.img");
    assert_eq!(disk.mode, MediaMode::Disk);
    assert!(!disk.read_only);
    assert_eq!(disk.configfs_cdrom(), "0");
    assert_eq!(disk.configfs_ro(), "0");

    let cdrom = MassStorageImage::cdrom("/srv/installer.iso");
    assert_eq!(cdrom.mode, MediaMode::CdRom);
    assert!(cdrom.read_only);
    assert_eq!(cdrom.configfs_cdrom(), "1");
    assert_eq!(cdrom.configfs_ro(), "1");
    assert!(cdrom.validate().is_ok());
}

#[test]
fn cdrom_cannot_be_made_writable() {
    let error = MassStorageImage::cdrom("/srv/installer.iso")
        .with_read_only(false)
        .unwrap_err();
    assert_eq!(error.kind(), "invalid-config");

    let error = VirtualMedia::cdrom("usb0", "/srv/installer.iso")
        .with_read_only(false)
        .unwrap_err();
    assert_eq!(error.kind(), "invalid-config");
}

#[test]
fn read_only_disk_is_preserved() {
    let image = MassStorageImage::disk("/srv/media.img")
        .with_read_only(true)
        .unwrap();
    assert!(image.read_only);
    assert_eq!(image.configfs_ro(), "1");
}

#[test]
fn size_must_be_positive() {
    let error = MassStorageImage::disk("/srv/media.img")
        .with_size(0)
        .unwrap_err();
    assert_eq!(error.kind(), "invalid-config");

    let image = MassStorageImage::disk("/srv/media.img")
        .with_size(4096)
        .unwrap();
    assert_eq!(image.size_bytes, Some(4096));
    assert_eq!(image.effective_size(), Some(4096));
}

#[test]
fn empty_path_is_rejected() {
    let image = MassStorageImage::disk("");
    assert_eq!(image.validate().unwrap_err().kind(), "invalid-config");
}

#[test]
fn virtual_media_attach_detach_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("media.img");
    fs::write(&path, vec![0u8; 4096]).unwrap();

    let mut media = VirtualMedia::disk("usb0", &path);
    assert!(!media.is_attached());
    assert_eq!(media.state, MediaState::Detached);

    let state = media.attach().unwrap();
    assert!(media.is_attached());
    assert!(state.is_attached());
    match &state {
        MediaState::Attached {
            path: state_path,
            mode,
            read_only,
            size_bytes,
        } => {
            assert_eq!(state_path, &path);
            assert_eq!(*mode, MediaMode::Disk);
            assert!(!read_only);
            assert_eq!(*size_bytes, Some(4096));
        }
        other => panic!("unexpected state: {other:?}"),
    }

    media.detach();
    assert!(!media.is_attached());
    assert_eq!(media.state, MediaState::Detached);
}

#[test]
fn attach_requires_an_existing_regular_file() {
    let temp = tempfile::tempdir().unwrap();

    let missing = temp.path().join("missing.img");
    let mut media = VirtualMedia::disk("usb0", &missing);
    assert_eq!(media.attach().unwrap_err().kind(), "media-not-found");

    let directory = temp.path().join("directory");
    fs::create_dir(&directory).unwrap();
    let mut media = VirtualMedia::disk("usb0", &directory);
    assert_eq!(media.attach().unwrap_err().kind(), "media-not-file");

    let empty = temp.path().join("empty.img");
    fs::write(&empty, []).unwrap();
    let mut media = VirtualMedia::disk("usb0", &empty);
    assert_eq!(media.attach().unwrap_err().kind(), "invalid-config");
}

#[test]
fn virtual_media_converts_to_a_mass_storage_function() {
    let media = VirtualMedia::cdrom("usb0", "/srv/installer.iso")
        .with_size(1024)
        .unwrap();
    let function = media.as_function();
    assert_eq!(function.configfs_name(), "mass_storage.usb0");
    match function {
        UsbFunction::MassStorage {
            image, removable, ..
        } => {
            assert!(removable);
            let image = image.expect("image");
            assert_eq!(image.mode, MediaMode::CdRom);
            assert!(image.read_only);
        }
        other => panic!("unexpected function: {other:?}"),
    }
    assert_eq!(media.configfs_function_name(), "mass_storage.usb0");
}

#[test]
fn function_name_must_be_safe() {
    let media = VirtualMedia::disk("../escape", "/srv/media.img");
    assert_eq!(media.validate().unwrap_err().kind(), "invalid-config");
}
