use crate::api::model::StreamError;
use bytes::{Bytes, BytesMut};
use futures::Stream;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tuliprox_mpegts::{
    packet::{TS_PACKET_SIZE, TS_SYNC_BYTE},
    splice::{TrimOutput, TsLiveEdgeTrim},
};

/// Maximum wall-clock time to hold stream startup packets waiting for the burst to settle.
pub const DEFAULT_MAX_HOLD: Duration = Duration::from_secs(5);

/// Maximum buffer memory to hold during startup burst before emitting.
pub const DEFAULT_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Maximum bytes to probe for an MPEG-TS sync byte before treating the input stream as non-TS.
pub const SYNC_PROBE_BYTES: usize = 8 * TS_PACKET_SIZE;

/// Stream adapter that holds a live MPEG-TS stream until the provider's startup burst settles,
/// then emits from a random access point at most `target` media seconds behind the newest PCR.
///
/// After the first emit, it passes inner stream chunks through directly without buffering.
pub struct LiveEdgeTrimStream<S> {
    inner: S,
    state: State,
}

enum State {
    Holding(Box<HoldingState>),
    Drain { chunk: Bytes, then: After },
    PassThrough,
    Ended,
    Error(StreamError),
}

enum After {
    PassThrough,
    End,
    Error(StreamError),
}

struct HoldingState {
    trim: TsLiveEdgeTrim,
    carry: BytesMut,
    hold_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    wake_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    max_hold: Duration,
    synced: bool,
}

impl HoldingState {
    fn new(target: Duration, max_hold: Duration, max_bytes: usize) -> Self {
        Self {
            trim: TsLiveEdgeTrim::new(target, max_hold, max_bytes),
            carry: BytesMut::with_capacity(SYNC_PROBE_BYTES),
            hold_sleep: None,
            wake_sleep: None,
            max_hold,
            synced: false,
        }
    }

    /// Arms the wake sleep to the next instant at which the trimmer's release decision can change without data.
    fn arm_wake_sleep(&mut self, now: std::time::Instant) {
        let Some(wake) = self.trim.next_wake(now).filter(|wake| *wake > now) else {
            self.wake_sleep = None;
            return;
        };
        let wake = tokio::time::Instant::from_std(wake);
        match &mut self.wake_sleep {
            Some(sleep) => sleep.as_mut().reset(wake),
            None => self.wake_sleep = Some(Box::pin(tokio::time::sleep_until(wake))),
        }
    }
}

fn confirmed_ts_sync_offset(buf: &[u8]) -> Option<usize> {
    let limit = buf.len().min(SYNC_PROBE_BYTES);
    let max_offset = limit.saturating_sub(TS_PACKET_SIZE);
    (0..max_offset).find(|&offset| {
        buf.get(offset) == Some(&TS_SYNC_BYTE) && buf.get(offset + TS_PACKET_SIZE) == Some(&TS_SYNC_BYTE)
    })
}

fn release(holding: &mut HoldingState, emitted: Option<Bytes>) -> Option<Bytes> {
    let trim_out = match emitted {
        Some(bytes) => Some(bytes),
        None => holding.trim.finish(),
    };
    let carry = if holding.carry.is_empty() { None } else { Some(holding.carry.split().freeze()) };
    match (trim_out, carry) {
        (Some(trim_out), Some(carry)) => {
            if trim_out.is_empty() {
                Some(carry)
            } else if carry.is_empty() {
                Some(trim_out)
            } else {
                let mut buf = BytesMut::with_capacity(trim_out.len().saturating_add(carry.len()));
                buf.extend_from_slice(&trim_out);
                buf.extend_from_slice(&carry);
                Some(buf.freeze())
            }
        }
        (Some(trim_out), None) => (!trim_out.is_empty()).then_some(trim_out),
        (None, Some(carry)) => (!carry.is_empty()).then_some(carry),
        (None, None) => None,
    }
}

impl<S> LiveEdgeTrimStream<S> {
    /// Creates a new live edge trim stream with default hold duration (5 s) and memory cap (32 MiB).
    pub fn new(inner: S, target: Duration) -> Self {
        Self::with_limits(inner, target, DEFAULT_MAX_HOLD, DEFAULT_MAX_BYTES)
    }

    /// Creates a new live edge trim stream with custom hold duration and memory limits.
    pub fn with_limits(inner: S, target: Duration, max_hold: Duration, max_bytes: usize) -> Self {
        Self { inner, state: State::Holding(Box::new(HoldingState::new(target, max_hold, max_bytes))) }
    }

    /// Returns `true` if the stream is currently holding packets waiting for the startup burst to settle.
    #[must_use]
    #[cfg(test)]
    pub fn is_holding(&self) -> bool { matches!(self.state, State::Holding(_)) }

    fn finish_hold(&mut self, emitted: Option<Bytes>, then: After) {
        let State::Holding(mut holding) = std::mem::replace(&mut self.state, State::Ended) else {
            return;
        };
        if let Some(chunk) = release(&mut holding, emitted) {
            self.state = State::Drain { chunk, then };
        } else {
            self.state = match then {
                After::PassThrough => State::PassThrough,
                After::End => State::Ended,
                After::Error(err) => State::Error(err),
            };
        }
    }
}

impl<S> Stream for LiveEdgeTrimStream<S>
where
    S: Stream<Item = Result<Bytes, StreamError>> + Unpin + Send,
{
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            match &mut this.state {
                State::PassThrough => return Pin::new(&mut this.inner).poll_next(cx),
                State::Ended => return Poll::Ready(None),
                State::Error(_) => {
                    let State::Error(err) = std::mem::replace(&mut this.state, State::Ended) else {
                        return Poll::Ready(None);
                    };
                    return Poll::Ready(Some(Err(err)));
                }
                State::Drain { .. } => {
                    let State::Drain { chunk, then } = std::mem::replace(&mut this.state, State::Ended) else {
                        return Poll::Ready(None);
                    };
                    this.state = match then {
                        After::PassThrough => State::PassThrough,
                        After::End => State::Ended,
                        After::Error(err) => State::Error(err),
                    };
                    return Poll::Ready(Some(Ok(chunk)));
                }
                State::Holding(holding) => {
                    let now = tokio::time::Instant::now().into_std();

                    if let Some(sleep) = &mut holding.hold_sleep {
                        if sleep.as_mut().poll(cx).is_ready() {
                            this.finish_hold(None, After::PassThrough);
                            continue;
                        }
                    }

                    // A release condition that changes with wall time alone (player-buffer floor, keyframe
                    // wait) is evaluated when its instant comes, also while the upstream stays silent.
                    if let Some(sleep) = &mut holding.wake_sleep {
                        if sleep.as_mut().poll(cx).is_ready() {
                            holding.wake_sleep = None;
                            if let TrimOutput::Emit(bytes) = holding.trim.push(Bytes::new(), now) {
                                this.finish_hold(Some(bytes), After::PassThrough);
                            } else {
                                holding.arm_wake_sleep(now);
                            }
                            continue;
                        }
                    }

                    match Pin::new(&mut this.inner).poll_next(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(None) => {
                            this.finish_hold(None, After::End);
                        }
                        Poll::Ready(Some(Err(err))) => {
                            this.finish_hold(None, After::Error(err));
                        }
                        Poll::Ready(Some(Ok(chunk))) => {
                            if chunk.is_empty() {
                                continue;
                            }

                            if holding.hold_sleep.is_none() {
                                let mut sleep = Box::pin(tokio::time::sleep(holding.max_hold));
                                let _ = sleep.as_mut().poll(cx);
                                holding.hold_sleep = Some(sleep);
                            }

                            holding.carry.extend_from_slice(&chunk);
                            if !holding.synced {
                                match confirmed_ts_sync_offset(&holding.carry) {
                                    Some(offset) => {
                                        holding.synced = true;
                                        let _ = holding.carry.split_to(offset);
                                    }
                                    None if holding.carry.len() >= SYNC_PROBE_BYTES => {
                                        let non_ts = holding.carry.split().freeze();
                                        this.finish_hold(Some(non_ts), After::PassThrough);
                                        continue;
                                    }
                                    None => continue,
                                }
                            }

                            let whole = (holding.carry.len() / TS_PACKET_SIZE) * TS_PACKET_SIZE;
                            if whole == 0 {
                                continue;
                            }
                            let packets = holding.carry.split_to(whole).freeze();

                            if let TrimOutput::Emit(emitted) = holding.trim.push(packets, now) {
                                this.finish_hold(Some(emitted), After::PassThrough);
                            } else {
                                holding.arm_wake_sleep(now);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::uninlined_format_args)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn mpeg2_crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFF_u32;
        for &byte in data {
            crc ^= u32::from(byte) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 == 0 { crc << 1 } else { (crc << 1) ^ 0x04C1_1DB7 };
            }
        }
        crc
    }

    fn finish_section(mut section: Vec<u8>) -> Vec<u8> {
        let length = section.len() - 3 + 4;
        section[1] = 0xB0 | ((length >> 8) as u8 & 0x0F);
        section[2] = length as u8;
        let crc = mpeg2_crc32(&section);
        section.extend_from_slice(&crc.to_be_bytes());
        section
    }

    fn ts_packet(pid: u16, pusi: bool, af_flags: Option<u8>, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0x47, (if pusi { 0x40 } else { 0 }) | ((pid >> 8) as u8 & 0x1F), pid as u8];
        if let Some(flags) = af_flags {
            let af_len = 184 - 1 - payload.len();
            packet.push(0x30);
            packet.push(af_len as u8);
            packet.push(flags);
            packet.extend(std::iter::repeat_n(0xFF, af_len - 1));
            packet.extend_from_slice(payload);
        } else {
            packet.push(0x10);
            packet.extend_from_slice(payload);
            packet.resize(TS_PACKET_SIZE, 0xFF);
        }
        packet
    }

    fn pat_packet(version: u8) -> Vec<u8> {
        let pmt_pid: u16 = 0x1000;
        let program: u16 = 1;
        let section = vec![
            0x00,
            0,
            0,
            0x00,
            0x01,
            0xC1 | (version << 1),
            0x00,
            0x00,
            (program >> 8) as u8,
            program as u8,
            0xE0 | (pmt_pid >> 8) as u8,
            pmt_pid as u8,
        ];
        let finished = finish_section(section);
        let mut payload = vec![0x00];
        payload.extend_from_slice(&finished);
        ts_packet(0, true, None, &payload)
    }

    fn pmt_packet(version: u8) -> Vec<u8> {
        let pmt_pid: u16 = 0x1000;
        let pcr_pid: u16 = 0x100;
        let program: u16 = 1;
        let mut section = vec![
            0x02,
            0,
            0,
            (program >> 8) as u8,
            program as u8,
            0xC1 | (version << 1),
            0x00,
            0x00,
            0xE0 | (pcr_pid >> 8) as u8,
            pcr_pid as u8,
            0xF0,
            0x00,
        ];
        section.extend([0x1B, 0xE0 | (pcr_pid >> 8) as u8, pcr_pid as u8, 0xF0, 0x00]);
        let finished = finish_section(section);
        let mut payload = vec![0x00];
        payload.extend_from_slice(&finished);
        ts_packet(pmt_pid, true, None, &payload)
    }

    fn pes_with_pcr(pid: u16, rai: bool, pcr_27mhz: u64, is_idr: bool) -> Vec<u8> {
        let mut payload = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x80, 0x05, 0x21, 0x00, 0x01, 0x00, 0x01];
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        let nal_byte = if is_idr { 0x65 } else { 0x61 };
        payload.push(nal_byte);
        payload.extend_from_slice(&[0x88, 0x84]);

        let mut packet = vec![0x47, 0x40 | ((pid >> 8) as u8 & 0x1F), pid as u8, 0x30];
        let af_len = 184 - 1 - payload.len();
        packet.push(af_len as u8);
        let flags = if rai { 0x50 } else { 0x10 };
        packet.push(flags);
        let base = pcr_27mhz / 300;
        let ext = pcr_27mhz % 300;
        packet.extend([
            (base >> 25) as u8,
            (base >> 17) as u8,
            (base >> 9) as u8,
            (base >> 1) as u8,
            (((base & 1) << 7) as u8) | 0x7E | ((ext >> 8) as u8),
            ext as u8,
        ]);
        packet.extend(std::iter::repeat_n(0xFF, af_len - 7));
        packet.extend_from_slice(&payload);
        packet
    }

    fn make_gop(gop_idx: u64, pcr_start_ticks: u64) -> Vec<u8> {
        const FRAME_TICKS: u64 = 1_080_000;
        const FRAMES_PER_GOP: u64 = 25;
        let mut gop = Vec::with_capacity(27 * TS_PACKET_SIZE);
        gop.extend_from_slice(&pat_packet((gop_idx % 32) as u8));
        gop.extend_from_slice(&pmt_packet(0));
        let gop_base_pcr = pcr_start_ticks.wrapping_add(gop_idx * FRAMES_PER_GOP * FRAME_TICKS);
        gop.extend_from_slice(&pes_with_pcr(0x100, true, gop_base_pcr, true));
        for frame in 1..FRAMES_PER_GOP {
            let frame_pcr = gop_base_pcr.wrapping_add(frame * FRAME_TICKS);
            gop.extend_from_slice(&pes_with_pcr(0x100, false, frame_pcr, false));
        }
        gop
    }

    #[tokio::test(start_paused = true)]
    async fn test_burst_then_realtime_starts_at_expected_pat() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let target = Duration::from_secs(3);
        let mut trim_stream = LiveEdgeTrimStream::new(rx, target);

        let start_time = tokio::time::Instant::now();

        for i in 0..20 {
            let gop = make_gop(i, 0);
            tx.unbounded_send(Ok(Bytes::from(gop))).unwrap();
            assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));
            tokio::time::advance(Duration::from_millis(50)).await;
        }

        tokio::time::advance(Duration::from_millis(1050)).await;
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));

        let gop20 = make_gop(20, 0);
        tx.unbounded_send(Ok(Bytes::from(gop20))).unwrap();

        let Poll::Ready(Some(Ok(emitted))) = futures::poll!(trim_stream.next()) else {
            panic!("expected emit on realtime GOP 20");
        };

        let elapsed = start_time.elapsed();
        assert_eq!(elapsed, Duration::from_millis(2050));
        assert!(elapsed < DEFAULT_MAX_HOLD);
        assert!(!trim_stream.is_holding());
        assert_eq!(emitted.len(), 3 * 27 * TS_PACKET_SIZE, "exact emitted length matches GOPs 18..20");

        let gop18 = make_gop(18, 0);
        assert_eq!(&emitted[..TS_PACKET_SIZE], &gop18[..TS_PACKET_SIZE]);
        assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &gop18[TS_PACKET_SIZE..2 * TS_PACKET_SIZE]);
        assert_eq!(&emitted[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE], &gop18[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE]);
    }

    #[tokio::test(start_paused = true)]
    async fn test_unaligned_chunk_boundaries() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let target = Duration::from_secs(3);
        let mut trim_stream = LiveEdgeTrimStream::new(rx, target);

        let start_time = tokio::time::Instant::now();

        let mut all_bytes = Vec::new();
        for i in 0..21 {
            all_bytes.extend_from_slice(&make_gop(i, 0));
        }

        let burst_len = 20 * 27 * TS_PACKET_SIZE;
        let burst_bytes = &all_bytes[..burst_len];
        let mut burst_chunks = 0;
        for chunk in burst_bytes.chunks(200) {
            tx.unbounded_send(Ok(Bytes::copy_from_slice(chunk))).unwrap();
            assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));
            tokio::time::advance(Duration::from_millis(2)).await;
            burst_chunks += 1;
        }

        tokio::time::advance(Duration::from_millis(1050)).await;
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));

        let realtime_bytes = &all_bytes[burst_len..];
        for chunk in realtime_bytes.chunks(200) {
            tx.unbounded_send(Ok(Bytes::copy_from_slice(chunk))).unwrap();
        }

        let Poll::Ready(Some(Ok(emitted))) = futures::poll!(trim_stream.next()) else {
            panic!("expected emit on realtime GOP 20");
        };

        let elapsed = start_time.elapsed();
        let expected_burst_time = Duration::from_millis(burst_chunks * 2);
        assert_eq!(elapsed, expected_burst_time + Duration::from_millis(1050));
        assert!(elapsed < DEFAULT_MAX_HOLD);
        assert!(!trim_stream.is_holding());
        assert_eq!(
            emitted.len(),
            16228,
            "exact emitted length matches GOPs 17..19 plus initial GOP 20 packets and carry"
        );
        let gop17 = make_gop(17, 0);
        assert_eq!(&emitted[..TS_PACKET_SIZE], &gop17[..TS_PACKET_SIZE]);
    }

    #[tokio::test(start_paused = true)]
    async fn test_garbage_prefix_before_first_sync_is_dropped() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let target = Duration::from_secs(3);
        let mut trim_stream = LiveEdgeTrimStream::new(rx, target);

        let start_time = tokio::time::Instant::now();

        let mut first_chunk = vec![0xAA; 13];
        first_chunk.extend_from_slice(&make_gop(0, 0));
        tx.unbounded_send(Ok(Bytes::from(first_chunk))).unwrap();
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));
        tokio::time::advance(Duration::from_millis(50)).await;

        for i in 1..20 {
            tx.unbounded_send(Ok(Bytes::from(make_gop(i, 0)))).unwrap();
            assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));
            tokio::time::advance(Duration::from_millis(50)).await;
        }

        tokio::time::advance(Duration::from_millis(1050)).await;
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));

        tx.unbounded_send(Ok(Bytes::from(make_gop(20, 0)))).unwrap();

        let Poll::Ready(Some(Ok(emitted))) = futures::poll!(trim_stream.next()) else {
            panic!("expected emit on realtime GOP 20");
        };

        let elapsed = start_time.elapsed();
        assert_eq!(elapsed, Duration::from_millis(2050));
        assert!(elapsed < DEFAULT_MAX_HOLD);
        assert!(!trim_stream.is_holding());
        assert_eq!(emitted.len(), 3 * 27 * TS_PACKET_SIZE, "exact emitted length matches GOPs 18..20");
        let gop18 = make_gop(18, 0);
        // Garbage prefix was dropped: output starts cleanly at GOP 18 PAT!
        assert_eq!(&emitted[..TS_PACKET_SIZE], &gop18[..TS_PACKET_SIZE]);
    }

    #[tokio::test(start_paused = true)]
    async fn test_non_ts_passes_through_byte_identical() {
        let chunks = vec![
            Ok(Bytes::from_static(b"<!DOCTYPE html><html><body>Error</body></html>")),
            Ok(Bytes::from_static(b"second non-ts chunk")),
            Ok(Bytes::from_static(b"third non-ts chunk")),
        ];
        let stream = futures::stream::iter(chunks.clone());
        let mut trim_stream = LiveEdgeTrimStream::new(stream, Duration::from_secs(3));

        let mut received = Vec::new();
        while let Some(item) = trim_stream.next().await {
            received.extend_from_slice(&item.unwrap());
        }

        let expected: Vec<u8> = chunks.into_iter().flat_map(|c| c.unwrap().to_vec()).collect();
        assert_eq!(received, expected);
    }

    #[tokio::test(start_paused = true)]
    async fn test_hold_deadline_fires_while_inner_is_pending() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let target = Duration::from_secs(3);
        let mut trim_stream = LiveEdgeTrimStream::new(rx, target);

        let gop0 = make_gop(0, 0);
        tx.unbounded_send(Ok(Bytes::from(gop0.clone()))).unwrap();

        let handle = tokio::spawn(async move { trim_stream.next().await });

        tokio::time::advance(Duration::from_secs(6)).await;

        let res = handle.await.unwrap();
        let emitted = res.unwrap().unwrap();
        assert_eq!(&emitted[..TS_PACKET_SIZE], &gop0[..TS_PACKET_SIZE]);
    }

    #[tokio::test(start_paused = true)]
    async fn test_inner_end_while_holding_emits_then_ends() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let target = Duration::from_secs(3);
        let mut trim_stream = LiveEdgeTrimStream::new(rx, target);

        let gop0 = make_gop(0, 0);
        tx.unbounded_send(Ok(Bytes::from(gop0.clone()))).unwrap();
        drop(tx);

        let emitted = trim_stream.next().await.unwrap().unwrap();
        assert_eq!(&emitted[..TS_PACKET_SIZE], &gop0[..TS_PACKET_SIZE]);

        let next = trim_stream.next().await;
        assert!(next.is_none(), "stream must end after emitting held bytes");
    }

    #[tokio::test(start_paused = true)]
    async fn test_inner_error_while_holding_emits_held_bytes_then_the_error() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let target = Duration::from_secs(3);
        let mut trim_stream = LiveEdgeTrimStream::new(rx, target);

        let gop0 = make_gop(0, 0);
        tx.unbounded_send(Ok(Bytes::from(gop0.clone()))).unwrap();
        tx.unbounded_send(Err(StreamError::Stream("upstream error".to_string()))).unwrap();

        let emitted = trim_stream.next().await.unwrap().unwrap();
        assert_eq!(&emitted[..TS_PACKET_SIZE], &gop0[..TS_PACKET_SIZE]);

        let err = trim_stream.next().await.unwrap().unwrap_err();
        match err {
            StreamError::Stream(msg) => assert_eq!(msg, "upstream error"),
            other => panic!("expected StreamError::Stream, got {other:?}"),
        }

        assert!(trim_stream.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn test_passthrough_after_emit_returns_inner_chunks_unchanged() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let target = Duration::from_secs(3);
        let mut trim_stream = LiveEdgeTrimStream::new(rx, target);

        for i in 0..20 {
            tx.unbounded_send(Ok(Bytes::from(make_gop(i, 0)))).unwrap();
            tokio::time::advance(Duration::from_millis(50)).await;
        }
        tokio::time::advance(Duration::from_millis(1050)).await;
        tx.unbounded_send(Ok(Bytes::from(make_gop(20, 0)))).unwrap();

        let first = trim_stream.next().await.unwrap().unwrap();
        assert_eq!(first.len(), 3 * 27 * TS_PACKET_SIZE, "exact emitted length matches GOPs 18..20");
        let gop18 = make_gop(18, 0);
        assert_eq!(&first[..TS_PACKET_SIZE], &gop18[..TS_PACKET_SIZE]);

        let chunk21 = Bytes::from_static(b"arbitrary chunk after emit");
        tx.unbounded_send(Ok(chunk21.clone())).unwrap();

        let chunk22 = Bytes::from_static(b"another arbitrary chunk");
        tx.unbounded_send(Ok(chunk22.clone())).unwrap();

        let received21 = trim_stream.next().await.unwrap().unwrap();
        assert_eq!(received21, chunk21);

        let received22 = trim_stream.next().await.unwrap().unwrap();
        assert_eq!(received22, chunk22);
    }

    #[tokio::test(start_paused = true)]
    async fn test_short_first_chunk_ts_stream_trimmed_as_usual() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let target = Duration::from_secs(3);
        let mut trim_stream = LiveEdgeTrimStream::new(rx, target);

        let start_time = tokio::time::Instant::now();

        // First chunk is 100 bytes of junk before the first sync
        tx.unbounded_send(Ok(Bytes::from(vec![0xAA; 100]))).unwrap();
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));

        let gop0 = make_gop(0, 0);
        tx.unbounded_send(Ok(Bytes::copy_from_slice(&gop0))).unwrap();
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));
        tokio::time::advance(Duration::from_millis(50)).await;

        for i in 1..20 {
            tx.unbounded_send(Ok(Bytes::from(make_gop(i, 0)))).unwrap();
            assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));
            tokio::time::advance(Duration::from_millis(50)).await;
        }

        tokio::time::advance(Duration::from_millis(1050)).await;
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));

        tx.unbounded_send(Ok(Bytes::from(make_gop(20, 0)))).unwrap();

        let Poll::Ready(Some(Ok(first))) = futures::poll!(trim_stream.next()) else {
            panic!("expected emit on realtime GOP 20");
        };

        let elapsed = start_time.elapsed();
        assert_eq!(elapsed, Duration::from_millis(2050));
        assert!(elapsed < DEFAULT_MAX_HOLD);
        assert!(!trim_stream.is_holding());
        assert_eq!(first.len(), 3 * 27 * TS_PACKET_SIZE, "exact emitted length matches GOPs 18..20");
        let gop18 = make_gop(18, 0);
        assert_eq!(
            &first[..TS_PACKET_SIZE],
            &gop18[..TS_PACKET_SIZE],
            "TS stream with 100-byte first chunk must still be trimmed to target GOP 18"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_non_ts_stream_in_100_byte_chunks_passes_through_byte_identical() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let target = Duration::from_secs(3);
        let mut trim_stream = LiveEdgeTrimStream::new(rx, target);

        // Non-TS data: 2500 bytes of non-TS data (no 0x47 sync bytes anywhere)
        let mut raw = Vec::with_capacity(2500);
        for i in 0..2500 {
            let byte = ((i * 7 + 13) % 250) as u8;
            raw.push(if byte == 0x47 { 0x48 } else { byte });
        }

        for chunk in raw.chunks(100) {
            tx.unbounded_send(Ok(Bytes::copy_from_slice(chunk))).unwrap();
        }
        drop(tx);

        let mut received = Vec::new();
        while let Some(item) = trim_stream.next().await {
            received.extend_from_slice(&item.unwrap());
        }

        assert_eq!(received, raw, "non-TS stream in 100-byte chunks must pass through byte-identical");
    }

    #[tokio::test(start_paused = true)]
    async fn test_non_ts_starting_with_0x47_passes_through_without_5s_hold() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let target = Duration::from_secs(3);
        let mut trim_stream = LiveEdgeTrimStream::new(rx, target);

        // Non-TS data: starts with 0x47, but is not TS packets (byte 188 is not 0x47).
        let mut raw = vec![0x47];
        raw.extend_from_slice(b"IF89a arbitrary image or text payload that happens to start with byte G (0x47)");
        raw.resize(2500, 0x55);

        for chunk in raw.chunks(100) {
            tx.unbounded_send(Ok(Bytes::copy_from_slice(chunk))).unwrap();
        }
        drop(tx);

        let first = trim_stream.next().await.unwrap().unwrap();
        assert_eq!(first.len(), 1600, "first chunk holds probed bytes up to SYNC_PROBE_BYTES");
        let mut received = first.to_vec();
        while let Some(item) = trim_stream.next().await {
            received.extend_from_slice(&item.unwrap());
        }

        assert_eq!(received.len(), 2500, "must emit all 2500 bytes without waiting for 5s hold");
        assert_eq!(&received[..], &raw[..]);
        assert!(!trim_stream.is_holding());
    }

    #[tokio::test(start_paused = true)]
    async fn test_junk_prefix_300_bytes_then_ts_trim_applies_at_exact_start_offset() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let target = Duration::from_secs(3);
        let mut trim_stream = LiveEdgeTrimStream::new(rx, target);

        let start_time = tokio::time::Instant::now();

        // 300 bytes of non-TS garbage prefix
        let mut first_chunk = vec![0xAA; 300];
        first_chunk.extend_from_slice(&make_gop(0, 0));
        tx.unbounded_send(Ok(Bytes::from(first_chunk))).unwrap();
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));
        tokio::time::advance(Duration::from_millis(50)).await;

        for i in 1..20 {
            tx.unbounded_send(Ok(Bytes::from(make_gop(i, 0)))).unwrap();
            assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));
            tokio::time::advance(Duration::from_millis(50)).await;
        }

        tokio::time::advance(Duration::from_millis(1050)).await;
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));

        tx.unbounded_send(Ok(Bytes::from(make_gop(20, 0)))).unwrap();
        let Poll::Ready(Some(Ok(emitted))) = futures::poll!(trim_stream.next()) else {
            panic!("expected emit on realtime GOP 20");
        };

        let elapsed = start_time.elapsed();
        assert_eq!(elapsed, Duration::from_millis(2050));
        assert!(elapsed < DEFAULT_MAX_HOLD);
        assert!(!trim_stream.is_holding());
        assert_eq!(emitted.len(), 3 * 27 * TS_PACKET_SIZE, "exact emitted length matches GOPs 18..20");

        let gop18 = make_gop(18, 0);
        // Garbage prefix was dropped: output starts cleanly at GOP 18 PAT!
        assert_eq!(&emitted[..TS_PACKET_SIZE], &gop18[..TS_PACKET_SIZE]);
        assert_eq!(&emitted[TS_PACKET_SIZE..2 * TS_PACKET_SIZE], &gop18[TS_PACKET_SIZE..2 * TS_PACKET_SIZE]);
        assert_eq!(&emitted[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE], &gop18[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE]);
    }

    #[tokio::test(start_paused = true)]
    async fn test_probe_1504_bytes_starting_with_0x47_without_sync_pair_released_at_once() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let target = Duration::from_secs(3);
        let mut trim_stream = LiveEdgeTrimStream::new(rx, target);

        // Exactly SYNC_PROBE_BYTES (1504 bytes) starting with 0x47, but byte 188 is not 0x47,
        // and no valid sync pair exists anywhere in the probe.
        let mut probe = vec![0x55; SYNC_PROBE_BYTES];
        probe[0] = 0x47;
        probe[188] = 0xAA;

        let start_time = tokio::time::Instant::now();
        tx.unbounded_send(Ok(Bytes::copy_from_slice(&probe))).unwrap();

        // Must be released immediately unchanged without advancing time or waiting for 5s hold:
        let Poll::Ready(Some(Ok(emitted))) = futures::poll!(trim_stream.next()) else {
            panic!("expected unconfirmed probe to be released at once");
        };

        assert_eq!(start_time.elapsed(), Duration::ZERO, "must be released at once without delay");
        assert!(!trim_stream.is_holding());
        assert_eq!(emitted.as_ref(), &probe[..]);
    }

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

    fn make_no_keyframe_chunk_ms(media_ms: u64, pat_cc: u8) -> Vec<u8> {
        let pcr_ticks = media_ms.wrapping_mul(27_000);
        let mut chunk = Vec::with_capacity(4 * TS_PACKET_SIZE);
        chunk.extend_from_slice(&pcr_packet(0x100, pcr_ticks));
        chunk.extend_from_slice(&pat_packet(pat_cc));
        chunk.extend_from_slice(&pmt_packet(0));
        chunk.extend_from_slice(&ts_packet(0x100, false, None, &[0x55; 100]));
        chunk
    }

    #[tokio::test(start_paused = true)]
    async fn test_settled_burst_without_keyframe_releases_at_first_rap_wait() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let target = Duration::from_secs(4);
        let mut trim_stream = LiveEdgeTrimStream::new(rx, target);

        let start_time = tokio::time::Instant::now();

        // 1. Initial burst at 0 ms, 50 ms, 100 ms:
        let c0 = make_no_keyframe_chunk_ms(0, 0);
        tx.unbounded_send(Ok(Bytes::from(c0))).unwrap();
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));

        tokio::time::advance(Duration::from_millis(50)).await;
        let c1 = make_no_keyframe_chunk_ms(1000, 1);
        tx.unbounded_send(Ok(Bytes::from(c1))).unwrap();
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));

        tokio::time::advance(Duration::from_millis(50)).await;
        let c2 = make_no_keyframe_chunk_ms(2000, 2);
        tx.unbounded_send(Ok(Bytes::from(c2))).unwrap();
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));

        // 2. Real-time delivery settling the stream before 2 s (PCR steps stay below 1 s, no discontinuity):
        tokio::time::advance(Duration::from_millis(1300)).await; // wall = 1400 ms
        let c3 = make_no_keyframe_chunk_ms(2900, 3);
        tx.unbounded_send(Ok(Bytes::from(c3))).unwrap();
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));

        tokio::time::advance(Duration::from_millis(300)).await; // wall = 1700 ms
        let c4 = make_no_keyframe_chunk_ms(3200, 4);
        tx.unbounded_send(Ok(Bytes::from(c4))).unwrap();
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));

        // 3. Inner stream pauses / stays pending indefinitely.
        // Advance to 1 ms before FIRST_RAP_WAIT (2 s from first PCR wall):
        tokio::time::advance(Duration::from_millis(299)).await; // wall = 1999 ms
        assert!(matches!(futures::poll!(trim_stream.next()), Poll::Pending));

        // Advance 1 ms to reach exactly 2000 ms (FIRST_RAP_WAIT):
        tokio::time::advance(Duration::from_millis(1)).await; // wall = 2000 ms
        let poll_res = futures::poll!(trim_stream.next());
        let Poll::Ready(Some(Ok(emitted))) = poll_res else {
            panic!("expected release at FIRST_RAP_WAIT instant without keyframe, got {poll_res:?}");
        };

        let elapsed = start_time.elapsed();
        assert_eq!(elapsed, Duration::from_secs(2));
        assert!(elapsed < DEFAULT_MAX_HOLD);
        assert!(!trim_stream.is_holding());

        let expected_pat = pat_packet(0);
        assert_eq!(&emitted[..TS_PACKET_SIZE], &expected_pat[..TS_PACKET_SIZE], "starts at chunk 0 PAT");
    }
}
