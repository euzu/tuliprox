use std::fmt;
use crate::model::{is_input_expired, ConfigInput, ConfigInputAlias, InputUserInfo};
use jsonwebtoken::get_current_timestamp;
use log::{debug};
use std::ops::Deref;
use std::sync::{Arc};
use tokio::sync::RwLock;
use shared::model::InputType;
use shared::utils::sanitize_sensitive_info;
use shared::write_if_some;
use crate::api::model::ProviderAllocation;
use crate::utils::debug_if_enabled;


pub type ProviderConnectionChangeCallback = Arc<dyn Fn(&Arc<str>, usize) + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub enum ProviderConfigAllocation {
    Exhausted,
    Available,
    GracePeriod,
}

#[derive(Debug, Default, Copy, Clone)]
pub struct ProviderConfigConnection {
    pub(crate) current_connections: usize,
    pub(crate) granted_grace: bool,
    pub(crate) grace_ts: u64,
}

/// This struct represents an individual provider configuration with fields like:
///
/// `id`, `name`, `url`, `username`, `password`
/// `input_type`: Determines the type of input the provider supports.
/// `max_connections`: Maximum allowed concurrent connections.
/// `priority`: Priority level for selecting providers.
/// `current_connections`: A `RwLock` to safely track the number of active connections.
pub struct ProviderConfig {
    pub id: u16,
    pub name: Arc<str>,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub input_type: InputType,
    max_connections: usize,
    priority: i16,
    exp_date: Option<i64>,
    connection: Arc<RwLock<ProviderConfigConnection>>,
    on_connection_change: ProviderConnectionChangeCallback,
}

impl fmt::Display for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProviderConfig {{")?;
        write!(f, "  id: {}", self.id)?;
        write!(f, ", name: {}", self.name)?;
        write!(f, ", url: {}", self.url)?;
        write!(f, ", input_type: {:?}", self.input_type)?;
        write!(f, ", max_connections: {}", self.max_connections)?;
        write!(f, ", priority: {}", self.priority)?;
        write_if_some!(f, self,
            ", username: " => username,
            ", password: " => password,
            ", exp_date: " => exp_date
        );
        write!(f, "}}")?;
        Ok(())
    }
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl PartialEq for ProviderConfig {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.name == other.name
            && self.url == other.url
            && self.username == other.username
            && self.password == other.password
            && self.input_type == other.input_type
            && self.max_connections == other.max_connections
            && self.priority == other.priority
            && self.exp_date == other.exp_date
           // Note: self.connection is skipped
    }
}

macro_rules! modify_connections {
    ($self:ident, $guard:ident, +1) => {{
        $guard.current_connections += 1;
        $self.notify_connection_change($guard.current_connections);
    }};
    ($self:ident, $guard:ident, -1) => {{
        $guard.current_connections = $guard.current_connections.saturating_sub(1);
        $self.notify_connection_change($guard.current_connections);
    }};
}

impl ProviderConfig {
    pub fn new(cfg: &ConfigInput, connection: Arc<RwLock<ProviderConfigConnection>>, on_connection_change: ProviderConnectionChangeCallback) -> Self {
        let panel_api_enabled = cfg.panel_api.as_ref().is_some_and(|panel_api| panel_api.enabled);
        // Logic change: panel api accounts are not considering unlimited provider access!
        let effective_max_connections = if panel_api_enabled && cfg.max_connections == 0 {
            debug_if_enabled!(
                "panel_api: input '{}' has max_connections=0; defaulting effective max_connections to 1 for pool accounting",
                cfg.name
            );
            1usize
        } else {
            cfg.max_connections as usize
        };
        Self {
            id: cfg.id,
            name: cfg.name.clone(),
            url: cfg.url.clone(),
            username: cfg.username.clone(),
            password: cfg.password.clone(),
            input_type: cfg.input_type,
            max_connections: effective_max_connections,
            priority: cfg.priority,
            exp_date: cfg.exp_date,
            connection,
            on_connection_change
        }
    }

    pub fn new_alias(
        cfg: &ConfigInput,
        alias: &ConfigInputAlias,
        connection: Arc<RwLock<ProviderConfigConnection>>,
        on_connection_change: ProviderConnectionChangeCallback,
    ) -> Self {
        let panel_api_enabled = cfg.panel_api.as_ref().is_some_and(|panel_api| panel_api.enabled);
        let effective_max_connections = if panel_api_enabled && alias.max_connections == 0 {
            debug_if_enabled!(
                "panel_api: alias '{}' has max_connections=0; defaulting effective max_connections to 1 for pool accounting",
                alias.name
            );
            1usize
        } else {
            alias.max_connections as usize
        };
        Self {
            id: alias.id,
            name: alias.name.clone(),
            url: alias.url.clone(),
            username: alias.username.clone(),
            password: alias.password.clone(),
            input_type: cfg.input_type,
            max_connections: effective_max_connections,
            priority: alias.priority,
            exp_date: alias.exp_date,
            connection,
            on_connection_change,
        }
    }

    #[inline]
    pub fn max_connections(&self) -> usize {
        self.max_connections
    }

    #[inline]
    pub(crate) fn exp_date(&self) -> Option<i64> {
        self.exp_date
    }

    pub fn get_user_info(&self) -> Option<InputUserInfo> {
        InputUserInfo::new(self.input_type, self.username.as_deref(), self.password.as_deref(), &self.url)
    }

    fn notify_connection_change(&self, new_connections: usize) {
        (self.on_connection_change)(&self.name, new_connections);
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
                debug!("Provider access denied, grace exhausted, too many connections, over limit: {}", self.name);
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

    async fn force_allocate(&self) -> bool {
        if is_input_expired(self.exp_date) {
            return false;
        }
        let mut guard = self.connection.write().await;
        modify_connections!(self, guard, +1);
        true
    }

    async fn try_allocate(&self, grace: bool, grace_period_timeout_secs: u64) -> ProviderConfigAllocation {
        if is_input_expired(self.exp_date) {
            return ProviderConfigAllocation::Exhausted;
        }

        let mut guard = self.connection.write().await;
        if self.max_connections == 0 {
            modify_connections!(self, guard, +1);
            return ProviderConfigAllocation::Available;
        }
        let connections = guard.current_connections;
        if connections < self.max_connections || (grace && connections <= self.max_connections) {
            if connections < self.max_connections {
                guard.granted_grace = false;
                guard.grace_ts = 0;
                modify_connections!(self, guard, +1);
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
            debug_if_enabled!(
                "Provider {} granting grace allocation (current_connections={}, max_connections={})",
                sanitize_sensitive_info(&self.name),
                connections,
                self.max_connections
            );
            guard.granted_grace = true;
            guard.grace_ts = now;
            modify_connections!(self, guard, +1);
            return ProviderConfigAllocation::GracePeriod;
        }
        ProviderConfigAllocation::Exhausted
    }

    // is intended to use with redirects, to cycle through provider
    // do not increment and connection counter!
    async fn get_next(&self, grace: bool, grace_period_timeout_secs: u64) -> bool {
        if is_input_expired(self.exp_date) {
            return false;
        }

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
                    debug!("Provider access denied, grace exhausted, too many connections, no connection available: {}", self.name);
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
        if guard.current_connections == 1 || guard.current_connections > self.max_connections {
            guard.granted_grace = false;
            guard.grace_ts = 0;
        }
        if guard.current_connections > 0 {
            modify_connections!(self, guard, -1);
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

impl fmt::Display for ProviderConfigWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.inner)
    }
}

impl ProviderConfigWrapper {
    pub fn new(cfg: ProviderConfig) -> Self {
        Self {
            inner: Arc::new(cfg)
        }
    }

    pub async fn force_allocate(&self) -> ProviderAllocation {
        if self.inner.force_allocate().await {
            ProviderAllocation::new_available(Arc::clone(&self.inner))
        } else {
            ProviderAllocation::Exhausted
        }
    }

    pub async fn try_allocate(&self, grace: bool, grace_period_timeout_secs: u64) -> ProviderAllocation {
        match self.inner.try_allocate(grace, grace_period_timeout_secs).await {
            ProviderConfigAllocation::Available => ProviderAllocation::new_available(Arc::clone(&self.inner)),
            ProviderConfigAllocation::GracePeriod => ProviderAllocation::new_grace_period(Arc::clone(&self.inner)),
            ProviderConfigAllocation::Exhausted => ProviderAllocation::Exhausted,
        }
    }

    pub async fn get_next(&self, grace: bool, grace_period_timeout_secs: u64) -> Option<Arc<ProviderConfig>> {
        if self.inner.get_next(grace, grace_period_timeout_secs).await {
            return Some(Arc::clone(&self.inner));
        }
        None
    }
}
impl Deref for ProviderConfigWrapper {
    type Target = ProviderConfig;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
