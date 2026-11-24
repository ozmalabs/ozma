// SPDX-License-Identifier: AGPL-3.0-only
//! Live NBD validation with QEMU's own client.
//!
//! These tests build real FAT32/ISO bytes, serve them through the in-process
//! NBD server, and let `qemu-img`/`qemu-io` negotiate. They skip (without
//! fabricating a result) when QEMU is not installed. The optional QMP test
//! attaches the export to a disposable `qemu-system-x86_64` USB-storage
//! device and is gated by `OZMA_NBD_QMP_TEST=1`.
//!
//! No test mounts a filesystem, opens a block device, or writes one.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ozma_usb::nbd_server::{
    NbdBackend, NbdExportCatalog, NbdExportSource, NbdMemoryImage, NbdOverlayBackend,
    NbdOverlayPolicy, NbdServer, NbdServerConfig, NbdServerHandle,
};
use ozma_usb::{fat32_image_from_directory, IsoImage, MassStorageImage, OverlayConfig};

const ISO_SECTOR: usize = 2048;

fn qemu_img() -> Option<PathBuf> {
    let output = Command::new("qemu-img").arg("--version").output().ok()?;
    output.status.success().then(|| PathBuf::from("qemu-img"))
}

fn qemu_io() -> Option<PathBuf> {
    let output = Command::new("qemu-io").arg("--version").output().ok()?;
    output.status.success().then(|| PathBuf::from("qemu-io"))
}

fn config() -> NbdServerConfig {
    NbdServerConfig {
        max_request_bytes: 1024 * 1024,
        max_clients: 4,
        handshake_timeout_ms: 15_000,
        idle_timeout_ms: Some(30_000),
        option_max_bytes: 64 * 1024,
    }
}

fn start(export: NbdExportSource) -> (NbdServerHandle, String) {
    let name = export.export.name.clone();
    let catalog = NbdExportCatalog::new(vec![export]).unwrap();
    let server = NbdServer::bind_tcp(
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
        catalog,
        config(),
    )
    .unwrap();
    let address = server.local_addr().unwrap();
    let handle = server.start().unwrap();
    (handle, format!("nbd://{address}/{name}"))
}

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
fn qemu_img_reads_a_generated_fat32_export() {
    let Some(qemu_img) = qemu_img() else {
        eprintln!("SKIP: qemu-img is not installed");
        return;
    };
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("hello.txt"), b"hello from ozma\n").unwrap();
    fs::create_dir(source.path().join("nested")).unwrap();
    fs::write(source.path().join("nested/data.bin"), vec![0x42u8; 4096]).unwrap();
    let image = fat32_image_from_directory(source.path()).unwrap();
    let expected_size = image.len() as u64;
    let export = NbdExportSource::fat32("fat32", &image).unwrap();
    let (handle, url) = start(export);

    let output = Command::new(qemu_img)
        .args(["info", "--output=json", &url])
        .output()
        .expect("run qemu-img info");
    let stats = handle.stop();
    assert!(
        output.status.success(),
        "qemu-img failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    eprintln!("qemu-img FAT32 NBD info: {info}");
    assert_eq!(info["virtual-size"].as_u64(), Some(expected_size));
    assert_eq!(info["format"].as_str(), Some("raw"));
    // The child NBD node reports its own format/size; the top-level raw device
    // is what a USB mass-storage LUN would expose.
    assert_eq!(
        info["children"][0]["info"]["virtual-size"].as_u64(),
        Some(expected_size)
    );
    assert!(stats.connections_accepted >= 1);
}

#[test]
fn qemu_img_reads_a_read_only_iso_export() {
    let Some(qemu_img) = qemu_img() else {
        eprintln!("SKIP: qemu-img is not installed");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let iso_path = temp.path().join("installer.iso");
    let bytes = minimal_iso();
    fs::write(&iso_path, &bytes).unwrap();
    let inspected = IsoImage::inspect(&iso_path).unwrap();
    let media: MassStorageImage = inspected.media_image();
    let export = NbdExportSource::from_media("iso", &media).unwrap();
    assert!(export.is_read_only());
    let (handle, url) = start(export);

    let output = Command::new(qemu_img)
        .args(["info", "--output=json", &url])
        .output()
        .expect("run qemu-img info");
    let stats = handle.stop();
    assert!(
        output.status.success(),
        "qemu-img failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    eprintln!("qemu-img ISO NBD info: {info}");
    assert_eq!(info["virtual-size"].as_u64(), Some(bytes.len() as u64));
    assert!(stats.connections_accepted >= 1);
}

#[test]
fn qemu_io_can_read_but_not_write_the_read_only_export() {
    let Some(qemu_io) = qemu_io() else {
        eprintln!("SKIP: qemu-io is not installed");
        return;
    };
    let data: Vec<u8> = (0..8192u32).map(|value| (value % 251) as u8).collect();
    let backend = Arc::new(NbdMemoryImage::new(data.clone(), true).unwrap());
    let export = NbdExportSource::from_backend("probe", backend.clone(), true).unwrap();
    let (handle, url) = start(export);

    let read = Command::new(&qemu_io)
        .args(["-r", "-f", "raw", "-c", "read -v 0 64", &url])
        .output()
        .expect("run qemu-io read");
    assert!(
        read.status.success(),
        "qemu-io read failed: {}",
        String::from_utf8_lossy(&read.stderr)
    );
    let text = String::from_utf8_lossy(&read.stdout);
    eprintln!("qemu-io read output:\n{text}");
    assert!(text.contains("00000000"));

    let write = Command::new(&qemu_io)
        .args(["-f", "raw", "-c", "write -P 0x11 0 64", &url])
        .output()
        .expect("run qemu-io write");
    let stats = handle.stop();
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&write.stdout),
        String::from_utf8_lossy(&write.stderr)
    );
    eprintln!("qemu-io write output:\n{combined}");
    assert!(
        !write.status.success(),
        "qemu-io unexpectedly wrote a read-only NBD export: {combined}"
    );
    assert!(
        combined.contains("Read-only")
            || combined.contains("read-only")
            || combined.contains("Permission denied")
            || combined.contains("not permitted"),
        "unexpected qemu-io write failure: {combined}"
    );
    // QEMU may refuse to open the export for writing locally after seeing
    // NBD_FLAG_READ_ONLY; either way no write reached the read-only backend.
    assert!(stats.connections_accepted >= 1);
    assert_eq!(backend.snapshot(), data);
}

#[test]
fn qemu_io_writes_and_reads_a_writable_overlay_export() {
    let Some(qemu_io) = qemu_io() else {
        eprintln!("SKIP: qemu-io is not installed");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let base_path = temp.path().join("base.raw");
    let persistence = temp.path().join("overlay.ozov");
    let base: Vec<u8> = (0..1_048_576u32).map(|value| (value % 251) as u8).collect();
    fs::write(&base_path, &base).unwrap();
    let export = NbdExportSource::overlay_file(
        "writable",
        &base_path,
        OverlayConfig::new(4096, 16 * 1024 * 1024).with_persistence(&persistence),
        NbdOverlayPolicy::default(),
    )
    .unwrap();
    let source = export.clone();
    let (handle, url) = start(export);

    let output = Command::new(&qemu_io)
        .args([
            "-f",
            "raw",
            "-c",
            "write -P 0x5a 0 4096",
            "-c",
            "flush",
            "-c",
            "read -P 0x5a 0 4096",
            &url,
        ])
        .output()
        .expect("run qemu-io writable overlay");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("qemu-io writable overlay output:\n{text}");
    assert!(
        output.status.success(),
        "qemu-io writable overlay failed: {text}"
    );

    // QEMU closed the connection after FLUSH; the live overlay already holds
    // the write and the immutable base file is unchanged.
    let mut actual = vec![0u8; 4096];
    source.backend.read_at(0, &mut actual).unwrap();
    assert_eq!(actual, vec![0x5au8; 4096]);
    assert_eq!(fs::read(&base_path).unwrap(), base);
    assert!(persistence.exists(), "qemu-io FLUSH persisted the overlay");

    let stats = handle.stop();
    assert!(stats.connections_accepted >= 1);
    assert!(stats.write_requests >= 1);
    assert!(stats.flush_requests >= 1);
    assert_eq!(stats.persist_failures, 0);

    // A fresh backend reloads the QEMU-written block from persistence.
    let reloaded = NbdOverlayBackend::load_from_file(
        &base_path,
        OverlayConfig::new(4096, 16 * 1024 * 1024).with_persistence(&persistence),
        NbdOverlayPolicy::default(),
        &persistence,
    )
    .unwrap();
    let mut restored = vec![0u8; 4096];
    reloaded.read_at(0, &mut restored).unwrap();
    assert_eq!(restored, vec![0x5au8; 4096]);
}

/// Optional disposable-QEMU USB attachment validated over QMP.
///
/// Run with:
/// `OZMA_NBD_QMP_TEST=1 cargo test -p ozma-usb --test nbd_qemu -- --ignored`
#[test]
#[ignore = "opt-in disposable qemu-system + QMP USB validation"]
fn qmp_reports_an_attached_usb_nbd_disk() {
    if std::env::var("OZMA_NBD_QMP_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP: set OZMA_NBD_QMP_TEST=1 to run the disposable-QEMU test");
        return;
    }
    let Some(qemu_system) = which("qemu-system-x86_64") else {
        eprintln!("SKIP: qemu-system-x86_64 is not installed");
        return;
    };
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("hello.txt"), b"qmp nbd\n").unwrap();
    let image = fat32_image_from_directory(source.path()).unwrap();
    let export = NbdExportSource::fat32("fat32", &image).unwrap();
    let (handle, url) = start(export);

    let drive = format!("if=none,id=nbd0,file={url},format=raw,readonly=on");
    let mut child = Command::new(qemu_system)
        .args([
            "-machine",
            "q35,accel=tcg",
            "-m",
            "64",
            "-nodefaults",
            "-display",
            "none",
            "-S",
            "-drive",
            &drive,
            "-device",
            "qemu-xhci,id=xhci",
            "-device",
            "usb-storage,drive=nbd0,bus=xhci.0,id=usbdisk",
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

    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("QMP"), "no QMP greeting: {line}");

    qmp(
        guard.stdin.as_mut().unwrap(),
        &mut reader,
        1,
        "qmp_capabilities",
    );
    let blocks = qmp(guard.stdin.as_mut().unwrap(), &mut reader, 2, "query-block");
    eprintln!("QMP query-block: {blocks}");
    assert!(
        blocks.contains("usbdisk") || blocks.contains("usb-storage"),
        "USB storage device missing from query-block: {blocks}"
    );
    assert!(
        blocks.contains("nbd://") || blocks.contains("nbd:"),
        "NBD backend missing from query-block: {blocks}"
    );
    writeln!(
        guard.stdin.as_mut().unwrap(),
        "{{\"execute\":\"quit\",\"id\":3}}"
    )
    .unwrap();
    guard.stdin.as_mut().unwrap().flush().unwrap();
    drop(reader);
    let stats = handle.stop();
    assert!(stats.connections_accepted >= 1);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = guard.child.as_mut().unwrap().try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = guard.child.as_mut().unwrap().kill();
            panic!("qemu did not exit after QMP quit");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "qemu exited with {status}");
    guard.child = None;
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
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
