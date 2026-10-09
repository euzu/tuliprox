use super::*;

#[test]
fn ts_inspector_restarts_incomplete_pat_on_new_current_version() {
    let mut stream = packetize_section(0, &pat_section_with_header(&[(1, 0x100)], 0, true, 0, 1), 0);
    stream.extend_from_slice(&packetize_section(
        0x100,
        &pmt_section_with_header(
            1,
            0x101,
            &[(0x02, 0x101)],
            0,
            PmtSectionHeader { version: 0, current: true, section_number: 0, last_section_number: 1 },
        ),
        0,
    ));
    stream.extend_from_slice(&packetize_section(0, &pat_section_with_header(&[(2, 0x200)], 1, true, 0, 0), 1));
    stream.extend_from_slice(&packetize_section(0x200, &pmt_section(2, 0x201, &[(0x1B, 0x201), (0x0F, 0x202)], 0), 0));
    stream.extend_from_slice(&null_packet());

    let signature = found(
        inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("probe succeeds"),
    );

    assert_eq!(signature.program_count, 1);
    assert_eq!(signature.stream_types.as_ref(), &[0x0F, 0x1B]);
}

#[test]
fn ts_inspector_restarts_incomplete_pmt_on_new_current_version() {
    let mut stream = packetize_section(0, &pat_section(&[(1, 0x100)]), 0);
    stream.extend_from_slice(&packetize_section(
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
    stream.extend_from_slice(&packetize_section(
        0x100,
        &pmt_section_with_header(
            1,
            0x201,
            &[(0x24, 0x201), (0x0F, 0x202)],
            0,
            PmtSectionHeader { version: 1, current: true, section_number: 0, last_section_number: 0 },
        ),
        1,
    ));
    stream.extend_from_slice(&null_packet());

    let signature = found(
        inspect_mpeg_ts(Cursor::new(stream), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
            .expect("probe succeeds"),
    );

    assert_eq!(signature.stream_types.as_ref(), &[0x0F, 0x24]);
}
