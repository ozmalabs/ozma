// SPDX-License-Identifier: AGPL-3.0-only
//! USB/IP URB codec golden vectors, offsets, bounds, and redaction.

use ozma_usb::usbip_urb::{
    UsbIpCmdSubmit, UsbIpCmdUnlink, UsbIpDirection, UsbIpIsoPacketDescriptor, UsbIpRetSubmit,
    UsbIpRetUnlink, UsbIpUrb, UsbIpUrbCommand, UsbIpUrbError, USBIP_CMD_SUBMIT, USBIP_CMD_UNLINK,
    USBIP_ISO_PACKET_BYTES, USBIP_MAX_ENDPOINT, USBIP_MAX_ISO_PACKETS, USBIP_RET_SUBMIT,
    USBIP_RET_UNLINK, USBIP_URB_HEADER_BYTES, USBIP_URB_MAX_TRANSFER_BYTES,
    USBIP_URB_NO_ISO_PACKETS, USBIP_URB_UNLINK_CANCELLED, USBIP_URB_UNLINK_NOT_FOUND,
};
use ozma_usb::UsbError;

fn out_submit() -> UsbIpCmdSubmit {
    UsbIpCmdSubmit {
        seqnum: 0x0102_0304,
        devid: 0x0a0b_0c0d,
        direction: UsbIpDirection::Out,
        ep: 2,
        transfer_flags: 0x0000_0002,
        transfer_buffer_length: 4,
        start_frame: 0,
        number_of_packets: USBIP_URB_NO_ISO_PACKETS,
        interval: 0,
        setup: [0; 8],
        data: b"ABCD".to_vec(),
        iso_packets: Vec::new(),
    }
}

fn in_completion() -> UsbIpRetSubmit {
    UsbIpRetSubmit {
        seqnum: 0x0102_0304,
        devid: 0x0a0b_0c0d,
        direction: UsbIpDirection::In,
        ep: 2,
        status: -32,
        actual_length: 4,
        start_frame: 0x10,
        number_of_packets: USBIP_URB_NO_ISO_PACKETS,
        error_count: 1,
        data: vec![0xde, 0xad, 0xbe, 0xef],
        iso_packets: Vec::new(),
    }
}

#[test]
fn layout_constants_match_the_kernel_urb_records() {
    assert_eq!(USBIP_CMD_SUBMIT, 0x0000_0001);
    assert_eq!(USBIP_CMD_UNLINK, 0x0000_0002);
    assert_eq!(USBIP_RET_SUBMIT, 0x0000_0003);
    assert_eq!(USBIP_RET_UNLINK, 0x0000_0004);
    assert_eq!(USBIP_URB_HEADER_BYTES, 48);
    assert_eq!(USBIP_ISO_PACKET_BYTES, 16);
    assert_eq!(USBIP_MAX_ENDPOINT, 15);
    assert_eq!(USBIP_MAX_ISO_PACKETS, 128);
    assert!(USBIP_URB_MAX_TRANSFER_BYTES >= 64 * 1024);
}

#[test]
fn cmd_submit_out_has_exact_basic_and_command_offsets() {
    let submit = out_submit();
    let frame = submit.encode().unwrap();
    assert_eq!(frame.len(), 52);
    assert_eq!(&frame[0..4], &USBIP_CMD_SUBMIT.to_be_bytes());
    assert_eq!(&frame[4..8], &0x0102_0304u32.to_be_bytes());
    assert_eq!(&frame[8..12], &0x0a0b_0c0du32.to_be_bytes());
    assert_eq!(&frame[12..16], &0u32.to_be_bytes()); // USBIP_DIR_OUT
    assert_eq!(&frame[16..20], &2u32.to_be_bytes());
    assert_eq!(&frame[20..24], &0x0000_0002u32.to_be_bytes());
    assert_eq!(&frame[24..28], &4u32.to_be_bytes());
    assert_eq!(&frame[28..32], &0u32.to_be_bytes());
    assert_eq!(&frame[32..36], &u32::MAX.to_be_bytes());
    assert_eq!(&frame[36..40], &0u32.to_be_bytes());
    assert_eq!(&frame[40..48], &[0u8; 8]);
    assert_eq!(&frame[48..52], b"ABCD");
    assert_eq!(UsbIpCmdSubmit::decode(&frame).unwrap(), submit);
}

#[test]
fn cmd_submit_in_has_no_payload_but_keeps_the_requested_length() {
    let submit = UsbIpCmdSubmit::in_request(7, 9, 1, 64);
    let frame = submit.encode().unwrap();
    assert_eq!(frame.len(), USBIP_URB_HEADER_BYTES);
    assert_eq!(&frame[12..16], &1u32.to_be_bytes());
    assert_eq!(&frame[24..28], &64u32.to_be_bytes());
    assert_eq!(UsbIpCmdSubmit::decode(&frame).unwrap(), submit);

    let mut invalid = submit;
    invalid.data.push(0xaa);
    assert_eq!(
        invalid.encode().unwrap_err(),
        UsbIpUrbError::InconsistentData {
            field: "IN payload",
            expected: 0,
            actual: 1,
        }
    );
}

#[test]
fn ret_submit_in_has_exact_completion_offsets_and_payload() {
    let completion = in_completion();
    let frame = completion.encode().unwrap();
    assert_eq!(frame.len(), 52);
    assert_eq!(&frame[0..4], &USBIP_RET_SUBMIT.to_be_bytes());
    assert_eq!(&frame[4..8], &0x0102_0304u32.to_be_bytes());
    assert_eq!(&frame[8..12], &0x0a0b_0c0du32.to_be_bytes());
    assert_eq!(&frame[12..16], &1u32.to_be_bytes());
    assert_eq!(&frame[16..20], &2u32.to_be_bytes());
    assert_eq!(&frame[20..24], &(-32i32).to_be_bytes());
    assert_eq!(&frame[24..28], &4u32.to_be_bytes());
    assert_eq!(&frame[28..32], &0x10u32.to_be_bytes());
    assert_eq!(&frame[32..36], &u32::MAX.to_be_bytes());
    assert_eq!(&frame[36..40], &1u32.to_be_bytes());
    assert_eq!(&frame[40..48], &[0u8; 8]);
    assert_eq!(&frame[48..52], &[0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(UsbIpRetSubmit::decode(&frame).unwrap(), completion);
}

#[test]
fn ret_submit_out_has_no_payload_even_for_a_nonzero_actual_length() {
    let mut completion = in_completion();
    completion.direction = UsbIpDirection::Out;
    completion.data.clear();
    completion.actual_length = 4096;
    let frame = completion.encode().unwrap();
    assert_eq!(frame.len(), USBIP_URB_HEADER_BYTES);
    assert_eq!(&frame[24..28], &4096u32.to_be_bytes());
    assert_eq!(UsbIpRetSubmit::decode(&frame).unwrap(), completion);

    let mut invalid = completion;
    invalid.data.push(1);
    assert_eq!(
        invalid.encode().unwrap_err(),
        UsbIpUrbError::InconsistentData {
            field: "OUT payload",
            expected: 0,
            actual: 1,
        }
    );
}

#[test]
fn kernel_style_ret_submit_direction_zero_can_be_decoded_with_context() {
    let completion = in_completion();
    let mut frame = completion.encode().unwrap();
    // A kernel server writes direction = 0 in every reply; the client knows the
    // original URB direction. A standalone decoder needs that context.
    frame[12..16].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(
        UsbIpRetSubmit::decode(&frame).unwrap_err(),
        UsbIpUrbError::TrailingBytes {
            expected: USBIP_URB_HEADER_BYTES,
            actual: 52,
        }
    );
    let decoded = UsbIpRetSubmit::decode_with_direction(&frame, UsbIpDirection::In).unwrap();
    assert_eq!(decoded.seqnum, completion.seqnum);
    assert_eq!(decoded.data, completion.data);
    assert_eq!(decoded.direction, UsbIpDirection::In);
}

#[test]
fn unlink_frames_are_fixed_48_byte_records_with_zeroed_padding() {
    let unlink = UsbIpCmdUnlink::new(10, 0x0101_0002, 9);
    let frame = unlink.encode().unwrap();
    assert_eq!(frame.len(), 48);
    assert_eq!(&frame[0..4], &USBIP_CMD_UNLINK.to_be_bytes());
    assert_eq!(&frame[4..8], &10u32.to_be_bytes());
    assert_eq!(&frame[8..12], &0x0101_0002u32.to_be_bytes());
    assert_eq!(&frame[12..20], &[0u8; 8]);
    assert_eq!(&frame[20..24], &9u32.to_be_bytes());
    assert_eq!(&frame[24..48], &[0u8; 24]);
    assert_eq!(UsbIpCmdUnlink::decode(&frame).unwrap(), unlink);

    let reply = UsbIpRetUnlink::cancelled(11, 0x0101_0002);
    let frame = reply.encode().unwrap();
    assert_eq!(frame.len(), 48);
    assert_eq!(&frame[0..4], &USBIP_RET_UNLINK.to_be_bytes());
    assert_eq!(&frame[20..24], &USBIP_URB_UNLINK_CANCELLED.to_be_bytes());
    assert_eq!(UsbIpRetUnlink::decode(&frame).unwrap(), reply);
    assert_eq!(
        UsbIpRetUnlink::not_found(12, 1).status,
        USBIP_URB_UNLINK_NOT_FOUND
    );
}

#[test]
fn enum_wrapper_round_trips_every_command() {
    let submit = out_submit();
    let completion = in_completion();
    let unlink = UsbIpCmdUnlink::new(3, 4, 5);
    let reply = UsbIpRetUnlink::cancelled(6, 4);
    let cases = [
        UsbIpUrb::CmdSubmit(submit.clone()),
        UsbIpUrb::RetSubmit(completion.clone()),
        UsbIpUrb::CmdUnlink(unlink),
        UsbIpUrb::RetUnlink(reply),
    ];
    let expected = [
        UsbIpUrbCommand::CmdSubmit,
        UsbIpUrbCommand::RetSubmit,
        UsbIpUrbCommand::CmdUnlink,
        UsbIpUrbCommand::RetUnlink,
    ];
    for (urb, command) in cases.into_iter().zip(expected) {
        assert_eq!(urb.command(), command);
        let frame = urb.encode().unwrap();
        assert_eq!(UsbIpUrb::decode(&frame).unwrap(), urb);
    }
}

#[test]
fn truncation_and_trailing_bytes_are_distinct_errors() {
    let frame = out_submit().encode().unwrap();
    let mut short = frame.clone();
    short.pop();
    assert_eq!(
        UsbIpUrb::decode(&short).unwrap_err(),
        UsbIpUrbError::Truncated {
            needed: 52,
            available: 51,
        }
    );
    let mut long = frame;
    long.push(0);
    assert_eq!(
        UsbIpUrb::decode(&long).unwrap_err(),
        UsbIpUrbError::TrailingBytes {
            expected: 52,
            actual: 53,
        }
    );
}

#[test]
fn unknown_commands_directions_and_endpoints_are_typed() {
    let mut frame = out_submit().encode().unwrap();
    frame[0..4].copy_from_slice(&0x9999u32.to_be_bytes());
    assert_eq!(
        UsbIpUrb::decode(&frame).unwrap_err(),
        UsbIpUrbError::UnknownCommand { command: 0x9999 }
    );

    let mut frame = out_submit().encode().unwrap();
    frame[12..16].copy_from_slice(&2u32.to_be_bytes());
    assert_eq!(
        UsbIpUrb::decode(&frame).unwrap_err(),
        UsbIpUrbError::UnsupportedDirection { direction: 2 }
    );

    let mut frame = out_submit().encode().unwrap();
    frame[16..20].copy_from_slice(&16u32.to_be_bytes());
    assert_eq!(
        UsbIpUrb::decode(&frame).unwrap_err(),
        UsbIpUrbError::InvalidEndpoint {
            ep: 16,
            limit: USBIP_MAX_ENDPOINT,
        }
    );
}

#[test]
fn oversized_and_negative_lengths_are_rejected_before_allocation() {
    let mut frame = out_submit().encode().unwrap();
    frame[24..28].copy_from_slice(&((USBIP_URB_MAX_TRANSFER_BYTES as u32) + 1).to_be_bytes());
    assert_eq!(
        UsbIpUrb::frame_len_from_header(&frame[..48]).unwrap_err(),
        UsbIpUrbError::Oversized {
            field: "transfer_buffer_length",
            limit: USBIP_URB_MAX_TRANSFER_BYTES,
            requested: USBIP_URB_MAX_TRANSFER_BYTES + 1,
        }
    );

    let mut frame = out_submit().encode().unwrap();
    frame[24..28].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(
        UsbIpUrb::frame_len_from_header(&frame[..48]).unwrap_err(),
        UsbIpUrbError::InvalidField {
            field: "transfer_buffer_length",
            ..
        }
    ));

    let mut frame = out_submit().encode().unwrap();
    frame[32..36].copy_from_slice(&((USBIP_MAX_ISO_PACKETS as u32) + 1).to_be_bytes());
    assert_eq!(
        UsbIpUrb::frame_len_from_header(&frame[..48]).unwrap_err(),
        UsbIpUrbError::Oversized {
            field: "number_of_packets",
            limit: USBIP_MAX_ISO_PACKETS,
            requested: USBIP_MAX_ISO_PACKETS + 1,
        }
    );
}

#[test]
fn iso_descriptor_arrays_round_trip_and_are_strictly_bounded() {
    let mut submit = UsbIpCmdSubmit::out(1, 2, 3, vec![0x11; 64]);
    submit.number_of_packets = 2;
    submit.iso_packets = vec![
        UsbIpIsoPacketDescriptor::new(0, 32, 32, 0),
        UsbIpIsoPacketDescriptor::new(32, 32, 32, 0),
    ];
    let frame = submit.encode().unwrap();
    assert_eq!(frame.len(), 48 + 64 + 2 * 16);
    assert_eq!(UsbIpCmdSubmit::decode(&frame).unwrap(), submit);

    let mut completion = UsbIpRetSubmit::new(1, 2, UsbIpDirection::In, 3, vec![0x22; 64]);
    completion.number_of_packets = 2;
    completion.iso_packets = vec![
        UsbIpIsoPacketDescriptor::new(0, 32, 32, 0),
        UsbIpIsoPacketDescriptor::new(32, 32, 32, 0),
    ];
    let frame = completion.encode().unwrap();
    assert_eq!(frame.len(), 48 + 64 + 2 * 16);
    assert_eq!(UsbIpRetSubmit::decode(&frame).unwrap(), completion);

    let mut out_of_bounds = UsbIpCmdSubmit::out(1, 2, 3, vec![0; 8]);
    out_of_bounds.number_of_packets = 1;
    out_of_bounds.iso_packets = vec![UsbIpIsoPacketDescriptor::new(4, 8, 0, 0)];
    assert_eq!(
        out_of_bounds.encode().unwrap_err(),
        UsbIpUrbError::IsoPacketOutOfBounds {
            index: 0,
            offset: 4,
            length: 8,
            limit: 8,
        }
    );

    let mut bad_actual = UsbIpRetSubmit::new(1, 2, UsbIpDirection::In, 3, vec![0; 8]);
    bad_actual.number_of_packets = 1;
    bad_actual.iso_packets = vec![UsbIpIsoPacketDescriptor::new(0, 8, 9, 0)];
    assert!(matches!(
        bad_actual.encode().unwrap_err(),
        UsbIpUrbError::InvalidField {
            field: "iso_packets.actual_length",
            ..
        }
    ));

    let mut count_mismatch = UsbIpCmdSubmit::out(1, 2, 3, vec![0; 8]);
    count_mismatch.number_of_packets = 2;
    count_mismatch.iso_packets = vec![UsbIpIsoPacketDescriptor::new(0, 8, 8, 0)];
    assert_eq!(
        count_mismatch.encode().unwrap_err(),
        UsbIpUrbError::CountMismatch {
            field: "iso_packets",
            declared: 2,
            actual: 1,
        }
    );
}

#[test]
fn bounded_decode_rejects_transfers_above_the_callers_ceiling() {
    let frame = UsbIpCmdSubmit::out(1, 2, 3, vec![0; 8]).encode().unwrap();
    assert_eq!(
        UsbIpUrb::decode_bounded(&frame, 4).unwrap_err(),
        UsbIpUrbError::Oversized {
            field: "transfer_buffer_length",
            limit: 4,
            requested: 8,
        }
    );
}

#[test]
fn payloads_are_redacted_from_debug_and_map_to_a_typed_error_kind() {
    let secret = b"super-secret-device-payload";
    let submit = UsbIpCmdSubmit::out(1, 2, 3, secret.to_vec());
    let completion = UsbIpRetSubmit::new(1, 2, UsbIpDirection::In, 3, secret.to_vec());
    let debug = format!(
        "{submit:?} {completion:?} {:?}",
        UsbIpUrb::CmdSubmit(submit.clone())
    );
    assert!(!debug.contains("super-secret-device-payload"));
    assert!(!debug.contains("115, 117, 112, 101, 114"));
    assert!(debug.contains("data_len"));

    let error = UsbError::from(UsbIpUrbError::UnknownCommand { command: 0x1234 });
    assert_eq!(error.kind(), "usbip-urb");
}
