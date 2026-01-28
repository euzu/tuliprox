use crate::api::model::provider_config::ProviderConfigWrapper;
use crate::api::model::{EventManager, ProviderConfig, ProviderConfigConnection, ProviderConnectionChangeCallback};
use crate::model::{is_input_expired, ConfigInput, GracePeriodOptions};
use crate::utils::debug_if_enabled;
use arc_swap::ArcSwap;
use dashmap::DashMap;
use log::{debug, log_enabled};
use shared::utils::{display_vec, sanitize_sensitive_info};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

macro_rules! gen_provider_search {
    ($fn_name:ident, $field: ident, $crit_type:ty) => {
        fn $fn_name<'a>(criteria: $crit_type, providers: &'a Vec<ProviderLineup>) -> Option<(&'a ProviderLineup, &'a ProviderConfigWrapper)> {
            for lineup in providers {
                match lineup {
                    ProviderLineup::Single(single) => {
                        if &single.provider.$field == criteria {
                            return Some((lineup, &single.provider));
                        }
                    }
                    ProviderLineup::Multi(multi) => {
                        for group in &multi.providers {
                            match group {
                                ProviderPriorityGroup::SingleProviderGroup(single) => {
                                    if &single.$field == criteria {
                                        return Some((lineup, single));
                                    }
                                }
                                ProviderPriorityGroup::MultiProviderGroup(_, configs) => {
                                    for config in configs {
                                        if &config.$field == criteria {
                                            return Some((lineup, config));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            None
        }
    }
}

fn get_or_create_provider_connection(
    provider_connections: &DashMap<Arc<str>, Arc<RwLock<ProviderConfigConnection>>>,
    provider_name: &Arc<str>,
) -> Arc<RwLock<ProviderConfigConnection>> {
    provider_connections
        .entry(provider_name.clone())
        .or_insert_with(|| Arc::new(RwLock::new(ProviderConfigConnection::default())))
        .clone()
}

#[derive(Debug, Clone)]
pub enum ProviderAllocation {
    Exhausted,
    Available(Arc<ProviderConfig>),
    GracePeriod(Arc<ProviderConfig>),
}

impl ProviderAllocation {
    pub fn short_key(&self) -> &str {
        match self {
            ProviderAllocation::Exhausted => "exhausted",
            ProviderAllocation::Available(_) => "available",
            ProviderAllocation::GracePeriod(_) => "grace_period",
        }
    }

    pub fn new_available(config: Arc<ProviderConfig>) -> Self {
        ProviderAllocation::Available(config)
    }

    pub fn new_grace_period(config: Arc<ProviderConfig>) -> Self {
        ProviderAllocation::GracePeriod(config)
    }

    pub fn get_provider_name(&self) -> Option<Arc<str>> {
        match self {
            ProviderAllocation::Exhausted => None,
            ProviderAllocation::Available(ref cfg) |
            ProviderAllocation::GracePeriod(ref cfg) => {
                Some(cfg.name.clone())
            }
        }
    }

    pub fn get_provider_id(&self) -> Option<u16> {
        match self {
            ProviderAllocation::Exhausted => None,
            ProviderAllocation::Available(ref cfg) |
            ProviderAllocation::GracePeriod(ref cfg) => {
                Some(cfg.id)
            }
        }
    }

    pub fn get_provider_config(&self) -> Option<Arc<ProviderConfig>> {
        match self {
            ProviderAllocation::Exhausted => None,
            ProviderAllocation::Available(ref cfg) |
            ProviderAllocation::GracePeriod(ref cfg) => {
                Some(Arc::clone(cfg))
            }
        }
    }

    pub async fn release(&self) {
        match &self {
            ProviderAllocation::Exhausted => {}
            ProviderAllocation::Available(config) |
            ProviderAllocation::GracePeriod(config) => {
                config.release().await;
            }
        }
    }
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

/// This manages different types of provider lineups:
///
/// `Single(SingleProviderLineup)`: A single provider.
/// `Multi(MultiProviderLineup)`: A set of providers grouped by priority.
#[derive(Debug)]
enum ProviderLineup {
    Single(SingleProviderLineup),
    Multi(MultiProviderLineup),
}

impl fmt::Display for ProviderLineup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderLineup::Single(lineup) => {
                write!(f, "SingleProviderLineup: {{ {} }}", lineup.provider)
            }
            ProviderLineup::Multi(lineup) => {
                write!(f, "MultiProviderLineup: {{")?;
                for (i, group) in lineup.providers.iter().enumerate() {
                    write!(f, "  Group {}: {}", i + 1, group)?;
                }
                write!(f, " }}")?;
                Ok(())
            }
        }
    }
}

impl ProviderLineup {
    async fn get_next(&self, grace_period_timeout_secs: u64) -> Option<Arc<ProviderConfig>> {
        match self {
            ProviderLineup::Single(lineup) => lineup.get_next(grace_period_timeout_secs).await,
            ProviderLineup::Multi(lineup) => lineup.get_next(grace_period_timeout_secs).await,
        }
    }

    async fn acquire(&self, with_grace: bool, grace_period_timeout_secs: u64) -> ProviderAllocation {
        match self {
            ProviderLineup::Single(lineup) => lineup.acquire(with_grace, grace_period_timeout_secs).await,
            ProviderLineup::Multi(lineup) => lineup.acquire(with_grace, grace_period_timeout_secs).await,
        }
    }
}

/// Handles a single provider and ensures safe allocation/release of connections.
#[derive(Debug)]
struct SingleProviderLineup {
    provider: ProviderConfigWrapper,
}

impl SingleProviderLineup {
    fn new(cfg: &ConfigInput, connection: Arc<RwLock<ProviderConfigConnection>>, connection_change: &ProviderConnectionChangeCallback) -> Self {
        Self {
            provider: ProviderConfigWrapper::new(ProviderConfig::new(cfg, connection, Arc::clone(connection_change))),
        }
    }

    async fn get_next(&self, grace_period_timeout_secs: u64) -> Option<Arc<ProviderConfig>> {
        self.provider.get_next(false, grace_period_timeout_secs).await
    }

    async fn acquire(&self, with_grace: bool, grace_period_timeout_secs: u64) -> ProviderAllocation {
        self.provider.try_allocate(with_grace, grace_period_timeout_secs).await
    }

    #[cfg(test)]
    async fn release(&self, provider_name: &Arc<str>) {
        if &self.provider.name == provider_name {
            self.provider.release().await;
        }
    }
}


/// Manages provider groups based on priority:
///
/// `SingleProviderGroup(ProviderConfig)`: A single provider.
/// `MultiProviderGroup(AtomicUsize, Vec<ProviderConfig>)`: A list of providers with a priority index.
#[derive(Debug)]
enum ProviderPriorityGroup {
    SingleProviderGroup(ProviderConfigWrapper),
    MultiProviderGroup(AtomicUsize, Vec<ProviderConfigWrapper>),
}

impl fmt::Display for ProviderPriorityGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderPriorityGroup::SingleProviderGroup(p) => {
                write!(f, "Single({p})")
            }
            ProviderPriorityGroup::MultiProviderGroup(_, providers) => {
                write!(f, "Multi({})", display_vec(providers))
            }
        }
    }
}

/// Manages multiple providers, ensuring that connections are allocated in a round-robin manner based on priority.
#[repr(align(64))]
#[derive(Debug)]
struct MultiProviderLineup {
    name: Arc<str>,
    providers: Vec<ProviderPriorityGroup>,
}

impl MultiProviderLineup {
    pub fn new(
        cfg_input: &ConfigInput,
        provider_connections: &DashMap<Arc<str>, Arc<RwLock<ProviderConfigConnection>>>,
        connection_change: &ProviderConnectionChangeCallback,
    ) -> Self {
        let input_connection = get_or_create_provider_connection(provider_connections, &cfg_input.name);
        let mut inputs = vec![ProviderConfigWrapper::new(ProviderConfig::new(cfg_input, input_connection, Arc::clone(connection_change)))];
        if let Some(aliases) = cfg_input.get_enabled_aliases() {
            for alias in aliases {
                let alias_connection = get_or_create_provider_connection(provider_connections, &alias.name);
                inputs.push(ProviderConfigWrapper::new(ProviderConfig::new_alias(
                    cfg_input,
                    alias,
                    alias_connection,
                    Arc::clone(connection_change),
                )));
            }
        }
        let mut providers = HashMap::new();
        for provider in inputs {
            let priority = provider.get_priority();
            providers.entry(priority)
                .or_insert_with(Vec::new)
                .push(provider);
        }
        let mut values: Vec<(i16, Vec<ProviderConfigWrapper>)> = providers.into_iter().collect();
        values.sort_by_key(|(p1, _)| *p1);
        let providers: Vec<ProviderPriorityGroup> = values.into_iter().map(|(_, mut group)| {
            if group.len() > 1 {
                ProviderPriorityGroup::MultiProviderGroup(AtomicUsize::new(0), group)
            } else {
                ProviderPriorityGroup::SingleProviderGroup(group.remove(0))
            }
        }).collect();

        Self {
            name: cfg_input.name.clone(),
            providers,
        }
    }

    /// Attempts to acquire the next available provider from a specific priority group.
    ///
    /// # Parameters
    /// - `priority_group`: The provider group to search within.
    ///
    /// # Returns
    /// - `ProviderAllocation`: A reference to the next available provider in the specified group.
    ///
    /// # Behavior
    /// - Iterates through the providers in the given group in a round-robin manner.
    /// - Checks if a provider has available capacity before selecting it.
    /// - Uses atomic operations to maintain fair provider selection.
    ///
    /// # Thread Safety
    /// - Uses `RwLock` for safe concurrent access.
    /// - Ensures fair provider allocation across multiple threads.
    ///
    /// # Example Usage
    /// ```rust
    /// let lineup = MultiProviderLineup::new(&config);
    /// match lineup.acquire_next_provider_from_group(priority_group) {
    ///    ProviderAllocation::Exhausted => println!("All providers exhausted"),
    ///    ProviderAllocation::Available(provider) =>  println!("Provider available {}", provider.name),
    ///    ProviderAllocation::GracePeriod(provider) =>  println!("Provider with grace period {}", provider.name),
    /// }
    /// }
    /// ```
    async fn acquire_next_provider_from_group(priority_group: &ProviderPriorityGroup, grace: bool, grace_period_timeout_secs: u64) -> ProviderAllocation {
        match priority_group {
            ProviderPriorityGroup::SingleProviderGroup(p) => {
                let result = p.try_allocate(grace, grace_period_timeout_secs).await;
                if !matches!(result, ProviderAllocation::Exhausted) {
                    return result;
                }
            }
            ProviderPriorityGroup::MultiProviderGroup(index, pg) => {
                let provider_count = pg.len();
                if grace {
                    for p in pg {
                        let result = p.try_allocate(true, grace_period_timeout_secs).await;
                        if !matches!(result, ProviderAllocation::Exhausted) {
                            return result;
                        }
                    }
                    return ProviderAllocation::Exhausted;
                }

                let start = index.fetch_add(1, Ordering::AcqRel) % provider_count;
                let mut idx = start;

                loop {
                    let p = &pg[idx];
                    let result = p.try_allocate(grace, grace_period_timeout_secs).await;
                    if !matches!(result, ProviderAllocation::Exhausted) {
                        index.store((idx + 1) % provider_count, Ordering::Release);
                        return result;
                    }

                    idx = (idx + 1) % provider_count;

                    // loop finished
                    if idx == start {
                        break;
                    }
                }

                index.store(idx, Ordering::Release);
            }
        }
        ProviderAllocation::Exhausted
    }

    // Used for redirect to cyclce through provider
    async fn get_next_provider_from_group(priority_group: &ProviderPriorityGroup, grace: bool, grace_period_timeout_secs: u64) -> Option<Arc<ProviderConfig>> {
        match priority_group {
            ProviderPriorityGroup::SingleProviderGroup(p) => {
                return p.get_next(grace, grace_period_timeout_secs).await;
            }
            ProviderPriorityGroup::MultiProviderGroup(index, pg) => {
                let provider_count = pg.len();
                let start = index.fetch_add(1, Ordering::AcqRel) % provider_count;
                let mut idx = start;

                loop {
                    let p = &pg[idx];
                    let result = p.get_next(grace, grace_period_timeout_secs).await;
                    if result.is_some() {
                        index.store((idx + 1) % provider_count, Ordering::Release);
                        return result;
                    }

                    idx = (idx + 1) % provider_count;

                    // loop finished
                    if idx == start {
                        break;
                    }
                }

                index.store(idx, Ordering::Release);
            }
        }
        None
    }

    /// Attempts to acquire a provider from the lineup based on priority and availability.
    ///
    /// # Returns
    /// - `ProviderAllocation`: A reference to the acquired provider if allocation was successful.
    ///
    /// # Behavior
    /// - The method iterates through provider priority groups in a round-robin fashion.
    /// - It attempts to allocate a provider from the highest priority group first.
    /// - If a provider has available capacity, it is returned.
    /// - If all providers in a group are exhausted, it moves to the next group.
    /// - Updates the internal index to ensure fair distribution of requests.
    ///
    /// # Thread Safety
    /// - Uses atomic operations (`AtomicUsize`) for thread-safe indexing.
    /// - Uses `RwLock` for thread-safe provider allocation.
    ///
    /// # Example Usage
    /// ```rust
    /// let lineup = MultiProviderLineup::new(&config);
    /// match lineup.acquire() {
    ///    ProviderAllocation::Exhausted => println!("All providers exhausted"),
    ///    ProviderAllocation::Available(provider) =>  println!("Provider available {}", provider.name),
    ///    ProviderAllocation::GracePeriod(provider) =>  println!("Provider with grace period {}", provider.name),
    /// }
    /// ```
    async fn acquire(&self, with_grace: bool, grace_period_timeout_secs: u64) -> ProviderAllocation {
        // Phase 1: prefer providers with available capacity (no grace allocations),
        // scanning priority groups from highest -> lowest.
        for priority_group in &self.providers {
            let allocation =
                Self::acquire_next_provider_from_group(priority_group, false, grace_period_timeout_secs).await;
            if !matches!(allocation, ProviderAllocation::Exhausted) {
                return allocation;
            }
        }

        if !with_grace {
            return ProviderAllocation::Exhausted;
        }

        // Phase 2: all providers are at capacity, allow grace allocations (still respecting priority order).
        for priority_group in &self.providers {
            let allocation =
                Self::acquire_next_provider_from_group(priority_group, true, grace_period_timeout_secs).await;
            if !matches!(allocation, ProviderAllocation::Exhausted) {
                return allocation;
            }
        }

        ProviderAllocation::Exhausted
    }

    // it intended to use with redirects to cycle through provider
    async fn get_next(&self, grace_period_timeout_secs: u64) -> Option<Arc<ProviderConfig>> {
        // Phase 1: prefer providers with available capacity (no grace allocations),
        // scanning priority groups from highest -> lowest.
        for priority_group in &self.providers {
            if let Some(config) =
                Self::get_next_provider_from_group(priority_group, false, grace_period_timeout_secs).await
            {
                return Some(config);
            }
        }

        // Phase 2: no provider is available, allow grace.
        for priority_group in &self.providers {
            if let Some(config) =
                Self::get_next_provider_from_group(priority_group, true, grace_period_timeout_secs).await
            {
                return Some(config);
            }
        }

        None
    }


    #[cfg(test)]
    async fn release(&self, provider_name: &Arc<str>) {
        for g in &self.providers {
            match g {
                ProviderPriorityGroup::SingleProviderGroup(pc) => {
                    if &pc.name == provider_name {
                        pc.release().await;
                        break;
                    }
                }
                ProviderPriorityGroup::MultiProviderGroup(_, group) => {
                    for pc in group {
                        if &pc.name == provider_name {
                            pc.release().await;
                            return;
                        }
                    }
                }
            }
        }
    }

    pub async fn get_total_connections(&self) -> usize {
        let mut total_connections = 0;

        for group in &self.providers {
            match group {
                ProviderPriorityGroup::SingleProviderGroup(provider) => {
                    total_connections += provider.get_current_connections().await;
                }
                ProviderPriorityGroup::MultiProviderGroup(_, providers) => {
                    for provider in providers {
                        total_connections += provider.get_current_connections().await;
                    }
                }
            }
        }
        total_connections
    }
}

pub(in crate::api::model) struct ProviderLineupManager {
    grace_period_millis: AtomicU64,
    grace_period_timeout_secs: AtomicU64,
    inputs: Arc<ArcSwap<Vec<Arc<ConfigInput>>>>,
    providers: Arc<ArcSwap<Vec<ProviderLineup>>>,
    provider_connections: DashMap<Arc<str>, Arc<RwLock<ProviderConfigConnection>>>,
    event_manager: Arc<EventManager>,
}

impl ProviderLineupManager {
    pub fn new(inputs: Vec<Arc<ConfigInput>>, grace_period_options: GracePeriodOptions, event_manager: &Arc<EventManager>) -> Self {
        let provider_connections: DashMap<Arc<str>, Arc<RwLock<ProviderConfigConnection>>> = DashMap::new();
        let lineups = inputs
            .iter()
            .map(|i| Self::create_lineup(i, &provider_connections, event_manager))
            .collect();
        Self {
            grace_period_millis: AtomicU64::new(grace_period_options.period_millis),
            grace_period_timeout_secs: AtomicU64::new(grace_period_options.timeout_secs),
            inputs: Arc::new(ArcSwap::from_pointee(inputs)),
            providers: Arc::new(ArcSwap::from_pointee(lineups)),
            provider_connections,
            event_manager: Arc::clone(event_manager),
        }
    }

    fn create_lineup(
        cfg_input: &ConfigInput,
        provider_connections: &DashMap<Arc<str>, Arc<RwLock<ProviderConfigConnection>>>,
        event_manager: &Arc<EventManager>,
    ) -> ProviderLineup {
        let event_manager = Arc::clone(event_manager);
        let on_connection_change: ProviderConnectionChangeCallback = Arc::new(move |name: &Arc<str>, connections: usize| {
            event_manager.send_provider_event(name, connections);
        });

        if cfg_input.has_enabled_aliases() {
            ProviderLineup::Multi(MultiProviderLineup::new(cfg_input, provider_connections, &on_connection_change))
        } else {
            let connection = get_or_create_provider_connection(provider_connections, &cfg_input.name);
            ProviderLineup::Single(SingleProviderLineup::new(cfg_input, connection, &on_connection_change))
        }
    }

    fn inputs_differ(a: &ConfigInput, b: &ConfigInput) -> bool {
        if a.enabled != b.enabled
            || a.max_connections != b.max_connections
            || a.priority != b.priority
            || a.input_type != b.input_type
            || a.username != b.username
            || a.password != b.password
            || a.url != b.url
            || a.exp_date != b.exp_date
        {
            return true;
        }

        match (&a.aliases, &b.aliases) {
            (None, None) => {}
            (Some(_), None) | (None, Some(_)) => return true,
            (Some(a_aliases), Some(b_aliases)) => {
                if a_aliases.len() != b_aliases.len() {
                    return true;
                }

                for b_alias in b_aliases {
                    let Some(a_alias) = a_aliases.iter().find(|a| a.name == b_alias.name) else {
                        return true;
                    };

                    if a_alias.max_connections != b_alias.max_connections
                        || a_alias.enabled != b_alias.enabled
                        || a_alias.priority != b_alias.priority
                        || a_alias.username != b_alias.username
                        || a_alias.password != b_alias.password
                        || a_alias.url != b_alias.url
                        || a_alias.exp_date != b_alias.exp_date
                    {
                        return true;
                    }
                }
            }
        }

        false
    }

    fn has_changed(&self, new_inputs: &[Arc<ConfigInput>]) -> bool {
        let old_inputs = self.inputs.load();
        if old_inputs.len() != new_inputs.len() {
            return true;
        }
        for new_input in new_inputs {
            let Some(old_input) = old_inputs.iter().find(|i| i.name == new_input.name) else {
                return true;
            };

            if Self::inputs_differ(old_input.as_ref(), new_input.as_ref()) {
                return true;
            }
        }

        false
    }

    pub fn update_config(&self, new_inputs: Vec<Arc<ConfigInput>>, grace_period_options: &GracePeriodOptions) {
        self.grace_period_millis.store(grace_period_options.period_millis, Ordering::Relaxed);
        self.grace_period_timeout_secs.store(grace_period_options.timeout_secs, Ordering::Relaxed);

        if !self.has_changed(&new_inputs) {
            return;
        }

        let mut new_lineups: Vec<ProviderLineup> = Vec::with_capacity(new_inputs.len());
        for input in &new_inputs {
            new_lineups.push(Self::create_lineup(input, &self.provider_connections, &self.event_manager));
        }

        debug_if_enabled!("inputs {}", sanitize_sensitive_info(&display_vec(&new_inputs)));
        debug_if_enabled!("lineup {}", sanitize_sensitive_info(&display_vec(&new_lineups)));

        self.inputs.store(Arc::new(new_inputs));
        self.providers.store(Arc::new(new_lineups));
    }

    pub async fn reconcile_connections(&self, mut counts: HashMap<Arc<str>, usize>) {
        // 1. Synchronize known providers from actual counts.
        // We take a snapshot of the keys and locks to avoid holding the DashMap's internal
        // shard locks while awaiting the RwLock of each provider. Holding both can lead to deadlocks.
        let snapshot: Vec<_> = self.provider_connections.iter()
            .map(|e| (e.key().clone(), Arc::clone(e.value())))
            .collect();

        for (name, conn_lock) in snapshot {
            let count = counts.remove(&name).unwrap_or(0);
            let mut conn = conn_lock.write().await;
            conn.current_connections = count;
            if count == 0 {
                conn.granted_grace = false;
                conn.grace_ts = 0;
            }
        }

        // 2. Handle new providers that weren't in the registry yet (e.g. newly added/renamed).
        for (name, count) in counts {
            let conn_lock = self.provider_connections
                .entry(name)
                .or_insert_with(|| Arc::new(RwLock::new(ProviderConfigConnection::default())))
                .clone();
            let mut conn = conn_lock.write().await;
            conn.current_connections = count;
        }

        // 3. Broadcast status updates to the UI/Event system.
        // We drop the snapshot before GC to ensure Arc counts are accurate.
        {
            let snapshot: Vec<_> = self.provider_connections.iter()
                .map(|e| (e.key().clone(), Arc::clone(e.value())))
                .collect();

            for (name, conn_lock) in snapshot {
                let count = conn_lock.read().await.current_connections;
                self.event_manager.send_provider_event(&name, count);
            }
        }

        // 4. Garbage Collection: Remove providers that are neither in the current config
        // nor referenced by any active stream allocations.
        //
        // deterministic check: Is the name still in any current lineup?
        let current_names: std::collections::HashSet<Arc<str>> = {
            let mut names = std::collections::HashSet::new();
            let lineups = self.providers.load();
            for lineup in lineups.iter() {
                match lineup {
                    ProviderLineup::Single(s) => { names.insert(s.provider.name.clone()); }
                    ProviderLineup::Multi(m) => {
                        names.insert(m.name.clone());
                        for g in &m.providers {
                            match g {
                                ProviderPriorityGroup::SingleProviderGroup(p) => { names.insert(p.name.clone()); }
                                ProviderPriorityGroup::MultiProviderGroup(_, group) => {
                                    for p in group { names.insert(p.name.clone()); }
                                }
                            }
                        }
                    }
                }
            }
            names
        };

        // Arc::strong_count == 1 means it's only held by this DashMap registry.
        self.provider_connections.retain(|name, conn_lock| {
            if current_names.contains(name) {
                return true; // Keep if in active config
            }
            if Arc::strong_count(conn_lock) > 1 {
                return true; // Keep if held by an active stream
            }
            debug!("Purging stale provider connection record: {name}");
            false
        });
    }

    gen_provider_search!(get_provider_config_by_name, name, &Arc<str>);


    fn log_allocation(allocation: &ProviderAllocation) {
        match allocation {
            ProviderAllocation::Exhausted => {}
            ProviderAllocation::Available(ref cfg) | ProviderAllocation::GracePeriod(ref cfg) => {
                debug_if_enabled!(
                    "Using provider {} (pool user: {}, max_connections: {})",
                    sanitize_sensitive_info(&cfg.name),
                    cfg.get_user_info()
                        .map_or_else(|| "?".to_string(), |ui| sanitize_sensitive_info(&ui.username).to_string())
                    ,
                    cfg.max_connections()
                );
            }
        }
    }

    pub async fn force_exact_acquire_connection(&self, provider_name: &Arc<str>) -> ProviderAllocation {
        let providers = self.providers.load();
        let allocation = match Self::get_provider_config_by_name(provider_name, &providers) {
            None => ProviderAllocation::Exhausted, // No Name matched, we don't have this provider
            Some((_lineup, config)) => config.force_allocate().await,
        };
        Self::log_allocation(&allocation);
        allocation
    }

    // Returns the next available provider connection
    pub(crate) async fn acquire_connection(&self, input_name: &Arc<str>) -> ProviderAllocation {
        self.acquire_connection_with_grace_override(input_name, true).await
    }

    /// Acquire a provider connection, optionally allowing provider-side grace allocations.
    ///
    /// When `allow_grace` is `false`, the lineup will not allocate providers in `GracePeriod`,
    /// even if a global grace period is configured.
    pub(crate) async fn acquire_connection_with_grace_override(
        &self,
        input_name: &Arc<str>,
        allow_grace: bool,
    ) -> ProviderAllocation {
        let providers = self.providers.load();
        let lineup_opt = Self::get_provider_config_by_name(input_name, &providers);
        let with_grace = allow_grace && self.grace_period_millis.load(Ordering::Acquire) > 0;
        let allocation = match lineup_opt {
            None => ProviderAllocation::Exhausted, // No Name matched, we don't have this provider
            Some((lineup, _config)) => {
                lineup
                    .acquire(with_grace, self.grace_period_timeout_secs.load(Ordering::Acquire))
                    .await
            }
        };
        if matches!(allocation, ProviderAllocation::Exhausted) {
            if let Some((lineup, _cfg)) = lineup_opt {
                Self::log_exhausted_pool_snapshot(input_name, lineup).await;
            }
        }
        Self::log_allocation(&allocation);
        allocation
    }

    async fn log_exhausted_pool_snapshot(input_name: &str, lineup: &ProviderLineup) {
        if !log_enabled!(log::Level::Debug) {
            return;
        }

        let mut entries: Vec<String> = Vec::new();
        match lineup {
            ProviderLineup::Single(single) => {
                entries.push(Self::format_provider_snapshot_entry(&single.provider).await);
            }
            ProviderLineup::Multi(multi) => {
                for group in &multi.providers {
                    match group {
                        ProviderPriorityGroup::SingleProviderGroup(cfg) => {
                            entries.push(Self::format_provider_snapshot_entry(cfg).await);
                        }
                        ProviderPriorityGroup::MultiProviderGroup(_, cfgs) => {
                            for cfg in cfgs {
                                entries.push(Self::format_provider_snapshot_entry(cfg).await);
                            }
                        }
                    }
                }
            }
        }

        debug_if_enabled!(
            "Provider pool exhausted for input {} (pool_snapshot=[{}])",
            sanitize_sensitive_info(input_name),
            sanitize_sensitive_info(&entries.join(", "))
        );
    }

    async fn format_provider_snapshot_entry(cfg: &ProviderConfigWrapper) -> String {
        let current = cfg.get_current_connections().await;
        let max = cfg.max_connections();
        let expired = is_input_expired(cfg.exp_date());
        format!("{}:{}/{} expired={expired}", cfg.name, current, max)
    }

    // This method is used for redirects to cycle through provider
    //
    pub async fn get_next_provider(&self, input_name: &Arc<str>) -> Option<Arc<ProviderConfig>> {
        let providers = self.providers.load();
        match Self::get_provider_config_by_name(input_name, &providers) {
            None => None,
            Some((lineup, _config)) => {
                let cfg = lineup.get_next(self.grace_period_timeout_secs.load(Ordering::Relaxed)).await;
                if log_enabled!(log::Level::Debug) {
                    if let Some(ref c) = cfg {
                        debug!("Using provider {}", c.name);
                    }
                }
                cfg
            }
        }
    }

    pub async fn active_connections(&self) -> Option<HashMap<Arc<str>, usize>> {
        let mut result = HashMap::<Arc<str>, usize>::new();
        let mut add_provider = |name: &Arc<str>, count: usize| {
            if count > 0 {
                result.insert(name.clone(), count);
            }
        };
        let providers = self.providers.load();
        for lineup in providers.iter() {
            match lineup {
                ProviderLineup::Single(provider_lineup) => {
                    let connections = provider_lineup.provider.get_current_connections().await;
                    add_provider(&provider_lineup.provider.name, connections);
                }
                ProviderLineup::Multi(provider_lineup) => {
                    let connections = provider_lineup.get_total_connections().await;
                    add_provider(&provider_lineup.name, connections);
                }
            }
        }
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    pub async fn active_connection_count(&self) -> usize {
        let mut count = 0;
        let providers = self.providers.load();
        for lineup in providers.iter() {
            match lineup {
                ProviderLineup::Single(provider_lineup) => {
                    count += provider_lineup.provider.get_current_connections().await;
                }
                ProviderLineup::Multi(provider_lineup) => {
                    count += provider_lineup.get_total_connections().await;
                }
            }
        }
        count
    }

    pub async fn is_over_limit(&self, provider_name: &Arc<str>) -> bool {
        let providers = self.providers.load();
        if let Some((_, config)) = Self::get_provider_config_by_name(provider_name, &providers) {
            config.is_over_limit(self.grace_period_timeout_secs.load(Ordering::Relaxed)).await
        } else {
            false
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ConfigInputAlias;
    use crate::Arc;
    use shared::model::{InputFetchMethod, InputType};
    use std::sync::atomic::AtomicU16;
    use std::thread;
    use shared::concat_string;
    use shared::utils::Internable;

    macro_rules! should_available {
        ($lineup:expr, $provider_id:expr, $grace_period_timeout_secs: expr) => {
            thread::sleep(std::time::Duration::from_millis(200));
            match $lineup.acquire(true, $grace_period_timeout_secs).await {
                ProviderAllocation::Exhausted => assert!(false, "Should available and not exhausted"),
                ProviderAllocation::Available(provider) => assert_eq!(provider.id, $provider_id),
                ProviderAllocation::GracePeriod(provider) => assert!(false, "Should available and not grace period: {}", provider.id),
            }
        };
    }
    macro_rules! should_grace_period {
        ($lineup:expr, $provider_id:expr, $grace_period_timeout_secs: expr) => {
            thread::sleep(std::time::Duration::from_millis(200));
            match $lineup.acquire(true, $grace_period_timeout_secs).await {
                ProviderAllocation::Exhausted => assert!(false, "Should grace period and not exhausted"),
                ProviderAllocation::Available(provider) => assert!(false, "Should grace period and not available: {}", provider.id),
                ProviderAllocation::GracePeriod(provider) => assert_eq!(provider.id, $provider_id),
            }
        };
    }

    macro_rules! should_exhausted {
        ($lineup:expr, $grace_period_timeout_secs: expr) => {
            thread::sleep(std::time::Duration::from_millis(200));
            match $lineup.acquire(true, $grace_period_timeout_secs).await {
                ProviderAllocation::Exhausted => {},
                ProviderAllocation::Available(provider) => assert!(false, "Should exhausted and not available: {}", provider.id),
                ProviderAllocation::GracePeriod(provider) => assert!(false, "Should exhausted and not grace period: {}", provider.id),
            }
        };
    }

    // Helper function to create a ConfigInput instance
    fn create_config_input(id: u16, name: &Arc<str>, priority: i16, max_connections: u16) -> ConfigInput {
        ConfigInput {
            id,
            name: Arc::clone(name),
            url: "http://example.com".to_string(),
            epg: Option::default(),
            username: None,
            password: None,
            persist: None,
            enabled: true,
            input_type: InputType::Xtream, // You can use a default value here
            max_connections,
            priority,
            aliases: None,
            headers: HashMap::default(),
            options: None,
            method: InputFetchMethod::default(),
            staged: None,
            exp_date: None,
            t_batch_url: None,
            panel_api: None,
            cache_duration_seconds: 0,
        }
    }

    // Helper function to create a ConfigInputAlias instance
    fn create_config_input_alias(id: u16, url: &str, priority: i16, max_connections: u16) -> ConfigInputAlias {
        ConfigInputAlias {
            id,
            name: concat_string!("alias_", &id.to_string()).intern(),
            url: url.to_string(),
            username: Some("alias_user".to_string()),
            password: Some("alias_pass".to_string()),
            priority,
            max_connections,
            exp_date: None,
            enabled: true,
        }
    }

    fn dummy_callback(_: &Arc<str>, _: usize) {}


    // Test acquiring with an alias
    #[test]
    fn test_provider_with_alias() {
        let mut input = create_config_input(1, &"provider1_1".intern(), 1, 1);
        let alias = create_config_input_alias(2, "http://alias1", 2, 2);

        // Adding alias to the provider
        input.aliases = Some(vec![alias]);

        let change_callback: ProviderConnectionChangeCallback = Arc::new(dummy_callback);
        let provider_connections: DashMap<Arc<str>, Arc<RwLock<ProviderConfigConnection>>> = DashMap::new();
        // Create MultiProviderLineup with the provider and alias
        let lineup = MultiProviderLineup::new(&input, &provider_connections, &change_callback);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            // Test that the alias provider is available
            should_available!(lineup, 1, 5);
            // Try acquiring again
            should_available!(lineup, 2, 5);
            should_available!(lineup, 2, 5);
            should_grace_period!(lineup, 1, 5);
            should_grace_period!(lineup, 2, 5);
            should_exhausted!(lineup, 5);
            should_exhausted!(lineup, 5);
        });
    }

    // Test acquiring from a MultiProviderLineup where the alias has a different priority
    #[test]
    fn test_provider_with_priority_alias() {
        let mut input = create_config_input(1, &"provider2_1".intern(), 1, 2);
        let alias = create_config_input_alias(2, "http://alias.com", 0, 2);
        // Adding alias with different priority
        input.aliases = Some(vec![alias]);
        let change_callback: ProviderConnectionChangeCallback = Arc::new(dummy_callback);
        let provider_connections: DashMap<Arc<str>, Arc<RwLock<ProviderConfigConnection>>> = DashMap::new();
        let lineup = MultiProviderLineup::new(&input, &provider_connections, &change_callback);
        // The alias has a higher priority, so the alias should be acquired first
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            for _ in 0..2 {
                should_available!(lineup, 2, 5);
            }
            should_available!(lineup, 1, 5);
        });
    }

    // Test provider when there are multiple aliases, all with distinct priorities
    #[test]
    fn test_provider_with_multiple_aliases() {
        let mut input = create_config_input(1, &"provider3_1".intern(), 1, 1);
        let alias1 = create_config_input_alias(2, "http://alias1.com", 1, 2);
        let alias2 = create_config_input_alias(3, "http://alias2.com", 0, 1);

        // Adding multiple aliases
        input.aliases = Some(vec![alias1, alias2]);
        let change_callback: ProviderConnectionChangeCallback = Arc::new(dummy_callback);
        let provider_connections: DashMap<Arc<str>, Arc<RwLock<ProviderConfigConnection>>> = DashMap::new();
        let lineup = MultiProviderLineup::new(&input, &provider_connections, &change_callback);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            // The alias with priority 0 should be acquired first (higher priority)
            should_available!(lineup, 3, 5);
            // Acquire again, and provider should still be available (with remaining capacity)
            should_available!(lineup, 1, 5);
            // // Check that the second alias with priority 2 is considered next
            should_available!(lineup, 2, 5);
            should_available!(lineup, 2, 5);

            should_grace_period!(lineup, 3, 5);
            should_grace_period!(lineup, 1, 5);
            should_grace_period!(lineup, 2, 5);

            should_exhausted!(lineup, 5);
        });
    }


    // Test acquiring when all aliases are exhausted
    #[test]
    fn test_provider_with_exhausted_aliases() {
        let mut input = create_config_input(1, &"provider4_1".intern(), 1, 1);
        let alias1 = create_config_input_alias(2, "http://alias.com", 2, 1);
        let alias2 = create_config_input_alias(3, "http://alias.com", -2, 1);

        // Adding alias
        input.aliases = Some(vec![alias1, alias2]);
        let change_callback: ProviderConnectionChangeCallback = Arc::new(dummy_callback);
        let provider_connections: DashMap<Arc<str>, Arc<RwLock<ProviderConfigConnection>>> = DashMap::new();
        let lineup = MultiProviderLineup::new(&input, &provider_connections, &change_callback);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            // Acquire connection from alias2
            should_available!(lineup, 3, 5);
            // Acquire connection from provider1
            should_available!(lineup, 1, 5);
            // Acquire connection from alias1
            should_available!(lineup, 2, 5);

            // Acquire connection from alias2
            should_grace_period!(lineup, 3, 5);
            // Acquire connection from provider1
            should_grace_period!(lineup, 1, 5);
            // Acquire connection from alias1
            should_grace_period!(lineup, 2, 5);

            // Now, all are exhausted
            should_exhausted!(lineup, 5);
        });
    }

    // Test acquiring a connection when there is available capacity
    #[test]
    fn test_acquire_when_capacity_available() {
        let cfg = create_config_input(1, &"provider5_1".intern(), 1, 2);
        let change_callback: ProviderConnectionChangeCallback = Arc::new(dummy_callback);
        let provider_connections: DashMap<Arc<str>, Arc<RwLock<ProviderConfigConnection>>> = DashMap::new();
        let connection = get_or_create_provider_connection(&provider_connections, &cfg.name);
        let lineup = SingleProviderLineup::new(&cfg, connection, &change_callback);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            // First acquire attempt should succeed
            should_available!(lineup, 1, 5);
            // Second acquire attempt should succeed as well
            should_available!(lineup, 1, 5);
            // Third with grace time
            should_grace_period!(lineup, 1, 5);
            // Fourth acquire attempt should fail as the provider is exhausted
            should_exhausted!(lineup, 5);
        });
    }


    // Test releasing a connection
    #[test]
    fn test_release_connection() {
        let cfg = create_config_input(1, &"provider7_1".intern(), 1, 2);
        let change_callback: ProviderConnectionChangeCallback = Arc::new(dummy_callback);
        let provider_connections: DashMap<Arc<str>, Arc<RwLock<ProviderConfigConnection>>> = DashMap::new();
        let connection = get_or_create_provider_connection(&provider_connections, &cfg.name);
        let lineup = SingleProviderLineup::new(&cfg, connection, &change_callback);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            // Acquire two connections
            should_available!(lineup, 1, 5);
            should_available!(lineup, 1, 5);
            should_grace_period!(lineup, 1, 5);
            should_exhausted!(lineup, 5);
            lineup.release(&"provider7_1".intern()).await;
            should_grace_period!(lineup, 1, 5);
            lineup.release(&"provider7_1".intern()).await;
            lineup.release(&"provider7_1".intern()).await;
            should_available!(lineup, 1, 5);
            should_grace_period!(lineup, 1, 5);
            should_exhausted!(lineup, 5);
        });
    }

    // Test acquiring with MultiProviderLineup and round-robin allocation
    #[test]
    fn test_multi_provider_acquire() {
        let mut cfg1 = create_config_input(1, &"provider8_1".intern(), 1, 2);
        let alias = create_config_input_alias(2, "http://alias1", 1, 1);

        // Adding alias to the provider
        cfg1.aliases = Some(vec![alias]);

        // Create MultiProviderLineup with the provider and alias
        let change_callback: ProviderConnectionChangeCallback = Arc::new(dummy_callback);
        let provider_connections: DashMap<Arc<str>, Arc<RwLock<ProviderConfigConnection>>> = DashMap::new();
        let lineup = MultiProviderLineup::new(&cfg1, &provider_connections, &change_callback);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            // Test acquiring the first provider
            should_available!(lineup, 1, 5);

            // Test acquiring the second provider
            should_available!(lineup, 2, 5);

            // Test acquiring the first provider
            should_available!(lineup, 1, 5);

            should_grace_period!(lineup, 1, 5);
            should_grace_period!(lineup, 2, 5);

            lineup.release(&"provider8_1".intern()).await;
            lineup.release(&"alias_2".intern()).await;
            lineup.release(&"provider8_1".intern()).await;

            should_available!(lineup, 1, 5);
            should_grace_period!(lineup, 1, 5);
            should_grace_period!(lineup, 2, 5);

            should_exhausted!(lineup, 5);
        });
    }

    // Test concurrent access to `acquire` using multiple threads
    #[test]
    fn test_concurrent_acquire() {
        let cfg = create_config_input(1, &"provider9_1".intern(), 1, 2);
        let change_callback: ProviderConnectionChangeCallback = Arc::new(dummy_callback);
        let provider_connections: DashMap<Arc<str>, Arc<RwLock<ProviderConfigConnection>>> = DashMap::new();
        let connection = get_or_create_provider_connection(&provider_connections, &cfg.name);
        let lineup = Arc::new(SingleProviderLineup::new(&cfg, connection, &change_callback));

        let available_count = Arc::new(AtomicU16::new(2));
        let grace_period_count = Arc::new(AtomicU16::new(1));
        let exhausted_count = Arc::new(AtomicU16::new(2));

        for _ in 0..5 {
            let lineup_clone = Arc::clone(&lineup);
            let available = Arc::clone(&available_count);
            let grace_period = Arc::clone(&grace_period_count);
            let exhausted = Arc::clone(&exhausted_count);
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                match lineup_clone.acquire(true, 5).await {
                    ProviderAllocation::Exhausted => exhausted.fetch_sub(1, Ordering::Acquire),
                    ProviderAllocation::Available(_) => available.fetch_sub(1, Ordering::Acquire),
                    ProviderAllocation::GracePeriod(_) => grace_period.fetch_sub(1, Ordering::Acquire),
                }
            });
        }
        assert_eq!(exhausted_count.load(Ordering::Acquire), 0);
        assert_eq!(available_count.load(Ordering::Acquire), 0);
        assert_eq!(grace_period_count.load(Ordering::Acquire), 0);
    }
}
