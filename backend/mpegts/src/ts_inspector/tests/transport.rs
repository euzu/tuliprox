use super::*;

#[test]
fn ts_inspector_reads_single_packet_pat_and_pmt() {
    let signature = found(
        inspect_mpeg_ts(Cursor::new(track_stream(0)), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("probe succeeds"),
    );
    assert_eq!(signature.program_count, 1);
    assert!(signature.has_pcr);
    assert_eq!(signature.stream_types.as_ref(), &[0x0F, 0x1B]);
    assert_eq!(
        signature.programs.as_ref(),
        &[HlsTsProgramTopology {
            transport_stream_id: 1,
            program_number: 1,
            pmt_pid: 0x100,
            pcr_pid: 0x101,
            streams: Arc::from([
                HlsTsElementaryStreamBinding { stream_type: 0x1B, elementary_pid: 0x101 },
                HlsTsElementaryStreamBinding { stream_type: 0x0F, elementary_pid: 0x102 },
                HlsTsElementaryStreamBinding { stream_type: 0x0F, elementary_pid: 0x103 },
            ]),
        }]
    );
}

#[test]
fn equal_stream_types_with_different_pid_topology_are_not_compatible() {
    let mut first = packetize_section(0, &pat_section(&[(1, 0x100)]), 0);
    first.extend_from_slice(&packetize_section(0x100, &pmt_section(1, 0x101, &[(0x1B, 0x101), (0x0F, 0x102)], 0), 0));
    let mut second = packetize_section(0, &pat_section(&[(1, 0x200)]), 0);
    second.extend_from_slice(&packetize_section(0x200, &pmt_section(1, 0x201, &[(0x1B, 0x201), (0x0F, 0x202)], 0), 0));

    let first = found(
        inspect_mpeg_ts(Cursor::new(first), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("first topology"),
    );
    let second = found(
        inspect_mpeg_ts(Cursor::new(second), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("second topology"),
    );

    assert_eq!(first.stream_types, second.stream_types);
    assert_ne!(first, second);
    assert_eq!(
        evaluate_mpeg_ts_splice_boundary(
            &HlsTsSpliceEvidence::compatible_for_test(first),
            &HlsTsSpliceEvidence::compatible_for_test(second),
        ),
        Err(HlsTsSpliceBoundaryIncompatibility::TopologyMismatch)
    );
}

#[tokio::test]
async fn complete_media_evidence_enforces_ffmpeg_continuity_semantics() {
    let pid = 0x101;
    let exact_pes = pes_bytes(0xE0, 4, &[1, 2, 3, 4]);
    let next_pes = pes_bytes(0xE0, 0, &[0x80, 0, 0]);
    let mut valid = track_stream(0);
    valid.extend_from_slice(&media_payload_packet(pid, 3, true, false, &exact_pes));
    valid.extend_from_slice(&media_adaptation_only_packet(pid, 3, false));
    valid.extend_from_slice(&media_payload_packet(pid, 4, true, false, &next_pes));
    assert!(matches!(complete_media_evidence(&valid).await.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));

    let mut discontinuity = track_stream(0);
    discontinuity.extend_from_slice(&media_payload_packet(pid, 3, true, false, &exact_pes));
    discontinuity.extend_from_slice(&media_adaptation_only_packet(pid, 11, true));
    discontinuity.extend_from_slice(&media_payload_packet(pid, 12, true, false, &next_pes));
    assert!(matches!(
        complete_media_evidence(&discontinuity).await.splice_evidence,
        HlsTsSpliceEvidence::Compatible(_)
    ));

    let mut invalid = track_stream(0);
    invalid.extend_from_slice(&media_payload_packet(pid, 3, true, false, &exact_pes));
    invalid.extend_from_slice(&media_adaptation_only_packet(pid, 4, false));
    assert!(matches!(
        complete_media_evidence(&invalid).await.splice_evidence,
        HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::ContinuityFailure {
            pid: 0x101,
            expected: 3,
            actual: 4,
            ..
        })
    ));
}

#[tokio::test]
async fn complete_media_evidence_rejects_tei_and_internal_payload_jump() {
    let pid = 0x101;
    let first = pes_bytes(0xE0, 200, &[0x11; 100]);
    let continuation = [0x22; 100];
    let mut jumped = track_stream(0);
    jumped.extend_from_slice(&media_payload_packet(pid, 5, true, false, &first));
    jumped.extend_from_slice(&media_payload_packet(pid, 9, false, false, &continuation));
    assert!(matches!(
        complete_media_evidence(&jumped).await.splice_evidence,
        HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::ContinuityFailure {
            pid: 0x101,
            expected: 6,
            actual: 9,
            ..
        })
    ));

    let mut tei_packet = media_payload_packet(pid, 5, true, false, &pes_bytes(0xE0, 0, &[0x80]));
    tei_packet[1] |= 0x80;
    let mut tei = track_stream(0);
    tei.extend_from_slice(&tei_packet);
    assert!(matches!(
        complete_media_evidence(&tei).await.splice_evidence,
        HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::TransportError { pid: 0x101, .. })
    ));
}

#[tokio::test]
async fn complete_media_evidence_accounts_finite_split_and_unbounded_pes() {
    let pid = 0x101;
    let mut truncated = track_stream(0);
    truncated.extend_from_slice(&media_payload_packet(pid, 0, true, false, &pes_bytes(0xE0, 20, &[0xAA; 8])));
    assert!(matches!(
        complete_media_evidence(&truncated).await.splice_evidence,
        HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::IncompletePes {
            pid: 0x101,
            declared_bytes: Some(20),
            observed_bytes: 8,
            ..
        })
    ));

    let mut exact = track_stream(0);
    exact.extend_from_slice(&media_payload_packet(pid, 0, true, false, &pes_bytes(0xE0, 8, &[0xAA; 8])));
    assert!(matches!(complete_media_evidence(&exact).await.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));

    let header = pes_bytes(0xE0, 5, &[1, 2, 3, 4, 5]);
    let mut split = track_stream(0);
    split.extend_from_slice(&media_payload_packet(pid, 0, true, false, &header[..4]));
    split.extend_from_slice(&media_payload_packet(pid, 1, false, false, &header[4..]));
    assert!(matches!(complete_media_evidence(&split).await.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));

    let mut unbounded = track_stream(0);
    unbounded.extend_from_slice(&media_payload_packet(pid, 0, true, false, &pes_bytes(0xE0, 0, &[0x80, 0, 0, 1])));
    assert!(matches!(complete_media_evidence(&unbounded).await.splice_evidence, HlsTsSpliceEvidence::Compatible(_)));

    let mut invalid = track_stream(0);
    invalid.extend_from_slice(&media_payload_packet(pid, 0, true, false, &[0x12, 0x34, 0x56]));
    assert!(matches!(
        complete_media_evidence(&invalid).await.splice_evidence,
        HlsTsSpliceEvidence::Incompatible(HlsTsSpliceIncompatibility::InvalidPes { pid: 0x101, .. })
    ));
}

#[test]
fn ts_inspector_reassembles_multi_packet_and_cross_chunk_psi() {
    let budget = HlsTsProbeBudget { read_chunk_bytes: 191, ..HlsTsProbeBudget::default() };
    let track_signature = found(
        inspect_mpeg_ts(Cursor::new(track_stream(400)), HlsTsProbeProtection::Clear, budget).expect("probe succeeds"),
    );
    assert_eq!(track_signature.stream_types.as_ref(), &[0x0F, 0x1B]);

    let program_signature = found(
        inspect_mpeg_ts(Cursor::new(multi_packet_pat_stream()), HlsTsProbeProtection::Clear, budget)
            .expect("probe succeeds"),
    );
    assert_eq!(program_signature.program_count, 46);
    assert_eq!(program_signature.stream_types.as_ref(), &[0x0F, 0x1B]);
}

#[test]
fn ts_inspector_never_mixes_pat_or_pmt_sections_across_versions() {
    let mut pat_version_mix = packetize_section(0, &pat_section_with_header(&[(1, 0x100)], 0, true, 0, 1), 0);
    pat_version_mix.extend_from_slice(&packetize_section(0, &pat_section_with_header(&[(2, 0x200)], 1, true, 1, 1), 1));
    pat_version_mix.extend_from_slice(&packetize_section(0x200, &pmt_section(2, 0x201, &[(0x1B, 0x201)], 0), 0));
    assert_eq!(
        inspect_mpeg_ts(Cursor::new(pat_version_mix), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default(),)
            .expect("probe completes"),
        HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::IncompleteProgramMetadata)
    );

    let mut program_map_version_mix = packetize_section(0, &pat_section(&[(1, 0x100)]), 0);
    program_map_version_mix.extend_from_slice(&packetize_section(
        0x100,
        &pmt_section_with_header(
            1,
            0x101,
            &[(0x1B, 0x101)],
            0,
            PmtSectionHeader { version: 0, current: true, section_number: 0, last_section_number: 1 },
        ),
        0,
    ));
    program_map_version_mix.extend_from_slice(&packetize_section(
        0x100,
        &pmt_section_with_header(
            1,
            0x102,
            &[(0x0F, 0x102)],
            0,
            PmtSectionHeader { version: 1, current: true, section_number: 1, last_section_number: 1 },
        ),
        1,
    ));
    assert_eq!(
            inspect_mpeg_ts(
                Cursor::new(program_map_version_mix),
                HlsTsProbeProtection::Clear,
                HlsTsProbeBudget::default(),
            )
            .expect("probe completes"),
            HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::IncompleteProgramMetadata)
        );
}

#[test]
fn ts_inspector_rejects_same_version_pat_and_pmt_section_contradictions() {
    let mut contradictory_pat = packetize_section(0, &pat_section_with_header(&[(1, 0x100)], 0, true, 0, 1), 0);
    contradictory_pat.extend_from_slice(&packetize_section(
        0,
        &pat_section_with_header(&[(1, 0x101)], 0, true, 0, 1),
        1,
    ));
    assert_eq!(
        inspect_mpeg_ts(Cursor::new(contradictory_pat), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default(),)
            .expect("probe completes"),
        HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPat)
    );

    let mut stream = packetize_section(0, &pat_section(&[(1, 0x100)]), 0);
    for (counter, stream_type) in [(0, 0x1B), (1, 0x0F)] {
        stream.extend_from_slice(&packetize_section(
            0x100,
            &pmt_section_with_header(
                1,
                0x101,
                &[(stream_type, 0x101)],
                0,
                PmtSectionHeader { version: 0, current: true, section_number: 0, last_section_number: 1 },
            ),
            counter,
        ));
    }

    assert_eq!(
        inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("probe completes"),
        HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPmt)
    );
}

#[test]
fn ts_inspector_next_pat_or_pmt_does_not_change_current_evidence() {
    let mut stream = packetize_section(0, &pat_section_with_header(&[(1, 0x100)], 0, true, 0, 1), 0);
    stream.extend_from_slice(&packetize_section(0, &pat_section_with_header(&[(2, 0x200)], 1, false, 1, 1), 1));
    stream.extend_from_slice(&packetize_section(0, &pat_section_with_header(&[], 0, true, 1, 1), 2));
    for (counter, header, stream_type, pcr_pid, elementary_pid) in [
        (
            0,
            PmtSectionHeader { version: 0, current: true, section_number: 0, last_section_number: 1 },
            0x1B,
            0x101,
            0x101,
        ),
        (
            1,
            PmtSectionHeader { version: 1, current: false, section_number: 1, last_section_number: 1 },
            0x24,
            0x201,
            0x201,
        ),
        (
            2,
            PmtSectionHeader { version: 0, current: true, section_number: 1, last_section_number: 1 },
            0x0F,
            0x101,
            0x102,
        ),
    ] {
        stream.extend_from_slice(&packetize_section(
            0x100,
            &pmt_section_with_header(1, pcr_pid, &[(stream_type, elementary_pid)], 0, header),
            counter,
        ));
    }

    let signature = found(
        inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("probe succeeds"),
    );

    assert_eq!(signature.stream_types.as_ref(), &[0x0F, 0x1B]);
}

#[test]
fn ts_inspector_reassembles_syntax_headers_split_across_packets() {
    let mut stream = packetize_section_with_split_syntax_header(0, &pat_section(&[(1, 0x100)]), 2);
    stream.extend_from_slice(&packetize_section_with_split_syntax_header(
        0x100,
        &pmt_section(1, 0x101, &[(0x1B, 0x101), (0x0F, 0x102)], 0),
        1,
    ));
    stream.extend_from_slice(&null_packet());
    let budget = HlsTsProbeBudget { read_chunk_bytes: 191, ..HlsTsProbeBudget::default() };

    let signature =
        found(inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, budget).expect("probe succeeds"));

    assert_eq!(signature.program_count, 1);
    assert_eq!(signature.stream_types.as_ref(), &[0x0F, 0x1B]);

    let mut stream = packetize_section_with_split_syntax_header(0, &pat_section(&[(1, 0x100)]), 5);
    stream.extend_from_slice(&packetize_section_with_split_syntax_header(
        0x100,
        &pmt_section(1, 0x101, &[(0x1B, 0x101), (0x0F, 0x102)], 0),
        7,
    ));
    stream.extend_from_slice(&null_packet());
    let signature =
        found(inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, budget).expect("probe succeeds"));
    assert_eq!(signature.stream_types.as_ref(), &[0x0F, 0x1B]);
}

#[test]
fn ts_inspector_accepts_pusi_pointer_that_completes_pending_syntax_header() {
    let pat = pat_section(&[(1, 0x100)]);
    let pmt = pmt_section(1, 0x101, &[(0x1B, 0x101), (0x0F, 0x102)], 0);
    let mut stream = synthetic_psi_packet(Pid::new(0), true, &pat[..2]).to_vec();
    stream.extend_from_slice(&psi_start_packet_with_pointer(0, &pat[2..], &pat));
    stream.extend_from_slice(&synthetic_psi_packet(Pid::new(0x100), true, &pmt[..5]));
    stream.extend_from_slice(&psi_start_packet_with_pointer(0x100, &pmt[5..], &pmt));
    stream.extend_from_slice(&null_packet());

    let signature = found(
        inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("probe succeeds"),
    );

    assert_eq!(signature.program_count, 1);
    assert_eq!(signature.stream_types.as_ref(), &[0x0F, 0x1B]);
}

#[test]
fn ts_inspector_rejects_pusi_without_section_start_bytes() {
    let mut stream = psi_start_packet_with_pointer(0, &[], &[]).to_vec();
    stream.extend_from_slice(&null_packet());

    let outcome = inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
        .expect("probe completes");

    assert!(matches!(outcome, HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPsiPointer)));
}

#[tokio::test]
async fn ts_inspector_async_clear_stops_in_small_prefix_of_logical_large_segment() {
    const LOGICAL_SEGMENT_BYTES: u64 = 20 * 1024 * 1024;
    let prefix: Arc<[u8]> = valid_track_prefix(HLS_TS_PROBE_READ_CHUNK_BYTES).into();
    let sync_read = Arc::new(AtomicU64::new(0));
    let async_read = Arc::new(AtomicU64::new(0));

    let sync_signature = found(
        inspect_mpeg_ts(
            VirtualTailReader::new(Arc::clone(&prefix), LOGICAL_SEGMENT_BYTES, Arc::clone(&sync_read)),
            HlsTsProbeProtection::Clear,
            HlsTsProbeBudget::default(),
        )
        .expect("sync probe succeeds"),
    );
    let async_signature = found(
        inspect_mpeg_ts_async(
            VirtualTailReader::new(prefix, LOGICAL_SEGMENT_BYTES, Arc::clone(&async_read)),
            HlsTsProbeProtection::Clear,
            HlsTsProbeBudget::default(),
        )
        .await
        .expect("async probe succeeds"),
    );

    assert_eq!(async_signature, sync_signature);
    assert_eq!(async_signature.stream_types.as_ref(), &[0x0F, 0x1B]);
    assert!(sync_read.load(Ordering::Relaxed) <= HLS_TS_PROBE_READ_CHUNK_BYTES as u64);
    assert!(async_read.load(Ordering::Relaxed) <= HLS_TS_PROBE_READ_CHUNK_BYTES as u64);
}

#[tokio::test]
async fn ts_inspector_async_aes_stops_in_small_prefix_of_logical_large_segment() {
    const LOGICAL_SEGMENT_BYTES: u64 = 20 * 1024 * 1024;
    let key = *b"0123456789abcdef";
    let iv = [0x5A; AES_128_BLOCK_BYTES];
    let mut plaintext = valid_track_prefix(HLS_TS_PROBE_READ_CHUNK_BYTES);
    plaintext.resize(plaintext.len().next_multiple_of(AES_128_BLOCK_BYTES), 0xFF);
    let ciphertext: Arc<[u8]> = encrypt_aes128_cbc(&plaintext, &key, iv).into();
    let sync_read = Arc::new(AtomicU64::new(0));
    let async_read = Arc::new(AtomicU64::new(0));

    let sync_signature = found(
        inspect_mpeg_ts(
            VirtualTailReader::new(Arc::clone(&ciphertext), LOGICAL_SEGMENT_BYTES, Arc::clone(&sync_read)),
            HlsTsProbeProtection::Aes128Cbc { key: &key, iv },
            HlsTsProbeBudget::default(),
        )
        .expect("sync AES probe succeeds"),
    );
    let async_signature = found(
        inspect_mpeg_ts_async(
            VirtualTailReader::new(ciphertext, LOGICAL_SEGMENT_BYTES, Arc::clone(&async_read)),
            HlsTsProbeProtection::Aes128Cbc { key: &key, iv },
            HlsTsProbeBudget::default(),
        )
        .await
        .expect("async AES probe succeeds"),
    );

    assert_eq!(async_signature, sync_signature);
    assert_eq!(async_signature.stream_types.as_ref(), &[0x0F, 0x1B]);
    assert!(sync_read.load(Ordering::Relaxed) <= HLS_TS_PROBE_READ_CHUNK_BYTES as u64);
    assert!(async_read.load(Ordering::Relaxed) <= HLS_TS_PROBE_READ_CHUNK_BYTES as u64);
}

#[test]
fn ts_inspector_reports_probe_budget_without_reading_whole_source() {
    let mut bytes = Vec::new();
    for _ in 0..100 {
        bytes.extend_from_slice(&null_packet());
    }
    let count = Rc::new(Cell::new(0));
    let reader = CountingReader { inner: Cursor::new(bytes), bytes_read: Rc::clone(&count) };
    let budget = HlsTsProbeBudget { max_bytes: (TS_PACKET_BYTES * 4) as u64, ..HlsTsProbeBudget::default() };
    let outcome = inspect_mpeg_ts(reader, HlsTsProbeProtection::Clear, budget).expect("probe succeeds");
    assert!(matches!(outcome, HlsTsProbeOutcome::ProbeBudgetExhausted { .. }));
    assert_eq!(count.get(), TS_PACKET_BYTES * 4);
}

#[test]
fn ts_inspector_policy_resolution_preserves_all_probe_reasons() {
    assert_eq!(
        HlsTrackEvidenceResolution::from(Ok(HlsTsProbeOutcome::ProbeBudgetExhausted {
            bytes_examined: 512,
            packets_examined: 2,
        })),
        HlsTrackEvidenceResolution::InsufficientEvidence { bytes_examined: 512, packets_examined: 2 }
    );
    assert_eq!(
        HlsTrackEvidenceResolution::from(Ok(HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPmt,)))
            .reason_code(),
        "invalid-pmt"
    );
    assert_eq!(
        HlsTrackEvidenceResolution::from(Ok(HlsTsProbeOutcome::Malformed(
            HlsTsMalformedReason::IncompleteProgramMetadata,
        ))),
        HlsTrackEvidenceResolution::IncompleteEvidence
    );
    let unsupported = HlsTrackEvidenceResolution::from(Ok(HlsTsProbeOutcome::UnsupportedProtection(
        HlsTsProtectionReason::TransportScrambling,
    )));
    assert_eq!(
        unsupported,
        HlsTrackEvidenceResolution::UnsupportedProtection(HlsTsProtectionReason::TransportScrambling)
    );
    assert_eq!(unsupported.reason_code(), "transport-scrambling");
    let key_unavailable = HlsTrackEvidenceResolution::from(Err(HlsTsProbeError::KeyUnavailable));
    assert_eq!(key_unavailable, HlsTrackEvidenceResolution::KeyUnavailable);
    assert_eq!(key_unavailable.reason_code(), "key-unavailable");
    let invalid_iv = HlsTrackEvidenceResolution::from(Err(HlsTsProbeError::InvalidIv));
    assert_eq!(invalid_iv, HlsTrackEvidenceResolution::InvalidIv);
    assert_eq!(invalid_iv.reason_code(), "invalid-iv");
    let decryption_failed = HlsTrackEvidenceResolution::from(Err(HlsTsProbeError::DecryptionFailed));
    assert_eq!(decryption_failed, HlsTrackEvidenceResolution::DecryptionFailed);
    assert_eq!(decryption_failed.reason_code(), "decryption-failed");
    let io = HlsTrackEvidenceResolution::from(Err(HlsTsProbeError::Io(std::io::Error::from(
        std::io::ErrorKind::PermissionDenied,
    ))));
    assert_eq!(io, HlsTrackEvidenceResolution::Io(std::io::ErrorKind::PermissionDenied));
    assert_eq!(io.reason_code(), "io");
}

#[test]
fn ts_inspector_resynchronizes_only_within_named_budget() {
    let budget = HlsTsProbeBudget { max_resync_bytes: 16, ..HlsTsProbeBudget::default() };
    let mut within_budget = vec![0xAA; 16];
    within_budget.extend_from_slice(&track_stream(0));
    assert!(matches!(
        inspect_mpeg_ts(Cursor::new(within_budget), HlsTsProbeProtection::Clear, budget)
            .expect("bounded resync probe completes"),
        HlsTsProbeOutcome::Found(_)
    ));

    let mut outside_budget = vec![0xAA; 17];
    outside_budget.extend_from_slice(&track_stream(0));
    assert_eq!(
        inspect_mpeg_ts(Cursor::new(outside_budget), HlsTsProbeProtection::Clear, budget)
            .expect("out-of-budget resync probe completes"),
        HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidSynchronization)
    );
}

#[test]
fn ts_inspector_types_framing_crc_and_pmt_errors() {
    let invalid_sync = vec![0_u8; TS_PACKET_BYTES * 3];
    assert_eq!(
        inspect_mpeg_ts(Cursor::new(invalid_sync), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("probe completes"),
        HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidSynchronization)
    );

    let mut invalid_crc = track_stream(0);
    invalid_crc[10] ^= 0x01;
    assert_eq!(
        inspect_mpeg_ts(Cursor::new(invalid_crc), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("probe completes"),
        HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPsiCrc)
    );

    let mut invalid_pmt = pmt_section(1, 0x101, &[(0x1B, 0x101)], 0);
    invalid_pmt[15] = 0xFF;
    invalid_pmt[16] = 0xFF;
    let crc_start = invalid_pmt.len().saturating_sub(4);
    invalid_pmt.truncate(crc_start);
    invalid_pmt = append_crc(invalid_pmt);
    let mut stream = packetize_section(0, &pat_section(&[(1, 0x100)]), 0);
    stream.extend_from_slice(&packetize_section(0x100, &invalid_pmt, 0));
    stream.extend_from_slice(&null_packet());
    assert_eq!(
        inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("probe completes"),
        HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPmt)
    );

    let mut malformed_descriptors = pmt_section(1, 0x101, &[(0x1B, 0x101)], 4);
    malformed_descriptors[13] = 5;
    let crc_start = malformed_descriptors.len().saturating_sub(4);
    malformed_descriptors.truncate(crc_start);
    malformed_descriptors = append_crc(malformed_descriptors);
    let mut stream = packetize_section(0, &pat_section(&[(1, 0x100)]), 0);
    stream.extend_from_slice(&packetize_section(0x100, &malformed_descriptors, 0));
    stream.extend_from_slice(&null_packet());
    assert_eq!(
        inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("probe completes"),
        HlsTsProbeOutcome::Malformed(HlsTsMalformedReason::InvalidPmt)
    );
}

#[test]
fn ts_inspector_types_scrambled_transport_as_unsupported_protection() {
    let mut stream = track_stream(0);
    stream[3] |= 0x80;

    assert_eq!(
        inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("probe completes"),
        HlsTsProbeOutcome::UnsupportedProtection(HlsTsProtectionReason::TransportScrambling)
    );
}

#[test]
fn ts_inspector_applies_explicit_and_sequence_derived_hls_ivs() {
    let sequence_iv = hls_aes128_cbc_iv(None, 0x0102_0304_0506_0708).expect("sequence IV");
    assert_eq!(sequence_iv, [0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]);
    let explicit_iv = hls_aes128_cbc_iv(Some("0x10203"), 0).expect("explicit IV");
    assert_eq!(explicit_iv, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3]);
    assert!(matches!(hls_aes128_cbc_iv(Some("10203"), 0), Err(HlsTsProbeError::InvalidIv)));
}
