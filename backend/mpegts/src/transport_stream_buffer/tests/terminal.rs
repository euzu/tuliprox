use super::*;

#[test]
fn runtime_custom_assets_use_exact_per_pid_presentation_duration() {
    let fixtures: [(&str, &[u8]); 6] = [
        (
            "channel_unavailable.ts",
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"))
                .as_slice(),
        ),
        (
            "hls_session_or_lease_expired.ts",
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../test/fixtures/hls/hls_session_or_lease_expired.ts"
            ))
            .as_slice(),
        ),
        (
            "low_priority_preempted.ts",
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/low_priority_preempted.ts"))
                .as_slice(),
        ),
        (
            "provider_connections_exhausted.ts",
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../test/fixtures/hls/provider_connections_exhausted.ts"
            ))
            .as_slice(),
        ),
        (
            "user_account_expired.ts",
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/user_account_expired.ts"))
                .as_slice(),
        ),
        (
            "user_connections_exhausted.ts",
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../test/fixtures/hls/user_connections_exhausted.ts"
            ))
            .as_slice(),
        ),
    ];
    for (name, bytes) in fixtures {
        let buffer = TransportStreamBuffer::new(bytes.to_vec());
        assert_eq!(buffer.duration_ticks_90khz(), Some(902_400), "{name} tick duration");
        assert_eq!(buffer.duration_ms(), Some(10_027), "{name} rounded HLS duration");
    }
}

#[test]
fn custom_asset_stride_does_not_overlap_previous_audio_presentation() {
    let buffer = TransportStreamBuffer::new(
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts")).to_vec(),
    );
    let duration = buffer.duration_ticks_90khz().expect("exact custom asset duration");
    let segment_zero = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 0,
            continuity_seed: 0,
            logical_segment_index: 0,
        })
        .expect("segment zero");
    let segment_one = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: duration,
            continuity_seed: 0,
            logical_segment_index: 1,
        })
        .expect("segment one");
    let zero = TransportStreamBuffer::new(segment_zero.to_vec());
    let one = TransportStreamBuffer::new(segment_one.to_vec());
    let zero_audio = zero
        .finite_hls_presentation_duration
        .as_ref()
        .and_then(|duration| duration.timelines.iter().find(|timeline| timeline.cadence_ticks_90khz == 1_920))
        .expect("segment zero audio timeline");
    let one_audio = one
        .finite_hls_presentation_duration
        .as_ref()
        .and_then(|duration| duration.timelines.iter().find(|timeline| timeline.cadence_ticks_90khz == 1_920))
        .expect("segment one audio timeline");

    assert_eq!(duration, 902_400);
    assert_ne!(duration, 901_650);
    assert!(one_audio.first_pts_90khz >= zero_audio.end_exclusive_90khz);
    assert!(
        zero_audio.first_pts_90khz.saturating_add(901_650) < zero_audio.end_exclusive_90khz,
        "the previous mixed-clock stride would overlap the final audio presentation"
    );
}

#[test]
fn terminal_splice_anchor_places_first_terminal_clock_after_live_tail() {
    let live_last_clock_90khz = 1_003_618_800;
    let live = HlsTsTimestampProfile {
        first_clock_90khz: live_last_clock_90khz - 900_000,
        last_clock_90khz: live_last_clock_90khz,
        span_ticks_90khz: 900_000,
        observed_pts_or_dts: true,
        observed_pcr: true,
    };
    let terminal = HlsTsTimestampProfile {
        first_clock_90khz: 0,
        last_clock_90khz: 902_400,
        span_ticks_90khz: 902_400,
        observed_pts_or_dts: true,
        observed_pcr: true,
    };

    let anchor = HlsTsSpliceAnchor::between(live, terminal).expect("valid modular splice");

    assert_eq!(
        forward_clock_distance_90khz(live_last_clock_90khz, anchor.terminal_first_clock),
        HLS_TS_SPLICE_MIN_GAP_TICKS_90KHZ
    );
    assert_eq!(anchor.timestamp_delta_ticks, anchor.terminal_first_clock);
    assert!(
        forward_clock_distance_90khz(live_last_clock_90khz, anchor.terminal_first_clock) < 90_000,
        "splice must not contain the observed near-full-cycle jump"
    );
}

#[test]
fn terminal_splice_preserves_terminal_asset_audio_video_offset() {
    let audio = build_pts_dts_payload_packet(0x0102, 0, 0, 0);
    let video = build_pts_dts_payload_packet(0x0101, 0, 1_920, 1_920);
    let mut raw = Vec::new();
    raw.extend_from_slice(&audio);
    raw.extend_from_slice(&video);
    raw.extend_from_slice(&build_pts_dts_payload_packet(0x0102, 1, 1_920, 1_920));
    raw.extend_from_slice(&build_pts_dts_payload_packet(0x0101, 1, 4_920, 4_920));
    let buffer = TransportStreamBuffer::new(raw);
    let asset_profile = buffer.finite_hls_timestamp_profile().expect("asset timestamp profile");
    let live = HlsTsTimestampProfile {
        first_clock_90khz: 1_003_000_000,
        last_clock_90khz: 1_003_618_800,
        span_ticks_90khz: 618_800,
        observed_pts_or_dts: true,
        observed_pcr: false,
    };
    let anchor = HlsTsSpliceAnchor::between(live, asset_profile).expect("splice anchor");
    let prepared = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 0,
            continuity_seed: 0,
            logical_segment_index: 0,
        })
        .expect("relative segment");
    let anchored = buffer
        .finalize_prepared_finite_hls_segment(
            &prepared,
            HlsFiniteTsFinalizeSpec {
                additional_timestamp_offset_ticks_90khz: anchor.timestamp_delta_ticks,
                discontinuity: HlsFiniteTsDiscontinuityMode::None,
            },
        )
        .expect("anchored segment");
    let audio_pts = decode_timestamp(&anchored[13..18]);
    let video_pts = decode_timestamp(&anchored[TS_PACKET_SIZE + 13..TS_PACKET_SIZE + 18]);

    assert_eq!(forward_clock_distance_90khz(audio_pts, video_pts), 1_920);
}

#[test]
fn terminal_splice_anchor_wraps_pts_dts_and_pcr_exactly() {
    let original_clock = 10;
    let packet = build_pts_dts_pcr_packet(0x0100, 7, original_clock, original_clock, original_clock);
    let next_packet = build_pts_dts_pcr_packet(0x0100, 8, 3_010, 3_010, 3_010);
    let mut raw = packet.to_vec();
    raw.extend_from_slice(&next_packet);
    let buffer = TransportStreamBuffer::new(raw);
    let asset_profile = buffer.finite_hls_timestamp_profile().expect("asset timestamp profile");
    let live_last_clock_90khz = MAX_PTS_DTS - 40;
    let live = HlsTsTimestampProfile {
        first_clock_90khz: live_last_clock_90khz - 90_000,
        last_clock_90khz: live_last_clock_90khz,
        span_ticks_90khz: 90_000,
        observed_pts_or_dts: true,
        observed_pcr: true,
    };
    let anchor = HlsTsSpliceAnchor::between(live, asset_profile).expect("wrapped splice anchor");
    let prepared = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 0,
            continuity_seed: 0,
            logical_segment_index: 0,
        })
        .expect("relative segment");
    let anchored = buffer
        .finalize_prepared_finite_hls_segment(
            &prepared,
            HlsFiniteTsFinalizeSpec {
                additional_timestamp_offset_ticks_90khz: anchor.timestamp_delta_ticks,
                discontinuity: HlsFiniteTsDiscontinuityMode::None,
            },
        )
        .expect("wrapped anchored segment");

    assert_eq!(anchor.terminal_first_clock, 50);
    assert_eq!(packet_timestamps(&anchored), (50, 50, 50));
    assert_eq!(forward_clock_distance_90khz(live_last_clock_90khz, 50), 90);
}

#[test]
fn first_terminal_segment_marks_discontinuity_for_each_non_null_pid() {
    let mut raw = Vec::new();
    raw.extend_from_slice(&build_payload_packet(0x0000, 9));
    raw.extend_from_slice(&build_payload_packet(0x0100, 3));
    raw.extend_from_slice(&build_pts_dts_payload_packet(0x0101, 7, 90_000, 87_000));
    raw.extend_from_slice(&build_pts_dts_payload_packet(0x0102, 5, 90_000, 90_000));
    raw.extend_from_slice(&build_pts_dts_pcr_packet(0x0103, 4, 90_000, 90_000, 90_000));
    raw.extend_from_slice(&build_payload_packet(NULL_PID, 12));
    let buffer = TransportStreamBuffer::new(raw);
    let prepared = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 0,
            continuity_seed: 2,
            logical_segment_index: 0,
        })
        .expect("relative segment");
    let finalized = buffer
        .finalize_prepared_finite_hls_segment(
            &prepared,
            HlsFiniteTsFinalizeSpec {
                additional_timestamp_offset_ticks_90khz: 90,
                discontinuity: HlsFiniteTsDiscontinuityMode::FirstPacketPerPid,
            },
        )
        .expect("splice segment");

    let packets = finalized.as_chunks::<TS_PACKET_SIZE>().0.iter().collect::<Vec<_>>();
    let mut marked_pids = Vec::new();
    for pair in packets.windows(2) {
        let marker = pair[0];
        let following = pair[1];
        if (marker[3] >> 4) & 0b11 == 0b10 && marker[4] == 183 && marker[5] & 0x80 != 0 {
            assert_eq!(ts_packet_pid(marker), ts_packet_pid(following));
            assert_eq!((marker[3] & 0x0F).wrapping_add(1) & 0x0F, following[3] & 0x0F);
            marked_pids.push(ts_packet_pid(marker));
        }
    }
    marked_pids.sort_unstable();

    assert_eq!(marked_pids, vec![0x0000, 0x0100, 0x0101, 0x0102, 0x0103]);
    assert!(!marked_pids.contains(&NULL_PID));
}

#[test]
fn finalized_terminal_asset_preserves_tracks_and_ffmpeg_continuity() {
    const TERMINAL_ASSET_BYTES: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));
    let buffer = TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
    let expected_signature = buffer.finite_hls_track_signature().expect("terminal asset track signature");
    let prepared = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 0,
            continuity_seed: 0,
            logical_segment_index: 0,
        })
        .expect("relative terminal asset");
    let finalized = buffer
        .finalize_prepared_finite_hls_segment(
            &prepared,
            HlsFiniteTsFinalizeSpec {
                additional_timestamp_offset_ticks_90khz: 1_003_618_890,
                discontinuity: HlsFiniteTsDiscontinuityMode::FirstPacketPerPid,
            },
        )
        .expect("anchored terminal asset");

    assert_ffmpeg_compatible_continuity(&finalized);
    let outcome = crate::ts_inspector::inspect_mpeg_ts(
        std::io::Cursor::new(finalized),
        crate::ts_inspector::HlsTsProbeProtection::Clear,
        crate::ts_inspector::HlsTsProbeBudget::default(),
    )
    .expect("finalized terminal asset remains inspectable");
    assert_eq!(outcome, crate::ts_inspector::HlsTsProbeOutcome::Found(expected_signature));
}

#[test]
fn prepared_terminal_finalization_rejects_packet_layout_mismatch() {
    let packet = build_pts_dts_pcr_packet(0x0100, 0, 90_000, 87_000, 86_000);
    let buffer = TransportStreamBuffer::new(packet.to_vec());
    let prepared = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 0,
            continuity_seed: 0,
            logical_segment_index: 0,
        })
        .expect("relative terminal segment");
    let mut mismatched = BytesMut::from(prepared.as_ref());
    mismatched[2] ^= 1;

    assert_eq!(
        buffer.finalize_prepared_finite_hls_segment(
            &mismatched.freeze(),
            HlsFiniteTsFinalizeSpec {
                additional_timestamp_offset_ticks_90khz: 1,
                discontinuity: HlsFiniteTsDiscontinuityMode::FirstPacketPerPid,
            },
        ),
        Err(HlsFiniteTsRenderError::PreparedLayoutMismatch)
    );
}

#[test]
fn later_terminal_segments_continue_cc_without_repeating_splice_marker() {
    let first = build_pts_dts_pcr_packet(0x0101, 7, 90_000, 87_000, 90_000);
    let second = build_pts_dts_payload_packet(0x0102, 5, 90_000, 90_000);
    let mut raw = Vec::new();
    raw.extend_from_slice(&first);
    raw.extend_from_slice(&second);
    raw.extend_from_slice(&build_pts_dts_pcr_packet(0x0101, 8, 180_000, 177_000, 180_000));
    raw.extend_from_slice(&build_pts_dts_payload_packet(0x0102, 6, 180_000, 180_000));
    let buffer = TransportStreamBuffer::new(raw);
    let duration = buffer.duration_ticks_90khz().expect("asset duration");
    let relative_zero = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 0,
            continuity_seed: 0,
            logical_segment_index: 0,
        })
        .expect("relative zero");
    let relative_one = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: duration,
            continuity_seed: 0,
            logical_segment_index: 1,
        })
        .expect("relative one");
    let finalized_zero = buffer
        .finalize_prepared_finite_hls_segment(
            &relative_zero,
            HlsFiniteTsFinalizeSpec {
                additional_timestamp_offset_ticks_90khz: 1_000_000,
                discontinuity: HlsFiniteTsDiscontinuityMode::FirstPacketPerPid,
            },
        )
        .expect("anchored zero");
    let finalized_one = buffer
        .finalize_prepared_finite_hls_segment(
            &relative_one,
            HlsFiniteTsFinalizeSpec {
                additional_timestamp_offset_ticks_90khz: 1_000_000,
                discontinuity: HlsFiniteTsDiscontinuityMode::None,
            },
        )
        .expect("anchored one");

    assert_eq!(finalized_one.len(), relative_one.len());
    assert!(finalized_one
        .as_chunks::<TS_PACKET_SIZE>()
        .0
        .iter()
        .all(|packet| !((packet[3] >> 4) & 0b11 == 0b10 && packet[4] == 183 && packet[5] & 0x80 != 0)));
    let zero_payload = finalized_zero
        .as_chunks::<TS_PACKET_SIZE>()
        .0
        .iter()
        .filter(|packet| (packet[3] >> 4) & 0b11 != 0b10)
        .collect::<Vec<_>>();
    let one_payload = finalized_one.as_chunks::<TS_PACKET_SIZE>().0.iter().collect::<Vec<_>>();
    for (zero, one) in zero_payload.into_iter().zip(one_payload) {
        assert_eq!(ts_packet_pid(zero), ts_packet_pid(one));
        assert_eq!((zero[3] & 0x0F).wrapping_add(2) & 0x0F, one[3] & 0x0F);
    }
    let mut concatenated = finalized_zero.to_vec();
    concatenated.extend_from_slice(&finalized_one);
    assert_ffmpeg_compatible_continuity(&concatenated);
}

#[test]
fn live_to_terminal_splice_has_monotone_pts_dts_pcr_and_valid_cc_reset() {
    let live_first_clock = 1_003_528_800;
    let live_last_clock = 1_003_618_800;
    let mut live_bytes = Vec::new();
    live_bytes.extend_from_slice(&build_pts_dts_pcr_packet(
        0x0101,
        13,
        live_first_clock,
        live_first_clock,
        live_first_clock,
    ));
    live_bytes.extend_from_slice(&build_pts_dts_pcr_packet(
        0x0101,
        14,
        live_last_clock,
        live_last_clock,
        live_last_clock,
    ));
    let live_buffer = TransportStreamBuffer::new(live_bytes.clone());
    let live_profile = live_buffer.finite_hls_timestamp_profile().expect("live tail profile");

    let mut terminal_bytes = Vec::new();
    terminal_bytes.extend_from_slice(&build_pts_dts_pcr_packet(0x0101, 0, 0, 0, 0));
    terminal_bytes.extend_from_slice(&build_pts_dts_pcr_packet(0x0101, 1, 90_000, 90_000, 90_000));
    let terminal_buffer = TransportStreamBuffer::new(terminal_bytes);
    let terminal_profile = terminal_buffer.finite_hls_timestamp_profile().expect("terminal asset profile");
    let anchor = HlsTsSpliceAnchor::between(live_profile, terminal_profile).expect("splice anchor");
    let duration = terminal_buffer.duration_ticks_90khz().expect("terminal duration");
    let relative_zero = terminal_buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 0,
            continuity_seed: 0,
            logical_segment_index: 0,
        })
        .expect("terminal zero");
    let relative_one = terminal_buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: duration,
            continuity_seed: 0,
            logical_segment_index: 1,
        })
        .expect("terminal one");
    let terminal_zero = terminal_buffer
        .finalize_prepared_finite_hls_segment(
            &relative_zero,
            HlsFiniteTsFinalizeSpec {
                additional_timestamp_offset_ticks_90khz: anchor.timestamp_delta_ticks,
                discontinuity: HlsFiniteTsDiscontinuityMode::FirstPacketPerPid,
            },
        )
        .expect("anchored terminal zero");
    let terminal_one = terminal_buffer
        .finalize_prepared_finite_hls_segment(
            &relative_one,
            HlsFiniteTsFinalizeSpec {
                additional_timestamp_offset_ticks_90khz: anchor.timestamp_delta_ticks,
                discontinuity: HlsFiniteTsDiscontinuityMode::None,
            },
        )
        .expect("anchored terminal one");

    let zero_packets = terminal_zero.as_chunks::<TS_PACKET_SIZE>().0.iter().collect::<Vec<_>>();
    assert_eq!((zero_packets[0][3] >> 4) & 0b11, 0b10);
    assert_eq!(zero_packets[0][5] & 0x80, 0x80);
    assert_eq!((zero_packets[0][3] & 0x0F).wrapping_add(1) & 0x0F, zero_packets[1][3] & 0x0F);
    let terminal_one_packets = terminal_one.as_chunks::<TS_PACKET_SIZE>().0;
    assert!(terminal_one_packets.iter().all(|packet| !((packet[3] >> 4) & 0b11 == 0b10 && packet[5] & 0x80 != 0)));

    let zero_first = packet_timestamps(zero_packets[1]);
    let zero_last = packet_timestamps(zero_packets[2]);
    let one_first = packet_timestamps(&terminal_one[..TS_PACKET_SIZE]);
    assert_eq!(forward_clock_distance_90khz(live_last_clock, zero_first.0), 90);
    assert!(forward_clock_distance_90khz(zero_first.0, zero_last.0) < MAX_PTS_DTS / 2);
    assert!(forward_clock_distance_90khz(zero_last.0, one_first.0) < MAX_PTS_DTS / 2);
    assert_eq!(forward_clock_distance_90khz(zero_last.0, one_first.0), duration.saturating_sub(90_000));
    assert_eq!(zero_first.0, zero_first.1);
    assert_eq!(zero_first.0, zero_first.2);
    assert_eq!(one_first.0, one_first.1);
    assert_eq!(one_first.0, one_first.2);

    let mut concatenated = live_bytes;
    concatenated.extend_from_slice(&terminal_zero);
    concatenated.extend_from_slice(&terminal_one);
    assert_ffmpeg_compatible_continuity(&concatenated);
    let mut scanner = HlsTsTimestampProfileScanner::new(duration.saturating_mul(3));
    for packet in concatenated.as_chunks::<TS_PACKET_SIZE>().0 {
        scanner.push_aligned_packet(packet);
    }
    let combined = scanner.finish().expect("combined splice profile");
    assert!(combined.span_ticks_90khz < duration.saturating_mul(3));
}

#[test]
fn hls_prepared_terminal_bundle_pcr_uses_300_factor_and_large_offsets_wrap_exactly() {
    let base_pcr_90khz = 123_456;
    let large_offset_90khz = u64::MAX - 37;
    let packet = build_pts_dts_pcr_packet(0x0100, 7, 90_000, 87_000, base_pcr_90khz);
    let buffer = TransportStreamBuffer::new(packet.to_vec());

    let rendered = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: large_offset_90khz,
            continuity_seed: 0,
            logical_segment_index: 11,
        })
        .expect("finite segment with a large timestamp offset");

    let actual_pcr_27mhz = decode_pcr(&rendered[6..12]);
    let expected_offset_27mhz = (u128::from(large_offset_90khz) * 300_u128) % u128::from(MAX_PCR);
    let expected_pcr_27mhz =
        u64::try_from((u128::from(base_pcr_90khz) * 300_u128 + expected_offset_27mhz) % u128::from(MAX_PCR))
            .expect("PCR modulo fits in u64");
    let legacy_wrapped_offset = large_offset_90khz.wrapping_mul(300) % MAX_PCR;

    assert_eq!(pcr_offset_27mhz(1), 300);
    assert_ne!(
        pcr_offset_27mhz(large_offset_90khz),
        legacy_wrapped_offset,
        "large offsets must be multiplied before modulo without u64 wrap"
    );
    assert_eq!(actual_pcr_27mhz, expected_pcr_27mhz);
}

#[test]
fn hls_prepared_terminal_bundle_pts_and_dts_large_offsets_wrap_modulo_33_bits() {
    let presentation_timestamp = MAX_PTS_DTS - 123;
    let decoding_timestamp = MAX_PTS_DTS - 456;
    let large_offset_90khz = u64::MAX - 37;
    let packet = build_pts_dts_pcr_packet(0x0100, 7, presentation_timestamp, decoding_timestamp, 90_000);
    let buffer = TransportStreamBuffer::new(packet.to_vec());

    let rendered = buffer
        .render_finite_hls_segment(HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: large_offset_90khz,
            continuity_seed: 0,
            logical_segment_index: 11,
        })
        .expect("finite segment with wrapped PTS and DTS");

    let expected_timestamp = |timestamp| {
        u64::try_from((u128::from(timestamp) + u128::from(large_offset_90khz)) % u128::from(MAX_PTS_DTS))
            .expect("PTS/DTS modulo fits in u64")
    };
    let (rendered_presentation_timestamp, rendered_decoding_timestamp, _) = packet_timestamps(&rendered);

    assert_eq!(rendered_presentation_timestamp, expected_timestamp(presentation_timestamp));
    assert_eq!(rendered_decoding_timestamp, expected_timestamp(decoding_timestamp));
}
