//! Session-owned permission for an origin's unusually short live playlists.

use crate::{
    manifest_acceptance::HlsManifestAcceptanceGeneration, manifest_origin_binding::HlsManifestOriginBinding,
    media_reserve::HlsManifestCommitIdentity, HlsSession,
};

/// The validated origin foundation, deliberately independent of window movement and leases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HlsOriginQualificationBasis {
    epoch: u64,
    work_generation: u64,
    binding: Option<HlsManifestOriginBinding>,
    host: Option<String>,
}

impl HlsOriginQualificationBasis {
    pub(crate) fn capture(session: &HlsSession) -> Self {
        Self {
            epoch: session.origin_epoch,
            work_generation: session.activity.origin_work_generation,
            binding: session.origin_control.manifest_origin_binding.clone(),
            host: session.origin_control.pinned_host.clone(),
        }
    }
}

/// Window sizes accepted for the current validated origin foundation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HlsQualifiedOriginWindows {
    Regular,
    ShortAllowed,
}

/// One shared qualification and its retry receipt; never consumed by a publication.
#[derive(Clone, Debug, Default)]
pub(crate) enum HlsOriginQualification {
    #[default]
    Unchecked,
    Checking {
        basis: HlsOriginQualificationBasis,
        generation: HlsManifestAcceptanceGeneration,
        target: HlsManifestOriginBinding,
    },
    Qualified {
        basis: HlsOriginQualificationBasis,
        generation: HlsManifestAcceptanceGeneration,
        windows: HlsQualifiedOriginWindows,
        /// Last publication when this origin foundation was qualified.
        publication_boundary: Option<HlsManifestCommitIdentity>,
    },
    RetryPending {
        basis: HlsOriginQualificationBasis,
        generation: HlsManifestAcceptanceGeneration,
        target: HlsManifestOriginBinding,
    },
}

impl HlsOriginQualification {
    pub(crate) fn qualified_windows(&self, basis: &HlsOriginQualificationBasis) -> Option<HlsQualifiedOriginWindows> {
        match self {
            Self::Qualified { basis: current, windows, .. } if current == basis => Some(*windows),
            Self::Unchecked | Self::Checking { .. } | Self::Qualified { .. } | Self::RetryPending { .. } => None,
        }
    }

    pub(crate) fn pending_is_current(&self, basis: &HlsOriginQualificationBasis) -> bool {
        match self {
            Self::Checking { basis: current, .. } | Self::RetryPending { basis: current, .. } => current == basis,
            Self::Unchecked | Self::Qualified { .. } => true,
        }
    }

    pub(crate) fn retry_target(&self, basis: &HlsOriginQualificationBasis) -> Option<&HlsManifestOriginBinding> {
        match self {
            Self::RetryPending { basis: current, target, .. } if current == basis => Some(target),
            Self::Unchecked | Self::Checking { .. } | Self::Qualified { .. } | Self::RetryPending { .. } => None,
        }
    }

    pub(crate) fn record_failure(&mut self) {
        if let Self::Checking { basis, generation, target } = self {
            *self = Self::RetryPending { basis: basis.clone(), generation: *generation, target: target.clone() };
        }
    }

    pub(crate) const fn generation(&self) -> Option<HlsManifestAcceptanceGeneration> {
        match self {
            Self::Unchecked => None,
            Self::Checking { generation, .. }
            | Self::Qualified { generation, .. }
            | Self::RetryPending { generation, .. } => Some(*generation),
        }
    }
}

impl HlsSession {
    pub(crate) fn origin_qualification_retry_target(&self) -> Option<&HlsManifestOriginBinding> {
        // Deterministic conflicts retain their own ordinary-sample path. Only
        // changed receipt evidence may authorize a replacement full burst.
        if self
            .origin_control
            .acceptance_episode
            .as_ref()
            .is_some_and(|episode| episode.deterministic_conflict_receipt().is_some())
        {
            return None;
        }
        self.origin_control.qualification.retry_target(&HlsOriginQualificationBasis::capture(self))
    }

    pub(crate) fn origin_is_qualified(&self) -> bool {
        self.origin_control.qualification.qualified_windows(&HlsOriginQualificationBasis::capture(self)).is_some()
    }

    pub(crate) fn origin_qualification_awaits_publication(&self) -> bool {
        match &self.origin_control.qualification {
            HlsOriginQualification::Qualified { basis, publication_boundary, .. }
                if basis == &HlsOriginQualificationBasis::capture(self) =>
            {
                self.last_normal_manifest_commit_identity() == *publication_boundary
            }
            HlsOriginQualification::Unchecked
            | HlsOriginQualification::Checking { .. }
            | HlsOriginQualification::Qualified { .. }
            | HlsOriginQualification::RetryPending { .. } => false,
        }
    }

    pub(crate) fn allows_short_origin_windows(&self) -> bool {
        self.origin_control.qualification.qualified_windows(&HlsOriginQualificationBasis::capture(self))
            == Some(HlsQualifiedOriginWindows::ShortAllowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HlsSessionKey;

    #[test]
    fn permission_survives_progress_but_not_origin_foundation_changes() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "short"), b"secret", 0);
        session.origin_control.qualification = HlsOriginQualification::Qualified {
            basis: HlsOriginQualificationBasis::capture(&session),
            generation: HlsManifestAcceptanceGeneration(1),
            windows: HlsQualifiedOriginWindows::ShortAllowed,
            publication_boundary: None,
        };
        for progress in 1..=10 {
            session.origin_seq_highwater = Some(progress);
            session.origin_control.record_media_progress(progress, 4_000);
            assert!(session.allows_short_origin_windows());
        }
        session.origin_epoch += 1;
        assert!(!session.allows_short_origin_windows());
        session.origin_epoch -= 1;
        session.activity.origin_work_generation += 1;
        assert!(!session.allows_short_origin_windows());
        session.activity.origin_work_generation -= 1;
        session.origin_control.pinned_host = Some("changed.example".to_string());
        assert!(!session.allows_short_origin_windows());
    }

    #[test]
    fn unchecked_regular_checking_and_failed_results_do_not_permit_short_publication() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "short"), b"secret", 0);
        assert!(!session.allows_short_origin_windows());
        let basis = HlsOriginQualificationBasis::capture(&session);
        let generation = HlsManifestAcceptanceGeneration(1);
        session.origin_control.qualification = HlsOriginQualification::Qualified {
            basis: basis.clone(),
            generation,
            windows: HlsQualifiedOriginWindows::Regular,
            publication_boundary: None,
        };
        assert!(!session.allows_short_origin_windows());
        session.origin_control.qualification = HlsOriginQualification::Checking {
            basis,
            generation,
            target: HlsManifestOriginBinding::new(url::Url::parse("https://origin.example/live.m3u8").unwrap(), None)
                .unwrap(),
        };
        assert!(!session.allows_short_origin_windows());
        session.origin_control.qualification.record_failure();
        assert!(!session.allows_short_origin_windows());
    }

    #[test]
    fn pending_publication_tracks_current_qualification_and_commit_generation() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "revalidation"), b"secret", 0);
        let first = session.next_manifest_commit_identity(10).unwrap();
        session.record_normal_manifest_commit_identity(first);
        session.origin_control.qualification = HlsOriginQualification::Qualified {
            basis: HlsOriginQualificationBasis::capture(&session),
            generation: HlsManifestAcceptanceGeneration(2),
            windows: HlsQualifiedOriginWindows::ShortAllowed,
            publication_boundary: Some(first),
        };
        assert!(session.origin_qualification_awaits_publication());
        let next = session.next_manifest_commit_identity(10).unwrap();
        session.record_normal_manifest_commit_identity(next);
        assert!(!session.origin_qualification_awaits_publication(), "equal timestamps still identify distinct commits");
        session.origin_control.qualification = HlsOriginQualification::Qualified {
            basis: HlsOriginQualificationBasis::capture(&session),
            generation: HlsManifestAcceptanceGeneration(3),
            windows: HlsQualifiedOriginWindows::ShortAllowed,
            publication_boundary: Some(next),
        };
        assert!(session.origin_qualification_awaits_publication());
        session.origin_epoch += 1;
        assert!(!session.origin_qualification_awaits_publication());
    }
}
