use super::{SharedStreamManager, SharedStreamState, YIELD_COUNTER};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use log::warn;
use shared::utils::sanitize_sensitive_info;
use std::sync::Arc;
use tokio::time::{sleep, Duration, Instant};
use tuliprox_core::utils::{debug_if_enabled, network::request::STREAM_IDLE_TIMEOUT, trace_if_enabled};

impl SharedStreamState {
    #[allow(clippy::too_many_lines)]
    pub(super) fn broadcast<S, E>(
        self: &Arc<Self>,
        stream_url: &str,
        bytes_stream: S,
        shared_streams: Arc<SharedStreamManager>,
    ) where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static + Send,
        E: std::fmt::Debug + Send,
    {
        let streaming_url = stream_url.to_string();
        let origin_state = Arc::clone(self);
        let stop_token = self.stop_token.clone();
        let burst_buffer = Arc::clone(&self.burst_buffer);
        let live_notification = Arc::clone(&self.live_notification);
        let broadcast_started_at = Instant::now();

        let broadcast_handle = tokio::spawn(async move {
            let mut source_stream = std::pin::pin!(bytes_stream);
            let mut counter = 0_usize;
            let idle_timeout = Duration::from_secs(STREAM_IDLE_TIMEOUT);
            let idle = sleep(idle_timeout);
            tokio::pin!(idle);
            let mut first_source_chunk_logged = false;
            let mut startup_chunks_seen = 0_usize;
            let mut startup_bytes_seen = 0_usize;
            let mut startup_stats_logged = false;
            // Track the time of the most recent upstream push so the broadcast can
            // detect a stalled source before the global idle timeout fires. This is
            // the diagnostic signal for H1 (broadcast stall with stale burst replay).
            let mut last_push_at: Option<Instant> = None;
            let mut idle_warning_emitted = false;
            let idle_warn_threshold = idle_timeout / 2;

            loop {
                tokio::select! {
                    biased;

                    () = stop_token.cancelled() => {
                        debug_if_enabled!(
                            "No shared stream subscribers left. Closing shared provider stream {}",
                            sanitize_sensitive_info(&streaming_url)
                        );
                        break;
                    }

                    () = &mut idle => {
                        debug_if_enabled!(
                            "Shared stream source idle timeout after {}s for {}",
                            STREAM_IDLE_TIMEOUT,
                            sanitize_sensitive_info(&streaming_url)
                        );
                        stop_token.cancel();
                        break;
                    }

                    chunk = source_stream.next() => {
                        // Only successful chunks count as liveness; resetting on Err would let an
                        // error-spinning source dodge the idle timeout forever
                        if matches!(chunk, Some(Ok(_))) {
                            idle.as_mut().reset(Instant::now() + idle_timeout);
                        }
                        match chunk {
                            Some(Ok(data)) => {
                                let chunk_len = data.len();
                                let push_seq = {
                                    let mut buffer = burst_buffer.lock().await;
                                    let seq = buffer.next_sequence;
                                    buffer.push(data);
                                    seq
                                };
                                live_notification.notify_waiters();
                                last_push_at = Some(Instant::now());
                                idle_warning_emitted = false;
                                trace_if_enabled!(
                                    "shared_stream.broadcast: push seq={} len={} url={}",
                                    push_seq,
                                    chunk_len,
                                    sanitize_sensitive_info(&streaming_url)
                                );

                                if !first_source_chunk_logged {
                                    debug_if_enabled!(
                                        "Shared stream source produced first chunk for {} after {} ms",
                                        sanitize_sensitive_info(&streaming_url),
                                        broadcast_started_at.elapsed().as_millis()
                                    );
                                    first_source_chunk_logged = true;
                                }
                                if !startup_stats_logged {
                                    startup_chunks_seen = startup_chunks_seen.saturating_add(1);
                                    startup_bytes_seen = startup_bytes_seen.saturating_add(chunk_len);
                                    if broadcast_started_at.elapsed() >= Duration::from_secs(5) {
                                        debug_if_enabled!(
                                            "Shared stream source startup throughput for {}: chunks={} bytes={} over {} ms",
                                            sanitize_sensitive_info(&streaming_url),
                                            startup_chunks_seen,
                                            startup_bytes_seen,
                                            broadcast_started_at.elapsed().as_millis()
                                        );
                                        startup_stats_logged = true;
                                    }
                                }

                                counter = counter.saturating_add(1);
                                if counter >= YIELD_COUNTER {
                                    tokio::task::yield_now().await;
                                    counter = 0;
                                }
                            }
                            Some(Err(e)) => {
                                trace_if_enabled!(
                                    "Shared stream source error for {}: {:?}",
                                    sanitize_sensitive_info(&streaming_url),
                                    e
                                );
                                tokio::task::yield_now().await;
                            }
                            None => {
                                debug_if_enabled!(
                                    "Shared stream source stream ended for {}",
                                    sanitize_sensitive_info(&streaming_url)
                                );
                                break;
                            }
                        }
                    }
                }

                // Edge-triggered stall warning: fires once when the broadcast has
                // not seen an upstream push for STREAM_IDLE_TIMEOUT/2 seconds, then
                // resets on the next successful push. Operators correlate this
                // warning with the "stuck in resending same buffer" symptom (H1).
                if let Some(last) = last_push_at {
                    let stalled_for = last.elapsed();
                    if stalled_for >= idle_warn_threshold && !idle_warning_emitted {
                        warn!(
                            "shared_stream.broadcast: no upstream bytes for {}s on url={}; \
                             source may be stalled. Subscribers will see cached burst until {}s timeout.",
                            stalled_for.as_secs(),
                            sanitize_sensitive_info(&streaming_url),
                            STREAM_IDLE_TIMEOUT
                        );
                        idle_warning_emitted = true;
                    }
                }
            }

            debug_if_enabled!(
                "Shared stream exiting for {} (last_push_age_secs={})",
                sanitize_sensitive_info(&streaming_url),
                last_push_at.map_or(0, |t| t.elapsed().as_secs())
            );
            shared_streams.unregister(&streaming_url, &origin_state).await;
        });

        // Keep the broadcast handle so shutdown can join it instead of leaving a detached task.
        // Registration is guaranteed: a short-lived synchronous lock avoids the try_write
        // failure that would otherwise drop the handle under contention.
        self.lock_task_handles().push(broadcast_handle);
    }
}
