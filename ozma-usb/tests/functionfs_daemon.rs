// SPDX-License-Identifier: AGPL-3.0-only
//! FunctionFS daemon core: descriptor encoding, ep0 control, enable/disable,
//! and injectable endpoint I/O.
//!
//! Default tests use a temporary directory and in-memory endpoints. They never
//! mount FunctionFS, open a UDC, or require root. The one real-mount test is
//! `#[ignore]` and environment-gated.

use std::fs;
use std::path::PathBuf;

use ozma_usb::{
    handle_control_request, parse_descriptor_blob, parse_string_blob, FakeFunctionFsDaemon,
    FileFunctionFsDaemon, FunctionFsConfig, FunctionFsControlRequest, FunctionFsDaemon,
    FunctionFsDescriptors, FunctionFsEndpoint, FunctionFsEvent, FunctionFsMount,
    FunctionFsOsDescriptor, FunctionFsOsProperty, FunctionFsReadiness, FunctionFsStatus,
    FunctionFsStrings, LoopbackFunctionFsDaemon, FUNCTIONFS_DESCRIPTORS_MAGIC_V2,
    FUNCTIONFS_HAS_FS_DESC, FUNCTIONFS_HAS_HS_DESC, FUNCTIONFS_HAS_MS_OS_DESC,
    FUNCTIONFS_STRINGS_MAGIC, MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES,
};

fn vendor_config() -> FunctionFsConfig {
    FunctionFsConfig::new("vendor0", 0x1209, 0x0001)
        .with_interface(0xff, 0x42, 0x01)
        .with_endpoint(FunctionFsEndpoint::bulk_out(1, 512))
        .with_endpoint(FunctionFsEndpoint::bulk_in(2, 512))
        .with_class_descriptor(vec![4, 0x24, 0x01, 0x00])
        .with_os_descriptor(FunctionFsOsDescriptor::compatible_id("WINUSB", ""))
        .with_os_descriptor(FunctionFsOsDescriptor::properties([
            FunctionFsOsProperty::new("DeviceInterfaceGUID", b"{ozma-test-guid}".to_vec()),
            FunctionFsOsProperty::new("DriverVersion", b"1".to_vec()).with_data_type(7),
        ]))
        .with_strings(FunctionFsStrings {
            manufacturer: "Ozma Labs".to_owned(),
            product: "Ozma Vendor".to_owned(),
            serial_number: "OZMA-VENDOR-1".to_owned(),
            configuration: "Vendor Configuration".to_owned(),
        })
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn prepared_daemon() -> (FunctionFsConfig, LoopbackFunctionFsDaemon) {
    let config = FunctionFsConfig::new("vendor0", 0x1209, 0x0001)
        .with_interface(0xff, 0x42, 0x01)
        .with_endpoint(FunctionFsEndpoint::bulk_out(1, 512))
        .with_endpoint(FunctionFsEndpoint::bulk_in(1, 512));
    let descriptors = config.ep0_descriptors().unwrap();
    let mut daemon = FakeFunctionFsDaemon::new(config.clone()).unwrap();
    daemon.write_descriptors(&config, &descriptors).unwrap();
    (config, daemon)
}

#[test]
fn descriptor_blob_layout_alignment_and_strings() {
    let config = vendor_config();
    let descriptors = FunctionFsDescriptors::from_config(&config).unwrap();
    let blob = descriptors.descriptor_blob().unwrap();

    assert_eq!(u32_at(&blob, 0), FUNCTIONFS_DESCRIPTORS_MAGIC_V2);
    assert_eq!(u32_at(&blob, 4), blob.len() as u32);
    let flags = u32_at(&blob, 8);
    assert_eq!(
        flags,
        FUNCTIONFS_HAS_FS_DESC | FUNCTIONFS_HAS_HS_DESC | FUNCTIONFS_HAS_MS_OS_DESC
    );
    // Count words start at byte 12 and are 4-byte aligned.
    assert_eq!(u32_at(&blob, 12), descriptors.full_speed_count());
    assert_eq!(u32_at(&blob, 16), descriptors.high_speed_count());
    assert_eq!(u32_at(&blob, 20), descriptors.os_count());
    assert_eq!(descriptors.full_speed_count(), 4); // interface + class + 2 endpoints
    assert_eq!(descriptors.os_count(), 2);

    let parsed = parse_descriptor_blob(&blob).unwrap();
    assert_eq!(parsed.fs, descriptors.full_speed());
    assert_eq!(parsed.hs, descriptors.high_speed());
    assert_eq!(parsed.fs_count, descriptors.full_speed_count());
    assert_eq!(parsed.hs_count, descriptors.high_speed_count());
    assert_eq!(parsed.os_count, 2);
    // Full-speed descriptors are function-scoped: interface, class, endpoints.
    assert_eq!(parsed.fs[0], 9);
    assert_eq!(parsed.fs[1], 0x04);
    assert_eq!(parsed.fs[9], 4);
    assert_eq!(parsed.fs[10], 0x24);
    assert_eq!(parsed.fs[15], 0x01); // bulk OUT address
    assert_eq!(parsed.fs[22], 0x82); // bulk IN address
    assert_eq!(
        u16::from_le_bytes([parsed.fs[17], parsed.fs[18]]),
        64,
        "full-speed bulk packets are clamped to 64 bytes"
    );
    assert_eq!(
        u16::from_le_bytes([parsed.hs[17], parsed.hs[18]]),
        512,
        "high-speed bulk packets keep the configured value"
    );

    let strings = descriptors.strings();
    assert_eq!(u32_at(strings, 0), FUNCTIONFS_STRINGS_MAGIC);
    assert_eq!(u32_at(strings, 4), strings.len() as u32);
    assert_eq!(u32_at(strings, 8), 4); // manufacturer/product/serial/configuration
    assert_eq!(u32_at(strings, 12), 1); // one language table
    assert_eq!(u16::from_le_bytes([strings[16], strings[17]]), 0x0409);
    assert_eq!(
        parse_string_blob(strings).unwrap(),
        vec![
            "Ozma Labs",
            "Ozma Vendor",
            "OZMA-VENDOR-1",
            "Vendor Configuration",
        ]
    );

    let mut written = Vec::new();
    let report = descriptors.write_to(&mut written).unwrap();
    assert_eq!(report.descriptor_bytes, blob.len());
    assert_eq!(report.string_bytes, strings.len());
    assert_eq!(report.total(), written.len());
    assert_eq!(&written[..blob.len()], blob.as_slice());
    assert_eq!(&written[blob.len()..], strings);
}

#[test]
fn temp_mount_requires_ep0_and_writes_both_chunks() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("ffs-vendor0");
    let missing = FunctionFsMount::new(&root);
    assert_eq!(
        missing.validate().unwrap_err().kind(),
        "missing-functionfs-mount"
    );

    fs::create_dir(&root).unwrap();
    let mount = FunctionFsMount::new(&root);
    assert_eq!(
        mount.validate().unwrap_err().kind(),
        "missing-functionfs-ep0"
    );

    fs::write(root.join("ep0"), b"").unwrap();
    mount.validate().unwrap();

    let descriptors = vendor_config().ep0_descriptors().unwrap();
    let report = mount.write_descriptors(&descriptors).unwrap();
    let writes = fs::read(root.join("ep0")).unwrap();
    assert_eq!(writes.len(), report.total());
    assert_eq!(
        &writes[..report.descriptor_bytes],
        descriptors.descriptor_blob().unwrap()
    );
    assert_eq!(&writes[report.descriptor_bytes..], descriptors.strings());

    // A regular path is not accepted as a mount root.
    let file = temp.path().join("not-a-dir");
    fs::write(&file, b"").unwrap();
    assert_eq!(
        FunctionFsMount::new(&file).validate().unwrap_err().kind(),
        "missing-functionfs-mount"
    );
}

#[test]
fn ep0_control_requests_are_typed_and_bounded() {
    let config = vendor_config();
    let device = FunctionFsControlRequest {
        bm_request_type: 0x80,
        b_request: 6,
        w_value: 0x0100,
        w_index: 0,
        w_length: 64,
    };
    let response = handle_control_request(&config, device, false, 0).unwrap();
    assert_eq!(response.data.len(), 18);
    assert_eq!(response.data[0], 18);
    assert_eq!(response.data[1], 0x01);

    let configuration = FunctionFsControlRequest {
        w_value: 0x0200,
        w_length: 4,
        ..device
    };
    let response = handle_control_request(&config, configuration, false, 0).unwrap();
    assert_eq!(response.data.len(), 4); // truncated to wLength
    assert_eq!(response.data[1], 0x02);

    let string = FunctionFsControlRequest {
        w_value: 0x0302,
        w_length: 255,
        ..device
    };
    let response = handle_control_request(&config, string, false, 0).unwrap();
    assert_eq!(response.data[1], 0x03);
    assert_eq!(
        &response.data[2..],
        &"Ozma Vendor"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()[..]
    );

    let set_configuration = FunctionFsControlRequest {
        bm_request_type: 0x00,
        b_request: 9,
        w_value: 1,
        w_index: 0,
        w_length: 0,
    };
    assert_eq!(
        handle_control_request(&config, set_configuration, false, 0)
            .unwrap()
            .configured,
        Some(1)
    );

    let get_configuration = FunctionFsControlRequest {
        bm_request_type: 0x80,
        b_request: 8,
        w_value: 0,
        w_index: 0,
        w_length: 1,
    };
    let response = handle_control_request(&config, get_configuration, true, 0).unwrap();
    assert_eq!(response.data, vec![1]);

    let bad_string = FunctionFsControlRequest {
        w_value: 0x03ff,
        ..device
    };
    assert_eq!(
        handle_control_request(&config, bad_string, false, 0)
            .unwrap_err()
            .kind(),
        "unsupported-functionfs-control"
    );

    let vendor = FunctionFsControlRequest {
        bm_request_type: 0x40,
        b_request: 1,
        w_value: 0,
        w_index: 0,
        w_length: 0,
    };
    assert_eq!(
        handle_control_request(&config, vendor, false, 0)
            .unwrap_err()
            .kind(),
        "unsupported-functionfs-control"
    );

    let unknown = FunctionFsControlRequest {
        b_request: 0x55,
        ..device
    };
    assert_eq!(
        handle_control_request(&config, unknown, false, 0)
            .unwrap_err()
            .kind(),
        "unsupported-functionfs-control"
    );
}

#[test]
fn enable_disable_live_status_and_ep0_events() {
    let config = vendor_config();
    let descriptors = config.ep0_descriptors().unwrap();
    let mut daemon = FakeFunctionFsDaemon::new(config.clone()).unwrap();

    let error = daemon
        .handle_event(&config, FunctionFsEvent::Enable)
        .unwrap_err();
    assert_eq!(error.kind(), "invalid-functionfs-state");

    let report = daemon.write_descriptors(&config, &descriptors).unwrap();
    assert_eq!(
        report.descriptor_bytes,
        descriptors.descriptor_blob().unwrap().len()
    );
    let before_enable = daemon.status(&config);
    let state = before_enable.daemon.as_ref().unwrap();
    assert!(state.ep0_written);
    assert!(state.strings_written);
    assert!(!state.enabled);
    assert!(!state.live);
    assert!(!before_enable.ready);

    let outcome = daemon
        .handle_event(&config, FunctionFsEvent::Enable)
        .unwrap();
    assert!(outcome.enabled);
    assert!(outcome.live);
    let status = daemon.status(&config);
    assert!(status.ready);
    assert_eq!(status.readiness, FunctionFsReadiness::Ready);
    let state = status.daemon.as_ref().unwrap();
    assert!(state.live);
    assert_eq!(state.open_endpoints, 2);
    assert_eq!(state.os_descriptors, 2);

    // Enable is acknowledged with a zero-length ep0 write.
    assert_eq!(daemon.ep0_writes().len(), 3);
    assert!(daemon.ep0_writes()[2].is_empty());

    let get_device = FunctionFsControlRequest {
        bm_request_type: 0x80,
        b_request: 6,
        w_value: 0x0100,
        w_index: 0,
        w_length: 18,
    };
    daemon.push_event(FunctionFsEvent::Setup(get_device));
    let outcome = daemon.handle_next_event().unwrap().unwrap();
    assert_eq!(outcome.response.as_ref().unwrap().len(), 18);
    assert_eq!(daemon.ep0_writes().len(), 4);
    assert_eq!(daemon.ep0_writes()[3].len(), 18);

    let set_configuration = FunctionFsControlRequest {
        bm_request_type: 0x00,
        b_request: 9,
        w_value: 1,
        w_index: 0,
        w_length: 0,
    };
    daemon.push_event(FunctionFsEvent::Setup(set_configuration));
    let outcome = daemon.handle_next_event().unwrap().unwrap();
    assert!(outcome.configured);
    assert!(outcome.live);
    assert!(daemon.ep0_writes()[4].is_empty());

    daemon.push_event(FunctionFsEvent::Disable);
    let outcome = daemon.handle_next_event().unwrap().unwrap();
    assert!(!outcome.enabled);
    assert!(!outcome.live);
    let status = daemon.status(&config);
    assert!(!status.daemon.as_ref().unwrap().live);
    assert_eq!(status.daemon.as_ref().unwrap().open_endpoints, 0);

    // Re-enable and shut down explicitly; shutdown never leaves `live` set.
    daemon
        .handle_event(&config, FunctionFsEvent::Enable)
        .unwrap();
    assert!(daemon.status(&config).daemon.as_ref().unwrap().live);
    daemon.shutdown();
    assert!(!daemon.status(&config).daemon.as_ref().unwrap().live);
    assert!(!daemon.status(&config).daemon.as_ref().unwrap().enabled);
}

#[test]
fn endpoint_read_write_loopback_and_bounds() {
    let (config, mut daemon) = prepared_daemon();
    daemon
        .handle_event(&config, FunctionFsEvent::Enable)
        .unwrap();

    daemon.host_write(0x01, b"hello").unwrap();
    assert_eq!(daemon.core_mut().read_endpoint(0x01, 32).unwrap(), b"hello");
    assert!(daemon
        .core_mut()
        .read_endpoint(0x01, 32)
        .unwrap()
        .is_empty());

    daemon.core_mut().write_endpoint(0x81, b"world").unwrap();
    assert_eq!(daemon.host_read(0x81).unwrap(), b"world");

    let wrong_read = daemon.core_mut().read_endpoint(0x81, 8).unwrap_err();
    assert_eq!(wrong_read.kind(), "invalid-functionfs-state");
    let wrong_write = daemon.core_mut().write_endpoint(0x01, b"x").unwrap_err();
    assert_eq!(wrong_write.kind(), "invalid-functionfs-state");

    daemon.host_write(0x01, b"echo").unwrap();
    let transfers = daemon.loopback_once(64).unwrap();
    assert_eq!(transfers.len(), 1);
    assert_eq!(transfers[0].address, 0x01);
    assert_eq!(transfers[0].data, b"echo");
    assert_eq!(daemon.host_read(0x81).unwrap(), b"echo");

    let oversized = vec![0u8; MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES + 1];
    assert_eq!(
        daemon.host_write(0x01, &oversized).unwrap_err().kind(),
        "functionfs-buffer-too-large"
    );
    assert_eq!(
        daemon
            .core_mut()
            .write_endpoint(0x81, &oversized)
            .unwrap_err()
            .kind(),
        "functionfs-buffer-too-large"
    );
    assert_eq!(
        daemon.core_mut().read_endpoint(0x01, 0).unwrap_err().kind(),
        "functionfs-buffer-too-large"
    );

    // Buffers are not available before enable.
    let mut fresh = FakeFunctionFsDaemon::new(config.clone()).unwrap();
    assert!(
        fresh.core_mut().endpoint_mut(0x01).is_none(),
        "endpoints are opened only on enable"
    );
}

#[test]
fn malformed_descriptors_and_oversized_control_are_typed() {
    let config = vendor_config();
    let descriptors = config.ep0_descriptors().unwrap();
    let blob = descriptors.descriptor_blob().unwrap();

    assert_eq!(
        parse_descriptor_blob(&[]).unwrap_err().kind(),
        "invalid-functionfs-descriptor"
    );

    let mut bad_magic = blob.clone();
    bad_magic[0] = 0;
    assert_eq!(
        parse_descriptor_blob(&bad_magic).unwrap_err().kind(),
        "invalid-functionfs-descriptor"
    );

    let mut bad_length = blob.clone();
    bad_length[4] = 0;
    assert_eq!(
        parse_descriptor_blob(&bad_length).unwrap_err().kind(),
        "invalid-functionfs-descriptor"
    );

    let mut bad_b_length = blob.clone();
    let fs_offset = 12 + 3 * 4; // three count words for FS, HS, OS
    bad_b_length[fs_offset] = 1;
    assert_eq!(
        parse_descriptor_blob(&bad_b_length).unwrap_err().kind(),
        "invalid-functionfs-descriptor"
    );

    let mut mismatched_counts = blob.clone();
    mismatched_counts[12] = mismatched_counts[12].saturating_sub(1);
    assert_eq!(
        parse_descriptor_blob(&mismatched_counts)
            .unwrap_err()
            .kind(),
        "invalid-functionfs-descriptor"
    );

    let mut reserved_descriptor = blob.clone();
    reserved_descriptor[fs_offset + 1] = 0x02; // CONFIGURATION is gadget-owned
    assert_eq!(
        parse_descriptor_blob(&reserved_descriptor)
            .unwrap_err()
            .kind(),
        "invalid-functionfs-descriptor"
    );

    let bad_class = vendor_config().with_class_descriptor(vec![5, 0x24]);
    assert_eq!(bad_class.validate().unwrap_err().kind(), "invalid-config");

    let bad_os = vendor_config()
        .with_os_descriptor(FunctionFsOsDescriptor::compatible_id("TOO-LONG-ID", ""));
    assert_eq!(bad_os.validate().unwrap_err().kind(), "invalid-config");

    let mut bad_strings = descriptors.strings().to_vec();
    let last = bad_strings.len() - 1;
    bad_strings[last] = b'x';
    assert_eq!(
        parse_string_blob(&bad_strings).unwrap_err().kind(),
        "invalid-functionfs-descriptor"
    );

    // Descriptor writes are single-shot in FunctionFS's ep0 state machine.
    let mut daemon = FakeFunctionFsDaemon::new(config.clone()).unwrap();
    daemon.write_descriptors(&config, &descriptors).unwrap();
    assert_eq!(
        daemon
            .write_descriptors(&config, &descriptors)
            .unwrap_err()
            .kind(),
        "invalid-functionfs-state"
    );
}

#[test]
fn file_daemon_opens_ep0_eagerly_and_endpoints_on_enable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("ffs-vendor0");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("ep0"), b"").unwrap();

    let config = vendor_config();
    let mount = FunctionFsMount::new(&root);
    let mut daemon = FileFunctionFsDaemon::open(config.clone(), mount.clone()).unwrap();
    let descriptors = config.ep0_descriptors().unwrap();

    let before = daemon.status(&config);
    assert!(!before.daemon.as_ref().unwrap().live);
    daemon.write_descriptors(&config, &descriptors).unwrap();
    assert_eq!(
        fs::read(root.join("ep0")).unwrap().len(),
        descriptors.descriptor_blob().unwrap().len() + descriptors.strings().len()
    );

    // FunctionFS creates ep<N> only after descriptors; this test creates them
    // on disk to model that point.
    fs::write(root.join("ep1"), b"").unwrap();
    fs::write(root.join("ep2"), b"").unwrap();
    daemon
        .handle_event(&config, FunctionFsEvent::Enable)
        .unwrap();
    let status = daemon.status(&config);
    assert!(status.daemon.as_ref().unwrap().live);
    assert_eq!(status.daemon.as_ref().unwrap().open_endpoints, 2);

    daemon.shutdown();
    assert!(!daemon.status(&config).daemon.as_ref().unwrap().live);
}

#[test]
fn file_daemon_reports_a_missing_endpoint_on_enable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("ffs-vendor0");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("ep0"), b"").unwrap();
    fs::write(root.join("ep1"), b"").unwrap();

    let config = vendor_config();
    let mut daemon =
        FileFunctionFsDaemon::open(config.clone(), FunctionFsMount::new(&root)).unwrap();
    let descriptors = config.ep0_descriptors().unwrap();
    daemon.write_descriptors(&config, &descriptors).unwrap();
    let error = daemon
        .handle_event(&config, FunctionFsEvent::Enable)
        .unwrap_err();
    assert_eq!(error.kind(), "missing-functionfs-endpoint");
    assert!(!daemon.status(&config).daemon.as_ref().unwrap().live);
}

#[test]
fn events_round_trip_and_status_shapes_remain_optional() {
    let requests = [
        FunctionFsEvent::Bind,
        FunctionFsEvent::Unbind,
        FunctionFsEvent::Enable,
        FunctionFsEvent::Disable,
        FunctionFsEvent::Setup(FunctionFsControlRequest {
            bm_request_type: 0x80,
            b_request: 6,
            w_value: 0x0100,
            w_index: 0,
            w_length: 18,
        }),
        FunctionFsEvent::Suspend,
        FunctionFsEvent::Resume,
    ];
    for event in requests {
        let encoded = event.encode();
        assert_eq!(FunctionFsEvent::decode(&encoded).unwrap(), event);
    }

    let status = FunctionFsStatus::from_config(&vendor_config());
    assert!(status.daemon.is_none());
    let json = serde_json::to_value(&status).unwrap();
    assert!(json.get("daemon").is_none());
    assert_eq!(
        serde_json::to_value(FunctionFsStatus::unobserved("vendor0")).unwrap()["readiness"],
        "unknown"
    );
}

#[test]
#[ignore = "requires an existing FunctionFS mount; set OZMA_FUNCTIONFS_MOUNT"]
fn real_functionfs_mount_accepts_descriptor_blob() {
    let Some(root) = std::env::var_os("OZMA_FUNCTIONFS_MOUNT") else {
        return;
    };
    let instance =
        std::env::var("OZMA_FUNCTIONFS_INSTANCE").unwrap_or_else(|_| "ozma-test".to_owned());
    let config = FunctionFsConfig::new(instance, 0x1209, 0x0001)
        .with_interface(0xff, 0x42, 0x01)
        .with_endpoint(FunctionFsEndpoint::bulk_out(1, 512))
        .with_endpoint(FunctionFsEndpoint::bulk_in(2, 512));
    let mount = FunctionFsMount::new(PathBuf::from(root));
    let mut daemon = FileFunctionFsDaemon::open(config.clone(), mount).unwrap();
    let descriptors = config.ep0_descriptors().unwrap();
    daemon.write_descriptors(&config, &descriptors).unwrap();

    // A real UDC enable event is not forced here: without a connected host the
    // daemon must not claim to be live.
    let status = daemon.status(&config);
    let state = status.daemon.as_ref().unwrap();
    assert!(state.ep0_written);
    assert!(state.strings_written);
    assert!(!state.live);
    drop(daemon);
}
