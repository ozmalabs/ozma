// SPDX-License-Identifier: AGPL-3.0-only
//! Writable NBD exports backed by a bounded copy-on-write overlay.
//!
//! These tests use regular files and in-memory duplexes only. They never
//! mount a filesystem, open a block device, or mutate a base image.

use std::io::{self, Cursor, Read, Write};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use ozma_usb::nbd::{
    NBD_FLAG_C_FIXED_NEWSTYLE, NBD_FLAG_C_NO_ZEROES, NBD_NEWSTYLE_MAGIC, NBD_OPT_EXPORT_NAME,
};
use ozma_usb::nbd_server::{
    serve_session, ManualClock, NbdBackend, NbdExportCatalog, NbdExportSource, NbdOverlayBackend,
    NbdOverlayPolicy, NbdServer, NbdServerConfig, NbdTransport,
};
use ozma_usb::{MassStorageImage, NbdRequest, OverlayConfig};

fn base_bytes() -> Vec<u8> {
    (0..4096u32).map(|value| (value % 251) as u8).collect()
}

fn config(block_size: usize, max_overlay_bytes: u64) -> OverlayConfig {
    OverlayConfig::new(block_size, max_overlay_bytes)
}

/// Scripted in-memory duplex: fixed input, captured output.
struct Duplex {
    input: Cursor<Vec<u8>>,
    output: Arc<Mutex<Vec<u8>>>,
}

impl Duplex {
    fn new(input: Vec<u8>) -> (Self, Arc<Mutex<Vec<u8>>>) {
        let output = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                input: Cursor::new(input),
                output: Arc::clone(&output),
            },
            output,
        )
    }
}

impl Read for Duplex {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.input.read(buffer)
    }
}

impl Write for Duplex {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.output.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl NbdTransport for Duplex {}

fn handshake_prefix() -> Vec<u8> {
    (NBD_FLAG_C_FIXED_NEWSTYLE | NBD_FLAG_C_NO_ZEROES)
        .to_be_bytes()
        .to_vec()
}

fn option(option: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&NBD_NEWSTYLE_MAGIC.to_be_bytes());
    bytes.extend_from_slice(&option.to_be_bytes());
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn read_reply(output: &[u8], cursor: &mut usize) -> (u32, u64) {
    let header = &output[*cursor..*cursor + 16];
    assert_eq!(
        u32::from_be_bytes(header[0..4].try_into().unwrap()),
        0x6744_6698
    );
    let error = u32::from_be_bytes(header[4..8].try_into().unwrap());
    let handle = u64::from_be_bytes(header[8..16].try_into().unwrap());
    *cursor += 16;
    (error, handle)
}

fn configured(max_request_bytes: u32) -> NbdServerConfig {
    NbdServerConfig {
        max_request_bytes,
        max_clients: 2,
        handshake_timeout_ms: 10_000,
        idle_timeout_ms: Some(30_000),
        option_max_bytes: 64 * 1024,
    }
}

#[test]
fn overlay_write_read_back_trim_and_base_immutability() {
    let temp = tempfile::tempdir().unwrap();
    let base_path = temp.path().join("base.raw");
    let base = base_bytes();
    std::fs::write(&base_path, &base).unwrap();

    let source = NbdExportSource::overlay_file(
        "ov",
        &base_path,
        config(512, 10_000),
        NbdOverlayPolicy::default(),
    )
    .unwrap();
    assert!(!source.is_read_only());
    assert_eq!(source.export.size_bytes, base.len() as u64);

    // A write is copy-on-write and reads back through the overlay.
    source.backend.write_at(100, b"ozma").unwrap();
    let mut actual = [0u8; 4];
    source.backend.read_at(100, &mut actual).unwrap();
    assert_eq!(&actual, b"ozma");
    // Bytes outside the written range still come from the base.
    let mut before = [0u8; 4];
    source.backend.read_at(96, &mut before).unwrap();
    assert_eq!(before, base[96..100]);
    // TRIM is a successful no-op and does not discard data.
    source.backend.trim(0, 4096).unwrap();
    source.backend.read_at(100, &mut actual).unwrap();
    assert_eq!(&actual, b"ozma");

    // The base file is byte-for-byte unchanged.
    assert_eq!(std::fs::read(&base_path).unwrap(), base);

    let status = source.backend.overlay_status().unwrap();
    assert_eq!(status.base_bytes, base.len() as u64);
    assert_eq!(status.block_size, 512);
    assert_eq!(status.allocated_blocks, 1);
    assert_eq!(status.allocated_bytes, 512);
    assert!(status.dirty);
    assert_eq!(status.persist_on_flush, true);
    assert_eq!(status.persist_on_stop, true);
}

#[test]
fn overlay_bounds_are_typed_and_preflighted() {
    let temp = tempfile::tempdir().unwrap();
    let base_path = temp.path().join("base.raw");
    let base = base_bytes();
    std::fs::write(&base_path, &base).unwrap();

    let source = NbdExportSource::overlay_file(
        "ov",
        &base_path,
        config(512, 512),
        NbdOverlayPolicy::manual(),
    )
    .unwrap();
    source.backend.write_at(0, b"first").unwrap();
    let before = source.backend.overlay_status().unwrap();
    assert_eq!(before.allocated_bytes, 512);

    // A second block would exceed the one-block ceiling. The failed write must
    // not allocate or change anything.
    let error = source.backend.write_at(512, b"second").unwrap_err();
    assert_eq!(error.kind(), "overlay-bounds");
    let after = source.backend.overlay_status().unwrap();
    assert_eq!(after.allocated_bytes, before.allocated_bytes);
    assert_eq!(after.allocated_blocks, before.allocated_blocks);
    let mut untouched = [0u8; 6];
    source.backend.read_at(512, &mut untouched).unwrap();
    assert_eq!(&untouched, &base[512..518]);

    // Writes outside the base are typed too.
    assert_eq!(
        source
            .backend
            .write_at(base.len() as u64, b"x")
            .unwrap_err()
            .kind(),
        "overlay-bounds"
    );

    // Read-only exports keep rejecting writes with the existing typed error.
    let fat32 = ozma_usb::build_fat32_image(vec![ozma_usb::Fat32Node::file(
        "hello.txt",
        b"hello".to_vec(),
    )])
    .unwrap();
    let fat_path = temp.path().join("fat.img");
    std::fs::write(&fat_path, fat32.as_bytes()).unwrap();
    let read_only =
        NbdExportSource::from_media("fat", &MassStorageImage::fat32(&fat_path)).unwrap();
    assert!(read_only.is_read_only());
    assert_eq!(
        read_only.backend.write_at(0, b"x").unwrap_err().kind(),
        "read-only-media"
    );
}

#[test]
fn flush_persists_and_reloads_through_a_new_backend() {
    let temp = tempfile::tempdir().unwrap();
    let base_path = temp.path().join("base.raw");
    let persistence = temp.path().join("overlay.ozov");
    let base = base_bytes();
    std::fs::write(&base_path, &base).unwrap();

    let config = config(512, 4096).with_persistence(&persistence);
    let source = NbdExportSource::overlay_file(
        "ov",
        &base_path,
        config.clone(),
        NbdOverlayPolicy::default(),
    )
    .unwrap();
    source.backend.write_at(0, b"boot").unwrap();
    source.backend.write_at(1024, b"middle").unwrap();
    assert!(
        !persistence.exists(),
        "the overlay is not persisted before flush"
    );
    source.backend.flush().unwrap();
    assert!(persistence.exists(), "flush commits the persistence file");
    assert!(!source.backend.overlay_status().unwrap().dirty);

    // A fresh backend loads the same base and persistence file.
    let reloaded = NbdOverlayBackend::load_from_file(
        &base_path,
        config,
        NbdOverlayPolicy::default(),
        &persistence,
    )
    .unwrap();
    let mut actual = [0u8; 4];
    reloaded.read_at(0, &mut actual).unwrap();
    assert_eq!(&actual, b"boot");
    reloaded.read_at(1024, &mut actual).unwrap();
    assert_eq!(&actual, b"midd");
    assert_eq!(reloaded.status().allocated_bytes, 1024);
    assert_eq!(std::fs::read(&base_path).unwrap(), base);
}

#[test]
fn flush_and_stop_commit_only_when_the_policy_allows() {
    let temp = tempfile::tempdir().unwrap();
    let base_path = temp.path().join("base.raw");
    let base = base_bytes();
    std::fs::write(&base_path, &base).unwrap();

    // Manual policy: neither flush nor the server stop writes the file.
    let manual_path = temp.path().join("manual.ozov");
    let manual = NbdOverlayBackend::from_file(
        &base_path,
        config(512, 4096).with_persistence(&manual_path),
        NbdOverlayPolicy::manual(),
    )
    .unwrap();
    manual.write_at(0, b"data").unwrap();
    manual.flush().unwrap();
    manual.persist().unwrap();
    assert!(!manual_path.exists());

    // Server stop persists when the policy says so.
    let stop_path = temp.path().join("stop.ozov");
    let source = NbdExportSource::overlay_file(
        "ov",
        &base_path,
        config(512, 4096).with_persistence(&stop_path),
        NbdOverlayPolicy::default(),
    )
    .unwrap();
    let catalog = NbdExportCatalog::new(vec![source.clone()]).unwrap();
    let server = NbdServer::bind_tcp(
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
        catalog,
        configured(1024),
    )
    .unwrap();
    let handle = server.start().unwrap();
    source.backend.write_at(0, b"stop").unwrap();
    let stats = handle.stop();
    assert_eq!(stats.persist_failures, 0);
    assert!(stop_path.exists(), "server stop commits the overlay");

    // The base was never touched.
    assert_eq!(std::fs::read(&base_path).unwrap(), base);
}

#[test]
fn server_session_serves_writes_reads_flush_and_bounded_trim() {
    let base = base_bytes();
    let source = NbdExportSource::overlay_bytes(
        "ov",
        base.clone(),
        config(512, 4096),
        NbdOverlayPolicy::manual(),
    )
    .unwrap();
    let catalog = NbdExportCatalog::new(vec![source.clone()]).unwrap();

    let mut script = handshake_prefix();
    script.extend(option(NBD_OPT_EXPORT_NAME, b"ov"));
    script.extend(
        NbdRequest::Write {
            handle: 1,
            offset: 16,
            data: b"overlay-data".to_vec(),
        }
        .encode(),
    );
    script.extend(
        NbdRequest::Read {
            handle: 2,
            offset: 12,
            length: 16,
        }
        .encode(),
    );
    script.extend(NbdRequest::Flush { handle: 3 }.encode());
    script.extend(
        NbdRequest::Trim {
            handle: 4,
            offset: 0,
            length: 32,
        }
        .encode(),
    );
    script.extend(NbdRequest::Disconnect { handle: 5 }.encode());

    let (mut duplex, output) = Duplex::new(script);
    let stop = AtomicBool::new(false);
    let outcome = serve_session(
        &mut duplex,
        &catalog,
        &configured(1024),
        &ManualClock::new(0),
        &stop,
    )
    .unwrap();
    assert_eq!(outcome.write_requests, 1);
    assert_eq!(outcome.flush_requests, 1);
    assert_eq!(outcome.trim_requests, 1);
    assert_eq!(outcome.bytes_written, 12);

    let output = output.lock().unwrap().clone();
    let mut cursor = 18 + 10; // banner + export-name size/flags
    let (error, replied) = read_reply(&output, &mut cursor);
    assert_eq!(error, 0);
    assert_eq!(replied, 1);
    let (error, replied) = read_reply(&output, &mut cursor);
    assert_eq!(error, 0);
    assert_eq!(replied, 2);
    let expected: Vec<u8> = base[12..16]
        .iter()
        .copied()
        .chain(b"overlay-data".iter().copied())
        .collect();
    // The read starts at 12, so the first 4 bytes are base[12..16], then the
    // overlay.
    assert_eq!(&output[cursor..cursor + 16], expected.as_slice());
    cursor += 16;
    for handle in [3, 4] {
        let (error, replied) = read_reply(&output, &mut cursor);
        assert_eq!(error, 0, "handle {handle}");
        assert_eq!(replied, handle);
    }

    // The base is unchanged and the overlay read-back matches.
    let mut actual = [0u8; 12];
    source.backend.read_at(16, &mut actual).unwrap();
    assert_eq!(&actual, b"overlay-data");
    assert_eq!(
        source.backend.overlay_status().unwrap().allocated_bytes,
        512
    );
}

#[test]
fn server_rejects_out_of_range_and_overlay_exhaustion_requests() {
    let base = vec![0u8; 4096];
    let source =
        NbdExportSource::overlay_bytes("ov", base, config(512, 512), NbdOverlayPolicy::manual())
            .unwrap();
    let catalog = NbdExportCatalog::new(vec![source]).unwrap();

    let mut script = handshake_prefix();
    script.extend(option(NBD_OPT_EXPORT_NAME, b"ov"));
    // Outside the export: EINVAL before the backend.
    script.extend(
        NbdRequest::Write {
            handle: 1,
            offset: 4096,
            data: b"x".to_vec(),
        }
        .encode(),
    );
    // Inside the export but over the overlay ceiling: ENOSPC.
    script.extend(
        NbdRequest::Write {
            handle: 2,
            offset: 0,
            data: b"first".to_vec(),
        }
        .encode(),
    );
    script.extend(
        NbdRequest::Write {
            handle: 3,
            offset: 512,
            data: b"second".to_vec(),
        }
        .encode(),
    );
    script.extend(NbdRequest::Disconnect { handle: 4 }.encode());

    let (mut duplex, output) = Duplex::new(script);
    let stop = AtomicBool::new(false);
    serve_session(
        &mut duplex,
        &catalog,
        &configured(1024),
        &ManualClock::new(0),
        &stop,
    )
    .unwrap();

    let output = output.lock().unwrap().clone();
    let mut cursor = 18 + 10;
    for (handle, errno) in [(1, 22), (2, 0), (3, 28)] {
        let (error, replied) = read_reply(&output, &mut cursor);
        assert_eq!(replied, handle);
        assert_eq!(error, errno, "handle {handle}");
    }
}

#[test]
fn overlay_status_is_serializable_and_reports_no_persistence_when_ephemeral() {
    let source = NbdExportSource::overlay_bytes(
        "ov",
        vec![0u8; 1024],
        config(512, 1024),
        NbdOverlayPolicy::default(),
    )
    .unwrap();
    source.backend.write_at(0, b"x").unwrap();
    source.backend.flush().unwrap();
    let status = source.backend.overlay_status().unwrap();
    assert!(status.persistence.is_none());
    assert!(status.dirty, "an ephemeral overlay stays dirty");
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["allocated_bytes"], 512);
    assert!(json.get("persistence").is_none());
    // A read-only export has no overlay status at all.
    let read_only = NbdExportSource::memory("ro", vec![0u8; 512], true).unwrap();
    assert!(read_only.backend.overlay_status().is_none());
    assert_eq!(
        read_only.backend.write_at(0, b"x").unwrap_err().kind(),
        "read-only-media"
    );
}
