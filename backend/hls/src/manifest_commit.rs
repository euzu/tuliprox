use super::{
    HlsAccountBindingProtection, HlsFreshManifestRequiredReason, HlsManifestCommitRequirement, HlsOriginAccountBinding,
    HlsSession, HlsSessionHandle, HlsSessionMode, HlsSessionStoreOutcome,
};
use std::{sync::Arc, time::Duration};

#[derive(Clone)]
pub enum HlsCommittedManifestBody {
    Normal(String),
    Transient(Arc<str>),
}

struct HlsCommittedManifestCandidate {
    body: HlsCommittedManifestBody,
    rendered_at_ms: u64,
    valid_until_ms: Option<u64>,
    finalized: bool,
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
    after_origin_manifest_commits: Option<u64>,
}

impl HlsCachedManifestOptions {
    #[cfg_attr(not(test), allow(dead_code))]
    pub const fn committed_only(wait_timeout: Duration) -> Self {
        Self {
            wait_timeout,
            policy: HlsCachedManifestPolicy::CommittedOnly,
            newer_than_rendered_at_ms: None,
            after_origin_manifest_commits: None,
        }
    }

    pub const fn initial(wait_timeout: Duration) -> Self {
        Self {
            wait_timeout,
            policy: HlsCachedManifestPolicy::AllowInitialNoMediaYet,
            newer_than_rendered_at_ms: None,
            after_origin_manifest_commits: None,
        }
    }

    pub const fn requiring_newer_manifest(mut self, rendered_at_ms: u64) -> Self {
        self.newer_than_rendered_at_ms = Some(rendered_at_ms);
        self
    }

    /// Requires a committed manifest stored after an origin commit newer than
    /// `origin_manifest_commits`.
    pub const fn requiring_origin_commit_after(mut self, origin_manifest_commits: u64) -> Self {
        self.after_origin_manifest_commits = Some(origin_manifest_commits);
        self
    }

    pub const fn requires_newer_manifest(self) -> bool {
        self.newer_than_rendered_at_ms.is_some() || self.after_origin_manifest_commits.is_some()
    }
}

pub const fn hls_cached_manifest_options_for_requirement(
    wait_timeout: Duration,
    requirement: HlsManifestCommitRequirement,
    previous_rendered_at_ms: u64,
    entry_origin_manifest_commits: u64,
) -> HlsCachedManifestOptions {
    let options = HlsCachedManifestOptions::initial(wait_timeout);
    match requirement {
        HlsManifestCommitRequirement::CommittedManifestAllowed => options,
        HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::StaleEntryWindow,
        } => options
            .requiring_newer_manifest(previous_rendered_at_ms)
            .requiring_origin_commit_after(entry_origin_manifest_commits),
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
        ) && manifest_rendered_after_required_boundary(Some(candidate.rendered_at_ms), options)
            && options
                .after_origin_manifest_commits
                .is_none_or(|commits| session.committed_manifest_follows_origin_commit(commits)))
        .then_some(candidate.body)
    })
}

/// The accelerated lease view is rendered from an origin commit, independently
/// of whether the shared session's older window can still be rendered.
pub fn hls_live_window_origin_allowed_for_request(
    session: &HlsSession,
    options: HlsCachedManifestOptions,
    started_at_ms: u64,
    now_ms: u64,
) -> bool {
    let Some(identity) = session.live_window_source_identity() else { return false };
    let duration = session.origin_window_durations_ms.iter().fold(0_u64, |sum, duration| sum.saturating_add(*duration));
    let candidate = HlsCommittedManifestCandidate {
        body: HlsCommittedManifestBody::Normal(String::new()),
        rendered_at_ms: identity.rendered_at_ms(),
        valid_until_ms: Some(identity.rendered_at_ms().saturating_add(duration)),
        finalized: false,
    };
    session.uses_fast_start_live_window()
        && can_serve_committed_manifest(
            session,
            &candidate,
            session.account_binding_protection(now_ms),
            options.policy,
            candidate.rendered_at_ms >= started_at_ms,
            now_ms,
        )
        && manifest_rendered_after_required_boundary(Some(candidate.rendered_at_ms), options)
        && options.after_origin_manifest_commits.is_none_or(|commits| session.origin_manifest_commits() > commits)
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
    new_playback_entry: bool,
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
    if let Some(candidate) = hls_committed_manifest_for_request(&session, now_ms) {
        // A finalized playlist no longer moves, so its window cannot fall behind the origin.
        if new_playback_entry && !candidate.finalized && !origin_window_is_fresh(&session, now_ms) {
            return HlsManifestCommitRequirement::FreshCommitRequired {
                reason: HlsFreshManifestRequiredReason::StaleEntryWindow,
            };
        }
        HlsManifestCommitRequirement::CommittedManifestAllowed
    } else {
        HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
        }
    }
}

/// Live origins advance about one segment per target duration, sometimes in
/// bursts. A new playback that starts on a window confirmed more than half a
/// target duration ago can already be a slide behind the origin; the next
/// refresh then drops the segments it was about to play and the player skips
/// forward. New playback waits for a fresh commit instead (one origin playlist
/// round trip).
fn origin_window_is_fresh(session: &HlsSession, now_ms: u64) -> bool {
    let Some(target_duration_ms) = session
        .target_duration
        .map(|seconds| u64::from(seconds).saturating_mul(1_000))
        .or_else(|| session.last_rendered_manifest.as_ref().map(|rendered| rendered.target_duration_ms))
    else {
        return false;
    };
    let max_age_ms = target_duration_ms / 2;
    session
        .origin_refresh
        .last_success_at_ms
        .is_some_and(|confirmed_at_ms| now_ms.saturating_sub(confirmed_at_ms) <= max_age_ms)
}

pub(crate) fn has_servable_committed_manifest(session: &HlsSession, now_ms: u64) -> bool {
    hls_committed_manifest_for_request(session, now_ms).is_some()
}

fn hls_committed_manifest_for_request(session: &HlsSession, now_ms: u64) -> Option<HlsCommittedManifestCandidate> {
    let protection = session.account_binding_protection(now_ms);
    hls_committed_manifest_candidate(session).filter(|candidate| {
        can_serve_committed_manifest(
            session,
            candidate,
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
                finalized: false,
            })
        }
        HlsSessionMode::TransientPassthrough { .. } => {
            session.transient.last_manifest_body.as_ref().map(|body| HlsCommittedManifestCandidate {
                body: HlsCommittedManifestBody::Transient(body.clone()),
                rendered_at_ms: session.transient.last_manifest_rendered_at_ms.unwrap_or_default(),
                valid_until_ms: session.transient.last_manifest_valid_until_ms(),
                finalized: session.transient.last_manifest_finalized(),
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
    candidate.finalized || candidate.valid_until_ms.is_some_and(|valid_until_ms| now_ms <= valid_until_ms)
}

#[cfg(test)]
mod tests {
    use super::{
        committed_manifest_valid_at, hls_cached_manifest_options_for_requirement,
        hls_committed_manifest_body_for_request, hls_committed_manifest_candidate, hls_manifest_commit_requirement,
    };
    use crate::{
        HlsFreshManifestRequiredReason, HlsManifestCommitRequirement, HlsSession, HlsSessionKey, HlsSessionMode,
        HlsSessionStoreOutcome, TransientPassthroughReason,
    };
    use std::{sync::Arc, time::Duration};
    use tokio::sync::RwLock;

    fn transient_session(body: &str) -> HlsSession {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "manifest-validity"), b"secret", 0);
        session.mode = HlsSessionMode::TransientPassthrough {
            reason: TransientPassthroughReason::UnsupportedTag { tag: "#EXT-X-ENDLIST".to_string() },
        };
        session.transient.replace_manifest_with_semantics(body.to_string(), 100, Some(1_000));
        session
    }

    #[test]
    fn finalized_transient_manifest_remains_current_after_playlist_duration() {
        let session = transient_session("#EXTM3U\n#EXTINF:1,\n/r/resource.ts\n#EXT-X-ENDLIST\n");
        let candidate = hls_committed_manifest_candidate(&session).expect("finalized committed manifest");

        assert!(candidate.finalized);
        assert_eq!(candidate.valid_until_ms, None);
        assert!(committed_manifest_valid_at(&candidate, u64::MAX));
        assert_eq!(session.transient.last_manifest_playlist_duration_ms, Some(1_000));
    }

    #[test]
    fn rolling_transient_manifest_remains_time_bounded() {
        let session = transient_session("#EXTM3U\n#EXTINF:1,\n/r/resource.ts\n");
        let candidate = hls_committed_manifest_candidate(&session).expect("rolling committed manifest");

        assert!(!candidate.finalized);
        assert_eq!(candidate.valid_until_ms, Some(1_100));
        assert!(committed_manifest_valid_at(&candidate, 1_100));
        assert!(!committed_manifest_valid_at(&candidate, 1_101));
    }

    #[tokio::test]
    async fn new_playback_entry_requires_fresh_commit_only_for_rolling_stale_windows() {
        let now_ms = 100_000;
        for (body, expected) in [
            (
                "#EXTM3U\n#EXTINF:1,\n/r/resource.ts\n#EXT-X-ENDLIST\n",
                HlsManifestCommitRequirement::CommittedManifestAllowed,
            ),
            (
                "#EXTM3U\n#EXTINF:1,\n/r/resource.ts\n",
                HlsManifestCommitRequirement::FreshCommitRequired {
                    reason: HlsFreshManifestRequiredReason::StaleEntryWindow,
                },
            ),
        ] {
            let mut session = transient_session(body);
            session.transient.replace_manifest_with_semantics(body.to_string(), now_ms, Some(1_000));
            session.target_duration = Some(1);
            session.mark_authorized_media_access(now_ms);
            session.origin_refresh.last_success_at_ms = Some(now_ms - 16_000);
            let session = Arc::new(RwLock::new(session));

            assert_eq!(
                hls_manifest_commit_requirement(&session, HlsSessionStoreOutcome::Reused, None, true, now_ms).await,
                expected,
                "{body:?}"
            );
        }
    }

    #[tokio::test]
    async fn new_playback_entry_requires_window_confirmed_within_half_a_target_duration() {
        // v3.3.151 validation: re-entry 6.3 s after the last refresh (target duration 7 s) was
        // served [1,2,3,4] while the origin had already moved to [3,4,5].
        let now_ms = 100_000;
        let body = "#EXTM3U\n#EXTINF:6,\n/r/resource.ts\n";
        for (confirmed_age_ms, expected) in [
            (
                6_300,
                HlsManifestCommitRequirement::FreshCommitRequired {
                    reason: HlsFreshManifestRequiredReason::StaleEntryWindow,
                },
            ),
            (3_000, HlsManifestCommitRequirement::CommittedManifestAllowed),
        ] {
            let mut session = transient_session(body);
            session.transient.replace_manifest_with_semantics(body.to_string(), now_ms, Some(6_000));
            session.target_duration = Some(7);
            session.mark_authorized_media_access(now_ms);
            session.origin_refresh.last_success_at_ms = Some(now_ms - confirmed_age_ms);
            let session = Arc::new(RwLock::new(session));

            assert_eq!(
                hls_manifest_commit_requirement(&session, HlsSessionStoreOutcome::Reused, None, true, now_ms).await,
                expected,
                "confirmed_age_ms={confirmed_age_ms}"
            );
        }
    }

    #[test]
    fn stale_entry_waits_for_a_manifest_stored_after_a_newer_origin_commit() {
        let now_ms = 100_000;
        let mut session = transient_session("#EXTM3U\n#EXTINF:1,\n/r/resource.ts\n");
        session.mark_authorized_media_access(now_ms);
        let options = hls_cached_manifest_options_for_requirement(
            Duration::ZERO,
            HlsManifestCommitRequirement::FreshCommitRequired {
                reason: HlsFreshManifestRequiredReason::StaleEntryWindow,
            },
            0,
            session.origin_manifest_commits(),
        );
        assert!(hls_committed_manifest_body_for_request(&session, options, 0, now_ms).is_none());

        session.record_origin_manifest_commit();
        let identity = session.next_manifest_commit_identity(now_ms).expect("commit generation");
        session.record_manifest_commit_identity(identity);

        assert!(hls_committed_manifest_body_for_request(&session, options, 0, now_ms).is_some());
    }
}
