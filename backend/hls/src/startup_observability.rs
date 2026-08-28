use super::{safe_hls_access_lease_id, HlsAccessLeaseId, HlsLogIdentity};
use log::debug;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

const MAX_STARTUP_OBSERVATIONS: usize = 2_048;

#[derive(Debug, Default)]
struct HlsStartupObservationState {
    identity: Option<HlsLogIdentity>,
    entry_master_response_at_ms: Option<u64>,
    media_manifest_request_at_ms: Option<u64>,
    origin_manifest_commit_at_ms: Option<u64>,
    startup_admission_at_ms: Option<u64>,
    media_manifest_publication_at_ms: Option<u64>,
    publication_generation: Option<u64>,
    visible_proxy_seqs: Arc<[u64]>,
    first_segment_proxy_seq: Option<u64>,
    first_segment_request_at_ms: Option<u64>,
    repair_decision_at_ms: Option<u64>,
    cache_response_prepared_at_ms: Option<u64>,
    first_body_chunk_at_ms: Option<u64>,
    body_completed_at_ms: Option<u64>,
    body_id: Option<String>,
    summary_emitted: bool,
}

#[derive(Debug, Default)]
struct HlsStartupObservationRegistry {
    observations: HashMap<HlsAccessLeaseId, HlsStartupObservationState>,
    insertion_order: VecDeque<HlsAccessLeaseId>,
}

#[derive(Debug, Default)]
pub struct HlsStartupObservability {
    registry: Mutex<HlsStartupObservationRegistry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsStartupSummary {
    lease: String,
    session: String,
    proxy_session: String,
    generation: u64,
    body_id: String,
    first_segment_proxy_seq: u64,
    entry_to_manifest_request_ms: Option<u64>,
    manifest_request_to_origin_commit_ms: Option<u64>,
    origin_commit_to_admission_ms: Option<u64>,
    admission_to_publication_ms: Option<u64>,
    publication_to_segment_request_ms: Option<u64>,
    segment_request_to_repair_decision_ms: Option<u64>,
    repair_decision_to_response_prepared_ms: Option<u64>,
    response_prepared_to_first_chunk_ms: Option<u64>,
    first_chunk_to_body_finished_ms: Option<u64>,
    outcome: &'static str,
}

impl std::fmt::Display for HlsStartupSummary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "lease={} session={} proxy_session={} generation={} body_id={} first_segment_proxy_seq={} entry_to_manifest_request_ms={} manifest_request_to_origin_commit_ms={} origin_commit_to_admission_ms={} admission_to_publication_ms={} publication_to_segment_request_ms={} segment_request_to_repair_decision_ms={} repair_decision_to_response_prepared_ms={} response_prepared_to_first_chunk_ms={} first_chunk_to_body_finished_ms={} outcome={} decoded_picture_at_ms=unobserved",
            self.lease,
            self.session,
            self.proxy_session,
            self.generation,
            self.body_id,
            self.first_segment_proxy_seq,
            optional_ms(self.entry_to_manifest_request_ms),
            optional_ms(self.manifest_request_to_origin_commit_ms),
            optional_ms(self.origin_commit_to_admission_ms),
            optional_ms(self.admission_to_publication_ms),
            optional_ms(self.publication_to_segment_request_ms),
            optional_ms(self.segment_request_to_repair_decision_ms),
            optional_ms(self.repair_decision_to_response_prepared_ms),
            optional_ms(self.response_prepared_to_first_chunk_ms),
            optional_ms(self.first_chunk_to_body_finished_ms),
            self.outcome,
        )
    }
}

fn optional_ms(value: Option<u64>) -> String { value.map_or_else(|| "none".to_string(), |value| value.to_string()) }

fn elapsed_ms(start: Option<u64>, end: Option<u64>) -> Option<u64> { end?.checked_sub(start?) }

impl HlsStartupObservability {
    pub fn record_entry_master_response(&self, lease_id: HlsAccessLeaseId, identity: HlsLogIdentity, now_ms: u64) {
        let mut registry = self.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !registry.observations.contains_key(&lease_id) {
            registry.insertion_order.push_back(lease_id.clone());
        }
        let observation = registry.observations.entry(lease_id).or_default();
        observation.identity.get_or_insert(identity);
        observation.entry_master_response_at_ms.get_or_insert(now_ms);
        prune_registry(&mut registry);
    }

    pub fn record_media_manifest_request(&self, lease_id: &HlsAccessLeaseId, now_ms: u64) {
        self.update(lease_id, |observation| {
            observation.media_manifest_request_at_ms.get_or_insert(now_ms);
        });
    }

    pub fn record_origin_manifest_commit(&self, lease_id: &HlsAccessLeaseId, now_ms: u64) {
        self.update(lease_id, |observation| {
            observation.origin_manifest_commit_at_ms.get_or_insert(now_ms);
        });
    }

    pub fn record_manifest_publication(
        &self,
        lease_id: &HlsAccessLeaseId,
        generation: u64,
        admission_at_ms: u64,
        published_at_ms: u64,
        visible_proxy_seqs: Arc<[u64]>,
    ) -> bool {
        let mut recorded = false;
        self.update(lease_id, |observation| {
            if observation.publication_generation.is_some() {
                return;
            }
            observation.startup_admission_at_ms = Some(admission_at_ms);
            observation.media_manifest_publication_at_ms = Some(published_at_ms);
            observation.publication_generation = Some(generation);
            observation.visible_proxy_seqs = visible_proxy_seqs;
            recorded = true;
        });
        recorded
    }

    pub fn record_first_visible_segment_request(&self, lease_id: &HlsAccessLeaseId, proxy_seq: u64, now_ms: u64) {
        self.update(lease_id, |observation| {
            if observation.first_segment_request_at_ms.is_some() || !observation.visible_proxy_seqs.contains(&proxy_seq)
            {
                return;
            }
            observation.first_segment_proxy_seq = Some(proxy_seq);
            observation.first_segment_request_at_ms = Some(now_ms);
        });
    }

    pub fn record_repair_decision(&self, lease_id: &HlsAccessLeaseId, proxy_seq: u64, now_ms: u64) {
        self.update(lease_id, |observation| {
            if observation.first_segment_proxy_seq == Some(proxy_seq) {
                observation.repair_decision_at_ms.get_or_insert(now_ms);
            }
        });
    }

    pub fn begin_cache_response(
        self: &Arc<Self>,
        lease_id: &HlsAccessLeaseId,
        proxy_seq: u64,
        body_id: &str,
        now_ms: u64,
    ) -> Option<HlsStartupBodyObservation> {
        let generation = {
            let mut registry = self.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let observation = registry.observations.get_mut(lease_id)?;
            if observation.first_segment_proxy_seq != Some(proxy_seq) || observation.body_id.is_some() {
                return None;
            }
            let generation = observation.publication_generation?;
            observation.cache_response_prepared_at_ms = Some(now_ms);
            observation.body_id = Some(body_id.to_string());
            generation
        };
        Some(HlsStartupBodyObservation {
            owner: Arc::clone(self),
            lease_id: lease_id.clone(),
            generation,
            body_id: body_id.to_string(),
        })
    }

    pub fn remove_access_lease(&self, lease_id: &HlsAccessLeaseId) {
        let mut registry = self.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.observations.remove(lease_id);
        registry.insertion_order.retain(|candidate| candidate != lease_id);
    }

    pub fn clear(&self) {
        let mut registry = self.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.observations.clear();
        registry.insertion_order.clear();
    }

    fn update(&self, lease_id: &HlsAccessLeaseId, operation: impl FnOnce(&mut HlsStartupObservationState)) {
        let mut registry = self.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(observation) = registry.observations.get_mut(lease_id) {
            operation(observation);
        }
    }

    fn record_first_chunk(&self, lease_id: &HlsAccessLeaseId, generation: u64, body_id: &str, now_ms: u64) -> bool {
        let mut recorded = false;
        self.update(lease_id, |observation| {
            if observation.publication_generation == Some(generation)
                && observation.body_id.as_deref() == Some(body_id)
                && observation.first_body_chunk_at_ms.is_none()
            {
                observation.first_body_chunk_at_ms = Some(now_ms);
                recorded = true;
            }
        });
        recorded
    }

    fn finish_body(
        &self,
        lease_id: &HlsAccessLeaseId,
        generation: u64,
        body_id: &str,
        now_ms: u64,
        outcome: &'static str,
    ) -> Option<HlsStartupSummary> {
        let mut registry = self.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let observation = registry.observations.get_mut(lease_id)?;
        if observation.publication_generation != Some(generation)
            || observation.body_id.as_deref() != Some(body_id)
            || observation.summary_emitted
        {
            return None;
        }
        let identity = observation.identity.as_ref()?.clone();
        let first_segment_proxy_seq = observation.first_segment_proxy_seq?;
        observation.summary_emitted = true;
        observation.body_completed_at_ms = Some(now_ms);
        Some(HlsStartupSummary {
            lease: safe_hls_access_lease_id(lease_id),
            session: identity.session(),
            proxy_session: identity.proxy_session(),
            generation,
            body_id: body_id.to_string(),
            first_segment_proxy_seq,
            entry_to_manifest_request_ms: elapsed_ms(
                observation.entry_master_response_at_ms,
                observation.media_manifest_request_at_ms,
            ),
            manifest_request_to_origin_commit_ms: elapsed_ms(
                observation.media_manifest_request_at_ms,
                observation.origin_manifest_commit_at_ms,
            ),
            origin_commit_to_admission_ms: elapsed_ms(
                observation.origin_manifest_commit_at_ms,
                observation.startup_admission_at_ms,
            ),
            admission_to_publication_ms: elapsed_ms(
                observation.startup_admission_at_ms,
                observation.media_manifest_publication_at_ms,
            ),
            publication_to_segment_request_ms: elapsed_ms(
                observation.media_manifest_publication_at_ms,
                observation.first_segment_request_at_ms,
            ),
            segment_request_to_repair_decision_ms: elapsed_ms(
                observation.first_segment_request_at_ms,
                observation.repair_decision_at_ms,
            ),
            repair_decision_to_response_prepared_ms: elapsed_ms(
                observation.repair_decision_at_ms,
                observation.cache_response_prepared_at_ms,
            ),
            response_prepared_to_first_chunk_ms: elapsed_ms(
                observation.cache_response_prepared_at_ms,
                observation.first_body_chunk_at_ms,
            ),
            first_chunk_to_body_finished_ms: elapsed_ms(
                observation.first_body_chunk_at_ms,
                observation.body_completed_at_ms,
            ),
            outcome,
        })
    }
}

fn prune_registry(registry: &mut HlsStartupObservationRegistry) {
    while registry.observations.len() > MAX_STARTUP_OBSERVATIONS {
        let Some(oldest) = registry.insertion_order.pop_front() else {
            return;
        };
        registry.observations.remove(&oldest);
    }
}

#[derive(Clone)]
pub struct HlsStartupBodyObservation {
    owner: Arc<HlsStartupObservability>,
    lease_id: HlsAccessLeaseId,
    generation: u64,
    body_id: String,
}

impl HlsStartupBodyObservation {
    pub fn record_first_chunk(&self, now_ms: u64) -> bool {
        self.owner.record_first_chunk(&self.lease_id, self.generation, &self.body_id, now_ms)
    }

    pub fn finish(&self, now_ms: u64, outcome: &'static str) {
        if let Some(summary) = self.owner.finish_body(&self.lease_id, self.generation, &self.body_id, now_ms, outcome) {
            debug!("HLS startup timing completed: {summary}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated_observation() -> (Arc<HlsStartupObservability>, HlsAccessLeaseId) {
        let owner = Arc::new(HlsStartupObservability::default());
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        owner.record_entry_master_response(
            lease_id.clone(),
            HlsLogIdentity::for_test("input:1|hls|stream", "proxy-session"),
            100,
        );
        owner.record_media_manifest_request(&lease_id, 110);
        owner.record_origin_manifest_commit(&lease_id, 130);
        assert!(owner.record_manifest_publication(&lease_id, 7, 140, 145, Arc::from([10, 11, 12])));
        owner.record_first_visible_segment_request(&lease_id, 10, 160);
        owner.record_repair_decision(&lease_id, 10, 170);
        (owner, lease_id)
    }

    #[test]
    fn startup_summary_is_emitted_once_per_publication_generation() {
        let (owner, lease_id) = populated_observation();
        assert!(!owner.record_manifest_publication(&lease_id, 8, 150, 155, Arc::from([20, 21, 22])));
        let body = owner.begin_cache_response(&lease_id, 10, "0000002a", 175).expect("first body observation");
        assert!(body.record_first_chunk(180));
        assert!(!body.record_first_chunk(181));

        let first = owner.finish_body(&lease_id, 7, "0000002a", 200, "ok");
        let duplicate = owner.finish_body(&lease_id, 7, "0000002a", 210, "drop");

        assert_eq!(first.as_ref().map(|summary| summary.body_id.as_str()), Some("0000002a"));
        assert_eq!(first.as_ref().and_then(|summary| summary.first_chunk_to_body_finished_ms), Some(20));
        assert!(duplicate.is_none());
    }

    #[test]
    fn unrelated_or_repeated_segment_response_cannot_claim_startup_body() {
        let (owner, lease_id) = populated_observation();

        assert!(owner.begin_cache_response(&lease_id, 11, "00000001", 175).is_none());
        assert!(owner.begin_cache_response(&lease_id, 10, "00000002", 175).is_some());
        assert!(owner.begin_cache_response(&lease_id, 10, "00000003", 176).is_none());
    }

    #[test]
    fn response_before_publication_does_not_poison_later_body_observation() {
        let owner = Arc::new(HlsStartupObservability::default());
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        owner.record_entry_master_response(
            lease_id.clone(),
            HlsLogIdentity::for_test("input:1|hls|stream", "proxy-session"),
            100,
        );
        {
            let mut registry = owner.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.observations.get_mut(&lease_id).expect("observation").first_segment_proxy_seq = Some(10);
        }

        assert!(owner.begin_cache_response(&lease_id, 10, "premature", 120).is_none());
        {
            let registry = owner.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let observation = registry.observations.get(&lease_id).expect("observation");
            assert!(observation.body_id.is_none());
            assert!(observation.cache_response_prepared_at_ms.is_none());
        }
        assert!(owner.record_manifest_publication(&lease_id, 7, 130, 140, Arc::from([10, 11, 12])));
        assert!(owner.begin_cache_response(&lease_id, 10, "published", 150).is_some());
    }

    #[test]
    fn incomplete_body_observation_does_not_latch_startup_summary() {
        let (owner, lease_id) = populated_observation();
        assert!(owner.begin_cache_response(&lease_id, 10, "body", 175).is_some());
        {
            let mut registry = owner.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.observations.get_mut(&lease_id).expect("observation").first_segment_proxy_seq = None;
        }

        assert!(owner.finish_body(&lease_id, 7, "body", 200, "ok").is_none());
        {
            let registry = owner.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let observation = registry.observations.get(&lease_id).expect("observation");
            assert!(!observation.summary_emitted);
            assert!(observation.body_completed_at_ms.is_none());
        }

        {
            let mut registry = owner.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.observations.get_mut(&lease_id).expect("observation").first_segment_proxy_seq = Some(10);
        }
        assert!(owner.finish_body(&lease_id, 7, "body", 210, "ok").is_some());
    }
}
