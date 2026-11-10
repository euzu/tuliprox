#![allow(clippy::cast_possible_truncation, reason = "test builders write PCR fields into single bytes")]

use super::RealtimeMeter;
use crate::packet::TS_PACKET_SIZE;
use std::time::{Duration, Instant};

const PCR_PID: u16 = 0x100;
const STEP: Duration = Duration::from_millis(100);

/// One packet on `pid` with a PCR of `pcr_27mhz` in its adaptation field.
fn pcr_packet(pid: u16, pcr_27mhz: u64) -> Vec<u8> {
    let base = pcr_27mhz / 300;
    let ext = pcr_27mhz % 300;
    let mut packet = vec![0x47, (pid >> 8) as u8 & 0x1F, pid as u8, 0x20, 183, 0x10];
    packet.extend([
        (base >> 25) as u8,
        (base >> 17) as u8,
        (base >> 9) as u8,
        (base >> 1) as u8,
        (((base & 1) << 7) as u8) | 0x7E | ((ext >> 8) as u8),
        ext as u8,
    ]);
    packet.resize(TS_PACKET_SIZE, 0xFF);
    packet
}

fn plain_packet(pid: u16) -> Vec<u8> {
    let mut packet = vec![0x47, (pid >> 8) as u8 & 0x1F, pid as u8, 0x10];
    packet.resize(TS_PACKET_SIZE, 0);
    packet
}

/// Feeds `steps` steps of 100 ms wall time, each advancing the PCR by `media` and reporting
/// `waited`; returns the first step that reported an underrun.
fn run(
    meter: &mut RealtimeMeter,
    start: Instant,
    pcr: &mut u64,
    steps: u32,
    media: Duration,
    waited: Duration,
) -> Option<u32> {
    let mut detected = None;
    for step in 0..steps {
        *pcr += u64::try_from(media.as_micros()).expect("micros") * 27;
        let now = start + STEP * step;
        if meter.observe(&pcr_packet(PCR_PID, *pcr), waited, now) && detected.is_none() {
            detected = Some(step);
        }
    }
    detected
}

#[test]
fn realtime_meter_detects_slow_provider() {
    let mut meter = RealtimeMeter::new(90, Duration::from_secs(15));
    let start = Instant::now();
    let mut pcr = 27_000_000;
    let detected = run(&mut meter, start, &mut pcr, 200, Duration::from_millis(70), STEP).expect("underrun detected");
    assert!((150..=170).contains(&detected), "detected at step {detected}");
}

#[test]
fn realtime_meter_ignores_slow_consumer() {
    let mut meter = RealtimeMeter::new(90, Duration::from_secs(15));
    let mut pcr = 27_000_000;
    assert_eq!(run(&mut meter, Instant::now(), &mut pcr, 300, STEP, Duration::from_millis(5)), None);
}

#[test]
fn realtime_meter_inactive_without_pcr() {
    let mut meter = RealtimeMeter::new(90, Duration::from_secs(2));
    let start = Instant::now();
    for step in 0..100 {
        assert!(!meter.observe(&plain_packet(PCR_PID), STEP, start + STEP * step));
    }
}

#[test]
fn realtime_meter_resets_on_pcr_jump() {
    let mut meter = RealtimeMeter::new(90, Duration::from_secs(15));
    let start = Instant::now();
    let mut pcr = 27_000_000;
    assert_eq!(run(&mut meter, start, &mut pcr, 100, Duration::from_millis(70), STEP), None);
    pcr += 27_000_000 * 5;
    let resumed = start + STEP * 100;
    assert_eq!(run(&mut meter, resumed, &mut pcr, 100, Duration::from_millis(70), STEP), None);
}

#[test]
fn realtime_meter_disabled_with_zero_ratio() {
    let mut meter = RealtimeMeter::new(0, Duration::from_secs(1));
    let mut pcr = 27_000_000;
    assert_eq!(run(&mut meter, Instant::now(), &mut pcr, 100, Duration::from_millis(10), STEP), None);
}

#[test]
fn realtime_meter_follows_the_pcr_pid_of_a_new_member() {
    let mut meter = RealtimeMeter::new(90, Duration::from_secs(5));
    let start = Instant::now();
    let mut pcr = 27_000_000;
    assert_eq!(run(&mut meter, start, &mut pcr, 20, STEP, STEP), None);
    // The next member carries its PCR on another PID and plays in real time.
    meter.begin_member(start + STEP * 20);
    let mut detected = None;
    let mut member_pcr = 54_000_000_u64;
    for step in 20..120 {
        member_pcr += 27_000 * 100;
        if meter.observe(&pcr_packet(0x200, member_pcr), STEP, start + STEP * step) {
            detected = Some(step);
            break;
        }
    }
    assert_eq!(detected, None, "a real-time member is no underrun");
}
