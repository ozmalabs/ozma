// SPDX-License-Identifier: AGPL-3.0-only
//! Fake-backed DRM lease and supervisor integration tests.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use ozma_multiseat::{
    DrmCardSelector, DrmCrtcSelector, DrmLeaseBackend, DrmLeaseError, DrmLeaseNoteKind,
    DrmLeaseRequest, DrmLeaseStatus, DrmObjectKind, FakeDisplayBackend, FakeDrmLeaseBackend,
    FakeDrmLeaseFailure, FakeInputBackend, FakeNodeLauncher, FakeReadinessProbe, HostTopology,
    MultiseatError, MultiseatPlan, MultiseatPlanner, MultiseatStatus, PlannerOptions,
    ProcessSeatSupervisor, SeatState, SeatSupervisor, SupervisorOptions,
};

fn display(index: u32, name: &str, x: i32) -> ozma_multiseat::DisplayOutput {
    ozma_multiseat::DisplayOutput::new(index, name, 1920, 1080, x, 0, ":0")
}

fn plan(displays: Vec<ozma_multiseat::DisplayOutput>) -> MultiseatPlan {
    let topology = HostTopology::enumerate(
        &FakeDisplayBackend::new(displays),
        &FakeInputBackend::empty(),
    )
    .expect("topology");
    let planner = MultiseatPlanner::new(PlannerOptions {
        machine_name: "host".to_owned(),
        base_udp_port: 7331,
        base_http_port: 7382,
        ..PlannerOptions::default()
    });
    topology.plan(&planner).expect("plan")
}

fn one_seat_plan() -> MultiseatPlan {
    plan(vec![display(0, "HDMI-1", 0)])
}

fn two_seat_plan() -> MultiseatPlan {
    plan(vec![display(0, "HDMI-1", 0), display(1, "DP-2", 1920)])
}

fn request() -> DrmLeaseRequest {
    DrmLeaseRequest::new(DrmCardSelector::AutoVirtual, "Virtual-1")
}

fn options(dry_run: bool, leases: BTreeMap<String, DrmLeaseRequest>) -> SupervisorOptions {
    SupervisorOptions {
        node_bin: None,
        readiness_timeout: Duration::from_millis(100),
        readiness_poll: Duration::from_millis(1),
        restart_backoff: Duration::from_millis(10),
        restart_backoff_max: Duration::from_millis(40),
        healthy_uptime: Duration::from_secs(30),
        dry_run,
        apply_ownership: false,
        apply_routing: false,
        input_isolation: false,
        input_targeting: Default::default(),
        seat_display: Default::default(),
        drm_leases: leases,
    }
}

fn supervisor(
    options: SupervisorOptions,
    backend: &FakeDrmLeaseBackend,
    launcher: &FakeNodeLauncher,
) -> ProcessSeatSupervisor {
    ProcessSeatSupervisor::with_parts(
        options,
        Arc::new(launcher.clone()),
        Arc::new(FakeReadinessProbe::new()),
    )
    .with_drm_lease_backend(Box::new(backend.clone()))
}

#[test]
fn enumerate_and_inspect_report_virtual_and_real_cards() {
    let backend = FakeDrmLeaseBackend::new();
    let cards = backend.enumerate().expect("enumerate");
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].name, "card9");
    assert_eq!(cards[0].driver, "vkms");
    assert!(cards[0].is_virtual());
    assert_eq!(cards[0].connectors[0].name, "Virtual-1");
    assert_eq!(cards[0].connectors[0].object_id, Some(101));
    assert_eq!(cards[0].crtcs, vec![201]);

    let inspected = backend.inspect(&cards[0]).expect("inspect");
    assert_eq!(inspected.connectors[0].object_id, Some(101));
}

#[test]
fn requests_parse_explicit_auto_and_crtc_ids() {
    let auto: DrmLeaseRequest = "auto:Virtual-1".parse().expect("auto request");
    assert_eq!(auto.card, DrmCardSelector::AutoVirtual);
    assert_eq!(auto.connector, "Virtual-1");
    assert_eq!(auto.crtc, DrmCrtcSelector::Auto);

    let explicit: DrmLeaseRequest = "/dev/dri/card3:Virtual-1@31"
        .parse()
        .expect("explicit request");
    assert_eq!(
        explicit.card,
        DrmCardSelector::Explicit("/dev/dri/card3".into())
    );
    assert_eq!(explicit.crtc, DrmCrtcSelector::Id(31));

    let short: DrmLeaseRequest = "card3:Virtual-1".parse().expect("short card name");
    assert_eq!(
        short.card,
        DrmCardSelector::Explicit("/dev/dri/card3".into())
    );

    assert!("Virtual-1".parse::<DrmLeaseRequest>().is_err());
    assert!("auto:".parse::<DrmLeaseRequest>().is_err());
}

#[test]
fn acquire_validates_objects_and_reports_ids_and_fd() {
    let backend = FakeDrmLeaseBackend::new();
    let mut lease = backend.acquire(&request()).expect("acquire");
    assert_eq!(lease.lessee_id(), 1);
    assert_eq!(lease.connector(), "Virtual-1");
    assert_eq!(lease.connector_id(), 101);
    assert_eq!(lease.crtc_id(), 201);
    assert!(lease.fd().is_some());
    assert!(lease.is_active());
    let objects = lease.validate().expect("validate lease");
    assert_eq!(objects.connectors, vec![101]);
    assert_eq!(objects.crtcs, vec![201]);
    assert_eq!(lease.list_lease_ids().expect("list lessees"), vec![1]);

    let status = DrmLeaseStatus::from_handle(lease.as_ref());
    assert!(status.applied);
    assert!(!status.released);
    assert_eq!(status.lessee_id, Some(1));
    assert_eq!(status.connector_id, Some(101));
    assert_eq!(status.crtc_id, Some(201));
    assert!(status
        .notes
        .iter()
        .any(|note| note.kind == DrmLeaseNoteKind::LeaseOnlyNoScanout));
    assert_eq!(backend.active_count(), 1);

    lease.revoke().expect("revoke");
    assert!(!lease.is_active());
    assert!(matches!(lease.validate(), Err(DrmLeaseError::Released(1))));
    assert_eq!(backend.active_count(), 0);
    assert_eq!(backend.revoked(), vec![1]);
}

#[test]
fn drop_releases_an_active_lease() {
    let backend = FakeDrmLeaseBackend::new();
    {
        let lease = backend.acquire(&request()).expect("acquire");
        assert!(lease.fd().is_some());
        assert_eq!(backend.active_count(), 1);
    }
    assert_eq!(backend.active_count(), 0);
    assert_eq!(backend.dropped(), vec![1]);
    assert!(backend.revoked().is_empty());
}

#[test]
fn duplicate_and_unknown_objects_are_typed() {
    let backend = FakeDrmLeaseBackend::new();
    let _lease = backend.acquire(&request()).expect("first lease");

    let duplicate = match backend.acquire(&request()) {
        Ok(_) => panic!("duplicate lease must fail"),
        Err(error) => error,
    };
    assert!(matches!(
        duplicate,
        DrmLeaseError::AlreadyLeased {
            kind: DrmObjectKind::Connector,
            id: 101,
            ..
        }
    ));

    let unknown_connector = DrmLeaseRequest::new(DrmCardSelector::AutoVirtual, "DP-404");
    assert!(matches!(
        backend.acquire(&unknown_connector),
        Err(DrmLeaseError::UnknownConnector { .. })
    ));

    let unknown_crtc =
        DrmLeaseRequest::new(DrmCardSelector::AutoVirtual, "Virtual-1").with_crtc(999);
    assert!(matches!(
        backend.acquire(&unknown_crtc),
        Err(DrmLeaseError::UnknownObject {
            kind: DrmObjectKind::Crtc,
            id: 999,
            ..
        })
    ));
}

#[test]
fn typed_open_and_capability_failures_are_not_fake_leases() {
    let backend = FakeDrmLeaseBackend::new();

    backend.fail_next(FakeDrmLeaseFailure::NoDrm);
    assert!(matches!(
        backend.acquire(&request()),
        Err(DrmLeaseError::NoDrm { .. })
    ));

    backend.fail_next_no_permission();
    assert!(matches!(
        backend.acquire(&request()),
        Err(DrmLeaseError::NoPermission { .. })
    ));

    backend.fail_next_unsupported();
    assert!(matches!(
        backend.acquire(&request()),
        Err(DrmLeaseError::Unsupported { .. })
    ));

    backend.fail_next_capability_required();
    assert!(matches!(
        backend.acquire(&request()),
        Err(DrmLeaseError::CapabilityRequired { .. })
    ));

    assert_eq!(backend.active_count(), 0);
    assert_eq!(backend.requests().len(), 4);
}

#[test]
fn auto_virtual_never_selects_a_real_card() {
    let mut backend = FakeDrmLeaseBackend::new();
    let real_only = FakeDrmLeaseBackend::new();
    // Replace the default virtual card by adding a real card under a separate
    // backend whose default is removed is not exposed; instead request the
    // explicit real path and confirm auto still picks the vkms card.
    backend.add_card(
        "card1",
        "/dev/dri/card1",
        "amdgpu",
        false,
        "DP-1",
        301,
        &[401],
    );
    let auto = backend.acquire(&request()).expect("auto virtual");
    assert_eq!(auto.card(), std::path::Path::new("/dev/dri/card9"));
    drop(auto);

    let real = DrmLeaseRequest::new(DrmCardSelector::Explicit("/dev/dri/card1".into()), "DP-1");
    let explicit = backend
        .acquire(&real)
        .expect("explicit real card named by operator");
    assert_eq!(explicit.connector_id(), 301);

    real_only.fail_next_no_drm();
    assert!(matches!(
        real_only.acquire(&request()),
        Err(DrmLeaseError::NoDrm { .. })
    ));
}

#[test]
fn supervisor_start_exports_fd_and_stop_revokes() {
    let plan = one_seat_plan();
    let seat_id = plan.seats[0].config.id.clone();
    let backend = FakeDrmLeaseBackend::new();
    let launcher = FakeNodeLauncher::new();
    let mut supervisor = supervisor(
        options(false, BTreeMap::from([(seat_id.clone(), request())])),
        &backend,
        &launcher,
    );

    let status = supervisor.start(&plan.seats[0]).expect("start");
    assert_eq!(status.state, SeatState::Running);
    assert!(status.resources.drm_lease_applied);
    assert_eq!(status.drm_lease.lessee_id, Some(1));
    assert_eq!(status.drm_lease.connector, "Virtual-1");
    assert!(status.drm_lease.fd.is_some());
    assert!(status
        .drm_lease
        .notes
        .iter()
        .any(|note| note.kind == DrmLeaseNoteKind::LeaseOnlyNoScanout));

    let spawns = launcher.spawns();
    assert_eq!(spawns.len(), 1);
    assert_eq!(
        spawns[0].env.get("OZMA_DRM_FD").map(String::as_str),
        status.drm_lease.fd.map(|fd| fd.to_string()).as_deref()
    );
    assert_eq!(
        spawns[0].env.get("OZMA_DRM_LEASE_ID").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        spawns[0].env.get("OZMA_DRM_CONNECTOR").map(String::as_str),
        Some("Virtual-1")
    );
    assert_eq!(
        spawns[0].env.get("OZMA_DRM_CRTC_ID").map(String::as_str),
        Some("201")
    );
    assert_eq!(backend.active_count(), 1);

    let stopped = supervisor.stop(&seat_id).expect("stop");
    assert_eq!(stopped.state, SeatState::Stopped);
    assert!(!stopped.resources.drm_lease_applied);
    assert!(stopped.resources.drm_lease_released);
    assert!(stopped.drm_lease.released);
    assert_eq!(stopped.drm_lease.fd, None);
    assert_eq!(backend.active_count(), 0);
    assert_eq!(backend.revoked(), vec![1]);
}

#[test]
fn lease_failure_prevents_spawn_and_later_start_rolls_back_earlier_lease() {
    let plan = two_seat_plan();
    let first = plan.seats[0].config.id.clone();
    let second = plan.seats[1].config.id.clone();
    let backend = FakeDrmLeaseBackend::new();
    let launcher = FakeNodeLauncher::new();
    let leases = BTreeMap::from([
        (
            first.clone(),
            DrmLeaseRequest::new(
                DrmCardSelector::Explicit("/dev/dri/card9".into()),
                "Virtual-1",
            ),
        ),
        (second.clone(), request()),
    ]);
    let mut supervisor = supervisor(options(false, leases), &backend, &launcher);

    supervisor.start(&plan.seats[0]).expect("first seat");
    assert_eq!(launcher.spawn_count(), 1);
    assert_eq!(backend.active_count(), 1);

    backend.fail_next_no_permission();
    let error = supervisor.start(&plan.seats[1]).expect_err("second seat");
    assert!(matches!(error, MultiseatError::DrmLease(_)));
    assert_eq!(launcher.spawn_count(), 1, "failed seat never spawned");
    assert_eq!(backend.active_count(), 0, "earlier lease rolled back");
    assert_eq!(backend.revoked(), vec![1]);

    let statuses = supervisor.status();
    let first_status = statuses
        .iter()
        .find(|status| status.seat_id == first)
        .expect("first status");
    assert!(first_status.resources.drm_lease_released);
    let second_status = statuses
        .iter()
        .find(|status| status.seat_id == second)
        .expect("second status");
    assert_eq!(second_status.state, SeatState::Failed);
    assert_eq!(
        second_status
            .drm_lease
            .error
            .as_ref()
            .map(|error| error.kind.as_str()),
        Some("no_permission")
    );
}

#[test]
fn missing_backend_is_typed_and_never_spawns() {
    let plan = one_seat_plan();
    let seat_id = plan.seats[0].config.id.clone();
    let launcher = FakeNodeLauncher::new();
    let mut supervisor = ProcessSeatSupervisor::with_parts(
        options(false, BTreeMap::from([(seat_id, request())])),
        Arc::new(launcher.clone()),
        Arc::new(FakeReadinessProbe::new()),
    );

    let error = supervisor.start(&plan.seats[0]).expect_err("no backend");
    assert!(matches!(error, MultiseatError::Unsupported(_)));
    assert_eq!(launcher.spawn_count(), 0);
}

#[test]
fn dry_run_reports_the_lease_without_acquiring_it() {
    let plan = one_seat_plan();
    let seat_id = plan.seats[0].config.id.clone();
    let backend = FakeDrmLeaseBackend::new();
    let launcher = FakeNodeLauncher::new();
    let mut supervisor = supervisor(
        options(true, BTreeMap::from([(seat_id, request())])),
        &backend,
        &launcher,
    );

    let status = supervisor.start(&plan.seats[0]).expect("dry run");
    assert_eq!(status.state, SeatState::Planned);
    assert!(status
        .message
        .as_deref()
        .is_some_and(|message| message.contains("would acquire a DRM lease")));
    assert!(backend.requests().is_empty());
    assert_eq!(backend.active_count(), 0);
    assert_eq!(launcher.spawn_count(), 0);
}

#[test]
fn status_document_carries_lease_state() {
    let plan = one_seat_plan();
    let seat_id = plan.seats[0].config.id.clone();
    let backend = FakeDrmLeaseBackend::new();
    let launcher = FakeNodeLauncher::new();
    let mut supervisor = supervisor(
        options(false, BTreeMap::from([(seat_id, request())])),
        &backend,
        &launcher,
    );
    supervisor.start(&plan.seats[0]).expect("start");

    let document = MultiseatStatus::from_plan_with_runtime(&plan, &supervisor.status());
    let seat = &document.seats[0];
    assert!(seat.resources.drm_lease_applied);
    assert_eq!(seat.drm_lease.lessee_id, Some(1));
    assert_eq!(seat.drm_lease.connector, "Virtual-1");

    let encoded = serde_json::to_value(&document).expect("status JSON");
    assert_eq!(encoded["seats"][0]["drm_lease"]["lessee_id"], 1);
    assert_eq!(encoded["seats"][0]["drm_lease"]["crtc_id"], 201);
    assert_eq!(encoded["seats"][0]["resources"]["drm_lease_applied"], true);
}
