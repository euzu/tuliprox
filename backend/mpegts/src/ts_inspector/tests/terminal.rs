use super::*;

#[tokio::test]
async fn current_terminal_asset_and_finalized_segment_zero_have_complete_splice_evidence() {
    const TERMINAL_ASSET_BYTES: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));
    let renderer = crate::transport_stream_buffer::TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
    let duration_ticks = renderer.duration_ticks_90khz().expect("terminal asset duration");
    let asset_profile = renderer.finite_hls_timestamp_profile().expect("terminal asset profile");
    let prepared = renderer
        .render_finite_hls_segment(crate::transport_stream_buffer::HlsFiniteTsRenderSpec {
            timestamp_offset_ticks_90khz: 0,
            continuity_seed: 0,
            logical_segment_index: 0,
        })
        .expect("prepared terminal segment zero");
    let finalized = renderer
        .finalize_prepared_finite_hls_segment(
            &prepared,
            crate::transport_stream_buffer::HlsFiniteTsFinalizeSpec {
                additional_timestamp_offset_ticks_90khz: 90_000,
                discontinuity: crate::transport_stream_buffer::HlsFiniteTsDiscontinuityMode::FirstPacketPerPid,
            },
        )
        .expect("finalized terminal segment zero");

    let base = complete_media_evidence_with_duration(TERMINAL_ASSET_BYTES, duration_ticks).await;
    let terminal = complete_media_evidence_with_duration(&finalized, duration_ticks).await;

    assert_eq!(base.timestamp_profile, Some(asset_profile));
    assert!(matches!(base.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));
    assert!(matches!(terminal.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));
    assert_eq!(evaluate_mpeg_ts_splice_boundary(&base.splice_evidence, &terminal.splice_evidence), Ok(()));
    assert_eq!(
        terminal.timestamp_profile.map(|profile| profile.span_ticks_90khz),
        Some(asset_profile.span_ticks_90khz)
    );
    assert!(duration_ticks > asset_profile.span_ticks_90khz);
}

#[tokio::test]
async fn aes128_terminal_base_timestamp_profile_uses_decrypted_cached_bytes() {
    const TERMINAL_ASSET_BYTES: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));
    let asset = crate::transport_stream_buffer::TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
    let expected_profile = asset.finite_hls_timestamp_profile().expect("fixture timestamp profile");
    let expected_signature = asset.finite_hls_track_signature().expect("fixture track signature");
    let key = *b"0123456789abcdef";
    let iv = [0xA5; AES_128_BLOCK_BYTES];
    let ciphertext = encrypt_aes128_cbc_pkcs7(TERMINAL_ASSET_BYTES, &key, iv);
    let source_size = u64::try_from(ciphertext.len()).expect("ciphertext size");
    let budget = HlsTsProbeBudget {
        max_bytes: source_size.saturating_add(1),
        max_packets: source_size.saturating_add(187) / 188 + 1,
        ..HlsTsProbeBudget::default()
    };

    let evidence = inspect_mpeg_ts_media_evidence_async(
        &ciphertext[..],
        HlsTsProbeProtection::Aes128Cbc { key: &key, iv },
        budget,
        asset.duration_ticks_90khz().expect("fixture duration"),
    )
    .await
    .expect("AES media evidence");

    assert_eq!(evidence.track_outcome, HlsTsProbeOutcome::Found(expected_signature));
    assert_eq!(evidence.timestamp_profile, Some(expected_profile));
    assert!(matches!(evidence.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));
}

#[test]
fn ts_inspector_rejects_invalid_psi_header_before_exhausting_probe_budget() {
    let mut invalid_pat = pat_section(&[(1, 0x100)]);
    invalid_pat[1] &= 0x7F;
    invalid_pat[1] |= 0x0F;
    invalid_pat[2] = 0xFF;
    let mut stream = packetize_section(0, &invalid_pat, 0);
    let null_packet = null_packet();
    while stream.len() <= usize::try_from(HLS_TS_PROBE_MAX_BYTES).unwrap_or(usize::MAX) {
        stream.extend_from_slice(&null_packet);
    }

    assert_eq!(
        inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("probe completes"),
        HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPat)
    );
}
