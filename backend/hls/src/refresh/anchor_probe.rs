//! Resource-only sharing for a single comparison round; final negative evidence
//! belongs to the session and is independent of retry/acceptance generations.
use super::{origin_probe::HlsOriginContentAnchor, OriginRefreshRequest};
use crate::{
    manifest_acceptance::HlsManifestRecoveryCandidateIdentity, sync_ext::MutexExt, HlsSession, SegmentFetchContext,
};
use futures::{
    future::{BoxFuture, Shared},
    FutureExt,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fmt,
    future::Future,
    sync::{Arc, Mutex, Weak},
};
use tokio_util::sync::CancellationToken;
use tuliprox_parser::hls::origin_manifest::ParsedByteRange;

type ProbeFuture = Shared<BoxFuture<'static, Result<[u8; 32], ()>>>;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ProbeIdentity {
    semantic: [u8; 32],
    anchor: HlsOriginContentAnchor,
    work_generation: u64,
    account_generation: u64,
    account_lease_generation: Option<u64>,
}

fn part(hash: &mut Sha256, bytes: &[u8]) {
    hash.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    hash.update(bytes);
}
fn headers(hash: &mut Sha256, values: &axum::http::HeaderMap) {
    let mut names = values.keys().collect::<Vec<_>>();
    names.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    hash.update(u64::try_from(names.len()).unwrap_or(u64::MAX).to_be_bytes());
    for name in names {
        part(hash, name.as_str().as_bytes());
        hash.update(u64::try_from(values.get_all(name).iter().count()).unwrap_or(u64::MAX).to_be_bytes());
        for value in values.get_all(name) {
            part(hash, value.as_bytes());
        }
    }
    part(hash, b"end-headers");
}
impl ProbeIdentity {
    pub(super) fn new(
        session: &HlsSession,
        anchor: &HlsOriginContentAnchor,
        context: &SegmentFetchContext,
        url: &str,
        range: Option<ParsedByteRange>,
    ) -> Result<Self, ()> {
        let mut hash = Sha256::new();
        part(&mut hash, b"tuliprox-hls-anchor-probe-binding-v1");
        part(&mut hash, session.proxy_session_id.0.as_bytes());
        for value in [anchor.proxy_seq, anchor.origin_key.origin_epoch, anchor.origin_key.host_local_sequence] {
            part(&mut hash, &value.to_be_bytes());
        }
        part(&mut hash, &anchor.digest);
        part(&mut hash, url.as_bytes()); // Exact resolved transport URL, never a sanitized/log identity.
        hash.update([u8::from(range.is_some())]);
        if let Some(range) = range {
            part(&mut hash, &range.offset.to_be_bytes());
            part(&mut hash, &range.length.to_be_bytes());
        }
        let effective_headers =
            crate::build_hls_origin_resource_headers(&context.headers, &context.origin_provider_session_headers, range)
                .map_err(|_| ())?;
        headers(&mut hash, &effective_headers);
        hash.update([u8::from(context.use_manual_redirects)]);
        hash.update([u8::from(session.origin_account_binding.is_some())]);
        if let Some(account) = &session.origin_account_binding {
            part(&mut hash, account.input_name.as_bytes());
            part(&mut hash, account.account_name.as_bytes());
        }
        // Cache timestamps, work generations and account lease renewals do not
        // change the confirmed media or its concrete HTTP/account binding.
        Ok(Self {
            semantic: hash.finalize().into(),
            anchor: anchor.clone(),
            work_generation: session.activity.origin_work_generation,
            account_generation: session.transient.origin_account_generation(),
            account_lease_generation: session.origin_account_binding.as_ref().map(|account| account.generation),
        })
    }
}

#[derive(Default)]
pub(super) struct HlsAnchorProbeEvidence {
    final_mismatches: HashSet<[u8; 32]>,
    round: Option<Weak<HlsAnchorProbeRound>>,
}
impl fmt::Debug for HlsAnchorProbeEvidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HlsAnchorProbeEvidence")
            .field("final_mismatches", &self.final_mismatches.len())
            .finish_non_exhaustive()
    }
}
impl HlsAnchorProbeEvidence {
    pub(super) fn round(&self) -> Result<Option<Arc<HlsAnchorProbeRound>>, ()> {
        match &self.round {
            None => Ok(None),
            Some(round) => round.upgrade().filter(|round| round.is_active()).map(Some).ok_or(()),
        }
    }
    pub(super) fn rejects(&self, key: &ProbeIdentity) -> bool { self.final_mismatches.contains(&key.semantic) }
    pub(super) fn reject(&mut self, key: &ProbeIdentity) { self.final_mismatches.insert(key.semantic); }
    pub(super) fn mark_candidate(&self, identity: HlsManifestRecoveryCandidateIdentity) {
        if let Ok(Some(round)) = self.round() {
            round.mark_rejected(identity);
        }
    }
}

pub(super) struct HlsAnchorProbeRound {
    capacity: usize,
    entries: Mutex<Vec<(ProbeIdentity, ProbeFuture)>>,
    reference_pins: Mutex<Vec<(std::path::PathBuf, Arc<crate::HlsRevisionFilePin>)>>,
    rejected: Mutex<Vec<HlsManifestRecoveryCandidateIdentity>>,
    pub(super) cancelled: CancellationToken,
}
impl HlsAnchorProbeRound {
    /// Pins the reference file for this round. `guarded` means a retained
    /// revision guard already pins `path`; the cache allows one active
    /// registration per path, so a second pin would fail.
    pub(super) fn pin_reference(
        &self,
        cache: &crate::HlsSegmentCache,
        path: &std::path::Path,
        guarded: bool,
    ) -> Result<Option<Arc<crate::HlsRevisionFilePin>>, ()> {
        let mut pins = self.reference_pins.lock_unpoisoned();
        if let Some((_, pin)) = pins.iter().find(|(current, _)| current == path) {
            return Ok(Some(Arc::clone(pin)));
        }
        if guarded {
            return Ok(None);
        }
        if !self.is_active() || pins.len() >= self.capacity {
            return Err(());
        }
        let pin = Arc::new(cache.pin_revision_file(path).map_err(|_| ())?);
        pins.push((path.to_path_buf(), Arc::clone(&pin)));
        Ok(Some(pin))
    }
    pub(super) fn is_active(&self) -> bool { !self.cancelled.is_cancelled() }
    pub(super) fn mark_rejected(&self, identity: HlsManifestRecoveryCandidateIdentity) {
        let mut rejected = self.rejected.lock_unpoisoned();
        if !rejected.contains(&identity) && rejected.len() < self.capacity {
            rejected.push(identity);
        }
    }
    pub(super) async fn digest(
        &self,
        key: ProbeIdentity,
        fetch: impl Future<Output = Result<[u8; 32], ()>> + Send + 'static,
    ) -> Result<[u8; 32], ()> {
        let shared = {
            let mut entries = self.entries.lock_unpoisoned();
            if !self.is_active() {
                return Err(());
            }
            if let Some((_, shared)) = entries.iter().find(|(current, _)| *current == key) {
                shared.clone()
            } else {
                if entries.len() >= self.capacity {
                    return Err(());
                }
                let cancelled = self.cancelled.clone();
                let shared = async move {
                    tokio::select! { biased; ()=cancelled.cancelled()=>Err(()), result=fetch=>result }
                }
                .boxed()
                .shared();
                entries.push((key, shared.clone()));
                shared
            }
        };
        tokio::select! { biased; ()=self.cancelled.cancelled()=>Err(()), result=shared=>result }
    }
}

pub(crate) struct HlsAnchorProbeRoundGuard(pub(super) Arc<HlsAnchorProbeRound>);
impl HlsAnchorProbeRoundGuard {
    pub(super) fn standalone(capacity: usize) -> Self {
        Self(Arc::new(HlsAnchorProbeRound {
            capacity,
            entries: Mutex::new(Vec::new()),
            reference_pins: Mutex::new(Vec::new()),
            rejected: Mutex::new(Vec::new()),
            cancelled: CancellationToken::new(),
        }))
    }
    pub(crate) async fn install(session: &crate::session_store::HlsSessionHandle, capacity: usize) -> Self {
        let guard = Self::standalone(capacity);
        session.write().await.origin_content.probes.round = Some(Arc::downgrade(&guard.0));
        guard
    }
}
impl Drop for HlsAnchorProbeRoundGuard {
    fn drop(&mut self) {
        self.0.cancelled.cancel();
        self.0.entries.lock_unpoisoned().clear();
        self.0.reference_pins.lock_unpoisoned().clear();
    }
}

pub(crate) fn candidate_has_final_mismatch(
    session: &HlsSession,
    identity: HlsManifestRecoveryCandidateIdentity,
) -> bool {
    session
        .origin_content
        .probes
        .round()
        .ok()
        .flatten()
        .is_some_and(|round| round.rejected.lock_unpoisoned().contains(&identity))
}

pub(super) async fn fetch_digest(
    request: OriginRefreshRequest,
    context: SegmentFetchContext,
    key: crate::SegmentCacheKey,
    reference: crate::OriginSegmentFetchRef,
) -> Result<[u8; 32], ()> {
    tokio::time::timeout(std::time::Duration::from_millis(request.origin_manifest_timeout_ms), async move {
        let _permit = request.segment_worker_pool.try_reserve_origin_probe(&request.session).await.ok_or(())?;
        let object = crate::segment_fetcher::stage_hls_switch_resource(
            &context,
            &request.segment_worker_pool.policy(),
            key,
            reference.resolved_origin_url,
            reference.byte_range,
            crate::segment_fetcher::HlsSwitchResourceKind::Segment,
        )
        .await
        .map_err(|_| ())?;
        let digest = super::origin_probe::sha256_file(&object.path).await.map_err(|_| ());
        request.segment_cache.remove_staged(object).await.map_err(|_| ())?;
        digest
    })
    .await
    .map_err(|_| ())?
}
