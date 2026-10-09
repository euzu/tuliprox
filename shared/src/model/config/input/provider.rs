use crate::{
    defaults::is_false,
    error::TuliproxError,
    utils::{arc_str_serde, arc_str_vec_serde, Internable},
};
use log::warn;
use std::{collections::HashMap, net::IpAddr, sync::Arc};
use strum_macros::{AsRefStr, Display, EnumString};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigProviderDto {
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(with = "arc_str_vec_serde")]
    pub urls: Vec<Arc<str>>,
    #[serde(default, skip_serializing_if = "is_default_provider_url_selection_policy")]
    pub provider_url_selection_policy: ProviderUrlSelectionPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<ProviderDnsDto>,
}

impl ConfigProviderDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.name = self.name.trim().intern();
        if self.name.is_empty() {
            return Err(TuliproxError::ConfigInput("Name for provider is mandatory".to_string()));
        }
        self.urls = self.urls.drain(..).filter(|url| !url.trim().is_empty()).map(|u| u.trim().intern()).collect();
        if self.urls.is_empty() {
            return Err(TuliproxError::ConfigInput("Urls for provider is mandatory".to_string()));
        }
        if let Some(dns) = self.dns.as_mut() {
            dns.prepare()?;
        }
        Ok(())
    }
}

pub const fn default_provider_dns_refresh_secs() -> u64 { 300 }

pub const fn is_default_provider_dns_refresh_secs(v: &u64) -> bool { *v == default_provider_dns_refresh_secs() }

pub fn is_default_provider_url_selection_policy(v: &ProviderUrlSelectionPolicy) -> bool {
    *v == ProviderUrlSelectionPolicy::default()
}

pub fn is_default_dns_prefer(v: &DnsPrefer) -> bool { *v == DnsPrefer::default() }

pub fn is_default_on_resolve_error(v: &OnResolveErrorPolicy) -> bool { *v == OnResolveErrorPolicy::default() }

pub fn is_default_on_connect_error(v: &OnConnectErrorPolicy) -> bool { *v == OnConnectErrorPolicy::default() }

#[derive(
    Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default, EnumString, AsRefStr, Display,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ProviderUrlSelectionPolicy {
    #[default]
    ResumeLastWorking,
    RestartFromFirst,
}

#[derive(
    Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default, EnumString, AsRefStr, Display,
)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum DnsPrefer {
    Ipv4,
    Ipv6,
    #[default]
    System,
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, EnumString, AsRefStr, Display)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum DnsScheme {
    Http,
    Https,
}

#[derive(
    Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default, EnumString, AsRefStr, Display,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum OnResolveErrorPolicy {
    #[default]
    KeepLastGood,
    FallbackToHostname,
}

#[derive(
    Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default, EnumString, AsRefStr, Display,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum OnConnectErrorPolicy {
    #[default]
    TryNextIp,
    RotateProviderUrl,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProviderDnsDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    #[serde(
        default = "default_provider_dns_refresh_secs",
        skip_serializing_if = "is_default_provider_dns_refresh_secs"
    )]
    pub refresh_secs: u64,
    #[serde(default, skip_serializing_if = "is_default_dns_prefer")]
    pub prefer: DnsPrefer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_addrs: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schemes: Option<Vec<DnsScheme>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub keep_vhost: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<HashMap<String, Vec<IpAddr>>>,
    #[serde(default, skip_serializing_if = "is_default_on_resolve_error")]
    pub on_resolve_error: OnResolveErrorPolicy,
    #[serde(default, skip_serializing_if = "is_default_on_connect_error")]
    pub on_connect_error: OnConnectErrorPolicy,
}

impl Default for ProviderDnsDto {
    fn default() -> Self {
        Self {
            enabled: false,
            refresh_secs: default_provider_dns_refresh_secs(),
            prefer: DnsPrefer::default(),
            max_addrs: None,
            schemes: None,
            keep_vhost: false,
            overrides: None,
            on_resolve_error: OnResolveErrorPolicy::default(),
            on_connect_error: OnConnectErrorPolicy::default(),
        }
    }
}

impl ProviderDnsDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if self.refresh_secs == 0 {
            self.refresh_secs = default_provider_dns_refresh_secs();
        } else if self.refresh_secs < 10 {
            // Explicit low values are honored for fast failover, but flagged
            warn!(
                "Provider dns refresh_secs={} is below the recommended minimum of 10s; this increases resolver load",
                self.refresh_secs
            );
        }
        if self.max_addrs == Some(0) {
            return Err(TuliproxError::ConfigInput("Provider dns max_addrs must be >= 1 when set".to_string()));
        }
        if let Some(schemes) = self.schemes.as_mut() {
            let mut unique = Vec::with_capacity(schemes.len());
            for scheme in schemes.drain(..) {
                if !unique.contains(&scheme) {
                    unique.push(scheme);
                }
            }
            *schemes = unique;
            if schemes.is_empty() {
                self.schemes = None;
            }
        }

        if let Some(overrides) = self.overrides.as_mut() {
            let mut normalized: HashMap<String, Vec<IpAddr>> = HashMap::new();
            for (host, ips) in std::mem::take(overrides) {
                let host = host.trim().to_ascii_lowercase();
                if host.is_empty() {
                    return Err(TuliproxError::ConfigInput(
                        "Provider dns overrides hostname must not be empty".to_string(),
                    ));
                }
                if ips.is_empty() {
                    return Err(TuliproxError::ConfigInput(
                        "Provider dns overrides for host '{host}' must not be empty".to_string(),
                    ));
                }
                let entry = normalized.entry(host.clone()).or_default();
                for ip in ips {
                    if !entry.contains(&ip) {
                        entry.push(ip);
                    }
                }
            }
            if normalized.is_empty() {
                self.overrides = None;
            } else {
                *overrides = normalized;
            }
        }

        Ok(())
    }
}
