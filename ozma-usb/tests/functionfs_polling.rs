// SPDX-License-Identifier: AGPL-3.0-only
//! Nonblocking FunctionFS endpoint tests over real OS pipes/FIFOs.
//!
//! These tests create pipes and FIFOs in temporary directories. They do not
//! mount FunctionFS, open a UDC endpoint, or touch a USB device.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use ozma_usb::{
    FunctionFsEndpoint, FunctionFsEndpointIo, MassStorageDevice, MassStorageLoop,
    MassStorageLoopConfig, PollingFileBulkTransport, PollingFileEndpointIo, SCSI_CBW_BYTES,
    SCSI_CSW_BYTES, SCSI_SECTOR_BYTES,
};
use rustix::fs::{mkfifoat, Mode, CWD};
use rustix::pipe::pipe;

fn out_endpoint() -> FunctionFsEndpoint {
    FunctionFsEndpoint::bulk_out(1, 512)
}

fn in_endpoint() -> FunctionFsEndpoint {
    FunctionFsEndpoint::bulk_in(2, 512)
}

fn make_fifo(path: &Path) {
    mkfifoat(CWD, path, Mode::from_bits_truncate(0o600)).unwrap();
}

fn sample_base(sectors: usize) -> Vec<u8> {
    let mut base = vec![0u8; sectors * SCSI_SECTOR_BYTES];
    for (index, byte) in base.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(13);
    }
    base
}

fn test_unit_ready_cbw() -> Vec<u8> {
    let mut bytes = vec![0u8; SCSI_CBW_BYTES];
    bytes[0..4].copy_from_slice(&0x4342_5355u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    bytes[8..12].copy_from_slice(&0u32.to_le_bytes());
    bytes[12] = 0x00;
    bytes[13] = 0;
    bytes[14] = 6;
    bytes
}

#[test]
fn zero_length_read_is_immediate_even_on_an_empty_pipe() {
    let (reader, _writer) = pipe().unwrap();
    let mut endpoint =
        PollingFileEndpointIo::from_file(out_endpoint(), "<pipe-out>", File::from(reader), 1_000)
            .unwrap();

    let started = Instant::now();
    assert_eq!(endpoint.read(&mut []).unwrap(), 0);
    assert!(started.elapsed() < Duration::from_millis(100));
}

#[test]
fn empty_pipe_read_times_out_with_the_endpoint_error() {
    let (reader, _writer) = pipe().unwrap();
    let mut endpoint =
        PollingFileEndpointIo::from_file(out_endpoint(), "<pipe-out>", File::from(reader), 40)
            .unwrap();

    let started = Instant::now();
    let error = endpoint.read_timeout(&mut [0u8; 8], 40).unwrap_err();
    assert_eq!(error.kind(), "mass-storage-endpoint-timeout");
    assert!(started.elapsed() >= Duration::from_millis(20));
    assert!(started.elapsed() < Duration::from_millis(1_000));
}

#[test]
fn pipe_data_is_returned_after_readiness() {
    let (reader, writer) = pipe().unwrap();
    let mut writer = File::from(writer);
    writer.write_all(b"ping").unwrap();
    writer.flush().unwrap();

    let mut endpoint =
        PollingFileEndpointIo::from_file(out_endpoint(), "<pipe-out>", File::from(reader), 1_000)
            .unwrap();
    let mut buffer = [0u8; 8];
    let count = endpoint.read(&mut buffer).unwrap();
    assert_eq!(&buffer[..count], b"ping");
}

#[test]
fn closed_writer_returns_eof_without_inventing_bytes() {
    let (reader, writer) = pipe().unwrap();
    drop(writer);
    let mut endpoint =
        PollingFileEndpointIo::from_file(out_endpoint(), "<pipe-out>", File::from(reader), 1_000)
            .unwrap();

    let mut buffer = [0u8; 4];
    assert_eq!(endpoint.read(&mut buffer).unwrap(), 0);
}

#[test]
fn partial_write_keeps_the_accepted_prefix_before_timeout() {
    let (reader, writer) = pipe().unwrap();
    let mut endpoint =
        PollingFileEndpointIo::from_file(in_endpoint(), "<pipe-in>", File::from(writer), 40)
            .unwrap();

    let payload = vec![0x5au8; 64 * 1024];
    let started = Instant::now();
    let mut timed_out = false;
    for _ in 0..4 {
        match endpoint.write_timeout(&payload, 40) {
            Ok(_) => continue,
            Err(error) => {
                assert_eq!(error.kind(), "mass-storage-endpoint-timeout");
                timed_out = true;
                break;
            }
        }
    }
    assert!(timed_out, "a full pipe must eventually hit the deadline");
    assert!(started.elapsed() >= Duration::from_millis(20));
    drop(endpoint);

    // The kernel accepted a prefix before the pipe filled; those bytes must
    // still be readable and must not be silently discarded.
    let mut reader = File::from(reader);
    let mut prefix = [0u8; 1];
    reader.read_exact(&mut prefix).unwrap();
    assert_eq!(prefix, [0x5a]);
}

#[test]
fn invalid_endpoint_deadlines_are_rejected() {
    let (reader, _writer) = pipe().unwrap();
    let error =
        PollingFileEndpointIo::from_file(out_endpoint(), "<pipe-out>", File::from(reader), 0)
            .unwrap_err();
    assert_eq!(error.kind(), "invalid-config");
}

#[test]
fn mass_storage_loop_classifies_a_stalled_fifo_as_a_timeout_then_recovers() {
    let temp = tempfile::tempdir().unwrap();
    let out_path = temp.path().join("ep-out.fifo");
    let in_path = temp.path().join("ep-in.fifo");
    make_fifo(&out_path);
    make_fifo(&in_path);

    let transport = PollingFileBulkTransport::open(
        out_endpoint(),
        out_path.clone(),
        in_endpoint(),
        in_path.clone(),
        40,
    )
    .unwrap();
    let device = MassStorageDevice::read_only_bytes(sample_base(2), 4096).unwrap();
    let config = MassStorageLoopConfig {
        transfer_timeout_ms: 40,
        ..MassStorageLoopConfig::default()
    };
    let mut loop_ = MassStorageLoop::new(device, transport, config).unwrap();

    let error = loop_.step().unwrap_err();
    assert_eq!(error.kind(), "mass-storage-endpoint-timeout");
    assert_eq!(loop_.stats().timeout_errors, 1);

    // Feeding the CBW through the FIFO lets the same loop make progress after
    // the deadline; the timeout is not a poisoned state.
    let mut writer = OpenOptions::new().write(true).open(&out_path).unwrap();
    writer.write_all(&test_unit_ready_cbw()).unwrap();
    writer.flush().unwrap();
    let outcome = loop_.step().unwrap();
    assert_eq!(outcome.data_in_bytes, 0);
    assert!(loop_.is_running());

    let mut reader = File::open(&in_path).unwrap();
    let mut csw = vec![0u8; SCSI_CSW_BYTES];
    reader.read_exact(&mut csw).unwrap();
    assert_eq!(&csw[0..4], &0x5342_5355u32.to_le_bytes());
    assert_eq!(csw[12], 0);
}

#[test]
#[ignore = "requires OZMA_FUNCTIONFS_MOUNT with real ep0/epN endpoint files"]
fn real_functionfs_endpoints_are_environment_gated() {
    let Ok(mount) = std::env::var("OZMA_FUNCTIONFS_MOUNT") else {
        eprintln!("set OZMA_FUNCTIONFS_MOUNT to run the real FunctionFS test");
        return;
    };
    let mount = std::path::PathBuf::from(mount);
    let mut endpoint =
        PollingFileEndpointIo::open(in_endpoint(), mount.join("ep2"), 1_000).unwrap();
    let _ = endpoint.write_timeout(&[0u8; 4], 1_000).unwrap();
}
