// SPDX-License-Identifier: AGPL-3.0-only
//! Fake-backed KMS scanout proof tests.
//!
//! Nothing here opens `/dev/dri`, allocates a real dumb buffer, or programs a
//! CRTC. The fake records lease/buffer/mode/flip/teardown events in memory.

use std::path::PathBuf;

use ozma_multiseat::{
    apply_frame_damage, ensure_scanout_card, fill_pattern, frame_buffer_count, frame_damage_rect,
    pattern_checksum, DrmCard, DrmCardSelector, DrmCrtcSelector, DrmLeaseRequest,
    DrmScanoutBackend, DrmScanoutCommit, DrmScanoutError, DrmScanoutNoteKind, DrmScanoutRequest,
    DrmScanoutStatus, FakeDrmScanoutBackend, FakeDrmScanoutFailure, FakeDrmScanoutFlipFailure,
    SCANOUT_MAX_FRAMES, SCANOUT_PATTERN,
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

#[test]
fn default_request_is_one_frame_and_frame_bounds_are_typed() {
    let request = request();
    assert_eq!(request.frames, 1);
    assert_eq!(
        request.flip_timeout_ms,
        ozma_multiseat::SCANOUT_FLIP_TIMEOUT_MS
    );
    assert!(request.validate().is_ok());
    assert_eq!(request.frame_plan().frames, 1);

    let error = request
        .clone()
        .with_frames(0)
        .validate()
        .expect_err("zero frames");
    assert!(matches!(error, DrmScanoutError::InvalidFrameCount { .. }));
    assert_eq!(error.code(), "invalid_frame_count");

    let error = request
        .with_frames(SCANOUT_MAX_FRAMES + 1)
        .validate()
        .expect_err("too many frames");
    assert!(matches!(error, DrmScanoutError::InvalidFrameCount { .. }));
}

#[test]
fn damage_rect_is_deterministic_and_moves_across_frames() {
    assert_eq!(frame_buffer_count(1), 1);
    assert_eq!(frame_buffer_count(2), 2);
    assert_eq!(frame_buffer_count(3), 3);
    assert_eq!(frame_buffer_count(SCANOUT_MAX_FRAMES), 3);

    let first = frame_damage_rect(1024, 768, 0);
    assert_eq!(first, frame_damage_rect(1024, 768, 0));
    assert_ne!(first, frame_damage_rect(1024, 768, 1));
    assert_eq!(first, frame_damage_rect(1024, 768, 9));
    let (x, y, w, h) = first;
    assert!(x + w <= 1024 && y + h <= 768);

    let width = 64;
    let height = 48;
    let pitch = width * 4;
    let mut base = vec![0u8; (pitch * height) as usize];
    fill_pattern(&mut base, width, height, pitch);
    let mut a = base.clone();
    let mut b = base.clone();
    let checksum_a = apply_frame_damage(&mut a, width, height, pitch, 2);
    let checksum_b = apply_frame_damage(&mut b, width, height, pitch, 2);
    assert_eq!(checksum_a, checksum_b);
    assert_eq!(a, b);
    let mut c = base;
    let checksum_c = apply_frame_damage(&mut c, width, height, pitch, 3);
    assert_ne!(checksum_a, checksum_c);
}

#[test]
fn frame_loop_rotates_buffers_and_accounts_every_completion() {
    let backend = FakeDrmScanoutBackend::new();
    let request = request().with_frames(4);
    let mut handle = backend.start(&request).expect("frame-loop start");
    let initial = handle.status().frames.clone().expect("initial stats");
    assert_eq!(initial.requested, 4);
    assert_eq!(initial.submitted, 1);
    assert_eq!(initial.completed, 1);
    assert_eq!(initial.buffer_count, 3);
    assert!(handle
        .status()
        .notes
        .iter()
        .any(|note| note.kind == DrmScanoutNoteKind::FrameLoop));

    let stats = handle
        .run_frames(request.frame_plan())
        .expect("bounded frame loop");
    assert_eq!(stats.requested, 4);
    assert_eq!(stats.submitted, 4);
    assert_eq!(stats.completed, 4);
    assert_eq!(stats.dropped, 0);
    assert_eq!(stats.missed, 0);
    assert_eq!(stats.buffer_count, 3);
    assert_eq!(stats.page_flip_events, 4);
    assert_eq!(stats.damage_updates, 3);
    assert!(stats.average_interval_us.is_some());
    assert_eq!(stats.last_sequence, Some(4));
    let final_checksum = stats.final_checksum.expect("final checksum");
    assert_eq!(handle.status().pattern_checksum, Some(final_checksum));
    assert_eq!(
        handle.status().frames.as_ref().map(|stats| stats.completed),
        Some(4)
    );

    let events = backend.events();
    let flips: Vec<&String> = events
        .iter()
        .filter(|event| event.starts_with("atomic_page_flip "))
        .collect();
    assert_eq!(flips.len(), 4, "{events:?}");
    // Three buffers rotate: frame 3 reuses the initial buffer.
    assert_eq!(flips[0], flips[3], "{flips:?}");
    assert_ne!(flips[0], flips[1], "{flips:?}");
    assert_ne!(flips[1], flips[2], "{flips:?}");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("frame ") && event.contains(" damage "))
            .count(),
        3,
        "{events:?}"
    );

    let verification = handle.verify().expect("verify final frame");
    assert!(verification.ok);
    assert_eq!(
        verification.framebuffer_id,
        handle.status().framebuffer_id.unwrap()
    );
    assert_eq!(verification.pattern_checksum, final_checksum);
    assert_eq!(
        verification.crtc_framebuffer,
        handle.status().framebuffer_id
    );

    handle.teardown().expect("teardown frame loop");
    let events = backend.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("destroy_framebuffer "))
            .count(),
        3,
        "{events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("destroy_buffer "))
            .count(),
        3,
        "{events:?}"
    );
    let tail = &events[events.len() - 3..];
    assert!(tail[0].starts_with("destroy_framebuffer "), "{tail:?}");
    assert!(tail[1].starts_with("destroy_buffer "), "{tail:?}");
    assert!(tail[2].starts_with("revoke "), "{tail:?}");
    assert!(events
        .last()
        .is_some_and(|event| event.starts_with("revoke ")));
    assert!(!backend.active());
}

#[test]
fn flip_timeout_is_counted_and_never_claimed_as_completed() {
    let backend = FakeDrmScanoutBackend::new();
    let request = request().with_frames(3);
    let mut handle = backend.start(&request).expect("frame-loop start");
    backend.fail_flip_at(0, FakeDrmScanoutFlipFailure::Timeout);

    let error = handle
        .run_frames(request.frame_plan())
        .expect_err("initial flip timeout");
    assert!(matches!(error, DrmScanoutError::FlipTimeout { .. }));
    assert_eq!(error.code(), "flip_timeout");
    let stats = handle.status().frames.clone().expect("partial stats");
    assert_eq!(stats.submitted, 1);
    assert_eq!(stats.completed, 0, "a timed-out flip is never completed");
    assert_eq!(stats.dropped, 1);
    assert_eq!(stats.missed, 1);
    assert_eq!(stats.damage_updates, 0);
    assert_eq!(
        handle
            .status()
            .error
            .as_ref()
            .map(|error| error.kind.as_str()),
        Some("flip_timeout")
    );

    handle.teardown().expect("teardown after timeout");
    assert!(!backend.active());
}

#[test]
fn mid_loop_flip_failure_keeps_partial_accounting_and_typed_error() {
    let backend = FakeDrmScanoutBackend::new();
    let request = request().with_frames(4);
    let mut handle = backend.start(&request).expect("frame-loop start");
    backend.fail_flip_at(2, FakeDrmScanoutFlipFailure::Failed);

    let error = handle
        .run_frames(request.frame_plan())
        .expect_err("mid-loop flip failure");
    assert!(matches!(error, DrmScanoutError::FlipFailed { .. }));
    assert_eq!(error.code(), "flip_failed");
    let stats = handle.status().frames.clone().expect("partial stats");
    assert_eq!(stats.submitted, 3);
    assert_eq!(stats.completed, 2);
    assert_eq!(stats.dropped, 1);
    assert_eq!(stats.missed, 0);
    assert_eq!(stats.damage_updates, 2);
    assert!(handle.is_active(), "a failed frame does not fake teardown");
    handle.teardown().expect("teardown");

    let unsupported = FakeDrmScanoutBackend::new();
    let mut handle = unsupported.start(&request).expect("start");
    unsupported.fail_flip_at(1, FakeDrmScanoutFlipFailure::Unsupported);
    let error = handle
        .run_frames(request.frame_plan())
        .expect_err("unsupported");
    assert!(matches!(error, DrmScanoutError::FlipUnsupported { .. }));
    assert_eq!(error.code(), "flip_unsupported");
}

#[test]
fn legacy_frame_loop_uses_page_flip_events() {
    let backend = FakeDrmScanoutBackend::new();
    let request = request().with_legacy_commit().with_frames(3);
    let mut handle = backend.start(&request).expect("legacy frame-loop start");
    assert_eq!(handle.status().commit, DrmScanoutCommit::Legacy);
    let stats = handle
        .run_frames(request.frame_plan())
        .expect("legacy frame loop");
    assert_eq!(stats.submitted, 3);
    assert_eq!(stats.completed, 3);
    assert_eq!(stats.dropped, 0);
    assert_eq!(stats.page_flip_events, 3);
    let events = backend.events().join("\n");
    assert!(
        !events.contains("atomic_page_flip "),
        "legacy loop must not use atomic flips: {events}"
    );
    assert_eq!(events.matches("page_flip ").count(), 3, "{events}");
    assert_eq!(events.matches("flip_complete ").count(), 2, "{events}");
    handle.teardown().expect("legacy teardown");
}

#[test]
fn frame_loop_status_serializes_accounting_fields() {
    let backend = FakeDrmScanoutBackend::new();
    let request = request().with_frames(3);
    let mut handle = backend.start(&request).expect("start");
    handle.run_frames(request.frame_plan()).expect("frame loop");
    handle.teardown().expect("teardown");
    let encoded = serde_json::to_string(handle.status()).expect("serialize");
    for expected in [
        "\"requested\":3",
        "\"submitted\":3",
        "\"completed\":3",
        "\"dropped\":0",
        "\"missed\":0",
        "\"buffer_count\":3",
        "\"average_interval_us\"",
        "\"final_checksum\"",
        "\"kind\":\"frame_loop\"",
        "\"released\":true",
    ] {
        assert!(
            encoded.contains(expected),
            "missing {expected} in {encoded}"
        );
    }
}
