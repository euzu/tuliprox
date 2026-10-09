use super::{
    segment_fetch_binding_matches, CachedSegmentMetadata, HlsSegmentRepairObjectContext, HlsSessionHandle,
    SegmentFetchContext, SegmentFetchError, SegmentFetchSnapshot,
};
use crate::sync_ext::MutexExt;
use log::debug;
use std::sync::Arc;
use tokio::time::Instant;
use tuliprox_core::utils::{content_coding::DecodedHttpResponse, current_time_millis};

/// Worst-case simultaneous on-disk copies of one segment body while a raw revision is filled.
const RAW_REVISION_PEAK_FACTOR: u64 = 5;
/// Worst-case simultaneous on-disk copies of one segment body while a processed revision is filled.
const PROCESSED_REVISION_PEAK_FACTOR: u64 = 3;

const fn revision_peak_factor(raw: bool) -> u64 {
    if raw {
        RAW_REVISION_PEAK_FACTOR
    } else {
        PROCESSED_REVISION_PEAK_FACTOR
    }
}

pub(super) fn reliable_decoded_content_length(response: &DecodedHttpResponse) -> Option<u64> {
    if response.was_content_decoded() {
        return None;
    }
    response.headers.get(axum::http::header::CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

pub(super) struct HlsObservedFill {
    pub(super) revision: super::super::SegmentRevisionGuard,
    pub(super) session: HlsSessionHandle,
    pub(super) context: SegmentFetchContext,
    pub(super) file: Option<tokio::fs::File>,
    pub(super) bytes: u64,
    pub(super) expected: Option<u64>,
    pub(super) limit: u64,
    pub(super) path: std::path::PathBuf,
    pub(super) generation: u64,
    pub(super) budget: Arc<super::super::ProgressiveBudgetManager>,
    pub(super) deadline: Instant,
    pub(super) repair_permit: Option<super::super::progressive_budget::DeferredRepairPermit>,
    pub(super) additional_reservations: Vec<super::super::HlsRevisionDiskReservation>,
}

impl Drop for HlsObservedFill {
    fn drop(&mut self) { self.revision.revision().fail(); }
}

impl HlsObservedFill {
    pub(super) async fn observe(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        self.bytes = self
            .bytes
            .checked_add(u64::try_from(chunk.len()).map_err(std::io::Error::other)?)
            .ok_or_else(|| std::io::Error::other("origin body length overflow"))?;
        if self.bytes > self.limit {
            self.extend_disk_reservation().await?;
        }
        if let Some(file) = &mut self.file {
            file.write_all(chunk).await?;
        }
        let has_prefix = if self.file.is_some() {
            let mut replay = self.revision.revision().replay.lock_unpoisoned();
            let result = replay.as_mut().map(|replay| replay.push(chunk));
            match result {
                Some(Err(_)) => {
                    replay.take();
                    drop(replay);
                    self.revision.revision().replay_exhausted();
                    debug!("HLS startup replay exhausted: proxy_seq={}", self.revision.revision().key.proxy_seq);
                    false
                }
                Some(Ok(())) => true,
                None => false,
            }
        } else {
            true
        };
        if has_prefix {
            let first = self.revision.revision().prefix_available.fetch_add(
                u64::try_from(chunk.len()).map_err(std::io::Error::other)?,
                std::sync::atomic::Ordering::AcqRel,
            ) == 0;
            self.revision.revision().prefix_changed();
            if first {
                let mut session = self.session.write().await;
                if session.activity.origin_work_generation == self.generation {
                    if let Err(error) = session.render_and_store_manifest(current_time_millis()) {
                        debug!(
                            "HLS startup manifest render after first prefix failed: proxy_seq={} error={error:?}",
                            self.revision.revision().key.proxy_seq
                        );
                    }
                    let worker = session.startup.as_ref().and_then(|startup| startup.worker.upgrade());
                    drop(session);
                    if let Some(worker) = worker {
                        worker.schedule_wake(self.context.clone(), current_time_millis());
                    }
                }
            }
        }
        Ok(())
    }

    async fn extend_disk_reservation(&mut self) -> std::io::Result<()> {
        if self.expected.is_some() {
            return Err(std::io::Error::other("origin body exceeds decoded content length"));
        }
        let next_limit = self.limit.saturating_mul(2).max(self.bytes);
        let factor = revision_peak_factor(self.file.is_some());
        let additional_peak = next_limit
            .checked_sub(self.limit)
            .and_then(|bytes| bytes.checked_mul(factor))
            .ok_or_else(|| std::io::Error::other("revision disk reservation overflow"))?;
        let mut reservation =
            self.context.segment_cache.reserve_revision_peak(&self.revision.revision().key, additional_peak).await?;
        if self.file.is_some() {
            reservation.mark_unaccounted_writes();
        }
        self.additional_reservations.push(reservation);
        self.limit = next_limit;
        Ok(())
    }

    pub(super) async fn finish(&mut self) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        if self.bytes == 0 || self.expected.is_some_and(|length| length != self.bytes) {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "incomplete origin segment body"));
        }
        if let Some(file) = self.file.take() {
            let mut file = file;
            file.flush().await?;
            drop(file);
            self.revision.revision().complete(CachedSegmentMetadata { path: self.path.clone(), size: self.bytes });
            self.revision.revision().replay.lock_unpoisoned().take();
        }
        self.repair_permit = Some(self.budget.wait_for_repair(self.deadline).await?);
        Ok(())
    }
}

pub(super) async fn commit_startup_response(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    response: DecodedHttpResponse,
    deadline: Instant,
    mut repair_context: HlsSegmentRepairObjectContext,
) -> Result<CachedSegmentMetadata, SegmentFetchError> {
    use futures::StreamExt;
    if repair_context.hls_access_lease_id.is_none() {
        let leases = context.session.read().await.startup.as_ref().map(|startup| Arc::clone(&startup.access_leases));
        if let Some(leases) = leases {
            repair_context.hls_access_lease_id = leases
                .write()
                .await
                .select_startup_repair_lease(&repair_context.proxy_session_id, current_time_millis());
        }
    }
    if let Some(lease_id) = &repair_context.hls_access_lease_id {
        context.segment_repair.ensure_access_lease_window(lease_id.clone()).await;
    }
    let expected = reliable_decoded_content_length(&response);
    if response.status != axum::http::StatusCode::OK {
        return Err(SegmentFetchError::cache_body(&std::io::Error::other(
            "startup requires a complete HTTP 200 response",
        )));
    }
    let prepared = prepare_startup_fill(context, snapshot, expected, deadline).await?;
    let PreparedStartupFill { revision, observed, _reservation, raw } = prepared;
    let chunks_owner = Arc::clone(&observed);
    let finish_owner = Arc::clone(&observed);
    let stream = tokio_util::io::ReaderStream::new(response.body)
        .then(move |chunk| {
            let observed = Arc::clone(&chunks_owner);
            async move {
                let chunk = chunk?;
                observed.lock().await.observe(&chunk).await?;
                Ok::<_, std::io::Error>(chunk)
            }
        })
        .chain(futures::stream::once(async move {
            finish_owner.lock().await.finish().await?;
            Ok::<_, std::io::Error>(bytes::Bytes::new())
        }));
    let reader = Box::pin(tokio_util::io::StreamReader::new(stream.boxed()));
    let mut metadata = context
        .segment_repair
        .commit_origin_response(&context.segment_cache, &snapshot.cache_key, reader, deadline, repair_context.clone())
        .await
        .map_err(|error| SegmentFetchError::cache_body(&error))?;
    if !raw {
        context
            .segment_repair
            .repair_ready_cache_hit(&context.segment_cache, &snapshot.cache_key, repair_context)
            .await
            .map_err(|error| SegmentFetchError::cache_body(&error))?;
        let latest = context
            .segment_cache
            .metadata(&snapshot.cache_key)
            .await
            .map_err(|error| SegmentFetchError::cache_body(&error))?
            .ok_or_else(|| {
                SegmentFetchError::cache_body(&std::io::Error::other("processed cache object disappeared"))
            })?;
        metadata = latest;
        let processed = context
            .segment_cache
            .publish_processed_revision(&revision.revision().key, &metadata.path, deadline)
            .await
            .map_err(|error| SegmentFetchError::cache_body(&error))?;
        revision.revision().complete(processed);
    }
    drop(observed);
    Ok(metadata)
}

pub(super) struct PreparedStartupFill {
    pub(super) revision: super::super::SegmentRevisionGuard,
    pub(super) observed: Arc<tokio::sync::Mutex<HlsObservedFill>>,
    pub(super) _reservation: super::super::HlsRevisionDiskReservation,
    pub(super) raw: bool,
}

pub(super) async fn prepare_startup_fill(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    expected: Option<u64>,
    deadline: Instant,
) -> Result<PreparedStartupFill, SegmentFetchError> {
    // A disk-capacity fallback switches the session to `first_ready` before it
    // asks for another attempt, so the retry prepares a processed revision.
    loop {
        if let Some(prepared) = try_prepare_startup_fill(context, snapshot, expected, deadline).await? {
            return Ok(prepared);
        }
    }
}

async fn try_prepare_startup_fill(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    expected: Option<u64>,
    deadline: Instant,
) -> Result<Option<PreparedStartupFill>, SegmentFetchError> {
    let (store, budget, mut mode, limit, session_id) = {
        let session = context.session.read().await;
        if !session.segments.get(&snapshot.proxy_seq).is_some_and(super::super::SegmentEntry::is_fast_start_eligible) {
            return Err(SegmentFetchError::UnsupportedStartupMedia);
        }
        let startup = session
            .startup
            .as_ref()
            .ok_or_else(|| SegmentFetchError::cache_body(&std::io::Error::other("startup policy unavailable")))?;
        (
            Arc::clone(&startup.store),
            Arc::clone(&startup.budget),
            startup.config.mode,
            startup.config.max_progressive_bytes_per_segment.get(),
            session.proxy_session_id.clone(),
        )
    };
    let mut replay = (mode == shared::model::HlsStartupMode::Progressive
        && expected.is_none_or(|length| length <= limit))
    .then(|| super::super::progressive_startup::ProgressiveReplay::new(&budget))
    .flatten();
    if mode == shared::model::HlsStartupMode::Progressive && replay.is_none() {
        let mut session = context.session.write().await;
        if session.startup.as_ref().is_some_and(|startup| !startup.policy_fixed) {
            if let Some(startup) = &mut session.startup {
                startup.config.mode = shared::model::HlsStartupMode::FirstReady;
            }
            mode = shared::model::HlsStartupMode::FirstReady;
            debug!("HLS startup fallback: reason=replay_admission effective_mode=first_ready");
        }
    }
    let raw = mode == shared::model::HlsStartupMode::Progressive;
    let kind = if raw { super::super::SegmentRevisionKind::Raw } else { super::super::SegmentRevisionKind::Processed };
    let revision =
        store.create(session_id, snapshot.proxy_seq, kind).map_err(|error| SegmentFetchError::cache_body(&error))?;
    let body_bound = expected.unwrap_or(limit).max(1);
    let peak = body_bound
        .checked_mul(revision_peak_factor(raw))
        .ok_or_else(|| SegmentFetchError::cache_body(&std::io::Error::other("revision peak overflow")))?;
    let mut reservation = match context.segment_cache.reserve_revision_peak(&revision.revision().key, peak).await {
        Ok(reservation) => reservation,
        Err(_) if raw && fallback_to_first_ready(&context.session).await => {
            debug!("HLS startup fallback: reason=disk_capacity effective_mode=first_ready");
            return Ok(None);
        }
        Err(error) => return Err(SegmentFetchError::cache_body(&error)),
    };
    let path = context.segment_cache.object_path(&revision.revision().key);
    let file = if raw {
        let (file, pin) = context
            .segment_cache
            .create_revision_spool(&revision.revision().key)
            .await
            .map_err(|error| SegmentFetchError::cache_body(&error))?;
        *revision.revision().file_pin.lock_unpoisoned() = Some(pin);
        *revision.revision().replay.lock_unpoisoned() = replay.take();
        reservation.mark_unaccounted_writes();
        Some(file)
    } else {
        let pin =
            context.segment_cache.pin_revision_file(&path).map_err(|error| SegmentFetchError::cache_body(&error))?;
        *revision.revision().file_pin.lock_unpoisoned() = Some(pin);
        None
    };
    bind_startup_revision(context, snapshot, &revision).await?;
    debug!("HLS startup fill: mode={mode} proxy_seq={} representation={kind:?}", snapshot.proxy_seq);
    let observed = Arc::new(tokio::sync::Mutex::new(HlsObservedFill {
        revision: revision.clone(),
        session: Arc::clone(&context.session),
        context: context.clone(),
        file,
        bytes: 0,
        expected,
        limit: body_bound,
        path,
        generation: snapshot.origin_work_generation,
        budget,
        deadline,
        repair_permit: None,
        additional_reservations: Vec::new(),
    }));
    Ok(Some(PreparedStartupFill { revision, observed, _reservation: reservation, raw }))
}

async fn bind_startup_revision(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    revision: &super::super::SegmentRevisionGuard,
) -> Result<(), SegmentFetchError> {
    let mut session = context.session.write().await;
    if session.activity.origin_work_generation != snapshot.origin_work_generation
        || !session
            .segments
            .get(&snapshot.proxy_seq)
            .is_some_and(|entry| segment_fetch_binding_matches(entry, snapshot))
    {
        return Err(SegmentFetchError::cache_body(&std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "startup fetch generation retired",
        )));
    }
    if let Some(startup) = &mut session.startup {
        startup.policy_fixed = true;
        startup.revisions.insert(snapshot.proxy_seq, revision.clone());
    }
    Ok(())
}

async fn fallback_to_first_ready(session: &HlsSessionHandle) -> bool {
    let mut session = session.write().await;
    if session.published_live_origin_baseline.is_some() {
        return false;
    }
    let Some(startup) = session.startup.as_mut().filter(|startup| !startup.policy_fixed) else {
        return false;
    };
    startup.config.mode = shared::model::HlsStartupMode::FirstReady;
    true
}
