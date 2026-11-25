// SPDX-License-Identifier: AGPL-3.0-only
//! SCSI mass-storage data-path tests.
//!
//! Every test uses an in-memory or temporary regular-file base. Nothing mounts
//! media, opens a block device, or performs a real USB transfer.

use std::fs;

use ozma_usb::{
    build_fat32_image, Fat32Node, MassStorageDevice, MassStorageImage, MassStoragePersistPolicy,
    OverlayConfig, ScsiStatus, SCSI_SECTOR_BYTES,
};

fn sample_base(sectors: usize) -> Vec<u8> {
    let mut base = vec![0u8; sectors * SCSI_SECTOR_BYTES];
    for sector in 0..sectors {
        for (index, byte) in base
            .iter_mut()
            .enumerate()
            .skip(sector * SCSI_SECTOR_BYTES)
            .take(SCSI_SECTOR_BYTES)
        {
            *byte = (sector as u8).wrapping_mul(17).wrapping_add(index as u8);
        }
    }
    base
}

fn cbw(opcode: u8, cdb: &[u8], transfer_length: u32, direction_in: bool) -> Vec<u8> {
    let mut bytes = vec![0u8; 31];
    bytes[0..4].copy_from_slice(&0x4342_5355u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    bytes[8..12].copy_from_slice(&transfer_length.to_le_bytes());
    bytes[12] = if direction_in { 0x80 } else { 0x00 };
    bytes[13] = 0;
    bytes[14] = cdb.len() as u8;
    bytes[15..15 + cdb.len()].copy_from_slice(cdb);
    bytes[15] = opcode;
    bytes
}

fn cdb10(lba: u32, count: u16) -> Vec<u8> {
    let mut cdb = vec![0u8; 10];
    cdb[2..6].copy_from_slice(&lba.to_be_bytes());
    cdb[7..9].copy_from_slice(&count.to_be_bytes());
    cdb
}

fn read10(lba: u32, count: u16) -> Vec<u8> {
    let mut cdb = cdb10(lba, count);
    cdb[0] = 0x28;
    cbw(0x28, &cdb, count as u32 * 512, true)
}

fn write10(lba: u32, count: u16, data: &[u8]) -> Vec<u8> {
    let mut cdb = cdb10(lba, count);
    cdb[0] = 0x2a;
    cbw(0x2a, &cdb, data.len() as u32, false)
}

fn sync_cache() -> Vec<u8> {
    let mut cdb = vec![0u8; 10];
    cdb[0] = 0x35;
    cbw(0x35, &cdb, 0, false)
}

fn overlay_config(max_overlay_bytes: u64) -> OverlayConfig {
    OverlayConfig::new(SCSI_SECTOR_BYTES, max_overlay_bytes)
}

#[test]
fn writable_scsi_round_trip_keeps_the_base_immutable() {
    let base = sample_base(8);
    let original = base.clone();
    let mut device = MassStorageDevice::writable_bytes(
        base,
        overlay_config(4096),
        MassStoragePersistPolicy::manual(),
    )
    .unwrap();
    assert!(!device.is_read_only());
    assert_eq!(device.sector_count(), 8);

    let base_sector = device.read_sectors(0, 1).unwrap();
    assert_eq!(base_sector, original[..512]);

    let mut payload = vec![b'Z'; SCSI_SECTOR_BYTES];
    payload[10] = 0x42;
    let reply = device
        .execute_cbw(&write10(1, 1, &payload), &payload)
        .unwrap();
    assert_eq!(reply.status, ScsiStatus::Good);
    assert_eq!(reply.residue, 0);
    assert!(reply.data_in.is_empty());

    let reply = device.execute_cbw(&read10(1, 1), &[]).unwrap();
    assert_eq!(reply.status, ScsiStatus::Good);
    assert_eq!(reply.data_in, payload);
    assert_eq!(reply.residue, 0);

    // Direct reads see the same overlay content, and untouched sectors still
    // come from the immutable base.
    let mut direct = vec![0u8; SCSI_SECTOR_BYTES];
    device.read_at(512, &mut direct).unwrap();
    assert_eq!(direct, payload);
    device.read_at(0, &mut direct).unwrap();
    assert_eq!(direct, original[..512]);

    let status = device.overlay_status().unwrap();
    assert_eq!(status.allocated_blocks, 1);
    assert_eq!(status.allocated_bytes, 512);
    assert!(status.dirty);
    assert_eq!(status.base_bytes, 8 * 512);

    // The caller's base vector is untouched: the device owns a private copy.
    assert_eq!(original, sample_base(8));
}

#[test]
fn flush_and_stop_persist_and_reload_the_overlay() {
    let temp = tempfile::tempdir().unwrap();
    let base_path = temp.path().join("base.img");
    let persistence = temp.path().join("overlay.ozov");
    let base = sample_base(4);
    fs::write(&base_path, &base).unwrap();

    let config = overlay_config(4096).with_persistence(&persistence);
    let mut device = MassStorageDevice::writable_file(
        &base_path,
        config.clone(),
        MassStoragePersistPolicy::default(),
    )
    .unwrap();

    let mut payload = vec![0x5a; SCSI_SECTOR_BYTES];
    payload[0] = 0x01;
    let reply = device
        .execute_cbw(&write10(2, 1, &payload), &payload)
        .unwrap();
    assert_eq!(reply.status, ScsiStatus::Good);
    assert!(!persistence.exists());

    // SYNCHRONIZE CACHE triggers the configured overlay commit.
    let reply = device.execute_cbw(&sync_cache(), &[]).unwrap();
    assert_eq!(reply.status, ScsiStatus::Good);
    assert!(persistence.is_file());

    // Stopping is idempotent and also follows persist_on_stop.
    device.stop().unwrap();

    let mut reloaded = MassStorageDevice::load_writable_file(
        &base_path,
        config,
        MassStoragePersistPolicy::default(),
        &persistence,
    )
    .unwrap();
    assert_eq!(reloaded.read_sectors(2, 1).unwrap(), payload);
    assert_eq!(reloaded.read_sectors(0, 1).unwrap(), base[..512]);
    assert!(!reloaded.overlay_status().unwrap().dirty);
}

#[test]
fn manual_policy_does_not_persist_implicitly() {
    let temp = tempfile::tempdir().unwrap();
    let base_path = temp.path().join("base.img");
    let persistence = temp.path().join("overlay.ozov");
    fs::write(&base_path, sample_base(2)).unwrap();
    let config = overlay_config(4096).with_persistence(&persistence);
    let mut device =
        MassStorageDevice::writable_file(&base_path, config, MassStoragePersistPolicy::manual())
            .unwrap();
    device.write_sectors(1, &vec![7u8; 512]).unwrap();
    device.flush().unwrap();
    device.stop().unwrap();
    assert!(!persistence.exists());

    // An explicit commit is the only way to persist under the manual policy.
    device.commit().unwrap();
    assert!(persistence.is_file());
}

#[test]
fn read_only_media_rejects_writes_with_typed_and_scsi_errors() {
    let base = sample_base(4);
    let mut device = MassStorageDevice::read_only_bytes(base.clone(), 4096).unwrap();
    assert!(device.is_read_only());

    let error = device.write_at(0, b"x").unwrap_err();
    assert_eq!(error.kind(), "read-only-media");
    assert_eq!(
        device.write_sectors(0, &[0u8; 512]).unwrap_err().kind(),
        "read-only-media"
    );
    assert_eq!(device.read_sectors(0, 1).unwrap(), base[..512]);

    let payload = vec![1u8; 512];
    let reply = device
        .execute_cbw(&write10(0, 1, &payload), &payload)
        .unwrap();
    assert_eq!(reply.status, ScsiStatus::CheckCondition);
    let sense = reply.sense.unwrap();
    assert_eq!(sense.key, ozma_usb::SENSE_DATA_PROTECT);
    assert_eq!(sense.asc, 0x27);

    // REQUEST SENSE returns and clears the stored sense.
    let mut request_sense = vec![0u8; 6];
    request_sense[0] = 0x03;
    request_sense[4] = 18;
    let reply = device
        .execute_cbw(&cbw(0x03, &request_sense, 18, true), &[])
        .unwrap();
    assert_eq!(reply.data_in[0], 0x70);
    assert_eq!(reply.data_in[2] & 0x0f, ozma_usb::SENSE_DATA_PROTECT);

    // INQUIRY marks read-only media removable and MODE SENSE sets the WP bit.
    let mut inquiry = vec![0u8; 6];
    inquiry[0] = 0x12;
    inquiry[4] = 36;
    let reply = device
        .execute_cbw(&cbw(0x12, &inquiry, 36, true), &[])
        .unwrap();
    assert_eq!(reply.data_in[1] & 0x80, 0x80);
    assert_eq!(&reply.data_in[8..12], b"OZMA");

    let mut mode = vec![0u8; 6];
    mode[0] = 0x1a;
    mode[4] = 4;
    let reply = device.execute_cbw(&cbw(0x1a, &mode, 4, true), &[]).unwrap();
    assert_eq!(reply.data_in[2] & 0x80, 0x80);
}

#[test]
fn bounds_and_malformed_commands_are_typed() {
    let mut device = MassStorageDevice::writable_bytes(
        sample_base(4),
        overlay_config(4096),
        MassStoragePersistPolicy::manual(),
    )
    .unwrap();
    assert_eq!(
        device.read_at(2048, &mut [0u8; 1]).unwrap_err().kind(),
        "overlay-bounds"
    );
    assert_eq!(
        device.write_sectors(4, &[0u8; 512]).unwrap_err().kind(),
        "overlay-bounds"
    );

    // A READ beyond the last LBA is a check condition, not a panic.
    let reply = device.execute_cbw(&read10(4, 1), &[]).unwrap();
    assert_eq!(reply.status, ScsiStatus::CheckCondition);
    assert_eq!(reply.sense.unwrap().asc, 0x21);

    // A WRITE beyond the last LBA is also ILLEGAL REQUEST.
    let payload = vec![0u8; 512];
    let reply = device
        .execute_cbw(&write10(4, 1, &payload), &payload)
        .unwrap();
    assert_eq!(reply.status, ScsiStatus::CheckCondition);
    assert_eq!(reply.sense.unwrap().asc, 0x21);

    // A short OUT data phase is a CSW phase error.
    let reply = device
        .execute_cbw(&write10(0, 1, &payload), &payload[..10])
        .unwrap();
    assert_eq!(reply.status, ScsiStatus::PhaseError);

    // Unknown opcodes fail with INVALID COMMAND rather than success.
    let unknown = cbw(0xff, &[0xff, 0, 0, 0, 0, 0], 0, false);
    let reply = device.execute_cbw(&unknown, &[]).unwrap();
    assert_eq!(reply.status, ScsiStatus::CheckCondition);
    assert_eq!(reply.sense.unwrap().asc, 0x20);

    // A malformed CBW signature is a typed protocol error.
    let mut bad = read10(0, 1);
    bad[0] = 0;
    assert_eq!(
        device.execute_cbw(&bad, &[]).unwrap_err().kind(),
        "protocol-error"
    );
}

#[test]
fn overlay_exhaustion_does_not_partially_allocate() {
    let base = sample_base(4);
    let mut device = MassStorageDevice::writable_bytes(
        base.clone(),
        overlay_config(SCSI_SECTOR_BYTES as u64),
        MassStoragePersistPolicy::manual(),
    )
    .unwrap();

    device.write_sectors(0, &vec![1u8; 512]).unwrap();
    assert_eq!(device.overlay_status().unwrap().allocated_blocks, 1);

    let error = device.write_sectors(1, &vec![2u8; 512]).unwrap_err();
    assert_eq!(error.kind(), "overlay-bounds");
    let status = device.overlay_status().unwrap();
    assert_eq!(
        status.allocated_blocks, 1,
        "a rejected write allocates nothing"
    );
    assert_eq!(status.allocated_bytes, 512);
    assert_eq!(device.read_sectors(1, 1).unwrap(), base[512..1024]);

    // The SCSI path reports MEDIUM ERROR/WRITE ERROR for exhaustion, while the
    // range itself is valid.
    let payload = vec![3u8; 512];
    let reply = device
        .execute_cbw(&write10(1, 1, &payload), &payload)
        .unwrap();
    assert_eq!(reply.status, ScsiStatus::CheckCondition);
    let sense = reply.sense.unwrap();
    assert_eq!(sense.key, ozma_usb::SENSE_MEDIUM_ERROR);
    assert_eq!(sense.asc, 0x0c);
}

#[test]
fn media_model_selects_read_only_or_overlay_backing() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("media.img");
    let base = sample_base(2);
    fs::write(&path, &base).unwrap();

    let disk = MassStorageImage::disk(&path);
    let mut device = MassStorageDevice::from_media(
        &disk,
        overlay_config(4096),
        MassStoragePersistPolicy::manual(),
    )
    .unwrap();
    assert!(!device.is_read_only());
    device.write_sectors(1, &vec![9u8; 512]).unwrap();
    assert_eq!(fs::read(&path).unwrap(), base, "base file stays immutable");

    for media in [
        MassStorageImage::cdrom(&path),
        MassStorageImage::fat32(&path),
    ] {
        let mut device = MassStorageDevice::from_media(
            &media,
            overlay_config(4096),
            MassStoragePersistPolicy::manual(),
        )
        .unwrap();
        assert!(
            device.is_read_only(),
            "{} must be read-only",
            media.path.display()
        );
        assert_eq!(
            device.write_sectors(0, &[0u8; 512]).unwrap_err().kind(),
            "read-only-media"
        );
    }
}

#[test]
fn read_only_fat32_image_is_served_unchanged() {
    let image = build_fat32_image(vec![Fat32Node::file(
        "hello.txt",
        b"hello from ozma".to_vec(),
    )])
    .unwrap();
    let expected = image.as_bytes().to_vec();
    let mut device = MassStorageDevice::read_only_fat32(&image).unwrap();

    assert_eq!(device.sector_count() as usize, expected.len() / 512);
    let mut first = vec![0u8; 1024];
    device.read_at(0, &mut first).unwrap();
    assert_eq!(first, expected[..1024]);
    assert_eq!(
        device.write_sectors(0, &[0u8; 512]).unwrap_err().kind(),
        "read-only-media"
    );

    // Reading the FAT32 boot sector through the SCSI path is byte-identical.
    let reply = device.execute_cbw(&read10(0, 1), &[]).unwrap();
    assert_eq!(reply.status, ScsiStatus::Good);
    assert_eq!(reply.data_in, expected[..512]);
}
