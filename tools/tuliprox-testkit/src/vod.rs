use crate::TestkitError;
use bytes::Bytes;
use futures::Stream;
use reqwest::{header, StatusCode};
use std::{pin::Pin, time::Duration};
use tokio::sync::oneshot;

pub const DEFAULT_VOD_SIZE: u64 = 128 * 1024 * 1024; // 128 MiB virtual media file
const BLOCK_SIZE: usize = 1024;

pub async fn read_range(url: &str, start: u64, end_inclusive: u64) -> Result<Vec<u8>, TestkitError> {
    if start > end_inclusive {
        return Err(TestkitError::Configuration("VOD range start exceeds end".to_owned()));
    }
    let range = format!("bytes={start}-{end_inclusive}");
    let response = reqwest::Client::new().get(url).header(header::RANGE, range).send().await?;
    if response.status() != StatusCode::PARTIAL_CONTENT {
        return Err(TestkitError::Protocol(format!("VOD range returned {}, expected 206", response.status())));
    }
    let expected_range = format!("bytes {start}-{end_inclusive}/");
    let content_range = response
        .headers()
        .get(header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| TestkitError::Protocol("VOD range response has no Content-Range".to_owned()))?;
    if !content_range.starts_with(&expected_range) {
        return Err(TestkitError::Protocol(format!("unexpected VOD Content-Range {content_range}")));
    }
    let body = response.bytes().await?.to_vec();
    let expected_len = usize::try_from(end_inclusive.saturating_sub(start).saturating_add(1))
        .map_err(|_| TestkitError::Configuration("VOD range is too large for this host".to_owned()))?;
    if body.len() != expected_len {
        return Err(TestkitError::Protocol(format!("VOD range has {} bytes, expected {expected_len}", body.len())));
    }
    Ok(body)
}

#[must_use]
pub fn parse_range_header(value: &str, total_length: u64) -> Option<(u64, u64)> {
    let bytes = value.strip_prefix("bytes=")?;
    if let Some(suffix) = bytes.strip_prefix('-') {
        let suffix_len = suffix.parse::<u64>().ok()?;
        if suffix_len == 0 || suffix_len > total_length {
            return Some((0, total_length));
        }
        let start = total_length.saturating_sub(suffix_len);
        return Some((start, total_length));
    }

    let (start_str, end_str) = bytes.split_once('-')?;
    let start = start_str.parse::<u64>().ok()?;
    if start >= total_length {
        return None;
    }

    let end = if end_str.is_empty() {
        total_length
    } else {
        let parsed_end = end_str.parse::<u64>().ok()?;
        parsed_end.min(total_length.saturating_sub(1)).saturating_add(1)
    };

    if start >= end {
        return None;
    }

    Some((start, end))
}

#[must_use]
pub fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let bytes = value.strip_prefix("bytes ")?;
    let (range_part, total_part) = bytes.split_once('/')?;
    let (start_str, end_str) = range_part.split_once('-')?;
    let start = start_str.parse::<u64>().ok()?;
    let end = end_str.parse::<u64>().ok()?;
    let total = if total_part == "*" { None } else { Some(total_part.parse::<u64>().ok()?) };
    Some((start, end, total))
}

#[must_use]
pub fn generate_deterministic_block(object: &str, block_index: u64) -> [u8; BLOCK_SIZE] {
    let seed = blake3::hash(object.as_bytes());
    let block_hash = blake3::keyed_hash(seed.as_bytes(), &block_index.to_le_bytes());
    let mut block = [0_u8; BLOCK_SIZE];
    block[0..8].copy_from_slice(&block_index.to_be_bytes());
    let hash_bytes = block_hash.as_bytes();
    for chunk in block[8..].chunks_mut(32) {
        let len = chunk.len().min(32);
        chunk.copy_from_slice(&hash_bytes[..len]);
    }
    block
}

#[must_use]
pub fn generate_deterministic_slice(object: &str, offset: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut current_offset = offset;
    let end_offset = offset.saturating_add(len as u64);

    while current_offset < end_offset {
        let block_index = current_offset / (BLOCK_SIZE as u64);
        let in_block = usize::try_from(current_offset % (BLOCK_SIZE as u64)).unwrap_or(0);
        let block = generate_deterministic_block(object, block_index);
        let remaining_in_block = BLOCK_SIZE.saturating_sub(in_block);
        let remaining_requested = usize::try_from(end_offset.saturating_sub(current_offset)).unwrap_or(0);
        let take = remaining_in_block.min(remaining_requested);
        out.extend_from_slice(&block[in_block..in_block.saturating_add(take)]);
        current_offset = current_offset.saturating_add(take as u64);
    }
    out
}

#[must_use]
pub fn verify_deterministic_slice(object: &str, offset: u64, data: &[u8]) -> bool {
    let expected = generate_deterministic_slice(object, offset, data.len());
    data == expected.as_slice()
}

#[derive(Debug, Clone)]
pub struct VodStreamOptions {
    pub object: String,
    pub start_offset: u64,
    pub length: u64,
    pub bitrate: u32,
    pub chunk_size: usize,
    pub stall_ms: Option<u64>,
    pub abort_after_bytes: Option<u64>,
}

impl Default for VodStreamOptions {
    fn default() -> Self {
        Self {
            object: "movie.mkv".to_owned(),
            start_offset: 0,
            length: DEFAULT_VOD_SIZE,
            bitrate: 0,
            chunk_size: 64 * 1024,
            stall_ms: None,
            abort_after_bytes: None,
        }
    }
}

struct VodStreamState {
    options: VodStreamOptions,
    evict_rx: oneshot::Receiver<()>,
    evicted_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    current_offset: u64,
    end_offset: u64,
    total_emitted: u64,
    interval: Option<tokio::time::Interval>,
    initial_stall_done: bool,
}

pub fn create_vod_stream(
    options: VodStreamOptions,
    evict_rx: oneshot::Receiver<()>,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, std::convert::Infallible>> + Send>> {
    create_vod_stream_with_evicted(options, evict_rx, None)
}

pub fn create_vod_stream_with_evicted(
    options: VodStreamOptions,
    evict_rx: oneshot::Receiver<()>,
    evicted_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, std::convert::Infallible>> + Send>> {
    let current_offset = options.start_offset;
    let end_offset = options.start_offset.saturating_add(options.length);

    let interval = if options.bitrate > 0 {
        let bits_per_chunk = (options.chunk_size as u128).saturating_mul(8);
        let nanos =
            bits_per_chunk.saturating_mul(1_000_000_000).checked_div(u128::from(options.bitrate.max(1))).unwrap_or(1);
        let duration = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX).max(1));
        let mut interval = tokio::time::interval(duration);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Some(interval)
    } else {
        None
    };

    let state = VodStreamState {
        options,
        evict_rx,
        evicted_flag,
        current_offset,
        end_offset,
        total_emitted: 0,
        interval,
        initial_stall_done: false,
    };

    Box::pin(futures::stream::unfold(state, |mut state| async move {
        if !state.initial_stall_done {
            state.initial_stall_done = true;
            if let Some(stall) = state.options.stall_ms {
                tokio::time::sleep(Duration::from_millis(stall)).await;
            }
        }

        if state.current_offset >= state.end_offset {
            return None;
        }

        if let Some(abort_limit) = state.options.abort_after_bytes {
            if state.total_emitted >= abort_limit {
                return None;
            }
        }

        let remaining = usize::try_from(state.end_offset.saturating_sub(state.current_offset)).unwrap_or(0);
        let mut take = state.options.chunk_size.min(remaining);
        if let Some(abort_limit) = state.options.abort_after_bytes {
            let bytes_until_abort = usize::try_from(abort_limit.saturating_sub(state.total_emitted)).unwrap_or(0);
            take = take.min(bytes_until_abort);
        }

        if take == 0 {
            return None;
        }

        if let Some(interval) = state.interval.as_mut() {
            tokio::select! {
                biased;
                res = &mut state.evict_rx => {
                    if res.is_ok() {
                        if let Some(flag) = state.evicted_flag.as_ref() {
                            flag.store(true, std::sync::atomic::Ordering::Release);
                        }
                    }
                    return None;
                }
                _ = interval.tick() => {}
            }
        } else {
            match state.evict_rx.try_recv() {
                Ok(()) => {
                    if let Some(flag) = state.evicted_flag.as_ref() {
                        flag.store(true, std::sync::atomic::Ordering::Release);
                    }
                    return None;
                }
                Err(oneshot::error::TryRecvError::Closed) => return None,
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
        }

        let chunk = generate_deterministic_slice(&state.options.object, state.current_offset, take);
        state.current_offset = state.current_offset.saturating_add(take as u64);
        state.total_emitted = state.total_emitted.saturating_add(take as u64);

        Some((Ok(Bytes::from(chunk)), state))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt;

    #[test]
    fn parses_open_and_closed_and_suffix_ranges() {
        assert_eq!(parse_range_header("bytes=0-1023", 10_000), Some((0, 1024)));
        assert_eq!(parse_range_header("bytes=100-", 10_000), Some((100, 10_000)));
        assert_eq!(parse_range_header("bytes=-500", 10_000), Some((9500, 10_000)));
        assert_eq!(parse_range_header("bytes=10000-", 10_000), None);
        assert_eq!(parse_range_header("bytes=500-200", 10_000), None);
    }

    #[test]
    fn deterministic_slice_verification_is_exact() {
        let object = "test_vod.mkv";
        let offset = 4096;
        let slice = generate_deterministic_slice(object, offset, 2048);
        assert_eq!(slice.len(), 2048);
        assert!(verify_deterministic_slice(object, offset, &slice));

        // Tampered byte must fail verification
        let mut tampered = slice.clone();
        tampered[10] ^= 0xFF;
        assert!(!verify_deterministic_slice(object, offset, &tampered));

        // Wrong offset must fail verification
        assert!(!verify_deterministic_slice(object, offset + 1, &slice));
    }

    #[tokio::test]
    async fn vod_stream_respects_abort_after_bytes() {
        let options = VodStreamOptions {
            object: "movie.mkv".to_owned(),
            start_offset: 0,
            length: 100_000,
            bitrate: 0,
            chunk_size: 1024,
            stall_ms: None,
            abort_after_bytes: Some(2500),
        };
        let (_tx, rx) = oneshot::channel();
        let mut stream = create_vod_stream(options, rx);

        let mut collected = Vec::new();
        while let Some(Ok(chunk)) = stream.next().await {
            collected.extend_from_slice(&chunk);
        }

        assert_eq!(collected.len(), 2500);
        assert!(verify_deterministic_slice("movie.mkv", 0, &collected));
    }

    #[tokio::test]
    async fn vod_stream_stops_on_eviction() {
        let options = VodStreamOptions {
            object: "movie.mkv".to_owned(),
            start_offset: 0,
            length: 1_000_000,
            bitrate: 0,
            chunk_size: 1024,
            stall_ms: None,
            abort_after_bytes: None,
        };
        let (tx, rx) = oneshot::channel();
        let mut stream = create_vod_stream(options, rx);

        // Read 1 chunk
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.len(), 1024);

        // Send eviction signal
        let _ = tx.send(());

        // Next read must terminate
        assert!(stream.next().await.is_none());
    }

    #[test]
    fn parse_content_range_valid_and_invalid() {
        assert_eq!(parse_content_range("bytes 0-65535/134217728"), Some((0, 65535, Some(134_217_728))));
        assert_eq!(parse_content_range("bytes 100-200/*"), Some((100, 200, None)));
        assert_eq!(parse_content_range("bytes 0-0/1"), Some((0, 0, Some(1))));
        assert_eq!(parse_content_range("0-65535/134217728"), None);
        assert_eq!(parse_content_range("bytes 0-65535"), None);
        assert_eq!(parse_content_range("bytes invalid-range/100"), None);
    }
}
