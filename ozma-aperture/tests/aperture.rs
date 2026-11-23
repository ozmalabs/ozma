// SPDX-License-Identifier: AGPL-3.0-only
//! Public-API tests for `ozma-aperture`.

use ozma_aperture::{
    evaluate, input_hash, Aperture, ApertureError, Dimension, Evaluation, Frame, PolicyDocument,
    Proposal, Provenance, Receipt, Recourse, Value,
};

fn aperture(pairs: impl IntoIterator<Item = (Dimension, Value)>) -> Aperture {
    pairs.into_iter().collect()
}

fn num(value: u64) -> Value {
    Value::number(value)
}

fn set(values: &[&str]) -> Value {
    Value::set(values)
}

fn token(value: &str) -> Value {
    Value::enumeration(value)
}

fn unknown(name: &str) -> Dimension {
    Dimension::Unknown(name.to_string())
}

#[test]
fn meet_is_commutative_associative_idempotent_and_order_independent() {
    let a = aperture([
        (Dimension::Width, num(3840)),
        (Dimension::Height, num(2160)),
        (Dimension::Fps, num(60)),
        (Dimension::Codec, set(&["h264", "hevc", "mjpeg"])),
        (Dimension::Hdr, Value::boolean(true)),
        (Dimension::Privacy, token("allow")),
        (Dimension::Trust, token("lan")),
        (unknown("future-mode"), Value::text("standard")),
    ]);
    let b = aperture([
        (Dimension::Width, num(1920)),
        (Dimension::Height, num(1080)),
        (Dimension::Fps, num(30)),
        (Dimension::Codec, set(&["h264", "mjpeg"])),
        (Dimension::Hdr, Value::boolean(false)),
        (Dimension::Privacy, token("consent-required")),
        (Dimension::Trust, token("local")),
        (Dimension::Bus, token("usb2")),
        (unknown("future-mode"), Value::text("standard")),
    ]);
    let c = aperture([
        (Dimension::Width, num(2560)),
        (Dimension::Height, num(1440)),
        (Dimension::Fps, num(30)),
        (Dimension::Codec, set(&["h264", "hevc"])),
        (Dimension::Privacy, token("deny")),
        (Dimension::Trust, token("wan")),
        (unknown("future-mode"), Value::text("standard")),
    ]);

    assert_eq!(a.meet(&b), b.meet(&a), "meet is commutative");
    assert_eq!(
        a.meet(&b).meet(&c),
        a.meet(&(b.meet(&c))),
        "meet is associative"
    );
    assert_eq!(a.meet(&a), a, "meet is idempotent");
    assert_eq!(b.meet(&b), b, "meet is idempotent");
    assert_eq!(c.meet(&c), c, "meet is idempotent");

    let left_fold = [&a, &b, &c]
        .into_iter()
        .fold(Aperture::new(), |effective, frame| effective.meet(frame));
    let right_fold = [&c, &a, &b]
        .into_iter()
        .fold(Aperture::new(), |effective, frame| effective.meet(frame));
    assert_eq!(left_fold, right_fold, "fold order does not matter");

    assert_eq!(left_fold.get(&Dimension::Width), Some(&num(1920)));
    assert_eq!(
        left_fold.get(&Dimension::Codec),
        Some(&set(&["h264"])),
        "set intersection is order independent"
    );
    assert_eq!(
        left_fold.get(&unknown("future-mode")),
        Some(&Value::text("standard")),
        "equal unknown dimensions are carried through"
    );
}

#[test]
fn numeric_minimum_set_intersection_and_restrictive_enum_ordering() {
    let physiology = aperture([
        (Dimension::Width, num(3840)),
        (Dimension::Height, num(2160)),
        (Dimension::Fps, num(60)),
        (Dimension::Codec, set(&["h264", "hevc", "mjpeg"])),
        (Dimension::Hdr, Value::boolean(true)),
        (Dimension::Privacy, token("allow")),
        (Dimension::Trust, token("lan")),
        (Dimension::AuthStrength, token("password")),
    ]);
    let forced = aperture([
        (Dimension::Width, num(1920)),
        (Dimension::Height, num(1080)),
        (Dimension::Fps, num(30)),
        (Dimension::Codec, set(&["h264", "mjpeg"])),
        (Dimension::Hdr, Value::boolean(false)),
        (Dimension::Privacy, token("deny")),
        (Dimension::Trust, token("local")),
        (Dimension::AuthStrength, token("passkey")),
    ]);

    let effective = physiology.meet(&forced);

    assert_eq!(effective.get(&Dimension::Width), Some(&num(1920)));
    assert_eq!(effective.get(&Dimension::Height), Some(&num(1080)));
    assert_eq!(effective.get(&Dimension::Fps), Some(&num(30)));
    assert_eq!(
        effective.get(&Dimension::Codec),
        Some(&set(&["h264", "mjpeg"]))
    );
    assert_eq!(effective.get(&Dimension::Hdr), Some(&Value::boolean(false)));
    assert_eq!(
        effective.get(&Dimension::Privacy),
        Some(&token("deny")),
        "deny is more restrictive than allow"
    );
    assert_eq!(
        effective.get(&Dimension::Trust),
        Some(&token("local")),
        "local is the stricter trust requirement"
    );
    assert_eq!(
        effective.get(&Dimension::AuthStrength),
        Some(&token("passkey")),
        "passkey is stronger than password"
    );

    let consent = aperture([(Dimension::Consent, token("required"))])
        .meet(&aperture([(Dimension::Consent, token("granted"))]));
    assert_eq!(
        consent.get(&Dimension::Consent),
        Some(&token("granted")),
        "a grant satisfies a pending consent requirement"
    );

    let denied = consent.meet(&aperture([(Dimension::Consent, token("denied"))]));
    assert_eq!(
        denied.get(&Dimension::Consent),
        Some(&token("denied")),
        "denied remains the most restrictive consent state"
    );
}

#[test]
fn same_residual_has_provenance_specific_recourse() {
    let physiology = aperture([(Dimension::Width, num(3840))]);
    let request = aperture([(Dimension::Width, num(2560))]);

    for (provenance, expected) in [
        (Provenance::Chosen, Recourse::Relax),
        (Provenance::Forced, Recourse::Escalate),
        (Provenance::Negotiated, Recourse::Propose),
    ] {
        let frame = Frame::new(
            format!("frame-{provenance:?}"),
            provenance,
            aperture([(Dimension::Width, num(1920))]),
        );
        let evaluation = evaluate(&physiology, &[frame.clone()], &request).expect("not empty");
        let Evaluation::Residual(residual) = evaluation else {
            panic!("expected a residual for {provenance:?}");
        };

        assert_eq!(residual.requested, request);
        assert_eq!(residual.effective.get(&Dimension::Width), Some(&num(1920)));
        assert_eq!(residual.recourse, expected);
        assert_eq!(residual.receipt.recourse, Some(expected));
        assert_eq!(residual.limiting.len(), 1);
        assert_eq!(residual.limiting[0].dimension, Dimension::Width);
        assert_eq!(residual.limiting[0].frame, frame.id);
        assert_eq!(residual.limiting[0].provenance, provenance);
        assert_eq!(residual.receipt.limiting, residual.limiting);
        assert_eq!(residual.receipt.residual.len(), 1);
        assert_eq!(residual.receipt.residual[0].requested, num(2560));
        assert_eq!(residual.receipt.residual[0].allowed, num(1920));
    }

    let beyond_device = evaluate(
        &aperture([(Dimension::Width, num(1920))]),
        &[],
        &aperture([(Dimension::Width, num(2560))]),
    )
    .expect("not empty");
    let Evaluation::Residual(residual) = beyond_device else {
        panic!("expected a residual beyond device capability");
    };
    assert_eq!(residual.recourse, Recourse::Escalate);
    assert_eq!(residual.limiting[0].frame, "physiology");

    let forced = Frame::new(
        "forced-cap",
        Provenance::Forced,
        aperture([(Dimension::Width, num(1920))]),
    );
    let chosen = Frame::new(
        "chosen-cap",
        Provenance::Chosen,
        aperture([(Dimension::Width, num(1920))]),
    );
    let both = evaluate(&physiology, &[chosen, forced], &request).expect("not empty");
    let Evaluation::Residual(residual) = both else {
        panic!("expected a residual with both frames");
    };
    assert_eq!(
        residual.recourse,
        Recourse::Escalate,
        "a forced limiter dominates a chosen limiter with the same value"
    );
    assert_eq!(residual.limiting[0].frame, "forced-cap");
}

#[test]
fn admitted_evaluation_has_no_residual_or_recourse() {
    let physiology = aperture([(Dimension::Width, num(3840))]);
    let request = aperture([(Dimension::Width, num(1920))]);

    let evaluation = evaluate(&physiology, &[], &request).expect("not empty");
    let Evaluation::Admitted(admission) = evaluation else {
        panic!("expected an admission");
    };

    assert_eq!(admission.effective.get(&Dimension::Width), Some(&num(3840)));
    assert!(admission.receipt.limiting.is_empty());
    assert!(admission.receipt.residual.is_empty());
    assert_eq!(admission.receipt.recourse, None);
    assert_eq!(admission.receipt.policy_version, "unversioned");
}

#[test]
fn negotiated_frame_requires_every_party_and_retraction_is_all_party() {
    let frame = Frame::new(
        "pairing-grant",
        Provenance::Negotiated,
        aperture([(Dimension::Permissions, set(&["view", "control"]))]),
    );
    let mut proposal = Proposal::new(frame.clone(), ["node", "user", "controller"]);

    assert!(!proposal.accept("node"));
    assert!(!proposal.is_adopted());
    assert!(!proposal.accept("controller"));
    assert!(!proposal.is_adopted());
    assert!(!proposal.accept("stranger"));
    assert!(!proposal.is_adopted());

    assert!(proposal.accept("user"), "all parties accepted");
    assert!(proposal.is_adopted());
    assert_eq!(proposal.adopted_frame(), Some(&frame));

    assert!(
        !proposal.retract("controller"),
        "one retraction does not fully retract"
    );
    assert!(
        !proposal.is_adopted(),
        "a retraction un-adopts the proposal"
    );
    assert!(!proposal.is_retracted());

    assert!(!proposal.retract("node"));
    assert!(proposal.retract("user"), "every party has retracted");
    assert!(proposal.is_retracted());
    assert!(!proposal.retract("user"), "retracting twice is a no-op");

    assert!(!proposal.accept("node"));
    assert!(!proposal.accept("user"));
    assert!(proposal.accept("controller"));
    assert!(proposal.is_adopted(), "the proposal can be adopted again");
}

#[test]
fn empty_aperture_returns_typed_error_with_conflicting_dimensions() {
    let physiology = aperture([
        (Dimension::Codec, set(&["h264"])),
        (Dimension::Permissions, set(&["view"])),
    ]);
    let frame = Frame::new(
        "restricted",
        Provenance::Forced,
        aperture([
            (Dimension::Codec, set(&["hevc"])),
            (Dimension::Permissions, set(&["control"])),
        ]),
    );

    let error = evaluate(
        &physiology,
        &[frame],
        &aperture([(Dimension::Codec, set(&["h264"]))]),
    )
    .expect_err("the intersection is empty");

    match &error {
        ApertureError::Empty { conflicting } => assert_eq!(
            conflicting,
            &vec!["codec".to_string(), "permissions".to_string()]
        ),
        other => panic!("expected aperture.empty, got {other:?}"),
    }
    let message = error.to_string();
    assert!(message.contains("aperture.empty"));
    assert!(message.contains("codec"));

    let unknown_value_error = evaluate(
        &aperture([(Dimension::Privacy, token("allow"))]),
        &[Frame::new(
            "future-policy",
            Provenance::Forced,
            aperture([(Dimension::Privacy, token("future-mode"))]),
        )],
        &Aperture::new(),
    )
    .expect_err("an unknown ordered token fails closed");

    assert_eq!(
        unknown_value_error,
        ApertureError::Empty {
            conflicting: vec!["privacy".to_string()]
        }
    );

    let unknown_dimension_error = evaluate(
        &aperture([(unknown("future-feature"), Value::text("on"))]),
        &[Frame::new(
            "future-policy",
            Provenance::Forced,
            aperture([(unknown("future-feature"), Value::text("off"))]),
        )],
        &Aperture::new(),
    )
    .expect_err("an unknown dimension with an unequal value fails closed");

    assert_eq!(
        unknown_dimension_error,
        ApertureError::Empty {
            conflicting: vec!["future-feature".to_string()]
        }
    );
}

#[test]
fn receipts_are_stable_serializable_and_include_evidence() {
    let document = PolicyDocument::from_json(
        r#"{
            "physiology": {"width": 3840, "height": 2160, "fps": 60},
            "frames": [
                {
                    "id": "usb2-bandwidth",
                    "provenance": "forced",
                    "aperture": {"width": 1920, "height": 1080, "fps": 30}
                }
            ],
            "policy_version": "opi5-usb2-v1"
        }"#,
    )
    .expect("valid policy");
    let request = aperture([
        (Dimension::Width, num(2560)),
        (Dimension::Height, num(1440)),
        (Dimension::Fps, num(60)),
    ]);

    let first = document.evaluate(&request).expect("not empty");
    let second = document.evaluate(&request).expect("not empty");
    assert_eq!(first, second, "evaluations are deterministic");

    let Evaluation::Residual(residual) = first else {
        panic!("expected a residual");
    };
    let receipt = &residual.receipt;

    assert_eq!(receipt.policy_version, "opi5-usb2-v1");
    assert_eq!(receipt.input_hash.len(), 16);
    assert!(receipt.input_hash.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(
        input_hash(
            &document.physiology,
            &document.frames,
            &request,
            &document.policy_version
        ),
        receipt.input_hash
    );

    let json = serde_json::to_string(receipt).expect("receipt serializes");
    assert!(json.contains("\"policy_version\":\"opi5-usb2-v1\""));
    assert!(json.contains("\"frame\":\"usb2-bandwidth\""));
    assert!(json.contains("\"provenance\":\"forced\""));
    assert!(json.contains("\"recourse\":\"escalate\""));
    assert!(json.contains("\"dimension\":\"width\""));

    let round_trip: Receipt = serde_json::from_str(&json).expect("receipt round-trips");
    assert_eq!(&round_trip, receipt);

    let other_request = aperture([(Dimension::Width, num(1280))]);
    assert_ne!(
        input_hash(
            &document.physiology,
            &document.frames,
            &other_request,
            &document.policy_version
        ),
        receipt.input_hash,
        "the input hash covers the request"
    );
}

#[test]
fn security_attenuation_never_widens_and_unknown_fails_closed() {
    let physiology = aperture([
        (Dimension::Width, num(3840)),
        (Dimension::Codec, set(&["h264", "hevc", "mjpeg"])),
        (Dimension::Privacy, token("allow")),
        (Dimension::Trust, token("lan")),
        (Dimension::Permissions, set(&["view", "control", "audio"])),
        (unknown("future-mode"), Value::text("standard")),
    ]);
    let mut effective = physiology;

    let frames = [
        Frame::new(
            "usb2-bandwidth",
            Provenance::Forced,
            aperture([
                (Dimension::Width, num(1920)),
                (Dimension::Codec, set(&["h264", "mjpeg"])),
                (Dimension::Trust, token("local")),
            ]),
        ),
        Frame::new(
            "chosen-session",
            Provenance::Chosen,
            aperture([
                (Dimension::Codec, set(&["h264"])),
                (Dimension::Permissions, set(&["view", "audio"])),
            ]),
        ),
        Frame::new(
            "privacy-policy",
            Provenance::Forced,
            aperture([(Dimension::Privacy, token("consent-required"))]),
        ),
    ];

    for frame in &frames {
        let next = effective.meet(&frame.aperture);
        assert!(
            next.is_no_wider_than(&effective),
            "frame {} widened the aperture",
            frame.id
        );
        assert_eq!(
            effective.meet(&next),
            next,
            "the new effective aperture stays below the previous one"
        );
        effective = next;
    }

    assert_eq!(effective.get(&Dimension::Width), Some(&num(1920)));
    assert_eq!(effective.get(&Dimension::Codec), Some(&set(&["h264"])));
    assert_eq!(
        effective.get(&Dimension::Permissions),
        Some(&set(&["view", "audio"]))
    );
    assert_eq!(
        effective.get(&Dimension::Privacy),
        Some(&token("consent-required"))
    );
    assert_eq!(effective.get(&Dimension::Trust), Some(&token("local")));

    let carried = effective.meet(&aperture([(
        unknown("future-mode"),
        Value::text("standard"),
    )]));
    assert_eq!(
        carried.get(&unknown("future-mode")),
        Some(&Value::text("standard"))
    );
    assert!(!carried.is_empty());

    let conflict = effective.meet(&aperture([(
        unknown("future-mode"),
        Value::text("experimental"),
    )]));
    assert!(conflict.is_empty());
    assert_eq!(conflict.conflicts(), vec!["future-mode".to_string()]);

    let emptied = effective.meet(&aperture([(Dimension::Codec, set(&["vp9"]))]));
    assert!(emptied.is_empty());
    assert!(
        emptied.is_no_wider_than(&effective),
        "an empty aperture is narrower than its predecessor"
    );

    let unknown_privacy = aperture([(Dimension::Privacy, token("allow"))])
        .meet(&aperture([(Dimension::Privacy, token("future-mode"))]));
    assert!(
        unknown_privacy.is_empty(),
        "unknown enum tokens fail closed"
    );
}

#[test]
fn json_policy_loader_parses_versions_unknown_dimensions_and_rejects_bad_json() {
    let json = r#"{
        "physiology": {
            "width": 3840,
            "height": 2160,
            "fps": 60,
            "codec": ["h264", "hevc", "mjpeg"],
            "hdr": true,
            "privacy": "allow",
            "trust": "lan",
            "permissions": ["view", "control", "audio"],
            "future-mode": "standard",
            "future-budget": 7
        },
        "frames": [
            {
                "id": "usb2-bandwidth",
                "provenance": "forced",
                "aperture": {
                    "width": 1920,
                    "height": 1080,
                    "fps": 30,
                    "codec": "h264",
                    "privacy": "consent-required"
                }
            },
            {
                "id": "preferred-codec",
                "provenance": "chosen",
                "aperture": {"codec": ["h264", "mjpeg"]}
            }
        ],
        "policy_version": "opi5-usb2-v1"
    }"#;

    let document = PolicyDocument::from_json(json).expect("valid policy");
    assert_eq!(document.policy_version, "opi5-usb2-v1");
    assert_eq!(document.frames.len(), 2);
    assert_eq!(document.frames[1].provenance, Provenance::Chosen);
    assert_eq!(document.physiology.get(&Dimension::Width), Some(&num(3840)));
    assert_eq!(
        document.physiology.get(&unknown("future-mode")),
        Some(&Value::text("standard"))
    );
    assert_eq!(
        document.physiology.get(&unknown("future-budget")),
        Some(&Value::number(7))
    );

    let request = aperture([
        (Dimension::Width, num(2560)),
        (Dimension::Height, num(1440)),
        (Dimension::Fps, num(60)),
        (Dimension::Codec, set(&["h264"])),
    ]);
    let evaluation = document.evaluate(&request).expect("not empty");
    let Evaluation::Residual(residual) = evaluation else {
        panic!("expected a residual");
    };

    assert_eq!(residual.effective.get(&Dimension::Width), Some(&num(1920)));
    assert_eq!(residual.effective.get(&Dimension::Height), Some(&num(1080)));
    assert_eq!(residual.effective.get(&Dimension::Fps), Some(&num(30)));
    assert_eq!(
        residual.effective.get(&Dimension::Codec),
        Some(&set(&["h264"]))
    );
    assert_eq!(residual.recourse, Recourse::Escalate);
    assert_eq!(residual.receipt.policy_version, "opi5-usb2-v1");
    assert!(residual.limiting.iter().any(
        |limiting| limiting.dimension == Dimension::Width && limiting.frame == "usb2-bandwidth"
    ));

    let serialized = document.to_json().expect("policy serializes");
    let reparsed = PolicyDocument::from_json(&serialized).expect("policy round-trips");
    assert_eq!(reparsed, document);

    assert!(matches!(
        PolicyDocument::from_json("{"),
        Err(ApertureError::InvalidJson { .. })
    ));

    let wrong_type = PolicyDocument::from_json(
        r#"{"physiology":{"width":"wide"},"frames":[],"policy_version":"v1"}"#,
    );
    assert!(matches!(wrong_type, Err(ApertureError::InvalidJson { .. })));
}
