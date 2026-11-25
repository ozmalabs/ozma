// SPDX-License-Identifier: AGPL-3.0-only
//! FunctionFS bulk-endpoint event-loop tests.
//!
//! Every test uses an in-memory fake transport or FunctionFS loopback
//! endpoints. No test mounts FunctionFS, opens a real UDC endpoint, or performs
//! a kernel USB transfer.

use std::fs;

use ozma_usb::{
    BulkTransportFault, FakeFunctionFsDaemon, FileBulkTransport, FunctionFsBulkTransport,
    FunctionFsConfig, FunctionFsDaemon, FunctionFsEndpoint, FunctionFsEvent, IdleBulkTransport,
    MassStorageDevice, MassStorageLoop, MassStorageLoopConfig, MassStoragePersistPolicy,
    MemoryBulkTransport, OverlayConfig, ScsiStatus, SCSI_CBW_BYTES, SCSI_CSW_BYTES,
    SCSI_SECTOR_BYTES,
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
    let mut bytes = vec![0u8; SCSI_CBW_BYTES];
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
    cbw(0x28, &cdb, count as u32 * SCSI_SECTOR_BYTES as u32, true)
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

fn test_unit_ready() -> Vec<u8> {
    cbw(0x00, &[0u8; 6], 0, false)
}

fn request_sense() -> Vec<u8> {
    let mut cdb = vec![0u8; 6];
    cdb[0] = 0x03;
    cbw(0x03, &cdb, 18, true)
}

fn csw_from(bytes: &[u8]) -> &[u8] {
    assert!(bytes.len() >= SCSI_CSW_BYTES);
    &bytes[bytes.len() - SCSI_CSW_BYTES..]
}

fn csw_status(bytes: &[u8]) -> u8 {
    csw_from(bytes)[12]
}

#[test]
fn read_only_cbw_round_trip_over_fake_endpoints() {
    let mut transport = MemoryBulkTransport::new();
    transport.push_host_data(&test_unit_ready()).unwrap();
    transport.push_host_data(&read10(0, 1)).unwrap();

    let device = MassStorageDevice::read_only_bytes(sample_base(4), 4096).unwrap();
    let mut loop_ = MassStorageLoop::new(device, transport, MassStorageLoopConfig::default())
        .expect("bounds are valid");

    let ready = loop_.step().unwrap();
    assert_eq!(ready.status, ScsiStatus::Good);
    assert_eq!(ready.data_in_bytes, 0);
    assert_eq!(ready.data_out_bytes, 0);
    let out = loop_.transport_mut().take_device_data();
    assert_eq!(csw_status(&out), 0);
    assert_eq!(&csw_from(&out)[0..4], &0x5342_5355u32.to_le_bytes());

    let read = loop_.step().unwrap();
    assert_eq!(read.status, ScsiStatus::Good);
    assert_eq!(read.data_in_bytes, SCSI_SECTOR_BYTES);
    let out = loop_.transport_mut().take_device_data();
    assert_eq!(out.len(), SCSI_SECTOR_BYTES + SCSI_CSW_BYTES);
    assert_eq!(
        &out[..SCSI_SECTOR_BYTES],
        &sample_base(4)[..SCSI_SECTOR_BYTES]
    );
    assert_eq!(csw_status(&out), 0);

    assert!(loop_.is_running());
    assert_eq!(loop_.stats().commands, 2);
    assert_eq!(loop_.stats().cbws, 2);
    assert_eq!(loop_.stats().csws, 2);
    assert_eq!(loop_.stats().read_commands, 1);
    assert_eq!(loop_.stats().data_in_bytes, SCSI_SECTOR_BYTES as u64);
}

#[test]
fn write_read_and_flush_round_trip_persists_the_overlay() {
    let temp = tempfile::tempdir().unwrap();
    let base_path = temp.path().join("base.img");
    let persistence = temp.path().join("overlay.ozov");
    let base = sample_base(4);
    fs::write(&base_path, &base).unwrap();

    let overlay = OverlayConfig::new(SCSI_SECTOR_BYTES, 4096).with_persistence(&persistence);
    let device = MassStorageDevice::writable_file(
        &base_path,
        overlay.clone(),
        MassStoragePersistPolicy::default(),
    )
    .unwrap();

    let mut payload = vec![0x5au8; SCSI_SECTOR_BYTES];
    payload[0] = 0x42;
    let mut transport = MemoryBulkTransport::new();
    transport.push_host_data(&write10(1, 1, &payload)).unwrap();
    transport.push_host_data(&payload).unwrap();
    transport.push_host_data(&read10(1, 1)).unwrap();
    transport.push_host_data(&sync_cache()).unwrap();

    let mut loop_ = MassStorageLoop::new(device, transport, MassStorageLoopConfig::default())
        .expect("bounds are valid");

    let write = loop_.step().unwrap();
    assert_eq!(write.status, ScsiStatus::Good);
    assert_eq!(write.data_out_bytes, SCSI_SECTOR_BYTES);
    assert_eq!(csw_status(&loop_.transport_mut().take_device_data()), 0);

    let read = loop_.step().unwrap();
    assert_eq!(read.status, ScsiStatus::Good);
    let out = loop_.transport_mut().take_device_data();
    assert_eq!(&out[..SCSI_SECTOR_BYTES], &payload[..]);
    assert_eq!(csw_status(&out), 0);

    let flush = loop_.step().unwrap();
    assert_eq!(flush.status, ScsiStatus::Good);
    assert_eq!(csw_status(&loop_.transport_mut().take_device_data()), 0);
    assert_eq!(loop_.stats().write_commands, 1);
    assert_eq!(loop_.stats().flush_commands, 1);

    loop_.stop().unwrap();
    assert!(!loop_.is_running());
    assert!(persistence.is_file(), "SYNCHRONIZE CACHE must persist");

    // The base image is immutable.
    assert_eq!(fs::read(&base_path).unwrap(), base);

    let mut reloaded = MassStorageDevice::load_writable_file(
        &base_path,
        overlay,
        MassStoragePersistPolicy::manual(),
        &persistence,
    )
    .unwrap();
    let mut sector = vec![0u8; SCSI_SECTOR_BYTES];
    reloaded
        .read_at(SCSI_SECTOR_BYTES as u64, &mut sector)
        .unwrap();
    assert_eq!(sector, payload);
}

#[test]
fn read_only_write_is_rejected_with_data_protect() {
    let mut payload = vec![0x7eu8; SCSI_SECTOR_BYTES];
    payload[3] = 0x11;
    let mut transport = MemoryBulkTransport::new();
    transport.push_host_data(&write10(0, 1, &payload)).unwrap();
    transport.push_host_data(&payload).unwrap();
    transport.push_host_data(&request_sense()).unwrap();

    let device = MassStorageDevice::read_only_bytes(sample_base(2), 4096).unwrap();
    let mut loop_ = MassStorageLoop::new(device, transport, MassStorageLoopConfig::default())
        .expect("bounds are valid");

    let write = loop_.step().unwrap();
    assert_eq!(write.status, ScsiStatus::CheckCondition);
    assert_eq!(write.data_in_bytes, 0);
    let out = loop_.transport_mut().take_device_data();
    assert_eq!(csw_status(&out), 1);
    assert_eq!(loop_.stats().check_conditions, 1);

    let sense = loop_.step().unwrap();
    assert_eq!(sense.status, ScsiStatus::Good);
    let out = loop_.transport_mut().take_device_data();
    assert_eq!(out[2] & 0x0f, 0x07, "DATA PROTECT sense key");
    assert_eq!(csw_status(&out), 0);
}

#[test]
fn oversized_transfer_is_rejected_with_a_typed_bound() {
    let mut transport = MemoryBulkTransport::new();
    transport.push_host_data(&read10(0, 2)).unwrap();

    let config = MassStorageLoopConfig {
        max_transfer_bytes: SCSI_SECTOR_BYTES,
        ..MassStorageLoopConfig::default()
    };
    let device = MassStorageDevice::read_only_bytes(sample_base(4), 4096).unwrap();
    let mut loop_ = MassStorageLoop::new(device, transport, config).expect("bounds are valid");

    let error = loop_
        .step()
        .expect_err("1024 bytes exceeds the 512-byte bound");
    assert_eq!(error.kind(), "mass-storage-buffer-too-large");
    assert_eq!(loop_.stats().bound_errors, 1);
    assert_eq!(loop_.transport().pending_host_bytes(), 0);
    assert!(!loop_.is_running());
}

#[test]
fn stalled_endpoints_surface_typed_timeouts_and_io_errors() {
    let device = MassStorageDevice::read_only_bytes(sample_base(2), 4096).unwrap();
    let mut transport = MemoryBulkTransport::new();
    transport.set_read_fault(Some(BulkTransportFault::Timeout));
    let mut loop_ = MassStorageLoop::new(device, transport, MassStorageLoopConfig::default())
        .expect("bounds are valid");

    let error = loop_.step().expect_err("stalled OUT endpoint");
    assert_eq!(error.kind(), "mass-storage-endpoint-timeout");
    assert_eq!(loop_.stats().timeout_errors, 1);
    assert!(!loop_.is_running());

    // A write fault is surfaced the same way after the CBW was consumed.
    let mut transport = MemoryBulkTransport::new();
    transport.push_host_data(&test_unit_ready()).unwrap();
    transport.set_write_fault(Some(BulkTransportFault::Timeout));
    let device = MassStorageDevice::read_only_bytes(sample_base(2), 4096).unwrap();
    let mut loop_ = MassStorageLoop::new(device, transport, MassStorageLoopConfig::default())
        .expect("bounds are valid");

    let error = loop_.step().expect_err("stalled IN endpoint");
    assert_eq!(error.kind(), "mass-storage-endpoint-timeout");
    assert_eq!(loop_.stats().timeout_errors, 1);

    // A non-timeout failure keeps its own kind.
    let mut transport = MemoryBulkTransport::new();
    transport.set_read_fault(Some(BulkTransportFault::Io));
    let device = MassStorageDevice::read_only_bytes(sample_base(2), 4096).unwrap();
    let mut loop_ = MassStorageLoop::new(device, transport, MassStorageLoopConfig::default())
        .expect("bounds are valid");
    let error = loop_.step().expect_err("injected endpoint I/O failure");
    assert_eq!(error.kind(), "mass-storage-transport");
    assert_eq!(loop_.stats().transport_errors, 1);
}

#[test]
fn short_transfers_are_assembled_and_counted() {
    let mut transport = MemoryBulkTransport::new().with_chunks(7, 5);
    transport.push_host_data(&read10(0, 1)).unwrap();

    let device = MassStorageDevice::read_only_bytes(sample_base(2), 4096).unwrap();
    let mut loop_ = MassStorageLoop::new(device, transport, MassStorageLoopConfig::default())
        .expect("bounds are valid");

    let read = loop_.step().unwrap();
    assert_eq!(read.status, ScsiStatus::Good);
    assert_eq!(read.data_in_bytes, SCSI_SECTOR_BYTES);
    let out = loop_.transport_mut().take_device_data();
    assert_eq!(out.len(), SCSI_SECTOR_BYTES + SCSI_CSW_BYTES);
    assert_eq!(
        &out[..SCSI_SECTOR_BYTES],
        &sample_base(2)[..SCSI_SECTOR_BYTES]
    );
    assert!(loop_.transport().reads() > 1, "CBW/data reads were short");
    assert!(loop_.transport().writes() > 1, "data/CSW writes were short");
}

#[test]
fn idle_transport_never_claims_running() {
    let device = MassStorageDevice::read_only_bytes(sample_base(2), 4096).unwrap();
    let mut loop_ =
        MassStorageLoop::new(device, IdleBulkTransport, MassStorageLoopConfig::default())
            .expect("bounds are valid");

    let error = loop_.step().expect_err("no endpoint is attached");
    assert_eq!(error.kind(), "mass-storage-not-attached");
    assert!(!loop_.is_running());
    assert_eq!(loop_.stats().commands, 0);
}

#[test]
fn stop_commits_according_to_policy() {
    let temp = tempfile::tempdir().unwrap();
    let base_path = temp.path().join("base.img");
    let persistence = temp.path().join("overlay.ozov");
    fs::write(&base_path, sample_base(2)).unwrap();

    let overlay = OverlayConfig::new(SCSI_SECTOR_BYTES, 4096).with_persistence(&persistence);
    let device =
        MassStorageDevice::writable_file(&base_path, overlay, MassStoragePersistPolicy::default())
            .unwrap();

    let mut payload = vec![0x33u8; SCSI_SECTOR_BYTES];
    payload[1] = 0x44;
    let mut transport = MemoryBulkTransport::new();
    transport.push_host_data(&write10(0, 1, &payload)).unwrap();
    transport.push_host_data(&payload).unwrap();

    let mut loop_ = MassStorageLoop::new(device, transport, MassStorageLoopConfig::default())
        .expect("bounds are valid");
    loop_.step().unwrap();
    loop_.transport_mut().take_device_data();
    assert!(loop_.is_running());
    assert!(!persistence.exists());

    loop_.stop().unwrap();
    assert!(!loop_.is_running());
    assert!(
        persistence.is_file(),
        "stop must persist the configured overlay"
    );
}

#[test]
fn owned_file_endpoints_drive_the_loop() {
    let temp = tempfile::tempdir().unwrap();
    let out_path = temp.path().join("ep-out");
    let in_path = temp.path().join("ep-in");
    fs::write(&out_path, read10(0, 1)).unwrap();
    fs::write(&in_path, b"").unwrap();

    let device = MassStorageDevice::read_only_bytes(sample_base(2), 4096).unwrap();
    {
        let transport = FileBulkTransport::open(
            FunctionFsEndpoint::bulk_out(1, 512),
            out_path.clone(),
            FunctionFsEndpoint::bulk_in(2, 512),
            in_path.clone(),
        )
        .unwrap();
        let mut loop_ =
            MassStorageLoop::new(device, transport, MassStorageLoopConfig::default()).unwrap();
        let outcome = loop_.step().unwrap();
        assert_eq!(outcome.status, ScsiStatus::Good);
        assert_eq!(outcome.data_in_bytes, SCSI_SECTOR_BYTES);
        assert!(loop_.is_running());
    }

    let out = fs::read(&in_path).unwrap();
    assert_eq!(out.len(), SCSI_SECTOR_BYTES + SCSI_CSW_BYTES);
    assert_eq!(
        &out[..SCSI_SECTOR_BYTES],
        &sample_base(2)[..SCSI_SECTOR_BYTES]
    );
    assert_eq!(csw_status(&out), 0);
}

#[test]
fn functionfs_loopback_endpoints_drive_the_scsi_loop() {
    let config = FunctionFsConfig::new("vendor0", 0x1209, 0x0001)
        .with_endpoint(FunctionFsEndpoint::bulk_out(1, 512))
        .with_endpoint(FunctionFsEndpoint::bulk_in(2, 512));
    let descriptors = config.ep0_descriptors().unwrap();
    let mut daemon = FakeFunctionFsDaemon::new(config.clone()).unwrap();
    daemon.write_descriptors(&config, &descriptors).unwrap();
    daemon
        .handle_event(&config, FunctionFsEvent::Enable)
        .unwrap();

    daemon.host_write(0x01, &read10(0, 1)).unwrap();

    let device = MassStorageDevice::read_only_bytes(sample_base(2), 4096).unwrap();
    {
        let transport = FunctionFsBulkTransport::new(daemon.core_mut(), 0x01, 0x82).unwrap();
        let mut loop_ =
            MassStorageLoop::new(device, transport, MassStorageLoopConfig::default()).unwrap();
        let outcome = loop_.step().unwrap();
        assert_eq!(outcome.status, ScsiStatus::Good);
        assert_eq!(outcome.data_in_bytes, SCSI_SECTOR_BYTES);
        assert!(loop_.is_running());
    }

    let out = daemon.host_read(0x82).unwrap();
    assert_eq!(out.len(), SCSI_SECTOR_BYTES + SCSI_CSW_BYTES);
    assert_eq!(
        &out[..SCSI_SECTOR_BYTES],
        &sample_base(2)[..SCSI_SECTOR_BYTES]
    );
    assert_eq!(csw_status(&out), 0);
    assert!(daemon.is_running(), "endpoint traffic was observed");
    assert!(daemon.core().stats().bytes_read > 0);
    assert!(daemon.core().stats().bytes_written > 0);
}
