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

/// Builds one synthetic GOP of `frames_per_gop + 2` packets:
/// - Packet 0: PAT with version `gop_idx as u8`
/// - Packet 1: Video PMT (version 0)
/// - Packet 2: IDR PES with PCR on `VIDEO_PID`
/// - Packets 3..: continuation video PES packets, each stepping PCR by 40 ms
fn make_gop_with_frames(gop_idx: u64, pcr_start_ticks: u64, frames_per_gop: u64) -> Vec<Vec<u8>> {
    let mut gop = Vec::with_capacity((frames_per_gop + 2) as usize);
    gop.push(pat((gop_idx % 32) as u8));
    gop.push(video_pmt(0));

    let gop_base_pcr = pcr_start_ticks.wrapping_add(gop_idx * frames_per_gop * FRAME_TICKS);
    // Keyframe IDR on frame 0
    gop.push(pes_with_pcr(VIDEO_PID, true, gop_base_pcr, &[&h264(5)]));

    // Non-keyframe slices for frames 1..frames_per_gop
    for frame in 1..frames_per_gop {
        let frame_pcr = gop_base_pcr.wrapping_add(frame * FRAME_TICKS);
        gop.push(pes_with_pcr(VIDEO_PID, false, frame_pcr, &[&h264(1)]));
    }
    gop
}

fn make_gop(gop_idx: u64, pcr_start_ticks: u64) -> Vec<Vec<u8>> {
    make_gop_with_frames(gop_idx, pcr_start_ticks, FRAMES_PER_GOP)
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

    // Push 20 GOPs within 1 second of wall time (fast burst: 50 ms steps, ending at 950 ms)
    for (i, gop) in gops.iter().enumerate() {
        let now = base + Duration::from_millis(50 * i as u64);
        let output = trim.push(join(gop), now);
        assert_eq!(output, TrimOutput::Hold, "holding during burst at GOP {i}");
    }

    // Now push in real time after burst ended at 950 ms.
    // Provider transitions to real-time delivery (100 ms media per 100 ms wall).
    // Pushes at 1050 ms, 1150 ms, 1250 ms must still hold (< 400 ms since burst ended).
    for step in 1..=3 {
        let wall_ms = 950 + step * 100;
        let pcr = (20 * 1000 + step * 100) * (PCR_HZ / 1000);
        let packet = pes_with_pcr(VIDEO_PID, false, pcr, &[&h264(1)]);
        let out = trim.push(Bytes::from(packet), base + Duration::from_millis(wall_ms));
        assert_eq!(out, TrimOutput::Hold, "holding in real time at {wall_ms} ms");
    }

    // Push at 1350 ms: first push at least 400 ms after burst ended at 950 ms.
    // Media advance in 400 ms window is 400 ms >= 300 ms, lead growth is 0 <= 100 ms -> settled!
    let pcr1350 = (20 * 1000 + 400) * (PCR_HZ / 1000);
    let pkt1350 = pes_with_pcr(VIDEO_PID, false, pcr1350, &[&h264(1)]);
    let output = trim.push(Bytes::from(pkt1350), base + Duration::from_millis(1350));

    let emitted = match output {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit after settle in real time at 1350 ms"),
    };

    assert!(!trim.is_holding());

    // Newest media time at 1350 ms is ~20.4 s. Target is 3 s.
    // Threshold is ~17.4 s -> newest GOP whose PAT media time is <= 17.4 s is GOP 17!
    let gop17 = make_gop(17, 0);
    assert_eq!(&emitted[..TS_PACKET_SIZE], &gop17[0][..], "emitted stream must start with GOP 17 PAT packet");
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &gop17[1][..]);
    assert_eq!(&emitted[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE], &gop17[2][..]);
}

#[test]
fn test_2_no_burst_realtime_emits_at_first_rap_after_300ms() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 1_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);
    let base = Instant::now();

    // Each short GOP has 3 frames (120 ms media duration, frames at 0, 40, 80 ms).
    // Pushed every 120 ms, lead stays constant at 80 ms <= NO_BURST_MAX_LEAD (150 ms).
    let gop0 = make_short_gop(0, 0, 3);
    let out0 = trim.push(join(&gop0), base);
    assert_eq!(out0, TrimOutput::Hold);

    let gop1 = make_short_gop(1, 0, 3);
    let out1 = trim.push(join(&gop1), base + Duration::from_millis(120));
    assert_eq!(out1, TrimOutput::Hold);

    let gop2 = make_short_gop(2, 0, 3);
    let out2 = trim.push(join(&gop2), base + Duration::from_millis(240));
    assert_eq!(out2, TrimOutput::Hold);

    // GOP 3 pushed at 360 ms: wall time 360 ms >= NO_BURST_PROBE (300 ms).
    // GOP 3 is the first RAP pushed after 300 ms. Lead is (3 * 120 + 80) - 360 = 80 ms <= 150 ms.
    let gop3 = make_short_gop(3, 0, 3);
    let out3 = trim.push(join(&gop3), base + Duration::from_millis(360));

    let emitted = match out3 {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected early release at first RAP after 300 ms"),
    };

    assert!(!trim.is_holding());
    // Emits starting at the oldest candidate (GOP 0)
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

    // Burst: 20 GOPs pushed in 1s wall time (ending at 950 ms)
    for (i, gop) in gops.iter().enumerate() {
        let now = base + Duration::from_millis(50 * i as u64);
        let output = trim.push(join(gop), now);
        assert_eq!(output, TrimOutput::Hold);
    }

    // Real time phase: pushes at 1050, 1150, 1250 ms hold (< 400 ms since burst ended)
    for step in 1..=3 {
        let wall_ms = 950 + step * 100;
        let pcr = start_pcr.wrapping_add((20 * 1000 + step * 100) * (PCR_HZ / 1000));
        let packet = pes_with_pcr(VIDEO_PID, false, pcr, &[&h264(1)]);
        let out = trim.push(Bytes::from(packet), base + Duration::from_millis(wall_ms));
        assert_eq!(out, TrimOutput::Hold, "holding in real time at {wall_ms} ms with wrapped PCR");
    }

    // Push at 1350 ms (first push at least 400 ms after burst ended at 950 ms)
    let pcr1350 = start_pcr.wrapping_add((20 * 1000 + 400) * (PCR_HZ / 1000));
    let pkt1350 = pes_with_pcr(VIDEO_PID, false, pcr1350, &[&h264(1)]);
    let output = trim.push(Bytes::from(pkt1350), base + Duration::from_millis(1350));

    let emitted = match output {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit after settle with PCR wrap"),
    };

    // Expected start is GOP 17
    let gop17 = make_gop(17, start_pcr);
    assert_eq!(&emitted[..TS_PACKET_SIZE], &gop17[0][..]);
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &gop17[1][..]);
    assert_eq!(&emitted[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE], &gop17[2][..]);
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

    // 1. 5 s media in 80 ms (GOPs 0..4, 20 ms apart)
    for i in 0..5 {
        let gop = make_gop(i, 0);
        let now = base + Duration::from_millis(20 * i);
        let out = trim.push(join(&gop), now);
        assert_eq!(out, TrimOutput::Hold, "holding GOP {i} in initial burst");
    }

    // 2. 300 ms pause inside the burst (< SETTLE_WINDOW 400 ms): must not emit
    let lone_chunk = join(&[pes_with_pcr(VIDEO_PID, false, 5 * FRAMES_PER_GOP * FRAME_TICKS, &[&h264(1)])]);
    let lone_now = base + Duration::from_millis(80 + 300); // 380 ms
    let lone_out = trim.push(lone_chunk, lone_now);
    assert_eq!(lone_out, TrimOutput::Hold, "must not settle and emit on lone chunk after 300 ms pause");

    // 3. Rest of a 20 s burst (15 s of media in 450 ms, chunks of 2 GOPs)
    let burst_gops: Vec<Vec<Vec<u8>>> = (5..20).map(|i| make_gop(i, 0)).collect();
    for (step, pair) in burst_gops.chunks(2).enumerate() {
        let chunk_bytes = join(&pair.concat());
        let now = base + Duration::from_millis(400 + step as u64 * 30);
        let out = trim.push(chunk_bytes, now);
        assert_eq!(out, TrimOutput::Hold, "holding burst chunks at step {step}");
    }
    let burst_end_ms = 400 + (burst_gops.len() / 2) as u64 * 30; // 610 ms

    // 4. Real time phase: pushes at +100, +200, +300 ms hold (< 400 ms since burst ended)
    for step in 1..=3 {
        let wall_ms = burst_end_ms + step * 100;
        let pcr = (20 * 1000 + step * 100) * (PCR_HZ / 1000);
        let packet = pes_with_pcr(VIDEO_PID, false, pcr, &[&h264(1)]);
        let out = trim.push(Bytes::from(packet), base + Duration::from_millis(wall_ms));
        assert_eq!(out, TrimOutput::Hold, "holding in real time at {wall_ms} ms");
    }

    // First push at least 400 ms after burst ended: at burst_end_ms + 400 ms
    let rt_now = base + Duration::from_millis(burst_end_ms + 400);
    let pcr_rt = (20 * 1000 + 400) * (PCR_HZ / 1000);
    let rt_pkt = pes_with_pcr(VIDEO_PID, false, pcr_rt, &[&h264(1)]);
    let rt_out = trim.push(Bytes::from(rt_pkt), rt_now);

    let emitted = match rt_out {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit after real-time phase"),
    };

    assert!(!trim.is_holding());

    // Newest media time is ~20.4 s, target 3 s -> threshold 17.4 s -> starts at GOP 17
    let gop17 = make_gop(17, 0);
    let gop17_bytes = join(&gop17);
    assert_eq!(&emitted[..gop17_bytes.len()], &gop17_bytes[..], "emitted stream must start with exact bytes of GOP 17");
}

#[test]
fn test_burst_pause_longer_than_window_settles_by_design() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 10_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();

    // 1. Initial burst: 5 s media in 200 ms (GOPs 0..4, 50 ms apart)
    for i in 0..5 {
        let gop = make_gop(i, 0);
        let now = base + Duration::from_millis(50 * i);
        let out = trim.push(join(&gop), now);
        assert_eq!(out, TrimOutput::Hold, "holding GOP {i} in initial burst");
    }

    // 2. 1.1 s pause: longer than SETTLE_WINDOW (400 ms).
    // When the next chunk with >= 300 ms media arrives at 200 + 1100 = 1300 ms,
    // the settle tracker sees wall span >= 400 ms, media advance >= 300 ms, and lead growth <= 100 ms.
    // By design, a pause longer than the settle window is indistinguishable from the end of the burst,
    // so it settles and emits on that chunk.
    let gop5 = make_gop(5, 0);
    let out = trim.push(join(&gop5), base + Duration::from_millis(1300));

    let emitted = match out {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit by design after pause exceeding settle window"),
    };

    assert!(!trim.is_holding());
    // At GOP 5 (media time ~5.96 s), target 3 s -> threshold 2.96 s -> starts at GOP 3
    let gop3 = make_gop(3, 0);
    assert_eq!(&emitted[..TS_PACKET_SIZE], &gop3[0][..]);
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &gop3[1][..]);
    assert_eq!(&emitted[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE], &gop3[2][..]);
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

    // Push at t = 200 ms (< SETTLE_WINDOW 400ms): still holds
    let out1 = trim.push(join(std::slice::from_ref(&p1)), base + Duration::from_millis(200));
    assert_eq!(out1, TrimOutput::Hold);

    // Push at t = 450 ms (>= SETTLE_WINDOW 400ms): emits immediately, not at max_hold (5s)
    let out2 = trim.push(join(std::slice::from_ref(&p2)), base + Duration::from_millis(450));
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

fn make_short_gop(gop_idx: u64, pcr_start_ticks: u64, frames: u64) -> Vec<Vec<u8>> {
    let mut gop = Vec::with_capacity(2 + frames as usize);
    gop.push(pat((gop_idx % 32) as u8));
    gop.push(video_pmt(0));

    let gop_base_pcr = pcr_start_ticks.wrapping_add(gop_idx * frames * FRAME_TICKS);
    // Keyframe IDR on frame 0
    gop.push(pes_with_pcr(VIDEO_PID, true, gop_base_pcr, &[&h264(5)]));

    for frame in 1..frames {
        let frame_pcr = gop_base_pcr.wrapping_add(frame * FRAME_TICKS);
        gop.push(pes_with_pcr(VIDEO_PID, false, frame_pcr, &[&h264(1)]));
    }
    gop
}

#[test]
fn test_no_burst_first_rap_at_700ms_emits_on_that_packet() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 1_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);
    let base = Instant::now();

    // Stream starts with PCR packets without any keyframe (non-IDR slices) in real time
    for i in 0..7 {
        let pcr_ticks = i * 100 * (PCR_HZ / 1000);
        let non_rap_packet = pes_with_pcr(VIDEO_PID, false, pcr_ticks, &[&h264(1)]);
        let now = base + Duration::from_millis(100 * i);
        let out = trim.push(Bytes::from(non_rap_packet), now);
        assert_eq!(out, TrimOutput::Hold, "holding non-RAP stream at {i}00 ms");
    }

    // At 700 ms, the first RAP arrives
    let rap_gop = make_short_gop(0, 700 * (PCR_HZ / 1000), 2);
    let rap_now = base + Duration::from_millis(700);
    let out = trim.push(join(&rap_gop), rap_now);

    let emitted = match out {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit on packet completing first RAP at 700 ms"),
    };

    assert!(!trim.is_holding());
    assert_eq!(&emitted[..TS_PACKET_SIZE], &rap_gop[0][..]);
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &rap_gop[1][..]);
    assert_eq!(&emitted[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE], &rap_gop[2][..]);
}

#[test]
fn test_lead_above_150ms_once_no_early_release_settles_by_window() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 1_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);
    let base = Instant::now();

    // Initial push with 6 frames (5 * 40 ms = 200 ms media) at t = 0.
    // Lead is 200 ms > NO_BURST_MAX_LEAD (150 ms) -> disqualifies early release!
    let gop0 = make_short_gop(0, 0, 6);
    let out0 = trim.push(join(&gop0), base);
    assert_eq!(out0, TrimOutput::Hold);

    // Stream continues in real time: pushes every 100 ms with 100 ms of media
    // Push at 100 ms (2.5 frames = 100 ms media)
    let pcr1 = 200 * (PCR_HZ / 1000);
    let p1 = pes_with_pcr(VIDEO_PID, false, pcr1 + 100 * (PCR_HZ / 1000), &[&h264(1)]);
    let out1 = trim.push(Bytes::from(p1), base + Duration::from_millis(100));
    assert_eq!(out1, TrimOutput::Hold);

    // Push at 200 ms
    let pcr2 = 300 * (PCR_HZ / 1000);
    let p2 = pes_with_pcr(VIDEO_PID, false, pcr2 + 100 * (PCR_HZ / 1000), &[&h264(1)]);
    let out2 = trim.push(Bytes::from(p2), base + Duration::from_millis(200));
    assert_eq!(out2, TrimOutput::Hold);

    // Push at 300 ms
    let pcr3 = 400 * (PCR_HZ / 1000);
    let p3 = pes_with_pcr(VIDEO_PID, false, pcr3 + 100 * (PCR_HZ / 1000), &[&h264(1)]);
    let out3 = trim.push(Bytes::from(p3), base + Duration::from_millis(300));
    assert_eq!(out3, TrimOutput::Hold);

    // Push at 350 ms: wall elapsed is 350 ms >= 300 ms, candidate 0 exists.
    // BUT lead was > 150 ms at t = 0, so early release must NOT trigger!
    let pcr350 = 500 * (PCR_HZ / 1000);
    let p350 = pes_with_pcr(VIDEO_PID, false, pcr350 + 50 * (PCR_HZ / 1000), &[&h264(1)]);
    let out350 = trim.push(Bytes::from(p350), base + Duration::from_millis(350));
    assert_eq!(out350, TrimOutput::Hold, "must not release early because lead exceeded 150 ms");

    // Push at 450 ms: 450 ms >= SETTLE_WINDOW (400 ms).
    // Sample at 0 ms had lead 200 ms. At 450 ms, media advance is ~450 ms >= 300 ms,
    // and lead growth is ~0 ms < 100 ms -> settles by window rule!
    let pcr450 = 550 * (PCR_HZ / 1000);
    let p450 = pes_with_pcr(VIDEO_PID, false, pcr450 + 100 * (PCR_HZ / 1000), &[&h264(1)]);
    let out450 = trim.push(Bytes::from(p450), base + Duration::from_millis(450));

    let emitted = match out450 {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit after settle window rule at 450 ms"),
    };

    assert!(!trim.is_holding());
    assert_eq!(&emitted[..TS_PACKET_SIZE], &gop0[0][..]);
}

#[test]
fn scrambled_stream_without_keyframes_starts_after_first_rap_wait() {
    let mut trim = TsLiveEdgeTrim::new(Duration::from_secs(2), Duration::from_secs(5), 1_000_000);
    let base = Instant::now();
    // PAT, PMT and PCR in real time, but no detectable keyframe (scrambled payload).
    let chunk = |sec: u64| {
        join(&[
            pcr_packet(VIDEO_PID, sec * PCR_HZ),
            pat(sec as u8),
            video_pmt(0),
            ts(VIDEO_PID, false, None, &[0x55; 100]),
        ])
    };
    // The stream runs in real time, so the burst-over rule alone would release at 1 s.
    assert_eq!(trim.push(chunk(0), base), TrimOutput::Hold);
    assert_eq!(trim.push(chunk(1), base + Duration::from_secs(1)), TrimOutput::Hold, "waits for a keyframe");
    // At 2 s (the keyframe wait) it starts at the newest PAT at least 2 s behind the newest PCR: the PAT of chunk 0.
    let TrimOutput::Emit(emitted) = trim.push(chunk(2), base + Duration::from_secs(2)) else {
        panic!("expected emit after the keyframe wait");
    };
    assert_eq!(&emitted[..TS_PACKET_SIZE], &pat(0)[..], "starts at a PAT");
}

#[test]
fn test_switch_with_player_buffer_waits_for_fresh_keyframe_after_settle() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 10_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();
    trim.set_player_buffer(Duration::from_secs(4), base);

    // 2-second GOPs (50 frames at 40 ms each = 2000 ms).
    // Burst: push GOP 0..2 (3 full GOPs = 6 s media) and GOP 3's first 35 frames (media 7.4 s) over 150 ms.
    for i in 0..3 {
        let gop = make_gop_with_frames(i, 0, 50);
        let out = trim.push(join(&gop), base + Duration::from_millis(50 * i));
        assert_eq!(out, TrimOutput::Hold, "holding during burst at GOP {i}");
    }

    let gop3 = make_gop_with_frames(3, 0, 50);
    // GOP 3 packets: 0: PAT, 1: PMT, 2: IDR (media 6.0 s), 3..37: frames 1..35 (PCR up to 6.0s + 35 * 40ms = 7.4s)
    let gop3_burst_packets = &gop3[..38]; // 2 PSI + 1 IDR + 35 frames = 38 packets (media 7.4 s)
    let out_burst = trim.push(join(gop3_burst_packets), base + Duration::from_millis(150));
    assert_eq!(out_burst, TrimOutput::Hold, "holding at GOP 3 partial burst");

    // Real-time delivery starts after burst ended at 150 ms (media at 7.4 s).
    // Pushes in 100 ms steps (100 ms media per 100 ms wall) to feed the settle tracker.
    // Pushes at 250, 350, 450 ms:
    for step in 1..=3 {
        let wall_ms = 150 + step * 100;
        let pcr_ms = 7400 + step * 100;
        let pkt = pes_with_pcr(VIDEO_PID, false, pcr_ms * (PCR_HZ / 1000), &[&h264(1)]);
        let out = trim.push(Bytes::from(pkt), base + Duration::from_millis(wall_ms));
        assert_eq!(out, TrimOutput::Hold, "holding in real time at {wall_ms} ms");
    }

    // At 550 ms (400 ms since burst ended at 150 ms):
    // Media advanced by 400 ms (from 7.4 s to 7.8 s) >= SETTLE_MIN_MEDIA_ADVANCE (300 ms).
    // Lead growth is 0 <= 100 ms -> stream is settled!
    // But chosen RAP is GOP 3 (media 6.0 s). Span = 7.8 s - 6.0 s = 1.8 s.
    // eff = target (3s) - remaining_player_buffer (4s - 550ms = 3.45s) = 0.
    // span (1.8s) > eff (0) + FRESH_RAP_SLACK (0.5s), and remaining (3.45s) > 0.5s.
    // Must NOT emit at older RAP after settle!
    let pcr550 = (7400 + 400) * (PCR_HZ / 1000);
    let pkt550 = pes_with_pcr(VIDEO_PID, false, pcr550, &[&h264(1)]);
    let out_settle = trim.push(Bytes::from(pkt550), base + Duration::from_millis(550));
    assert_eq!(out_settle, TrimOutput::Hold, "must hold at settle because RAP is not fresh");

    // Real time continues: push at 650 ms (media 7.9 s):
    let pcr650 = (7400 + 500) * (PCR_HZ / 1000);
    let pkt650 = pes_with_pcr(VIDEO_PID, false, pcr650, &[&h264(1)]);
    let out650 = trim.push(Bytes::from(pkt650), base + Duration::from_millis(650));
    assert_eq!(out650, TrimOutput::Hold, "holding at 650 ms");

    // Next at 750 ms (PCR 8.0 s) is the start of GOP 4!
    // GOP 4 arrives: PAT, PMT, IDR keyframe at 8.0 s!
    let gop4 = make_gop_with_frames(4, 0, 50);
    let gop4_start = join(&gop4[..3]); // PAT, PMT, IDR
    let now_gop4 = base + Duration::from_millis(750);
    let out_gop4 = trim.push(gop4_start, now_gop4);

    let emitted = match out_gop4 {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit when fresh RAP arrives at 750 ms"),
    };

    assert!(!trim.is_holding());
    // Span at this instant: newest media time is 8.0 s, chosen RAP is GOP 4 (8.0 s) -> span = 0 <= slack!
    // Emits starting at exact start GOP: GOP 4!
    assert_eq!(&emitted[..TS_PACKET_SIZE], &gop4[0][..], "starts at GOP 4 PAT");
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &gop4[1][..], "second packet is PMT");
    assert_eq!(&emitted[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE], &gop4[2][..], "third packet is keyframe");
}

#[test]
fn test_switch_with_player_buffer_emits_when_buffer_drains_below_slack() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 10_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();
    trim.set_player_buffer(Duration::from_secs(4), base);

    // Same burst setup as above: GOP 0..2 full, GOP 3 up to 7.4 s over 150 ms.
    for i in 0..3 {
        let gop = make_gop_with_frames(i, 0, 50);
        let out = trim.push(join(&gop), base + Duration::from_millis(50 * i));
        assert_eq!(out, TrimOutput::Hold);
    }

    let gop3 = make_gop_with_frames(3, 0, 50);
    let gop3_burst_packets = &gop3[..38];
    assert_eq!(trim.push(join(gop3_burst_packets), base + Duration::from_millis(150)), TrimOutput::Hold);

    // Stream settles at 550 ms (400 ms window), holds because GOP 3 (at 6.0 s) is not fresh:
    for step in 1..=4 {
        let wall_ms = 150 + step * 100;
        let pcr_ms = 7400 + step * 100;
        let pkt = pes_with_pcr(VIDEO_PID, false, pcr_ms * (PCR_HZ / 1000), &[&h264(1)]);
        let out = trim.push(Bytes::from(pkt), base + Duration::from_millis(wall_ms));
        assert_eq!(out, TrimOutput::Hold, "holding during settle check at {wall_ms} ms");
    }

    // Now, instead of a fresh RAP arriving, non-keyframe packets continue in real time.
    // Keyframe does not arrive (e.g. 5 s GOP interval).
    // Provider streams non-keyframe packets at 100 ms intervals (100 ms media per 100 ms wall).
    // At wall time 2950 ms: remaining player buffer = 4000 - 2950 = 1050 ms > PLAYER_BUFFER_FLOOR (1000 ms).
    // eff = 3000 - 1050 = 1950 ms. eff + slack = 2450 ms.
    // Media time: 7.8 s + (2950 - 550) ms = 10.2 s. Span = 10.2 s - 6.0 s = 4.2 s > 2.45 s.
    // Still holds at 2950 ms!
    let mut wall_ms = 550;
    let mut media_ms = 7800;
    while wall_ms + 100 <= 2950 {
        wall_ms += 100;
        media_ms += 100;
        let pcr = media_ms * (PCR_HZ / 1000);
        let pkt = pes_with_pcr(VIDEO_PID, false, pcr, &[&h264(1)]);
        let out = trim.push(Bytes::from(pkt), base + Duration::from_millis(wall_ms));
        assert_eq!(out, TrimOutput::Hold, "holding in real time at {wall_ms} ms");
    }

    // Next push at 3050 ms: remaining_player_buffer = 4000 ms - 3050 ms = 950 ms <= PLAYER_BUFFER_FLOOR (1000 ms).
    // Waiting is no longer free: it must emit at this exact instant with the best available RAP (GOP 3).
    let pcr3050 = (media_ms + 100) * (PCR_HZ / 1000);
    let pkt3050 = pes_with_pcr(VIDEO_PID, false, pcr3050, &[&h264(1)]);
    let out3050 = trim.push(Bytes::from(pkt3050), base + Duration::from_millis(3050));

    let emitted = match out3050 {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit when player buffer drains to the floor at 3050 ms"),
    };

    assert!(!trim.is_holding());
    // Emitted stream starts at the best available RAP (GOP 3):
    assert_eq!(&emitted[..TS_PACKET_SIZE], &gop3[0][..], "starts at GOP 3 PAT");
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &gop3[1][..], "second packet is PMT");
    assert_eq!(&emitted[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE], &gop3[2][..], "third packet is keyframe");
}

#[test]
fn burst_then_20ms_realtime_chunks_settles_before_max_hold() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 10_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);
    let base = Instant::now();

    let gops: Vec<Vec<Vec<u8>>> = (0..20).map(|i| make_gop(i, 0)).collect();

    for (i, gop) in gops.iter().enumerate() {
        let now = base + Duration::from_millis(50 * i as u64);
        let output = trim.push(join(gop), now);
        assert_eq!(output, TrimOutput::Hold, "holding during burst at GOP {i}");
    }

    let mut emitted_bytes = None;
    for step in 1..=30 {
        let wall_ms = 950 + step * 20;
        let pcr = (20 * 1000 + step * 20) * (PCR_HZ / 1000);
        let packet = pes_with_pcr(VIDEO_PID, false, pcr, &[&h264(1)]);
        let out = trim.push(Bytes::from(packet), base + Duration::from_millis(wall_ms));
        if wall_ms < 1350 {
            assert_eq!(out, TrimOutput::Hold, "holding in real time at {wall_ms} ms");
        } else if let TrimOutput::Emit(bytes) = out {
            emitted_bytes = Some(bytes);
            break;
        }
    }

    let emitted = emitted_bytes.expect("expected emit after settle in real time with 20ms chunks before max_hold");
    assert!(!trim.is_holding());
    let gop17 = make_gop(17, 0);
    assert_eq!(&emitted[..TS_PACKET_SIZE], &gop17[0][..]);
    assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &gop17[1][..]);
    assert_eq!(&emitted[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE], &gop17[2][..]);
}

#[test]
fn burst_before_discontinuity_in_same_chunk_disqualifies_no_burst() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 1_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);
    let base = Instant::now();

    let pcr_300ms = 300 * (PCR_HZ / 1000);
    let pcr_jump = (5 * 1000 + 300) * (PCR_HZ / 1000);
    let chunk0 = join(&[
        pat(0),
        video_pmt(0),
        pes_with_pcr(VIDEO_PID, true, 0, &[&h264(5)]),
        pes_with_pcr(VIDEO_PID, false, pcr_300ms, &[&h264(1)]),
        pes_with_pcr(VIDEO_PID, false, pcr_jump, &[&h264(1)]),
    ]);
    let out0 = trim.push(chunk0, base);
    assert_eq!(out0, TrimOutput::Hold);

    let gop1 = make_short_gop(1, pcr_jump, 3);
    let out1 = trim.push(join(&gop1), base + Duration::from_millis(120));
    assert_eq!(out1, TrimOutput::Hold);

    let gop2 = make_short_gop(2, pcr_jump, 3);
    let out2 = trim.push(join(&gop2), base + Duration::from_millis(240));
    assert_eq!(out2, TrimOutput::Hold);

    let gop3 = make_short_gop(3, pcr_jump, 3);
    let out3 = trim.push(join(&gop3), base + Duration::from_millis(360));
    assert_eq!(out3, TrimOutput::Hold, "no-burst early release must NOT happen after burst in initial chunk");

    let gop4 = make_short_gop(4, pcr_jump, 3);
    let out4 = trim.push(join(&gop4), base + Duration::from_millis(480));
    assert!(matches!(out4, TrimOutput::Emit(_)), "settles through regular settle window");
    assert!(!trim.is_holding());
}

#[test]
fn skip_candidate_whose_pat_was_dropped() {
    let target = Duration::from_secs(1);
    let max_hold = Duration::from_secs(10);
    // 6 packets = 1128 bytes cap
    let max_bytes = 6 * TS_PACKET_SIZE;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);
    let base = Instant::now();

    // Chunk 0: 4 packets (0..752 bytes). Opens PES at PAT 0 (pos 0).
    let sei = [vec![0x06], vec![0x05; 150]].concat();
    let pes_open = pes(VIDEO_PID, false, false, &[&h264(9), &sei]);
    let chunk0 = join(&[pat(0), video_pmt(0), pcr_packet(VIDEO_PID, 0), pes_open]);
    assert_eq!(trim.push(chunk0, base), TrimOutput::Hold);

    // Chunk 1: 3 packets (752..1316 bytes). Exceeds 1128 byte cap; force_drop_on_cap drops
    // to offset 752 (pcr_sample at packet 4), dropping PAT 0.
    let chunk1 = join(&[
        pcr_packet(VIDEO_PID, 50 * (PCR_HZ / 1000)),
        ts(AUDIO_PID, false, None, &[1; 20]),
        ts(AUDIO_PID, false, None, &[1; 20]),
    ]);
    assert_eq!(trim.push(chunk1, base + Duration::from_millis(10)), TrimOutput::Hold);

    // Chunk 2: 1 packet (1316..1504 bytes). Completes the open PES whose PAT was at pos 0 (< dropped_bytes 752).
    let idr_cont = ts(VIDEO_PID, false, None, &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84]);
    assert_eq!(trim.push(Bytes::from(idr_cont), base + Duration::from_millis(20)), TrimOutput::Hold);

    // Chunk 3: 3 packets with a valid second keyframe at PAT 1 (pos 1504 >= dropped_bytes).
    let chunk3 = join(&[pat(1), video_pmt(1), pes_with_pcr(VIDEO_PID, true, 100 * (PCR_HZ / 1000), &[&h264(5)])]);
    assert_eq!(trim.push(chunk3, base + Duration::from_millis(30)), TrimOutput::Hold);

    let emitted = trim.finish().expect("held bytes");
    assert_eq!(pid_of(&emitted[..TS_PACKET_SIZE]), 0, "must start at PAT packet, not dropped non-PAT at offset 0");
    assert_eq!(&emitted[..TS_PACKET_SIZE], &pat(1)[..], "must start at PAT 1");
}

#[test]
fn candidate_media_time_retained_across_many_pats() {
    let target = Duration::from_secs(5);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 10_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);
    let base = Instant::now();

    let sei = [vec![0x06], vec![0x05; 150]].concat();
    let pes_open = pes(VIDEO_PID, false, false, &[&h264(9), &sei]);
    let chunk0 = join(&[pat(0), video_pmt(0), pcr_packet(VIDEO_PID, 0), pes_open]);
    assert_eq!(trim.push(chunk0, base), TrimOutput::Hold);

    // Send 10 PATs, evicting PAT 0 from pat_history (capacity 8).
    for i in 1..=10 {
        let pcr = (i * 100) * (PCR_HZ / 1000);
        let chunk = join(&[pat(i as u8), pcr_packet(VIDEO_PID, pcr)]);
        assert_eq!(trim.push(chunk, base + Duration::from_millis(i * 10)), TrimOutput::Hold);
    }

    // Complete the open PES which started at PAT 0.
    let idr_cont = ts(VIDEO_PID, false, None, &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84]);
    assert_eq!(trim.push(Bytes::from(idr_cont), base + Duration::from_millis(110)), TrimOutput::Hold);

    // Verify candidate media time was retained from PAT 0 (0s), not substituted with current clock (1s).
    let candidates = trim.candidate_media_times();
    assert_eq!(candidates.len(), 1, "exactly one candidate from completed open PES");
    assert_eq!(candidates[0].0, 0, "candidate offset points to PAT 0");
    assert_eq!(
        candidates[0].1,
        Duration::ZERO,
        "candidate media time must be retained opening PAT media time (0s), not clock time at completion (1s)"
    );

    let emitted = trim.finish().expect("held bytes");
    assert_eq!(&emitted[..TS_PACKET_SIZE], &pat(0)[..], "must retain GOP 0 PAT");
}

#[test]
fn switch_hold_wakes_and_releases_at_player_buffer_floor() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 10_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();
    trim.set_player_buffer(Duration::from_secs(4), base);

    // Burst: push GOP 0..2 (3 full GOPs = 6 s media) and GOP 3's first 35 frames (media 7.4 s) over 150 ms.
    for i in 0..3 {
        let gop = make_gop_with_frames(i, 0, 50);
        let out = trim.push(join(&gop), base + Duration::from_millis(50 * i));
        assert_eq!(out, TrimOutput::Hold, "holding during burst at GOP {i}");
    }

    let gop3 = make_gop_with_frames(3, 0, 50);
    let gop3_burst_packets = &gop3[..38];
    let out_burst = trim.push(join(gop3_burst_packets), base + Duration::from_millis(150));
    assert_eq!(out_burst, TrimOutput::Hold, "holding at GOP 3 partial burst");

    // Real-time delivery every 100 ms after the burst ended at 150 ms. The hold settles, but the only RAP old
    // enough (GOP 3 at 6 s) is not fresh, so it holds until the player buffer drains to the floor.
    for wall_ms in (250..=2900).step_by(100) {
        let pcr_ms = 7400 + (wall_ms - 150);
        let pkt = pes_with_pcr(VIDEO_PID, false, pcr_ms * (PCR_HZ / 1000), &[&h264(1)]);
        let out = trim.push(Bytes::from(pkt), base + Duration::from_millis(wall_ms));
        assert_eq!(out, TrimOutput::Hold, "holding in real time at {wall_ms} ms");
    }

    // The upstream pauses. Floor instant: sampled_at (base) + buffer (4 s) - PLAYER_BUFFER_FLOOR (1 s).
    let floor_instant = base + Duration::from_millis(3000);
    let hold_deadline = base + max_hold;

    let before_floor = base + Duration::from_millis(2950);
    assert_eq!(trim.next_wake(before_floor), Some(floor_instant));
    assert_eq!(trim.push(Bytes::new(), before_floor), TrimOutput::Hold);
    assert_eq!(trim.next_wake(base + Duration::from_millis(3500)), Some(hold_deadline));

    // At the floor instant, empty push emits!
    let out_floor = trim.push(Bytes::new(), floor_instant);
    let emitted = match out_floor {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit at floor instant on empty push"),
    };
    assert!(!trim.is_holding());
    assert_eq!(&emitted[..TS_PACKET_SIZE], &gop3[0][..], "starts at GOP 3 PAT");
    assert_eq!(trim.next_wake(floor_instant), None, "no wake when not holding");
}

#[test]
fn first_rap_wait_wake_releases_settled_hold_without_keyframe() {
    let mut trim = TsLiveEdgeTrim::new(Duration::from_secs(2), Duration::from_secs(5), 1_000_000);
    let base = Instant::now();
    // PAT, PMT and PCR in real time, but no detectable keyframe (scrambled payload).
    let chunk = |media_ms: u64, version: u8| {
        join(&[
            pcr_packet(VIDEO_PID, media_ms * (PCR_HZ / 1000)),
            pat(version),
            video_pmt(0),
            ts(VIDEO_PID, false, None, &[0x55; 100]),
        ])
    };
    let at = |ms: u64| base + Duration::from_millis(ms);

    // Burst of 2 s media within 100 ms, then real time; PCR steps stay below 1 s (no discontinuity).
    for (wall_ms, media_ms, version) in [(0, 0, 0), (50, 1000, 1), (100, 2000, 2), (1400, 2900, 3), (1700, 3200, 4)] {
        assert_eq!(trim.push(chunk(media_ms, version), at(wall_ms)), TrimOutput::Hold, "holding at {wall_ms} ms");
    }
    let expected_wake = at(2000);
    assert_eq!(trim.next_wake(at(1700)), Some(expected_wake));

    // The upstream pauses; at the wait instant the settled hold without keyframe releases at a PAT.
    let t2 = expected_wake;
    let TrimOutput::Emit(emitted) = trim.push(Bytes::new(), t2) else {
        panic!("expected emit at FIRST_RAP_WAIT instant");
    };
    assert_eq!(&emitted[..TS_PACKET_SIZE], &pat(1)[..], "starts at the PAT at least the target behind");
    assert!(!trim.is_holding());
    assert_eq!(trim.next_wake(t2), None);
}

#[test]
fn no_pcr_hold_wakes_and_releases_at_settle_window() {
    let mut trim = TsLiveEdgeTrim::new(Duration::from_secs(3), Duration::from_secs(5), 1_000_000);
    let base = Instant::now();
    let packet = ts(0x200, false, None, &[0xAA; 100]);

    assert_eq!(trim.push(join(std::slice::from_ref(&packet)), base), TrimOutput::Hold);
    let settle = base + Duration::from_millis(400);
    assert_eq!(trim.next_wake(base + Duration::from_millis(100)), Some(settle));

    // The upstream stays silent; the wake at the settle window releases everything held.
    let TrimOutput::Emit(emitted) = trim.push(Bytes::new(), settle) else {
        panic!("expected emit at the settle window without PCR");
    };
    assert_eq!(emitted.as_ref(), packet.as_slice());
    assert_eq!(trim.next_wake(settle), None);
}

#[test]
fn next_wake_no_burst_realtime_releases_at_first_candidate_after_300ms() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 1_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);
    let base = Instant::now();

    let gop0 = join(&[pat(0), video_pmt(0), pes_with_pcr(VIDEO_PID, true, 0, &[&h264(5)])]);
    assert_eq!(trim.push(gop0.clone(), base), TrimOutput::Hold);

    let pcr40 = 40 * (PCR_HZ / 1000);
    let pkt40 = pes_with_pcr(VIDEO_PID, false, pcr40, &[&h264(1)]);
    let at_40 = base + Duration::from_millis(40);
    assert_eq!(trim.push(Bytes::from(pkt40), at_40), TrimOutput::Hold);

    let pcr80 = 80 * (PCR_HZ / 1000);
    let pkt80 = pes_with_pcr(VIDEO_PID, false, pcr80, &[&h264(1)]);
    let at_80 = base + Duration::from_millis(80);
    assert_eq!(trim.push(Bytes::from(pkt80), at_80), TrimOutput::Hold);

    let expected_wake = base + Duration::from_millis(300);
    assert_eq!(trim.next_wake(at_80), Some(expected_wake));

    let TrimOutput::Emit(emitted) = trim.push(Bytes::new(), expected_wake) else {
        panic!("expected emit at no-burst wake instant");
    };
    assert!(!trim.is_holding());
    assert_eq!(&emitted[..TS_PACKET_SIZE], &pat(0)[..], "emits at first candidate RAP");
}

#[test]
fn next_wake_settle_by_aging_and_growth_exact_instant() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 1_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);
    let base = Instant::now();

    let pcr200 = 200 * (PCR_HZ / 1000);
    let gop = join(&[pat(0), video_pmt(0), pes_with_pcr(VIDEO_PID, true, pcr200, &[&h264(5)])]);
    assert_eq!(trim.push(gop, base), TrimOutput::Hold);

    let at_100 = base + Duration::from_millis(100);
    let pcr700 = 700 * (PCR_HZ / 1000);
    let pkt700 = pcr_packet(VIDEO_PID, pcr700);
    assert_eq!(trim.push(Bytes::from(pkt700), at_100), TrimOutput::Hold);

    let expected_wake = base + Duration::from_micros(400_001);
    assert_eq!(trim.next_wake(at_100), Some(expected_wake));

    let before_wake = base + Duration::from_millis(400);
    assert_eq!(trim.push(Bytes::new(), before_wake), TrimOutput::Hold);

    let output = trim.push(Bytes::new(), expected_wake);
    assert!(matches!(output, TrimOutput::Emit(_)), "emits at exact wake instant");
    assert!(!trim.is_holding());
}

#[test]
fn next_wake_negative_idle_stream_low_media_advance_returns_hold_deadline() {
    let max_hold = Duration::from_secs(5);
    let mut trim = TsLiveEdgeTrim::new(Duration::from_secs(3), max_hold, 1_000_000);
    let base = Instant::now();

    let first_packet = pcr_packet(VIDEO_PID, 0);
    assert_eq!(trim.push(Bytes::from(first_packet), base), TrimOutput::Hold);

    let t_mid = base + Duration::from_millis(50);
    let mid_packet = pcr_packet(VIDEO_PID, 100 * (PCR_HZ / 1000));
    assert_eq!(trim.push(Bytes::from(mid_packet), t_mid), TrimOutput::Hold);

    let t_last = base + Duration::from_millis(100);
    let last_packet = pcr_packet(VIDEO_PID, 200 * (PCR_HZ / 1000));
    assert_eq!(trim.push(Bytes::from(last_packet), t_last), TrimOutput::Hold);

    let hold_deadline = base + max_hold;
    let query_time = base + Duration::from_millis(150);
    assert_eq!(trim.next_wake(query_time), Some(hold_deadline));
}

#[test]
fn switch_hold_wakes_and_releases_at_fresh_rap_instant() {
    let target = Duration::from_secs(4);
    let max_hold = Duration::from_secs(10);
    let max_bytes = 10_000_000;
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, max_bytes);

    let base = Instant::now();
    trim.set_player_buffer(Duration::from_secs(4), base);

    // 2-second GOPs (50 frames at 40 ms each = 2000 ms).
    // Burst: push GOP 0..2 (media 0..6000 ms) over 100 ms.
    for i in 0..3 {
        let gop = make_gop_with_frames(i, 0, 50);
        let out = trim.push(join(&gop), base + Duration::from_millis(50 * i));
        assert_eq!(out, TrimOutput::Hold, "holding during burst at GOP {i}");
    }

    // GOP 3 starts at 6000 ms (PAT, PMT, IDR keyframe).
    // Burst continues with GOP 3 start and frames up to 6660 ms at wall 150 ms.
    let gop3 = make_gop_with_frames(3, 0, 50);
    let mut gop3_burst = gop3[..3].to_vec();
    gop3_burst.push(pes_with_pcr(VIDEO_PID, false, 6660 * (PCR_HZ / 1000), &[&h264(1)]));
    let out_burst = trim.push(join(&gop3_burst), base + Duration::from_millis(150));
    assert_eq!(out_burst, TrimOutput::Hold, "holding at GOP 3 partial burst");

    // Real-time delivery starts after burst ended at 150 ms (media at 6.66 s).
    // Feeds settle tracker in 100 ms steps (PCR = wall_ms + 6510 ms).
    for wall_ms in (250..=950).step_by(100) {
        let pcr_ms = wall_ms + 6510;
        let pkt = pes_with_pcr(VIDEO_PID, false, pcr_ms * (PCR_HZ / 1000), &[&h264(1)]);
        let out = trim.push(Bytes::from(pkt), base + Duration::from_millis(wall_ms));
        assert_eq!(out, TrimOutput::Hold, "holding in real time at {wall_ms} ms");
    }

    // Data stops at 950 ms with PCR = 7460 ms (exactly 50 ms before computed fresh instant 1000 ms).
    // GOP 3 PAT was recorded at media time 5960 ms (last PCR before IDR), so c = 5960 ms.
    // Span = 7460 ms - 5960 ms = 1500 ms.
    // Needed eff = span - FRESH_RAP_SLACK = 1500 ms - 500 ms = 1000 ms.
    // Needed remaining = target (4000 ms) - 1000 ms = 3000 ms.
    // Fresh instant = base + buffer (4000 ms) - 3000 ms = base + 1000 ms.
    // Floor instant = base + 4000 ms - 1000 ms = base + 3000 ms.
    let fresh_instant = base + Duration::from_millis(1000);
    let floor_instant = base + Duration::from_millis(3000);
    assert!(fresh_instant < floor_instant);

    let query_now = base + Duration::from_millis(950);
    assert_eq!(trim.next_wake(query_now), Some(fresh_instant));

    // 1 ms before fresh instant: still holds.
    let before_fresh = base + Duration::from_millis(999);
    assert_eq!(trim.push(Bytes::new(), before_fresh), TrimOutput::Hold);

    // At fresh instant: emits starting at GOP 3 PAT!
    let out_fresh = trim.push(Bytes::new(), fresh_instant);
    let emitted = match out_fresh {
        TrimOutput::Emit(bytes) => bytes,
        TrimOutput::Hold => panic!("expected emit at fresh instant on empty push"),
    };
    assert!(!trim.is_holding());
    assert_eq!(&emitted[..TS_PACKET_SIZE], &gop3[0][..], "emits at GOP 3 PAT");
    assert_eq!(trim.next_wake(fresh_instant), None, "no wake when not holding");
}

#[test]
fn next_wake_negative_no_player_buffer_has_no_fresh_term() {
    let target = Duration::from_secs(3);
    let max_hold = Duration::from_secs(10);
    let mut trim = TsLiveEdgeTrim::new(target, max_hold, 10_000_000);
    let base = Instant::now();

    for i in 0..3 {
        let gop = make_gop_with_frames(i, 0, 50);
        let out = trim.push(join(&gop), base + Duration::from_millis(50 * i));
        assert_eq!(out, TrimOutput::Hold);
    }

    // After no-burst probe window (300 ms) and without real-time progress to settle,
    // next_wake without a player buffer returns the hold deadline (no floor or fresh terms).
    let query_time = base + Duration::from_millis(350);
    let hold_deadline = base + max_hold;
    assert_eq!(trim.next_wake(query_time), Some(hold_deadline));
}
