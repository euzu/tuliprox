use super::{
    BudgetedChunk, BufferedChunk, BurstBuffer, BurstRead, RandomAccessPoint, MAX_RAP_LOOKBACK, MIN_BURST_BUFFER_CHUNKS,
    MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES,
};
use bytes::Bytes;
use std::{
    collections::VecDeque,
    fmt,
    fmt::{Debug, Formatter},
    sync::Arc,
};
use tokio::{
    sync::{mpsc, mpsc::Sender, Semaphore},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::SharedSubscriberId;

pub(super) type SubscriberId = SharedSubscriberId;

#[allow(clippy::missing_fields_in_debug)]
impl Debug for BurstBuffer {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("BurstBuffer")
            .field("buffer_size", &self.buffer_size)
            .field("max_chunks", &self.max_chunks)
            .field("current_bytes", &self.current_bytes)
            .finish()
    }
}

impl BurstBuffer {
    pub fn new(buf_size: usize) -> Self {
        Self {
            buffer: VecDeque::new(),
            buffer_size: buf_size,
            max_chunks: Self::max_chunks_for_buffer_size(buf_size),
            current_bytes: 0,
            next_sequence: 0,
            next_start: 0,
            raps: VecDeque::new(),
        }
    }

    /// Extracts the join burst from the ring.
    ///
    /// Selects a start point keyframe-aligned within the requested burst window:
    /// (a) newest RAP with arrived <= target;
    /// (b) else oldest RAP in the ring;
    /// (c) else newest chunk with arrived <= target;
    /// (d) else oldest chunk (whole ring).
    /// Returns the sliced/cloned chunks and the ring head (`next_sequence`).
    pub fn join_snapshot(&self, now: Instant, burst: Duration) -> (Vec<Bytes>, u64) {
        if self.buffer.is_empty() {
            return (Vec::new(), self.next_sequence);
        }

        if burst.is_zero() {
            return (self.slice_from(0, None), self.next_sequence);
        }

        let Some(target) = now.checked_sub(burst) else {
            return (self.slice_from(0, None), self.next_sequence);
        };

        if !self.raps.is_empty() {
            let rap_idx = self.raps.partition_point(|rap| rap.arrived <= target);
            let chosen_rap = if rap_idx > 0 {
                let candidate = &self.raps[rap_idx - 1];
                if target.saturating_duration_since(candidate.arrived) <= MAX_RAP_LOOKBACK {
                    Some(candidate)
                } else {
                    None
                }
            } else {
                // (b) Oldest RAP in the ring when no RAP has arrived <= target.
                self.raps.front()
            };

            if let Some(rap) = chosen_rap {
                let chunk_idx = self.start_index_for_sequence(rap.sequence);
                if let Some(chunk) = self.buffer.get(chunk_idx) {
                    debug_assert_eq!(chunk.sequence, rap.sequence);
                    return (self.slice_from(chunk_idx, Some(rap.offset)), self.next_sequence);
                }
            }
        }

        // Fallback when no RAPs exist in the ring, or RAP is stale, or chunk is missing
        let chunk_idx = self.buffer.partition_point(|chunk| chunk.arrived <= target);
        let start_idx = if chunk_idx > 0 {
            // (c) Newest chunk with arrived <= target
            chunk_idx - 1
        } else {
            // (d) Oldest chunk in the ring
            0
        };
        (self.slice_from(start_idx, None), self.next_sequence)
    }

    fn slice_from(&self, start_idx: usize, rap_offset: Option<u32>) -> Vec<Bytes> {
        if start_idx >= self.buffer.len() {
            return Vec::new();
        }
        let mut chunks = Vec::with_capacity(self.buffer.len().saturating_sub(start_idx));
        let first = &self.buffer[start_idx];
        if let Some(offset) = rap_offset {
            let offset = usize::try_from(offset).unwrap_or(0);
            debug_assert!(offset < first.bytes.len());
            if offset <= first.bytes.len() {
                chunks.push(first.bytes.slice(offset..));
            } else {
                chunks.push(first.bytes.clone());
            }
        } else {
            chunks.push(first.bytes.clone());
        }
        for chunk in self.buffer.range(start_idx.saturating_add(1)..) {
            chunks.push(chunk.bytes.clone());
        }
        chunks
    }

    pub fn read_from_into(&self, next_sequence: u64, chunks: &mut Vec<Bytes>, max_chunks: usize) -> BurstRead {
        chunks.clear();
        let earliest_sequence = self.buffer.front().map_or(self.next_sequence, |chunk| chunk.sequence);
        let start_sequence = next_sequence.max(earliest_sequence);
        let skipped = start_sequence.saturating_sub(next_sequence);
        let start_index = self.start_index_for_sequence(start_sequence);
        let mut read_next_sequence = start_sequence;
        for chunk in self.buffer.range(start_index..).take(max_chunks) {
            chunks.push(chunk.bytes.clone());
            read_next_sequence = chunk.sequence.saturating_add(1);
        }

        BurstRead { next_sequence: read_next_sequence, skipped }
    }

    /// Appends one packet to the burst ring, pruning older entries when bounds are exceeded.
    ///
    /// Invariants:
    /// - Only the broadcast task writes to `BurstBuffer` under the `burst_buffer` mutex.
    /// - `raps` is pruned in the same call as eviction so every entry refers to a buffered chunk.
    /// - At most one RAP entry is retained per chunk, holding the latest offset for that chunk.
    /// - Stream positions (`next_start`) and scanner positions count the exact same bytes in order.
    pub fn push(&mut self, packet: Bytes, arrived: Instant, rap: Option<u64>) {
        let packet_len = packet.len();
        while !self.buffer.is_empty()
            && (self.buffer.len() >= self.max_chunks
                || self.current_bytes.saturating_add(packet_len) > self.buffer_size)
        {
            if let Some(popped) = self.buffer.pop_front() {
                self.current_bytes = self.current_bytes.saturating_sub(popped.bytes.len());
            }
        }

        // Eviction invariant: remove any RAP whose referenced chunk was evicted in this call.
        if let Some(front) = self.buffer.front() {
            let min_seq = front.sequence;
            while self.raps.front().is_some_and(|r| r.sequence < min_seq) {
                self.raps.pop_front();
            }
        } else {
            self.raps.clear();
        }

        let packet_len_u64 = u64::try_from(packet_len).unwrap_or(0);
        let start = self.next_start;
        self.next_start = self.next_start.saturating_add(packet_len_u64);
        self.current_bytes = self.current_bytes.saturating_add(packet_len);
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.buffer.push_back(BufferedChunk { sequence, start, arrived, bytes: packet });

        if let Some(pos) = rap {
            let idx = self.buffer.partition_point(|chunk| chunk.start <= pos);
            if idx > 0 {
                if let Some(candidate) = self.buffer.get(idx - 1) {
                    let chunk_len = u64::try_from(candidate.bytes.len()).unwrap_or(0);
                    if pos >= candidate.start && pos < candidate.start.saturating_add(chunk_len) {
                        if let Ok(offset) = u32::try_from(pos.saturating_sub(candidate.start)) {
                            let candidate_seq = candidate.sequence;
                            let candidate_arrived = candidate.arrived;
                            if let Some(last) = self.raps.back_mut() {
                                if candidate_seq > last.sequence {
                                    self.raps.push_back(RandomAccessPoint {
                                        sequence: candidate_seq,
                                        offset,
                                        arrived: candidate_arrived,
                                    });
                                } else if candidate_seq == last.sequence && offset > last.offset {
                                    last.offset = offset;
                                    last.arrived = candidate_arrived;
                                }
                            } else {
                                self.raps.push_back(RandomAccessPoint {
                                    sequence: candidate_seq,
                                    offset,
                                    arrived: candidate_arrived,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn push_now(&mut self, packet: Bytes) { self.push(packet, Instant::now(), None); }

    pub(super) fn start_index_for_sequence(&self, sequence: u64) -> usize {
        let mut left = 0_usize;
        let mut right = self.buffer.len();

        while left < right {
            let mid = left + ((right - left) / 2);
            let mid_sequence = self.buffer.get(mid).map_or(u64::MAX, |chunk| chunk.sequence);
            if mid_sequence < sequence {
                left = mid.saturating_add(1);
            } else {
                right = mid;
            }
        }

        left
    }

    pub(super) fn max_chunks_for_buffer_size(buffer_size: usize) -> usize {
        buffer_size.div_ceil(MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES).max(MIN_BURST_BUFFER_CHUNKS)
    }
}

/// Terminal outcome of trying to deliver one chunk to a subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SendOutcome {
    Sent,
    Cancelled,
    Closed,
    TimedOut,
}

async fn reserve_byte_permit(
    byte_budget: &Arc<Semaphore>,
    len: usize,
    cancellation_token: &CancellationToken,
    progress_deadline: Instant,
) -> Result<tokio::sync::OwnedSemaphorePermit, SendOutcome> {
    let permits = u32::try_from(len).unwrap_or(u32::MAX);
    tokio::select! {
        biased;
        () = cancellation_token.cancelled() => Err(SendOutcome::Cancelled),
        result = tokio::time::timeout_at(progress_deadline, byte_budget.clone().acquire_many_owned(permits)) => {
            match result {
                Ok(Ok(permit)) => Ok(permit),
                Ok(Err(_)) => Err(SendOutcome::Closed),
                Err(_) => Err(SendOutcome::TimedOut),
            }
        }
    }
}

pub(super) async fn send_burst_buffer(
    start_buffer: &[Bytes],
    client_tx: &Sender<BudgetedChunk>,
    cancellation_token: &CancellationToken,
    idle_timeout: Duration,
    byte_budget: &Arc<Semaphore>,
) -> Result<usize, SendOutcome> {
    let mut sent = 0_usize;
    let mut last_progress = Instant::now();
    for buf in start_buffer {
        let deadline = last_progress + idle_timeout;
        match send_client_chunk(client_tx, buf.clone(), cancellation_token, deadline, byte_budget).await {
            SendOutcome::Sent => {
                sent = sent.saturating_add(1);
                last_progress = Instant::now();
            }
            outcome => return Err(outcome),
        }
    }
    Ok(sent)
}

pub(super) async fn send_client_chunk(
    client_tx: &Sender<BudgetedChunk>,
    data: Bytes,
    cancellation_token: &CancellationToken,
    progress_deadline: Instant,
    byte_budget: &Arc<Semaphore>,
) -> SendOutcome {
    if cancellation_token.is_cancelled() {
        return SendOutcome::Cancelled;
    }

    let permit = match reserve_byte_permit(byte_budget, data.len(), cancellation_token, progress_deadline).await {
        Ok(permit) => permit,
        Err(outcome) => return outcome,
    };

    let chunk = BudgetedChunk { bytes: data, _permit: permit };
    match client_tx.try_send(chunk) {
        Ok(()) => SendOutcome::Sent,
        Err(mpsc::error::TrySendError::Closed(_)) => SendOutcome::Closed,
        Err(mpsc::error::TrySendError::Full(chunk)) => tokio::select! {
            biased;
            () = cancellation_token.cancelled() => SendOutcome::Cancelled,
            result = tokio::time::timeout_at(progress_deadline, client_tx.send(chunk)) => match result {
                Ok(Ok(())) => SendOutcome::Sent,
                Ok(Err(_)) => SendOutcome::Closed,
                Err(_) => SendOutcome::TimedOut,
            },
        },
    }
}
