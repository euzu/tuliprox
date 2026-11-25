#![allow(clippy::cast_possible_truncation, reason = "test builders write PIDs and lengths into single bytes")]

use super::{
    live_edge::{TrimOutput, TsLiveEdgeTrim},
    test_support::*,
};
use crate::packet::TS_PACKET_SIZE;
use bytes::Bytes;
use std::time::{Duration, Instant};

const PCR_HZ: u64 = 27_000_000;
const FRAME_TICKS: u64 = 1_080_000; // 40 ms at 27 MHz
const FRAMES_PER_GOP: u64 = 25; // 1 second per GOP

/// Builds one synthetic GOP of 27 packets:
/// - Packet 0: PAT with version `gop_idx as u8`
/// - Packet 1: Video PMT (version 0)
/// - Packet 2: IDR PES with PCR on `VIDEO_PID`
/// - Packets 3..26: 24 continuation video PES packets, each stepping PCR by 40 ms
fn make_gop(gop_idx: u64, pcr_start_ticks: u64) -> Vec<Vec<u8>> {
    let mut gop = Vec::with_capacity(27);
    gop.push(pat((gop_idx % 32) as u8));
    gop.push(video_pmt(0));

    let gop_base_pcr = pcr_start_ticks.wrapping_add(gop_idx * FRAMES_PER_GOP * FRAME_TICKS);
    // Keyframe IDR on frame 0
    gop.push(pes_with_pcr(VIDEO_PID, true, gop_base_pcr, &[&h264(5)]));

    // Non-keyframe slices for frames 1..24
    for frame in 1..FRAMES_PER_GOP {
        let frame_pcr = gop_base_pcr.wrapping_add(frame * FRAME_TICKS);
        gop.push(pes_with_pcr(VIDEO_PID, false, frame_pcr, &[&h264(1)]));
    }
    gop
}

#[test]
fn test_1_burst_then_realtime_emits_at_target() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 10_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();

    // Build 20 GOPs (20 seconds of media)
    let gops: Vec<Vec<Vec<u8>>> = (0..20).map(|i| make_gop(i, 0)).collect();

    // Push 20 GOPs within 1 second of wall time (fast burst: 50 ms steps)
    for (i, gop) in gops.iter().enumerate() {
        let now = base + Duration::from_millis(50 * i as u64);
        let output = trim.push(join(gop), now);
        assert_eq!(output, TrimOutput::Hold, "holding during burst at GOP {i}");
    }

    // Now push in real time (1 GOP = 1 second media per 1 second wall time)
    // GOP 20 pushed at wall + 2.05s
    let gop20 = make_gop(20, 0);
    let now = base + Duration::from_millis(2050);
    let output = trim.push(join(&gop20), now);

    // Over the last 1 s of wall time (between 1.0s and 2.05s), media grew by 1 s,
    // so lead growth is ~0 ms < 250 ms -> settled!
    let emitted = match output {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit after settle in real time"),
    };

    assert!(!trim.is_holding());

    // Newest media time at GOP 20 is ~21s (20.96s). Target is 3s.
    // Threshold is ~18s (17.96s) -> newest GOP whose PAT media time is <= 17.96s is GOP 18!
    let gop18 = make_gop(18, 0);
    assert_eq!(&emitted[..TS_PACKET_SIZE], &gop18[0][..], "emitted stream must start with GOP 18 PAT packet");
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &gop18[1][..]);
    assert_eq!(&emitted[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE], &gop18[2][..]);
}

#[test]
fn test_2_no_burst_realtime_emits_after_settle_window_at_oldest_candidate() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 1_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();
    let gop0 = make_gop(0, 0);
    let gop1 = make_gop(1, 0);

    // Push GOP 0 at t = 0
    let out0 = trim.push(join(&gop0), base);
    assert_eq!(out0, TrimOutput::Hold);

    // Push GOP 1 at t = 1.05s (after SETTLE_WINDOW = 1.0s)
    let out1 = trim.push(join(&gop1), base + Duration::from_millis(1050));

    // In real time from start, lead growth over 1s is 0 < 250 ms -> settled!
    let emitted = match out1 {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit after settle window in real time"),
    };

    // Newest media is 1s, which is less than target 3s, so emits from oldest candidate (GOP 0)
    assert_eq!(&emitted[..TS_PACKET_SIZE], &gop0[0][..]);
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &gop0[1][..]);
    assert_eq!(&emitted[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE], &gop0[2][..]);
}

#[test]
fn test_3_slow_burst_never_settles_emits_at_max_hold() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(5);
    let max_bytes = 10_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();

    // Provider sends 2s media per 1s wall time (2x real time): lead grows by 1s every 1s wall,
    // which is 1000 ms > 250 ms, so it never settles.
    // 10 GOPs pushed over 5 seconds (500 ms per GOP)
    for i in 0..10 {
        let gop = make_gop(i, 0);
        let now = base + Duration::from_millis(500 * i);
        let out = trim.push(join(&gop), now);
        assert_eq!(out, TrimOutput::Hold, "holding at step {i}");
    }

    // Now at max_hold (5.0s): push GOP 10 at base + 5001ms
    let gop10 = make_gop(10, 0);
    let now = base + Duration::from_millis(5001);
    let out = trim.push(join(&gop10), now);

    let emitted = match out {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit at max_hold timeout"),
    };

    assert!(!trim.is_holding());
    // At GOP 10, media time is 10.96s. Target is 3s. Threshold is 7.96s.
    // Expected start is GOP 8!
    let gop8 = make_gop(8, 0);
    assert_eq!(&emitted[..TS_PACKET_SIZE], &gop8[0][..]);
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &gop8[1][..]);
    assert_eq!(&emitted[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE], &gop8[2][..]);
}

#[test]
fn test_4_memory_bound_during_long_burst() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(60);
    let max_bytes = 10_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();
    let gop_size = 27 * TS_PACKET_SIZE;

    // Push 30 GOPs (30 seconds of media) in 0.5s of wall time
    for i in 0..30 {
        let gop = make_gop(i, 0);
        let now = base + Duration::from_millis(10 * i);
        let out = trim.push(join(&gop), now);
        assert_eq!(out, TrimOutput::Hold);
    }

    // Trimmer is still holding
    assert!(trim.is_holding());

    // When we finish(), the held buffer length must be bounded by target (3s = 3 GOPs) + 2 GOPs
    let held = trim.finish().expect("held bytes");
    let max_allowed = 6 * gop_size; // at most ~5-6 GOPs
    assert!(held.len() <= max_allowed, "held bytes {} exceeds bound {}", held.len(), max_allowed);
}

#[test]
fn test_5_pcr_wrap_behaves_like_case_1() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 10_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();
    // Start PCR 5 seconds before PCR_MODULUS so it wraps around during the burst
    let start_pcr = super::pcr_clock::PCR_MODULUS - 5 * PCR_HZ;

    let gops: Vec<Vec<Vec<u8>>> = (0..20).map(|i| make_gop(i, start_pcr)).collect();

    // Burst: 20 GOPs pushed in 1s wall time
    for (i, gop) in gops.iter().enumerate() {
        let now = base + Duration::from_millis(50 * i as u64);
        let output = trim.push(join(gop), now);
        assert_eq!(output, TrimOutput::Hold);
    }

    // Real time: GOP 20 at wall + 2.05s
    let gop20 = make_gop(20, start_pcr);
    let now = base + Duration::from_millis(2050);
    let output = trim.push(join(&gop20), now);

    let emitted = match output {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit after settle with PCR wrap"),
    };

    // Expected start is GOP 18
    let gop18 = make_gop(18, start_pcr);
    assert_eq!(&emitted[..TS_PACKET_SIZE], &gop18[0][..]);
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &gop18[1][..]);
    assert_eq!(&emitted[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE], &gop18[2][..]);
}

#[test]
fn test_6_pcr_jump_drops_candidates_before_jump() {
    let target = Duration::from_secs(1);
    let max_hold = Duration::from_secs(5);
    let max_bytes = 10_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();

    // Push 3 GOPs on timeline A (GOP 0, 1, 2)
    let gop0 = make_gop(0, 0);
    let gop1 = make_gop(1, 0);
    let gop2 = make_gop(2, 0);

    trim.push(join(&gop0), base);
    trim.push(join(&gop1), base + Duration::from_millis(100));
    trim.push(join(&gop2), base + Duration::from_millis(200));

    // Now a PCR jump: GOP 3 has PCR jump of +10 seconds
    let jump_pcr = 10 * PCR_HZ;
    let gop3 = make_gop(3, jump_pcr);
    let gop4 = make_gop(4, jump_pcr);
    let gop5 = make_gop(5, jump_pcr);

    trim.push(join(&gop3), base + Duration::from_millis(300));
    trim.push(join(&gop4), base + Duration::from_millis(400));
    let out = trim.push(join(&gop5), base + Duration::from_millis(5001)); // max_hold reached

    let emitted = match out {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit at max_hold"),
    };

    // Must start on timeline B (GOP 3, 4, or 5), NEVER GOP 0, 1, or 2
    assert_ne!(&emitted[..TS_PACKET_SIZE], &gop0[0][..]);
    assert_ne!(&emitted[..TS_PACKET_SIZE], &gop1[0][..]);
    assert_ne!(&emitted[..TS_PACKET_SIZE], &gop2[0][..]);

    // On timeline B, media time at GOP 5 is 2.96s, target 1s -> threshold 1.96s -> GOP 5
    assert_eq!(&emitted[..TS_PACKET_SIZE], &gop5[0][..]);
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &gop5[1][..]);
    assert_eq!(&emitted[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE], &gop5[2][..]);
}

#[test]
fn test_7_radio_pmt_without_video_candidate() {
    let target = Duration::from_secs(1);
    let max_hold = Duration::from_secs(5);
    let max_bytes = 1_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();

    // Build radio GOPs: PAT, audio PMT, PCR packet on AUDIO_PID, audio PES
    let radio_pmt_pkt = pmt(0, &[(AAC, AUDIO_PID)]);
    let make_radio_chunk = |idx: u64| {
        let pcr = idx * PCR_HZ;
        vec![pat(idx as u8), radio_pmt_pkt.clone(), pcr_packet(AUDIO_PID, pcr), ts(AUDIO_PID, true, None, &[0x11; 50])]
    };

    let r0 = make_radio_chunk(0);
    let r1 = make_radio_chunk(1);
    let r2 = make_radio_chunk(2);

    trim.push(join(&r0), base);
    trim.push(join(&r1), base + Duration::from_millis(50));
    let out = trim.push(join(&r2), base + Duration::from_millis(5001));

    let emitted = match out {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit for radio"),
    };

    // Starts at PAT of radio program (r2: media_time 1s <= threshold 1s at newest 2s)
    assert_eq!(&emitted[..TS_PACKET_SIZE], &r2[0][..]);
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &r2[1][..]);
}

#[test]
fn test_8_scrambled_like_pcr_boundary_fallback() {
    let target = Duration::from_secs(2);
    let max_hold = Duration::from_secs(5);
    let max_bytes = 1_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();

    // Stream with PCR on VIDEO_PID, but only non-IDR slices with NO RAI (no keyframe detected)
    let make_scrambled_chunk = |sec: u64| {
        let pcr = sec * PCR_HZ;
        vec![pcr_packet(VIDEO_PID, pcr), ts(VIDEO_PID, false, None, &[0x55; 100])]
    };

    let s0 = make_scrambled_chunk(0);
    let s1 = make_scrambled_chunk(1);
    let s2 = make_scrambled_chunk(2);
    let s3 = make_scrambled_chunk(3);

    trim.push(join(&s0), base);
    trim.push(join(&s1), base + Duration::from_millis(50));
    trim.push(join(&s2), base + Duration::from_millis(100));
    let out = trim.push(join(&s3), base + Duration::from_millis(5001));

    let emitted = match out {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit on scrambled fallback"),
    };

    // At sec 3, target 2s -> threshold 1s -> newest PCR sample <= 1s is s1!
    // So emitted bytes start at the PCR packet of s1!
    assert_eq!(&emitted[..TS_PACKET_SIZE], &s1[0][..]);
}

#[test]
fn test_9_non_ts_input_emits_immediately() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(5);
    let max_bytes = 100_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let garbage = Bytes::from(vec![0x12, 0x34, 0x56, 0x78]);
    let out = trim.push(garbage.clone(), Instant::now());

    assert_eq!(out, TrimOutput::Emit(garbage));
    assert!(!trim.is_holding());
}

#[test]
fn test_10_no_pcr_emits_everything_at_max_hold() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(5);
    let max_bytes = 1_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();

    // Plain TS packets without PCR
    let p0 = ts(0x200, false, None, &[0xAA; 100]);
    let p1 = ts(0x200, false, None, &[0xBB; 100]);

    let out0 = trim.push(join(std::slice::from_ref(&p0)), base);
    assert_eq!(out0, TrimOutput::Hold);

    let out1 = trim.push(join(std::slice::from_ref(&p1)), base + Duration::from_secs(5));
    let emitted = match out1 {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit at max_hold"),
    };

    // Emits everything held, starting from offset 0
    assert_eq!(emitted.len(), 2 * TS_PACKET_SIZE);
    assert_eq!(&emitted[..TS_PACKET_SIZE], &p0[..]);
    assert_eq!(&emitted[TS_PACKET_SIZE..], &p1[..]);
}

#[test]
fn test_11_after_emit_push_is_passthrough_zero_copy() {
    let target = Duration::from_secs(1);
    let max_hold = Duration::from_secs(2);
    let max_bytes = 100_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    // Non-TS causes immediate emit
    let first = Bytes::from(vec![0x00; 100]);
    let _ = trim.push(first, Instant::now());
    assert!(!trim.is_holding());

    // Subsequent pushes return input unchanged (zero copy)
    let next = Bytes::from(vec![0x47; 188]);
    let out = trim.push(next.clone(), Instant::now());
    match out {
        TrimOutput::Emit(emitted) => {
            assert_eq!(emitted, next);
            assert_eq!(emitted.as_ptr(), next.as_ptr(), "pointer equality for zero copy");
        }
        TrimOutput::Hold => panic!("must not hold after emit"),
    }
}

#[test]
fn test_12_finish_during_hold_returns_best_start_point() {
    let target = Duration::from_secs(1);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 1_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();
    let gop0 = make_gop(0, 0);
    let gop1 = make_gop(1, 0);
    let gop2 = make_gop(2, 0);

    trim.push(join(&gop0), base);
    trim.push(join(&gop1), base + Duration::from_millis(50));
    trim.push(join(&gop2), base + Duration::from_millis(100));

    assert!(trim.is_holding());

    let finished = trim.finish().expect("held bytes returned on finish");
    assert!(!trim.is_holding());

    // At GOP 2 (media 2.96s), target 1s -> threshold 1.96s -> starts at GOP 2!
    assert_eq!(&finished[..TS_PACKET_SIZE], &gop2[0][..]);

    // Subsequent finish returns None
    assert_eq!(trim.finish(), None);

    // Subsequent push is pass-through
    let next = Bytes::from(vec![0x47; 188]);
    let out = trim.push(next.clone(), Instant::now());
    assert_eq!(out, TrimOutput::Emit(next));
}

#[test]
fn test_burst_pause_does_not_settle() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 10_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();

    // 1. 5 s media in 100 ms (GOPs 0..5, 20 ms apart)
    for i in 0..5 {
        let gop = make_gop(i, 0);
        let now = base + Duration::from_millis(20 * i);
        let out = trim.push(join(&gop), now);
        assert_eq!(out, TrimOutput::Hold, "holding GOP {i} in initial burst");
    }

    // 2. 1.1 s pause, then one chunk: must not emit at the lone chunk
    let lone_chunk = join(&[pes_with_pcr(VIDEO_PID, false, 5 * FRAMES_PER_GOP * FRAME_TICKS, &[&h264(1)])]);
    let lone_now = base + Duration::from_millis(80 + 1100);
    let lone_out = trim.push(lone_chunk, lone_now);
    assert_eq!(lone_out, TrimOutput::Hold, "must not settle and emit on lone chunk after pause");

    // 3. Rest of a 20 s burst within 0.5 s (15 s of media in 500 ms, chunks of 2 GOPs)
    let burst_gops: Vec<Vec<Vec<u8>>> = (5..20).map(|i| make_gop(i, 0)).collect();
    for (step, pair) in burst_gops.chunks(2).enumerate() {
        let chunk_bytes = join(&pair.concat());
        let now = base + Duration::from_millis(1200 + step as u64 * 60);
        let out = trim.push(chunk_bytes, now);
        assert_eq!(out, TrimOutput::Hold, "holding burst chunks at step {step}");
    }

    // 4. Real time phase after the burst: GOP 20 at +1.05 s after the burst
    let gop20 = make_gop(20, 0);
    let rt_now = base + Duration::from_millis(1200 + 15 * 30 + 1050);
    let rt_out = trim.push(join(&gop20), rt_now);

    let emitted = match rt_out {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit after real-time phase"),
    };

    assert!(!trim.is_holding());

    // Newest media time at GOP 20 is ~21 s (20.96 s). Target is 3 s.
    // Threshold is 17.96 s -> newest GOP <= 17.96 s is GOP 18.
    let gop18 = make_gop(18, 0);
    let gop18_bytes = join(&gop18);
    assert_eq!(&emitted[..gop18_bytes.len()], &gop18_bytes[..], "emitted stream must start with exact bytes of GOP 18");
}

#[test]
fn test_repeated_pat_inside_open_pes_preserves_rap_media_time() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 1_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);
    let base = Instant::now();

    // 0: PCR packet to establish media time = 0 at start
    let p0 = pcr_packet(VIDEO_PID, 10 * PCR_HZ);
    // 1: First PAT (PAT 0) at stream pos 188, media time = 0 ms
    let p1 = pat(0);
    // 2: PMT
    let p2 = video_pmt(0);
    // 3: Video PES start on VIDEO_PID without keyframe NAL (open PES begins, pes_pat = 188), PCR = 40 ms
    let p3 = pes_with_pcr(VIDEO_PID, false, 10 * PCR_HZ + 40 * (PCR_HZ / 1000), &[&h264(1)]);
    // 4: Repeated PAT (PAT 1) at stream pos 752 inside open PES
    let p4 = pat(1);
    // 5: Video continuation packet with PCR = 120 ms and IDR keyframe NAL
    let mut keyframe_payload = vec![0x00, 0x00, 0x01, 0x65, 0x88, 0x84];
    keyframe_payload.resize(100, 0xFF);
    let p5 = ts(VIDEO_PID, false, Some(0x00), &keyframe_payload);

    let packets = join(&[p0, p1, p2, p3, p4, p5]);
    let out = trim.push(packets, base);
    assert_eq!(out, TrimOutput::Hold);

    let candidates = trim.candidate_media_times();
    assert_eq!(candidates.len(), 1, "exactly one RAP candidate from the completed open PES");
    assert_eq!(candidates[0].0, 188, "candidate offset points to first PAT");
    assert_eq!(
        candidates[0].1,
        Duration::ZERO,
        "candidate media time must match first PAT media time, not completion time"
    );
}

fn make_large_gop(gop_idx: u64, pcr_start_ticks: u64, continuation_packets: usize) -> Vec<Vec<u8>> {
    let mut gop = Vec::with_capacity(3 + continuation_packets);
    gop.push(pat((gop_idx % 32) as u8));
    gop.push(video_pmt(0));
    let gop_base_pcr = pcr_start_ticks.wrapping_add(gop_idx * FRAMES_PER_GOP * FRAME_TICKS);
    gop.push(pes_with_pcr(VIDEO_PID, true, gop_base_pcr, &[&h264(5)]));
    let step_ticks = (FRAMES_PER_GOP * FRAME_TICKS) / (continuation_packets as u64 + 1);
    for frame in 1..=continuation_packets {
        let frame_pcr = gop_base_pcr.wrapping_add(frame as u64 * step_ticks);
        gop.push(pes_with_pcr(VIDEO_PID, false, frame_pcr, &[&h264(1)]));
    }
    gop
}

#[test]
fn test_f3_1_large_gop_memory_cap_continues_hold_until_settle() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    // 500 continuation packets per GOP = 503 packets = 94,564 bytes per GOP
    // Target (3s) = ~284 KB; Target + GOP (4s) = ~378 KB.
    // Set cap to 300 KB (below target + GOP, but above target).
    let max_bytes = 300_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);
    let base = Instant::now();

    let gop_list: Vec<Vec<Vec<u8>>> = (0..10).map(|i| make_large_gop(i, 0, 500)).collect();

    // Push 10 GOPs in fast burst (20 ms apart)
    for (i, gop) in gop_list.iter().enumerate() {
        let now = base + Duration::from_millis(20 * i as u64);
        let out = trim.push(join(gop), now);
        assert_eq!(out, TrimOutput::Hold, "must hold during burst at GOP {i} despite memory cap");
    }

    assert!(trim.is_holding(), "trimmer must still be holding during burst");

    // Push in real time after burst settle window: GOP 10 at 2.05 s
    let gop10 = make_large_gop(10, 0, 500);
    let rt_now = base + Duration::from_millis(2050);
    let out = trim.push(join(&gop10), rt_now);

    let emitted = match out {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit after settle in real time"),
    };

    assert!(!trim.is_holding());
    // At GOP 10 (media time ~10.96 s), target 3 s -> threshold 7.96 s -> newest candidate <= 7.96 s is GOP 8.
    let expected_gop8 = make_large_gop(8, 0, 500);
    assert_eq!(&emitted[..TS_PACKET_SIZE], &expected_gop8[0][..], "emitted stream must start cleanly at GOP 8 PAT");
}

#[test]
fn test_f3_3_no_pcr_emits_after_settle_window() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(5);
    let max_bytes = 1_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();
    let p0 = ts(0x200, false, None, &[0xAA; 100]);
    let p1 = ts(0x200, false, None, &[0xBB; 100]);
    let p2 = ts(0x200, false, None, &[0xCC; 100]);

    // Push at t = 0
    let out0 = trim.push(join(std::slice::from_ref(&p0)), base);
    assert_eq!(out0, TrimOutput::Hold);

    // Push at t = 500 ms (< SETTLE_WINDOW 1s): still holds
    let out1 = trim.push(join(std::slice::from_ref(&p1)), base + Duration::from_millis(500));
    assert_eq!(out1, TrimOutput::Hold);

    // Push at t = 1050 ms (>= SETTLE_WINDOW 1s): emits immediately, not at max_hold (5s)
    let out2 = trim.push(join(std::slice::from_ref(&p2)), base + Duration::from_millis(1050));
    let emitted = match out2 {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit after SETTLE_WINDOW without PCR, not at max_hold"),
    };

    assert_eq!(emitted.len(), 3 * TS_PACKET_SIZE);
    assert_eq!(&emitted[..TS_PACKET_SIZE], &p0[..]);
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &p1[..]);
    assert_eq!(&emitted[2 * TS_PACKET_SIZE..], &p2[..]);
}

#[test]
fn test_f3_7_scrambled_like_starts_at_pat() {
    let target = Duration::from_secs(2);
    let max_hold = Duration::from_secs(5);
    let max_bytes = 1_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();

    // Stream with PAT, PMT, PCR on VIDEO_PID, but only non-IDR slices (no keyframe)
    let make_scrambled_chunk = |sec: u64| {
        let pcr = sec * PCR_HZ;
        vec![pcr_packet(VIDEO_PID, pcr), pat(sec as u8), video_pmt(0), ts(VIDEO_PID, false, None, &[0x55; 100])]
    };

    let s0 = make_scrambled_chunk(0);
    let s1 = make_scrambled_chunk(1);
    let s2 = make_scrambled_chunk(2);
    let s3 = make_scrambled_chunk(3);

    trim.push(join(&s0), base);
    trim.push(join(&s1), base + Duration::from_millis(50));
    trim.push(join(&s2), base + Duration::from_millis(100));
    let out = trim.push(join(&s3), base + Duration::from_millis(5001));

    let emitted = match out {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit on scrambled fallback"),
    };

    // At sec 3, target 2s -> threshold 1s -> newest PAT with media_time <= 1s is s1 PAT (PID 0)
    assert_eq!(&emitted[..TS_PACKET_SIZE], &s1[1][..], "emitted stream must start at PAT, not PCR packet");
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &s1[2][..]);
}
