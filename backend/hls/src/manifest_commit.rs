use super::{
    HlsAccountBindingProtection, HlsFreshManifestRequiredReason, HlsManifestCommitRequirement, HlsOriginAccountBinding,
    HlsSession, HlsSessionHandle, HlsSessionMode, HlsSessionStoreOutcome,
};
use std::time::Duration;

#[derive(Clone)]
pub enum HlsCommittedManifestBody {
    Normal(String),
    Transient(String),
}

struct HlsCommittedManifestCandidate {
    body: HlsCommittedManifestBody,
    rendered_at_ms: u64,
    valid_until_ms: Option<u64>,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy)]
pub enum HlsCachedManifestPolicy {
    CommittedOnly,
    AllowInitialNoMediaYet,
}

#[derive(Clone, Copy)]
pub struct HlsCachedManifestOptions {
    pub wait_timeout: Duration,
    policy: HlsCachedManifestPolicy,
    newer_than_rendered_at_ms: Option<u64>,
}

impl HlsCachedManifestOptions {
    #[cfg_attr(not(test), allow(dead_code))]
    pub const fn committed_only(wait_timeout: Duration) -> Self {
        Self { wait_timeout, policy: HlsCachedManifestPolicy::CommittedOnly, newer_than_rendered_at_ms: None }
    }

    pub const fn initial(wait_timeout: Duration) -> Self {
        Self { wait_timeout, policy: HlsCachedManifestPolicy::AllowInitialNoMediaYet, newer_than_rendered_at_ms: None }
    }

    pub const fn requiring_newer_manifest(mut self, rendered_at_ms: u64) -> Self {
        self.newer_than_rendered_at_ms = Some(rendered_at_ms);
        self
    }

    const fn requires_newer_manifest(self) -> bool { self.newer_than_rendered_at_ms.is_some() }
}

pub const fn hls_cached_manifest_options_for_requirement(
    wait_timeout: Duration,
    requirement: HlsManifestCommitRequirement,
    previous_rendered_at_ms: u64,
) -> HlsCachedManifestOptions {
    let options = HlsCachedManifestOptions::initial(wait_timeout);
    match requirement {
        HlsManifestCommitRequirement::CommittedManifestAllowed => options,
        HlsManifestCommitRequirement::FreshCommitRequired { .. } => {
            options.requiring_newer_manifest(previous_rendered_at_ms)
        }
    }
}

pub fn hls_committed_manifest_body_for_request(
    session: &HlsSession,
    options: HlsCachedManifestOptions,
    started_at_ms: u64,
    now_ms: u64,
) -> Option<HlsCommittedManifestBody> {
    let protection = session.account_binding_protection(now_ms);
    hls_committed_manifest_candidate(session).and_then(|candidate| {
        (can_serve_committed_manifest(
            session,
            &candidate,
            protection,
            options.policy,
            candidate.rendered_at_ms >= started_at_ms,
            now_ms,
        ) && manifest_rendered_after_required_boundary(Some(candidate.rendered_at_ms), options))
        .then_some(candidate.body)
    })
}

pub fn hls_should_wait_for_initial_manifest_commit(
    session: &HlsSession,
    selected_committed_body: bool,
    should_wait: bool,
    options: HlsCachedManifestOptions,
    now_ms: u64,
) -> bool {
    let protection = session.account_binding_protection(now_ms);
    !selected_committed_body
        && (matches!(protection, HlsAccountBindingProtection::NoMediaYet | HlsAccountBindingProtection::Expired)
            || options.requires_newer_manifest())
        && should_wait
        && !options.wait_timeout.is_zero()
}

pub async fn hls_manifest_commit_requirement(
    session: &HlsSessionHandle,
    session_outcome: HlsSessionStoreOutcome,
    handoff_previous_rendered_at_ms: Option<u64>,
    now_ms: u64,
) -> HlsManifestCommitRequirement {
    if handoff_previous_rendered_at_ms.is_some() {
        return HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::ProvisioningHandoff,
        };
    }
    if matches!(session_outcome, HlsSessionStoreOutcome::Created) {
        return HlsManifestCommitRequirement::FreshCommitRequired { reason: HlsFreshManifestRequiredReason::ColdStart };
    }

    let session = session.read().await;
    if let Some(reason) = session.fresh_manifest_commit_required {
        return HlsManifestCommitRequirement::FreshCommitRequired { reason };
    }
    if hls_committed_manifest_available_for_request(&session, now_ms) {
        HlsManifestCommitRequirement::CommittedManifestAllowed
    } else {
        HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
        }
    }
}

fn hls_committed_manifest_available_for_request(session: &HlsSession, now_ms: u64) -> bool {
    let protection = session.account_binding_protection(now_ms);
    hls_committed_manifest_candidate(session).is_some_and(|candidate| {
        can_serve_committed_manifest(
            session,
            &candidate,
            protection,
            HlsCachedManifestPolicy::AllowInitialNoMediaYet,
            false,
            now_ms,
        )
    })
}

fn hls_committed_manifest_candidate(session: &HlsSession) -> Option<HlsCommittedManifestCandidate> {
    match session.mode {
        HlsSessionMode::NormalCacheTimeline => {
            session.last_rendered_manifest.as_ref().map(|rendered| HlsCommittedManifestCandidate {
                body: HlsCommittedManifestBody::Normal(rendered.body.clone()),
                rendered_at_ms: rendered.rendered_at_ms,
                valid_until_ms: Some(rendered.valid_until_ms),
            })
        }
        HlsSessionMode::TransientPassthrough { .. } => {
            session.transient.last_manifest_body.as_ref().map(|body| HlsCommittedManifestCandidate {
                body: HlsCommittedManifestBody::Transient(body.clone()),
                rendered_at_ms: session.transient.last_manifest_rendered_at_ms.unwrap_or_default(),
                valid_until_ms: session.transient.last_manifest_valid_until_ms,
            })
        }
    }
}

fn manifest_rendered_after_required_boundary(rendered_at_ms: Option<u64>, options: HlsCachedManifestOptions) -> bool {
    let Some(boundary) = options.newer_than_rendered_at_ms else {
        return true;
    };
    rendered_at_ms.is_some_and(|rendered_at_ms| rendered_at_ms > boundary)
}

fn can_serve_committed_manifest(
    session: &HlsSession,
    candidate: &HlsCommittedManifestCandidate,
    protection: HlsAccountBindingProtection,
    policy: HlsCachedManifestPolicy,
    refreshed_after_wait_started: bool,
    now_ms: u64,
) -> bool {
    match protection {
        HlsAccountBindingProtection::HardActive { .. } | HlsAccountBindingProtection::SoftActive { .. } => true,
        HlsAccountBindingProtection::NoMediaYet => {
            matches!(policy, HlsCachedManifestPolicy::AllowInitialNoMediaYet)
                && committed_manifest_valid_at(candidate, now_ms)
        }
        HlsAccountBindingProtection::Expired => {
            matches!(policy, HlsCachedManifestPolicy::AllowInitialNoMediaYet)
                && (refreshed_after_wait_started
                    || (session.origin_account_binding.as_ref().is_some_and(HlsOriginAccountBinding::is_active)
                        && committed_manifest_valid_at(candidate, now_ms)))
        }
    }
}

fn committed_manifest_valid_at(candidate: &HlsCommittedManifestCandidate, now_ms: u64) -> bool {
    candidate.valid_until_ms.is_some_and(|valid_until_ms| now_ms <= valid_until_ms)
}
