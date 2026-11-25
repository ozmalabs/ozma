// SPDX-License-Identifier: AGPL-3.0-only
//! Opt-in live DRM lease test against a `vkms` virtual card.
//!
//! Nothing runs unless `OZMA_DRM_LIVE=1`. The test finds the card by sysfs
//! driver name (`vkms`) and never touches a real GPU even when the operator
//! explicitly names another card.

use std::time::Duration;

use ozma_multiseat::{
    enumerate_drm_cards, DrmCardSelector, DrmCrtcSelector, DrmLeaseBackend, DrmLeaseError,
    DrmLeaseRequest, RealDrmLeaseBackend,
};

#[test]
#[ignore = "opt-in live vkms DRM lease: set OZMA_DRM_LIVE=1 and load vkms"]
fn vkms_lease_create_validate_duplicate_revoke() {
    if std::env::var("OZMA_DRM_LIVE").as_deref() != Ok("1") {
        eprintln!("skipping: set OZMA_DRM_LIVE=1 to run the live vkms DRM lease test");
        return;
    }

    let cards = enumerate_drm_cards().expect("enumerate DRM cards");
    let requested_card = std::env::var("OZMA_DRM_CARD").ok();
    let vkms = cards
        .into_iter()
        .find(|card| match requested_card.as_deref() {
            Some(path) => card.path == std::path::Path::new(path),
            None => card.is_vkms(),
        })
        .expect(
            "no vkms card found; run `sudo modprobe vkms` and check /sys/class/drm/card*/device/driver",
        );
    eprintln!(
        "vkms card: {} path={} driver={}",
        vkms.name,
        vkms.path.display(),
        vkms.driver
    );
    assert!(
        vkms.is_vkms() || matches!(vkms.driver.as_str(), "vkms" | "faux_driver"),
        "live test refuses non-vkms card {} ({})",
        vkms.path.display(),
        vkms.driver
    );
    assert!(vkms.is_virtual());

    let backend = RealDrmLeaseBackend::new();
    let inspected = backend
        .inspect(&vkms)
        .expect("open/inspect the vkms card (needs video-group read/write)");
    eprintln!(
        "connectors: {:?}",
        inspected
            .connectors
            .iter()
            .map(|connector| (
                connector.name.as_str(),
                connector.object_id,
                connector.crtc_id,
                connector.status.clone()
            ))
            .collect::<Vec<_>>()
    );
    eprintln!("crtcs: {:?}", inspected.crtcs);
    assert!(!inspected.connectors.is_empty(), "vkms has no connector");
    assert!(!inspected.crtcs.is_empty(), "vkms has no CRTC");

    let connector = inspected
        .connectors
        .iter()
        .find(|connector| connector.object_id.is_some())
        .expect("vkms connector with object id");
    let request = DrmLeaseRequest {
        card: DrmCardSelector::Explicit(vkms.path.clone()),
        connector: connector.name.clone(),
        crtc: DrmCrtcSelector::Auto,
        plane: Default::default(),
        writeback_connector: None,
    };

    let mut lease = backend.acquire(&request).unwrap_or_else(|error| {
        panic!(
            "create lease for {}:{} failed: {error} (run this test as root or with \
             CAP_SYS_ADMIN/DRM master)",
            vkms.path.display(),
            connector.name
        )
    });
    eprintln!(
        "leased lessee={} fd={:?} connector={} id={} crtc={} planes={:?}",
        lease.lessee_id(),
        lease.fd(),
        lease.connector(),
        lease.connector_id(),
        lease.crtc_id(),
        lease.plane_ids()
    );
    assert_eq!(lease.card(), vkms.path.as_path());
    assert_eq!(lease.connector(), connector.name);
    assert!(lease.fd().is_some(), "lease must own a lessee fd");
    assert!(lease.crtc_id() != 0);

    let objects = lease.validate().expect("GET_LEASE on the lessee fd");
    eprintln!("kernel lease objects: {objects:?}");
    assert!(objects.connectors.contains(&lease.connector_id()));
    assert!(objects.crtcs.contains(&lease.crtc_id()));

    // Verify the export mechanism a real seat child uses: clear FD_CLOEXEC,
    // spawn a child with the fd number in the environment, and confirm the fd
    // is present after exec.
    let lease_fd = lease.fd().expect("lessee fd");
    lease
        .set_inheritable(true)
        .expect("clear FD_CLOEXEC on lessee fd");
    let inherited = std::process::Command::new("sh")
        .arg("-c")
        .arg("test -e /proc/self/fd/$OZMA_DRM_FD")
        .env("OZMA_DRM_FD", lease_fd.to_string())
        .status()
        .expect("spawn fd inheritance probe");
    lease
        .set_inheritable(false)
        .expect("restore FD_CLOEXEC on lessee fd");
    eprintln!("child fd inheritance probe: {inherited}");
    assert!(inherited.success(), "child did not inherit the lessee fd");

    let before = lease
        .list_lease_ids()
        .expect("list lessees through the lessor's master fd");
    eprintln!("lessees before revoke: {before:?}");
    assert!(before.contains(&lease.lessee_id()));

    // A second request for the same connector must be a typed already-leased
    // error, never a second fake lease.
    let duplicate = match backend.acquire(&request) {
        Ok(_) => panic!("kernel/backend created a duplicate lease for the same connector"),
        Err(error) => error,
    };
    eprintln!("duplicate lease error: {duplicate}");
    assert!(matches!(
        duplicate,
        DrmLeaseError::AlreadyLeased { .. } | DrmLeaseError::LeaseFailed { .. }
    ));

    lease.revoke().expect("revoke lease");
    assert!(!lease.is_active());
    assert!(matches!(lease.validate(), Err(DrmLeaseError::Released(_))));

    // A fresh acquire of the same connector/CRTC proves the revoked lease no
    // longer holds the objects. Then release the replacement as well.
    let mut replacement = backend
        .acquire(&request)
        .expect("same objects must be leasable again after revoke");
    eprintln!(
        "replacement lessee={} fd={:?}",
        replacement.lessee_id(),
        replacement.fd()
    );
    assert!(replacement.is_active());
    replacement.revoke().expect("revoke replacement lease");

    // Closing the lessor's card and the lessee fd is the final release path.
    drop(lease);
    std::thread::sleep(Duration::from_millis(50));
}
