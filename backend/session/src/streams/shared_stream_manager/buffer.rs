use super::{
    BudgetedChunk, BufferedChunk, BurstBuffer, BurstRead, MIN_BURST_BUFFER_CHUNKS,
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
        }
    }

    pub fn snapshot(&self) -> (Vec<Bytes>, u64) {
        (self.buffer.iter().map(|chunk| chunk.bytes.clone()).collect::<Vec<Bytes>>(), self.next_sequence)
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

    pub fn push(&mut self, packet: Bytes) {
        let packet_len = packet.len();
        while !self.buffer.is_empty()
            && (self.buffer.len() >= self.max_chunks
                || self.current_bytes.saturating_add(packet_len) > self.buffer_size)
        {
            if let Some(popped) = self.buffer.pop_front() {
                self.current_bytes = self.current_bytes.saturating_sub(popped.bytes.len());
            }
        }
        self.current_bytes = self.current_bytes.saturating_add(packet_len);
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.buffer.push_back(BufferedChunk { sequence, bytes: packet });
    }

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
