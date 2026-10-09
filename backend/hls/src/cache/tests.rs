use super::{
    hls_cache_capacity_from_io, hls_cache_object_limit_from_io, staging::run_owned_cache_operation,
    CacheInvalidationOutcome, HlsCacheCapacityReclaimOutcome, HlsCacheCapacityReclaimRequest,
    HlsCacheCapacityReclaimer, HlsCacheObjectKey, HlsSegmentCache, MapCacheKey, SegmentCacheKey,
    TransientObjectCacheKey, MAX_CONCURRENT_OWNED_CACHE_OPERATIONS, MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN,
};
use crate::{transient::build_transient_resource_id, HlsOriginResourceFetchError, ProxySessionId};
use futures::{future::BoxFuture, FutureExt};
use std::{
    collections::{HashSet, VecDeque},
    future::Future,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, ReadBuf},
    sync::{oneshot, Semaphore},
};

fn cache_key() -> SegmentCacheKey { SegmentCacheKey::new(ProxySessionId("proxy_session".to_string()), 123, "ts") }

async fn cache_with_projected_session_pressure(
) -> (tempfile::TempDir, Arc<HlsSegmentCache>, SegmentCacheKey, SegmentCacheKey) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let proxy_session_id = ProxySessionId("capacity-session".to_string());
    let resident = SegmentCacheKey::new(proxy_session_id.clone(), 0, "ts");
    let first = SegmentCacheKey::new(proxy_session_id.clone(), 1, "ts");
    let second = SegmentCacheKey::new(proxy_session_id, 2, "ts");
    cache.update_cache_limits(5, 5);
    cache.write_bytes_and_commit(&resident, b"123").await.expect("resident object commits");
    (temp_dir, cache, first, second)
}

struct PausingCapacityReclaimer {
    cache: Arc<HlsSegmentCache>,
    victims: Mutex<VecDeque<SegmentCacheKey>>,
    first_reclaimed: Mutex<Option<oneshot::Sender<()>>>,
    resume_first: Mutex<Option<oneshot::Receiver<()>>>,
}

impl HlsCacheCapacityReclaimer for PausingCapacityReclaimer {
    fn reclaim_capacity(
        &self,
        _request: HlsCacheCapacityReclaimRequest,
    ) -> BoxFuture<'_, io::Result<HlsCacheCapacityReclaimOutcome>> {
        async move {
            let victim = self.victims.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pop_front();
            let Some(victim) = victim else {
                return Ok(HlsCacheCapacityReclaimOutcome::default());
            };
            self.cache.delete_if_inactive(&victim).await?;
            let first_reclaimed = self.first_reclaimed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
            if let Some(first_reclaimed) = first_reclaimed {
                let resume_first = self.resume_first.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
                let _ = first_reclaimed.send(());
                if let Some(resume_first) = resume_first {
                    let _ = resume_first.await;
                }
            }
            Ok(HlsCacheCapacityReclaimOutcome {
                reclaimed_session_bytes: 10,
                reclaimed_global_bytes: 10,
                protected_working_set_bytes: 0,
                reclaimable_bytes: 10,
            })
        }
        .boxed()
    }
}

struct ControlledReader {
    started: Option<oneshot::Sender<()>>,
    release: oneshot::Receiver<Vec<u8>>,
    body: Option<std::io::Cursor<Vec<u8>>>,
}

impl Unpin for ControlledReader {}

impl AsyncRead for ControlledReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some(started) = self.started.take() {
            let _send_result = started.send(());
        }
        if self.body.is_none() {
            match Pin::new(&mut self.release).poll(context) {
                Poll::Ready(Ok(body)) => self.body = Some(std::io::Cursor::new(body)),
                Poll::Ready(Err(_)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "controlled cache reader release dropped",
                    )))
                }
                Poll::Pending => return Poll::Pending,
            }
        }
        let body = self.body.as_mut().expect("controlled reader body is initialized");
        let position = usize::try_from(body.position()).unwrap_or(body.get_ref().len());
        let remaining = &body.get_ref()[position.min(body.get_ref().len())..];
        let copied = remaining.len().min(buffer.remaining());
        buffer.put_slice(&remaining[..copied]);
        let next_position = body.position().saturating_add(u64::try_from(copied).unwrap_or_default());
        body.set_position(next_position);
        Poll::Ready(Ok(()))
    }
}

mod admission;
mod http;
mod lifecycle;
mod policy;
mod publication;
mod storage;
mod streaming;
mod terminal;
mod transport;
