use super::ActiveProviderManager;
use std::sync::Arc;

impl ActiveProviderManager {
    pub fn observe_account(
        &self,
        observation: tuliprox_core::model::ProviderAccountObservation,
    ) -> Option<tuliprox_core::model::ProviderAccountObservation> {
        self.core.providers.observe_account(observation)
    }

    pub fn reset_account_health(&self, name: &Arc<str>) { self.core.providers.forget_account_health(name); }
    pub fn pending_account_observations(&self) -> Vec<tuliprox_core::model::ProviderAccountObservation> {
        self.core.providers.pending_account_observations()
    }
    pub fn is_observation_pending(&self, observation: &tuliprox_core::model::ProviderAccountObservation) -> bool {
        self.core.providers.is_observation_pending(observation)
    }
    pub fn acknowledge_account_observation(&self, observation: &tuliprox_core::model::ProviderAccountObservation) {
        self.core.providers.acknowledge_account_observation(observation);
    }
    pub fn request_account_probe(&self, name: &Arc<str>) { self.core.providers.request_account_probe(name); }
    pub fn is_account_probe_requested(&self, name: &Arc<str>) -> bool {
        self.core.providers.is_account_probe_requested(name)
    }
    pub fn clear_account_probe(&self, name: &Arc<str>) { self.core.providers.clear_account_probe(name); }
    pub async fn account_health_changed(&self) { self.core.providers.account_health_changed.notified().await; }
}
