// SPDX-License-Identifier: AGPL-3.0-only
//! Opt-in live KMS scanout proof against a `vkms` virtual card.
//!
//! Nothing runs unless `OZMA_DRM_LIVE=1`. The test refuses any card that is not
//! a vkms/faux virtual card, even if `OZMA_DRM_CARD` names it. It creates one
//! dumb buffer, programs one connector/CRTC, verifies the result (optionally
//! cross-checking the CRTC through a second card fd), tears everything down,
//! and proves the same objects can be programmed again afterwards.

use std::fs::OpenOptions;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::Path;

use drm::control::{crtc, Device as ControlDevice, RawResourceHandle};
use ozma_multiseat::{
    enumerate_drm_cards, DrmCardSelector, DrmLeaseBackend, DrmLeaseRequest, DrmScanoutBackend,
    DrmScanoutRequest, RealDrmLeaseBackend, RealDrmScanoutBackend,
};

struct ReadCard(std::fs::File);

impl AsFd for ReadCard {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for ReadCard {}
impl ControlDevice for ReadCard {}

/// Independently re-read one CRTC from a second card fd: `(framebuffer, mode)`.
///
/// `modetest -M vkms` always opens the default vkms instance when more than one
/// is present (its `-D` is a libdrm busid, and configfs faux instances are not
/// listed by busid), so the test uses the same `GET_CRTC` ioctl directly.
fn read_crtc(card: &Path, crtc_id: u32) -> Option<(Option<u32>, Option<String>, (u32, u32))> {
    let file = OpenOptions::new().read(true).write(true).open(card).ok()?;
    let card = ReadCard(file);
    let raw = NonZeroU32::new(crtc_id)?;
    let handle = crtc::Handle::from(RawResourceHandle::from(raw));
    let info = card.get_crtc(handle).ok()?;
    Some((
        info.framebuffer().map(u32::from),
        info.mode()
            .map(|mode| mode.name().to_string_lossy().into_owned()),
        info.position(),
    ))
}

#[test]
#[ignore = "opt-in live vkms KMS scanout: set OZMA_DRM_LIVE=1 and load vkms"]
fn vkms_scanout_pattern_verify_teardown() {
    if std::env::var("OZMA_DRM_LIVE").as_deref() != Ok("1") {
        eprintln!("skipping: set OZMA_DRM_LIVE=1 to run the live vkms scanout test");
        return;
    }

    let cards = enumerate_drm_cards().expect("enumerate DRM cards");
    let requested_card = std::env::var("OZMA_DRM_CARD").ok();
    let vkms = cards
        .into_iter()
        .find(|card| match requested_card.as_deref() {
            Some(path) => card.path == Path::new(path),
            None => card.is_vkms(),
        })
        .expect(
            "no vkms card found; run `sudo modprobe vkms` (or create a configfs instance) and \
             check /sys/class/drm/card*/device/driver",
        );
    assert!(
        vkms.is_vkms() || matches!(vkms.driver.as_str(), "vkms" | "faux_driver"),
        "live test refuses non-vkms card {} ({})",
        vkms.path.display(),
        vkms.driver
    );
    eprintln!(
        "vkms card: {} path={} driver={}",
        vkms.name,
        vkms.path.display(),
        vkms.driver
    );

    let lease_backend = RealDrmLeaseBackend::new();
    let inspected = lease_backend
        .inspect(&vkms)
        .expect("open/inspect the vkms card");
    eprintln!(
        "connectors: {:?}",
        inspected
            .connectors
            .iter()
            .map(|connector| (
                connector.name.as_str(),
                connector.object_id,
                connector.crtc_id,
                connector.status.clone(),
                connector.modes.clone()
            ))
            .collect::<Vec<_>>()
    );
    eprintln!("crtcs: {:?}", inspected.crtcs);
    let connector = inspected
        .connectors
        .iter()
        .find(|connector| {
            connector.object_id.is_some()
                && !matches!(connector.status, ozma_multiseat::DrmStatus::Disconnected)
        })
        .expect("vkms connector with object id");
    assert!(!inspected.crtcs.is_empty(), "vkms has no CRTC");

    let request = DrmScanoutRequest::new(DrmLeaseRequest::new(
        DrmCardSelector::Explicit(vkms.path.clone()),
        connector.name.clone(),
    ));
    let backend = RealDrmScanoutBackend::new();

    let mut handle = backend.start(&request).unwrap_or_else(|error| {
        panic!(
            "scanout on {}:{} failed: {error} (run as root or with DRM master/CAP_SYS_ADMIN; a \
             default vkms card is often mastered by systemd-logind)",
            vkms.path.display(),
            connector.name
        )
    });
    let status = handle.status().clone();
    eprintln!("scanout status: {status:#?}");
    assert!(status.applied, "scanout must be applied");
    assert!(status.framebuffer_id.is_some());
    assert!(status.buffer_handle.is_some());
    assert_eq!(status.connector, connector.name);
    assert_eq!(status.connector_id, connector.object_id);
    assert!(status.crtc_id.is_some());
    assert_eq!(status.pattern, ozma_multiseat::SCANOUT_PATTERN);
    assert!(status.pattern_checksum.is_some());
    assert!(status.page_flipped, "vkms should accept a page flip");
    let lessee_fd = status.lessee_fd.expect("lessee fd recorded");
    assert!(
        Path::new(&format!("/proc/self/fd/{lessee_fd}")).exists(),
        "lessee fd {lessee_fd} must be open while scanning out"
    );

    let verification = handle.verify().expect("verify live scanout");
    eprintln!("verification: {verification:#?}");
    assert!(
        verification.ok,
        "verification mismatch: {:?}",
        verification.mismatch
    );
    assert_eq!(
        verification.crtc_framebuffer, status.framebuffer_id,
        "CRTC must report our framebuffer"
    );
    assert_eq!(
        verification.crtc_mode.as_deref(),
        Some(status.mode.as_str())
    );
    assert_eq!(verification.connector_crtc, status.crtc_id);
    assert!(
        verification.framebuffer_buffer_handle.is_some(),
        "GETFB must report a backing buffer handle"
    );
    assert_eq!(verification.framebuffer_size, (status.width, status.height));
    assert_eq!(verification.buffer_size, (status.width, status.height));
    assert_eq!(
        verification.pattern_checksum,
        status.pattern_checksum.unwrap()
    );

    if let Some(crtc_id) = status.crtc_id {
        if let Some((framebuffer, mode, position)) = read_crtc(&vkms.path, crtc_id) {
            eprintln!(
                "independent GET_CRTC: crtc {crtc_id} framebuffer={framebuffer:?} \
                 mode={mode:?} position={position:?}"
            );
            assert_eq!(
                framebuffer, status.framebuffer_id,
                "a second card fd must see the proof framebuffer on the CRTC"
            );
            assert_eq!(mode.as_deref(), Some(status.mode.as_str()));
        } else {
            eprintln!("independent GET_CRTC evidence unavailable");
        }
    }

    let framebuffer_id = status.framebuffer_id;
    let crtc_id = status.crtc_id;
    handle.teardown().expect("teardown live scanout");
    let released = handle.status().clone();
    eprintln!("released status: {released:#?}");
    assert!(!released.applied);
    assert!(released.released);
    assert_eq!(released.lessee_fd, None);
    assert_eq!(released.framebuffer_id, framebuffer_id);
    assert!(
        !Path::new(&format!("/proc/self/fd/{lessee_fd}")).exists(),
        "lessee fd {lessee_fd} must be closed after teardown"
    );

    if let Some(crtc_id) = crtc_id {
        if let Some((framebuffer, mode, _)) = read_crtc(&vkms.path, crtc_id) {
            eprintln!(
                "post-teardown GET_CRTC: crtc {crtc_id} framebuffer={framebuffer:?} mode={mode:?}"
            );
            match framebuffer {
                None => {}
                Some(id) => assert_ne!(
                    Some(id),
                    framebuffer_id,
                    "the proof framebuffer must not remain on the CRTC after teardown"
                ),
            }
        }
    }

    // A second full cycle on the same objects proves teardown released the
    // lease, framebuffer, and dumb buffer rather than leaking them.
    let mut replacement = backend
        .start(&request)
        .expect("the same vkms connector/CRTC must be usable again");
    replacement
        .verify()
        .expect("replacement scanout verification");
    replacement.teardown().expect("replacement teardown");
    assert!(replacement.status().released);
    eprintln!("second scanout cycle completed and released");
}
