use super::{HlsAccessLeaseId, ProxySessionId};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use tuliprox_core::model::AppConfig;
use tuliprox_session::StreamMeterHandle;

/// Runtime decision for Shared-HLS `QoS` hooks derived from global reverse proxy settings.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct HlsQosRuntimeConfig {
    pub live_metering_enabled: bool,
}

impl HlsQosRuntimeConfig {
    pub fn from_app_config(app_config: &AppConfig) -> Self {
        let config = app_config.config.load();
        let Some(reverse_proxy) = config.reverse_proxy.as_ref() else {
            return Self::default();
        };
        Self { live_metering_enabled: reverse_proxy.stream.as_ref().is_some_and(|stream| stream.metrics_enabled) }
    }
}

pub struct HlsQosMeterInit {
    pub meter_uid: u32,
    pub meter: Arc<StreamMeterHandle>,
}

pub struct HlsQosRegistration {
    pub meter_uid: u32,
    pub meter: Option<Arc<StreamMeterHandle>>,
    pub register_meter: Option<Arc<StreamMeterHandle>>,
    pub emit_connect_record: bool,
}

#[derive(Clone)]
struct HlsAccessLeaseQosState {
    proxy_session_id: ProxySessionId,
    meter_uid: u32,
    meter: Option<Arc<StreamMeterHandle>>,
}

impl HlsAccessLeaseQosState {
    fn registration(
        &self,
        emit_connect_record: bool,
        register_meter: Option<Arc<StreamMeterHandle>>,
    ) -> HlsQosRegistration {
        HlsQosRegistration { meter_uid: self.meter_uid, meter: self.meter.clone(), register_meter, emit_connect_record }
    }
}

#[derive(Default)]
pub struct HlsQosRegistry {
    states: RwLock<HashMap<HlsAccessLeaseId, HlsAccessLeaseQosState>>,
}

impl HlsQosRegistry {
    pub async fn ensure_access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        _now_ms: u64,
        meter_init: Option<HlsQosMeterInit>,
    ) -> HlsQosRegistration {
        let mut states = self.states.write().await;
        if let Some(state) = states.get(lease_id) {
            return state.registration(false, None);
        }

        let (meter_uid, meter) = meter_init.map_or((0, None), |init| (init.meter_uid, Some(init.meter)));
        let register_meter = meter.clone();
        let state = HlsAccessLeaseQosState { proxy_session_id: proxy_session_id.clone(), meter_uid, meter };
        let registration = state.registration(true, register_meter);
        states.insert(lease_id.clone(), state);
        registration
    }

    pub async fn meter_for_access_lease(&self, lease_id: &HlsAccessLeaseId) -> Option<Arc<StreamMeterHandle>> {
        self.states.read().await.get(lease_id).and_then(|state| state.meter.clone())
    }

    pub async fn remove_access_lease(&self, lease_id: &HlsAccessLeaseId) -> bool {
        self.states.write().await.remove(lease_id).is_some()
    }

    pub async fn remove_access_leases(&self, lease_ids: &[HlsAccessLeaseId]) -> usize {
        let mut states = self.states.write().await;
        lease_ids.iter().filter(|lease_id| states.remove(*lease_id).is_some()).count()
    }

    pub async fn remove_proxy_session_state(&self, proxy_session_id: &ProxySessionId) -> usize {
        let mut states = self.states.write().await;
        let before = states.len();
        states.retain(|_, state| &state.proxy_session_id != proxy_session_id);
        before.saturating_sub(states.len())
    }

    pub async fn clear(&self) -> usize {
        let mut states = self.states.write().await;
        let removed = states.len();
        states.clear();
        removed
    }

    pub async fn len(&self) -> usize {
        self.states.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.states.read().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{HlsQosMeterInit, HlsQosRegistry};
    use crate::{HlsAccessLeaseId, ProxySessionId};
    use std::sync::Arc;
    use tuliprox_session::{EventManager, StreamMeterHandle};

    #[tokio::test]
    async fn qos_registration_emits_connect_once_per_access_lease() {
        let registry = HlsQosRegistry::default();
        let event_manager = Arc::new(EventManager::new());
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let proxy_session_id = ProxySessionId("session-a".to_string());
        let meter = Arc::new(StreamMeterHandle::new(7, Arc::downgrade(&event_manager)));

        let first = registry
            .ensure_access_lease(
                &lease_id,
                &proxy_session_id,
                100,
                Some(HlsQosMeterInit { meter_uid: 7, meter: Arc::clone(&meter) }),
            )
            .await;
        let second = registry.ensure_access_lease(&lease_id, &proxy_session_id, 200, None).await;

        assert!(first.emit_connect_record);
        assert_eq!(first.meter_uid, 7);
        assert!(first.register_meter.is_some());
        assert!(!second.emit_connect_record);
        assert_eq!(second.meter_uid, 7);
        assert!(second.register_meter.is_none());
        assert!(Arc::ptr_eq(second.meter.as_ref().expect("meter must be retained"), &meter));
    }

    #[tokio::test]
    async fn qos_cleanup_removes_only_matching_session_state() {
        let registry = HlsQosRegistry::default();
        let session_a = ProxySessionId("session-a".to_string());
        let session_b = ProxySessionId("session-b".to_string());
        let lease_a = HlsAccessLeaseId("lease-a".to_string());
        let lease_b = HlsAccessLeaseId("lease-b".to_string());

        registry.ensure_access_lease(&lease_a, &session_a, 100, None).await;
        registry.ensure_access_lease(&lease_b, &session_b, 100, None).await;

        assert_eq!(registry.remove_proxy_session_state(&session_a).await, 1);
        assert_eq!(registry.len().await, 1);
        assert!(registry.meter_for_access_lease(&lease_a).await.is_none());
        assert!(registry.remove_access_lease(&lease_b).await);
    }
}
