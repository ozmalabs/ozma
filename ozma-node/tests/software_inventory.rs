// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! End-to-end inventory collection across Rust source adapters.
//!
//! These tests use synthetic Steam metadata and an injected Microsoft Store
//! runner, so they never require Steam, Windows, or a real Store installation.

use std::fs;
use std::path::{Path, PathBuf};

use ozma_node::software::{collect_inventory_with, SoftwareScope, UpdateState};

fn temp_dir(label: &str) -> PathBuf {
    let unique = format!(
        "ozma-software-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let path = std::env::temp_dir().join(unique);
    fs::create_dir_all(&path).unwrap();
    path
}

fn write_steam_library(root: &Path, appid: &str, name: &str, buildid: &str) {
    let steamapps = root.join("steamapps");
    fs::create_dir_all(steamapps.join("common").join(name)).unwrap();
    fs::write(
        steamapps.join("libraryfolders.vdf"),
        format!(
            "\"libraryfolders\"\n{{\n    \"0\"\n    {{\n        \"path\" \"{}\"\n    }}\n}}\n",
            root.display()
        ),
    )
    .unwrap();
    fs::write(
        steamapps.join(format!("appmanifest_{appid}.acf")),
        format!(
            "\"AppState\"\n{{\n    \"appid\" \"{appid}\"\n    \"name\" \"{name}\"\n    \
             \"installdir\" \"{name}\"\n    \"buildid\" \"{buildid}\"\n    \
             \"StateFlags\" \"4\"\n    \"SizeOnDisk\" \"1024\"\n}}\n"
        ),
    )
    .unwrap();
}

#[test]
fn collects_steam_and_store_records_together() {
    let root = temp_dir("combined");
    write_steam_library(&root, "70", "Half-Life", "1234");

    let store = |_script: &str| {
        Ok(r#"{
            "packages": [{
                "Name": "Store App",
                "PackageFamilyName": "Store.App_abc",
                "Version": "1.2.3",
                "Architecture": "X64",
                "UserSid": "S-1-5-21"
            }],
            "provisioned": [{
                "Name": "Provisioned App",
                "PackageFamilyName": "Provisioned.App_xyz",
                "Version": "2.0.0"
            }]
        }"#
        .to_string())
    };

    let records = collect_inventory_with(Some(std::slice::from_ref(&root)), "1000", Some(&store));

    let steam = records
        .iter()
        .find(|record| record.source == "steam" && record.source_id == "70")
        .expect("steam record");
    assert_eq!(steam.name, "Half-Life");
    assert_eq!(steam.version, "1234");
    assert_eq!(steam.scope, SoftwareScope::User);
    assert_eq!(steam.update_state, UpdateState::ExternalManaged);
    assert!(steam.install_path.ends_with("Half-Life"));

    let store_app = records
        .iter()
        .find(|record| record.source_id == "Store.App_abc")
        .expect("store record");
    assert_eq!(store_app.scope, SoftwareScope::User);
    assert_eq!(store_app.user_id, "S-1-5-21");
    assert_eq!(store_app.architecture, "x64");

    let provisioned = records
        .iter()
        .find(|record| record.source_id == "Provisioned.App_xyz")
        .expect("provisioned record");
    assert_eq!(provisioned.scope, SoftwareScope::Provisioned);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn malformed_steam_metadata_does_not_block_store_inventory() {
    let root = temp_dir("malformed");
    fs::create_dir_all(root.join("steamapps")).unwrap();
    fs::write(
        root.join("steamapps").join("libraryfolders.vdf"),
        "\"libraryfolders\" { \"0\" { \"path\" ",
    )
    .unwrap();

    let store = |_script: &str| {
        Ok(r#"{"packages":[{"Name":"Store App","PackageFamilyName":"Store.App_abc","Version":"1.0"}]}"#
            .to_string())
    };

    let records = collect_inventory_with(Some(std::slice::from_ref(&root)), "", Some(&store));

    assert!(records
        .iter()
        .any(|record| record.source == "microsoft_store"));
    assert!(records.iter().all(|record| record.source != "steam"));

    let _ = fs::remove_dir_all(root);
}
