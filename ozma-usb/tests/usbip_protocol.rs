// SPDX-License-Identifier: AGPL-3.0-only
//! USB/IP control-codec unit vectors: byte order, fixed widths, counts, and
//! typed rejection of bad versions/opcodes/truncated/oversized frames.

use ozma_usb::usbip_protocol::{
    UsbIpDevListReply, UsbIpDevListRequest, UsbIpDeviceDescriptor, UsbIpImportReply,
    UsbIpImportRequest, UsbIpInterface, UsbIpOpCode, UsbIpOpHeader, UsbIpProtocolError,
    UsbIpStatus, OP_REP_DEVLIST, OP_REP_IMPORT, OP_REQ_DEVLIST, OP_REQ_IMPORT, USBIP_BUSID_BYTES,
    USBIP_DEVICE_BYTES, USBIP_DEVLIST_HEADER_BYTES, USBIP_IMPORT_REPLY_BYTES,
    USBIP_IMPORT_REQUEST_BYTES, USBIP_MAX_FRAME_BYTES, USBIP_MAX_INTERFACES, USBIP_OP_HEADER_BYTES,
    USBIP_PATH_BYTES, USBIP_PROTOCOL_VERSION,
};
use ozma_usb::UsbError;

fn descriptor(bus_id: &str) -> UsbIpDeviceDescriptor {
    UsbIpDeviceDescriptor::new(format!("/sys/devices/pci0000:00/usb1/{bus_id}"), bus_id)
        .with_numbers(1, 2)
        .with_speed(3)
        .with_ids(0x1d6b, 0x0002)
        .with_bcd_device(0x0510)
        .with_device_class(9, 0, 3)
        .with_configuration(1, 1)
        .with_interfaces(vec![
            UsbIpInterface::new(9, 0, 0),
            UsbIpInterface::new(3, 1, 2),
        ])
}

fn header_bytes(version: u16, code: u16, status: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&version.to_be_bytes());
    bytes.extend_from_slice(&code.to_be_bytes());
    bytes.extend_from_slice(&status.to_be_bytes());
    bytes
}

#[test]
fn layout_constants_match_the_kernel_records() {
    assert_eq!(USBIP_PROTOCOL_VERSION, 0x0111);
    assert_eq!(USBIP_OP_HEADER_BYTES, 8);
    assert_eq!(USBIP_PATH_BYTES, 256);
    assert_eq!(USBIP_BUSID_BYTES, 32);
    assert_eq!(USBIP_DEVICE_BYTES, 312);
    assert_eq!(USBIP_DEVLIST_HEADER_BYTES, 12);
    assert_eq!(USBIP_IMPORT_REQUEST_BYTES, 40);
    assert_eq!(USBIP_IMPORT_REPLY_BYTES, 320);
    assert_eq!(USBIP_MAX_INTERFACES, 32);
    assert!(USBIP_MAX_FRAME_BYTES > USBIP_DEVLIST_HEADER_BYTES);
}

#[test]
fn opcode_golden_bytes_round_trip() {
    let cases = [
        (UsbIpOpCode::ReqDevList, OP_REQ_DEVLIST, [0x80, 0x05]),
        (UsbIpOpCode::RepDevList, OP_REP_DEVLIST, [0x00, 0x05]),
        (UsbIpOpCode::ReqImport, OP_REQ_IMPORT, [0x80, 0x03]),
        (UsbIpOpCode::RepImport, OP_REP_IMPORT, [0x00, 0x03]),
    ];
    for (opcode, raw, code_bytes) in cases {
        assert_eq!(opcode.raw(), raw);
        assert_eq!(UsbIpOpCode::from_raw(raw), Some(opcode));
        assert_eq!(&opcode.raw().to_be_bytes(), &code_bytes);
        let header = if opcode.is_request() {
            UsbIpOpHeader::request(opcode)
        } else {
            UsbIpOpHeader::reply(opcode, UsbIpStatus::Ok)
        };
        let encoded = header.encode();
        assert_eq!(&encoded[0..2], &[0x01, 0x11]);
        assert_eq!(&encoded[2..4], &code_bytes);
        assert_eq!(&encoded[4..8], &[0, 0, 0, 0]);
        assert_eq!(UsbIpOpHeader::decode(&encoded).unwrap(), header);
    }
    assert!(UsbIpOpCode::from_raw(0x8004).is_none());
    assert_eq!(
        UsbIpOpCode::ReqDevList.reply(),
        Some(UsbIpOpCode::RepDevList)
    );
    assert_eq!(UsbIpOpCode::ReqImport.reply(), Some(UsbIpOpCode::RepImport));
}

#[test]
fn status_codes_round_trip_and_reject_unknown() {
    let cases = [
        (UsbIpStatus::Ok, 0x00),
        (UsbIpStatus::Na, 0x01),
        (UsbIpStatus::DevBusy, 0x02),
        (UsbIpStatus::DevErr, 0x03),
        (UsbIpStatus::NoDev, 0x04),
        (UsbIpStatus::Error, 0x05),
    ];
    for (status, raw) in cases {
        assert_eq!(status.raw(), raw);
        assert_eq!(UsbIpStatus::from_raw(raw), Some(status));
    }
    assert!(UsbIpStatus::from_raw(0x06).is_none());

    let bytes = header_bytes(0x0111, OP_REP_IMPORT, 0x06);
    assert_eq!(
        UsbIpOpHeader::decode(&bytes).unwrap_err(),
        UsbIpProtocolError::UnknownStatus { status: 0x06 }
    );
}

#[test]
fn device_record_is_network_byte_order_at_kernel_offsets() {
    let device = descriptor("1-1");
    let device_bytes = device.encode_devlist().unwrap();
    assert_eq!(device_bytes.len(), USBIP_DEVICE_BYTES + 2 * 4);
    // fixed-width strings: path at 0, busid at 256, NUL padded.
    assert_eq!(&device_bytes[0..device.path.len()], device.path.as_bytes());
    assert_eq!(device_bytes[device.path.len()], 0);
    assert_eq!(&device_bytes[256..259], b"1-1");
    assert_eq!(device_bytes[259], 0);
    // network-order scalars.
    assert_eq!(&device_bytes[288..292], &1u32.to_be_bytes());
    assert_eq!(&device_bytes[292..296], &2u32.to_be_bytes());
    assert_eq!(&device_bytes[296..300], &3u32.to_be_bytes());
    assert_eq!(&device_bytes[300..302], &0x1d6bu16.to_be_bytes());
    assert_eq!(&device_bytes[302..304], &0x0002u16.to_be_bytes());
    assert_eq!(&device_bytes[304..306], &0x0510u16.to_be_bytes());
    assert_eq!(device_bytes[306], 9);
    assert_eq!(device_bytes[307], 0);
    assert_eq!(device_bytes[308], 3);
    assert_eq!(device_bytes[309], 1);
    assert_eq!(device_bytes[310], 1);
    assert_eq!(device_bytes[311], 2);
    // interface records (class/subclass/protocol/zero padding).
    assert_eq!(&device_bytes[312..316], &[9, 0, 0, 0]);
    assert_eq!(&device_bytes[316..320], &[3, 1, 2, 0]);
    assert_eq!(
        UsbIpDeviceDescriptor::decode_devlist(&device_bytes).unwrap(),
        device
    );
}

#[test]
fn devlist_reply_golden_frame_and_round_trip() {
    let empty = UsbIpDevListReply::new(Vec::new()).encode().unwrap();
    assert_eq!(empty, vec![0x01, 0x11, 0x00, 0x05, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(UsbIpDevListReply::decode(&empty).unwrap().devices.len(), 0);

    let device = descriptor("1-1");
    let reply = UsbIpDevListReply::new(vec![device.clone()]);
    let frame = reply.encode().unwrap();
    assert_eq!(frame.len(), 12 + 320);
    assert_eq!(&frame[0..2], &[0x01, 0x11]);
    assert_eq!(&frame[2..4], &[0x00, 0x05]);
    assert_eq!(&frame[4..8], &[0, 0, 0, 0]);
    assert_eq!(&frame[8..12], &1u32.to_be_bytes());
    // Device record starts at byte 12; re-check a couple of shifted offsets.
    assert_eq!(
        &frame[12..12 + USBIP_PATH_BYTES][0..device.path.len()],
        device.path.as_bytes()
    );
    assert_eq!(&frame[12 + 256..12 + 259], b"1-1");
    assert_eq!(&frame[12 + 288..12 + 292], &1u32.to_be_bytes());
    assert_eq!(&frame[12 + 300..12 + 302], &0x1d6bu16.to_be_bytes());
    assert_eq!(&frame[12 + 311], &2u8);
    assert_eq!(&frame[324..328], &[9, 0, 0, 0]);

    let decoded = UsbIpDevListReply::decode(&frame).unwrap();
    assert_eq!(decoded, reply);
}

#[test]
fn import_request_is_a_fixed_forty_byte_frame() {
    let request = UsbIpImportRequest::new("1-2.3");
    let frame = request.encode().unwrap();
    assert_eq!(frame.len(), 40);
    assert_eq!(&frame[0..8], &[0x01, 0x11, 0x80, 0x03, 0, 0, 0, 0]);
    assert_eq!(&frame[8..13], b"1-2.3");
    assert!(frame[13..40].iter().all(|byte| *byte == 0));
    assert_eq!(UsbIpImportRequest::decode(&frame).unwrap(), request);

    // A DEVLIST opcode in an import frame is a typed mismatch, not a guess.
    let mut wrong = frame.clone();
    wrong[2..4].copy_from_slice(&OP_REQ_DEVLIST.to_be_bytes());
    assert!(matches!(
        UsbIpImportRequest::decode(&wrong).unwrap_err(),
        UsbIpProtocolError::UnexpectedOpcode { .. }
    ));
}

#[test]
fn import_reply_success_carries_only_the_device_record() {
    let device = descriptor("1-1");
    let reply = UsbIpImportReply::new(device.clone());
    let frame = reply.encode().unwrap();
    assert_eq!(frame.len(), 320);
    assert_eq!(&frame[0..8], &[0x01, 0x11, 0x00, 0x03, 0, 0, 0, 0]);
    assert_eq!(&frame[8 + 256..8 + 259], b"1-1");
    assert_eq!(&frame[8 + 288..8 + 292], &1u32.to_be_bytes());

    let decoded = UsbIpImportReply::decode(&frame).unwrap();
    let decoded_device = decoded.device.unwrap();
    assert_eq!(decoded_device.bus_id, "1-1");
    assert_eq!(decoded_device.vendor_id, 0x1d6b);
    assert_eq!(decoded_device.num_interfaces, 2);
    // The import reply has no interface records on the wire.
    assert!(decoded_device.interfaces.is_empty());

    let error = UsbIpImportReply::error(UsbIpStatus::NoDev);
    let frame = error.encode().unwrap();
    assert_eq!(frame, vec![0x01, 0x11, 0x00, 0x03, 0, 0, 0, 4]);
    let decoded = UsbIpImportReply::decode(&frame).unwrap();
    assert_eq!(decoded.header.status, UsbIpStatus::NoDev);
    assert!(decoded.device.is_none());
}

#[test]
fn bad_version_and_opcode_are_typed() {
    let mut version = header_bytes(USBIP_PROTOCOL_VERSION, OP_REQ_DEVLIST, 0);
    version[1] = 0x10;
    assert_eq!(
        UsbIpOpHeader::decode(&version).unwrap_err(),
        UsbIpProtocolError::UnsupportedVersion {
            version: 0x0110,
            expected: USBIP_PROTOCOL_VERSION,
        }
    );

    let unknown = header_bytes(USBIP_PROTOCOL_VERSION, 0x9999, 0);
    assert_eq!(
        UsbIpOpHeader::decode(&unknown).unwrap_err(),
        UsbIpProtocolError::UnknownOpcode { code: 0x9999 }
    );

    let reply_as_request = header_bytes(USBIP_PROTOCOL_VERSION, OP_REP_DEVLIST, 0);
    assert!(matches!(
        UsbIpDevListRequest::decode(&reply_as_request).unwrap_err(),
        UsbIpProtocolError::UnexpectedOpcode { .. }
    ));

    let mut request_with_status = header_bytes(USBIP_PROTOCOL_VERSION, OP_REQ_IMPORT, 0);
    request_with_status[7] = 1;
    assert!(matches!(
        UsbIpOpHeader::decode(&request_with_status).unwrap_err(),
        UsbIpProtocolError::InconsistentStatus { .. }
    ));
}

#[test]
fn truncated_frames_are_distinct_from_trailing_bytes() {
    assert_eq!(
        UsbIpOpHeader::decode(&[0x01, 0x11, 0x80]).unwrap_err(),
        UsbIpProtocolError::Truncated {
            needed: 8,
            available: 3,
        }
    );
    let mut header = header_bytes(USBIP_PROTOCOL_VERSION, OP_REQ_DEVLIST, 0);
    header.push(0);
    assert_eq!(
        UsbIpOpHeader::decode(&header).unwrap_err(),
        UsbIpProtocolError::TrailingBytes {
            expected: 8,
            actual: 9,
        }
    );

    let request = UsbIpImportRequest::new("1-1").encode().unwrap();
    assert_eq!(
        UsbIpImportRequest::decode(&request[..39]).unwrap_err(),
        UsbIpProtocolError::Truncated {
            needed: 40,
            available: 39,
        }
    );
    let mut trailing = request.clone();
    trailing.push(0);
    assert_eq!(
        UsbIpImportRequest::decode(&trailing).unwrap_err(),
        UsbIpProtocolError::TrailingBytes {
            expected: 40,
            actual: 41,
        }
    );

    let device = descriptor("1-1").encode_devlist().unwrap();
    assert_eq!(
        UsbIpDeviceDescriptor::decode_devlist(&device[..device.len() - 1]).unwrap_err(),
        UsbIpProtocolError::Truncated {
            needed: device.len(),
            available: device.len() - 1,
        }
    );

    let reply = UsbIpImportReply::new(descriptor("1-1")).encode().unwrap();
    assert_eq!(
        UsbIpImportReply::decode(&reply[..319]).unwrap_err(),
        UsbIpProtocolError::Truncated {
            needed: 320,
            available: 319,
        }
    );
}

#[test]
fn oversized_counts_frames_and_strings_are_rejected() {
    // ndev is rejected before any device loop/allocation.
    let mut frame = UsbIpDevListReply::new(vec![descriptor("1-1")])
        .encode()
        .unwrap();
    frame[8..12].copy_from_slice(&257u32.to_be_bytes());
    assert_eq!(
        UsbIpDevListReply::decode(&frame).unwrap_err(),
        UsbIpProtocolError::Oversized {
            field: "ndev",
            limit: 256,
            requested: 257,
        }
    );

    // bNumInterfaces > 32 is rejected by the device decoder.
    let mut frame = UsbIpDevListReply::new(vec![descriptor("1-1")])
        .encode()
        .unwrap();
    frame[12 + 311] = 33;
    assert!(matches!(
        UsbIpDevListReply::decode(&frame).unwrap_err(),
        UsbIpProtocolError::Oversized {
            field: "bNumInterfaces",
            ..
        }
    ));

    // A caller-supplied frame ceiling is enforced before parsing.
    let frame = UsbIpDevListReply::new(Vec::new()).encode().unwrap();
    assert_eq!(
        UsbIpDevListReply::decode_bounded(&frame, 11).unwrap_err(),
        UsbIpProtocolError::Oversized {
            field: "frame",
            limit: 11,
            requested: 12,
        }
    );

    // Strings must fit the fixed buffer with a terminating NUL slot.
    let long_path = "x".repeat(USBIP_PATH_BYTES);
    let device = UsbIpDeviceDescriptor::new(long_path, "1-1");
    assert_eq!(
        device.validate().unwrap_err(),
        UsbIpProtocolError::Oversized {
            field: "path",
            limit: USBIP_PATH_BYTES - 1,
            requested: USBIP_PATH_BYTES,
        }
    );
    let bad = UsbIpDeviceDescriptor::new("/sys/devices/usb1/1-1", "1-1\n");
    assert!(matches!(
        bad.validate().unwrap_err(),
        UsbIpProtocolError::InvalidString { field: "busid", .. }
    ));
    let bad = UsbIpImportRequest::new("-1");
    assert!(matches!(
        bad.encode().unwrap_err(),
        UsbIpProtocolError::InvalidString { field: "busid", .. }
    ));
}

#[test]
fn non_printable_strings_and_inconsistent_bodies_are_rejected() {
    let mut frame = UsbIpDevListReply::new(vec![descriptor("1-1")])
        .encode()
        .unwrap();
    frame[12] = 0x01;
    assert!(matches!(
        UsbIpDevListReply::decode(&frame).unwrap_err(),
        UsbIpProtocolError::InvalidString { field: "path", .. }
    ));

    let device = descriptor("1-1");
    let error_with_device = UsbIpImportReply {
        header: UsbIpOpHeader::reply(UsbIpOpCode::RepImport, UsbIpStatus::NoDev),
        device: Some(device.clone()),
    };
    assert!(matches!(
        error_with_device.encode().unwrap_err(),
        UsbIpProtocolError::InconsistentStatus { .. }
    ));

    let success_without_device = UsbIpImportReply {
        header: UsbIpOpHeader::reply(UsbIpOpCode::RepImport, UsbIpStatus::Ok),
        device: None,
    };
    assert!(matches!(
        success_without_device.encode().unwrap_err(),
        UsbIpProtocolError::InconsistentStatus { .. }
    ));

    let mut error_with_devices = UsbIpDevListReply::error(UsbIpStatus::Error);
    error_with_devices.devices.push(device);
    assert!(matches!(
        error_with_devices.encode().unwrap_err(),
        UsbIpProtocolError::InconsistentStatus { .. }
    ));
}

#[test]
fn protocol_errors_convert_to_a_typed_usb_error() {
    let error = UsbError::from(UsbIpProtocolError::UnknownOpcode { code: 0x1234 });
    assert_eq!(error.kind(), "usbip-protocol");
    assert!(error.to_string().contains("0x1234"));
    match error {
        UsbError::UsbIpProtocol(inner) => {
            assert_eq!(inner, UsbIpProtocolError::UnknownOpcode { code: 0x1234 });
        }
        other => panic!("unexpected error {other:?}"),
    }
}
