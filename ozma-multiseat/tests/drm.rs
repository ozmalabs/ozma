// SPDX-License-Identifier: AGPL-3.0-only

use std::fs;
use std::path::Path;

use ozma_multiseat::drm::detect_at;
use ozma_multiseat::DrmStatus;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, contents).expect("write fixture");
}

#[test]
fn detects_connected_drm_connectors_and_modes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("drm");
    write(&root.join("card0-HDMI-A-1/status"), "connected\n");
    write(&root.join("card0-HDMI-A-1/modes"), "1920x1080\n1280x720\n");
    write(&root.join("card0-DP-1/status"), "disconnected\n");
    write(&root.join("card0-Writeback-1/status"), "writeback\n");

    let detection = detect_at(&root);
    assert!(detection.available);
    assert_eq!(detection.connectors.len(), 3);
    assert_eq!(detection.connected().count(), 1);

    let hdmi = &detection.connectors[0];
    assert_eq!(hdmi.card, "card0");
    assert_eq!(hdmi.connector, "DP-1");
    assert_eq!(hdmi.status, DrmStatus::Disconnected);

    let hdmi = detection
        .connectors
        .iter()
        .find(|connector| connector.connector == "HDMI-A-1")
        .expect("HDMI connector");
    assert_eq!(hdmi.status, DrmStatus::Connected);
    assert_eq!(hdmi.modes, vec!["1920x1080", "1280x720"]);

    let writeback = detection
        .connectors
        .iter()
        .find(|connector| connector.connector == "Writeback-1")
        .expect("writeback connector");
    assert_eq!(writeback.status, DrmStatus::Unknown("writeback".to_owned()));

    let backend = detection.as_backend_detection();
    assert!(backend.available);
    assert_eq!(backend.backend, "drm");
}

#[test]
fn directories_without_status_are_ignored_and_missing_roots_are_unavailable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("drm");
    fs::create_dir_all(root.join("card0-HDMI-A-1")).expect("empty connector");
    fs::create_dir_all(root.join("not-a-connector")).expect("non connector");

    let detection = detect_at(&root);
    assert!(!detection.available);
    assert!(detection.connectors.is_empty());
    assert!(detection.reason.is_some());

    let missing = detect_at(&temp.path().join("missing"));
    assert!(!missing.available);
    assert!(missing.reason.is_some());
}
