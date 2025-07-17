use crate::api::model::active_provider_manager::ProviderAllocation;
use crate::model::{ConfigInput, ConfigInputAlias, InputUserInfo};
use jsonwebtoken::get_current_timestamp;
use log::debug;
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::RwLock;
use shared::model::InputType;

#[derive(Debug, Clone, Copy)]
pub enum ProviderConfigAllocation {
    Exhausted,
    Available,
    GracePeriod,
}

#[derive(Debug, Default, Copy, Clone)]
pub struct ProviderConfigConnection {
    current_connections: usize,
    granted_grace: bool,
    grace_ts: u64,
}

/// This struct represents an individual provider configuration with fields like:
///
/// `id`, `name`, `url`, `username`, `password`
/// `input_type`: Determines the type of input the provider supports.
/// `max_connections`: Maximum allowed concurrent connections.
/// `priority`: Priority level for selecting providers.
/// `current_connections`: A `RwLock` to safely track the number of active connections.
#[derive(Debug)]
pub struct ProviderConfig {
    pub id: u16,
    pub name: String,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub input_type: InputType,
    max_connections: usize,
    priority: i16,
    connection: RwLock<ProviderConfigConnection>,
}

impl ProviderConfig {
    pub fn new<'a, F>(cfg: &ConfigInput, get_connection: Option<F>) -> Self
    where
        F: Fn(&str) -> Option<&'a ProviderConfigConnection>,
    {
        Self {
            id: cfg.id,
            name: cfg.name.clone(),
            url: cfg.url.clone(),
            username: cfg.username.clone(),
            password: cfg.password.clone(),
            input_type: cfg.input_type,
            max_connections: cfg.max_connections as usize,
            priority: cfg.priority,
            connection: RwLock::new(get_connection.and_then(|f| f(cfg.name.as_str())).map_or_else(Default::default, Clone::clone)),
        }
    }

    pub fn new_alias<'a, F>(cfg: &ConfigInput, alias: &ConfigInputAlias, get_connection: Option<F>) -> Self
    where
        F: Fn(&str) -> Option<&'a ProviderConfigConnection>,
    {
        Self {
            id: alias.id,
            name: alias.name.clone(),
            url: alias.url.clone(),
            username: alias.username.clone(),
            password: alias.password.clone(),
            input_type: cfg.input_type,
            max_connections: alias.max_connections as usize,
            priority: alias.priority,
            connection: RwLock::new(get_connection.and_then(|f| f(alias.name.as_str())).map_or_else(Default::default, Clone::clone)),
        }
    }

    pub fn get_user_info(&self) -> Option<InputUserInfo> {
        InputUserInfo::new(self.input_type, self.username.as_deref(), self.password.as_deref(), &self.url)
    }

    #[inline]
    pub async fn is_exhausted(&self) -> bool {
        let max = self.max_connections;
        if max == 0 {
            return false;
        }
        self.connection.read().await.current_connections >= max
    }

    #[inline]
    pub async fn is_over_limit(&self, grace_period_timeout_secs: u64) -> bool {
        let max = self.max_connections;
        if max == 0 {
            return false;
        }
        let mut guard = self.connection.write().await;
        if guard.current_connections < self.max_connections {
            guard.granted_grace = false;
            guard.grace_ts = 0;
        }

        if guard.granted_grace && guard.current_connections > max {
            let now = get_current_timestamp();
            if now - guard.grace_ts <= grace_period_timeout_secs {
                // Grace timeout still active, deny connection
                debug!("Provider access denied, grace exhausted, too many connections: {}", self.name);
                return true;
            }
        }
        guard.current_connections > max
    }

    //
    // #[inline]
    // pub fn has_capacity(&self) -> bool {
    //     !self.is_exhausted()
    // }


    async fn force_allocate(&self) {
        let mut guard = self.connection.write().await;
        guard.current_connections += 1;
    }

    async fn try_allocate(&self, grace: bool, grace_period_timeout_secs: u64) -> ProviderConfigAllocation {
        let mut guard = self.connection.write().await;
        if self.max_connections == 0 {
            guard.current_connections += 1;
            return ProviderConfigAllocation::Available;
        }
        let connections = guard.current_connections;
        if connections < self.max_connections || (grace && connections <= self.max_connections) {
            if connections < self.max_connections {
                guard.granted_grace = false;
                guard.grace_ts = 0;
                guard.current_connections += 1;
                return ProviderConfigAllocation::Available;
            }

            let now = get_current_timestamp();
            if guard.granted_grace  && now - guard.grace_ts <= grace_period_timeout_secs {
                if guard.current_connections > self.max_connections && now - guard.grace_ts <= grace_period_timeout_secs {
                    // Grace timeout still active, deny connection
                    debug!("Provider access denied, grace exhausted, too many connections: {}", self.name);
                    return ProviderConfigAllocation::Exhausted;
                }
                // Grace timeout expired, reset grace counters
                guard.granted_grace = false;
                guard.grace_ts = 0;
            }
            guard.granted_grace = true;
            guard.grace_ts = now;
            guard.current_connections += 1;
            return ProviderConfigAllocation::GracePeriod;
        }
        ProviderConfigAllocation::Exhausted
    }

    // is intended to use with redirects, to cycle through provider
    // do not increment and connection counter!
    async fn get_next(&self, grace: bool, grace_period_timeout_secs: u64) -> bool {
        if self.max_connections == 0 {
            return true;
        }
        let mut guard = self.connection.write().await;
        let connections = guard.current_connections;
        if connections < self.max_connections || (grace && connections <= self.max_connections) {
            if connections < self.max_connections {
                guard.granted_grace = false;
                guard.grace_ts = 0;
            }

            let now = get_current_timestamp();
            if guard.granted_grace {
                if connections > self.max_connections && now - guard.grace_ts <= grace_period_timeout_secs {
                    // Grace timeout still active, deny connection
                    debug!("Provider access denied, grace exhausted, too many connections: {}", self.name);
                    return false;
                }
                // Grace timeout expired, reset grace counters
                guard.granted_grace = false;
                guard.grace_ts = 0;
            }
            return true;
        }
        false
    }

    pub async fn release(&self) {
        let mut guard = self.connection.write().await;
        if guard.current_connections > 0 {
            guard.current_connections -= 1;
        }

        if guard.current_connections == 0  || guard.current_connections < self.max_connections {
            guard.granted_grace = false;
            guard.grace_ts = 0;
        }
    }

    #[inline]
    pub(crate) async fn get_current_connections(&self) -> usize {
        self.connection.read().await.current_connections
    }

    #[inline]
    pub(crate) fn get_priority(&self) -> i16 {
        self.priority
    }
}

#[derive(Clone, Debug)]
pub(in crate::api::model) struct ProviderConfigWrapper {
    inner: Arc<ProviderConfig>,
}


impl ProviderConfigWrapper {
    pub fn new(cfg: ProviderConfig) -> Self {
        Self {
            inner: Arc::new(cfg)
        }
    }

    pub async fn force_allocate(&self) -> ProviderAllocation {
        self.inner.force_allocate().await;
        ProviderAllocation::Available(Arc::clone(&self.inner))
    }

    pub async fn try_allocate(&self, grace: bool, grace_period_timeout_secs: u64) -> ProviderAllocation {
        match self.inner.try_allocate(grace, grace_period_timeout_secs).await {
            ProviderConfigAllocation::Available => ProviderAllocation::Available(Arc::clone(&self.inner)),
            ProviderConfigAllocation::GracePeriod => ProviderAllocation::GracePeriod(Arc::clone(&self.inner)),
            ProviderConfigAllocation::Exhausted => ProviderAllocation::Exhausted,
        }
    }

    pub async fn get_next(&self, grace: bool, grace_period_timeout_secs: u64) -> Option<Arc<ProviderConfig>> {
        if self.inner.get_next(grace, grace_period_timeout_secs).await {
            return Some(Arc::clone(&self.inner));
        }
        None
    }

    pub async fn get_connection_info(&self) -> ProviderConfigConnection {
        let guard = self.inner.connection.read().await;
        ProviderConfigConnection {
            current_connections: guard.current_connections,
            granted_grace: guard.granted_grace,
            grace_ts: guard.grace_ts,
        }
    }
}
impl Deref for ProviderConfigWrapper {
    type Target = ProviderConfig;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}