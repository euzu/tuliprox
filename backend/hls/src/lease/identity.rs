use super::{
    lease_state_allows_use, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseStore, HlsLeaseManifestPublicationGuard,
    HlsLeasePlaybackMode, HlsMediaLeaseIdentity, HlsTerminalTailGeneration, ProxySessionId, HLS_ACCESS_LEASE_ID_BYTES,
};
use base64::{engine::general_purpose, Engine as _};
use rand::{rngs::OsRng, RngCore, TryRngCore};
use std::fmt;

impl fmt::Debug for HlsAccessLeaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("HlsAccessLeaseId").field(&"<redacted>").finish()
    }
}

impl HlsMediaLeaseIdentity {
    pub const fn same_playback_authorization(self, other: Self) -> bool {
        self.issued_at_ms == other.issued_at_ms
            && match (self.playback, other.playback) {
                (
                    HlsMediaLeasePlaybackIdentity::Live { admission_generation: left },
                    HlsMediaLeasePlaybackIdentity::Live { admission_generation: right },
                ) => left == right,
                (
                    HlsMediaLeasePlaybackIdentity::TerminalTail { generation: left },
                    HlsMediaLeasePlaybackIdentity::TerminalTail { generation: right },
                ) => left.0 == right.0,
                _ => false,
            }
    }
    pub const fn is_live(self) -> bool { matches!(self.playback, HlsMediaLeasePlaybackIdentity::Live { .. }) }

    pub const fn lease_issued_at_ms(self) -> u64 { self.issued_at_ms }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum HlsMediaLeasePlaybackIdentity {
    Live { admission_generation: u64 },
    TerminalTail { generation: HlsTerminalTailGeneration },
}

impl HlsLeaseManifestPublicationGuard {
    pub const fn same_lease_authorization(self, other: Self) -> bool {
        self.issued_at_ms == other.issued_at_ms && self.admission_generation == other.admission_generation
    }
    pub(crate) const fn has_source_binding(self) -> bool { self.source.is_some() }
    pub fn is_for_session(self, session: &crate::HlsSessionHandle) -> bool {
        self.source.is_none_or(|source| source.session_address == std::sync::Arc::as_ptr(session) as usize)
    }

    pub(crate) fn bind_source(&mut self, session_address: usize, session: &crate::HlsSession) {
        self.source = Some(super::HlsLeasePublicationSource {
            session_address,
            origin_epoch: session.origin_epoch,
            origin_work_generation: session.activity.origin_work_generation,
            origin_commits: session.origin_manifest_commits(),
            media_readiness_generation: session.activity.media_readiness_generation,
        });
    }

    pub(crate) fn source_is_current(self, session_address: usize, session: &crate::HlsSession) -> bool {
        self.source.is_none_or(|source| {
            source
                == super::HlsLeasePublicationSource {
                    session_address,
                    origin_epoch: session.origin_epoch,
                    origin_work_generation: session.activity.origin_work_generation,
                    origin_commits: session.origin_manifest_commits(),
                    media_readiness_generation: session.activity.media_readiness_generation,
                }
        })
    }
    pub const fn entry_generation(self) -> u64 { self.entry_generation }

    pub const fn lease_state(self) -> super::HlsAccessLeaseState { self.state }

    pub fn entry_strip_policy(self) -> Option<tuliprox_core::model::StripConfig> {
        self.strip_policy.map(|(mode, value)| tuliprox_core::model::StripConfig { mode, value })
    }

    pub(crate) fn bind_strip_policy(&mut self, strip: &tuliprox_core::model::StripConfig) {
        self.strip_policy = Some((strip.mode, strip.value));
    }

    pub(crate) fn strip_policy_matches(self, strip: &tuliprox_core::model::StripConfig) -> bool {
        self.strip_policy.is_none_or(|policy| policy == (strip.mode, strip.value))
    }

    pub(crate) fn without_live_entry_admission(mut self) -> Self {
        self.admits_live_entry = false;
        self
    }
    pub const fn lease_issued_at_ms(self) -> u64 { self.issued_at_ms }
    pub(crate) const fn media_identity(self) -> HlsMediaLeaseIdentity {
        HlsMediaLeaseIdentity {
            issued_at_ms: self.issued_at_ms,
            entry_generation: Some(self.entry_generation),
            playback: HlsMediaLeasePlaybackIdentity::Live { admission_generation: self.admission_generation },
        }
    }
}

impl HlsAccessLease {
    pub fn media_identity(&self) -> Option<HlsMediaLeaseIdentity> {
        let playback = match &self.playback_mode {
            HlsLeasePlaybackMode::Live => {
                HlsMediaLeasePlaybackIdentity::Live { admission_generation: self.admission_generation }
            }
            HlsLeasePlaybackMode::TerminalTail(plan) => {
                HlsMediaLeasePlaybackIdentity::TerminalTail { generation: plan.generation }
            }
            HlsLeasePlaybackMode::TerminalCustomResponse(_)
            | HlsLeasePlaybackMode::TerminalUnavailable { .. }
            | HlsLeasePlaybackMode::Ended => return None,
        };
        Some(HlsMediaLeaseIdentity {
            issued_at_ms: self.issued_at_ms,
            entry_generation: self.admitted_entry_generation.or(self.entry_generation),
            playback,
        })
    }
}

impl HlsAccessLeaseStore {
    pub fn media_identity_is_current(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        expected: HlsMediaLeaseIdentity,
        now_ms: u64,
    ) -> bool {
        let Some(lease) = self.by_lease_id.get(lease_id) else {
            return false;
        };
        if lease.proxy_session_id != *proxy_session_id || self.refresh_access_lease_validity(lease_id, now_ms).is_err()
        {
            return false;
        }
        self.by_lease_id.get(lease_id).is_some_and(|lease| {
            lease_state_allows_use(lease.state)
                && lease.issued_at_ms == expected.issued_at_ms
                && lease.media_identity().is_some_and(|current| current.same_playback_authorization(expected))
        })
    }
}

pub fn new_hls_access_lease_id() -> HlsAccessLeaseId {
    let mut bytes = [0u8; HLS_ACCESS_LEASE_ID_BYTES];
    if OsRng.try_fill_bytes(&mut bytes).is_err() {
        rand::rng().fill_bytes(&mut bytes);
    }
    HlsAccessLeaseId(general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}
