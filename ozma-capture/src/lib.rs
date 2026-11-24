// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-capture` — capture-source discovery, real V4L2 MMAP capture, the
//! deterministic fake path, and VM display providers.
//!
//! This crate is `unsafe`-free except for one isolated module. V4L2 discovery
//! reads `/dev` and sysfs without opening a device; opening a source uses the
//! safe [`v4l`] crate to query capabilities, negotiate a single-plane format,
//! and stream MMAP buffers. A dedicated worker thread owns the blocking
//! device/stream pair so the async [`ozma_media::CaptureSource`] remains
//! responsive. Tests use [`FakeCaptureProvider`] for CI; the real capture test
//! is opt-in because it needs `/dev/video*` hardware.
//!
//! The exception is `dbus_dmabuf`, which maps a QEMU DMA-BUF with
//! `memmap2`. That crate's mapping constructor is `unsafe` because a regular
//! file can be truncated while mapped. A DMA-BUF's size is fixed for the
//! lifetime of its export and the helper takes a read-only mapping, so the
//! call is justified and confined to that module; the rest of the crate stays
//! under `#![deny(unsafe_code)]`.
//!
//! On Linux the optional [`PipeWireCaptureProvider`] adds graph-native video
//! capture without replacing V4L2. [`CaptureRegistry::system`] remains
//! V4L2-only; [`CaptureRegistry::system_with_pipewire`] registers both.
//!
//! VM display paths live in provider modules that are not registered in
//! [`CaptureRegistry::system`] by default:
//!
//! * [`dbus_display`] is the low-latency VM path: it hands a socket to QEMU
//!   over QMP, authenticates the peer-to-peer D-Bus display, registers a
//!   listener with SCM_RIGHTS, and turns `Scanout`/`Update` pushes into owned
//!   BGRA frames ([`DBusDisplaySource`]). Register it with
//!   [`CaptureRegistry::with_dbus_display`] or from the VM profile.
//! * [`qmp_display`] polls QEMU's display through QMP `screendump`, parses the
//!   P6 PPM into owned RGB24 frames, and enforces frame-rate pacing
//!   ([`QmpDisplaySource`]). It is a polling path, not a real-time framebuffer
//!   subscription. Register it explicitly with
//!   [`CaptureRegistry::with_qmp_display`] or from the VM profile. When the
//!   node owns one [`ozma_qmp::SharedQmpClient`] for power/input/snapshots,
//!   use [`CaptureRegistry::with_qmp_display_shared`] instead so capture does
//!   not open a second QMP control connection.
//! * [`vnc_display`] reads QEMU's VNC display through ffmpeg's VNC input and
//!   returns packed RGB24 frames ([`VncDisplaySource`]). It is a transition
//!   path for hosts where the D-Bus display is unavailable; it needs no
//!   v4l2loopback device. Register it with
//!   [`CaptureRegistry::with_vnc_display`].
//! * [`lookingglass`] parses Looking Glass KVMFR shared memory into owned
//!   frames ([`LookingGlassSource`]). [`LookingGlassProvider`] advertises the
//!   region as [`CaptureKind::LookingGlass`] by reading only the KVMFR header;
//!   it never starts a capture during discovery. It needs no v4l2loopback
//!   device. Register it with [`CaptureRegistry::with_looking_glass`].
//! * `virtual` (raw identifier `r#virtual`, because `virtual` is reserved)
//!   owns the v4l2loopback + ffmpeg lifecycle that publishes a VM's VNC
//!   display as `/dev/videoN` ([`VirtualCapture`]). [`VirtualCaptureProvider`]
//!   discovers an existing device by its card label without loading a module
//!   or starting ffmpeg, and opens it through the lifecycle. Its capabilities
//!   use [`CaptureKind::V4l2`] because the device really is V4L2. Register it
//!   with [`CaptureRegistry::with_virtual_capture`].
//!
//! All of them report typed errors when their host dependencies are missing;
//! none silently fabricates a working source. Their registration belongs to
//! the VM profile workstream.

#![deny(unsafe_code)]

pub mod dbus_display;
// The DMA-BUF importer isolates the one `unsafe` operation in this crate:
// `memmap2`'s read-only mapping constructor. See its module documentation.
#[allow(unsafe_code)]
mod dbus_dmabuf;
pub mod fake;
pub mod lookingglass;
#[cfg(target_os = "linux")]
pub mod pipewire;
pub mod provider;
pub mod qmp_display;
pub mod v4l2;
// `virtual` is a reserved Rust keyword; the raw identifier keeps the intended
// module name and file (`virtual.rs`) while still compiling.
pub mod r#virtual;
pub mod vnc_display;

pub use dbus_display::{
    DBusDisplayConfig, DBusDisplayProvider, DBusDisplaySource, DBusError, DBusHeader, DBusMessage,
    DBusTransport, DBusValue, Framebuffer, UnixDBusTransport, DBUS_ADD_CLIENT_PROTOCOL,
    DBUS_CONSOLE_INTERFACE, DBUS_CONSOLE_PATH_PREFIX, DBUS_DEFAULT_CONNECT_TIMEOUT,
    DBUS_DEFAULT_CONSOLE_INDEX, DBUS_DEFAULT_FPS, DBUS_DEFAULT_HEIGHT, DBUS_DEFAULT_ID,
    DBUS_DEFAULT_NAME, DBUS_DEFAULT_WIDTH, DBUS_FD_NAME, DBUS_LISTENER_INTERFACE,
    DBUS_PROPERTIES_INTERFACE, MAX_DBUS_BODY_BYTES, MAX_DBUS_DIMENSION,
};
pub use fake::FakeCaptureProvider;
pub use lookingglass::{
    extract_frame, frame_format, parse_frame_header, parse_header, requires_conversion,
    validate_frame_bounds, KvmfrFrameHeader, KvmfrHeader, LookingGlassProvider,
    LookingGlassProviderConfig, LookingGlassSource, LOOKING_GLASS_DEFAULT_FPS,
    LOOKING_GLASS_DEFAULT_ID, LOOKING_GLASS_DEFAULT_NAME, LOOKING_GLASS_DEFAULT_SHM_PATH,
};
#[cfg(target_os = "linux")]
pub use pipewire::{
    capability_for, capability_id, map_pipewire_error, target_for_capability, target_matches,
    PipeWireCaptureProvider, PipeWireCaptureSource, PipeWireOptions,
};
pub use provider::{CaptureProvider, CaptureRegistry, CaptureScan};
pub use qmp_display::{
    parse_ppm_p6, QmpDisplayConfig, QmpDisplayProvider, QmpDisplaySource, MAX_PPM_DIMENSION,
    QMP_DEFAULT_ID, QMP_DEFAULT_MAX_FPS, QMP_DEFAULT_MAX_HEIGHT, QMP_DEFAULT_MAX_WIDTH,
    QMP_DEFAULT_NAME,
};
pub use r#virtual::{
    bounded_fps, next_device_number, ChildId, CommandOutput, CommandRunner, ProcessCommandRunner,
    V4l2SourceFactory, VirtualCapture, VirtualCaptureConfig, VirtualCaptureProvider,
    VirtualCaptureProviderConfig, VirtualCaptureSource, VirtualSourceFactory,
    VIRTUAL_CAPTURE_DEFAULT_DEV_ROOT, VIRTUAL_CAPTURE_DEFAULT_SYS_ROOT,
};
pub use v4l2::{
    choose_resolution, discover_v4l2, fourcc_to_pixel_format, h264_access_unit_contains_idr,
    is_capture_like_name, keyframe_for, monotonic_pts, next_sequence, parse_video_index,
    preferred_fourcc, timestamp_micros, V4l2Capture, V4l2Node, V4l2Provider, DEFAULT_FRAME_TIMEOUT,
    DEFAULT_MAX_HEIGHT, DEFAULT_MAX_WIDTH, PREFERRED_FOURCCS,
};
pub use vnc_display::{
    ffmpeg_argv, ProcessVncSpawner, VncChild, VncDisplayConfig, VncDisplayProvider,
    VncDisplaySource, VncSpawner, MAX_STDERR_BYTES, MAX_VNC_DIMENSION, VNC_DEFAULT_CONNECT_TIMEOUT,
    VNC_DEFAULT_FFMPEG_BIN, VNC_DEFAULT_FPS, VNC_DEFAULT_HEIGHT, VNC_DEFAULT_HOST, VNC_DEFAULT_ID,
    VNC_DEFAULT_NAME, VNC_DEFAULT_PORT, VNC_DEFAULT_WIDTH,
};

/// Re-export of the capture vocabulary callers need alongside this crate.
pub use ozma_protocol::{CaptureCapability, CaptureKind};

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_media::{CaptureSource, MediaError};
    use ozma_protocol::{codes, ApiError};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct BrokenProvider {
        calls: Arc<AtomicUsize>,
    }

    impl CaptureProvider for BrokenProvider {
        fn kind(&self) -> CaptureKind {
            CaptureKind::Kms
        }

        fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Err(ApiError::new(
                codes::CAPTURE_UNAVAILABLE,
                "KMS discovery failed",
            ))
        }

        fn open(
            &self,
            _capability: &CaptureCapability,
        ) -> Result<Box<dyn CaptureSource>, ApiError> {
            Err(ApiError::new(codes::NOT_FOUND, "nothing to open"))
        }
    }

    #[tokio::test]
    async fn fake_provider_is_a_real_registry_path() {
        let mut registry = CaptureRegistry::new();
        registry.register(
            FakeCaptureProvider::new()
                .with_frame_limit(2)
                .with_fps(1_000),
        );

        let capabilities = registry.enumerate().unwrap();
        assert_eq!(capabilities.len(), 1);
        assert_eq!(capabilities[0].id, "fake0");
        assert_eq!(capabilities[0].kind, CaptureKind::Fake);

        let mut source = registry.open(&capabilities[0]).unwrap();
        let first = source.next_frame().await.unwrap();
        let second = source.next_frame().await.unwrap();
        assert!(!first.is_empty());
        assert_eq!(first.sequence(), 0);
        assert_eq!(second.sequence(), 1);
        assert!(second.pts_micros() > first.pts_micros());
        assert_eq!(
            source.next_frame().await.unwrap_err(),
            MediaError::EndOfStream
        );
    }

    #[test]
    fn open_id_rejects_unknown_sources() {
        let registry = CaptureRegistry::new().with(FakeCaptureProvider::new());
        let error = registry.open_id("missing").err().unwrap();
        assert_eq!(error.code, codes::NOT_FOUND);
    }

    #[test]
    fn scan_surfaces_provider_errors_without_hiding_other_sources() {
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = CaptureRegistry::new()
            .with(BrokenProvider {
                calls: Arc::clone(&calls),
            })
            .with(FakeCaptureProvider::new());

        let scans = registry.scan();
        assert_eq!(scans.len(), 2);
        assert_eq!(scans[0].kind, CaptureKind::Kms);
        assert!(scans[0].error.is_some());
        assert!(scans[1].is_ok());
        assert_eq!(scans[1].capabilities.len(), 1);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // The all-or-nothing view reports the failure instead of dropping it.
        let error = registry.enumerate().unwrap_err();
        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
    }

    #[test]
    fn system_registry_starts_with_v4l2_discovery() {
        let registry = CaptureRegistry::system();
        assert_eq!(registry.provider_count(), 1);
        // Discovery on the build host is allowed to return any number of
        // devices, including none; it must not error for a normal host.
        assert!(registry.enumerate().is_ok());
    }

    #[test]
    fn system_with_pipewire_registers_both_backends() {
        let registry = CaptureRegistry::system_with_pipewire();
        #[cfg(target_os = "linux")]
        assert_eq!(registry.provider_count(), 2);
        #[cfg(not(target_os = "linux"))]
        assert_eq!(registry.provider_count(), 1);
        // V4L2 must still be present; a missing PipeWire session is reported
        // as an empty source list, not a discovery failure.
        let scans = registry.scan();
        assert!(scans.iter().any(|scan| scan.kind == CaptureKind::V4l2));
        #[cfg(target_os = "linux")]
        {
            let pipewire = scans
                .iter()
                .find(|scan| scan.kind == CaptureKind::PipeWire)
                .expect("PipeWire provider is registered");
            assert_eq!(pipewire.kind, CaptureKind::PipeWire);
            assert!(
                pipewire.is_ok(),
                "a missing PipeWire session must not fail the scan: {:?}",
                pipewire.error
            );
        }
    }

    #[test]
    fn qmp_display_is_opt_in() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("qmp.sock");
        std::fs::write(&socket, b"placeholder").unwrap();

        let registry =
            CaptureRegistry::new().with_qmp_display(QmpDisplayConfig::new(socket.as_path()));
        assert_eq!(registry.provider_count(), 1);

        let scans = registry.scan();
        assert_eq!(scans[0].kind, CaptureKind::Qmp);
        assert_eq!(scans[0].capabilities.len(), 1);
        assert_eq!(scans[0].capabilities[0].id, "qmp0");

        // Hardware/system registries must not grow a QMP provider implicitly.
        assert!(CaptureRegistry::system()
            .scan()
            .iter()
            .all(|scan| scan.kind != CaptureKind::Qmp));
    }

    #[test]
    fn looking_glass_and_virtual_capture_are_opt_in() {
        let directory = tempfile::tempdir().unwrap();

        let looking_glass = CaptureRegistry::new().with_looking_glass(
            LookingGlassProviderConfig::new(directory.path().join("absent-kvmfr")),
        );
        assert_eq!(looking_glass.provider_count(), 1);
        let scans = looking_glass.scan();
        assert_eq!(scans[0].kind, CaptureKind::LookingGlass);
        assert!(scans[0].capabilities.is_empty());

        let sys_root = directory.path().join("sys");
        std::fs::create_dir_all(sys_root.join("video12")).unwrap();
        std::fs::write(sys_root.join("video12/name"), "Ozma Virtual vm1\n").unwrap();
        let mut capture = VirtualCaptureConfig::new("vm1");
        capture.device_number = Some(12);
        let virtual_capture = CaptureRegistry::new().with_virtual_capture(
            VirtualCaptureProviderConfig::new(capture)
                .with_roots(std::path::PathBuf::from("/dev"), sys_root.clone()),
        );
        assert_eq!(virtual_capture.provider_count(), 1);
        let scans = virtual_capture.scan();
        assert_eq!(scans[0].kind, CaptureKind::V4l2);
        assert_eq!(scans[0].capabilities.len(), 1);
        assert_eq!(scans[0].capabilities[0].id, "/dev/video12");

        // Neither VM provider is registered implicitly on a hardware node.
        assert!(CaptureRegistry::system()
            .scan()
            .iter()
            .all(|scan| scan.kind != CaptureKind::LookingGlass));
    }

    #[test]
    fn vnc_display_is_opt_in_and_registry_driven() {
        let registry = CaptureRegistry::new()
            .with_vnc_display(VncDisplayConfig::new("127.0.0.1", 5901).with_dimensions(640, 480));
        assert_eq!(registry.provider_count(), 1);
        let scans = registry.scan();
        assert_eq!(scans[0].kind, CaptureKind::Vnc);
        assert_eq!(scans[0].capabilities.len(), 1);
        assert_eq!(scans[0].capabilities[0].id, VNC_DEFAULT_ID);
        assert_eq!(scans[0].capabilities[0].formats, vec!["rgb24".to_owned()]);

        assert!(CaptureRegistry::system()
            .scan()
            .iter()
            .all(|scan| scan.kind != CaptureKind::Vnc));
        assert!(CaptureRegistry::system_with_pipewire()
            .scan()
            .iter()
            .all(|scan| scan.kind != CaptureKind::Vnc));
    }
}
