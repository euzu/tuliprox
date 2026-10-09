use super::*;

#[test]
fn finite_discontinuity_marker_sets_ffmpeg_compatible_counter_baseline() {
    let cases = [
        ("payload wrap", build_payload_packet(0x0100, 0), 15),
        ("payload increment", build_payload_packet(0x0101, 7), 6),
        ("adaptation only", build_adaptation_only_packet(0x0102, 5), 5),
    ];
    for (name, following, expected_marker_cc) in cases {
        let mut output = BytesMut::new();
        append_finite_discontinuity_packet(&following, &mut output);
        let marker = &output[..TS_PACKET_SIZE];

        assert_eq!((marker[3] >> 4) & 0b11, 0b10, "{name} marker AFC");
        assert_eq!(marker[5] & 0x80, 0x80, "{name} discontinuity");
        assert_eq!(marker[3] & 0x0F, expected_marker_cc, "{name} marker CC");

        output.extend_from_slice(&following);
        assert_ffmpeg_compatible_continuity(&output);
    }
}

#[test]
fn discontinuity_packet_does_not_advance_payload_cc() {
    let packet = build_pts_dts_payload_packet(0x0100, 7, 90_000, 87_000);
    let mut buf = TransportStreamBuffer::new(packet.to_vec());
    let chunk = buf.next_chunk().expect("expected chunk");
    assert!(chunk.len() >= TS_PACKET_SIZE * 2);

    // First emitted packet is injected discontinuity (adaptation-only),
    // second is the actual payload packet for the same PID.
    let disc_cc = chunk[3] & 0x0F;
    let disc_afc = (chunk[3] >> 4) & 0b11;
    let payload_cc = chunk[TS_PACKET_SIZE + 3] & 0x0F;
    assert_eq!(disc_afc, 0b10);
    assert_eq!(disc_cc, payload_cc);
}

#[test]
fn adaptation_only_packets_keep_same_continuity_counter() {
    let packet = build_adaptation_only_packet(0x0011, 5);
    let mut buf = TransportStreamBuffer::new(packet.to_vec());
    let chunk = buf.next_chunk().expect("expected chunk");
    // Each of the 7 loop iterations emits a discontinuity packet + the actual packet
    // because the single-packet buffer loops on every iteration.
    let total_packets = PACKET_COUNT * 2;
    assert_eq!(chunk.len(), TS_PACKET_SIZE * total_packets);

    for i in 0..total_packets {
        let cc = chunk[i * TS_PACKET_SIZE + 3] & 0x0F;
        assert_eq!(cc, 5, "packet {i} CC mismatch");
    }
}

#[test]
fn finite_hls_segments_are_deterministic_aligned_and_timestamp_shifted() {
    let first = build_pts_dts_payload_packet(0x0100, 7, 90_000, 87_000);
    let second = build_pts_dts_payload_packet(0x0100, 8, 180_000, 177_000);
    let mut raw = Vec::new();
    raw.extend_from_slice(&first);
    raw.extend_from_slice(&second);
    let buffer = TransportStreamBuffer::new(raw);
    let spec =
        HlsFiniteTsRenderSpec { timestamp_offset_ticks_90khz: 900_000, continuity_seed: 3, logical_segment_index: 0 };

    let rendered = buffer.render_finite_hls_segment(spec).expect("finite render");
    let repeated = buffer.render_finite_hls_segment(spec).expect("repeated finite render");

    assert_eq!(rendered, repeated);
    assert_eq!(rendered.len() % TS_PACKET_SIZE, 0);
    assert_eq!(rendered[3] & 0x0F, 3);
    assert_eq!(rendered[TS_PACKET_SIZE + 3] & 0x0F, 4);
    assert_eq!(decode_timestamp(&rendered[13..18]), 990_000);
    assert_eq!(decode_timestamp(&rendered[18..23]), 987_000);
}

#[test]
fn consecutive_finite_hls_segments_advance_cc_pts_dts_and_pcr() {
    let first = build_pts_dts_pcr_packet(0x0100, 7, 90_000, 87_000, 90_000);
    let second = build_pts_dts_pcr_packet(0x0100, 8, 180_000, 177_000, 180_000);
    let mut raw = Vec::new();
    raw.extend_from_slice(&first);
    raw.extend_from_slice(&second);
    let buffer = TransportStreamBuffer::new(raw);
    let duration = buffer.duration_ticks_90khz().expect("transportable duration");

    let segment_zero = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 0,
            continuity_seed: 3,
            logical_segment_index: 0,
        })
        .expect("first segment");
    let segment_one = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: duration,
            continuity_seed: 3,
            logical_segment_index: 1,
        })
        .expect("second segment");

    assert_eq!(segment_zero[TS_PACKET_SIZE + 3] & 0x0F, 4);
    assert_eq!(segment_one[3] & 0x0F, 5);
    assert_eq!(segment_one[TS_PACKET_SIZE + 3] & 0x0F, 6);

    let zero_last = packet_timestamps(&segment_zero[TS_PACKET_SIZE..]);
    let one_first = packet_timestamps(&segment_one[..TS_PACKET_SIZE]);
    assert!(one_first.0 > zero_last.0, "PTS must advance across segment boundary");
    assert!(one_first.1 > zero_last.1, "DTS must advance across segment boundary");
    assert!(one_first.2 > zero_last.2, "PCR must advance across segment boundary");
}

#[test]
fn finite_hls_timestamp_rewrite_wraps_pts_dts_and_pcr_without_overflow() {
    let packet = build_pts_dts_pcr_packet(
        0x0100,
        7,
        MAX_PTS_DTS.saturating_sub(10),
        MAX_PTS_DTS.saturating_sub(20),
        MAX_PTS_DTS.saturating_sub(10),
    );
    let buffer = TransportStreamBuffer::new(packet.to_vec());

    let rendered = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 30,
            continuity_seed: 0,
            logical_segment_index: 0,
        })
        .expect("finite wrapped segment");

    assert_eq!(packet_timestamps(&rendered), (20, 10, 20));
}

#[test]
fn split_pts_layout_profile_relative_and_anchored_rewrite_are_exact() {
    let mut packets = build_split_pes_header_packets(0x0101, 0, &pts_only_pes_header(90_000), &[12, 2]);
    packets.push(build_pts_only_payload_packet(0x0101, 2, 93_000));
    let raw = packets.into_iter().flatten().collect::<Vec<_>>();
    let buffer = TransportStreamBuffer::new(raw);
    let layout = buffer.finite_hls_layout.as_ref().expect("split PTS layout");
    let pts_fields = layout
        .timestamp_fields
        .iter()
        .copied()
        .filter(|field| field.kind == HlsTsTimestampFieldKind::Pts)
        .collect::<Vec<_>>();
    assert_eq!(pts_fields.len(), 2);
    let split_field = pts_fields[0];
    assert_ne!(split_field.byte_offsets[0] / TS_PACKET_SIZE, split_field.byte_offsets[4] / TS_PACKET_SIZE);
    assert_eq!(
        buffer.finite_hls_timestamp_profile(),
        Some(HlsTsTimestampProfile {
            first_clock_90khz: 90_000,
            last_clock_90khz: 93_000,
            span_ticks_90khz: 3_000,
            observed_pts_or_dts: true,
            observed_pcr: false,
        })
    );

    let relative = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 7_000,
            continuity_seed: 0,
            logical_segment_index: 0,
        })
        .expect("relative split PTS");
    assert_eq!(decode_timestamp_at_location(&relative, split_field).expect("relative split PTS value"), 97_000);
    let anchored = buffer
        .finalize_prepared_finite_hls_segment(
            &relative,
            HlsFiniteTsFinalizeSpec {
                additional_timestamp_offset_ticks_90khz: 11_000,
                discontinuity: HlsFiniteTsDiscontinuityMode::None,
            },
        )
        .expect("anchored split PTS");
    assert_eq!(decode_timestamp_at_location(&anchored, split_field).expect("anchored split PTS value"), 108_000);
    assert_ne!(decode_timestamp_at_location(&anchored, split_field).expect("rewritten split PTS"), 90_000);
    let encoded = gather_timestamp_bytes(&anchored, split_field).expect("anchored split PTS bytes");
    assert!(encoded[0] & 1 != 0 && encoded[2] & 1 != 0 && encoded[4] & 1 != 0);
}

#[test]
fn split_pts_dts_layout_rewrites_both_fields_across_distinct_packet_boundaries() {
    let packets = build_split_pes_header_packets(0x0101, 5, &pts_dts_pes_header(90_000, 87_000), &[11, 5, 3]);
    let raw = packets.into_iter().flatten().collect::<Vec<_>>();
    let buffer = TransportStreamBuffer::new(raw);
    let layout = buffer.finite_hls_layout.as_ref().expect("split PTS+DTS layout");
    let pts = layout
        .timestamp_fields
        .iter()
        .copied()
        .find(|field| field.kind == HlsTsTimestampFieldKind::Pts)
        .expect("split PTS location");
    let dts = layout
        .timestamp_fields
        .iter()
        .copied()
        .find(|field| field.kind == HlsTsTimestampFieldKind::Dts)
        .expect("split DTS location");
    assert_ne!(pts.byte_offsets[0] / TS_PACKET_SIZE, pts.byte_offsets[4] / TS_PACKET_SIZE);
    assert_ne!(dts.byte_offsets[0] / TS_PACKET_SIZE, dts.byte_offsets[4] / TS_PACKET_SIZE);
    assert_ne!(pts.byte_offsets[0] / TS_PACKET_SIZE, dts.byte_offsets[0] / TS_PACKET_SIZE);

    let relative = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 1_000,
            continuity_seed: 0,
            logical_segment_index: 0,
        })
        .expect("relative split PTS+DTS");
    let anchored = buffer
        .finalize_prepared_finite_hls_segment(
            &relative,
            HlsFiniteTsFinalizeSpec {
                additional_timestamp_offset_ticks_90khz: 2_000,
                discontinuity: HlsFiniteTsDiscontinuityMode::None,
            },
        )
        .expect("anchored split PTS+DTS");
    assert_eq!(decode_timestamp_at_location(&anchored, pts), Ok(93_000));
    assert_eq!(decode_timestamp_at_location(&anchored, dts), Ok(90_000));
    for field in [pts, dts] {
        let encoded = gather_timestamp_bytes(&anchored, field).expect("split timestamp bytes");
        assert!(encoded[0] & 1 != 0 && encoded[2] & 1 != 0 && encoded[4] & 1 != 0);
    }
}

#[test]
fn timestamp_profile_scanner_reads_split_pts_across_packets() {
    let mut packets = build_split_pes_header_packets(0x0101, 0, &pts_only_pes_header(90_000), &[12, 2]);
    packets.push(build_pts_only_payload_packet(0x0101, 2, 93_000));
    let mut scanner = HlsTsTimestampProfileScanner::new(10_000);
    for packet in packets {
        scanner.push_aligned_packet(&packet);
    }

    assert_eq!(
        scanner.finish(),
        Some(HlsTsTimestampProfile {
            first_clock_90khz: 90_000,
            last_clock_90khz: 93_000,
            span_ticks_90khz: 3_000,
            observed_pts_or_dts: true,
            observed_pcr: false,
        })
    );
}

#[test]
fn timestamp_profile_scanner_rejects_one_clock_sample() {
    let packet = build_pts_only_payload_packet(0x0101, 0, 90_000);
    let mut scanner = HlsTsTimestampProfileScanner::new(10_000);
    scanner.push_aligned_packet(&packet);

    assert_eq!(scanner.finish(), None);
}

#[test]
fn split_pts_incomplete_at_eof_fails_with_typed_layout_error() {
    let packets = build_split_pes_header_packets(0x0101, 0, &pts_only_pes_header(90_000), &[12, 2]);
    let buffer = TransportStreamBuffer::new(packets[0].to_vec());

    assert!(matches!(
        buffer.finite_hls_layout.as_ref(),
        Err(HlsFiniteTsLayoutError::IncompletePesTimestampHeader { pid: 0x0101 })
    ));
    assert_eq!(
        buffer.render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 1,
            continuity_seed: 0,
            logical_segment_index: 0,
        }),
        Err(HlsFiniteTsRenderError::InvalidAsset)
    );
    assert_eq!(buffer.duration_ticks_90khz(), None);
}

#[test]
fn split_pts_continuity_discontinuity_fails_with_typed_layout_error() {
    let mut packets = build_split_pes_header_packets(0x0101, 0, &pts_only_pes_header(90_000), &[12, 2]);
    packets[1][3] = (packets[1][3] & 0xF0) | 3;
    let raw = packets.into_iter().flatten().collect::<Vec<_>>();
    let buffer = TransportStreamBuffer::new(raw);

    assert!(matches!(
        buffer.finite_hls_layout.as_ref(),
        Err(HlsFiniteTsLayoutError::PesTimestampContinuityDiscontinuity { pid: 0x0101, expected: Some(1), actual: 3 })
    ));
}

#[test]
fn pcr_fallback_uses_observed_cadence_and_rejects_a_single_sample() {
    let pid = 0x0101;
    let packets = [build_pcr_only_packet(pid, 0, 90_000), build_pcr_only_packet(pid, 0, 91_920)];
    let buffer = TransportStreamBuffer::new(packets.into_iter().flatten().collect());
    let duration = buffer.finite_hls_presentation_duration.as_ref().expect("PCR fallback duration");

    assert_eq!(duration.source, HlsTsPresentationClockSource::PcrFallback);
    assert_eq!(duration.first_presentation_clock_90khz, 90_000);
    assert_eq!(duration.end_exclusive_clock_90khz, 93_840);
    assert_eq!(duration.duration_ticks_90khz, 3_840);
    assert_eq!(
        duration.timelines.as_ref(),
        [HlsTsPidPresentationTimeline {
            pid,
            first_pts_90khz: 90_000,
            last_pts_90khz: 91_920,
            cadence_ticks_90khz: 1_920,
            end_exclusive_90khz: 93_840,
        }]
    );

    let single = TransportStreamBuffer::new(build_pcr_only_packet(pid, 0, 90_000).to_vec());
    assert_eq!(single.duration_ticks_90khz(), None);
}

#[test]
fn timestamp_profile_scanner_unwraps_one_33_bit_boundary() {
    let before_wrap = MAX_PTS_DTS - 1_000;
    let packets = [
        build_pts_dts_pcr_packet(0x0100, 0, before_wrap + 200, before_wrap + 100, before_wrap),
        build_pts_dts_pcr_packet(0x0100, 1, 1_200, 1_100, 1_000),
    ];
    let mut scanner = HlsTsTimestampProfileScanner::new(5_000);
    for packet in packets {
        scanner.push_aligned_packet(&packet);
    }

    assert_eq!(
        scanner.finish(),
        Some(HlsTsTimestampProfile {
            first_clock_90khz: before_wrap,
            last_clock_90khz: 1_200,
            span_ticks_90khz: 2_200,
            observed_pts_or_dts: true,
            observed_pcr: true,
        })
    );
}

#[test]
fn timestamp_profile_scanner_rejects_implausible_backward_segment_jump() {
    let packets = [
        build_pts_dts_pcr_packet(0x0100, 0, 500_200, 500_100, 500_000),
        build_pts_dts_pcr_packet(0x0100, 1, 100_200, 100_100, 100_000),
    ];
    let mut scanner = HlsTsTimestampProfileScanner::new(90_000);
    for packet in packets {
        scanner.push_aligned_packet(&packet);
    }

    assert_eq!(scanner.finish(), None);
}
