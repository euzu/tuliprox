//! What a provider has already told us about itself.
//!
//! Capability knowledge was scattered and then thrown away on every refresh. Whether a
//! portal supports `get_all_channels` was inferred from the *shape of the error* it
//! returned; which bootstrap recipe worked was discovered by walking a five-entry fallback
//! chain from the top; which of three endpoint candidates answered was rediscovered per
//! call. None of it survived the run, so the next refresh re-probed endpoints already
//! known to 404 and re-walked a chain whose answer was already known.
//!
//! Re-probing is not free in the way a cache miss usually is. A handshake chain replayed
//! from scratch against a portal with stale credentials looks, from the provider's side,
//! a great deal like credential stuffing — which is the failure mode the Xtream side
//! already carries a standing TODO about.
//!
//! A snapshot is a hint, never a contract: a provider that starts supporting an action is
//! free to do so, which is what [`ProviderCapabilities::is_stale`] and the re-probe
//! interval are for.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// How long a snapshot is trusted before the client re-probes what it says is missing.
/// A day: long enough to be worth persisting, short enough that a provider fixing its
/// portal is picked up without anyone clearing state by hand.
pub const CAPABILITY_TTL_SECS: u64 = 24 * 60 * 60;

/// What one provider has proven about itself, as of `observed_at_epoch_secs`.
///
/// `BTreeSet` rather than `HashSet` so the persisted form has a stable field order and a
/// snapshot that has not changed does not rewrite the file with reshuffled contents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    /// Actions the provider answered in a way that means "I do not implement this".
    /// Distinct from an action that merely failed — see
    /// [`StalkerError::is_unsupported_catalog_action`](crate::stalker::error::StalkerError::is_unsupported_catalog_action).
    #[serde(default)]
    pub unsupported_actions: BTreeSet<String>,
    /// The bootstrap recipe that last completed a handshake, so the fallback chain can
    /// start where it previously ended rather than at the top.
    #[serde(default)]
    pub bootstrap_recipe: Option<String>,
    /// The endpoint candidate that last answered.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// When this snapshot was taken. Zero means "never observed".
    #[serde(default)]
    pub observed_at_epoch_secs: u64,
}

impl ProviderCapabilities {
    /// True once the snapshot is old enough that its negative claims should be re-tested.
    ///
    /// A clock that has gone backwards reports "not stale" rather than treating the
    /// snapshot as infinitely old, which would re-probe every action on every call.
    #[must_use]
    pub fn is_stale(&self, now_epoch_secs: u64) -> bool {
        if self.observed_at_epoch_secs == 0 {
            return true;
        }
        now_epoch_secs.saturating_sub(self.observed_at_epoch_secs) >= CAPABILITY_TTL_SECS
    }

    /// Whether `action` is known not to work, according to a snapshot still worth
    /// believing. A stale snapshot answers `false` — the action gets re-probed.
    #[must_use]
    pub fn is_unsupported(&self, action: &str, now_epoch_secs: u64) -> bool {
        !self.is_stale(now_epoch_secs) && self.unsupported_actions.contains(action)
    }

    /// Record that `action` is not implemented by this provider. Returns whether this
    /// changed anything, so a caller can skip a write that would persist nothing new.
    pub fn record_unsupported(&mut self, action: &str, now_epoch_secs: u64) -> bool {
        self.observed_at_epoch_secs = now_epoch_secs;
        self.unsupported_actions.insert(action.to_string())
    }

    /// Record that `action` worked after all, undoing an earlier negative claim.
    pub fn record_supported(&mut self, action: &str, now_epoch_secs: u64) -> bool {
        self.observed_at_epoch_secs = now_epoch_secs;
        self.unsupported_actions.remove(action)
    }

    /// Record the recipe and endpoint that completed a handshake. Returns whether either
    /// changed.
    pub fn record_handshake(&mut self, recipe: &str, endpoint: &str, now_epoch_secs: u64) -> bool {
        self.observed_at_epoch_secs = now_epoch_secs;
        let changed =
            self.bootstrap_recipe.as_deref() != Some(recipe) || self.endpoint.as_deref() != Some(endpoint);
        self.bootstrap_recipe = Some(recipe.to_string());
        self.endpoint = Some(endpoint.to_string());
        changed
    }

    /// The recipe that last completed a handshake, when the snapshot is still worth
    /// believing.
    #[must_use]
    pub fn remembered_recipe(&self, now_epoch_secs: u64) -> Option<&str> {
        self.bootstrap_recipe.as_deref().filter(|_| !self.is_stale(now_epoch_secs))
    }

    /// Reorder `candidates` so the remembered one is tried first, keeping the rest in
    /// their configured order.
    ///
    /// Reordering rather than replacing: the remembered answer is a hint, and a portal
    /// that has moved must still be reachable through the others.
    #[must_use]
    pub fn prefer_remembered<T, K>(&self, candidates: Vec<T>, key: K, now_epoch_secs: u64) -> Vec<T>
    where
        K: Fn(&T) -> String,
    {
        let Some(remembered) = self.endpoint.as_deref().filter(|_| !self.is_stale(now_epoch_secs)) else {
            return candidates;
        };
        let Some(position) = candidates.iter().position(|candidate| key(candidate) == remembered) else {
            return candidates;
        };
        let mut candidates = candidates;
        candidates.swap(0, position);
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::{ProviderCapabilities, CAPABILITY_TTL_SECS};

    const NOW: u64 = 1_700_000_000;

    #[test]
    fn a_never_observed_snapshot_is_stale_and_claims_nothing() {
        let capabilities = ProviderCapabilities::default();
        assert!(capabilities.is_stale(NOW));
        assert!(!capabilities.is_unsupported("get_all_channels", NOW));
    }

    #[test]
    fn a_negative_claim_expires_so_a_fixed_portal_is_retried() {
        let mut capabilities = ProviderCapabilities::default();
        assert!(capabilities.record_unsupported("get_all_channels", NOW));

        assert!(capabilities.is_unsupported("get_all_channels", NOW));
        assert!(capabilities.is_unsupported("get_all_channels", NOW + CAPABILITY_TTL_SECS - 1));
        assert!(!capabilities.is_unsupported("get_all_channels", NOW + CAPABILITY_TTL_SECS));
    }

    #[test]
    fn recording_the_same_claim_twice_reports_no_change() {
        let mut capabilities = ProviderCapabilities::default();
        assert!(capabilities.record_unsupported("get_all_channels", NOW));
        assert!(!capabilities.record_unsupported("get_all_channels", NOW), "a no-op must not trigger a write");
    }

    #[test]
    fn an_action_that_starts_working_can_be_taken_back() {
        let mut capabilities = ProviderCapabilities::default();
        capabilities.record_unsupported("get_all_channels", NOW);
        assert!(capabilities.record_supported("get_all_channels", NOW));
        assert!(!capabilities.is_unsupported("get_all_channels", NOW));
    }

    #[test]
    fn a_backwards_clock_does_not_invalidate_the_snapshot() {
        let mut capabilities = ProviderCapabilities::default();
        capabilities.record_unsupported("get_all_channels", NOW);
        assert!(capabilities.is_unsupported("get_all_channels", 1));
    }

    #[test]
    fn the_remembered_endpoint_is_tried_first_and_the_rest_still_follow() {
        let mut capabilities = ProviderCapabilities::default();
        capabilities.record_handshake("GenericSafe", "portal.php", NOW);

        let ordered = capabilities.prefer_remembered(
            vec!["server/load.php", "portal.php", "c/"],
            |candidate| (*candidate).to_string(),
            NOW,
        );

        assert_eq!(ordered, vec!["portal.php", "server/load.php", "c/"]);
    }

    #[test]
    fn an_unknown_or_stale_remembered_endpoint_leaves_the_order_alone() {
        let mut capabilities = ProviderCapabilities::default();
        capabilities.record_handshake("GenericSafe", "gone.php", NOW);
        let candidates = vec!["server/load.php", "portal.php"];

        let key = |candidate: &&str| (*candidate).to_string();
        assert_eq!(capabilities.prefer_remembered(candidates.clone(), key, NOW), candidates);
        assert_eq!(
            capabilities.prefer_remembered(candidates.clone(), key, NOW + CAPABILITY_TTL_SECS),
            candidates
        );
    }

    #[test]
    fn a_stale_snapshot_stops_recommending_its_recipe() {
        let mut capabilities = ProviderCapabilities::default();
        capabilities.record_handshake("StrictMag", "portal.php", NOW);

        assert_eq!(capabilities.remembered_recipe(NOW), Some("StrictMag"));
        assert_eq!(capabilities.remembered_recipe(NOW + CAPABILITY_TTL_SECS), None);
    }

    #[test]
    fn recording_an_unchanged_handshake_reports_no_change() {
        let mut capabilities = ProviderCapabilities::default();
        assert!(capabilities.record_handshake("GenericSafe", "portal.php", NOW));
        assert!(!capabilities.record_handshake("GenericSafe", "portal.php", NOW + 5));
    }

    #[test]
    fn the_persisted_form_round_trips() -> Result<(), serde_json::Error> {
        let mut capabilities = ProviderCapabilities::default();
        capabilities.record_unsupported("get_all_channels", NOW);
        capabilities.record_handshake("StrictMag", "server/load.php", NOW);

        let restored: ProviderCapabilities = serde_json::from_str(&serde_json::to_string(&capabilities)?)?;
        assert_eq!(restored, capabilities);
        Ok(())
    }
}
