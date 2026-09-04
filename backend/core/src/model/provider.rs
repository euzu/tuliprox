//! Provider configuration, allocation and the handle that represents a claim.
//!
//! These are the value types of provider brokerage: what a provider is, whether
//! one is available, and the handle a caller holds while using it. The manager
//! that allocates them stays in `api`; these do not, because playlist processing
//! passes them around and would otherwise have to name the API layer.

use crate::{
    model::{is_input_expired, ConfigInput, ConfigInputAlias, InputUserInfo},
    utils::debug_if_enabled,
};
use log::debug;
use shared::{model::InputType, utils::sanitize_sensitive_info, write_if_some};
use std::{fmt, net::SocketAddr, ops::Deref, sync::Arc, time::Duration};
use tokio::{sync::RwLock, time::Instant};
use tokio_util::sync::CancellationToken;

/// Address of the client holding a provider connection.
pub type ClientConnectionId = SocketAddr;
/// Identifier of one allocation within a provider's lineup.
pub type AllocationId = u64;

#[derive(Debug, Clone)]
pub enum ProviderAllocation {
    Exhausted,
    Available(Arc<ProviderConfig>),
    GracePeriod(Arc<ProviderConfig>),
}

impl ProviderAllocation {
    /// The provider this allocation landed on, if it landed on one.
    ///
    /// Both success variants carry a provider and callers that only need
    /// "which one" should not have to match on why it was granted.
    #[must_use]
    pub fn provider(&self) -> Option<&Arc<ProviderConfig>> {
        match self {
            Self::Exhausted => None,
            Self::Available(config) | Self::GracePeriod(config) => Some(config),
        }
    }

    pub fn short_key(&self) -> &str {
        match self {
            ProviderAllocation::Exhausted => "exhausted",
            ProviderAllocation::Available(_) => "available",
            ProviderAllocation::GracePeriod(_) => "grace_period",
        }
    }

    pub fn new_available(config: Arc<ProviderConfig>) -> Self { ProviderAllocation::Available(config) }

    pub fn new_grace_period(config: Arc<ProviderConfig>) -> Self { ProviderAllocation::GracePeriod(config) }

    pub fn get_provider_name(&self) -> Option<Arc<str>> {
        match self {
            ProviderAllocation::Exhausted => None,
            ProviderAllocation::Available(ref cfg) | ProviderAllocation::GracePeriod(ref cfg) => Some(cfg.name.clone()),
        }
    }

    pub fn get_provider_id(&self) -> Option<u16> {
        match self {
            ProviderAllocation::Exhausted => None,
            ProviderAllocation::Available(ref cfg) | ProviderAllocation::GracePeriod(ref cfg) => Some(cfg.id),
        }
    }

    pub fn get_provider_config(&self) -> Option<Arc<ProviderConfig>> {
        match self {
            ProviderAllocation::Exhausted => None,
            ProviderAllocation::Available(ref cfg) | ProviderAllocation::GracePeriod(ref cfg) => Some(Arc::clone(cfg)),
        }
    }

    pub async fn release(&self) {
        match &self {
            ProviderAllocation::Exhausted => {}
            ProviderAllocation::Available(config) | ProviderAllocation::GracePeriod(config) => {
                config.release().await;
            }
        }
    }

    #[inline]
    pub fn is_unlimited_provider(&self) -> bool {
        matches!(self, Self::Available(c) | Self::GracePeriod(c) if c.is_unlimited())
    }
}

#[derive(Debug, Clone)]
pub struct ProviderHandle {
    pub client_id: ClientConnectionId,
    pub allocation_id: AllocationId,
    pub allocation: ProviderAllocation,
    // Token to cancel the background task (e.g. internal probe) if preempted
    pub cancel_token: Option<CancellationToken>,
}

impl ProviderHandle {
    pub fn new(
        client_id: ClientConnectionId,
        allocation_id: AllocationId,
        allocation: ProviderAllocation,
        cancel_token: Option<CancellationToken>,
    ) -> Self {
        Self { client_id, allocation_id, allocation, cancel_token }
    }
}

pub type ProviderConnectionChangeCallback = Arc<dyn Fn(&Arc<str>, usize) + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub enum ProviderConfigAllocation {
    Exhausted,
    Available,
    GracePeriod,
}

#[derive(Debug, Default, Copy, Clone)]
pub struct ProviderConfigConnection {
    pub current_connections: usize,
    pub grace_started_at: Option<Instant>,
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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{self}") }
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
    fn grace_is_active(connection: &ProviderConfigConnection, now: Instant, grace_period_timeout_secs: u64) -> bool {
        connection.grace_started_at.is_some_and(|started_at| {
            now.checked_duration_since(started_at).unwrap_or_default() <= Duration::from_secs(grace_period_timeout_secs)
        })
    }

    pub fn new(
        cfg: &ConfigInput,
        connection: Arc<RwLock<ProviderConfigConnection>>,
        on_connection_change: ProviderConnectionChangeCallback,
    ) -> Self {
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
            on_connection_change,
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
    pub fn max_connections(&self) -> usize { self.max_connections }

    #[inline]
    pub fn is_unlimited(&self) -> bool { self.max_connections == 0 }

    #[inline]
    pub fn exp_date(&self) -> Option<i64> { self.exp_date }

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
            guard.grace_started_at = None;
        }

        if guard.current_connections > max {
            if Self::grace_is_active(&guard, Instant::now(), grace_period_timeout_secs) {
                // Grace timeout still active, deny connection
                debug!("Provider access denied, grace exhausted, too many connections, over limit: {}", self.name);
            }
            return true;
        }
        false
    }

    //
    // #[inline]
    // pub fn has_capacity(&self) -> bool {
    //     !self.is_exhausted()
    // }

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
        if connections < self.max_connections {
            guard.grace_started_at = None;
            modify_connections!(self, guard, +1);
            return ProviderConfigAllocation::Available;
        }
        if !grace || connections > self.max_connections {
            return ProviderConfigAllocation::Exhausted;
        }

        let now = Instant::now();
        if Self::grace_is_active(&guard, now, grace_period_timeout_secs) {
            debug!("Provider access denied, grace exhausted, too many connections: {}", self.name);
            return ProviderConfigAllocation::Exhausted;
        }
        debug_if_enabled!(
            "Provider {} granting grace allocation (current_connections={}, max_connections={})",
            sanitize_sensitive_info(&self.name),
            connections,
            self.max_connections
        );
        guard.grace_started_at = Some(now);
        modify_connections!(self, guard, +1);
        ProviderConfigAllocation::GracePeriod
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
        if connections < self.max_connections {
            guard.grace_started_at = None;
            return true;
        }
        if !grace || connections > self.max_connections {
            return false;
        }

        if Self::grace_is_active(&guard, Instant::now(), grace_period_timeout_secs) {
            debug!(
                "Provider access denied, grace exhausted, too many connections, no connection available: {}",
                self.name
            );
            return false;
        }
        guard.grace_started_at = None;
        true
    }

    pub async fn release(&self) {
        let mut guard = self.connection.write().await;
        // Releasing while over capacity ends the single outstanding grace grant.
        // Unlimited providers do not use grace tracking.
        if self.max_connections > 0 && guard.current_connections > self.max_connections {
            guard.grace_started_at = None;
        }
        if guard.current_connections > 0 {
            modify_connections!(self, guard, -1);
        }
    }

    #[inline]
    pub async fn get_current_connections(&self) -> usize { self.connection.read().await.current_connections }

    #[inline]
    pub fn get_priority(&self) -> i16 { self.priority }
}

#[derive(Clone, Debug)]
pub struct ProviderConfigWrapper {
    inner: Arc<ProviderConfig>,
}

impl fmt::Display for ProviderConfigWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.inner) }
}

impl ProviderConfigWrapper {
    pub fn new(cfg: ProviderConfig) -> Self { Self { inner: Arc::new(cfg) } }

    pub fn config(&self) -> Arc<ProviderConfig> { Arc::clone(&self.inner) }

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

    fn deref(&self) -> &Self::Target { &self.inner }
}

impl PartialEq for ProviderAllocation {
    fn eq(&self, other: &Self) -> bool {
        // Note: released flag ignored
        match (self, other) {
            (ProviderAllocation::Exhausted, ProviderAllocation::Exhausted) => true,
            (ProviderAllocation::Available(cfg1), ProviderAllocation::Available(cfg2))
            | (ProviderAllocation::GracePeriod(cfg1), ProviderAllocation::GracePeriod(cfg2)) => cfg1 == cfg2,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{InputFetchMethod, InputType, StagedInputType};
    use std::{
        collections::HashMap,
        sync::atomic::{AtomicUsize, Ordering},
    };

    fn build_test_config(max_connections: u16) -> ProviderConfig {
        let input = ConfigInput {
            id: 1,
            name: Arc::from("test-provider"),
            url: "http://test".to_string(),
            username: Some("u".to_string()),
            password: Some("p".to_string()),
            max_connections,
            priority: 0,
            input_type: InputType::Xtream,
            headers: HashMap::new(),
            epg: None,
            persist: None,
            enabled: true,
            sequential_group: None,
            options: None,
            media_server: None,
            aliases: None,
            method: InputFetchMethod::default(),
            staged_type: StagedInputType::default(),
            staged: None,
            exp_date: None,
            t_batch_url: None,
            panel_api: None,
            provider_configs: None,
            cache_duration_seconds: 0,
            stalker: None,
        };
        let conn = Arc::new(RwLock::new(ProviderConfigConnection::default()));
        let counter = Arc::new(AtomicUsize::new(0));
        let cb_counter = Arc::clone(&counter);
        let cb: ProviderConnectionChangeCallback = Arc::new(move |_name, count| {
            cb_counter.store(count, Ordering::SeqCst);
        });
        ProviderConfig::new(&input, conn, cb)
    }

    #[tokio::test]
    async fn release_does_not_touch_grace_for_unlimited_provider() {
        // For unlimited providers (max_connections == 0) the grace-reset branch in
        // `release` must be a no-op even if the fields contain non-default values.
        // This is stronger than checking defaults and catches regressions where the
        // old `current > max` branch would clear grace state for unlimited inputs.
        let provider = build_test_config(0);
        assert!(provider.is_unlimited());

        // Drive connection count up so `release` executes its normal path.
        assert!(matches!(provider.try_allocate(false, 0).await, ProviderConfigAllocation::Available));
        assert!(matches!(provider.try_allocate(false, 0).await, ProviderConfigAllocation::Available));
        assert_eq!(provider.get_current_connections().await, 2);

        {
            let mut guard = provider.connection.write().await;
            guard.grace_started_at = Some(Instant::now());
        }

        provider.release().await;

        let guard = provider.connection.read().await;
        assert_eq!(guard.current_connections, 1, "release must still decrement connection count");
        assert!(guard.grace_started_at.is_some(), "release must not clear grace state for unlimited provider");
    }

    #[tokio::test]
    async fn concurrent_allocations_grant_only_one_grace_connection() {
        let provider = build_test_config(1);
        assert!(matches!(provider.try_allocate(false, 10).await, ProviderConfigAllocation::Available));

        let (first, second) = tokio::join!(provider.try_allocate(true, 10), provider.try_allocate(true, 10));
        let grace_count = [first, second]
            .into_iter()
            .filter(|allocation| matches!(allocation, ProviderConfigAllocation::GracePeriod))
            .count();
        let exhausted_count = [first, second]
            .into_iter()
            .filter(|allocation| matches!(allocation, ProviderConfigAllocation::Exhausted))
            .count();

        assert_eq!(grace_count, 1);
        assert_eq!(exhausted_count, 1);
        assert_eq!(provider.get_current_connections().await, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn active_grace_at_limit_blocks_another_grant_until_timeout() {
        let provider = build_test_config(1);
        {
            let mut guard = provider.connection.write().await;
            guard.current_connections = 1;
            guard.grace_started_at = Some(Instant::now());
        }

        assert!(matches!(provider.try_allocate(true, 10).await, ProviderConfigAllocation::Exhausted));
        assert!(!provider.get_next(true, 10).await);
        assert!(provider.connection.read().await.grace_started_at.is_some());

        tokio::time::advance(Duration::from_secs(11)).await;

        assert!(provider.get_next(true, 10).await);
        assert!(provider.connection.read().await.grace_started_at.is_none());
        assert!(matches!(provider.try_allocate(true, 10).await, ProviderConfigAllocation::GracePeriod));
    }
}
