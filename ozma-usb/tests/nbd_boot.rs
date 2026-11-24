// SPDX-License-Identifier: AGPL-3.0-only
//! Live NBD ISO boot evidence with a disposable QEMU.
//!
//! The test serves a synthetic El Torito ISO from the in-process NBD server,
//! boots a disposable `qemu-system-x86_64` from that export, captures the
//! guest's serial boot line, and records QMP `query-block` evidence. QEMU does
//! not touch an existing VM and is killed/quit in every path.
//!
//! Run explicitly with:
//!
//! ```bash
//! OZMA_NBD_BOOT_TEST=1 cargo test -p ozma-usb --test nbd_boot -- --ignored
//! ```
//!
//! It skips (without claiming a boot) when QEMU is absent. No test mounts a
//! filesystem, opens a block device, or writes an image.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use ozma_usb::nbd_server::{
    NbdExportCatalog, NbdExportSource, NbdServer, NbdServerConfig, NbdServerHandle,
};

const SECTOR: usize = 2048;
const BOOT_MARKER: &str = "OZMA-NBD-BOOT";

/// Minimal 16-bit boot sector: initialise COM1 and print a marker, then halt.
const BOOT_CODE: &[u8] = &[
    0xfa, 0x31, 0xc0, 0x8e, 0xd8, 0x8e, 0xc0, 0x8e, 0xd0, 0xbc, 0x00, 0x7c, 0xfb, 0xba, 0xf9, 0x03,
    0x30, 0xc0, 0xee, 0xba, 0xfb, 0x03, 0xb0, 0x80, 0xee, 0xba, 0xf8, 0x03, 0xb0, 0x01, 0xee, 0xba,
    0xf9, 0x03, 0x30, 0xc0, 0xee, 0xba, 0xfb, 0x03, 0xb0, 0x03, 0xee, 0xba, 0xfa, 0x03, 0xb0, 0xc7,
    0xee, 0xbe, 0x42, 0x7c, 0xac, 0x84, 0xc0, 0x74, 0x06, 0xba, 0xf8, 0x03, 0xee, 0xeb, 0xf5, 0xf4,
    0xeb, 0xfd, b'O', b'Z', b'M', b'A', b'-', b'N', b'B', b'D', b'-', b'B', b'O', b'O', b'T', 0x0d,
    0x0a, 0x00,
];

fn qemu_system() -> Option<PathBuf> {
    let output = Command::new("qemu-system-x86_64")
        .arg("--version")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from("qemu-system-x86_64"))
}

fn config() -> NbdServerConfig {
    NbdServerConfig {
        max_request_bytes: 1024 * 1024,
        max_clients: 4,
        handshake_timeout_ms: 15_000,
        idle_timeout_ms: Some(60_000),
        option_max_bytes: 64 * 1024,
    }
}

fn start_export(iso: Vec<u8>) -> (NbdServerHandle, String) {
    let export = NbdExportSource::memory("boot-iso", iso, true).unwrap();
    let catalog = NbdExportCatalog::new(vec![export]).unwrap();
    let server = NbdServer::bind_tcp(
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
        catalog,
        config(),
    )
    .unwrap();
    let address = server.local_addr().unwrap();
    let handle = server.start().unwrap();
    (handle, format!("nbd://{address}/boot-iso"))
}

fn put_u16_le(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u16_be(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn put_u32_le(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u32_be(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn root_directory_record(extent_lba: u32, size: u32, name: &[u8], flags: u8) -> Vec<u8> {
    let mut record = vec![0u8; 34];
    record[0] = record.len() as u8;
    record[2..6].copy_from_slice(&extent_lba.to_le_bytes());
    record[6..10].copy_from_slice(&extent_lba.to_be_bytes());
    record[10..14].copy_from_slice(&size.to_le_bytes());
    record[14..18].copy_from_slice(&size.to_be_bytes());
    record[25] = flags;
    put_u16_le(&mut record, 28, 1);
    put_u16_be(&mut record, 30, 1);
    record[32] = name.len() as u8;
    record[33..33 + name.len()].copy_from_slice(name);
    record
}

/// Build a valid ISO 9660 image with an El Torito no-emulation boot catalog.
fn bootable_iso() -> Vec<u8> {
    let total_sectors = 25u32;
    let pvd_lba = 16usize;
    let boot_record_lba = 17usize;
    let terminator_lba = 18usize;
    let catalog_lba = 19usize;
    let boot_image_lba = 20usize;
    let root_lba = 24usize;

    let mut image = vec![0u8; total_sectors as usize * SECTOR];

    // Primary Volume Descriptor.
    {
        let pvd = &mut image[pvd_lba * SECTOR..(pvd_lba + 1) * SECTOR];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        pvd[8..40].fill(b' ');
        pvd[8..12].copy_from_slice(b"OZMA");
        pvd[40..72].fill(b' ');
        pvd[40..53].copy_from_slice(b"OZMA_NBD_BOOT");
        put_u32_le(pvd, 80, total_sectors);
        put_u32_be(pvd, 84, total_sectors);
        put_u16_le(pvd, 120, 1);
        put_u16_be(pvd, 122, 1);
        put_u16_le(pvd, 124, 1);
        put_u16_be(pvd, 126, 1);
        put_u32_le(pvd, 128, SECTOR as u32);
        put_u32_be(pvd, 132, SECTOR as u32);
        put_u32_le(pvd, 140, 0);
        put_u32_be(pvd, 144, 0);
        let root = root_directory_record(root_lba as u32, SECTOR as u32, &[0], 0x02);
        pvd[156..156 + root.len()].copy_from_slice(&root);
    }

    // Boot Record Volume Descriptor.
    {
        let record = &mut image[boot_record_lba * SECTOR..(boot_record_lba + 1) * SECTOR];
        record[0] = 0;
        record[1..6].copy_from_slice(b"CD001");
        record[6] = 1;
        record[7..30].copy_from_slice(b"EL TORITO SPECIFICATION");
        record[39..47].copy_from_slice(b"OZMA    ");
        put_u32_le(record, 71, catalog_lba as u32);
    }

    // Volume Descriptor Set Terminator.
    {
        let terminator = &mut image[terminator_lba * SECTOR..(terminator_lba + 1) * SECTOR];
        terminator[0] = 255;
        terminator[1..6].copy_from_slice(b"CD001");
        terminator[6] = 1;
    }

    // El Torito boot catalog.
    {
        let catalog = &mut image[catalog_lba * SECTOR..(catalog_lba + 1) * SECTOR];
        let validation = &mut catalog[..32];
        validation[0] = 1;
        validation[1] = 0;
        validation[4..28].fill(b' ');
        validation[4..17].copy_from_slice(b"OZMA NBD BOOT");
        validation[30] = 0x55;
        validation[31] = 0xAA;
        let sum = validation
            .chunks_exact(2)
            .map(|word| u16::from_le_bytes([word[0], word[1]]))
            .fold(0u16, |sum, word| sum.wrapping_add(word));
        put_u16_le(validation, 28, 0u16.wrapping_sub(sum));

        let entry = &mut catalog[32..64];
        entry[0] = 0x88;
        entry[1] = 0;
        put_u16_le(entry, 2, 0);
        entry[4] = 0;
        put_u16_le(entry, 6, 4);
        put_u32_le(entry, 8, boot_image_lba as u32);
    }

    // Boot image (one 2048-byte sector, four 512-byte El Torito sectors).
    {
        let boot = &mut image[boot_image_lba * SECTOR..(boot_image_lba + 1) * SECTOR];
        boot[..BOOT_CODE.len()].copy_from_slice(BOOT_CODE);
        boot[510] = 0x55;
        boot[511] = 0xaa;
    }

    // Root directory with `.` and `..`.
    {
        let root = &mut image[root_lba * SECTOR..(root_lba + 1) * SECTOR];
        let dot = root_directory_record(root_lba as u32, SECTOR as u32, &[0], 0x02);
        let dotdot = root_directory_record(root_lba as u32, SECTOR as u32, &[1], 0x02);
        root[..dot.len()].copy_from_slice(&dot);
        root[dot.len()..dot.len() + dotdot.len()].copy_from_slice(&dotdot);
    }

    image
}

#[test]
#[ignore = "opt-in disposable QEMU NBD ISO boot; set OZMA_NBD_BOOT_TEST=1"]
fn qemu_boots_a_read_only_iso_over_nbd() {
    if std::env::var("OZMA_NBD_BOOT_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP: set OZMA_NBD_BOOT_TEST=1 to run the live NBD boot test");
        return;
    }
    let Some(qemu_system) = qemu_system() else {
        eprintln!("SKIP: qemu-system-x86_64 is not installed");
        return;
    };

    let iso = bootable_iso();
    // The ISO is parsed by the same read-only inspector used for media.
    let temp = tempfile::tempdir().unwrap();
    let iso_path = temp.path().join("boot.iso");
    fs::write(&iso_path, &iso).unwrap();
    ozma_usb::IsoImage::inspect(&iso_path).unwrap();

    let (handle, url) = start_export(iso.clone());
    let serial_path = temp.path().join("serial.log");
    let serial = fs::File::create(&serial_path).unwrap();

    let drive = format!("if=none,id=cd0,file={url},format=raw,readonly=on,media=cdrom");
    let serial_arg = format!("file:{}", serial_path.display());
    let mut child = Command::new(&qemu_system)
        .args([
            "-machine",
            "pc,accel=tcg",
            "-m",
            "128",
            "-nodefaults",
            "-display",
            "none",
            "-no-reboot",
            "-boot",
            "d",
            "-drive",
            drive.as_str(),
            "-device",
            "ide-cd,drive=cd0,bootindex=1",
            "-serial",
            serial_arg.as_str(),
            "-qmp",
            "stdio",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn qemu-system-x86_64");
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut guard = QemuGuard {
        child: Some(child),
        stdin: Some(stdin),
    };
    drop(serial);

    let mut reader = BufReader::new(stdout);
    let mut greeting = String::new();
    reader.read_line(&mut greeting).unwrap();
    assert!(greeting.contains("QMP"), "no QMP greeting: {greeting}");
    qmp(
        guard.stdin.as_mut().unwrap(),
        &mut reader,
        1,
        "qmp_capabilities",
    );

    // Poll the guest serial log while QEMU runs; a failed boot must not hang the
    // suite forever.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut booted = false;
    while Instant::now() < deadline {
        if let Ok(log) = fs::read_to_string(&serial_path) {
            if log.contains(BOOT_MARKER) {
                booted = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let boot_log = fs::read_to_string(&serial_path).unwrap_or_default();
    eprintln!("guest serial boot log: {boot_log:?}");

    let block = qmp(guard.stdin.as_mut().unwrap(), &mut reader, 2, "query-block");
    eprintln!("QMP query-block: {block}");
    assert!(
        block.contains("cd0") || block.contains("ide-cd") || block.contains("ide"),
        "CD-ROM device missing from query-block: {block}"
    );
    assert!(
        block.contains("nbd://") || block.contains("nbd:"),
        "NBD backend missing from query-block: {block}"
    );

    // Quit and wait, killing only if QEMU does not honour the QMP request.
    writeln!(
        guard.stdin.as_mut().unwrap(),
        "{{\"execute\":\"quit\",\"id\":3}}"
    )
    .unwrap();
    guard.stdin.as_mut().unwrap().flush().unwrap();
    drop(reader);
    let stats = handle.stop();
    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = guard.child.as_mut().unwrap().try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= exit_deadline {
            let _ = guard.child.as_mut().unwrap().kill();
            panic!("qemu did not exit after QMP quit");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    guard.child = None;

    assert!(status.success(), "qemu exited with {status}");
    assert!(stats.connections_accepted >= 1);
    assert!(
        booted,
        "QEMU did not emit {BOOT_MARKER}; serial log: {boot_log:?}"
    );
}

fn qmp(
    stdin: &mut ChildStdin,
    reader: &mut BufReader<ChildStdout>,
    id: u64,
    execute: &str,
) -> String {
    writeln!(stdin, "{{\"execute\":\"{execute}\",\"id\":{id}}}").unwrap();
    stdin.flush().unwrap();
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).unwrap();
        assert_ne!(read, 0, "QEMU closed QMP before replying to {execute}");
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if value.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
            continue;
        }
        assert!(value.get("error").is_none(), "QMP {execute} failed: {line}");
        return line;
    }
}

struct QemuGuard {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
}

impl Drop for QemuGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
