use crate::model::{macros, ConfigInput, ConfigTarget, ProcessTargets};
use indexmap::IndexMap;
use parking_lot::RwLock;
use shared::error::{info_err_res, TuliproxError};
use shared::model::{
    ConfigProviderDto, ConfigSourceDto, DnsPrefer, DnsScheme, OnConnectErrorPolicy, OnResolveErrorPolicy, PatternTemplate,
    SourcesConfigDto,
};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDnsConfig {
    pub enabled: bool,
    pub refresh_secs: u64,
    pub prefer: DnsPrefer,
    pub max_addrs: Option<usize>,
    pub schemes: Vec<DnsScheme>,
    pub keep_vhost: bool,
    pub overrides: HashMap<String, Vec<IpAddr>>,
    pub on_resolve_error: OnResolveErrorPolicy,
    pub on_connect_error: OnConnectErrorPolicy,
}

impl ProviderDnsConfig {
    pub fn supports_scheme(&self, scheme: &str) -> bool {
        if !self.enabled {
            return false;
        }
        match scheme.to_ascii_lowercase().as_str() {
            "http" => self.schemes.contains(&DnsScheme::Http),
            "https" => self.schemes.contains(&DnsScheme::Https),
            _ => false,
        }
    }
}

impl From<&shared::model::ProviderDnsDto> for ProviderDnsConfig {
    fn from(dto: &shared::model::ProviderDnsDto) -> Self {
        Self {
            enabled: dto.enabled,
            refresh_secs: dto.refresh_secs,
            prefer: dto.prefer,
            max_addrs: dto.max_addrs,
            schemes: dto
                .schemes
                .as_ref()
                .filter(|list| !list.is_empty())
                .cloned()
                .unwrap_or_else(|| vec![DnsScheme::Http, DnsScheme::Https]),
            keep_vhost: dto.keep_vhost,
            overrides: dto.overrides.clone().unwrap_or_default(),
            on_resolve_error: dto.on_resolve_error,
            on_connect_error: dto.on_connect_error,
        }
    }
}

#[derive(Debug, Default)]
pub struct ProviderDnsCacheEntry {
    pub ips: Vec<IpAddr>,
    pub rr_index: AtomicUsize,
    pub last_ok: Option<SystemTime>,
    pub last_err: Option<String>,
}

impl Clone for ProviderDnsCacheEntry {
    fn clone(&self) -> Self {
        Self {
            ips: self.ips.clone(),
            rr_index: AtomicUsize::new(self.rr_index.load(Ordering::Relaxed)),
            last_ok: self.last_ok,
            last_err: self.last_err.clone(),
        }
    }
}

#[derive(Debug, Default)]
pub struct ProviderDnsCache {
    by_host: RwLock<HashMap<String, ProviderDnsCacheEntry>>,
}

impl ProviderDnsCache {
    pub fn select_ip_from(&self, host: &str, ips: &[IpAddr]) -> Option<IpAddr> {
        if ips.is_empty() {
            return None;
        }
        let len = ips.len();
        let mut guard = self.by_host.write();
        let entry = guard.entry(host.to_ascii_lowercase()).or_default();
        // `% len` guards against a stale index when the override list changed length.
        let idx = entry.rr_index.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |i| Some((i + 1) % len)).unwrap_or_else(|i| i) % len;
        Some(ips[idx])
    }

    pub fn select_cached_ip(&self, host: &str) -> Option<IpAddr> {
        let guard = self.by_host.read();
        let entry = guard.get(&host.to_ascii_lowercase())?;
        if entry.ips.is_empty() {
            return None;
        }
        let len = entry.ips.len();
        let idx = entry.rr_index.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |i| Some((i + 1) % len)).unwrap_or_else(|i| i) % len;
        Some(entry.ips[idx])
    }

    pub fn store_resolved(&self, host: &str, ips: Vec<IpAddr>) {
        let mut guard = self.by_host.write();
        let entry = guard.entry(host.to_ascii_lowercase()).or_default();
        let new_len = ips.len();
        entry.ips = ips;
        if new_len == 0 {
            entry.rr_index.store(0, Ordering::Relaxed);
        } else {
            entry.rr_index.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |i| Some(i % new_len)).ok();
        }
        entry.last_ok = Some(SystemTime::now());
        entry.last_err = None;
    }

    pub fn clear_resolved(&self, host: &str) {
        let mut guard = self.by_host.write();
        if let Some(entry) = guard.get_mut(&host.to_ascii_lowercase()) {
            entry.ips.clear();
            entry.rr_index.store(0, Ordering::Relaxed);
            entry.last_ok = None;
        }
    }

    pub fn mark_resolve_error(&self, host: &str, err: impl Into<String>) {
        let mut guard = self.by_host.write();
        let entry = guard.entry(host.to_ascii_lowercase()).or_default();
        entry.last_err = Some(err.into());
    }

    pub fn snapshot_resolved(&self) -> HashMap<String, Vec<IpAddr>> {
        let guard = self.by_host.read();
        guard
            .iter()
            .filter_map(|(host, entry)| (!entry.ips.is_empty()).then_some((host.clone(), entry.ips.clone())))
            .collect()
    }

    pub fn ip_count(&self, host: &str) -> usize {
        let guard = self.by_host.read();
        guard
            .get(&host.to_ascii_lowercase())
            .map_or(0, |entry| entry.ips.len())
    }
}

#[derive(Debug)]
pub struct ConfigProvider {
    pub name: Arc<str>,
    pub urls: Vec<Arc<str>>,
    pub current_url_index: AtomicUsize,
    pub dns: Option<ProviderDnsConfig>,
    pub dns_cache: Arc<ProviderDnsCache>,
}

impl Clone for ConfigProvider {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            urls: self.urls.clone(),
            current_url_index: AtomicUsize::new(self.current_url_index.load(Ordering::Relaxed)),
            dns: self.dns.clone(),
            dns_cache: Arc::clone(&self.dns_cache),
        }
    }
}


macros::from_impl!(ConfigProvider);
impl From<&ConfigProviderDto> for ConfigProvider {
    fn from(dto: &ConfigProviderDto) -> Self {
        let dns_cfg = dto.dns.as_ref().map(ProviderDnsConfig::from);
        let dns_cache = Arc::new(ProviderDnsCache::default());
        Self {
            name: dto.name.clone(),
            urls: dto.urls.clone(),
            current_url_index: AtomicUsize::new(0),
            dns: dns_cfg,
            dns_cache,
        }
    }
}

impl ConfigProvider {
    /// Gets the current URL from the provider
    pub fn get_current_url(&self) -> Option<&Arc<str>> {
        let index = self.current_url_index.load(Ordering::Relaxed);
        self.urls.get(index)
    }

    /// Resets the current URL index to 0
    pub fn reset_index(&self) {
        self.current_url_index.store(0, Ordering::Relaxed);
    }

    /// Gets the current URL index
    #[inline]
    pub fn get_current_index(&self) -> usize {
        self.current_url_index.load(Ordering::Relaxed)
    }

    pub fn get_dns_config(&self) -> Option<&ProviderDnsConfig> { self.dns.as_ref() }

    pub fn dns_enabled_for_scheme(&self, scheme: &str) -> bool {
        self.dns.as_ref().is_some_and(|cfg| cfg.supports_scheme(scheme))
    }

    pub fn select_ip_for_host(&self, host: &str) -> Option<IpAddr> {
        let dns = self.dns.as_ref()?;
        let normalized = host.trim().to_ascii_lowercase();
        if let Some(ips) = dns.overrides.get(&normalized) {
            return self.dns_cache.select_ip_from(&normalized, ips);
        }
        self.dns_cache.select_cached_ip(&normalized)
    }

    pub fn ip_count_for_host(&self, host: &str) -> usize {
        let Some(dns) = self.dns.as_ref() else {
            return 0;
        };
        let normalized = host.trim().to_ascii_lowercase();
        if let Some(ips) = dns.overrides.get(&normalized) {
            return ips.len();
        }
        self.dns_cache.ip_count(&normalized)
    }

    pub fn hostnames_from_urls(&self) -> Vec<String> {
        let mut hostnames = Vec::new();
        let mut seen = HashSet::new();
        for raw in &self.urls {
            let raw = raw.as_ref();
            let candidate = if raw.contains("://") {
                raw.to_string()
            } else {
                format!("http://{raw}")
            };
            let Ok(parsed) = Url::parse(&candidate) else {
                continue;
            };
            let Some(host) = parsed.host_str() else {
                continue;
            };
            if host.parse::<IpAddr>().is_ok() {
                continue;
            }
            let normalized = host.to_ascii_lowercase();
            if seen.insert(normalized.clone()) {
                hostnames.push(normalized);
            }
        }
        hostnames
    }

    pub fn store_resolved(&self, host: &str, ips: Vec<IpAddr>) { self.dns_cache.store_resolved(host, ips); }

    pub fn clear_resolved(&self, host: &str) { self.dns_cache.clear_resolved(host); }

    pub fn mark_resolve_error(&self, host: &str, err: impl Into<String>) { self.dns_cache.mark_resolve_error(host, err); }

    pub fn snapshot_resolved(&self) -> HashMap<String, Vec<IpAddr>> { self.dns_cache.snapshot_resolved() }

    pub fn snapshot_resolved_ordered(&self) -> IndexMap<String, Vec<IpAddr>> {
        let mut remaining = self.snapshot_resolved();
        if remaining.is_empty() {
            return IndexMap::new();
        }

        let mut ordered = IndexMap::new();
        for host in self.hostnames_from_urls() {
            if let Some(ips) = remaining.remove(&host) {
                ordered.insert(host, ips);
            }
        }

        let mut extras: Vec<_> = remaining.into_iter().collect();
        extras.sort_by(|left, right| left.0.cmp(&right.0));
        for (host, ips) in extras {
            ordered.insert(host, ips);
        }
        ordered
    }

    /// Rotates to next URL, checking if a full cycle has been completed.
    /// Returns None if we've cycled back to the `start_index`, indicating all URLs were tried.
    ///
    /// Use this method when you need to try all URLs exactly once before failing.
    /// Call `get_current_index()` at the start of a failover session to get the `start_index`.
    pub fn rotate_to_next_url_with_cycle_check(&self, start_index: usize) -> Option<&Arc<str>> {
        let len = self.urls.len();
        if len == 0 {
            return None;
        }

        let previous = self.current_url_index
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                let next = (current + 1) % len;
                // If we've cycled back to start, we've tried all URLs.
                (next != start_index).then_some(next)
            })
            .ok()?;
        let next = (previous + 1) % len;
        self.urls.get(next)
    }
}

#[derive(Debug, Clone)]
pub struct ConfigSource {
    pub inputs: Vec<Arc<str>>,
    pub targets: Vec<Arc<ConfigTarget>>,
}

impl ConfigSource {
    // Determines whether this source should be processed for the given user targets.
    //
    // Returns `true` if:
    // - `user_targets.targets` is empty (process all sources), OR
    // - At least one target in this source matches an ID in `user_targets.targets`
    //
    // Returns `false` otherwise.
    pub fn should_process_for_user_targets(&self, user_targets: &ProcessTargets) -> bool {
        user_targets.targets.is_empty()
            || self.targets.iter().any(|t| user_targets.targets.contains(&t.id))
    }
}

impl From<&ConfigSourceDto> for ConfigSource {
    fn from(dto: &ConfigSourceDto) -> Self {
        Self {
            inputs: dto.inputs.clone(),
            targets: dto.targets.iter().map(|c| Arc::new(ConfigTarget::from(c))).collect(),
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct SourcesConfig {
    pub batch_files: Vec<PathBuf>,
    pub templates: Option<Vec<PatternTemplate>>,
    pub provider: Vec<Arc<ConfigProvider>>,
    pub inputs: Vec<Arc<ConfigInput>>,
    pub sources: Vec<ConfigSource>,
}

macros::try_from_impl!(SourcesConfig);
impl TryFrom<&SourcesConfigDto> for SourcesConfig {
    type Error = TuliproxError;
    fn try_from(dto: &SourcesConfigDto) -> Result<Self, TuliproxError> {
        let mut inputs = Vec::<Arc<ConfigInput>>::new();
        let mut batch_files = Vec::<PathBuf>::new();
        let mut input_names = HashSet::new();
        let provider: Vec<_> = dto.provider.as_ref()
            .map(|list| list.iter().map(ConfigProvider::from).map(Arc::new).collect())
            .unwrap_or_default();

        for input_dto in &dto.inputs {
            let mut input = ConfigInput::from(input_dto);
            // Prepare input
            if let Some(path) = input.prepare(&provider)? {
                batch_files.push(path);
            }
            input_names.insert(input.name.clone());
            inputs.push(Arc::new(input));
        }

        let mut sources = Vec::new();
        for source_dto in &dto.sources {
            // Validate that all input references exist
            for input_name in &source_dto.inputs {
                if !input_names.contains(input_name) {
                    return info_err_res!("Source references unknown input: {input_name}");
                }
            }
            sources.push(ConfigSource::from(source_dto));
        }

        Ok(Self {
            batch_files,
            templates: dto.templates.clone(),
            provider,
            inputs,
            sources,
        })
    }
}

impl SourcesConfig {
    pub(crate) fn get_source_at(&self, idx: usize) -> Option<&ConfigSource> {
        self.sources.get(idx)
    }

    pub fn get_target_by_id(&self, target_id: u16) -> Option<Arc<ConfigTarget>> {
        for source in &self.sources {
            for target in &source.targets {
                if target.id == target_id {
                    return Some(Arc::clone(target));
                }
            }
        }
        None
    }

    pub fn get_source_inputs_by_target_by_name(&self, target_name: &str) -> Option<Vec<Arc<str>>> {
        for source in &self.sources {
            for target in &source.targets {
                if target.name == target_name {
                    return Some(source.inputs.clone());
                }
            }
        }
        None
    }

    /// Returns the targets that were specified as parameters.
    /// If invalid targets are found, the program will be terminated.
    /// The return value has `enabled` set to true, if selective targets should be processed, otherwise false.
    ///
    /// * `target_args` the program parameters given with `-target` parameter.
    /// * `sources` configured sources in config file
    ///
    pub fn validate_targets(&self, target_args: Option<&Vec<String>>) -> Result<ProcessTargets, TuliproxError> {
        let mut enabled = true;
        let inputs: Vec<u16> = self.inputs.iter().map(|i| i.id).collect();
        let mut targets: Vec<u16> = vec![];
        let mut target_names: Vec<String> = vec![];
        if let Some(user_targets) = target_args {
            let mut check_targets: HashMap<String, u16> = user_targets.iter().map(|t| (t.to_lowercase(), 0)).collect();
            for source in &self.sources {
                for target in &source.targets {
                    for user_target in user_targets {
                        let key = user_target.to_lowercase();
                        if target.name.eq_ignore_ascii_case(key.as_str()) {
                            targets.push(target.id);
                            target_names.push(target.name.clone());
                            if let Some(value) = check_targets.get(key.as_str()) {
                                check_targets.insert(key, value + 1);
                            }
                        }
                    }
                }
            }

            let missing_targets: Vec<String> = check_targets.iter().filter(|&(_, v)| *v == 0).map(|(k, _)| k.clone()).collect();
            if !missing_targets.is_empty() {
                return info_err_res!("No target found for {}", missing_targets.join(", "));
            }
            // let processing_targets: Vec<String> = check_targets.iter().filter(|&(_, v)| *v != 0).map(|(k, _)| k.to_string()).collect();
            // info!("Processing targets {}", processing_targets.join(", "));
        } else {
            enabled = false;
        }

        Ok(ProcessTargets {
            enabled,
            inputs,
            targets,
            target_names,
        })
    }

    pub fn get_unique_target_names(&self) -> HashSet<Cow<'_, str>> {
        let mut seen_names = HashSet::new();
        for source in &self.sources {
            for target in &source.targets {
                // check the target name is unique
                let target_name = Cow::Borrowed(target.name.as_str());
                seen_names.insert(target_name);
            }
        }
        seen_names
    }

    pub fn get_input_files(&self) -> HashSet<PathBuf> {
        let mut file_names = HashSet::new();
        for file in &self.batch_files {
            file_names.insert(file.clone());
        }
        file_names
    }

    pub fn get_input_by_name(&self, name: &Arc<str>) -> Option<&Arc<ConfigInput>> {
        self.inputs.iter().find(|i| &i.name == name)
    }

    pub fn get_provider_by_name(&self, name: &str) -> Option<&Arc<ConfigProvider>> {
        self.provider.iter().find(|p| p.name.as_ref() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::ConfigProvider;
    use shared::model::ConfigProviderDto;
    use std::net::IpAddr;

    #[test]
    fn hostnames_from_urls_preserve_definition_order() {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "p1".into(),
            urls: vec![
                "http://cdn-b.example.net".into(),
                "https://cdn-a.example.net/live".into(),
                "http://cdn-b.example.net/redundant".into(),
                "http://203.0.113.10".into(),
            ],
            dns: None,
        });

        assert_eq!(
            provider.hostnames_from_urls(),
            vec!["cdn-b.example.net".to_string(), "cdn-a.example.net".to_string()]
        );
    }

    #[test]
    fn snapshot_resolved_ordered_matches_url_order_and_stable_extras() {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "p1".into(),
            urls: vec![
                "http://cdn-a.example.net".into(),
                "http://cdn-b.example.net".into(),
                "http://cdn-c.example.net".into(),
            ],
            dns: None,
        });

        provider.store_resolved(
            "cdn-c.example.net",
            vec!["203.0.113.30".parse::<IpAddr>().expect("ip parse should work")],
        );
        provider.store_resolved(
            "cdn-a.example.net",
            vec!["203.0.113.10".parse::<IpAddr>().expect("ip parse should work")],
        );
        provider.store_resolved(
            "extra-b.example.net",
            vec!["203.0.113.50".parse::<IpAddr>().expect("ip parse should work")],
        );
        provider.store_resolved(
            "extra-a.example.net",
            vec!["203.0.113.40".parse::<IpAddr>().expect("ip parse should work")],
        );

        let ordered = provider.snapshot_resolved_ordered();
        let keys: Vec<_> = ordered.keys().cloned().collect();
        assert_eq!(
            keys,
            vec![
                "cdn-a.example.net".to_string(),
                "cdn-c.example.net".to_string(),
                "extra-a.example.net".to_string(),
                "extra-b.example.net".to_string(),
            ]
        );
    }
}
