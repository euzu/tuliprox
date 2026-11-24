use super::{test_support::*, SpliceOutput, TsSpliceAligner};
use crate::packet::{is_idr_start, mpeg2_crc32, ts_null_packet_bytes, TS_NULL_PACKET, TS_PACKET_SIZE};
use bytes::Bytes;
use std::time::{Duration, Instant};

fn emitted(output: SpliceOutput) -> Bytes {
    match output {
        SpliceOutput::Emit(bytes) => bytes,
        SpliceOutput::EmitTwo(first, second) => Bytes::from([first, second].concat()),
        SpliceOutput::Hold => panic!("expected emitted packets"),
    }
}

/// Version of the single-packet section in `packet`, after checking its CRC.
fn section_version(packet: &[u8]) -> u8 {
    let payload_start = if packet[3] & 0x20 != 0 { 5 + usize::from(packet[4]) } else { 4 };
    let section = &packet[payload_start + 1 + usize::from(packet[payload_start])..];
    let length = 3 + ((usize::from(section[1] & 0x0F) << 8) | usize::from(section[2]));
    assert_eq!(mpeg2_crc32(&section[..length]), 0, "section CRC verifies");
    (section[5] >> 1) & 0x1F
}

fn discontinuity(packet: &[u8]) -> bool { packet[3] & 0x20 != 0 && packet[4] >= 1 && packet[5] & 0x80 != 0 }

fn aligner() -> TsSpliceAligner { TsSpliceAligner::new(1024 * 1024, Duration::from_secs(5)) }

/// An aligner that already streamed a first member with PAT/PMT `version` and is switching.
fn switched_aligner(version: u8, now: Instant) -> TsSpliceAligner {
    let mut aligner = aligner();
    emitted(aligner.push(join(&[pat(version), video_pmt(version), pes(VIDEO_PID, true, false, &[&h264(5)])]), now));
    aligner.begin_member(now);
    aligner
}

#[test]
fn null_packet_is_valid_ts() {
    assert_eq!(TS_NULL_PACKET.len(), TS_PACKET_SIZE);
    assert_eq!(TS_NULL_PACKET[0], 0x47);
    assert_eq!(pid_of(&TS_NULL_PACKET), 0x1FFF);
    assert_eq!(ts_null_packet_bytes().as_ptr(), TS_NULL_PACKET.as_ptr());
}

#[test]
fn crc32_matches_known_pat() {
    let section = [0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00];
    assert_eq!(mpeg2_crc32(&section), 0x2AB1_04B2);
}

#[test]
fn passthrough_emits_aligned_chunk_zero_copy() {
    let mut aligner = aligner();
    let chunk = join(&[pat(0), video_pmt(0), video_cont(false)]);
    let out = emitted(aligner.push(chunk.clone(), Instant::now()));
    assert_eq!(out.as_ptr(), chunk.as_ptr());
    assert_eq!(out, chunk);
}

#[test]
fn realigns_unaligned_input_to_188() {
    let mut aligner = aligner();
    let stream = join(&[pat(0), video_pmt(0), video_cont(false)]);
    let now = Instant::now();
    let first = emitted(aligner.push(stream.slice(..200), now));
    assert_eq!(&first[..], &stream[..188]);
    assert!(matches!(aligner.push(stream.slice(200..300), now), SpliceOutput::Hold));
    let rest = emitted(aligner.push(stream.slice(300..), now));
    assert_eq!(&rest[..], &stream[188..]);
}

#[test]
fn drops_garbage_before_sync() {
    let mut aligner = aligner();
    let packets_in = [pat(0), video_pmt(0), video_cont(false), video_cont(false)].concat();
    let mut input = vec![0x00; 17];
    input.extend_from_slice(&packets_in);
    let out = emitted(aligner.push(Bytes::from(input), Instant::now()));
    assert_eq!(&out[..], &packets_in[..]);
}

#[test]
fn waits_for_keyframe_with_rai() {
    let now = Instant::now();
    let mut aligner = switched_aligner(0, now);
    assert!(matches!(aligner.push(join(&[pat(0), video_pmt(0), video_cont(false)]), now), SpliceOutput::Hold));
    let out = emitted(aligner.push(join(&[pes(VIDEO_PID, true, false, &[&h264(1)])]), now));
    let out = packets(&out);
    assert_eq!(out.len(), 4);
    assert_eq!((pid_of(out[0]), pid_of(out[1])), (0, PMT_PID));
}

#[test]
fn detects_h264_idr_without_rai() {
    assert!(is_idr_start(H264, &pes(VIDEO_PID, false, false, &[&h264(9), &h264(7), &h264(8), &h264(5)])[4..]));
    let now = Instant::now();
    let mut aligner = switched_aligner(0, now);
    let out = aligner.push(join(&[pat(0), video_pmt(0), pes(VIDEO_PID, false, false, &[&h264(7), &h264(5)])]), now);
    assert_eq!(packets(&emitted(out)).len(), 3);
}

#[test]
fn detects_hevc_idr_without_rai() {
    for nal_type in [19, 20, 21] {
        assert!(is_idr_start(HEVC, &pes(VIDEO_PID, false, false, &[&hevc(32), &hevc(nal_type)])[4..]));
    }
    let now = Instant::now();
    let mut aligner = switched_aligner(0, now);
    let pmt = pmt(0, &[(HEVC, VIDEO_PID)]);
    let out = aligner.push(join(&[pat(0), pmt, pes(VIDEO_PID, false, false, &[&hevc(19)])]), now);
    assert_eq!(packets(&emitted(out)).len(), 3);
}

#[test]
fn ignores_non_idr_slices() {
    assert!(!is_idr_start(H264, &pes(VIDEO_PID, false, false, &[&h264(9), &h264(1)])[4..]));
    assert!(!is_idr_start(HEVC, &pes(VIDEO_PID, false, false, &[&hevc(1)])[4..]));
    let now = Instant::now();
    let mut aligner = switched_aligner(0, now);
    let out = aligner.push(join(&[pat(0), video_pmt(0), pes(VIDEO_PID, false, false, &[&h264(1)])]), now);
    assert!(matches!(out, SpliceOutput::Hold));
}

#[test]
fn emits_from_pat_before_keyframe() {
    let now = Instant::now();
    let mut aligner = switched_aligner(0, now);
    let input = [
        video_cont(false),
        pat(0),
        video_pmt(0),
        video_cont(false),
        pat(0),
        video_pmt(0),
        pes(VIDEO_PID, true, false, &[&h264(5)]),
        video_cont(false),
    ];
    let out = emitted(aligner.push(join(&input), now));
    let out = packets(&out);
    assert_eq!(out.len(), 4);
    assert_eq!(pid_of(out[0]), 0);
    assert_eq!(pid_of(out[1]), PMT_PID);
    assert_eq!(pid_of(out[2]), VIDEO_PID);
}

#[test]
fn audio_only_member_starts_at_pmt() {
    let now = Instant::now();
    let mut aligner = switched_aligner(0, now);
    let audio_pmt = pmt(0, &[(AAC, AUDIO_PID)]);
    let out = aligner.push(join(&[ts(AUDIO_PID, false, None, &[1; 10]), pat(0), audio_pmt]), now);
    let out = emitted(out);
    let out = packets(&out);
    assert_eq!(out.len(), 2);
    assert_eq!(pid_of(out[0]), 0);
}

#[test]
fn falls_back_after_byte_budget_from_first_pat() {
    let now = Instant::now();
    let mut aligner = TsSpliceAligner::new(5 * TS_PACKET_SIZE, Duration::from_secs(5));
    emitted(aligner.push(join(&[pat(0), video_pmt(0)]), now));
    aligner.begin_member(now);
    let input = [video_cont(false), pat(0), video_pmt(0), video_cont(false), video_cont(false), video_cont(false)];
    let out = emitted(aligner.push(join(&input), now));
    let out = packets(&out);
    assert_eq!(out.len(), 5);
    assert_eq!(pid_of(out[0]), 0);
}

#[test]
fn falls_back_after_time_budget() {
    let now = Instant::now();
    let mut aligner = switched_aligner(0, now);
    assert!(matches!(aligner.push(join(&[video_cont(false), pat(0)]), now), SpliceOutput::Hold));
    let later = now + Duration::from_secs(6);
    let out = emitted(aligner.push(join(&[video_cont(false)]), later));
    let out = packets(&out);
    assert_eq!(out.len(), 2);
    assert_eq!(pid_of(out[0]), 0);
}

#[test]
fn sets_discontinuity_only_with_existing_adaptation_field() {
    let now = Instant::now();
    let mut aligner = switched_aligner(0, now);
    let input = [
        pat(0),
        video_pmt(0),
        pes(VIDEO_PID, true, false, &[&h264(5)]),
        ts(AUDIO_PID, true, None, &[1; 20]),
        ts(AUDIO_PID, false, Some(0x00), &[1; 20]),
        video_cont(true),
    ];
    let out = emitted(aligner.push(join(&input), now));
    let out = packets(&out);
    assert!(discontinuity(out[2]), "first video packet has an adaptation field");
    assert!(!discontinuity(out[3]), "first audio packet has none");
    assert!(!discontinuity(out[4]), "audio was already seen");
    assert!(!discontinuity(out[5]), "video was already seen");
}

#[test]
fn second_member_pat_and_pmt_get_new_version_and_valid_crc() {
    let now = Instant::now();
    let mut aligner = switched_aligner(0, now);
    let out = emitted(aligner.push(join(&[pat(0), video_pmt(0), pes(VIDEO_PID, true, false, &[&h264(5)])]), now));
    let out = packets(&out);
    assert_eq!(section_version(out[0]), 1);
    assert_eq!(section_version(out[1]), 1);
    let later = emitted(aligner.push(join(&[pat(0), video_pmt(0)]), now));
    let later = packets(&later);
    assert_eq!((section_version(later[0]), section_version(later[1])), (1, 1));
}

#[test]
fn member_version_change_is_preserved_with_offset() {
    let now = Instant::now();
    let mut aligner = switched_aligner(4, now);
    emitted(aligner.push(join(&[pat(31), video_pmt(31), pes(VIDEO_PID, true, false, &[&h264(5)])]), now));
    let out = emitted(aligner.push(join(&[pat(0), video_pmt(0)]), now));
    let out = packets(&out);
    assert_eq!((section_version(out[0]), section_version(out[1])), (6, 6));
}

#[test]
fn first_member_is_not_rewritten() {
    let mut aligner = aligner();
    let input = join(&[pat(7), video_pmt(9), pes(VIDEO_PID, true, false, &[&h264(5)])]);
    let out = emitted(aligner.push(input.clone(), Instant::now()));
    assert_eq!(out, input);
}

#[test]
fn multi_packet_section_passes_unchanged() {
    let now = Instant::now();
    let mut aligner = switched_aligner(0, now);
    let mut long_pmt = video_pmt(0);
    long_pmt[7] = 0xBF; // section_length claims more bytes than the packet holds
    emitted(aligner.push(join(&[pat(0), video_pmt(0), pes(VIDEO_PID, true, false, &[&h264(5)])]), now));
    let out = emitted(aligner.push(Bytes::from(long_pmt.clone()), now));
    assert_eq!(&out[..], &long_pmt[..]);
}

#[test]
fn passthrough_splits_unaligned_chunk_without_copying_body() {
    let mut aligner = aligner();
    let stream = join(&[pat(0), video_pmt(0), video_cont(false), video_cont(false), video_cont(false)]);
    let now = Instant::now();
    let first = emitted(aligner.push(stream.slice(..300), now));
    assert_eq!(&first[..], &stream[..188]);
    let second = stream.slice(300..);
    let SpliceOutput::EmitTwo(head, body) = aligner.push(second.clone(), now) else {
        panic!("expected the completed packet and the aligned rest")
    };
    assert_eq!(&head[..], &stream[188..376]);
    assert_eq!(&body[..], &stream[376..]);
    assert_eq!(body.as_ptr(), second[76..].as_ptr(), "the aligned rest is not copied");
}

#[test]
fn h264_sps_counts_as_keyframe_start() {
    assert!(is_idr_start(H264, &pes(VIDEO_PID, false, false, &[&h264(9), &h264(7)])[4..]));
    assert!(is_idr_start(HEVC, &pes(VIDEO_PID, false, false, &[&hevc(32)])[4..]));
}

#[test]
fn detects_idr_in_second_packet_of_pes() {
    let now = Instant::now();
    let mut aligner = switched_aligner(0, now);
    // AUD and a large SEI fill the PES start packet; the IDR slice follows in the next packet.
    let sei = [vec![0x06], vec![0x05; 150]].concat();
    let pes_start = pes(VIDEO_PID, false, false, &[&h264(9), &sei]);
    let idr_continuation = ts(VIDEO_PID, false, None, &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84]);
    let input = [pat(0), video_pmt(0), pes_start, idr_continuation, video_cont(false)];
    let out = emitted(aligner.push(join(&input), now));
    let out = packets(&out);
    assert_eq!(out.len(), 5, "the splice starts at the PAT before the PES start");
    assert_eq!(pid_of(out[0]), 0);
}

#[test]
fn psi_rewrite_of_unique_chunk_does_not_copy() {
    let now = Instant::now();
    let mut aligner = switched_aligner(0, now);
    emitted(aligner.push(join(&[pat(0), video_pmt(0), pes(VIDEO_PID, true, false, &[&h264(5)])]), now));
    let chunk = join(&[pat(0), video_pmt(0), video_cont(false)]);
    let input_ptr = chunk.as_ptr();
    let out = emitted(aligner.push(chunk, now));
    assert_eq!(out.as_ptr(), input_ptr, "the versions are rewritten in place");
    let out = packets(&out);
    assert_eq!((section_version(out[0]), section_version(out[1])), (1, 1));
}
