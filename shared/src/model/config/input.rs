use super::PanelApiConfigDto;
use crate::{
    defaults::{default_as_true, is_true, is_zero_i16, is_zero_u16},
    error::TuliproxError,
    model::{config::media_server_catalog::MediaServerInputConfigDto, EpgConfigDto, StalkerInputConfigDto},
    utils::{arc_str_serde, deserialize_timestamp, is_blank_optional_string, Internable},
};
use std::{collections::HashMap, sync::Arc};

#[macro_export]
macro_rules! apply_batch_aliases {
    ($source:expr, $batch_aliases:expr, $index:expr) => {{
        if $batch_aliases.is_empty() {
            $source.aliases = None;
            None
        } else {
            if let Some(aliases) = $source.aliases.as_mut() {
                let mut names = aliases.iter().map(|a| a.name.clone()).collect::<std::collections::HashSet<Arc<str>>>();
                names.insert($source.name.clone());

                for alias in $batch_aliases.into_iter() {
                    if !names.contains(&alias.name) {
                        aliases.push(alias)
                    }
                }
            } else {
                $source.aliases = Some($batch_aliases);
            }
            if let Some(index) = $index {
                let mut idx = index + 1;
                // set to the same id as the first alias, because the first alias is copied into this input
                $source.id = idx;
                if let Some(aliases) = $source.aliases.as_mut() {
                    for alias in aliases {
                        idx += 1;
                        alias.id = idx;
                    }
                }
                Some(idx)
            } else {
                None
            }
        }
    }};
}

#[macro_export]
macro_rules! check_provider_scheme_url {
    ($url:expr, $provider_names:expr) => {
        if $url.starts_with(PROVIDER_SCHEME_PREFIX) {
            let (host, _path) = match parse_provider_scheme_url_parts(&$url) {
                Ok(parts) => parts,
                Err(err) => {
                    return Err(TuliproxError::ConfigInput(format!(
                        "Malformed provider URL {}: {}",
                        sanitize_sensitive_info(&$url),
                        sanitize_sensitive_info(&err.to_string())
                    )));
                }
            };
            if !$provider_names.contains(host) {
                return Err(TuliproxError::ConfigInput(format!("Provider name {host} is not defined")));
            }
        }
    };
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputDto {
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub id: u16,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(default, rename = "type")]
    pub input_type: InputType,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg: Option<EpgConfigDto>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub persist: Option<String>,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Excludes the root account while keeping enabled aliases available.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub account_disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequential_group: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<ConfigInputOptionsDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_server: Option<MediaServerInputConfigDto>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub cache_duration: Option<String>,
    #[serde(skip)]
    pub cache_duration_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<ConfigInputAliasDto>>,
    #[serde(default, skip_serializing_if = "is_zero_i16")]
    pub priority: i16,
    #[serde(default)]
    pub max_connections: u16,
    #[serde(default, skip_serializing_if = "InputFetchMethod::is_default")]
    pub method: InputFetchMethod,
    #[serde(default, skip_serializing_if = "StagedInputType::is_default")]
    pub staged_type: StagedInputType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<ConfigInputStagedDto>,
    #[serde(default, deserialize_with = "deserialize_timestamp", skip_serializing_if = "Option::is_none")]
    pub exp_date: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_api: Option<PanelApiConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Vec<ConfigProviderDto>>,
    /// Stalker/Ministra portal configuration. Required when `input_type`
    /// is `Stalker` or `StalkerBatch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalker: Option<StalkerInputConfigDto>,
}

impl Default for ConfigInputDto {
    fn default() -> Self {
        ConfigInputDto {
            id: 0,
            name: "".intern(),
            input_type: InputType::default(),
            headers: HashMap::new(),
            url: String::new(),
            epg: None,
            username: None,
            password: None,
            persist: None,
            enabled: default_as_true(),
            account_disabled: false,
            sequential_group: None,
            options: None,
            media_server: None,
            cache_duration: None,
            cache_duration_seconds: 0,
            aliases: None,
            priority: 0,
            max_connections: 0,
            method: InputFetchMethod::default(),
            staged_type: StagedInputType::default(),
            staged: None,
            exp_date: None,
            panel_api: None,
            provider: None,
            stalker: None,
        }
    }
}

impl ConfigInputDto {
    pub fn new_with_type(input_type: InputType) -> Self {
        let stalker = input_type.is_stalker().then(StalkerInputConfigDto::default);
        Self {
            input_type,
            media_server: input_type.is_media_server().then(MediaServerInputConfigDto::default),
            stalker,
            ..Self::default()
        }
    }

    /// Validate and normalize the `stalker` sub-struct.
    ///
    /// Stalker/Ministra portals require *some* identity anchor — a MAC address
    /// or account credentials. The derived fields (`device_id`, `signature`,
    /// `device_id2`) are filled by the network layer at runtime; we only
    /// verify the user-supplied inputs here.
    pub fn prepare_stalker_input(&mut self) -> Result<(), TuliproxError> {
        prepare_stalker_config(
            &self.name,
            &self.input_type,
            &mut self.stalker,
            false,
            self.username.as_deref(),
            self.password.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests;

mod aliases;
mod kind;
mod options;
mod prepare;
mod provider;
mod stalker;
pub use aliases::ConfigInputAliasDto;
pub use kind::{
    ConfigInputStagedDto, InputCapabilities, InputFetchMethod, InputPersistence, InputType, StagedInputType,
};
pub use options::{default_flussonic_hls_catchup_max_duration_secs, ConfigInputOptionsDto, FlussonicHlsCatchup};
pub use provider::{
    default_provider_dns_refresh_secs, is_default_dns_prefer, is_default_on_connect_error, is_default_on_resolve_error,
    is_default_provider_dns_refresh_secs, is_default_provider_url_selection_policy, ConfigProviderDto, DnsPrefer,
    DnsScheme, OnConnectErrorPolicy, OnResolveErrorPolicy, ProviderDnsDto, ProviderUrlSelectionPolicy,
};
use stalker::prepare_stalker_config;
