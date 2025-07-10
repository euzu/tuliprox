use crate::api::model::provider_config::{ProviderConfig, ProviderConfigConnection, ProviderConfigWrapper};
use crate::model::{AppConfig, ConfigInput};
use arc_swap::ArcSwap;
use log::{debug, log_enabled};
use shared::utils::{default_grace_period_millis, default_grace_period_timeout_secs};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct ProviderConnectionGuard {
    allocation: ProviderAllocation,
}

impl ProviderConnectionGuard {
    pub fn new(allocation: ProviderAllocation) -> Self {
        Self {
            allocation,
        }
    }

    pub fn get_provider_name(&self) -> Option<String> {
        match self.allocation {
            ProviderAllocation::Exhausted => None,
            ProviderAllocation::Available(ref cfg) |
            ProviderAllocation::GracePeriod(ref cfg) => {
                Some(cfg.name.clone())
            }
        }
    }
    pub fn get_provider_config(&self) -> Option<Arc<ProviderConfig>> {
        match self.allocation {
            ProviderAllocation::Exhausted => None,
            ProviderAllocation::Available(ref cfg) |
            ProviderAllocation::GracePeriod(ref cfg) => {
                Some(Arc::clone(cfg))
            }
        }
    }
}

impl Deref for ProviderConnectionGuard {
    type Target = ProviderAllocation;
    fn deref(&self) -> &Self::Target {
        &self.allocation
    }
}

impl Drop for ProviderConnectionGuard {
    fn drop(&mut self) {
        match &self.allocation {
            ProviderAllocation::Exhausted => {}
            ProviderAllocation::Available(config) |
            ProviderAllocation::GracePeriod(config) => {
                // let manager = self.manager.clone();
                let provider_config = Arc::clone(config);
                tokio::spawn(async move {
                    provider_config.release().await;
                    // manager.release_connection(&provider_config.name).await;
                });
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum ProviderAllocation {
    Exhausted,
    Available(Arc<ProviderConfig>),
    GracePeriod(Arc<ProviderConfig>),
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

    async fn release(&self, provider_name: &str) {
        match self {
            ProviderLineup::Single(lineup) => lineup.release(provider_name).await,
            ProviderLineup::Multi(lineup) => lineup.release(provider_name).await,
        }
    }
}

/// Handles a single provider and ensures safe allocation/release of connections.
#[derive(Debug)]
struct SingleProviderLineup {
    provider: ProviderConfigWrapper,
}

impl SingleProviderLineup {
    fn new<'a, F>(cfg: &ConfigInput, get_connection: Option<F>) -> Self
    where
        F: Fn(&str) -> Option<&'a ProviderConfigConnection>,
    {
        Self {
            provider: ProviderConfigWrapper::new(ProviderConfig::new(cfg, get_connection)),
        }
    }

    async fn get_next(&self, grace_period_timeout_secs: u64) -> Option<Arc<ProviderConfig>> {
        self.provider.get_next(false, grace_period_timeout_secs).await
    }

    async fn acquire(&self, with_grace: bool, grace_period_timeout_secs: u64) -> ProviderAllocation {
        self.provider.try_allocate(with_grace, grace_period_timeout_secs).await
    }

    async fn release(&self, provider_name: &str) {
        if self.provider.name == provider_name {
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
    MultiProviderGroup(Mutex<usize>, Vec<ProviderConfigWrapper>),
}

impl ProviderPriorityGroup {
    async fn is_exhausted(&self) -> bool {
        match self {
            ProviderPriorityGroup::SingleProviderGroup(g) => g.is_exhausted().await,
            ProviderPriorityGroup::MultiProviderGroup(_, groups) => {
                for g in groups {
                    if !g.is_exhausted().await {
                        return false;
                    }
                }
                true
            }
        }
    }
}


/// Manages multiple providers, ensuring that connections are allocated in a round-robin manner based on priority.
#[repr(align(64))]
#[derive(Debug)]
struct MultiProviderLineup {
    providers: Vec<ProviderPriorityGroup>,
    index: AtomicUsize,
}

impl MultiProviderLineup {
    pub fn new<'a, F>(input: &ConfigInput, get_connection: Option<F>) -> Self
    where
        F: Fn(&str) -> Option<&'a ProviderConfigConnection> + Copy,
    {
        let mut inputs = vec![ProviderConfigWrapper::new(ProviderConfig::new(input, get_connection))];
        if let Some(aliases) = &input.aliases {
            for alias in aliases {
                inputs.push(ProviderConfigWrapper::new(ProviderConfig::new_alias(input, alias, get_connection)));
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
        values.sort_by(|(p1, _), (p2, _)| p1.cmp(p2));
        let providers: Vec<ProviderPriorityGroup> = values.into_iter().map(|(_, mut group)| {
            if group.len() > 1 {
                ProviderPriorityGroup::MultiProviderGroup(Mutex::new(0), group)
            } else {
                ProviderPriorityGroup::SingleProviderGroup(group.remove(0))
            }
        }).collect();

        Self {
            providers,
            index: AtomicUsize::new(0),
        }
    }

    /// Attempts to acquire the next available provider from a specific priority group.
    ///
    /// # Parameters
    /// - `priority_group`: Thep rovider group to search within.
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
    ///    ProviderAllocation::GracePeriodprovider) =>  println!("Provider with grace period {}", provider.name),
    /// }
    /// }
    /// ```
    async fn acquire_next_provider_from_group(priority_group: &ProviderPriorityGroup, grace: bool, grace_period_timeout_secs: u64) -> ProviderAllocation {
        match priority_group {
            ProviderPriorityGroup::SingleProviderGroup(p) => {
                let result = p.try_allocate(grace, grace_period_timeout_secs).await;
                match result {
                    ProviderAllocation::Exhausted => {}
                    ProviderAllocation::Available(_) | ProviderAllocation::GracePeriod(_) => return result
                }
            }
            ProviderPriorityGroup::MultiProviderGroup(index, pg) => {
                let provider_count = pg.len();
                let mut idx = {
                    *index.lock().await
                };
                let start = idx;

                for _ in start..provider_count {
                    let p = pg.get(idx).unwrap();
                    idx = (idx + 1) % provider_count;
                    let result = p.try_allocate(grace, grace_period_timeout_secs).await;
                    match result {
                        ProviderAllocation::Exhausted => {}
                        ProviderAllocation::Available(_) | ProviderAllocation::GracePeriod(_) => {
                            *index.lock().await = idx;
                            return result;
                        }
                    }
                }
                *index.lock().await = idx;
            }
        }
        ProviderAllocation::Exhausted
    }

    // Used for redirect to cylce through provider
    async fn get_next_provider_from_group(priority_group: &ProviderPriorityGroup, grace: bool, grace_period_timeout_secs: u64) -> Option<Arc<ProviderConfig>> {
        match priority_group {
            ProviderPriorityGroup::SingleProviderGroup(p) => {
                return p.get_next(grace, grace_period_timeout_secs).await;
            }
            ProviderPriorityGroup::MultiProviderGroup(index, pg) => {
                let mut idx_guard = index.lock().await;
                let mut idx = *idx_guard;
                let provider_count = pg.len();
                let start = idx;
                for _ in start..provider_count {
                    let p = pg.get(idx).unwrap();
                    idx = (idx + 1) % provider_count;
                    let result = p.get_next(grace, grace_period_timeout_secs).await;
                    if result.is_some() {
                        *idx_guard = idx;
                        return result;
                    }
                }
                *idx_guard = idx;
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
    ///    ProviderAllocation::GracePeriodprovider) =>  println!("Provider with grace period {}", provider.name),
    /// }
    /// ```
    async fn acquire(&self, with_grace: bool, grace_period_timeout_secs: u64) -> ProviderAllocation {
        let main_idx = self.index.load(Ordering::SeqCst);
        let provider_count = self.providers.len();

        for index in main_idx..provider_count {
            let priority_group = &self.providers[index];
            let allocation = {
                let without_grace_allocation = Self::acquire_next_provider_from_group(priority_group, false, grace_period_timeout_secs).await;
                if with_grace && matches!(without_grace_allocation, ProviderAllocation::Exhausted) {
                    Self::acquire_next_provider_from_group(priority_group, true, grace_period_timeout_secs).await
                } else {
                    without_grace_allocation
                }
            };
            match allocation {
                ProviderAllocation::Exhausted => {}
                ProviderAllocation::Available(_) |
                ProviderAllocation::GracePeriod(_) => {
                    if priority_group.is_exhausted().await {
                        self.index.store((index + 1) % provider_count, Ordering::SeqCst);
                    }
                    return allocation;
                }
            }
        }

        ProviderAllocation::Exhausted
    }

    // it intended to use with redirects to cycle through provider
    async fn get_next(&self, grace_period_timeout_secs: u64) -> Option<Arc<ProviderConfig>> {
        let main_idx = self.index.load(Ordering::SeqCst);
        let provider_count = self.providers.len();

        for index in main_idx..provider_count {
            let priority_group = &self.providers[index];
            let allocation = {
                let config = Self::get_next_provider_from_group(priority_group, false, grace_period_timeout_secs).await;
                if config.is_none() {
                    Self::get_next_provider_from_group(priority_group, true, grace_period_timeout_secs).await
                } else {
                    config
                }
            };
            match allocation {
                None => {}
                Some(config) => {
                    if priority_group.is_exhausted().await {
                        self.index.store((index + 1) % provider_count, Ordering::SeqCst);
                    }
                    return Some(config);
                }
            }
        }

        None
    }


    async fn release(&self, provider_name: &str) {
        for g in &self.providers {
            match g {
                ProviderPriorityGroup::SingleProviderGroup(pc) => {
                    if pc.name == provider_name {
                        pc.release().await;
                        break;
                    }
                }
                ProviderPriorityGroup::MultiProviderGroup(_, group) => {
                    for pc in group {
                        if pc.name == provider_name {
                            pc.release().await;
                            return;
                        }
                    }
                }
            }
        }
    }
}


struct ProviderLineupManager {
    grace_period_millis: AtomicU64,
    grace_period_timeout_secs: AtomicU64,
    inputs: Arc<ArcSwap<Vec<Arc<ConfigInput>>>>,
    providers: Arc<ArcSwap<Vec<ProviderLineup>>>,
}
impl ProviderLineupManager {
    pub fn new(inputs: Vec<Arc<ConfigInput>>, grace_period_millis: u64, grace_period_timeout_secs: u64) -> Self {
        let lineups = inputs.iter().map(|i| Self::create_lineup(i, None)).collect();
        Self {
            grace_period_millis: AtomicU64::new(grace_period_millis),
            grace_period_timeout_secs: AtomicU64::new(grace_period_timeout_secs),
            inputs: Arc::new(ArcSwap::from_pointee(inputs)),
            providers: Arc::new(ArcSwap::from_pointee(lineups)),
        }
    }
    fn create_lineup(input: &ConfigInput, provider_connections: Option<&HashMap<&str, ProviderConfigConnection>>) -> ProviderLineup {
        let get_connections = provider_connections.map(|c| |name: &str| c.get(name));

        if input.aliases.as_ref().is_some_and(|a| !a.is_empty()) {
            ProviderLineup::Multi(MultiProviderLineup::new(input, get_connections))
        } else {
            ProviderLineup::Single(SingleProviderLineup::new(input, get_connections))
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
                        || a_alias.priority != b_alias.priority
                        || a_alias.username != b_alias.username
                        || a_alias.password != b_alias.password
                        || a_alias.url != b_alias.url
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

    pub async fn update_config(&self, new_inputs: Vec<Arc<ConfigInput>>, grace_period_millis: u64, grace_period_timeout_secs: u64) {
        self.grace_period_millis.store(grace_period_millis, Ordering::Relaxed);
        self.grace_period_timeout_secs.store(grace_period_timeout_secs, Ordering::Relaxed);

        if !self.has_changed(&new_inputs) {
            return;
        }

        let old_lineups = self.providers.load();
        let mut provider_connections = HashMap::new();
        for lineup in old_lineups.iter() {
            match lineup {
                ProviderLineup::Single(single) => {
                    provider_connections.insert(single.provider.name.as_str(), single.provider.get_connection_info().await);
                }
                ProviderLineup::Multi(multi) => {
                    for group in &multi.providers {
                        match group {
                            ProviderPriorityGroup::SingleProviderGroup(cfg) => {
                                provider_connections.insert(cfg.name.as_str(), cfg.get_connection_info().await);
                            }
                            ProviderPriorityGroup::MultiProviderGroup(_, cfgs) => {
                                for cfg in cfgs {
                                    provider_connections.insert(cfg.name.as_str(), cfg.get_connection_info().await);
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut new_lineups: Vec<ProviderLineup> = Vec::with_capacity(new_inputs.len());
        let connections = Some(provider_connections);
        for input in &new_inputs {
            new_lineups.push(Self::create_lineup(input, connections.as_ref()));
        }

        debug!("inputs {new_inputs:?}");
        debug!("lineup {new_lineups:?}");

        self.inputs.store(Arc::new(new_inputs));
        self.providers.store(Arc::new(new_lineups));

    }

    fn get_provider_config<'a>(name: &str, providers: &'a Vec<ProviderLineup>) -> Option<(&'a ProviderLineup, &'a ProviderConfigWrapper)> {
        for lineup in providers {
            match lineup {
                ProviderLineup::Single(single) => {
                    if single.provider.name == name {
                        return Some((lineup, &single.provider));
                    }
                }
                ProviderLineup::Multi(multi) => {
                    for group in &multi.providers {
                        match group {
                            ProviderPriorityGroup::SingleProviderGroup(single) => {
                                if single.name == name {
                                    return Some((lineup, single));
                                }
                            }
                            ProviderPriorityGroup::MultiProviderGroup(_, configs) => {
                                for config in configs {
                                    if config.name == name {
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

    pub async fn force_exact_acquire_connection(&self, provider_name: &str) -> ProviderConnectionGuard {
        let providers = self.providers.load();
        let allocation = match Self::get_provider_config(provider_name, &providers) {
            None => ProviderAllocation::Exhausted, // No Name matched, we don't have this provider
            Some((_lineup, config)) => config.force_allocate().await,
        };

        ProviderConnectionGuard::new(allocation)
    }

    // Returns the next available provider connection
    pub async fn acquire_connection(&self, input_name: &str) -> ProviderConnectionGuard {
        let providers = self.providers.load();
        let allocation = match Self::get_provider_config(input_name, &providers) {
            None => ProviderAllocation::Exhausted, // No Name matched, we don't have this provider
            Some((lineup, _config)) => lineup.acquire(self.grace_period_millis.load(Ordering::Relaxed) > 0,
                                                      self.grace_period_timeout_secs.load(Ordering::Relaxed)).await
        };

        if log_enabled!(log::Level::Debug) {
            match allocation {
                ProviderAllocation::Exhausted => {}
                ProviderAllocation::Available(ref cfg) |
                ProviderAllocation::GracePeriod(ref cfg) => {
                    debug!("Using provider {}", cfg.name);
                }
            }
        }

        ProviderConnectionGuard::new(allocation)
    }

    // This method is used for redirects to cycle through provider
    //
    pub async fn get_next_provider(&self, input_name: &str) -> Option<Arc<ProviderConfig>> {
        let providers = self.providers.load();
        match Self::get_provider_config(input_name, &providers) {
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

    // we need the provider_name to exactly release this provider
    pub async fn release_connection(&self, provider_name: &str) {
        let providers = self.providers.load();
        if let Some((lineup, _config)) = Self::get_provider_config(provider_name, &providers) {
            lineup.release(provider_name).await;
        }
    }

    pub async fn active_connections(&self) -> Option<HashMap<String, usize>> {
        let mut result = HashMap::<String, usize>::new();
        let mut add_provider = async |provider: &ProviderConfig| {
            let count = provider.get_current_connections().await;
            if count > 0 {
                result.insert(provider.name.to_string(), count);
            }
        };
        let providers = self.providers.load();
        for lineup in providers.iter() {
            match lineup {
                ProviderLineup::Single(provider_lineup) => {
                    add_provider(&provider_lineup.provider).await;
                }
                ProviderLineup::Multi(provider_lineup) => {
                    for provider_group in &provider_lineup.providers {
                        match provider_group {
                            ProviderPriorityGroup::SingleProviderGroup(provider) => {
                                add_provider(provider).await;
                            }
                            ProviderPriorityGroup::MultiProviderGroup(_, providers) => {
                                for provider in providers {
                                    add_provider(provider).await;
                                }
                            }
                        }
                    }
                }
            }
        }
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    pub async fn is_over_limit(&self, provider_name: &str) -> bool {
        let providers = self.providers.load();
        if let Some((_, config)) = Self::get_provider_config(provider_name, &providers) {
            config.is_over_limit(self.grace_period_timeout_secs.load(Ordering::Relaxed)).await
        } else {
            false
        }
    }
}

pub struct ActiveProviderManager {
    providers: ProviderLineupManager,
}

impl ActiveProviderManager {
    pub fn new(cfg: &AppConfig) -> Self {
        let (grace_period_millis, grace_period_timeout_secs) = Self::get_grace_options(cfg);
        let inputs = Self::get_config_inputs(cfg);
        Self {
            providers: ProviderLineupManager::new(inputs, grace_period_millis, grace_period_timeout_secs),
        }
    }

    fn get_config_inputs(cfg: &AppConfig) -> Vec<Arc<ConfigInput>> {
        cfg.sources.load().sources
            .iter().flat_map(|s| s.inputs.iter().map(Arc::clone)).collect()
    }

    fn get_grace_options(cfg: &AppConfig) -> (u64, u64) {
        let (grace_period_millis, grace_period_timeout_secs) = cfg.config.load().reverse_proxy.as_ref()
            .and_then(|r| r.stream.as_ref())
            .map_or_else(|| (default_grace_period_millis(), default_grace_period_timeout_secs()), |s| (s.grace_period_millis, s.grace_period_timeout_secs));
        (grace_period_millis, grace_period_timeout_secs)
    }

    pub async fn update_config(&self, cfg: &AppConfig) {
        let (grace_period_millis, grace_period_timeout_secs) = Self::get_grace_options(cfg);
        let inputs = Self::get_config_inputs(cfg);
        self.providers.update_config(inputs, grace_period_millis, grace_period_timeout_secs).await;
    }

    pub async fn force_exact_acquire_connection(&self, provider_name: &str) -> ProviderConnectionGuard {
        self.providers.force_exact_acquire_connection(provider_name).await
    }

    // Returns the next available provider connection
    pub async fn acquire_connection(&self, input_name: &str) -> ProviderConnectionGuard {
        self.providers.acquire_connection(input_name).await
    }

    // This method is used for redirects to cycle through provider
    pub async fn get_next_provider(&self, provider_name: &str) -> Option<Arc<ProviderConfig>> {
        self.providers.get_next_provider(provider_name).await
    }

    // we need the provider_name to exactly release this provider
    pub async fn release_connection(&self, provider_name: &str) {
        self.providers.release_connection(provider_name).await;
    }

    pub async fn active_connections(&self) -> Option<HashMap<String, usize>> {
        self.providers.active_connections().await
    }

    pub async fn is_over_limit(&self, provider_name: &str) -> bool {
        self.providers.is_over_limit(provider_name).await
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
    fn create_config_input(id: u16, name: &str, priority: i16, max_connections: u16) -> ConfigInput {
        ConfigInput {
            id,
            name: name.to_string(),
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
        }
    }

    // Helper function to create a ConfigInputAlias instance
    fn create_config_input_alias(id: u16, url: &str, priority: i16, max_connections: u16) -> ConfigInputAlias {
        ConfigInputAlias {
            id,
            name: format!("alias_{id}"),
            url: url.to_string(),
            username: Some("alias_user".to_string()),
            password: Some("alias_pass".to_string()),
            priority,
            max_connections,
        }
    }

    // Test acquiring with an alias
    #[test]
    fn test_provider_with_alias() {
        let mut input = create_config_input(1, "provider1_1", 1, 1);
        let alias = create_config_input_alias(2, "http://alias1", 2, 2);

        // Adding alias to the provider
        input.aliases = Some(vec![alias]);

        // Create MultiProviderLineup with the provider and alias
        let lineup = MultiProviderLineup::new(&input, None);
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

    // // Test acquiring from a MultiProviderLineup where the alias has a different priority
    #[test]
    fn test_provider_with_priority_alias() {
        let mut input = create_config_input(1, "provider2_1", 1, 2);
        let alias = create_config_input_alias(2, "http://alias.com", 0, 2);
        // Adding alias with different priority
        input.aliases = Some(vec![alias]);
        let lineup = MultiProviderLineup::new(&input, None);
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
        let mut input = create_config_input(1, "provider3_1", 1, 1);
        let alias1 = create_config_input_alias(2, "http://alias1.com", 1, 2);
        let alias2 = create_config_input_alias(3, "http://alias2.com", 0, 1);

        // Adding multiple aliases
        input.aliases = Some(vec![alias1, alias2]);

        let lineup = MultiProviderLineup::new(&input, None);
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


    // // Test acquiring when all aliases are exhausted
    #[test]
    fn test_provider_with_exhausted_aliases() {
        let mut input = create_config_input(1, "provider4_1", 1, 1);
        let alias1 = create_config_input_alias(2, "http://alias.com", 2, 1);
        let alias2 = create_config_input_alias(3, "http://alias.com", -2, 1);

        // Adding alias
        input.aliases = Some(vec![alias1, alias2]);

        let lineup = MultiProviderLineup::new(&input, None);
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
        let cfg = create_config_input(1, "provider5_1", 1, 2);
        let lineup = SingleProviderLineup::new(&cfg, None);
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
        let cfg = create_config_input(1, "provider7_1", 1, 2);
        let lineup = SingleProviderLineup::new(&cfg, None);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            // Acquire two connections
            should_available!(lineup, 1, 5);
            should_available!(lineup, 1, 5);
            should_grace_period!(lineup, 1, 5);
            should_exhausted!(lineup, 5);
            lineup.release("provider7_1").await;
            should_grace_period!(lineup, 1, 5);
            lineup.release("provider7_1").await;
            lineup.release("provider7_1").await;
            should_available!(lineup, 1, 5);
            should_grace_period!(lineup, 1, 5);
            should_exhausted!(lineup, 5);
        });
    }

    // Test acquiring with MultiProviderLineup and round-robin allocation
    #[test]
    fn test_multi_provider_acquire() {
        let mut cfg1 = create_config_input(1, "provider8_1", 1, 2);
        let alias = create_config_input_alias(2, "http://alias1", 1, 1);

        // Adding alias to the provider
        cfg1.aliases = Some(vec![alias]);

        // Create MultiProviderLineup with the provider and alias
        let lineup = MultiProviderLineup::new(&cfg1, None);
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

            lineup.release("provider8_1").await;
            lineup.release("alias_2").await;
            lineup.release("provider8_1").await;

            should_available!(lineup, 1, 5);
            should_grace_period!(lineup, 1, 5);
            should_grace_period!(lineup, 2, 5);

            should_exhausted!(lineup, 5);
        });
    }

    // Test concurrent access to `acquire` using multiple threads
    #[test]
    fn test_concurrent_acquire() {
        let cfg = create_config_input(1, "provider9_1", 1, 2);
        let lineup = Arc::new(SingleProviderLineup::new(&cfg, None));

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
                    ProviderAllocation::Exhausted => exhausted.fetch_sub(1, Ordering::SeqCst),
                    ProviderAllocation::Available(_) => available.fetch_sub(1, Ordering::SeqCst),
                    ProviderAllocation::GracePeriod(_) => grace_period.fetch_sub(1, Ordering::SeqCst),
                }
            });
        }
        assert_eq!(exhausted_count.load(Ordering::SeqCst), 0);
        assert_eq!(available_count.load(Ordering::SeqCst), 0);
        assert_eq!(grace_period_count.load(Ordering::SeqCst), 0);
    }
}
