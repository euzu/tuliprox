use super::{test_support::*, TsRandomAccessScanner};

#[test]
fn pat_pmt_idr_in_one_aligned_chunk() {
    let mut scanner = TsRandomAccessScanner::new();
    let stream = join(&[pat(0), video_pmt(0), pes(VIDEO_PID, true, false, &[&h264(5)])]);
    assert_eq!(scanner.observe(&stream), Some(0));
}

#[test]
fn leading_garbage_bytes_then_packets() {
    let mut scanner = TsRandomAccessScanner::new();
    let mut input = vec![0x12, 0x34, 0x56, 0x78, 0x9A];
    input.extend_from_slice(&join(&[pat(0), video_pmt(0), pes(VIDEO_PID, true, false, &[&h264(5)])]));
    assert_eq!(scanner.observe(&input), Some(5));
}

#[test]
fn split_chunks_of_odd_sizes() {
    let stream = join(&[pat(0), video_pmt(0), pes(VIDEO_PID, true, false, &[&h264(5)])]);
    assert_eq!(stream.len(), 564);

    let mut unsplit_scanner = TsRandomAccessScanner::new();
    let unsplit_pos = unsplit_scanner.observe(&stream);
    assert_eq!(unsplit_pos, Some(0));

    let mut scanner = TsRandomAccessScanner::new();
    assert_eq!(scanner.observe(&stream[..1]), None);
    assert_eq!(scanner.observe(&stream[1..188]), None);
    assert_eq!(scanner.observe(&stream[188..377]), None);
    assert_eq!(scanner.observe(&stream[377..477]), None);
    assert_eq!(scanner.observe(&stream[477..]), Some(0));
}

#[test]
fn split_across_chunks() {
    let mut scanner = TsRandomAccessScanner::new();
    let chunk1 = join(&[pat(0), video_pmt(0)]);
    let chunk2 = join(&[pes(VIDEO_PID, true, false, &[&h264(5)])]);

    assert_eq!(scanner.observe(&chunk1), None);
    assert_eq!(scanner.observe(&chunk2), Some(0));
}

#[test]
fn keyframe_in_second_packet_of_pes() {
    let mut scanner = TsRandomAccessScanner::new();
    let sei = [vec![0x06], vec![0x05; 150]].concat();
    let pes_start = pes(VIDEO_PID, false, false, &[&h264(9), &sei]);
    let idr_continuation = ts(VIDEO_PID, false, None, &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84]);
    let stream = join(&[pat(0), video_pmt(0), pes_start.clone(), idr_continuation.clone()]);

    assert_eq!(scanner.observe(&stream), Some(0));

    let mut split_scanner = TsRandomAccessScanner::new();
    let chunk1 = join(&[pat(0), video_pmt(0), pes_start]);
    let chunk2 = join(&[idr_continuation]);

    assert_eq!(split_scanner.observe(&chunk1), None);
    assert_eq!(split_scanner.observe(&chunk2), Some(0));
}

#[test]
fn non_keyframe_pes_start() {
    let mut scanner = TsRandomAccessScanner::new();
    let stream = join(&[pat(0), video_pmt(0), pes(VIDEO_PID, false, false, &[&h264(1)])]);
    assert_eq!(scanner.observe(&stream), None);
}

#[test]
fn two_gops_in_one_chunk() {
    let mut scanner = TsRandomAccessScanner::new();
    let gop1 = [pat(0), video_pmt(0), pes(VIDEO_PID, true, false, &[&h264(5)])];
    let gop2 = [pat(0), video_pmt(0), pes(VIDEO_PID, true, false, &[&h264(5)])];
    let stream = join(&[gop1.as_slice(), gop2.as_slice()].concat());

    assert_eq!(scanner.observe(&stream), Some(564));
}

#[test]
fn radio_pmt_without_video() {
    let mut scanner = TsRandomAccessScanner::new();
    let audio_pmt = pmt(0, &[(AAC, AUDIO_PID)]);
    let stream = join(&[pat(0), audio_pmt]);
    assert_eq!(scanner.observe(&stream), Some(0));
}

#[test]
fn non_ts_bytes_and_resync() {
    let mut scanner = TsRandomAccessScanner::new();
    let garbage = vec![0x00; 1000];
    assert_eq!(scanner.observe(&garbage), None);

    let valid = join(&[pat(0), video_pmt(0), pes(VIDEO_PID, true, false, &[&h264(5)])]);
    assert_eq!(scanner.observe(&valid), Some(1000));
}

#[test]
fn empty_chunk() {
    let mut scanner = TsRandomAccessScanner::new();
    assert_eq!(scanner.observe(&[]), None);
}

#[test]
fn malformed_pes_start_followed_by_idr_continuation_yields_no_rap() {
    let mut scanner = TsRandomAccessScanner::new();
    let malformed_pes_start = ts(VIDEO_PID, true, None, &[0xFF; 20]);
    let idr_continuation = ts(VIDEO_PID, false, None, &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84]);
    let stream = join(&[pat(0), video_pmt(0), malformed_pes_start, idr_continuation]);
    assert_eq!(scanner.observe(&stream), None);
}

#[test]
fn h264_idr_without_rai_flag_in_start_packet() {
    let mut scanner = TsRandomAccessScanner::new();
    let stream = join(&[pat(0), video_pmt(0), pes(VIDEO_PID, false, false, &[&h264(5)])]);
    assert_eq!(scanner.observe(&stream), Some(0));
}

#[test]
fn hevc_idr_without_rai_flag_in_start_packet() {
    let mut scanner = TsRandomAccessScanner::new();
    let pmt = pmt(0, &[(HEVC, VIDEO_PID), (AAC, AUDIO_PID)]);
    let stream = join(&[pat(0), pmt, pes(VIDEO_PID, false, false, &[&hevc(19)])]);
    assert_eq!(scanner.observe(&stream), Some(0));
}

#[test]
fn mpeg2_sequence_header_without_rai_flag_in_start_packet() {
    let mut scanner = TsRandomAccessScanner::new();
    let pmt = pmt(0, &[(MPEG2, VIDEO_PID), (AAC, AUDIO_PID)]);
    let stream = join(&[pat(0), pmt, pes(VIDEO_PID, false, false, &[&mpeg2(0xB3)])]);
    assert_eq!(scanner.observe(&stream), Some(0));
}
