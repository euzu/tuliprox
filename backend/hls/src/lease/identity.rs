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
    pub const fn is_live(self) -> bool { matches!(self.playback, HlsMediaLeasePlaybackIdentity::Live { .. }) }

    pub const fn lease_issued_at_ms(self) -> u64 { self.issued_at_ms }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum HlsMediaLeasePlaybackIdentity {
    Live { admission_generation: u64 },
    TerminalTail { generation: HlsTerminalTailGeneration },
}

impl HlsLeaseManifestPublicationGuard {
    pub(crate) const fn lease_issued_at_ms(self) -> u64 { self.issued_at_ms }
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
            HlsLeasePlaybackMode::TerminalUnavailable { .. } | HlsLeasePlaybackMode::Ended => return None,
        };
        Some(HlsMediaLeaseIdentity { issued_at_ms: self.issued_at_ms, playback })
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
                && lease.media_identity() == Some(expected)
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
