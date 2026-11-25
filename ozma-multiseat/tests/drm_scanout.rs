// SPDX-License-Identifier: AGPL-3.0-only
//! Fake-backed KMS scanout proof tests.
//!
//! Nothing here opens `/dev/dri`, allocates a real dumb buffer, or programs a
//! CRTC. The fake records lease/buffer/mode/flip/teardown events in memory.

use std::path::PathBuf;

use ozma_multiseat::{
    ensure_scanout_card, fill_pattern, pattern_checksum, DrmCard, DrmCardSelector, DrmCrtcSelector,
    DrmLeaseRequest, DrmScanoutBackend, DrmScanoutCommit, DrmScanoutError, DrmScanoutNoteKind,
    DrmScanoutRequest, DrmScanoutStatus, FakeDrmScanoutBackend, FakeDrmScanoutFailure,
    SCANOUT_PATTERN,
};

fn request() -> DrmScanoutRequest {
    DrmScanoutRequest::new(DrmLeaseRequest::new(
        DrmCardSelector::AutoVirtual,
        "Virtual-1",
    ))
}

fn explicit_request(path: &str, connector: &str) -> DrmScanoutRequest {
    DrmScanoutRequest::new(DrmLeaseRequest::new(
        DrmCardSelector::Explicit(PathBuf::from(path)),
        connector,
    ))
}

fn real_card() -> DrmCard {
    DrmCard {
        name: "card0".to_owned(),
        path: PathBuf::from("/dev/dri/card0"),
        sysfs_path: PathBuf::from("/sys/class/drm/card0"),
        driver: "xe".to_owned(),
        virtual_card: false,
        connectors: Vec::new(),
        crtcs: vec![20],
    }
}

#[test]
fn requests_parse_the_lease_selector_and_optional_mode() {
    let parsed: DrmScanoutRequest = "/dev/dri/card4:Virtual-3@41"
        .parse()
        .expect("scanout request");
    assert_eq!(
        parsed.lease.card,
        DrmCardSelector::Explicit("/dev/dri/card4".into())
    );
    assert_eq!(parsed.lease.connector, "Virtual-3");
    assert_eq!(parsed.lease.crtc, DrmCrtcSelector::Id(41));
    assert_eq!(parsed.mode, None);

    let with_mode = parsed.with_mode("1024x768");
    assert_eq!(with_mode.mode.as_deref(), Some("1024x768"));

    assert!("Virtual-1".parse::<DrmScanoutRequest>().is_err());
    assert!("auto:".parse::<DrmScanoutRequest>().is_err());
}

#[test]
fn auto_selection_refuses_a_real_card_but_explicit_names_it() {
    let card = real_card();
    let refused = ensure_scanout_card(&card, &DrmCardSelector::AutoVirtual)
        .expect_err("auto must refuse a real card");
    assert!(matches!(refused, DrmScanoutError::RealCardRefused { .. }));
    assert_eq!(refused.code(), "real_card_refused");

    assert!(
        ensure_scanout_card(
            &card,
            &DrmCardSelector::Explicit(PathBuf::from("/dev/dri/card0"))
        )
        .is_ok(),
        "an explicit path may name a real card"
    );
}

#[test]
fn fake_start_allocates_fills_commits_and_verifies() {
    let backend = FakeDrmScanoutBackend::new();
    let mut handle = backend.start(&request()).expect("fake scanout");

    let status = handle.status().clone();
    assert!(status.applied);
    assert!(!status.released);
    assert_eq!(status.card, "/dev/dri/card9");
    assert_eq!(status.connector, "Virtual-1");
    assert_eq!(status.connector_id, Some(101));
    assert_eq!(status.crtc_id, Some(201));
    assert_eq!(status.plane_id, Some(301));
    assert_eq!(status.commit, DrmScanoutCommit::Atomic);
    assert_eq!(status.mode, "1024x768");
    assert_eq!((status.width, status.height), (1024, 768));
    assert_eq!(status.pitch, 1024 * 4);
    assert!(status.framebuffer_id.is_some());
    assert!(status.buffer_handle.is_some());
    assert_eq!(status.pattern, SCANOUT_PATTERN);
    assert!(status.pattern_checksum.is_some());
    assert!(status.page_flipped);
    assert!(status
        .notes
        .iter()
        .any(|note| note.kind == DrmScanoutNoteKind::ScanoutProofOnly));
    assert!(backend.active());

    let verification = handle.verify().expect("verify fake scanout");
    assert!(verification.ok);
    assert_eq!(verification.crtc_id, 201);
    assert_eq!(verification.crtc_framebuffer, status.framebuffer_id);
    assert_eq!(verification.crtc_mode.as_deref(), Some("1024x768"));
    assert_eq!(verification.connector_state, "connected");
    assert_eq!(verification.connector_crtc, Some(201));
    assert_eq!(verification.plane_id, Some(301));
    assert_eq!(verification.plane_crtc, Some(201));
    assert_eq!(verification.plane_framebuffer, status.framebuffer_id);
    assert_eq!(verification.framebuffer_buffer_handle, status.buffer_handle);
    assert_eq!(
        verification.pattern_checksum,
        status.pattern_checksum.unwrap()
    );

    let events = backend.events();
    let joined = events.join("\n");
    for expected in [
        "lease",
        "buffer",
        "fill",
        "framebuffer",
        "atomic_property plane FB_ID",
        "atomic_property plane CRTC_ID",
        "atomic_commit",
        "atomic_page_flip",
    ] {
        assert!(
            joined.contains(expected),
            "missing {expected:?} event in {joined}"
        );
    }
    assert!(
        !joined.contains("set_crtc"),
        "atomic path must not use legacy set_crtc: {joined}"
    );

    handle.teardown().expect("teardown");
    let status = handle.status();
    assert!(!status.applied);
    assert!(status.released);
    assert_eq!(status.lessee_fd, None);
    assert!(!backend.active());
}

#[test]
fn teardown_clears_before_destroying_and_revokes() {
    let backend = FakeDrmScanoutBackend::new();
    let mut handle = backend.start(&request()).expect("fake scanout");
    handle.teardown().expect("teardown");
    let events = backend.events();
    let tail = &events[events.len() - 4..];
    assert!(tail[0].starts_with("atomic_disable "), "{tail:?}");
    assert!(tail[1].starts_with("destroy_framebuffer "), "{tail:?}");
    assert!(tail[2].starts_with("destroy_buffer "), "{tail:?}");
    assert!(tail[3].starts_with("revoke "), "{tail:?}");

    // Idempotent: a second teardown records no new events.
    let count = events.len();
    handle.teardown().expect("second teardown");
    assert_eq!(backend.events().len(), count);
}

#[test]
fn dropping_an_active_proof_tears_it_down() {
    let backend = FakeDrmScanoutBackend::new();
    {
        let _handle = backend.start(&request()).expect("fake scanout");
        assert!(backend.active());
    }
    assert!(!backend.active());
    let events = backend.events();
    assert!(events.iter().any(|event| event.starts_with("revoke ")));
}

#[test]
fn explicit_mode_is_selected_and_unknown_mode_is_typed() {
    let backend = FakeDrmScanoutBackend::new();
    let handle = backend
        .start(&request().with_mode("800x600"))
        .expect("explicit mode");
    assert_eq!(handle.status().mode, "800x600");
    assert_eq!((handle.status().width, handle.status().height), (800, 600));

    let error = backend
        .start(&request().with_mode("1920x1080"))
        .err()
        .expect("unknown mode");
    assert!(matches!(error, DrmScanoutError::ModeFailed { .. }));
    assert_eq!(error.code(), "mode_failed");
}

#[test]
fn explicit_real_card_is_allowed_and_noted() {
    let mut backend = FakeDrmScanoutBackend::new();
    backend.without_cards().add_card(
        "card0",
        "/dev/dri/card0",
        "xe",
        false,
        "DP-1",
        10,
        20,
        &["1920x1080"],
    );
    let auto = backend
        .start(&DrmScanoutRequest::new(DrmLeaseRequest::new(
            DrmCardSelector::AutoVirtual,
            "DP-1",
        )))
        .err()
        .expect("auto must not select a real card");
    assert!(matches!(auto, DrmScanoutError::NoDrm { .. }));

    let handle = backend
        .start(&explicit_request("/dev/dri/card0", "DP-1"))
        .expect("explicit real card");
    assert!(handle
        .status()
        .notes
        .iter()
        .any(|note| note.kind == DrmScanoutNoteKind::RealGpuNamed));
}

#[test]
fn typed_failures_are_stable_codes() {
    let cases = [
        (FakeDrmScanoutFailure::NoDrm, "no_drm"),
        (FakeDrmScanoutFailure::NoPermission, "no_permission"),
        (
            FakeDrmScanoutFailure::CapabilityRequired,
            "capability_required",
        ),
        (FakeDrmScanoutFailure::UnsupportedVkms, "unsupported_vkms"),
        (
            FakeDrmScanoutFailure::UnsupportedConnector,
            "unsupported_connector",
        ),
        (FakeDrmScanoutFailure::ModeFailure, "mode_failed"),
        (FakeDrmScanoutFailure::BufferFailure, "buffer_failed"),
        (
            FakeDrmScanoutFailure::FramebufferFailure,
            "framebuffer_failed",
        ),
        (FakeDrmScanoutFailure::UnsupportedPlane, "unsupported_plane"),
        (FakeDrmScanoutFailure::InvalidFormat, "invalid_format"),
        (
            FakeDrmScanoutFailure::MissingAtomicProperty,
            "missing_atomic_property",
        ),
    ];
    for (failure, code) in cases {
        let backend = FakeDrmScanoutBackend::new();
        backend.fail_next(failure);
        let error = backend.start(&request()).err().expect("injected failure");
        assert_eq!(error.code(), code, "failure {failure:?}");
    }
}

#[test]
fn verification_failure_is_typed_and_kept_in_status() {
    let backend = FakeDrmScanoutBackend::new();
    backend.fail_next(FakeDrmScanoutFailure::VerificationFailure);
    let mut handle = backend.start(&request()).expect("start");
    let error = handle.verify().expect_err("injected verification failure");
    assert!(matches!(error, DrmScanoutError::VerificationFailed { .. }));
    assert_eq!(error.code(), "verification_failed");
    assert_eq!(
        handle
            .status()
            .error
            .as_ref()
            .map(|error| error.kind.as_str()),
        Some("verification_failed")
    );
}

#[test]
fn page_flip_unavailable_is_non_fatal() {
    let backend = FakeDrmScanoutBackend::new();
    backend.fail_next(FakeDrmScanoutFailure::PageFlipUnavailable);
    let handle = backend.start(&request()).expect("commit still succeeds");
    assert!(!handle.status().page_flipped);
}

#[test]
fn status_serializes_notes_ids_and_verification() {
    let backend = FakeDrmScanoutBackend::new();
    let mut handle = backend.start(&request()).expect("start");
    handle.verify().expect("verify");
    handle.teardown().expect("teardown");
    let status: &DrmScanoutStatus = handle.status();
    let encoded = serde_json::to_string(status).expect("serialize status");
    assert!(encoded.contains("\"connector_id\":101"));
    assert!(encoded.contains("\"crtc_id\":201"));
    assert!(encoded.contains("\"plane_id\":301"));
    assert!(encoded.contains("\"commit\":\"atomic\""));
    assert!(encoded.contains("\"framebuffer_id\""));
    assert!(encoded.contains("\"kind\":\"scanout_proof_only\""));
    assert!(encoded.contains("\"released\":true"));
    assert!(encoded.contains("\"ok\":true"));
    assert!(DrmScanoutStatus::default().is_empty());
}

#[test]
fn explicit_plane_is_validated_and_used_by_atomic_commit() {
    let backend = FakeDrmScanoutBackend::new();
    let handle = backend
        .start(&request().with_plane(301))
        .expect("explicit plane");
    assert_eq!(handle.status().plane_id, Some(301));
    assert_eq!(handle.status().commit, DrmScanoutCommit::Atomic);
    let events = backend.events().join("\n");
    assert!(events.contains("atomic_property plane FB_ID"), "{events}");

    let error = backend
        .start(&request().with_plane(999))
        .err()
        .expect("unknown plane must fail");
    assert!(matches!(error, DrmScanoutError::UnsupportedPlane { .. }));
    assert_eq!(error.code(), "unsupported_plane");
}

#[test]
fn atomic_unsupported_falls_back_to_legacy_with_note() {
    let backend = FakeDrmScanoutBackend::new();
    backend.fail_next(FakeDrmScanoutFailure::AtomicUnsupported);
    let handle = backend.start(&request()).expect("legacy fallback");
    let status = handle.status();
    assert_eq!(status.commit, DrmScanoutCommit::Legacy);
    assert!(status
        .notes
        .iter()
        .any(|note| { note.kind == DrmScanoutNoteKind::LegacyCommitFallback }));
    let events = backend.events().join("\n");
    assert!(events.contains("set_crtc "), "{events}");
    assert!(events.contains("page_flip "), "{events}");
    assert!(!events.contains("atomic_commit "), "{events}");
}

#[test]
fn forced_legacy_commit_is_explicit_and_clears_legacy() {
    let backend = FakeDrmScanoutBackend::new();
    let mut handle = backend
        .start(&request().with_legacy_commit())
        .expect("forced legacy");
    assert_eq!(handle.status().commit, DrmScanoutCommit::Legacy);
    assert!(handle
        .status()
        .notes
        .iter()
        .any(|note| note.kind == DrmScanoutNoteKind::LegacyCommitForced));
    handle.teardown().expect("legacy teardown");
    let events = backend.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("clear_crtc "))
            .count(),
        1,
        "{events:?}"
    );
}

#[test]
fn pattern_fill_is_deterministic_and_respects_pitch() {
    let mut tight = vec![0u8; 16 * 8];
    let tight_checksum = fill_pattern(&mut tight, 4, 2, 16);
    assert_eq!(tight_checksum, pattern_checksum(&tight));

    let mut padded = vec![0xAAu8; 32 * 2];
    let padded_checksum = fill_pattern(&mut padded, 4, 2, 32);
    assert_eq!(padded[..16], tight[..16]);
    assert_eq!(padded[16..32], [0xAA; 16]);
    assert_eq!(padded[32..48], tight[16..32]);
    assert_ne!(padded_checksum, pattern_checksum(&tight));
}
