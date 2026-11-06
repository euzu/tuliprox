//! Content evidence for query changes and same-host origin restarts.
//! Probes use the existing resource runner and cache admission. No session lock
//! is held during I/O; the captured basis is checked again at atomic commit.

use super::{commit::materialize_normal_key_resources, OriginRefreshRequest};
use crate::{
    manifest_fetch::{
        fetched_effective_manifest_host, FetchedOriginManifest, HlsManifestCommitAcceptanceMode,
        HlsManifestCommitError, HlsManifestRejectLogReason,
    },
    resource_identity::HlsMediaResourceIdentity,
    segment_fetcher::{stage_hls_switch_resource, HlsSwitchResourceKind},
    sync_ext::MutexExt,
    timeline::{effective_origin_host_id, HlsOriginHandoffPreview},
    CachedSegmentMetadata, HlsSession, OriginSegmentFetchRef, OriginSegmentKey, SegmentCacheKey, SegmentCacheStatus,
    SegmentEntry, SegmentRevisionGuard, SegmentRevisionKind,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};
use tuliprox_parser::hls::origin_manifest::{
    parse_origin_media_manifest, OriginManifestParseOutcome, ParsedOriginManifest, ParsedOriginSegment,
};

const CONTENT_EVIDENCE_LIMIT: usize = 1_024;
const PROBE_SEGMENT_LIMIT: usize = crate::manifest_acceptance::HLS_MANIFEST_FINGERPRINT_SEGMENT_LIMIT;

#[derive(Clone, Debug)]
struct OriginContentRecord {
    origin_key: OriginSegmentKey,
    cache_key: SegmentCacheKey,
    identity: HlsMediaResourceIdentity,
    generation: u64,
    digest: [u8; 32],
    ready_revision: Option<(u64, u64)>,
}

/// One successful READY commit, including its original, pre-processing bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HlsOriginContentAnchor {
    pub(crate) proxy_seq: u64,
    pub(crate) origin_key: OriginSegmentKey,
    pub(crate) cache_key: SegmentCacheKey,
    pub(crate) identity: HlsMediaResourceIdentity,
    pub(crate) digest: [u8; 32],
    generation: u64,
    ready_revision: (u64, u64),
}

fn ready_revision(entry: &SegmentEntry) -> Option<(u64, u64)> {
    match entry.status {
        SegmentCacheStatus::Ready { content_length, ready_at_ms } => Some((content_length, ready_at_ms)),
        _ => None,
    }
}

#[derive(Debug, Default)]
pub(crate) struct HlsOriginContentEvidence {
    records: BTreeMap<u64, OriginContentRecord>,
    /// Retained slots whose rendered discontinuity was inserted by the proxy.
    pub(crate) proxy_only_discontinuities: BTreeSet<u64>,
    pub(super) probes: super::anchor_probe::HlsAnchorProbeEvidence,
}

impl HlsOriginContentEvidence {
    pub(crate) fn record(&mut self, entry: &SegmentEntry, generation: u64, digest: [u8; 32]) {
        let Some(identity) = entry.media_resource_identity() else { return };
        self.records.insert(
            entry.proxy_seq,
            OriginContentRecord {
                origin_key: entry.origin_key,
                cache_key: entry.cache_key.clone(),
                identity,
                generation,
                digest,
                ready_revision: ready_revision(entry),
            },
        );
        while self.records.len() > CONTENT_EVIDENCE_LIMIT {
            self.records.pop_first();
        }
    }

    /// Discovery, in-flight fetches and staged candidates cannot advance this reference.
    /// If the newest READY medium has no usable raw proof, do not substitute an older one.
    pub(crate) fn latest_ready(session: &HlsSession) -> Option<HlsOriginContentAnchor> {
        let entry =
            session.segments.values().rev().find(|entry| {
                entry.origin_key.origin_epoch == session.origin_epoch && ready_revision(entry).is_some()
            })?;
        if entry.map_ref.is_some() || entry.encryption.is_some() || !entry.proxy_file_ext.eq_ignore_ascii_case("ts") {
            return None;
        }
        let digest = session.origin_content.digest(entry, session.activity.origin_work_generation)?;
        let record = session.origin_content.records.get(&entry.proxy_seq)?;
        Some(HlsOriginContentAnchor {
            proxy_seq: entry.proxy_seq,
            origin_key: entry.origin_key,
            cache_key: entry.cache_key.clone(),
            identity: record.identity,
            digest,
            generation: record.generation,
            ready_revision: ready_revision(entry)?,
        })
    }

    pub(super) fn digest(&self, entry: &SegmentEntry, generation: u64) -> Option<[u8; 32]> {
        self.records
            .get(&entry.proxy_seq)
            .filter(|record| {
                record.origin_key == entry.origin_key
                    && record.cache_key == entry.cache_key
                    && entry.media_resource_identity() == Some(record.identity)
                    && record.generation <= generation
                    && record.ready_revision == ready_revision(entry)
            })
            .map(|record| record.digest)
    }
}

/// Observes the exact decoded origin representation, before repair or remux.
/// A digest exists only after EOF; cancellation and partial bodies prove nothing.
#[derive(Clone, Default)]
pub(crate) struct OriginContentDigest(Arc<Mutex<(Sha256, bool)>>);

impl OriginContentDigest {
    pub(crate) fn observe(
        &self,
        body: tuliprox_core::utils::network::DynReader,
    ) -> tuliprox_core::utils::network::DynReader {
        Box::pin(DigestReader { body, digest: self.clone() })
    }

    pub(crate) fn completed(&self) -> Option<[u8; 32]> {
        let state = self.0.lock_unpoisoned();
        state.1.then(|| state.0.clone().finalize().into())
    }
}

struct DigestReader {
    body: tuliprox_core::utils::network::DynReader,
    digest: OriginContentDigest,
}

impl AsyncRead for DigestReader {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buffer.filled().len();
        match self.body.as_mut().poll_read(cx, buffer) {
            Poll::Ready(Ok(())) => {
                let bytes = &buffer.filled()[before..];
                let mut state = self.digest.0.lock_unpoisoned();
                state.0.update(bytes);
                if bytes.is_empty() && buffer.remaining() > 0 {
                    state.1 = true;
                }
                Poll::Ready(Ok(()))
            }
            result => result,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProbeBasis {
    generation: u64,
    account: u64,
    epoch: u64,
    next: Option<u64>,
    commits: u64,
    readiness: u64,
    revalidation: Option<u64>,
}

impl ProbeBasis {
    fn capture(session: &HlsSession) -> Self {
        Self {
            generation: session.activity.origin_work_generation,
            account: session.transient.origin_account_generation(),
            epoch: session.origin_epoch,
            next: session.proxy_next_seq,
            commits: session.origin_manifest_commits(),
            readiness: session.activity.media_readiness_generation,
            revalidation: session.live_entry_revalidation_generation(),
        }
    }
}

struct PreparedMedia {
    entry: SegmentEntry,
    metadata: CachedSegmentMetadata,
    digest: [u8; 32],
    revision: Option<SegmentRevisionGuard>,
    cleanup: crate::gc::HlsSwitchCacheCleanupReservation,
}

struct PreparedQuery {
    entry: SegmentEntry,
    fetch_ref: OriginSegmentFetchRef,
    digest: Option<[u8; 32]>,
}

impl PreparedQuery {
    fn new(entry: SegmentEntry, parsed: &ParsedOriginSegment, digest: Option<[u8; 32]>) -> Self {
        Self {
            entry,
            fetch_ref: OriginSegmentFetchRef {
                resolved_origin_url: parsed.resolved_origin_url.clone(),
                byte_range: parsed.origin_byte_range,
                valid_until_ms: None,
            },
            digest,
        }
    }

    fn is_current(&self, session: &HlsSession) -> bool {
        session.segments.get(&self.entry.proxy_seq).is_some_and(|current| {
            current.origin_key == self.entry.origin_key
                && current.cache_key == self.entry.cache_key
                && current.status == self.entry.status
                && current.origin_fetch_ref == self.entry.origin_fetch_ref
                && current.retained_resource_identity == self.entry.retained_resource_identity
                && current.map_ref == self.entry.map_ref
                && current.encryption == self.entry.encryption
        })
    }
}

pub(super) struct PreparedOriginUpdate {
    basis: ProbeBasis,
    host: u64,
    queries: Vec<PreparedQuery>,
    restart: Option<HlsOriginHandoffPreview>,
    media: Vec<PreparedMedia>,
    dependencies: Vec<BoundDependency>,
    _reference_pins: Vec<super::commit::HlsCommittedAcceptanceReadPin>,
}

impl PreparedOriginUpdate {
    pub(super) fn is_restart(&self) -> bool { self.restart.is_some() }

    pub(super) fn apply(
        &self,
        session: &mut HlsSession,
        manifest: &ParsedOriginManifest,
    ) -> Result<(), HlsManifestCommitError> {
        if ProbeBasis::capture(session) != self.basis || !self.dependencies.iter().all(|item| item.is_current(session))
        {
            return Err(invalidated());
        }
        if let Some(preview) = &self.restart {
            session
                .apply_verified_restart(manifest, self.host, preview)
                .map_err(super::switch_staging::handoff_preview_error)?;
            for media in &self.media {
                let Some(entry) = session.segments.get_mut(&media.entry.proxy_seq) else { return Err(invalidated()) };
                entry.status = SegmentCacheStatus::Ready {
                    content_length: media.metadata.size,
                    ready_at_ms: tuliprox_core::utils::current_time_millis(),
                };
                session.origin_content.record(entry, self.basis.generation, media.digest);
                if let Some(revision) = &media.revision {
                    revision.revision().bind_origin(session);
                    if let Some(startup) = &mut session.startup {
                        startup.revisions.insert(media.entry.proxy_seq, revision.clone());
                    }
                }
            }
            session.advance_media_readiness_generation();
            log::debug!(
                "HLS origin media verification: decision=restart-committed origin_epoch={} segments={}",
                session.origin_epoch,
                self.media.len()
            );
        } else {
            if !self.queries.iter().all(|query| query.is_current(session)) {
                return Err(invalidated());
            }
            let updates =
                self.queries.iter().map(|query| (query.entry.proxy_seq, query.fetch_ref.clone())).collect::<Vec<_>>();
            session
                .apply_verified_query_manifest(manifest, self.host, &updates)
                .map_err(|error| HlsManifestCommitError::TimelineRejected { reason: error.into() })?;
            let mut verified = 0;
            for query in &self.queries {
                let seq = query.entry.proxy_seq;
                if let Some(entry) = session.segments.get_mut(&seq) {
                    entry.retained_resource_identity = None;
                    if let Some(digest) = query.digest {
                        session.origin_content.record(entry, self.basis.generation, digest);
                    }
                }
                if query.digest.is_some() {
                    verified += 1;
                    if let Some(revision) = session.startup.as_ref().and_then(|startup| startup.revisions.get(&seq)) {
                        revision.revision().bind_origin(session);
                    }
                }
            }
            log::debug!(
                "HLS origin media verification: decision=query-update verified={verified} rebound={}",
                self.queries.len() - verified
            );
        }
        Ok(())
    }

    pub(super) fn disarm_cleanup(&mut self) {
        for media in &mut self.media {
            media.cleanup.disarm();
        }
    }
}

fn unavailable() -> HlsManifestCommitError {
    super::switch_staging::switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable)
}
fn invalidated() -> HlsManifestCommitError {
    super::switch_staging::switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated)
}

pub(super) async fn commit_pinned_manifest(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
    mode: HlsManifestCommitAcceptanceMode,
) -> Result<(super::commit::HlsManifestCommitProgressEvidence, bool, bool), HlsManifestCommitError> {
    let mut update = prepare_origin_update(request, fetched, mode).await?;
    let result = {
        let mut session = request.session.write().await;
        if update.is_none() && mode == request.manifest_commit_requirement.acceptance_mode() {
            super::commit::commit_fetched_manifest(&mut session, fetched, request, request.hls_proxy.now_ms())
        } else {
            super::commit::commit_fetched_manifest_with_origin_update(
                &mut session,
                fetched,
                request,
                request.hls_proxy.now_ms(),
                mode,
                None,
                update.as_ref(),
            )
        }
    };
    if result.is_ok() {
        if let Some(update) = &mut update {
            update.disarm_cleanup();
        }
    }
    result
}

pub(super) async fn prepare_origin_update(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
    mode: HlsManifestCommitAcceptanceMode,
) -> Result<Option<PreparedOriginUpdate>, HlsManifestCommitError> {
    tokio::time::timeout(Duration::from_millis(request.origin_manifest_timeout_ms), prepare(request, fetched, mode))
        .await
        .map_err(|_| unavailable())?
}

type StartupContext = Option<(Arc<crate::SegmentRevisionStore>, shared::model::HlsStartupMode)>;

struct ProbeInputs {
    basis: ProbeBasis,
    references: Vec<(Option<SegmentEntry>, Option<[u8; 32]>)>,
    preview: HlsOriginHandoffPreview,
    startup: StartupContext,
    base: SegmentEntry,
    known_content: Vec<[u8; 32]>,
    _pins: Vec<super::commit::HlsCommittedAcceptanceReadPin>,
}

enum OriginProbePlan {
    None,
    QueryRotation(BoundQueryInputs),
    Restart(Box<ProbeInputs>),
}

fn restart_authorized(
    session: &HlsSession,
    request: &OriginRefreshRequest,
    mode: HlsManifestCommitAcceptanceMode,
) -> bool {
    matches!(
        mode,
        HlsManifestCommitAcceptanceMode::FreshPinnedRevalidation | HlsManifestCommitAcceptanceMode::FreshBaseline
    ) || request.manifest_commit_requirement.fresh_reason()
        == Some(crate::HlsFreshManifestRequiredReason::StaleEntryWindow)
        || session.origin_control.acceptance_episode.as_ref().is_some_and(|episode| {
            // A short observed outage can recover before reserve pressure or
            // terminal publication. Its completed burst still needs the same
            // fresh byte proof; a healthy Observe episode authorizes nothing.
            episode.full_burst_completed
                && (matches!(
                    episode.trigger(),
                    crate::manifest_acceptance::HlsManifestAcceptanceTrigger::RecoveryRequired
                        | crate::manifest_acceptance::HlsManifestAcceptanceTrigger::Critical
                ) || (episode.trigger() == crate::manifest_acceptance::HlsManifestAcceptanceTrigger::Observe
                    && session.origin_refresh.consecutive_failures > 0))
        })
}

fn current_references(session: &HlsSession, manifest: &ParsedOriginManifest) -> Vec<Option<SegmentEntry>> {
    manifest
        .segments
        .iter()
        .map(|parsed| {
            session
                .segments
                .values()
                .rev()
                .find(|entry| {
                    entry.origin_key.origin_epoch == session.origin_epoch
                        && entry.origin_key.host_local_sequence == parsed.origin_seq
                })
                .cloned()
        })
        .collect::<Vec<_>>()
}

fn is_unpublished_bootstrap_repeat(
    session: &HlsSession,
    manifest: &ParsedOriginManifest,
    references: &[Option<SegmentEntry>],
) -> bool {
    // A failed initial admission may have parsed a window without publishing or
    // caching any media. Repeating those exact bindings is still bootstrap,
    // rather than a restart claiming new media from an old READY object.
    session.published_live_origin_baseline.is_none()
        && manifest.segments.iter().zip(references).all(|(parsed, reference)| {
            reference.as_ref().is_some_and(|entry| {
                !matches!(entry.status, SegmentCacheStatus::Ready { .. })
                    && entry.media_resource_identity()
                        == Some(HlsMediaResourceIdentity::from_url(
                            &parsed.resolved_origin_url,
                            parsed.origin_byte_range,
                        ))
            })
        })
}

fn ordinary_stale_entry(
    request: &OriginRefreshRequest,
    session: &HlsSession,
    mode: HlsManifestCommitAcceptanceMode,
) -> bool {
    // Recent playback distinguishes an ordinary freshness check from re-entry
    // after inactivity. Use the existing account overlap window for that test.
    let now_ms = request.hls_proxy.now_ms();
    let active_recovery = session.origin_control.acceptance_episode.as_ref().is_some_and(|episode| {
        episode.state != crate::manifest_acceptance::HlsManifestAcceptanceState::Completed
            && episode.trigger().recovery_required()
    });
    mode == HlsManifestCommitAcceptanceMode::StrictPinnedHost
        && request.manifest_commit_requirement.fresh_reason()
            == Some(crate::HlsFreshManifestRequiredReason::StaleEntryWindow)
        && !request.acceptance_directive.trigger.recovery_required()
        && !active_recovery
        && session.origin_account_manifest_pending.is_none()
        && !session.live_entry_revalidation.required
        && !session.origin_control.path_is_degraded_at(now_ms)
        && session.origin_refresh.consecutive_failures == 0
        && !matches!(
            crate::classify_account_binding_protection(
                session.activity.last_authorized_media_at_ms,
                now_ms,
                &session.account_overlap_timing()
            ),
            crate::HlsAccountBindingProtection::Expired
        )
}

fn identical_live_window(
    session: &HlsSession,
    manifest: &ParsedOriginManifest,
    references: &[Option<SegmentEntry>],
) -> bool {
    let previous = session.live_origin_sequences();
    manifest.segments.last().map(|last| last.origin_seq) == session.origin_seq_highwater
        && previous.len() == manifest.segments.len()
        && manifest.segments.iter().zip(references).zip(previous).all(|((parsed, reference), seq)| {
            reference.as_ref().is_some_and(|entry| {
                entry.proxy_seq == *seq
                    && entry.origin_key.effective_host_id == session.origin_epoch_effective_host_id.unwrap_or_default()
                    && entry.map_ref.is_none()
                    && entry.encryption.is_none()
                    && same_media_timing(session, entry, parsed)
                    && entry.media_resource_identity()
                        == Some(HlsMediaResourceIdentity::from_url(
                            &parsed.resolved_origin_url,
                            parsed.origin_byte_range,
                        ))
            })
        })
}

fn same_media_timing(session: &HlsSession, entry: &SegmentEntry, parsed: &ParsedOriginSegment) -> bool {
    entry.duration_ms == parsed.duration_ms
        && session.origin_discontinuity_before(entry) == parsed.discontinuity_before
        && entry.program_date_time == parsed.program_date_time
        && entry.daterange_tags_before == parsed.daterange_tags_before
}

fn latest_clear_ready(session: &HlsSession) -> Option<&SegmentEntry> {
    session.segments.values().rev().find(|entry| {
        entry.origin_key.origin_epoch == session.origin_epoch
            && entry.map_ref.is_none()
            && entry.encryption.is_none()
            && matches!(entry.status, SegmentCacheStatus::Ready { .. })
    })
}

fn capture_probe_inputs(
    request: &OriginRefreshRequest,
    session: &HlsSession,
    manifest: &mut ParsedOriginManifest,
    mode: HlsManifestCommitAcceptanceMode,
    host: u64,
) -> Result<OriginProbePlan, HlsManifestCommitError> {
    if manifest.segments.iter().any(|segment| segment.map_ref.is_some() || segment.encryption.is_some()) {
        return bound_query_inputs(request, session, manifest, host, false);
    }
    if session.origin_seq_highwater.is_none()
        || session.segments.is_empty()
        || session.origin_epoch_effective_host_id != Some(host)
    {
        return Ok(OriginProbePlan::None);
    }
    let references = current_references(session, manifest);
    if is_unpublished_bootstrap_repeat(session, manifest, &references)
        || (ordinary_stale_entry(request, session, mode) && identical_live_window(session, manifest, &references))
    {
        return Ok(OriginProbePlan::None);
    }
    let query_changed = manifest.segments.iter().zip(&references).any(|(parsed, reference)| {
        reference.as_ref().and_then(SegmentEntry::media_resource_identity).is_some_and(|old| {
            let new = HlsMediaResourceIdentity::from_url(&parsed.resolved_origin_url, parsed.origin_byte_range);
            old.same_path_and_range(new) && !old.matches(new)
        })
    });
    let restart_allowed = restart_authorized(session, request, mode);
    if !restart_allowed
        && session.published_live_origin_baseline.is_some()
        && manifest.segments.last().is_some_and(|last| Some(last.origin_seq) < session.origin_seq_highwater)
    {
        return Err(unavailable());
    }
    let reset = restart_allowed
        && !session.origin_qualification_awaits_publication()
        && manifest.segments.last().is_some_and(|last| Some(last.origin_seq) <= session.origin_seq_highwater);
    if !reset {
        return if query_changed {
            bound_query_inputs(request, session, manifest, host, true)
        } else {
            Ok(OriginProbePlan::None)
        };
    }
    if manifest.segments.len() > PROBE_SEGMENT_LIMIT {
        return Err(unavailable());
    }
    materialize_normal_key_resources(
        manifest,
        &request.reverse_proxy_rewrite_secret,
        request.now_ms,
        request.transient_resource_ttl_ms,
        session.transient.origin_account_generation(),
    );
    // Bindings are compared before any content-equivalence decision. This
    // path intentionally cannot infer equality across changed MAPs or keys.
    if manifest.segments.iter().any(|segment| segment.map_ref.is_some() || segment.encryption.is_some()) {
        return Err(unavailable());
    }
    let base = latest_clear_ready(session).cloned().ok_or_else(unavailable)?;
    let mut pins = references
        .iter()
        .flatten()
        .filter(|entry| matches!(entry.status, SegmentCacheStatus::Ready { .. }))
        .map(|entry| super::commit::HlsCommittedAcceptanceReadPin::acquire(Arc::clone(&entry.access), request.now_ms))
        .collect::<Vec<_>>();
    pins.push(super::commit::HlsCommittedAcceptanceReadPin::acquire(Arc::clone(&base.access), request.now_ms));
    let preview =
        session.preview_verified_restart(manifest, host).map_err(super::switch_staging::handoff_preview_error)?;
    let references = references
        .into_iter()
        .map(|entry| {
            let digest = entry
                .as_ref()
                .and_then(|entry| session.origin_content.digest(entry, session.activity.origin_work_generation));
            (entry, digest)
        })
        .collect::<Vec<_>>();
    let startup = session.startup.as_ref().map(|startup| (Arc::clone(&startup.store), startup.config.mode));
    let known_content = session
        .segments
        .values()
        .filter(|entry| entry.origin_key.origin_epoch == session.origin_epoch)
        .filter_map(|entry| session.origin_content.digest(entry, session.activity.origin_work_generation))
        .collect();
    Ok(OriginProbePlan::Restart(Box::new(ProbeInputs {
        basis: ProbeBasis::capture(session),
        references,
        preview,
        startup,
        base,
        known_content,
        _pins: pins,
    })))
}

async fn prepare(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
    mode: HlsManifestCommitAcceptanceMode,
) -> Result<Option<PreparedOriginUpdate>, HlsManifestCommitError> {
    let OriginManifestParseOutcome::Normal(mut manifest) =
        parse_origin_media_manifest(&fetched.body, &fetched.final_manifest_url)
    else {
        return Ok(None);
    };
    let host = fetched_effective_manifest_host(fetched).map_or(0, |host| effective_origin_host_id(&host));
    let plan = {
        let session = request.session.read().await;
        super::commit::ensure_refresh_origin_work_generation_is_current(&session, request)?;
        capture_probe_inputs(request, &session, &mut manifest, mode, host)?
    };
    let inputs = match plan {
        OriginProbePlan::None => return Ok(None),
        OriginProbePlan::QueryRotation(inputs) => {
            return prepare_bound_queries(request, fetched, host, inputs).await.map(Some);
        }
        OriginProbePlan::Restart(inputs) => inputs,
    };
    let ProbeInputs { basis, references, preview, startup, base, known_content, _pins: pins } = *inputs;
    let _permit =
        request.segment_worker_pool.try_reserve_origin_probe(&request.session).await.ok_or_else(unavailable)?;
    let base_tracks = super::switch_staging::inspect_cache_object_tracks(&request.segment_cache, &base.cache_key)
        .await
        .signature()
        .cloned()
        .ok_or_else(unavailable)?;
    let context = super::switch_staging::alternative_switch_fetch_context(request, fetched);
    let policy = request.segment_worker_pool.policy();
    let mut queries = Vec::new();
    let mut staged = Vec::new();
    let mut changed = false;
    log::debug!(
        "HLS origin media verification: decision=probe-start restart_candidate=true segments={}",
        manifest.segments.len()
    );
    for (index, (parsed, (reference, digest))) in manifest.segments.iter().zip(&references).enumerate() {
        let key = preview.segments.get(index).map(|entry| entry.cache_key.clone()).ok_or_else(unavailable)?;
        let object = stage_hls_switch_resource(
            &context,
            &policy,
            key,
            parsed.resolved_origin_url.clone(),
            parsed.origin_byte_range,
            HlsSwitchResourceKind::Segment,
        )
        .await
        .map_err(|_| unavailable())?;
        let hash = sha256_file(&object.path).await?;
        ensure_compatible_tracks(&object.path, &base_tracks).await?;
        let comparison = compare_probe(reference.as_ref(), *digest, parsed, hash, &known_content);
        if let Some(query) = comparison.query {
            queries.push(query);
        }
        changed |= comparison.new_media;
        staged.push((index, object, hash));
    }
    if !changed {
        // Replayed HTTP 200 windows must consume the remaining existing budget.
        let session = request.session.read().await;
        if session.live_entry_revalidation.required
            || session
                .origin_control
                .acceptance_episode
                .as_ref()
                .is_some_and(|episode| episode.trigger().recovery_required())
            || manifest.segments.last().map(|last| last.origin_seq) < session.origin_seq_highwater
        {
            log::debug!("HLS origin media verification: decision=unchanged-media");
            return Err(unavailable());
        }
        return Ok(Some(PreparedOriginUpdate {
            basis,
            host,
            queries,
            restart: None,
            media: Vec::new(),
            dependencies: Vec::new(),
            _reference_pins: pins,
        }));
    }
    let mut update = PreparedOriginUpdate {
        basis,
        host,
        queries,
        restart: Some(preview),
        media: Vec::new(),
        dependencies: Vec::new(),
        _reference_pins: pins,
    };
    if changed {
        write_restart_media(request, &mut update, &startup, staged).await?;
    }
    Ok(Some(update))
}

struct ProbeComparison {
    query: Option<PreparedQuery>,
    new_media: bool,
}

fn compare_probe(
    reference: Option<&SegmentEntry>,
    digest: Option<[u8; 32]>,
    parsed: &ParsedOriginSegment,
    hash: [u8; 32],
    known_content: &[[u8; 32]],
) -> ProbeComparison {
    let identity = HlsMediaResourceIdentity::from_url(&parsed.resolved_origin_url, parsed.origin_byte_range);
    let query_changed = reference
        .and_then(SegmentEntry::media_resource_identity)
        .is_some_and(|old| old.same_path_and_range(identity) && !old.matches(identity));
    let equal = reference.is_some_and(|entry| {
        entry.map_ref.is_none() && entry.encryption.is_none() && entry.duration_ms == parsed.duration_ms
    }) && digest == Some(hash);
    if equal {
        let query =
            reference.filter(|_| query_changed).map(|entry| PreparedQuery::new(entry.clone(), parsed, Some(hash)));
        return ProbeComparison { query, new_media: false };
    }
    let unverified_reused_url = digest.is_none()
        && reference
            .and_then(SegmentEntry::media_resource_identity)
            .is_some_and(|old| old.same_path_and_range(identity));
    // An uncached prefix contributes no restart proof. It can accompany a
    // restart proved by other media, because every candidate is freshly staged
    // under a distinct cache key; it never becomes an old-object cache hit.
    ProbeComparison { query: None, new_media: !unverified_reused_url && !known_content.contains(&hash) }
}

async fn write_restart_media(
    request: &OriginRefreshRequest,
    update: &mut PreparedOriginUpdate,
    startup: &StartupContext,
    staged: Vec<(usize, crate::StagedCacheObject, [u8; 32])>,
) -> Result<(), HlsManifestCommitError> {
    for (index, object, digest) in staged {
        let entry =
            update.restart.as_ref().and_then(|preview| preview.segments.get(index)).cloned().ok_or_else(invalidated)?;
        let cleanup =
            request.hls_proxy.reserve_switch_segment_cleanup(entry.cache_key.clone()).ok_or_else(unavailable)?;
        let deadline = tokio::time::Instant::now() + request.segment_repair.postprocess_timeout();
        let revision = if let Some((store, mode)) = startup {
            let kind =
                if mode.streams_raw_revisions() { SegmentRevisionKind::Raw } else { SegmentRevisionKind::Processed };
            let guard = store
                .create(entry.cache_key.proxy_session_id().clone(), entry.proxy_seq, kind)
                .map_err(|_| unavailable())?;
            if mode.streams_raw_revisions() {
                let metadata = request
                    .segment_cache
                    .publish_processed_revision(&guard.revision().key, &object.path, deadline)
                    .await
                    .map_err(|_| unavailable())?;
                *guard.revision().file_pin.lock_unpoisoned() =
                    Some(request.segment_cache.pin_revision_file(&metadata.path).map_err(|_| unavailable())?);
                guard.revision().complete(metadata);
            }
            Some(guard)
        } else {
            None
        };
        let metadata = request
            .segment_repair
            .commit_origin_response(
                &request.segment_cache,
                &entry.cache_key,
                tokio::fs::File::open(&object.path).await.map_err(|_| unavailable())?,
                deadline,
                crate::HlsSegmentRepairObjectContext {
                    source: crate::HlsSegmentRepairSource::Normal,
                    log_identity: {
                        let session = request.session.read().await;
                        crate::HlsLogIdentity::from_session(&session)
                    },
                    proxy_session_id: entry.cache_key.proxy_session_id().clone(),
                    hls_access_lease_id: request.access_lease_id.clone(),
                    rendered_object_id: crate::HlsRepairRenderedObjectId::Normal { proxy_seq: entry.proxy_seq },
                    resource_id: format!("{:06}", entry.proxy_seq),
                    file_ext: entry.proxy_file_ext.clone(),
                    origin_fetch_uri_for_diagnostics: parsed_url(&entry),
                    media_sequence: Some(entry.origin_key.host_local_sequence),
                    discontinuity_sequence: None,
                    complete_object: entry.origin_byte_range.is_none(),
                    encrypted: false,
                    custom_response: false,
                },
            )
            .await
            .map_err(|_| unavailable())?;
        if let Some(guard) = &revision {
            if !guard.revision().is_complete() {
                let processed = request
                    .segment_cache
                    .publish_processed_revision(&guard.revision().key, &metadata.path, deadline)
                    .await
                    .map_err(|_| unavailable())?;
                *guard.revision().file_pin.lock_unpoisoned() =
                    Some(request.segment_cache.pin_revision_file(&processed.path).map_err(|_| unavailable())?);
                guard.revision().complete(processed);
            }
        }
        update.media.push(PreparedMedia { entry, metadata, digest, revision, cleanup });
    }

    Ok(())
}

fn parsed_url(entry: &SegmentEntry) -> String {
    entry.origin_fetch_ref.as_ref().map_or_else(String::new, |fetch_ref| fetch_ref.resolved_origin_url.clone())
}

enum BoundDependency {
    Map(crate::MapEntry),
    Key { lookup: crate::TransientObjectCacheKey, key: crate::TransientObjectCacheKey, uri: String },
}

impl BoundDependency {
    fn is_current(&self, session: &HlsSession) -> bool {
        match self {
            Self::Map(map) => session.maps.get(&map.proxy_map_id).is_some_and(|current| {
                current.cache_key == map.cache_key
                    && current.origin_key == map.origin_key
                    && matches!(current.status, crate::MapCacheStatus::Ready { .. })
            }),
            Self::Key { lookup, key, .. } => session.transient.object_cache.get(lookup).is_some_and(|current| {
                current.key == *key && matches!(current.status, crate::TransientObjectCacheStatus::Ready { .. })
            }),
        }
    }

    fn same_object(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Map(left), Self::Map(right)) => left.cache_key == right.cache_key,
            (Self::Key { key: left, .. }, Self::Key { key: right, .. }) => left == right,
            _ => false,
        }
    }

    async fn verify(
        &self,
        request: &OriginRefreshRequest,
        context: &crate::SegmentFetchContext,
    ) -> Result<(), HlsManifestCommitError> {
        match self {
            Self::Map(map) => {
                verify_dependency(
                    request,
                    context,
                    map.cache_key.clone(),
                    map.origin_key.resolved_origin_uri.clone(),
                    map.byte_range,
                )
                .await
            }
            Self::Key { key, uri, .. } => verify_dependency(request, context, key.clone(), uri.clone(), None).await,
        }
    }
}

struct BoundQueryInputs {
    basis: ProbeBasis,
    queries: Vec<PreparedQuery>,
    clear_base: Option<SegmentCacheKey>,
    dependencies: Vec<BoundDependency>,
    pins: Vec<super::commit::HlsCommittedAcceptanceReadPin>,
}

fn query_dependencies(
    session: &HlsSession,
    entry: &SegmentEntry,
    parsed: &ParsedOriginSegment,
    manifest: &ParsedOriginManifest,
    now_ms: u64,
    verify_cached: bool,
    pins: &mut Vec<super::commit::HlsCommittedAcceptanceReadPin>,
) -> Result<Vec<BoundDependency>, HlsManifestCommitError> {
    let mut dependencies = Vec::new();
    match (entry.map_ref, parsed.map_ref) {
        (None, None) => {}
        (Some(current), Some(candidate)) => {
            let map = session.maps.get(&current).ok_or_else(unavailable)?;
            let candidate = manifest.maps.get(candidate).ok_or_else(unavailable)?;
            if map.origin_key.resolved_origin_uri != candidate.resolved_origin_uri
                || map.byte_range != candidate.byte_range
            {
                return Err(unavailable());
            }
            if verify_cached {
                if !matches!(map.status, crate::MapCacheStatus::Ready { .. }) {
                    return Err(unavailable());
                }
                pins.push(super::commit::HlsCommittedAcceptanceReadPin::acquire(Arc::clone(&map.access), now_ms));
                dependencies.push(BoundDependency::Map(map.clone()));
            }
        }
        _ => return Err(unavailable()),
    }
    if crate::timeline::map_segment_encryption(parsed).map_err(|_| unavailable())? != entry.encryption {
        return Err(unavailable());
    }
    if let Some(encryption) = &entry.encryption {
        let candidate = parsed.encryption.as_ref().ok_or_else(unavailable)?;
        let resource = session
            .transient
            .resources
            .get(&encryption.resource_id)
            .filter(|resource| {
                resource.resolved_origin_uri == candidate.resolved_origin_uri
                    && (!verify_cached || resource.is_valid_at(now_ms))
            })
            .ok_or_else(unavailable)?;
        if !verify_cached {
            return Ok(dependencies);
        }
        if session
            .transient
            .ready_key_object_valid_until_ms(
                &session.proxy_session_id,
                &encryption.resource_id,
                &encryption.resource_extension,
                now_ms,
            )
            .is_none()
        {
            return Err(unavailable());
        }
        let key = crate::TransientPassthroughState::transient_object_key(
            &session.proxy_session_id,
            &encryption.resource_id,
            encryption.resource_extension.clone(),
        );
        let object = session.transient.object_cache.get(&key).ok_or_else(unavailable)?;
        pins.push(super::commit::HlsCommittedAcceptanceReadPin::acquire(Arc::clone(&object.access), now_ms));
        pins.push(super::commit::HlsCommittedAcceptanceReadPin::acquire(Arc::clone(&resource.access), now_ms));
        dependencies.push(BoundDependency::Key {
            lookup: key,
            key: object.key.clone(),
            uri: candidate.resolved_origin_uri.clone(),
        });
    }
    Ok(dependencies)
}

fn bound_query_inputs(
    request: &OriginRefreshRequest,
    session: &HlsSession,
    manifest: &mut ParsedOriginManifest,
    host: u64,
    clear_ts: bool,
) -> Result<OriginProbePlan, HlsManifestCommitError> {
    if session.origin_seq_highwater.is_none() || session.origin_epoch_effective_host_id != Some(host) {
        return Ok(OriginProbePlan::None);
    }
    if manifest.segments.last().map(|segment| segment.origin_seq) < session.origin_seq_highwater {
        return Err(unavailable());
    }
    materialize_normal_key_resources(
        manifest,
        &request.reverse_proxy_rewrite_secret,
        request.now_ms,
        request.transient_resource_ttl_ms,
        session.transient.origin_account_generation(),
    );
    let mut inputs = BoundQueryInputs {
        basis: ProbeBasis::capture(session),
        queries: Vec::new(),
        clear_base: None,
        dependencies: Vec::new(),
        pins: Vec::new(),
    };
    for parsed in &manifest.segments {
        let Some(entry) = session.segments.values().rev().find(|entry| {
            entry.origin_key.origin_epoch == session.origin_epoch
                && entry.origin_key.effective_host_id == host
                && entry.origin_key.host_local_sequence == parsed.origin_seq
        }) else {
            continue;
        };
        let old = entry.media_resource_identity().ok_or_else(unavailable)?;
        let new = HlsMediaResourceIdentity::from_url(&parsed.resolved_origin_url, parsed.origin_byte_range);
        if old.matches(new) {
            continue;
        }
        if !old.same_path_and_range(new)
            || matches!(entry.status, SegmentCacheStatus::FailedPermanent { .. } | SegmentCacheStatus::Expired)
            || !same_media_timing(session, entry, parsed)
        {
            log::debug!("HLS origin media verification: decision=unverified reason=resource-binding");
            return Err(unavailable());
        }
        let verify_cached = matches!(entry.status, SegmentCacheStatus::Ready { .. });
        let digest = if verify_cached {
            let digest =
                session.origin_content.digest(entry, session.activity.origin_work_generation).ok_or_else(|| {
                    log::debug!("HLS origin media verification: decision=unverified reason=missing-origin-digest");
                    unavailable()
                })?;
            inputs
                .pins
                .push(super::commit::HlsCommittedAcceptanceReadPin::acquire(Arc::clone(&entry.access), request.now_ms));
            Some(digest)
        } else {
            None
        };
        let dependencies =
            query_dependencies(session, entry, parsed, manifest, request.now_ms, verify_cached, &mut inputs.pins)
                .inspect_err(|_| {
                    log::debug!("HLS origin media verification: decision=unverified reason=media-dependency");
                })?;
        for dependency in dependencies {
            if !inputs.dependencies.iter().any(|current| current.same_object(&dependency)) {
                inputs.dependencies.push(dependency);
            }
        }
        inputs.queries.push(PreparedQuery::new(entry.clone(), parsed, digest));
    }
    let verified = inputs.queries.iter().filter(|query| query.digest.is_some()).count();
    let probe_size = if clear_ts { manifest.segments.len() } else { verified };
    if (!clear_ts && session.live_entry_revalidation.required) || (verified > 0 && probe_size > PROBE_SEGMENT_LIMIT) {
        log::debug!("HLS origin media verification: decision=unverified reason=revalidation-or-probe-limit");
        return Err(unavailable());
    }
    if clear_ts && verified > 0 {
        let base = latest_clear_ready(session).ok_or_else(unavailable)?;
        inputs
            .pins
            .push(super::commit::HlsCommittedAcceptanceReadPin::acquire(Arc::clone(&base.access), request.now_ms));
        inputs.clear_base = Some(base.cache_key.clone());
    }
    Ok(if inputs.queries.is_empty() { OriginProbePlan::None } else { OriginProbePlan::QueryRotation(inputs) })
}

async fn prepare_bound_queries(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
    host: u64,
    inputs: BoundQueryInputs,
) -> Result<PreparedOriginUpdate, HlsManifestCommitError> {
    if inputs.queries.iter().any(|query| query.digest.is_some()) {
        verify_bound_queries(request, fetched, &inputs).await?;
    }
    Ok(PreparedOriginUpdate {
        basis: inputs.basis,
        host,
        queries: inputs.queries,
        restart: None,
        media: Vec::new(),
        dependencies: inputs.dependencies,
        _reference_pins: inputs.pins,
    })
}

async fn verify_bound_queries(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
    inputs: &BoundQueryInputs,
) -> Result<(), HlsManifestCommitError> {
    let _permit =
        request.segment_worker_pool.try_reserve_origin_probe(&request.session).await.ok_or_else(unavailable)?;
    let context = super::switch_staging::alternative_switch_fetch_context(request, fetched);
    let base_tracks = if let Some(cache_key) = &inputs.clear_base {
        Some(
            super::switch_staging::inspect_cache_object_tracks(&request.segment_cache, cache_key)
                .await
                .signature()
                .cloned()
                .ok_or_else(unavailable)?,
        )
    } else {
        None
    };
    for dependency in &inputs.dependencies {
        dependency.verify(request, &context).await?;
    }
    for (query, digest) in inputs.queries.iter().filter_map(|query| Some((query, query.digest?))) {
        let object = stage_hls_switch_resource(
            &context,
            &request.segment_worker_pool.policy(),
            query.entry.cache_key.clone(),
            query.fetch_ref.resolved_origin_url.clone(),
            query.fetch_ref.byte_range,
            HlsSwitchResourceKind::Segment,
        )
        .await
        .map_err(|_| unavailable())?;
        if sha256_file(&object.path).await? != digest {
            return Err(unavailable());
        }
        if let Some(tracks) = &base_tracks {
            ensure_compatible_tracks(&object.path, tracks).await?;
        }
    }
    Ok(())
}

async fn verify_dependency<K: crate::HlsCacheObjectKey + Clone + Send + Sync + 'static>(
    request: &OriginRefreshRequest,
    context: &crate::SegmentFetchContext,
    key: K,
    uri: String,
    range: Option<tuliprox_parser::hls::origin_manifest::ParsedByteRange>,
) -> Result<(), HlsManifestCommitError> {
    let reference = request.segment_cache.metadata(&key).await.map_err(|_| unavailable())?.ok_or_else(unavailable)?;
    let candidate = stage_hls_switch_resource(
        context,
        &request.segment_worker_pool.policy(),
        key,
        uri,
        range,
        HlsSwitchResourceKind::Map,
    )
    .await
    .map_err(|_| unavailable())?;
    if sha256_file(&reference.path).await? != sha256_file(&candidate.path).await? {
        return Err(unavailable());
    }
    Ok(())
}

async fn ensure_compatible_tracks(
    path: &std::path::Path,
    base_tracks: &crate::HlsTsTrackSignature,
) -> Result<(), HlsManifestCommitError> {
    let tracks: crate::HlsTrackEvidenceResolution = crate::inspect_mpeg_ts_async(
        tokio::fs::File::open(path).await.map_err(|_| unavailable())?,
        crate::HlsTsProbeProtection::Clear,
        crate::HlsTsProbeBudget::default(),
    )
    .await
    .into();
    if tracks.signature() != Some(base_tracks) {
        return Err(unavailable());
    }
    Ok(())
}

pub(super) async fn sha256_file(path: &std::path::Path) -> Result<[u8; 32], HlsManifestCommitError> {
    let mut file = tokio::fs::File::open(path).await.map_err(|_| unavailable())?;
    let mut hasher = Sha256::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut chunk).await.map_err(|_| unavailable())?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn partial_origin_body_never_proves_content_equivalence() {
        let digest = OriginContentDigest::default();
        let mut reader = digest.observe(Box::pin(std::io::Cursor::new(b"unprocessed origin bytes")));
        let mut prefix = [0; 3];
        reader.read_exact(&mut prefix).await.unwrap();
        assert!(digest.completed().is_none());
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).await.unwrap();
        assert_eq!(digest.completed(), Some(Sha256::digest(b"unprocessed origin bytes").into()));
    }
}
