#![allow(
    clippy::cast_possible_truncation,
    clippy::similar_names,
    reason = "test packet builder and mock chunk variables"
)]

use super::BurstBuffer;
use bytes::Bytes;
use std::time::Duration;
use tokio::time::{advance, Instant};
use tuliprox_mpegts::splice::TsRandomAccessScanner;

#[tokio::test(start_paused = true)]
async fn test_burst_zero_returns_whole_ring() {
    let mut buffer = BurstBuffer::new(1024);
    let now = Instant::now();
    buffer.push(Bytes::from_static(b"chunk0"), now, None);
    buffer.push(Bytes::from_static(b"chunk1"), now, None);
    buffer.push(Bytes::from_static(b"chunk2"), now, None);

    let (chunks, next_seq) = buffer.join_snapshot(now, Duration::from_secs(0));
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0], Bytes::from_static(b"chunk0"));
    assert_eq!(chunks[1], Bytes::from_static(b"chunk1"));
    assert_eq!(chunks[2], Bytes::from_static(b"chunk2"));
    assert_eq!(next_seq, 3);
}

#[tokio::test(start_paused = true)]
async fn test_join_burst_picks_newest_rap_older_than_target() {
    let mut buffer = BurstBuffer::new(4096);
    let mut chunk_bytes = Vec::new();

    // Push chunks at t = 0..=10 s (one per second, exactly 10 bytes each)
    // RAPs at chunks 2, 5, 8
    for i in 0..=10 {
        let chunk = Bytes::from(format!("chunk_{i:04}"));
        assert_eq!(chunk.len(), 10);
        chunk_bytes.push(chunk.clone());
        let now = Instant::now();
        let rap = match i {
            2 => Some(20 + 3), // chunk 2 start is 20, rap offset 3
            5 => Some(50 + 2), // chunk 5 start is 50, rap offset 2
            8 => Some(80 + 4), // chunk 8 start is 80, rap offset 4
            _ => None,
        };
        buffer.push(chunk, now, rap);
        if i < 10 {
            advance(Duration::from_secs(1)).await;
        }
    }

    // Join at t=10 with burst 3 s -> target is t=7 s.
    // Newest RAP with arrived <= 7 s is chunk 5 (arrived at t=5 s).
    let (chunks, next_seq) = buffer.join_snapshot(Instant::now(), Duration::from_secs(3));
    assert_eq!(chunks.len(), 6); // chunks 5..=10
    assert_eq!(chunks[0], chunk_bytes[5].slice(2..)); // sliced at RAP offset 2
    for (idx, expected) in chunk_bytes[6..=10].iter().enumerate() {
        assert_eq!(&chunks[idx + 1], expected);
    }
    assert_eq!(next_seq, 11);
}

#[tokio::test(start_paused = true)]
async fn test_join_burst_picks_rap_for_shorter_burst() {
    let mut buffer = BurstBuffer::new(4096);
    let mut chunk_bytes = Vec::new();

    for i in 0..=10 {
        let chunk = Bytes::from(format!("chunk_{i:04}"));
        chunk_bytes.push(chunk.clone());
        let now = Instant::now();
        let rap = match i {
            2 => Some(20 + 3),
            5 => Some(50 + 2),
            8 => Some(80 + 4),
            _ => None,
        };
        buffer.push(chunk, now, rap);
        if i < 10 {
            advance(Duration::from_secs(1)).await;
        }
    }

    // Join at t=10 with burst 1 s -> target is t=9 s.
    // Newest RAP with arrived <= 9 s is chunk 8 (arrived at t=8 s <= 9 s).
    let (chunks, next_seq) = buffer.join_snapshot(Instant::now(), Duration::from_secs(1));
    assert_eq!(chunks.len(), 3); // chunks 8..=10
    assert_eq!(chunks[0], chunk_bytes[8].slice(4..)); // sliced at RAP offset 4
    assert_eq!(chunks[1], chunk_bytes[9]);
    assert_eq!(chunks[2], chunk_bytes[10]);
    assert_eq!(next_seq, 11);
}

#[tokio::test(start_paused = true)]
async fn test_raps_newer_than_target_uses_oldest_rap() {
    let mut buffer = BurstBuffer::new(4096);
    let mut chunk_bytes = Vec::new();

    for i in 0..=10 {
        let chunk = Bytes::from(format!("chunk_{i:04}"));
        chunk_bytes.push(chunk.clone());
        let now = Instant::now();
        let rap = if i == 9 { Some(90 + 1) } else { None };
        buffer.push(chunk, now, rap);
        if i < 10 {
            advance(Duration::from_secs(1)).await;
        }
    }

    // Join at t=10 with burst 3 s -> target is t=7 s.
    // Only RAP is at chunk 9 (t=9 s > 7 s), so oldest RAP (chunk 9) is used.
    let (chunks, next_seq) = buffer.join_snapshot(Instant::now(), Duration::from_secs(3));
    assert_eq!(chunks.len(), 2); // chunks 9..=10
    assert_eq!(chunks[0], chunk_bytes[9].slice(1..));
    assert_eq!(chunks[1], chunk_bytes[10]);
    assert_eq!(next_seq, 11);
}

#[tokio::test(start_paused = true)]
async fn test_no_raps_fallback_to_time_and_oldest_chunk() {
    let mut buffer = BurstBuffer::new(4096);
    let mut chunk_bytes = Vec::new();

    for i in 0..=10 {
        let chunk = Bytes::from(format!("chunk_{i:04}"));
        chunk_bytes.push(chunk.clone());
        let now = Instant::now();
        buffer.push(chunk, now, None);
        if i < 10 {
            advance(Duration::from_secs(1)).await;
        }
    }

    // Part A: Target is t=7 s. Newest chunk with arrived <= 7 s is chunk 7 (arrived at t=7 s).
    let (chunks, next_seq) = buffer.join_snapshot(Instant::now(), Duration::from_secs(3));
    assert_eq!(chunks.len(), 4); // chunks 7..=10
    assert_eq!(chunks[0], chunk_bytes[7]); // whole chunk, not sliced
    assert_eq!(chunks[1], chunk_bytes[8]);
    assert_eq!(chunks[2], chunk_bytes[9]);
    assert_eq!(chunks[3], chunk_bytes[10]);
    assert_eq!(next_seq, 11);

    // Part B: Ring where no chunk is old enough -> oldest chunk (whole ring).
    let mut short_buffer = BurstBuffer::new(4096);
    let now = Instant::now();
    short_buffer.push(Bytes::from_static(b"a"), now, None);
    advance(Duration::from_secs(1)).await;
    let now = Instant::now();
    short_buffer.push(Bytes::from_static(b"b"), now, None);

    // Join with burst 10 s -> target underflows or is older than all chunks.
    let (chunks, next_seq) = short_buffer.join_snapshot(Instant::now(), Duration::from_secs(10));
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0], Bytes::from_static(b"a"));
    assert_eq!(chunks[1], Bytes::from_static(b"b"));
    assert_eq!(next_seq, 2);
}

#[tokio::test(start_paused = true)]
async fn test_checked_sub_underflow_and_empty_ring() {
    let buffer = BurstBuffer::new(1024);
    let (chunks, next_seq) = buffer.join_snapshot(Instant::now(), Duration::from_secs(3));
    assert_eq!(chunks.len(), 0);
    assert_eq!(next_seq, 0);

    let mut buffer = BurstBuffer::new(1024);
    advance(Duration::from_secs(1)).await;
    buffer.push(Bytes::from_static(b"first"), Instant::now(), None);

    // Target = 1 s - 10 s underflows -> checked_sub returns None -> whole ring
    let (chunks, next_seq) = buffer.join_snapshot(Instant::now(), Duration::from_secs(10));
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0], Bytes::from_static(b"first"));
    assert_eq!(next_seq, 1);
}

#[tokio::test(start_paused = true)]
async fn test_eviction_prunes_raps_and_ignores_evicted_rap() {
    // Buffer fits 3 chunks (188 * 3 bytes)
    let mut buffer = BurstBuffer::new(188 * 3);

    // Push chunk 0 with RAP at pos 2
    buffer.push(Bytes::from_static(b"0123456789"), Instant::now(), Some(2));
    assert_eq!(buffer.raps.len(), 1);
    advance(Duration::from_secs(1)).await;

    // Push chunk 1
    buffer.push(Bytes::from_static(b"abcdefghij"), Instant::now(), None);
    assert_eq!(buffer.raps.len(), 1);
    advance(Duration::from_secs(1)).await;

    // Push chunk 2 (3rd chunk, buffer now holds chunks 0, 1, 2)
    buffer.push(Bytes::from_static(b"klmnopqrst"), Instant::now(), None);
    assert_eq!(buffer.raps.len(), 1);
    assert_eq!(buffer.buffer.len(), 3);
    advance(Duration::from_secs(1)).await;

    // Push chunk 3 -> evicts chunk 0! RAP at pos 2 must be pruned.
    buffer.push(Bytes::from_static(b"uvwxyz0123"), Instant::now(), None);
    assert_eq!(buffer.buffer.len(), 3);
    assert_eq!(buffer.raps.len(), 0);

    // Join burst must not reference the evicted RAP; falls back to time-based
    let (chunks, next_seq) = buffer.join_snapshot(Instant::now(), Duration::from_secs(2));
    assert_eq!(chunks.len(), 3); // chunks 1, 2, 3
    assert_eq!(chunks[0], Bytes::from_static(b"abcdefghij"));
    assert_eq!(next_seq, 4);

    // A RAP pointing into an already-evicted chunk (pos 2) must be ignored
    buffer.push(Bytes::from_static(b"newest____"), Instant::now(), Some(2));
    assert_eq!(buffer.raps.len(), 0);
}

#[tokio::test(start_paused = true)]
async fn test_rap_in_earlier_chunk_stored_with_correct_sequence_and_offset() {
    let mut buffer = BurstBuffer::new(1024);

    // Chunk 0: pos 0..100
    let chunk0 = Bytes::from(vec![0xAA_u8; 100]);
    buffer.push(chunk0.clone(), Instant::now(), None);
    advance(Duration::from_secs(1)).await;

    // Chunk 1: pos 100..200; scanner completes RAP in chunk 0 at pos 42
    let chunk1 = Bytes::from(vec![0xBB_u8; 100]);
    buffer.push(chunk1.clone(), Instant::now(), Some(42));

    assert_eq!(buffer.raps.len(), 1);
    assert_eq!(buffer.raps[0].sequence, 0);
    assert_eq!(buffer.raps[0].offset, 42);

    let (chunks, next_seq) = buffer.join_snapshot(Instant::now(), Duration::from_secs(1));
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0], chunk0.slice(42..));
    assert_eq!(chunks[1], chunk1);
    assert_eq!(next_seq, 2);
}

#[tokio::test(start_paused = true)]
async fn test_two_raps_for_same_chunk_updates_to_larger_offset() {
    let mut buffer = BurstBuffer::new(1024);

    let chunk0 = Bytes::from(vec![0xAA_u8; 100]);
    buffer.push(chunk0.clone(), Instant::now(), Some(20));
    advance(Duration::from_secs(1)).await;

    let chunk1 = Bytes::from(vec![0xBB_u8; 100]);
    buffer.push(chunk1.clone(), Instant::now(), Some(60));

    assert_eq!(buffer.raps.len(), 1);
    assert_eq!(buffer.raps[0].sequence, 0);
    assert_eq!(buffer.raps[0].offset, 60);

    let (chunks, next_seq) = buffer.join_snapshot(Instant::now(), Duration::from_secs(2));
    assert_eq!(next_seq, 2);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0], chunk0.slice(60..));
    assert_eq!(chunks[1], chunk1);
}

fn ts_packet(pid: u16, pusi: bool, payload: &[u8]) -> [u8; 188] {
    let mut pkt = [0xFF; 188];
    pkt[0] = 0x47;
    pkt[1] = (if pusi { 0x40 } else { 0x00 }) | ((pid >> 8) as u8 & 0x1F);
    pkt[2] = (pid & 0xFF) as u8;
    pkt[3] = 0x10;
    let copy_len = payload.len().min(184);
    pkt[4..4 + copy_len].copy_from_slice(&payload[..copy_len]);
    pkt
}

fn finish_section(mut section: Vec<u8>) -> Vec<u8> {
    let length = section.len() - 3 + 4;
    section[1] = 0xB0 | ((length >> 8) as u8 & 0x0F);
    section[2] = (length & 0xFF) as u8;
    let crc = tuliprox_mpegts::packet::mpeg2_crc32(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

fn pat_packet() -> [u8; 188] {
    let pmt_pid: u16 = 0x1000;
    let section = vec![
        0x00,
        0,
        0,
        0x00,
        0x01,
        0xC1,
        0x00,
        0x00,
        0x00,
        0x01,
        0xE0 | ((pmt_pid >> 8) as u8 & 0x1F),
        (pmt_pid & 0xFF) as u8,
    ];
    let finished = finish_section(section);
    let mut payload = vec![0x00];
    payload.extend_from_slice(&finished);
    ts_packet(0, true, &payload)
}

fn pmt_packet() -> [u8; 188] {
    let section = vec![0x02, 0, 0, 0x00, 0x01, 0xC1, 0x00, 0x00, 0xE1, 0x00, 0xF0, 0x00, 0x1B, 0xE1, 0x00, 0xF0, 0x00];
    let finished = finish_section(section);
    let mut payload = vec![0x00];
    payload.extend_from_slice(&finished);
    ts_packet(0x1000, true, &payload)
}

fn idr_packet() -> [u8; 188] {
    let payload =
        [0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00];
    ts_packet(0x100, true, &payload)
}

#[tokio::test(start_paused = true)]
async fn test_real_ts_through_scanner_join_starts_at_pat_sync_byte() {
    let mut scanner = TsRandomAccessScanner::new();
    let mut buffer = BurstBuffer::new(64 * 1024);

    // Build GOP 0 (prefix padding, then PAT, PMT, IDR)
    let mut gop0 = Vec::new();
    let prefix_padding = [0x55_u8; 64];
    gop0.extend_from_slice(&prefix_padding);
    gop0.extend_from_slice(&pat_packet());
    gop0.extend_from_slice(&pmt_packet());
    gop0.extend_from_slice(&idr_packet());
    let gop0_bytes = Bytes::from(gop0);

    let rap0 = scanner.observe(&gop0_bytes);
    assert_eq!(rap0, Some(64)); // PAT sync byte starts at byte offset 64
    buffer.push(gop0_bytes, Instant::now(), rap0);

    advance(Duration::from_secs(5)).await;

    // Build GOP 1 at t=5 s (PAT, PMT, IDR)
    let mut gop1 = Vec::new();
    gop1.extend_from_slice(&pat_packet());
    gop1.extend_from_slice(&pmt_packet());
    gop1.extend_from_slice(&idr_packet());
    let gop1_bytes = Bytes::from(gop1);

    let rap1 = scanner.observe(&gop1_bytes);
    assert_eq!(rap1, Some(64 + 188 * 3)); // GOP 1 PAT is at 64 + 3 * 188
    buffer.push(gop1_bytes, Instant::now(), rap1);

    advance(Duration::from_secs(1)).await;

    // Join at t=6 s with burst 2 s -> target is t=4 s.
    // Newest RAP with arrived <= 4 s is GOP 0.
    let (chunks, next_seq) = buffer.join_snapshot(Instant::now(), Duration::from_secs(2));
    assert_eq!(next_seq, 2);
    assert_eq!(chunks.len(), 2);
    // The first chunk must be sliced at offset 64, which is the PAT sync byte 0x47!
    assert_eq!(chunks[0][0], 0x47);
    assert_eq!(chunks[0].len(), 188 * 3);

    // Join at t=6 s with burst 1 s -> target is t=5 s.
    // Newest RAP with arrived <= 5 s is GOP 1.
    let (chunks, next_seq) = buffer.join_snapshot(Instant::now(), Duration::from_secs(1));
    assert_eq!(next_seq, 2);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0][0], 0x47);
    assert_eq!(chunks[0].len(), 188 * 3);
}

#[tokio::test(start_paused = true)]
async fn test_stale_rap_older_than_max_lookback_falls_back_to_time_start() {
    let mut buffer = BurstBuffer::new(4096);
    let t0 = Instant::now();
    // RAP at t=0
    buffer.push(Bytes::from_static(b"rap_chunk0"), t0, Some(0));

    // Advance 20 seconds
    advance(Duration::from_secs(20)).await;
    let t20 = Instant::now();
    buffer.push(Bytes::from_static(b"time_chunk"), t20, None);

    // Advance 5 seconds to t=25
    advance(Duration::from_secs(5)).await;
    // Join at t=25 with burst 5s -> target is t=20.
    // RAP is at t=0, which is 20s before target (target - arrived = 20s > MAX_RAP_LOOKBACK 10s).
    // Stale RAP -> fallback to time-only start: newest chunk with arrived <= target (t=20), which is time_chunk.
    let (chunks, next_seq) = buffer.join_snapshot(Instant::now(), Duration::from_secs(5));
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0], Bytes::from_static(b"time_chunk"));
    assert_eq!(next_seq, 2);
}

#[tokio::test(start_paused = true)]
async fn test_rap_within_max_lookback_is_used() {
    let mut buffer = BurstBuffer::new(4096);
    let t0 = Instant::now();
    buffer.push(Bytes::from_static(b"chunk_old0"), t0, None);

    // Advance 15 seconds: RAP at t=15
    advance(Duration::from_secs(15)).await;
    let t15 = Instant::now();
    buffer.push(Bytes::from_static(b"rap_chunk_"), t15, Some(10 + 2));

    // Advance 5 seconds: chunk at t=20
    advance(Duration::from_secs(5)).await;
    let t20 = Instant::now();
    buffer.push(Bytes::from_static(b"chunk_latr"), t20, None);

    // Advance 5 seconds to t=25
    advance(Duration::from_secs(5)).await;
    // Join at t=25 with burst 5s -> target is t=20.
    // RAP is at t=15, which is 5s before target (target - arrived = 5s <= MAX_RAP_LOOKBACK 10s).
    // RAP is accepted and sliced at offset 2.
    let (chunks, next_seq) = buffer.join_snapshot(Instant::now(), Duration::from_secs(5));
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0], Bytes::from_static(b"p_chunk_"));
    assert_eq!(chunks[1], Bytes::from_static(b"chunk_latr"));
    assert_eq!(next_seq, 3);
}

#[tokio::test(start_paused = true)]
async fn test_missing_rap_chunk_falls_back_to_time_path() {
    let mut buffer = BurstBuffer::new(1024);
    let now = Instant::now();
    buffer.push(Bytes::from_static(b"chunk0"), now, None);
    // Artificially insert a RAP referencing non-existent sequence 999
    buffer.raps.push_back(super::RandomAccessPoint { sequence: 999, offset: 0, arrived: now });
    let (chunks, next_seq) = buffer.join_snapshot(now, Duration::from_secs(1));
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0], Bytes::from_static(b"chunk0"));
    assert_eq!(next_seq, 1);
}
