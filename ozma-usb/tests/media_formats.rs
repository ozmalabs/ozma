// SPDX-License-Identifier: AGPL-3.0-only
//! ISO, NBD framing, and `MediaFormat` integration.
//!
//! No test mounts an ISO, opens an NBD socket, or writes a block device.

use std::fs;

use ozma_usb::{
    build_fat32_image, parse_iso_volume_descriptors, Fat32Node, IsoImage, MassStorageImage,
    MediaFormat, MediaMode, NbdExport, NbdExportDescriptor, NbdHandshake, NbdReply, NbdRequest,
    NbdRequestHeader, UsbError, VirtualMedia, NBD_HANDSHAKE_LEN, NBD_REQUEST_HEADER_LEN,
};

const ISO_SECTOR: usize = 2048;

fn minimal_iso() -> Vec<u8> {
    let mut bytes = vec![0u8; 18 * ISO_SECTOR];
    {
        let pvd = &mut bytes[16 * ISO_SECTOR..17 * ISO_SECTOR];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        pvd[8..40].fill(b' ');
        pvd[40..72].fill(b' ');
        pvd[8..13].copy_from_slice(b"LINUX");
        pvd[40..50].copy_from_slice(b"OZMA_MEDIA");
        pvd[80..84].copy_from_slice(&18u32.to_le_bytes());
        pvd[84..88].copy_from_slice(&18u32.to_be_bytes());
        pvd[124..126].copy_from_slice(&1u16.to_le_bytes());
        pvd[126..128].copy_from_slice(&1u16.to_be_bytes());
        pvd[128..132].copy_from_slice(&2048u32.to_le_bytes());
        pvd[132..136].copy_from_slice(&2048u32.to_be_bytes());
        pvd[156] = 34;
        pvd[158..162].copy_from_slice(&17u32.to_le_bytes());
        pvd[166..170].copy_from_slice(&2048u32.to_le_bytes());
    }
    {
        let terminator = &mut bytes[17 * ISO_SECTOR..18 * ISO_SECTOR];
        terminator[0] = 255;
        terminator[1..6].copy_from_slice(b"CD001");
        terminator[6] = 1;
    }
    bytes
}

#[test]
fn iso_volume_descriptor_and_read_only_image() {
    let bytes = minimal_iso();
    let volume = parse_iso_volume_descriptors(&bytes[16 * ISO_SECTOR..]).unwrap();
    assert_eq!(volume.volume_id, "OZMA_MEDIA");
    assert_eq!(volume.system_id, "LINUX");
    assert_eq!(volume.block_size, 2048);
    assert_eq!(volume.block_count, 18);
    assert_eq!(volume.root_extent, 17);
    assert_eq!(volume.volume_sequence_number, 1);

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("installer.iso");
    fs::write(&path, &bytes).unwrap();

    let image = IsoImage::inspect(&path).unwrap();
    assert!(image.read_only);
    assert_eq!(image.size_bytes, bytes.len() as u64);
    assert_eq!(image.volume.volume_id, "OZMA_MEDIA");
    image.validate().unwrap();

    let media = image.media_image();
    assert_eq!(media.mode, MediaMode::CdRom);
    assert!(media.read_only);
    assert_eq!(media.format, MediaFormat::Iso9660);
    assert_eq!(media.size_bytes, Some(bytes.len() as u64));
    assert_eq!(media.validate_for_attach().unwrap(), bytes.len() as u64);
}

#[test]
fn iso_rejects_missing_or_malformed_descriptors() {
    let temp = tempfile::tempdir().unwrap();
    let empty = temp.path().join("empty.iso");
    fs::write(&empty, []).unwrap();
    assert_eq!(
        IsoImage::inspect(&empty).unwrap_err().kind(),
        "invalid-media"
    );

    let zeroes = temp.path().join("zeroes.iso");
    fs::write(&zeroes, vec![0u8; 18 * ISO_SECTOR]).unwrap();
    assert_eq!(
        IsoImage::inspect(&zeroes).unwrap_err().kind(),
        "invalid-media"
    );

    let mut bytes = minimal_iso();
    bytes[16 * ISO_SECTOR + 84] ^= 0xFF;
    assert!(parse_iso_volume_descriptors(&bytes[16 * ISO_SECTOR..]).is_err());
}

#[test]
fn media_format_consistency_is_enforced() {
    let iso = MassStorageImage::iso("/srv/installer.iso");
    assert_eq!(iso.format, MediaFormat::Iso9660);
    assert_eq!(iso.mode, MediaMode::CdRom);
    assert!(iso.read_only);
    iso.validate().unwrap();

    let error = MassStorageImage::disk("/srv/disk.img")
        .with_format(MediaFormat::Iso9660)
        .unwrap_err();
    assert_eq!(error.kind(), "unsupported-media-format");

    let error = MassStorageImage::cdrom("/srv/installer.iso")
        .with_format(MediaFormat::Fat32)
        .unwrap_err();
    assert_eq!(error.kind(), "unsupported-media-format");

    let fat32 = MassStorageImage::fat32("/srv/media.img");
    assert_eq!(fat32.mode, MediaMode::Disk);
    assert!(fat32.read_only);
    assert_eq!(fat32.format, MediaFormat::Fat32);

    let nbd = MassStorageImage::nbd("/srv/media.img");
    assert_eq!(nbd.format, MediaFormat::Nbd);
    assert_eq!(
        nbd.validate_for_attach().unwrap_err().kind(),
        "unsupported-media-format"
    );

    let virtual_media = VirtualMedia::iso("usb0", "/srv/installer.iso");
    assert_eq!(virtual_media.image.format, MediaFormat::Iso9660);
    let virtual_media = VirtualMedia::fat32("usb0", "/srv/media.img");
    assert_eq!(virtual_media.image.format, MediaFormat::Fat32);
    let virtual_media = VirtualMedia::nbd("usb0", "/srv/media.img");
    assert_eq!(virtual_media.image.format, MediaFormat::Nbd);
}

#[test]
fn raw_media_serialization_is_unchanged_and_new_formats_are_tagged() {
    let raw = serde_json::to_value(MassStorageImage::disk("/srv/media.img")).unwrap();
    assert!(raw.get("format").is_none());

    let iso = serde_json::to_value(MassStorageImage::iso("/srv/installer.iso")).unwrap();
    assert_eq!(iso["format"], "iso9660");
    assert_eq!(iso["mode"], "cdrom");
}

#[test]
fn fat32_media_attach_probes_the_boot_sector() {
    let image = build_fat32_image(vec![Fat32Node::file("hello.txt", b"hello".to_vec())]).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("media.img");
    fs::write(&path, image.as_bytes()).unwrap();

    let media = MassStorageImage::fat32(&path)
        .with_size(image.len() as u64)
        .unwrap();
    assert_eq!(media.validate_for_attach().unwrap(), image.len() as u64);

    let mut corrupt = image.as_bytes().to_vec();
    corrupt[510] = 0;
    corrupt[511] = 0;
    let corrupt_path = temp.path().join("corrupt.img");
    fs::write(&corrupt_path, corrupt).unwrap();
    assert_eq!(
        MassStorageImage::fat32(&corrupt_path)
            .validate_for_attach()
            .unwrap_err()
            .kind(),
        "invalid-media"
    );
}

#[test]
fn nbd_export_descriptors_validate() {
    let export = NbdExport::read_only("ozma-media", 16 * 1024 * 1024).with_block_size(4096);
    export.validate().unwrap();
    assert!(export.handshake_flags() & 0x2 != 0);
    assert!(export.handshake_flags() & 0x1 != 0);

    let tcp = NbdExportDescriptor::tcp(export.clone());
    tcp.validate().unwrap();
    assert!(tcp.is_read_only());

    let socket = NbdExportDescriptor::unix(export.clone(), "/run/ozma/media.sock");
    socket.validate().unwrap();

    let invalid = NbdExportDescriptor {
        export: export.clone(),
        host: String::new(),
        port: 10809,
        unix_socket: None,
    };
    assert_eq!(invalid.validate().unwrap_err().kind(), "invalid-media");

    let invalid_block = NbdExport::read_only("x", 1024).with_block_size(1000);
    assert_eq!(
        invalid_block.validate().unwrap_err().kind(),
        "invalid-media"
    );

    let zero = NbdExport::read_only("x", 0);
    assert_eq!(zero.validate().unwrap_err().kind(), "invalid-media");
}

#[test]
fn nbd_handshake_framing_round_trips() {
    let export = NbdExport::read_only("ozma-media", 4 * 1024 * 1024);
    let handshake = export.handshake();
    let bytes = handshake.encode();
    assert_eq!(bytes.len(), NBD_HANDSHAKE_LEN);
    assert_eq!(&bytes[..8], b"NBDMAGIC");
    assert_eq!(&bytes[16..24], &(4u64 * 1024 * 1024).to_be_bytes());
    assert_eq!(NbdHandshake::decode(&bytes).unwrap(), handshake);

    let mut bad = bytes;
    bad[0] = b'X';
    assert_eq!(
        NbdHandshake::decode(&bad).unwrap_err().kind(),
        "protocol-error"
    );
    assert_eq!(
        NbdHandshake::decode(&[]).unwrap_err().kind(),
        "protocol-error"
    );
}

#[test]
fn nbd_request_and_reply_framing_round_trips() {
    let read = NbdRequest::Read {
        handle: 7,
        offset: 4096,
        length: 512,
    };
    let encoded = read.encode();
    assert_eq!(encoded.len(), NBD_REQUEST_HEADER_LEN);
    let header = NbdRequestHeader::decode(&encoded).unwrap();
    assert_eq!(header.handle, 7);
    assert_eq!(header.offset, 4096);
    assert_eq!(header.length, 512);
    assert_eq!(NbdRequest::decode(&encoded, &[]).unwrap(), read);

    let write = NbdRequest::Write {
        handle: 8,
        offset: 0,
        data: vec![0xAB; 16],
    };
    let encoded = write.encode();
    let (header, payload) = encoded.split_at(NBD_REQUEST_HEADER_LEN);
    assert_eq!(NbdRequest::decode(header, payload).unwrap(), write);
    assert_eq!(
        NbdRequest::decode(header, &payload[..4])
            .unwrap_err()
            .kind(),
        "protocol-error"
    );

    let disconnect = NbdRequest::Disconnect { handle: 9 };
    assert_eq!(
        NbdRequest::decode(&disconnect.encode(), &[]).unwrap(),
        disconnect
    );
    let flush = NbdRequest::Flush { handle: 9 };
    assert_eq!(NbdRequest::decode(&flush.encode(), &[]).unwrap(), flush);

    let mut bad = read.encode();
    bad[0] = 0;
    assert_eq!(
        NbdRequestHeader::decode(&bad).unwrap_err().kind(),
        "protocol-error"
    );

    let reply = NbdReply::success(7);
    let encoded = reply.encode();
    assert_eq!(NbdReply::decode(&encoded).unwrap(), reply);
    assert!(reply.is_success());
    assert_eq!(
        NbdReply::decode(&encoded[..4]).unwrap_err().kind(),
        "protocol-error"
    );
}

#[test]
fn nbd_media_errors_are_typed() {
    let error: UsbError = MassStorageImage::nbd("/srv/media.img")
        .validate_for_attach()
        .unwrap_err();
    assert_eq!(error.kind(), "unsupported-media-format");
}
