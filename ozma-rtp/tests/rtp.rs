// SPDX-License-Identifier: AGPL-3.0-only
//! Integration tests for the H.264 RTP packetizer/depacketizer.

use ozma_rtp::{
    annex_b_is_keyframe, micros_to_rtp_timestamp, push_annex_b, split_annex_b,
    DepacketizedAccessUnit, DepacketizerConfig, H264Depacketizer, H264Packetizer, PacketizerConfig,
    RtpError, RtpHeader, RtpTimestampClock, DEFAULT_PAYLOAD_TYPE,
};

const SPS: &[u8] = &[0x67, 0x42, 0xc0, 0x1f, 0x8c, 0x8d, 0x40];
const PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];

fn idr(len: usize) -> Vec<u8> {
    let mut nal = vec![0x65];
    nal.extend((0..len).map(|index| (index % 251) as u8));
    nal
}

fn delta(len: usize) -> Vec<u8> {
    let mut nal = vec![0x41];
    nal.extend((0..len).map(|index| (index % 239 + 1) as u8));
    nal
}

fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        push_annex_b(&mut out, nal);
    }
    out
}

fn packetizer(mtu: usize, repeat_parameter_sets: bool) -> H264Packetizer {
    H264Packetizer::with_config(
        0xdead_beef,
        PacketizerConfig {
            mtu,
            repeat_parameter_sets,
            ..PacketizerConfig::default()
        },
    )
    .unwrap()
}

fn depacketize(packets: &[bytes::Bytes]) -> Vec<DepacketizedAccessUnit> {
    let mut depacketizer = H264Depacketizer::new();
    let mut units = Vec::new();
    for packet in packets {
        if let Some(unit) = depacketizer.push(packet).expect("valid packet") {
            units.push(unit);
        }
    }
    units
}

fn nals_of(unit: &DepacketizedAccessUnit) -> Vec<Vec<u8>> {
    split_annex_b(&unit.data)
        .unwrap()
        .into_iter()
        .map(|nal| nal.to_vec())
        .collect()
}

#[test]
fn roundtrip_stap_a_and_single_nal_keyframe() {
    let slice = idr(40);
    let access_unit = annex_b(&[SPS, PPS, &slice]);
    assert!(annex_b_is_keyframe(&access_unit));

    let mut packetizer = packetizer(1200, false);
    let packets = packetizer.packetize(&access_unit, 90_000, true).unwrap();
    assert_eq!(packets.len(), 2, "SPS+PPS aggregate then one IDR");
    assert!(packetizer.stats().keyframes == 1);

    let first = RtpHeader::parse(&packets[0]).unwrap();
    let second = RtpHeader::parse(&packets[1]).unwrap();
    assert_eq!(first.payload_type, DEFAULT_PAYLOAD_TYPE);
    assert!(!first.marker);
    assert!(second.marker);
    assert_eq!(first.sequence, 0);
    assert_eq!(second.sequence, 1);
    assert_eq!(first.timestamp, 90_000);
    assert_eq!(second.timestamp, 90_000);
    assert_eq!(first.ssrc, 0xdead_beef);
    assert_eq!(packets[0][0] >> 6, 2, "RTP version 2");

    let stap = first.payload(&packets[0]).unwrap();
    assert_eq!(stap[0] & 0x1f, 24, "STAP-A payload type");
    let mut offset = 1;
    let mut aggregated = Vec::new();
    while offset < stap.len() {
        let len = u16::from_be_bytes([stap[offset], stap[offset + 1]]) as usize;
        offset += 2;
        aggregated.push(stap[offset..offset + len].to_vec());
        offset += len;
    }
    assert_eq!(aggregated.len(), 2);
    assert_eq!(aggregated[0], SPS);
    assert_eq!(aggregated[1], PPS);

    let units = depacketize(&packets);
    assert_eq!(units.len(), 1);
    let unit = &units[0];
    assert!(unit.keyframe);
    assert_eq!(unit.timestamp, 90_000);
    assert_eq!(unit.packets, 2);
    assert_eq!(nals_of(unit), vec![SPS.to_vec(), PPS.to_vec(), slice]);
}

#[test]
fn fu_a_fragmentation_at_small_mtu_roundtrips() {
    let slice = idr(700);
    let access_unit = annex_b(&[SPS, PPS, &slice]);
    let mut packetizer = packetizer(120, false);
    let packets = packetizer.packetize(&access_unit, 1_800, true).unwrap();
    assert!(packets.len() > 5);

    let mut fragment_count = 0;
    for (index, packet) in packets.iter().enumerate() {
        let header = RtpHeader::parse(packet).unwrap();
        let payload = header.payload(packet).unwrap();
        let nal_type = payload[0] & 0x1f;
        if nal_type == 28 {
            fragment_count += 1;
            let fu = payload[1];
            assert_eq!(fu & 0x1f, 5, "FU-A carries the original IDR type");
            assert_eq!(
                fu & 0x80 != 0,
                index == 1,
                "only the first IDR fragment starts"
            );
            if index == 1 {
                assert!(fu & 0x80 != 0);
            }
            assert_eq!(
                fu & 0x40 != 0,
                index + 1 == packets.len(),
                "last fragment ends"
            );
            assert!(packet.len() <= 120, "packet respects the MTU");
        } else {
            assert_eq!(nal_type, 24, "parameter sets use STAP-A");
        }
        assert_eq!(header.marker, index + 1 == packets.len());
    }
    assert!(fragment_count >= 6);

    let units = depacketize(&packets);
    assert_eq!(units.len(), 1);
    assert_eq!(nals_of(&units[0]), vec![SPS.to_vec(), PPS.to_vec(), slice]);
    let mut depacketizer = H264Depacketizer::new();
    for packet in &packets {
        let _ = depacketizer.push(packet).unwrap();
    }
    assert!(depacketizer.stats().fu_a_packets >= 6);
    assert_eq!(depacketizer.stats().stap_a_packets, 1);
}

#[test]
fn marker_sequence_and_timestamp_progress_across_access_units() {
    let key = annex_b(&[SPS, PPS, &idr(30)]);
    let p1 = annex_b(&[&delta(25)]);
    let p2 = annex_b(&[&delta(26)]);
    let mut packetizer = packetizer(1200, false);

    let key_packets = packetizer.packetize(&key, 90_000, true).unwrap();
    let p1_packets = packetizer.packetize(&p1, 93_000, false).unwrap();
    let p2_packets = packetizer.packetize(&p2, 96_000, false).unwrap();

    let mut expected_sequence = 0u16;
    for packet in key_packets.iter().chain(&p1_packets).chain(&p2_packets) {
        let header = RtpHeader::parse(packet).unwrap();
        assert_eq!(header.sequence, expected_sequence);
        expected_sequence = expected_sequence.wrapping_add(1);
    }
    assert!(
        RtpHeader::parse(key_packets.last().unwrap())
            .unwrap()
            .marker
    );
    assert!(RtpHeader::parse(p1_packets.last().unwrap()).unwrap().marker);
    assert!(RtpHeader::parse(p2_packets.last().unwrap()).unwrap().marker);
    assert!(!RtpHeader::parse(&key_packets[0]).unwrap().marker);
    assert_eq!(RtpHeader::parse(&p1_packets[0]).unwrap().timestamp, 93_000);
    assert_eq!(RtpHeader::parse(&p2_packets[0]).unwrap().timestamp, 96_000);

    assert_eq!(micros_to_rtp_timestamp(1_000_000), 90_000);
    let mut clock = RtpTimestampClock::default();
    assert_eq!(clock.timestamp(5_000_000), 0);
    assert_eq!(clock.timestamp(5_011_111), 999);
}

#[test]
fn parameter_sets_are_repeated_before_later_idr() {
    let first = annex_b(&[SPS, PPS, &idr(20)]);
    let second_idr = annex_b(&[&idr(20)]);
    let mut packetizer = packetizer(1200, true);

    let first_packets = packetizer.packetize(&first, 0, true).unwrap();
    let second_packets = packetizer.packetize(&second_idr, 3_000, true).unwrap();
    assert_eq!(second_packets.len(), 2, "cached STAP-A plus IDR");
    assert_eq!(
        RtpHeader::parse(&second_packets[0])
            .unwrap()
            .payload(&second_packets[0])
            .unwrap()[0]
            & 0x1f,
        24
    );
    let units = depacketize(&second_packets);
    assert_eq!(units.len(), 1);
    assert_eq!(nals_of(&units[0]).len(), 3);
    assert_eq!(nals_of(&units[0])[0], SPS);
    assert_eq!(nals_of(&units[0])[1], PPS);
    assert!(!first_packets.is_empty());
}

#[test]
fn loss_is_counted_and_does_not_emit_a_broken_unit() {
    let slice = idr(800);
    let access_unit = annex_b(&[&slice]);
    let mut packetizer = packetizer(120, false);
    let packets = packetizer.packetize(&access_unit, 7_000, true).unwrap();
    assert!(packets.len() > 6);

    let mut depacketizer = H264Depacketizer::new();
    let dropped = packets[3].clone();
    let mut saw_unit = false;
    for (index, packet) in packets.iter().enumerate() {
        if index == 3 {
            continue;
        }
        if depacketizer.push(packet).unwrap().is_some() {
            saw_unit = true;
        }
    }
    assert!(!saw_unit, "an access unit with a gap must not be emitted");
    assert_eq!(depacketizer.pending_len(), 1);
    assert!(depacketizer.pending_missing_fragments() >= 1);
    assert_eq!(depacketizer.flush_incomplete(), 1);
    let stats = depacketizer.stats();
    assert_eq!(stats.incomplete_access_units, 1);
    assert_eq!(stats.dropped_access_units, 1);
    assert!(stats.missing_fragments >= 1);

    // The dropped packet's absence is visible; feeding it late does not
    // resurrect a unit that was already flushed.
    assert!(depacketizer.push(&dropped).is_ok());
}

#[test]
fn reordering_middle_fragments_and_marker_before_a_gap_is_tolerated() {
    let slice = idr(700);
    let access_unit = annex_b(&[&slice]);
    let mut packetizer = packetizer(160, false);
    let packets = packetizer.packetize(&access_unit, 9_000, true).unwrap();
    assert!(packets.len() >= 4);

    // Start, marker/end, then the missing middle fragments.
    let mut order: Vec<usize> = Vec::new();
    order.push(0);
    order.push(packets.len() - 1);
    for index in 1..packets.len() - 1 {
        order.push(index);
    }

    let mut depacketizer = H264Depacketizer::new();
    let mut unit = None;
    for index in order {
        if let Some(complete) = depacketizer.push(&packets[index]).unwrap() {
            unit = Some(complete);
        }
    }
    let unit = unit.expect("out-of-order fragments complete");
    assert_eq!(nals_of(&unit), vec![slice]);
    assert!(depacketizer.stats().reordered_packets > 0);
}

#[test]
fn swaps_two_fragments_before_marker() {
    let slice = idr(800);
    let access_unit = annex_b(&[&slice]);
    let mut packetizer = packetizer(140, false);
    let mut packets = packetizer.packetize(&access_unit, 10_000, true).unwrap();
    assert!(packets.len() >= 5);
    packets.swap(1, 2);
    let units = depacketize(&packets);
    assert_eq!(units.len(), 1);
    assert_eq!(nals_of(&units[0]), vec![slice]);
}

#[test]
fn sequence_wrap_is_reassembled() {
    let access_unit = annex_b(&[SPS, PPS, &idr(300)]);
    let mut packetizer = packetizer(120, false);
    packetizer.set_sequence(65_534);
    let packets = packetizer.packetize(&access_unit, 11_000, true).unwrap();
    let sequences: Vec<u16> = packets
        .iter()
        .map(|packet| RtpHeader::parse(packet).unwrap().sequence)
        .collect();
    assert_eq!(sequences[0], 65_534);
    assert_eq!(sequences[1], 65_535);
    assert_eq!(sequences[2], 0);
    assert_eq!(packets.len(), 4);

    let units = depacketize(&packets);
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].first_sequence, 65_534);
    assert_eq!(units[0].last_sequence, 1);
    assert_eq!(nals_of(&units[0]).len(), 3);
}

#[test]
fn malformed_packets_are_reported_without_panicking() {
    let mut depacketizer = H264Depacketizer::new();
    assert!(matches!(
        depacketizer.push(&[]),
        Err(RtpError::PacketTooShort { .. })
    ));

    let mut bad_version = vec![0u8; 14];
    bad_version[0] = 0x40;
    assert!(matches!(
        depacketizer.push(&bad_version),
        Err(RtpError::UnsupportedVersion(1))
    ));

    let empty_payload = vec![0x80, 96, 0, 1, 0, 0, 0, 0, 1, 2, 3, 4];
    assert!(matches!(
        depacketizer.push(&empty_payload),
        Err(RtpError::EmptyPayload)
    ));

    let mut truncated_stap = empty_payload.clone();
    truncated_stap[1] = 96;
    truncated_stap.extend_from_slice(&[24, 0, 8, 0x67]);
    assert!(matches!(
        depacketizer.push(&truncated_stap),
        Err(RtpError::InvalidPayload(_))
    ));

    let mut unsupported = empty_payload.clone();
    unsupported.extend_from_slice(&[25, 0, 1, 0x67]);
    assert!(matches!(
        depacketizer.push(&unsupported),
        Err(RtpError::UnsupportedH264PayloadType(25))
    ));

    let stats = depacketizer.stats();
    assert_eq!(stats.malformed_packets, 5);
    assert!(depacketizer.pending_len() == 0);
}

#[test]
fn conflicting_duplicates_and_duplicate_packets_are_distinguished() {
    let access_unit = annex_b(&[&idr(400)]);
    let mut packetizer = packetizer(120, false);
    let packets = packetizer.packetize(&access_unit, 0, false).unwrap();
    assert!(packets.len() > 2);
    let mut depacketizer = H264Depacketizer::new();
    assert!(depacketizer.push(&packets[0]).unwrap().is_none());
    assert!(depacketizer.push(&packets[0]).unwrap().is_none());
    assert_eq!(depacketizer.stats().duplicates, 1);

    let mut conflicting = packets[0].to_vec();
    let last = conflicting.len() - 1;
    conflicting[last] ^= 0xff;
    assert!(matches!(
        depacketizer.push(&conflicting),
        Err(RtpError::ConflictingDuplicate(0))
    ));
}

#[test]
fn payload_type_filter_and_limits() {
    let mut packetizer = packetizer(1200, false);
    let packets = packetizer
        .packetize(&annex_b(&[&delta(20)]), 0, false)
        .unwrap();
    let mut depacketizer = H264Depacketizer::with_config(DepacketizerConfig {
        payload_type: Some(97),
        ..DepacketizerConfig::default()
    });
    assert!(matches!(
        depacketizer.push(&packets[0]),
        Err(RtpError::UnexpectedPayloadType {
            found: 96,
            expected: 97
        })
    ));
}

#[test]
fn packetizer_rejects_non_annex_b_and_small_mtu() {
    let mut packetizer = packetizer(1200, false);
    assert!(matches!(
        packetizer.packetize(b"not h264", 0, false),
        Err(RtpError::NotAnnexB)
    ));
    assert!(H264Packetizer::new(1, 8).is_err());
}

#[test]
fn fu_a_missing_start_reports_malformed_access_unit() {
    // A middle FU-A fragment followed by a marker is not a valid access unit.
    let mut middle = vec![0x80, 96, 0, 1, 0, 0, 0, 1, 1, 2, 3, 4];
    middle.extend_from_slice(&[0x7c, 0x05, 0xaa, 0xbb]);
    let mut marker = middle.clone();
    marker[2] = 0;
    marker[3] = 2;
    marker[1] |= 0x80;
    marker[13] = 0x45;

    let mut depacketizer = H264Depacketizer::new();
    assert!(depacketizer.push(&middle).unwrap().is_none());
    assert!(matches!(
        depacketizer.push(&marker),
        Err(RtpError::InvalidPayload(_))
    ));
    assert_eq!(depacketizer.stats().dropped_access_units, 1);
}
