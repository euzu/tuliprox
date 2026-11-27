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

fn aligner() -> TsSpliceAligner {
    TsSpliceAligner::new(1024 * 1024, Duration::from_secs(5), Duration::from_secs(5), Duration::ZERO)
}

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
    let mut aligner =
        TsSpliceAligner::new(5 * TS_PACKET_SIZE, Duration::from_secs(5), Duration::from_secs(5), Duration::ZERO);
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
fn first_rap_timeout_without_pat_emits_from_first_packet_on_poll_hold() {
    let now = Instant::now();
    let budget = Duration::from_secs(5);
    let packets_in = join(&[video_cont(false), video_cont(false)]);

    let mut aligner = switched_aligner(0, now);
    assert!(matches!(aligner.push(packets_in.clone(), now), SpliceOutput::Hold));
    assert_eq!(aligner.poll_hold(now + Duration::from_millis(4999)), None, "holds within the budget");
    assert!(aligner.is_aligning());
    assert_eq!(aligner.poll_hold(now + budget), Some(packets_in.clone()), "starts at the first packet");
    assert!(!aligner.is_aligning());

    // A member that ends before it sent a PAT emits nothing.
    let mut ended = switched_aligner(0, now);
    assert!(matches!(ended.push(packets_in, now), SpliceOutput::Hold));
    assert_eq!(ended.finish_at(now + budget), None);
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

const FRAME_TICKS: u64 = 1_080_000; // 40 ms at 27 MHz
const FRAMES_PER_GOP: u64 = 25; // 1 second per GOP
const PCR_HZ: u64 = 27_000_000;

fn make_pcr_gop(gop_idx: u64, pat_version: u8, pcr_start_ticks: u64) -> Vec<Vec<u8>> {
    let mut gop = Vec::with_capacity(27);
    gop.push(pat(pat_version));
    gop.push(video_pmt(0));
    let gop_base_pcr = pcr_start_ticks.wrapping_add(gop_idx * FRAMES_PER_GOP * FRAME_TICKS);
    gop.push(pes_with_pcr(VIDEO_PID, true, gop_base_pcr, &[&h264(5)]));
    for frame in 1..FRAMES_PER_GOP {
        let frame_pcr = gop_base_pcr.wrapping_add(frame * FRAME_TICKS);
        gop.push(pes_with_pcr(VIDEO_PID, false, frame_pcr, &[&h264(1)]));
    }
    gop
}

#[test]
fn switched_member_with_target_trims_burst_and_continues_psi_versions() {
    let target = Duration::from_secs(3);
    let mut aligner = TsSpliceAligner::new(10 * 1024 * 1024, Duration::from_secs(10), Duration::from_secs(10), target);

    let base = Instant::now();
    let m1_gop0 = make_pcr_gop(0, 0, 0);
    assert!(matches!(aligner.push(join(&m1_gop0), base), SpliceOutput::Hold));
    let m1_gop1 = make_pcr_gop(1, 0, 0);
    let out1 = aligner.push(join(&m1_gop1), base + Duration::from_millis(1050));
    assert!(!aligner.is_aligning());
    let _ = emitted(out1);

    let switch_time = base + Duration::from_millis(1050) + Duration::from_secs(5);
    aligner.begin_member(switch_time);
    assert!(aligner.is_aligning());

    let gops: Vec<Vec<Vec<u8>>> = (0..20).map(|i| make_pcr_gop(i, 0, 0)).collect();
    for (i, gop) in gops.iter().enumerate() {
        let now = switch_time + Duration::from_millis(50 * i as u64);
        let output = aligner.push(join(gop), now);
        assert!(matches!(output, SpliceOutput::Hold), "member 2 held during burst at GOP {i}");
    }

    let gop20 = make_pcr_gop(20, 0, 0);
    let now = switch_time + Duration::from_millis(2050);
    let output = aligner.push(join(&gop20), now);
    assert!(!aligner.is_aligning(), "settled and emitted");

    let out = emitted(output);
    let out_packets = packets(&out);

    // Newest PCR is in GOP 20 (about 20.96 s); the start is the newest RAP at least 3 s behind: GOP 18.
    assert_eq!(out_packets.len(), 3 * 27, "member 2 starts at GOP 18 and emits GOPs 18 to 20");
    assert_eq!(pid_of(out_packets[0]), 0, "first emitted packet is PAT");
    assert_eq!(pid_of(out_packets[1]), PMT_PID, "second packet is PMT");
    assert_eq!(pid_of(out_packets[2]), VIDEO_PID, "third packet is keyframe");

    assert_eq!(section_version(out_packets[0]), 1, "PAT version continues after member 1's version 0");
    assert_eq!(section_version(out_packets[1]), 1, "PMT version continues after member 1's version 0");

    assert!(discontinuity(out_packets[2]), "first video packet has discontinuity flag set");
}

#[test]
fn first_member_with_target_trims_burst_without_rewriting_psi() {
    let target = Duration::from_secs(3);
    let mut aligner = TsSpliceAligner::new(10 * 1024 * 1024, Duration::from_secs(10), Duration::from_secs(10), target);
    assert!(aligner.is_aligning(), "first member starts aligning when target > 0");

    let base = Instant::now();
    let gops: Vec<Vec<Vec<u8>>> = (0..20).map(|i| make_pcr_gop(i, 7, 0)).collect();
    for (i, gop) in gops.iter().enumerate() {
        let now = base + Duration::from_millis(50 * i as u64);
        let output = aligner.push(join(gop), now);
        assert!(matches!(output, SpliceOutput::Hold), "first member held during burst at GOP {i}");
    }

    let gop20 = make_pcr_gop(20, 7, 0);
    let now = base + Duration::from_millis(2050);
    let output = aligner.push(join(&gop20), now);
    assert!(!aligner.is_aligning(), "settled and emitted");

    let out = emitted(output);
    let out_packets = packets(&out);

    assert_eq!(out_packets.len(), 3 * 27, "first member starts at GOP 18 and emits GOPs 18 to 20");
    assert_eq!(pid_of(out_packets[0]), 0);
    assert_eq!(pid_of(out_packets[1]), PMT_PID);
    assert_eq!(pid_of(out_packets[2]), VIDEO_PID);

    assert_eq!(section_version(out_packets[0]), 7, "first member PAT version is not rewritten");
    assert!(!discontinuity(out_packets[2]), "first member has no discontinuity flag");
}

#[test]
fn target_zero_starts_first_member_immediately_in_passthrough() {
    let mut aligner = TsSpliceAligner::new(1024 * 1024, Duration::from_secs(5), Duration::from_secs(5), Duration::ZERO);
    assert!(!aligner.is_aligning());
    let chunk = join(&[pat(0), video_pmt(0), video_cont(false)]);
    let out = emitted(aligner.push(chunk.clone(), Instant::now()));
    assert_eq!(out, chunk);
}

#[test]
fn buffer_estimate_tracks_burst_realtime_and_drain() {
    let mut aligner =
        TsSpliceAligner::new(10 * 1024 * 1024, Duration::from_secs(10), Duration::from_secs(10), Duration::ZERO);
    let t0 = Instant::now();

    let frame_dur = Duration::from_nanos(FRAME_TICKS * 1_000 / 27);
    assert_eq!(frame_dur, Duration::from_millis(40));

    // Emit 10 s of media (10 GOPs) within 1 s wall time.
    for i in 0..10 {
        let t = t0 + Duration::from_millis(100 * i);
        let gop = make_pcr_gop(i, 0, 0);
        let out = aligner.push(join(&gop), t);
        let _ = emitted(out);
    }

    let t_burst_end = t0 + Duration::from_secs(1);
    // In GOP 0, packet 2 is the initial PCR baseline (0 delta). Across 10 GOPs there are
    // 10 * FRAMES_PER_GOP - 1 frame advances of FRAME_TICKS.
    let expected_media = Duration::from_nanos((10 * FRAMES_PER_GOP - 1) * FRAME_TICKS * 1_000 / 27);
    let expected_buffer = expected_media.saturating_sub(Duration::from_secs(1));

    let est = aligner.buffer_at(t_burst_end);
    assert_eq!(est, expected_buffer);
    assert!(expected_media.abs_diff(Duration::from_secs(10)) <= frame_dur);

    // Real time: emit 1 GOP (1 s media) over 1 s wall time.
    let t_real = t_burst_end + Duration::from_secs(1);
    let gop10 = make_pcr_gop(10, 0, 0);
    let out = aligner.push(join(&gop10), t_real);
    let _ = emitted(out);

    // 1 s media advance minus 1 s wall delta leaves the estimated buffer unchanged.
    let est_real = aligner.buffer_at(t_real);
    assert_eq!(est_real, expected_buffer);

    // Wall time passes with nothing emitted: drains exactly with wall elapsed.
    let t_pause = t_real + Duration::from_secs(3);
    let est_pause = aligner.buffer_at(t_pause);
    assert_eq!(est_pause, expected_buffer.saturating_sub(Duration::from_secs(3)));

    // Drains to zero when nothing is emitted for longer than the buffer.
    let t_drained = t_real + Duration::from_secs(20);
    let est_drained = aligner.buffer_at(t_drained);
    assert_eq!(est_drained, Duration::ZERO);

    // Never negative.
    let t_much_later = t_real + Duration::from_secs(100);
    let est_later = aligner.buffer_at(t_much_later);
    assert_eq!(est_later, Duration::ZERO);
}

#[test]
fn switch_with_buffer_accounts_for_player_lead() {
    let target = Duration::from_secs(3);
    let mut aligner = TsSpliceAligner::new(10 * 1024 * 1024, Duration::from_secs(10), Duration::from_secs(10), target);

    let base = Instant::now();
    // First member bursts 20 GOPs and settles at GOP 20 (about 20.96 s PCR).
    for i in 0..20 {
        let now = base + Duration::from_millis(50 * i);
        let output = aligner.push(join(&make_pcr_gop(i, 0, 0)), now);
        assert!(matches!(output, SpliceOutput::Hold));
    }

    let emit_time = base + Duration::from_millis(2050);
    let out = aligner.push(join(&make_pcr_gop(20, 0, 0)), emit_time);
    assert!(!aligner.is_aligning());
    let _ = emitted(out);

    // First member was trimmed to target 3 s: emitted GOPs 18..20 (74 frames of PCR advances).
    let initial_buffer = Duration::from_nanos((3 * FRAMES_PER_GOP - 1) * FRAME_TICKS * 1_000 / 27);
    assert_eq!(aligner.buffer_at(emit_time), initial_buffer);

    // Stream for 10 s in real time (GOPs 21..30 at 1 s intervals).
    for k in 1..=10 {
        let now = emit_time + Duration::from_secs(k);
        let out = aligner.push(join(&make_pcr_gop(20 + k, 0, 0)), now);
        let _ = emitted(out);
    }

    let last_stream_time = emit_time + Duration::from_secs(10);
    assert_eq!(aligner.buffer_at(last_stream_time), initial_buffer);

    // Switch with full 3 s buffer (no gap before switch).
    let switch_time = last_stream_time;
    assert_eq!(aligner.buffer_at(switch_time), initial_buffer);

    aligner.begin_member(switch_time);
    assert!(aligner.is_aligning());

    // New member bursts 20 GOPs.
    for i in 0..20 {
        let now = switch_time + Duration::from_millis(50 * i);
        let output = aligner.push(join(&make_pcr_gop(i, 0, 0)), now);
        assert!(matches!(output, SpliceOutput::Hold));
    }

    // 1960 ms hold: player drains 1.96 s of its 2.96 s buffer, leaving 1.0 s.
    let settle_time = switch_time + Duration::from_millis(1960);
    let output = aligner.push(join(&make_pcr_gop(20, 0, 0)), settle_time);
    assert!(!aligner.is_aligning());

    let out = emitted(output);
    let out_packets = packets(&out);

    // The effective target evaluated at emit is 3 s - 1 s = 2 s.
    // In GOP 20 (newest PCR ~20.96 s), newest RAP <= 18.96 s is GOP 19 (at 18.96 s).
    // It emits GOPs 19 and 20 (2 GOPs = 54 packets), NOT GOP 18 (3 GOPs = 81 packets).
    assert_eq!(out_packets.len(), 2 * 27, "member 2 starts at GOP 19, not GOP 18");
    assert_eq!(pid_of(out_packets[0]), 0);
    assert_eq!(pid_of(out_packets[1]), PMT_PID);
    assert_eq!(pid_of(out_packets[2]), VIDEO_PID);
}

#[test]
fn three_switches_in_a_row_keep_buffer_bounded() {
    let target = Duration::from_secs(3);
    let gop_duration = Duration::from_nanos(FRAMES_PER_GOP * FRAME_TICKS * 1_000 / 27);
    let max_allowed_buffer = target + gop_duration;
    let mut aligner = TsSpliceAligner::new(10 * 1024 * 1024, Duration::from_secs(10), Duration::from_secs(10), target);

    let base = Instant::now();
    // Member 1 bursts and settles.
    for i in 0..20 {
        let _ = aligner.push(join(&make_pcr_gop(i, 0, 0)), base + Duration::from_millis(50 * i));
    }
    let m1_emit_time = base + Duration::from_millis(2050);
    let initial_out = emitted(aligner.push(join(&make_pcr_gop(20, 0, 0)), m1_emit_time));
    let initial_packets = packets(&initial_out);
    assert_eq!(initial_packets.len(), 3 * 27, "first member emits GOPs 18..20");

    let mut current_time = m1_emit_time;
    let initial_buffer = aligner.buffer_at(current_time);
    assert!(initial_buffer <= max_allowed_buffer);

    for switch_idx in 1..=3 {
        // Stream in real time for 5 s.
        for k in 1..=5 {
            current_time += Duration::from_secs(1);
            let out = aligner.push(join(&make_pcr_gop(20 + k, 0, 0)), current_time);
            let _ = emitted(out);
        }

        // 1 s gap before switch.
        current_time += Duration::from_secs(1);
        aligner.begin_member(current_time);

        // Next member bursts 20 GOPs.
        for i in 0..20 {
            let _ = aligner.push(join(&make_pcr_gop(i, 0, 0)), current_time + Duration::from_millis(50 * i));
        }
        current_time += Duration::from_millis(2050);
        let out = emitted(aligner.push(join(&make_pcr_gop(20, 0, 0)), current_time));
        assert!(!aligner.is_aligning());

        let out_packets = packets(&out);
        // Over the 1s gap + 2.05s hold, the buffer completely drains, so each switch emits the full 3s target.
        let (expected_packets, expected_buf) = (3 * 27, Duration::from_millis(2960));
        assert_eq!(out_packets.len(), expected_packets, "switch {switch_idx} emits exact expected GOP count");

        let buf = aligner.buffer_at(current_time);
        assert!(buf <= max_allowed_buffer, "switch {switch_idx} buffer {buf:?} exceeds limit {max_allowed_buffer:?}");
        assert_eq!(
            buf, expected_buf,
            "switch {switch_idx} buffer matches exact expected value derived from emitted GOPs"
        );
    }
}

#[test]
fn target_zero_switch_starts_at_first_rap() {
    let now = Instant::now();
    let mut aligner = TsSpliceAligner::new(1024 * 1024, Duration::from_secs(5), Duration::from_secs(5), Duration::ZERO);
    emitted(aligner.push(join(&[pat(0), video_pmt(0), pes(VIDEO_PID, true, false, &[&h264(5)])]), now));
    assert_eq!(aligner.buffer_at(now), Duration::ZERO);

    aligner.begin_member(now);
    // With trim_target == 0, switch uses FirstRap and starts at the very first RAP.
    let input = [video_cont(false), pat(0), video_pmt(0), pes(VIDEO_PID, true, false, &[&h264(5)])];
    let out = emitted(aligner.push(join(&input), now));
    let out_packets = packets(&out);
    assert_eq!(out_packets.len(), 3);
    assert_eq!(pid_of(out_packets[0]), 0);
    assert_eq!(pid_of(out_packets[1]), PMT_PID);
    assert_eq!(pid_of(out_packets[2]), VIDEO_PID);
}

#[test]
fn test_f3_4_switch_with_3s_buffer_and_2s_hold_evaluates_target_at_emit() {
    let target = Duration::from_secs(3);
    let mut aligner = TsSpliceAligner::new(10 * 1024 * 1024, Duration::from_secs(10), Duration::from_secs(10), target);

    let base = Instant::now();
    // First member bursts and settles to establish ~3 s buffer:
    for i in 0..20 {
        let _ = aligner.push(join(&make_pcr_gop(i, 0, 0)), base + Duration::from_millis(50 * i));
    }
    let m1_emit_time = base + Duration::from_millis(2050);
    let initial_out = emitted(aligner.push(join(&make_pcr_gop(20, 0, 0)), m1_emit_time));
    assert_eq!(packets(&initial_out).len(), 3 * 27, "member 1 emits 3 GOPs (GOPs 18..20)");

    let expected_buf = Duration::from_nanos((3 * FRAMES_PER_GOP - 1) * FRAME_TICKS * 1_000 / 27);
    assert_eq!(aligner.buffer_at(m1_emit_time), expected_buf);

    // Switch at switch_time = m1_emit_time. Buffer at switch is ~2.96 s (~3 s).
    let switch_time = m1_emit_time;
    aligner.begin_member(switch_time);
    assert!(aligner.is_aligning());

    // New member bursts 20 GOPs over 50 ms intervals.
    for i in 0..20 {
        let now = switch_time + Duration::from_millis(50 * i);
        let out = aligner.push(join(&make_pcr_gop(i, 0, 0)), now);
        assert!(matches!(out, SpliceOutput::Hold));
    }

    // Settle at switch_time + 1.96 s (hold duration is ~2 s):
    // Player drains 1.96 s from 2.96 s, leaving 1.0 s buffer remaining at emit.
    // Effective target at emit is 3 - (3 - 2) = 2 s.
    let settle_time = switch_time + Duration::from_millis(1960);
    let out = aligner.push(join(&make_pcr_gop(20, 0, 0)), settle_time);
    assert!(!aligner.is_aligning());

    let out = emitted(out);
    let out_packets = packets(&out);

    // Target 2 s behind GOP 20 (newest PCR ~20.96 s) is 18.96 s -> starts at GOP 19 (at 18.96 s).
    // It emits GOPs 19 and 20 (exact GOP: 2 GOPs = 54 packets).
    assert_eq!(out_packets.len(), 2 * 27, "effective target at emit is 2s (3s - (3s - 2s)): starts at GOP 19 (2 GOPs)");
    assert_eq!(pid_of(out_packets[0]), 0);
    assert_eq!(pid_of(out_packets[1]), PMT_PID);
    assert_eq!(pid_of(out_packets[2]), VIDEO_PID);
}

#[test]
fn switch_buffer_drains_from_the_switch_not_from_the_first_packet() {
    let target = Duration::from_secs(3);
    let mut aligner = TsSpliceAligner::new(10 * 1024 * 1024, Duration::from_secs(10), Duration::from_secs(10), target);
    let base = Instant::now();
    for i in 0..20 {
        let _ = aligner.push(join(&make_pcr_gop(i, 0, 0)), base + Duration::from_millis(50 * i));
    }
    let switch_time = base + Duration::from_millis(2050);
    let _ = emitted(aligner.push(join(&make_pcr_gop(20, 0, 0)), switch_time));
    aligner.begin_member(switch_time);

    // The new member's first bytes arrive 1.5 s after the switch (member open time).
    let first_packet = switch_time + Duration::from_millis(1500);
    for i in 0..20 {
        let out = aligner.push(join(&make_pcr_gop(i, 0, 0)), first_packet + Duration::from_millis(50 * i));
        assert!(matches!(out, SpliceOutput::Hold));
    }
    // Emit 3.46 s after the switch: the 2.96 s player buffer is drained, so the full 3 s target applies
    // and the member starts at GOP 18 (GOPs 18 to 20).
    let out = emitted(aligner.push(join(&make_pcr_gop(20, 0, 0)), first_packet + Duration::from_millis(1960)));
    assert_eq!(packets(&out).len(), 3 * 27, "buffer drained since the switch: start 3 s behind at GOP 18");
}

fn make_pcr_gop_4s(gop_idx: u64, pat_version: u8, pcr_start_ticks: u64) -> Vec<Vec<u8>> {
    let frames_per_gop = 100;
    let mut gop = Vec::with_capacity(102);
    gop.push(pat(pat_version));
    gop.push(video_pmt(0));
    let gop_base_pcr = pcr_start_ticks.wrapping_add(gop_idx * frames_per_gop * FRAME_TICKS);
    gop.push(pes_with_pcr(VIDEO_PID, true, gop_base_pcr, &[&h264(5)]));
    for frame in 1..frames_per_gop {
        let frame_pcr = gop_base_pcr.wrapping_add(frame * FRAME_TICKS);
        gop.push(pes_with_pcr(VIDEO_PID, false, frame_pcr, &[&h264(1)]));
    }
    gop
}

#[test]
fn five_switches_with_4s_gop_keep_buffer_bounded_and_non_growing() {
    let target = Duration::from_secs(3);
    let mut aligner = TsSpliceAligner::new(10 * 1024 * 1024, Duration::from_secs(10), Duration::from_secs(10), target);
    let base = Instant::now();
    let one_frame = Duration::from_millis(40);
    let max_allowed_buffer = target + super::live_edge::FRESH_RAP_SLACK + one_frame;

    // Member 1 (first member, no player buffer) bursts 4 GOPs (0..3, each 4s) over 150 ms.
    for i in 0..4 {
        let _ = aligner.push(join(&make_pcr_gop_4s(i, 0, 0)), base + Duration::from_millis(50 * i));
    }
    // Member 1 settles at 600 ms (media reaches 16.0s + 7 frames = 16.28s).
    // Target 3s behind 16.28s is 13.28s -> newest RAP <= 13.28s is GOP 3 at 12.0s.
    // Emits GOP 3 (100 frames) + 7 frames = 107 frames = 4280 ms media delta.
    let m1_gop4 = make_pcr_gop_4s(4, 0, 0);
    let m1_emit_time = base + Duration::from_millis(600);
    let m1_out = emitted(aligner.push(join(&m1_gop4[..10]), m1_emit_time));
    assert!(!aligner.is_aligning());
    let _ = m1_out;

    let mut current_time = m1_emit_time;
    let initial_buf = aligner.buffer_at(current_time);
    assert_eq!(initial_buf, Duration::from_millis(4280));

    let mut prev_switch_buf = initial_buf;

    // Five switches in a row:
    for switch_idx in 1..=5 {
        // Switch to next member:
        let buf_at_switch = aligner.buffer_at(current_time);
        assert_eq!(buf_at_switch, Duration::from_millis(4280));

        aligner.begin_member(current_time);
        assert!(aligner.is_aligning());

        // Next member bursts 3 GOPs (each 4s) over 150 ms (media reaches 12.0s):
        for i in 0..3 {
            let _ = aligner.push(join(&make_pcr_gop_4s(i, 0, 0)), current_time + Duration::from_millis(50 * i));
        }
        // At 150 ms, burst ends with GOP 3's keyframe at 12.0s plus 85 frames (media 15.4s):
        let gop3 = make_pcr_gop_4s(3, 0, 0);
        let _ = aligner.push(join(&gop3[..88]), current_time + Duration::from_millis(150));

        // Real-time frames arrive from 150 ms to 550 ms (400 ms window) in 100 ms steps:
        for step in 1..=3 {
            let wall_ms = 150 + step * 100;
            let pcr_ms = 15_400 + step * 100;
            let pkt = pes_with_pcr(VIDEO_PID, false, pcr_ms * (PCR_HZ / 1000), &[&h264(1)]);
            let out = aligner.push(Bytes::from(pkt), current_time + Duration::from_millis(wall_ms));
            assert!(matches!(out, SpliceOutput::Hold));
        }

        // At 550 ms (400 ms since burst end), stream settles into real time.
        // But GOP 3 is at 12.0 s (span = 15.8 s - 12.0 s = 3.8 s > 500 ms). Aligner must hold!
        let pcr550 = 15_800 * (PCR_HZ / 1000);
        let pkt550 = pes_with_pcr(VIDEO_PID, false, pcr550, &[&h264(1)]);
        let out_settle = aligner.push(Bytes::from(pkt550), current_time + Duration::from_millis(550));
        assert!(matches!(out_settle, SpliceOutput::Hold), "switch {switch_idx} must hold after settle");

        // Real time continues: push at 650 ms (media 15.9 s):
        let pcr650 = 15_900 * (PCR_HZ / 1000);
        let pkt650 = pes_with_pcr(VIDEO_PID, false, pcr650, &[&h264(1)]);
        let out650 = aligner.push(Bytes::from(pkt650), current_time + Duration::from_millis(650));
        assert!(matches!(out650, SpliceOutput::Hold));

        // At 750 ms, GOP 4 arrives with keyframe at 16.0 s!
        let gop4 = make_pcr_gop_4s(4, 0, 0);
        let out_emit = emitted(aligner.push(join(&gop4[..3]), current_time + Duration::from_millis(750)));
        current_time += Duration::from_millis(750);
        assert!(!aligner.is_aligning(), "switch {switch_idx} emits when fresh GOP 4 arrives");

        let out_pkts = packets(&out_emit);
        assert_eq!(pid_of(out_pkts[0]), 0, "first emitted packet is PAT");
        assert_eq!(pid_of(out_pkts[1]), PMT_PID, "second packet is PMT");
        assert_eq!(pid_of(out_pkts[2]), VIDEO_PID, "third packet is keyframe");

        let buf_after_switch = aligner.buffer_at(current_time);
        assert!(
            buf_after_switch <= max_allowed_buffer,
            "switch {switch_idx} buffer {buf_after_switch:?} exceeds max allowed {max_allowed_buffer:?}"
        );
        assert!(
            buf_after_switch <= prev_switch_buf,
            "switch {switch_idx} buffer {buf_after_switch:?} grew from previous {prev_switch_buf:?}"
        );
        assert_eq!(
            buf_after_switch,
            Duration::from_millis(3530),
            "switch {switch_idx} buffer matches exact derived value"
        );
        prev_switch_buf = buf_after_switch;

        // Player streams 50 frames (2000 ms media) over 1250 ms wall time (25 ms per frame),
        // recovering 750 ms to restore the buffer to 4280 ms for the next switch.
        let mut stream_pcr_ticks = 16_000 * (PCR_HZ / 1000);
        for _ in 0..50 {
            current_time += Duration::from_millis(25);
            stream_pcr_ticks += 40 * (PCR_HZ / 1000);
            let pkt = pes_with_pcr(VIDEO_PID, false, stream_pcr_ticks, &[&h264(1)]);
            let _ = emitted(aligner.push(Bytes::from(pkt), current_time));
        }
    }
}

#[test]
fn pcr_clock_rejects_extension_above_299() {
    let mut clock = super::pcr_clock::PcrClock::new();
    let pid = 0x100;
    let pkt1 = pcr_packet_raw(pid, 1000, 299, false);
    let pcr1 = 1000 * 300 + 299;
    assert_eq!(clock.observe(crate::packet::TsPacket(&pkt1)), super::pcr_clock::PcrProgress::First { pcr: pcr1 });
    assert_eq!(clock.media_time(), Duration::ZERO);

    let pkt_invalid1 = pcr_packet_raw(pid, 1000, 300, false);
    assert_eq!(clock.observe(crate::packet::TsPacket(&pkt_invalid1)), super::pcr_clock::PcrProgress::None);
    assert_eq!(clock.media_time(), Duration::ZERO);

    let pkt_invalid2 = pcr_packet_raw(pid, 1000, 511, false);
    assert_eq!(clock.observe(crate::packet::TsPacket(&pkt_invalid2)), super::pcr_clock::PcrProgress::None);
    assert_eq!(clock.media_time(), Duration::ZERO);

    let pkt2 = pcr_packet_raw(pid, 1090, 299, false);
    assert_eq!(
        clock.observe(crate::packet::TsPacket(&pkt2)),
        super::pcr_clock::PcrProgress::Advance {
            delta: Duration::from_millis(1),
            media_time: Duration::from_millis(1),
        }
    );
    assert_eq!(clock.media_time(), Duration::from_millis(1));
}

#[test]
fn pcr_clock_discontinuity_flag_resets_timeline_on_small_delta() {
    let mut clock = super::pcr_clock::PcrClock::new();
    let pid = 0x100;
    let pkt1 = pcr_packet_raw(pid, 1000, 0, true);
    assert!(crate::packet::TsPacket(&pkt1).discontinuity());
    assert_eq!(clock.observe(crate::packet::TsPacket(&pkt1)), super::pcr_clock::PcrProgress::First { pcr: 300_000 });
    assert_eq!(clock.media_time(), Duration::ZERO);

    let pkt2 = pcr_packet_raw(pid, 1090, 0, false);
    assert!(!crate::packet::TsPacket(&pkt2).discontinuity());
    assert_eq!(
        clock.observe(crate::packet::TsPacket(&pkt2)),
        super::pcr_clock::PcrProgress::Advance {
            delta: Duration::from_millis(1),
            media_time: Duration::from_millis(1),
        }
    );
    assert_eq!(clock.media_time(), Duration::from_millis(1));

    let pkt3 = pcr_packet_raw(pid, 1180, 0, true);
    assert!(crate::packet::TsPacket(&pkt3).discontinuity());
    let pcr3 = 1180 * 300;
    assert_eq!(
        clock.observe(crate::packet::TsPacket(&pkt3)),
        super::pcr_clock::PcrProgress::Discontinuity { pcr: pcr3 }
    );
    assert_eq!(clock.media_time(), Duration::ZERO);

    let pkt4 = pcr_packet_raw(pid, 1270, 0, false);
    assert_eq!(
        clock.observe(crate::packet::TsPacket(&pkt4)),
        super::pcr_clock::PcrProgress::Advance {
            delta: Duration::from_millis(1),
            media_time: Duration::from_millis(1),
        }
    );
    assert_eq!(clock.media_time(), Duration::from_millis(1));
}

#[test]
fn aligner_two_program_non_video_pmt_does_not_qualify_video_keyframe() {
    let now = Instant::now();
    let mut aligner = switched_aligner(0, now);

    // Prime member with video PMT so video stream is selected
    let prime = join(&[two_program_pat(), video_pmt(0)]);
    assert!(matches!(aligner.push(prime, now), SpliceOutput::Hold));

    // Next chunk in member: PAT + non-video PMT + video keyframe must NOT qualify as RAP
    let non_video_chunk = join(&[two_program_pat(), radio_pmt(), pes(VIDEO_PID, true, false, &[&h264(5)])]);
    let out = aligner.push(non_video_chunk, now);
    assert!(matches!(out, SpliceOutput::Hold), "keyframe after non-video PMT must NOT produce a RAP");

    // Chunk with video PMT + video keyframe MUST qualify as RAP
    let video_chunk = join(&[two_program_pat(), video_pmt(0), pes(VIDEO_PID, true, false, &[&h264(5)])]);
    let out2 = emitted(aligner.push(video_chunk, now));
    let pkts = packets(&out2);
    assert_eq!(pid_of(pkts[0]), 0);
    assert_eq!(pid_of(pkts[1]), PMT_PID);
    assert_eq!(pid_of(pkts[2]), VIDEO_PID);
}

#[test]
fn poll_hold_at_floor_instant_emits_with_psi_rewrite() {
    let target = Duration::from_secs(3);
    let mut aligner = TsSpliceAligner::new(10 * 1024 * 1024, Duration::from_secs(10), Duration::from_secs(10), target);
    let base = Instant::now();

    // Member 1 (first member, no player buffer) bursts 4 GOPs (0..3, each 4s) over 150 ms:
    for i in 0..4 {
        let _ = aligner.push(join(&make_pcr_gop_4s(i, 0, 0)), base + Duration::from_millis(50 * i));
    }
    // Member 1 settles at 600 ms and emits:
    let m1_gop4 = make_pcr_gop_4s(4, 0, 0);
    let m1_emit_time = base + Duration::from_millis(600);
    let m1_out = emitted(aligner.push(join(&m1_gop4[..10]), m1_emit_time));
    assert!(!aligner.is_aligning());
    let _ = m1_out;

    let initial_buf = aligner.buffer_at(m1_emit_time);
    assert_eq!(initial_buf, Duration::from_millis(4280));

    // Switch to member 2 at m1_emit_time with 4280ms player buffer:
    let switch_time = m1_emit_time;
    aligner.begin_member(switch_time);
    assert!(aligner.is_aligning());

    // Floor instant: switch_time + (4280ms - 1000ms) = switch_time + 3280ms:
    let floor_instant = switch_time + Duration::from_millis(3280);

    // Member 2 bursts 3 GOPs (each 4s) over 150 ms (media reaches 12.0s):
    for i in 0..3 {
        let _ = aligner.push(join(&make_pcr_gop_4s(i, 0, 0)), switch_time + Duration::from_millis(50 * i));
    }

    // Real-time delivery every 100 ms after the burst ended at 150 ms, up to shortly before the floor instant.
    // The hold settles, but the chosen RAP (GOP 2 at 8 s) is not fresh, so it holds.
    for wall_ms in (250..=3250).step_by(100) {
        let pcr_ms = 12000 + (wall_ms - 150);
        let pkt = pes_with_pcr(VIDEO_PID, false, pcr_ms * (PCR_HZ / 1000), &[&h264(1)]);
        let out = aligner.push(Bytes::from(pkt), switch_time + Duration::from_millis(wall_ms));
        assert!(matches!(out, SpliceOutput::Hold), "holding at {wall_ms} ms");
    }

    // The upstream pauses. Before the floor instant the hold stays; the wake instant is the floor instant.
    let before_floor = switch_time + Duration::from_millis(3260);
    assert_eq!(aligner.hold_deadline(before_floor), Some(floor_instant));
    assert!(aligner.poll_hold(before_floor).is_none());
    assert!(aligner.is_aligning());

    // Poll at floor instant emits:
    let out = aligner.poll_hold(floor_instant);
    let emitted = out.expect("poll_hold at floor instant emits");
    assert!(!aligner.is_aligning(), "aligning ended");

    let out_packets = packets(&emitted);
    assert_eq!(section_version(out_packets[0]), 1, "PAT version continues after member 1's version 0");
    assert_eq!(section_version(out_packets[1]), 1, "PMT version continues after member 1's version 0");
    assert!(discontinuity(out_packets[2]), "first video packet has discontinuity flag set");
}
