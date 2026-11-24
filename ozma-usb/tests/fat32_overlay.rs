// SPDX-License-Identifier: AGPL-3.0-only
//! Bounded FAT32/raw write-overlay tests.
//!
//! These tests use in-memory or regular-file base images only. They never
//! mount a filesystem, open a block device, or write a block device.

use std::fs;

use ozma_usb::fat32::{build_fat32_image, Fat32Node};
use ozma_usb::{OverlayConfig, WriteOverlay};

const HEADER: usize = 32;

fn data_offset(image: &[u8]) -> usize {
    let reserved = u16::from_le_bytes([image[14], image[15]]) as usize;
    let fats = image[16] as usize;
    let fat_sectors = u32::from_le_bytes([image[36], image[37], image[38], image[39]]) as usize;
    (reserved + fats * fat_sectors) * 512
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn rewrite_header_checksum(bytes: &mut [u8]) {
    bytes[24..28].fill(0);
    let checksum = fnv1a(&bytes[..HEADER]);
    bytes[24..28].copy_from_slice(&checksum.to_le_bytes());
}

fn config(block_size: usize, max_overlay_bytes: u64) -> OverlayConfig {
    OverlayConfig::new(block_size, max_overlay_bytes)
}

#[test]
fn writes_reads_and_leaves_the_fat32_base_immutable() {
    let image = build_fat32_image(vec![
        Fat32Node::file("hello.txt", b"hello from the base image".to_vec()),
        Fat32Node::file("other.bin", vec![0x5a; 2048]),
    ])
    .unwrap();
    let base = image.as_bytes().to_vec();
    let original = base.clone();
    let mut overlay = WriteOverlay::from_fat32(&image, config(512, 1024 * 1024)).unwrap();

    let entry = image
        .entries()
        .iter()
        .find(|entry| entry.name == "hello.txt")
        .unwrap();
    let offset = data_offset(&base) as u64 + (entry.first_cluster as u64 - 2) * 512;

    let report = overlay.write_at(offset, b"OVERLAY").unwrap();
    assert_eq!(report.bytes_written, 7);
    assert_eq!(report.blocks_touched, 1);
    assert_eq!(report.newly_allocated_bytes, 512);
    assert_eq!(overlay.read_at(offset, 7).unwrap(), b"OVERLAY");
    assert_eq!(
        overlay.read_at(offset + 7, 9).unwrap(),
        &base[offset as usize + 7..offset as usize + 16]
    );

    // The bytes outside the written range are still base bytes.
    assert_eq!(
        overlay.read_at(0, 512).unwrap(),
        &base[..512],
        "the boot sector is untouched"
    );

    // The overlay is a full materialised image for a consumer that wants one,
    // and re-parsing the untouched structure still succeeds.
    let materialised = overlay.materialize();
    assert_eq!(
        &materialised[offset as usize..offset as usize + 7],
        b"OVERLAY"
    );
    assert_eq!(&materialised[..offset as usize], &base[..offset as usize]);
    image.validate().unwrap();

    // The base object and the captured base bytes are unchanged.
    assert_eq!(image.as_bytes(), original.as_slice());
    assert_eq!(overlay.base_bytes(), original.as_slice());
    assert!(overlay.is_dirty());
}

#[test]
fn allocation_is_bounded_and_copy_on_write_is_per_block() {
    let base = vec![0x11u8; 2048];
    let mut overlay = WriteOverlay::from_bytes(base.clone(), config(512, 512)).unwrap();

    let first = overlay.write_at(0, b"first").unwrap();
    assert_eq!(first.newly_allocated_bytes, 512);
    assert_eq!(overlay.allocated_bytes(), 512);
    assert_eq!(overlay.allocated_blocks(), 1);

    // A second write to the same block does not allocate again.
    let second = overlay.write_at(16, b"second").unwrap();
    assert_eq!(second.newly_allocated_bytes, 0);
    assert_eq!(overlay.allocated_bytes(), 512);
    assert_eq!(overlay.read_at(0, 5).unwrap(), b"first");
    assert_eq!(overlay.read_at(16, 6).unwrap(), b"second");

    // A different block would exceed the single-block ceiling; the failed write
    // must not allocate or mutate anything.
    let error = overlay.write_at(512, b"nope").unwrap_err();
    assert_eq!(error.kind(), "overlay-bounds");
    assert_eq!(overlay.allocated_bytes(), 512);
    assert_eq!(overlay.allocated_blocks(), 1);
    assert_eq!(overlay.read_at(512, 4).unwrap(), &base[512..516]);

    // Out-of-range reads/writes and misaligned sector writes are typed.
    assert_eq!(
        overlay.write_at(2048, b"x").unwrap_err().kind(),
        "overlay-bounds"
    );
    assert_eq!(
        overlay.read_at(2048, 1).unwrap_err().kind(),
        "overlay-bounds"
    );
    assert_eq!(
        overlay.write_sectors(0, b"short").unwrap_err().kind(),
        "invalid-config"
    );

    let mut invalid = config(300, 512);
    invalid.block_size = 300;
    assert_eq!(
        WriteOverlay::from_bytes(base, invalid).unwrap_err().kind(),
        "invalid-config"
    );
}

#[test]
fn persistence_reloads_and_validates_against_its_base() {
    let base: Vec<u8> = (0..4096u32).map(|value| (value % 241) as u8).collect();
    let temp = tempfile::tempdir().unwrap();
    let persistence = temp.path().join("overlay.bin");
    let mut overlay = WriteOverlay::from_bytes(
        base.clone(),
        config(512, 1024 * 1024).with_persistence(&persistence),
    )
    .unwrap();
    overlay.write_at(0, b"boot").unwrap();
    overlay.write_at(4096 - 4, b"tail").unwrap();
    overlay.write_at(1024, b"middle").unwrap();
    let report = overlay.commit().unwrap();
    assert_eq!(report, persistence);
    assert!(!overlay.is_dirty());

    let bytes = fs::read(&persistence).unwrap();
    assert_eq!(&bytes[..4], b"OZOV");
    assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 1);
    assert!(bytes.len() > HEADER);

    let mut reloaded = WriteOverlay::from_bytes(base.clone(), config(512, 1024 * 1024)).unwrap();
    reloaded.load_from(&persistence).unwrap();
    assert_eq!(reloaded.read_at(0, 4).unwrap(), b"boot");
    assert_eq!(reloaded.read_at(1024, 6).unwrap(), b"middle");
    assert_eq!(reloaded.read_at(4096 - 4, 4).unwrap(), b"tail");
    assert_eq!(reloaded.allocated_bytes(), 3 * 512);
    assert!(!reloaded.is_dirty());

    // Loading a persistence file against a different base is a typed corrupt
    // failure, even when the two images have the same length.
    let other_base = vec![0x99u8; 4096];
    let mut mismatched = WriteOverlay::from_bytes(other_base, config(512, 1024 * 1024)).unwrap();
    let error = mismatched.load_from(&persistence).unwrap_err();
    assert_eq!(error.kind(), "overlay-corrupt");
    assert_eq!(mismatched.allocated_bytes(), 0);

    // A file base is read-only and never modified by commits or writes.
    let base_path = temp.path().join("base.img");
    fs::write(&base_path, &base).unwrap();
    let mut file_overlay = WriteOverlay::from_file(
        &base_path,
        config(512, 1024 * 1024).with_persistence(temp.path().join("file-overlay.bin")),
    )
    .unwrap();
    file_overlay.write_at(100, b"file").unwrap();
    file_overlay.commit().unwrap();
    assert_eq!(fs::read(&base_path).unwrap(), base);
    assert_eq!(file_overlay.read_at(100, 4).unwrap(), b"file");
}

#[test]
fn corrupt_persistence_files_are_rejected_with_typed_errors() {
    let base = vec![0x22u8; 4096];
    let temp = tempfile::tempdir().unwrap();
    let valid_path = temp.path().join("valid.bin");
    let mut overlay = WriteOverlay::from_bytes(base.clone(), config(512, 4096)).unwrap();
    overlay.write_at(0, b"one").unwrap();
    overlay.write_at(1024, b"two").unwrap();
    overlay.commit_to(&valid_path).unwrap();
    let valid = fs::read(&valid_path).unwrap();
    let entry_len = 16 + 512;

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("bad magic", {
            let mut bytes = valid.clone();
            bytes[0] = 0;
            bytes
        }),
        ("bad header checksum", {
            let mut bytes = valid.clone();
            bytes[24] ^= 0xff;
            bytes
        }),
        ("bad version", {
            let mut bytes = valid.clone();
            bytes[4] = 9;
            rewrite_header_checksum(&mut bytes);
            bytes
        }),
        ("truncated header", valid[..10].to_vec()),
        ("truncated block", valid[..valid.len() - 1].to_vec()),
        ("trailing bytes", {
            let mut bytes = valid.clone();
            bytes.push(0);
            bytes
        }),
        ("bad block checksum", {
            let mut bytes = valid.clone();
            bytes[HEADER + 16] ^= 1;
            bytes
        }),
        ("block index out of range", {
            let mut bytes = valid.clone();
            bytes[HEADER..HEADER + 8].copy_from_slice(&999u64.to_le_bytes());
            bytes
        }),
        ("duplicate block", {
            let mut bytes = valid.clone();
            let first = bytes[HEADER..HEADER + entry_len].to_vec();
            bytes.extend_from_slice(&first);
            bytes[20..24].copy_from_slice(&3u32.to_le_bytes());
            rewrite_header_checksum(&mut bytes);
            bytes
        }),
        ("wrong base length", {
            let mut bytes = valid.clone();
            bytes[12..20].copy_from_slice(&8192u64.to_le_bytes());
            rewrite_header_checksum(&mut bytes);
            bytes
        }),
        ("wrong base checksum", {
            let mut bytes = valid.clone();
            bytes[28] ^= 0xff;
            rewrite_header_checksum(&mut bytes);
            bytes
        }),
    ];

    for (name, bytes) in cases {
        let path = temp.path().join(format!("{name}.bin"));
        fs::write(&path, &bytes).unwrap();
        let mut candidate = WriteOverlay::from_bytes(base.clone(), config(512, 4096)).unwrap();
        let error = candidate.load_from(&path).unwrap_err();
        assert_eq!(error.kind(), "overlay-corrupt", "case {name}: {error}");
        assert_eq!(
            candidate.allocated_bytes(),
            0,
            "case {name} mutated the overlay"
        );
    }
}

#[test]
fn partial_final_block_allocates_and_persists_its_real_length() {
    let base = vec![0x33u8; 1000];
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("partial.bin");
    let mut overlay = WriteOverlay::from_bytes(base.clone(), config(512, 2048)).unwrap();
    let report = overlay.write_at(900, &[0x77; 100]).unwrap();
    assert_eq!(report.blocks_touched, 1);
    assert_eq!(report.newly_allocated_bytes, 488);
    assert_eq!(overlay.allocated_bytes(), 488);
    overlay.commit_to(&path).unwrap();

    let mut reloaded = WriteOverlay::from_bytes(base, config(512, 2048)).unwrap();
    reloaded.load_from(&path).unwrap();
    assert_eq!(reloaded.allocated_bytes(), 488);
    assert_eq!(reloaded.read_at(900, 100).unwrap(), vec![0x77; 100]);
    assert_eq!(reloaded.read_at(899, 1).unwrap(), vec![0x33]);
}

#[test]
fn raw_base_and_discard_return_to_the_base() {
    let base: Vec<u8> = (0..1024u32).map(|value| value as u8).collect();
    let mut overlay = WriteOverlay::from_bytes(base.clone(), config(512, 1024)).unwrap();
    overlay.write_at(0, &[0xaa; 512]).unwrap();
    assert_eq!(overlay.allocated_bytes(), 512);
    assert_eq!(overlay.read_at(0, 4).unwrap(), vec![0xaa; 4]);

    overlay.discard_all();
    assert_eq!(overlay.allocated_bytes(), 0);
    assert_eq!(overlay.allocated_blocks(), 0);
    assert_eq!(overlay.read_at(0, 4).unwrap(), &base[..4]);
    assert_eq!(overlay.materialize(), base);
}
