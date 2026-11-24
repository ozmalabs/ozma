// SPDX-License-Identifier: AGPL-3.0-only
//! FAT32 image construction and read-only parsing.
//!
//! These tests never mount the image, write a block device, or run `mkfs`.

use std::fs;

use ozma_usb::fat32::{
    build_fat32_image, fat32_image_from_directory, parse_fat32_image, Fat32ImageBuilder, Fat32Node,
    Fat32Options,
};

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn fat_entry(bytes: &[u8], cluster: u32) -> u32 {
    let reserved = read_u16(bytes, 14) as usize;
    let fat_offset = reserved * read_u16(bytes, 11) as usize;
    let offset = fat_offset + cluster as usize * 4;
    read_u32(bytes, offset) & 0x0FFF_FFFF
}

fn data_offset(bytes: &[u8]) -> usize {
    let reserved = read_u16(bytes, 14) as usize;
    let fats = bytes[16] as usize;
    let fat_sectors = read_u32(bytes, 36) as usize;
    (reserved + fats * fat_sectors) * read_u16(bytes, 11) as usize
}

fn fat_string(bytes: &[u8], offset: usize, length: usize) -> String {
    String::from_utf8_lossy(&bytes[offset..offset + length])
        .trim_end_matches([' ', '\0'])
        .to_owned()
}

#[test]
fn builds_a_valid_fat32_image_with_files_and_directories() {
    let image = build_fat32_image(vec![
        Fat32Node::file("hello.txt", b"hello from ozma".to_vec()),
        Fat32Node::file("empty.bin", Vec::new()),
        Fat32Node::directory(
            "docs",
            vec![Fat32Node::file("readme.md", b"# readme\n".to_vec())],
        ),
    ])
    .unwrap();

    // Boot sector / BPB.
    let bytes = image.as_bytes();
    assert_eq!(read_u16(bytes, 11), 512);
    assert_eq!(bytes[13], 1);
    assert_eq!(read_u16(bytes, 14), 32);
    assert_eq!(bytes[16], 2);
    assert_eq!(read_u16(bytes, 17), 0);
    assert_eq!(read_u16(bytes, 19), 0);
    assert_eq!(read_u16(bytes, 22), 0);
    assert_eq!(read_u32(bytes, 36), image.layout().fat_sectors);
    assert_eq!(read_u32(bytes, 44), 2);
    assert_eq!(&bytes[82..90], b"FAT32   ");
    assert_eq!(&bytes[510..512], &[0x55, 0xAA]);
    assert_eq!(&bytes[6 * 512..6 * 512 + 512], &bytes[..512]);
    assert_eq!(fat_string(bytes, 71, 11), "OZMA");

    // FAT32 geometry and chains.
    assert!(image.layout().total_clusters >= 65_525);
    assert_eq!(fat_entry(bytes, 0) & 0x0FFF_FFF8, 0x0FFF_FFF8);
    assert_eq!(fat_entry(bytes, 1), 0x0FFF_FFFF);
    assert_eq!(fat_entry(bytes, 2), 0x0FFF_FFFF);

    let info = image.info();
    assert_eq!(info.label, "OZMA");
    assert_eq!(info.bytes_per_sector, 512);
    assert_eq!(info.cluster_size, 512);
    assert_eq!(info.root_cluster, 2);

    let hello = image
        .entries()
        .iter()
        .find(|entry| entry.name == "hello.txt")
        .expect("hello.txt");
    assert!(!hello.is_dir);
    assert_eq!(hello.size_bytes, 15);
    assert!(hello.first_cluster >= 3);
    assert_eq!(hello.clusters, vec![hello.first_cluster]);
    assert_eq!(fat_entry(bytes, hello.first_cluster), 0x0FFF_FFFF);

    let start = data_offset(bytes) + (hello.first_cluster as usize - 2) * 512;
    assert_eq!(&bytes[start..start + 15], b"hello from ozma");

    let empty = image
        .entries()
        .iter()
        .find(|entry| entry.name == "empty.bin")
        .expect("empty.bin");
    assert_eq!(empty.size_bytes, 0);
    assert_eq!(empty.first_cluster, 0);
    assert!(empty.clusters.is_empty());

    let readme = image
        .entries()
        .iter()
        .find(|entry| entry.name == "docs/readme.md")
        .expect("docs/readme.md");
    let start = data_offset(bytes) + (readme.first_cluster as usize - 2) * 512;
    assert_eq!(&bytes[start..start + 9], b"# readme\n");

    // Root directory contains a volume label and at least one LFN entry for
    // the lowercase names. Files are marked read-only.
    let root = &bytes[data_offset(bytes)..data_offset(bytes) + 512];
    assert!(root[11] == 0x08 || root[11] == 0x0F);
    assert!(root.chunks_exact(32).any(|entry| entry[11] == 0x0F));
    assert!(root
        .chunks_exact(32)
        .any(|entry| entry[11] & 0x01 != 0 && entry[11] & 0x10 == 0));

    // Independent parser agrees with the builder.
    let parsed = parse_fat32_image(bytes).unwrap();
    assert_eq!(parsed, *info);
    assert!(image.is_read_only());
}

#[test]
fn builds_from_a_host_directory_without_following_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("visible.txt"), b"visible").unwrap();
    fs::write(temp.path().join(".hidden"), b"hidden").unwrap();
    fs::create_dir(temp.path().join("nested")).unwrap();
    fs::write(temp.path().join("nested/inner.txt"), b"inner").unwrap();
    fs::write(temp.path().join("outside.txt"), b"outside").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            temp.path().join("outside.txt"),
            temp.path().join("nested/link.txt"),
        )
        .unwrap();
    }

    let image = fat32_image_from_directory(temp.path()).unwrap();
    let names = image
        .entries()
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"visible.txt"));
    assert!(names.contains(&"nested/inner.txt"));
    assert!(!names.contains(&".hidden"));
    assert!(!names.contains(&"nested/link.txt"));
}

#[test]
fn builder_enforces_size_file_count_and_name_bounds() {
    let mut options = Fat32Options::with_label("TEST");
    options.max_file_bytes = 4;
    let mut builder = Fat32ImageBuilder::new("TEST").with_options(options);
    assert_eq!(
        builder
            .add_file("too-big.bin", vec![0u8; 5])
            .unwrap_err()
            .kind(),
        "invalid-media"
    );

    let mut options = Fat32Options::with_label("TEST");
    options.max_files = 1;
    let mut builder = Fat32ImageBuilder::new("TEST").with_options(options);
    builder.add_file("one.txt", b"1".to_vec()).unwrap();
    assert_eq!(
        builder
            .add_file("two.txt", b"2".to_vec())
            .unwrap_err()
            .kind(),
        "invalid-media"
    );

    let mut builder = Fat32ImageBuilder::new("TEST");
    assert_eq!(
        builder
            .add_file("../escape", b"x".to_vec())
            .unwrap_err()
            .kind(),
        "invalid-media"
    );
    assert_eq!(
        builder
            .add_file("bad\0name", b"x".to_vec())
            .unwrap_err()
            .kind(),
        "invalid-media"
    );

    let mut options = Fat32Options::with_label("");
    options.label.clear();
    let mut builder = Fat32ImageBuilder::new("").with_options(options);
    assert_eq!(
        builder.add_file("x.txt", b"x".to_vec()).unwrap_err().kind(),
        "invalid-media"
    );
}

#[test]
fn long_names_and_short_name_collisions_stay_unique() {
    let image = build_fat32_image(vec![
        Fat32Node::file("averylongfilename1.txt", b"one".to_vec()),
        Fat32Node::file("averylongfilename2.txt", b"two".to_vec()),
    ])
    .unwrap();

    let names = image
        .entries()
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"averylongfilename1.txt"));
    assert!(names.contains(&"averylongfilename2.txt"));

    let bytes = image.as_bytes();
    let root = &bytes[data_offset(bytes)..data_offset(bytes) + 512];
    let mut short_names = Vec::new();
    for entry in root.chunks_exact(32) {
        if entry[0] == 0 {
            break;
        }
        if entry[11] != 0x0F && entry[11] & 0x08 == 0 {
            short_names.push(entry[..11].to_vec());
        }
    }
    assert_eq!(short_names.len(), 2);
    assert_ne!(short_names[0], short_names[1]);
}

#[test]
fn parser_rejects_corrupt_boot_sectors() {
    let image = build_fat32_image(vec![Fat32Node::file("x.txt", b"x".to_vec())]).unwrap();
    let mut bytes = image.as_bytes().to_vec();

    bytes[510] = 0;
    bytes[511] = 0;
    assert_eq!(
        parse_fat32_image(&bytes).unwrap_err().kind(),
        "invalid-media"
    );

    let mut bytes = image.as_bytes().to_vec();
    bytes[82..90].copy_from_slice(b"FAT16   ");
    assert_eq!(
        parse_fat32_image(&bytes).unwrap_err().kind(),
        "invalid-media"
    );
}
