use super::PanelApiConfigDto;
use crate::{
    check_input_connections, check_input_credentials,
    defaults::{
        default_as_true, default_probe_delay_secs, default_probe_live_interval, default_resolve_background,
        default_resolve_delay_secs, default_xtream_live_stream_use_prefix, is_default_probe_delay_secs,
        is_default_probe_live_interval, is_default_resolve_delay_secs, is_false, is_true, is_zero_i16, is_zero_u16,
    },
    error::TuliproxError,
    foundation::{get_filter, Filter},
    model::{
        config::media_server_catalog::MediaServerInputConfigDto, ClusterFlags, EpgConfigDto, PatternTemplate,
        StalkerAuthMode, StalkerInputConfigDto,
    },
    utils::{
        arc_str_option_serde, arc_str_serde, arc_str_vec_serde, deserialize_timestamp, get_credentials_from_url_str,
        get_trimmed_string, is_blank_optional_arc_str, is_blank_optional_string, is_non_blank_optional_string,
        parse_duration_seconds, parse_provider_scheme_url_parts, sanitize_sensitive_info,
        serialize_option_vec_flow_map_items, trim_last_slash, Internable, BATCH_SCHEME_PREFIX, PROVIDER_SCHEME_PREFIX,
    },
};
use log::warn;
use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    sync::Arc,
};
use strum_macros::{AsRefStr, Display, EnumIter, EnumString};

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

#[derive(
    Debug,
    Copy,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    Default,
    EnumIter,
    Display,
    EnumString,
    AsRefStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum InputType {
    #[default]
    M3u,
    Xtream,
    M3uBatch,
    XtreamBatch,
    Stalker,
    StalkerBatch,
    Library,
    Emby,
    Jellyfin,
    Plex,
    Staged,
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default, Display, EnumString)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StagedInputType {
    #[default]
    M3u,
    Xtream,
}

impl StagedInputType {
    pub fn is_default(value: &Self) -> bool { matches!(value, Self::M3u) }

    pub const fn input_type(self) -> InputType {
        match self {
            Self::M3u => InputType::M3u,
            Self::Xtream => InputType::Xtream,
        }
    }
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputStagedDto {
    #[serde(
        default,
        alias = "provider",
        skip_serializing_if = "is_blank_optional_arc_str",
        with = "arc_str_option_serde"
    )]
    pub for_input: Option<Arc<str>>,
    #[serde(default)]
    pub clusters: ClusterFlags,
}

impl InputType {
    pub fn is_xtream(&self) -> bool { matches!(self, Self::Xtream | Self::XtreamBatch) }
    pub fn is_m3u(&self) -> bool { matches!(self, Self::M3u | Self::M3uBatch) }
    pub fn is_stalker(&self) -> bool { matches!(self, Self::Stalker | Self::StalkerBatch) }
    pub fn uses_standard_input_url(&self) -> bool {
        matches!(
            self,
            Self::M3u | Self::Xtream | Self::M3uBatch | Self::XtreamBatch | Self::Stalker | Self::StalkerBatch
        )
    }
    pub fn is_library(&self) -> bool { matches!(self, Self::Library) }
    pub fn is_media_server(&self) -> bool { matches!(self, Self::Emby | Self::Jellyfin | Self::Plex) }
    pub fn is_batch(&self) -> bool { matches!(self, Self::M3uBatch | Self::XtreamBatch | Self::StalkerBatch) }
    pub fn is_staged(&self) -> bool { matches!(self, Self::Staged) }

    /// Single source of truth for the categorical behavior of an input type.
    ///
    /// Adding a new [`InputType`] variant forces an arm here (the match is
    /// exhaustive), and every site that consumes [`InputCapabilities`] —
    /// persistence/load routing, probe requirements, the custom-provider
    /// endpoint gate — stays in sync automatically instead of relying on a
    /// parallel `match` somewhere else that is easy to forget.
    #[must_use]
    pub const fn capabilities(self) -> InputCapabilities {
        match self {
            Self::M3u | Self::M3uBatch => InputCapabilities {
                persistence: InputPersistence::M3u,
                requires_provider_connection_for_probe: true,
                served_on_custom_provider_endpoint: true,
            },
            Self::Xtream | Self::XtreamBatch => InputCapabilities {
                persistence: InputPersistence::Xtream,
                requires_provider_connection_for_probe: true,
                served_on_custom_provider_endpoint: true,
            },
            Self::Stalker | Self::StalkerBatch => InputCapabilities {
                persistence: InputPersistence::Stalker,
                requires_provider_connection_for_probe: true,
                served_on_custom_provider_endpoint: true,
            },
            Self::Library => InputCapabilities {
                persistence: InputPersistence::Library,
                requires_provider_connection_for_probe: false,
                served_on_custom_provider_endpoint: false,
            },
            Self::Emby | Self::Jellyfin | Self::Plex => InputCapabilities {
                persistence: InputPersistence::MediaServer,
                requires_provider_connection_for_probe: false,
                served_on_custom_provider_endpoint: false,
            },
            Self::Staged => InputCapabilities {
                persistence: InputPersistence::M3u,
                requires_provider_connection_for_probe: false,
                served_on_custom_provider_endpoint: false,
            },
        }
    }

    /// Persistence/load backend family for this input type.
    #[must_use]
    pub const fn persistence(self) -> InputPersistence { self.capabilities().persistence }
}

/// Storage/loading backend family an [`InputType`] maps onto.
///
/// Multiple input variants collapse onto the same persistence family (for
/// example every media-server variant shares the same on-disk format), so
/// persist/load routing can match on this instead of re-listing variants.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum InputPersistence {
    M3u,
    Xtream,
    Library,
    MediaServer,
    Stalker,
}

/// Categorical capabilities of an [`InputType`], declared once in
/// [`InputType::capabilities`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct InputCapabilities {
    /// Storage/loading backend family.
    pub persistence: InputPersistence,
    /// Whether generic stream probing must open a provider connection.
    pub requires_provider_connection_for_probe: bool,
    /// Whether the custom-provider HTTP endpoint can serve this input.
    pub served_on_custom_provider_endpoint: bool,
}

#[derive(
    Debug,
    Copy,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    EnumIter,
    PartialEq,
    Eq,
    Default,
    Display,
    EnumString,
    AsRefStr,
)]
#[strum(serialize_all = "UPPERCASE")]
#[serde(rename_all = "UPPERCASE")]
pub enum InputFetchMethod {
    #[default]
    GET,
    POST,
}

impl InputFetchMethod {
    pub fn is_default(value: &InputFetchMethod) -> bool { matches!(value, Self::GET) }
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputOptionsDto {
    #[serde(default, alias = "xtream_skip_live", alias = "stalker_skip_live", skip_serializing_if = "is_false")]
    pub skip_live: bool,
    #[serde(default, alias = "xtream_skip_vod", alias = "stalker_skip_vod", skip_serializing_if = "is_false")]
    pub skip_vod: bool,
    #[serde(default, alias = "xtream_skip_series", alias = "stalker_skip_series", skip_serializing_if = "is_false")]
    pub skip_series: bool,
    #[serde(default = "default_xtream_live_stream_use_prefix", skip_serializing_if = "is_true")]
    pub xtream_live_stream_use_prefix: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub xtream_live_stream_without_extension: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disable_hls_streaming: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resolve_tmdb: bool,
    #[serde(default = "default_resolve_background", skip_serializing_if = "is_true")]
    pub resolve_background: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resolve_series: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resolve_vod: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub probe_series: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub probe_vod: bool,
    #[serde(default = "default_resolve_delay_secs", skip_serializing_if = "is_default_resolve_delay_secs")]
    pub resolve_delay: u16,
    #[serde(default = "default_probe_delay_secs", skip_serializing_if = "is_default_probe_delay_secs")]
    pub probe_delay: u16,
    #[serde(default, alias = "resolve_live", skip_serializing_if = "is_false")]
    pub probe_live: bool,
    #[serde(
        default = "default_probe_live_interval",
        alias = "resolve_live_interval_hours",
        skip_serializing_if = "is_default_probe_live_interval"
    )]
    pub probe_live_interval_hours: u32,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub resolve_filter: Option<String>,
    #[serde(skip)]
    pub t_resolve_filter: Option<Filter>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub probe_filter: Option<String>,
    #[serde(skip)]
    pub t_probe_filter: Option<Filter>,
    /// Bulk-fetch EPG for live channels during playlist processing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stalker_bulk_epg: bool,
}

impl Default for ConfigInputOptionsDto {
    fn default() -> Self {
        ConfigInputOptionsDto {
            skip_live: false,
            skip_vod: false,
            skip_series: false,
            xtream_live_stream_use_prefix: default_xtream_live_stream_use_prefix(),
            xtream_live_stream_without_extension: false,
            disable_hls_streaming: false,
            resolve_tmdb: false,
            resolve_background: default_resolve_background(),
            resolve_series: false,
            resolve_vod: false,
            probe_series: false,
            probe_vod: false,
            resolve_delay: default_resolve_delay_secs(),
            probe_delay: default_probe_delay_secs(),
            probe_live: false,
            probe_live_interval_hours: default_probe_live_interval(),
            resolve_filter: None,
            t_resolve_filter: None,
            probe_filter: None,
            t_probe_filter: None,
            stalker_bulk_epg: false,
        }
    }
}

impl ConfigInputOptionsDto {
    pub fn is_empty(&self) -> bool {
        !self.skip_live
            && !self.skip_vod
            && !self.skip_series
            && self.xtream_live_stream_use_prefix
            && !self.xtream_live_stream_without_extension
            && !self.disable_hls_streaming
            && !self.resolve_tmdb
            && self.resolve_background
            && !self.resolve_series
            && !self.resolve_vod
            && !self.probe_series
            && !self.probe_vod
            && is_default_resolve_delay_secs(&self.resolve_delay)
            && is_default_probe_delay_secs(&self.probe_delay)
            && !self.probe_live
            && is_default_probe_live_interval(&self.probe_live_interval_hours)
            && self.resolve_filter.as_ref().is_none_or(|s| s.trim().is_empty())
            && self.probe_filter.as_ref().is_none_or(|s| s.trim().is_empty())
            && !self.stalker_bulk_epg
    }

    pub fn clean(&mut self) {
        self.skip_live = false;
        self.skip_vod = false;
        self.skip_series = false;
        self.xtream_live_stream_use_prefix = default_as_true();
        self.xtream_live_stream_without_extension = false;
        self.disable_hls_streaming = false;
        self.resolve_tmdb = false;
        self.resolve_background = default_as_true();
        self.resolve_series = false;
        self.resolve_vod = false;
        self.probe_series = false;
        self.probe_vod = false;
        self.resolve_delay = default_resolve_delay_secs();
        self.probe_delay = default_probe_delay_secs();
        self.probe_live = false;
        self.probe_live_interval_hours = default_probe_live_interval();
        self.resolve_filter = None;
        self.t_resolve_filter = None;
        self.probe_filter = None;
        self.t_probe_filter = None;
        self.stalker_bulk_epg = false;
    }

    pub fn prepare(&mut self, templates: Option<&[PatternTemplate]>) -> Result<(), TuliproxError> {
        if let Some(raw_filter) = &self.resolve_filter {
            self.t_resolve_filter = Some(get_filter(raw_filter, templates)?);
        }
        if let Some(raw_filter) = &self.probe_filter {
            self.t_probe_filter = Some(get_filter(raw_filter, templates)?);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputAliasDto {
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub id: u16,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    pub url: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_i16")]
    pub priority: i16,
    #[serde(default)]
    pub max_connections: u16,
    #[serde(default, deserialize_with = "deserialize_timestamp", skip_serializing_if = "Option::is_none")]
    pub exp_date: Option<i64>,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalker: Option<StalkerInputConfigDto>,
}

impl ConfigInputAliasDto {
    pub fn prepare(&mut self, index: u16, input_type: &InputType) -> Result<u16, TuliproxError> {
        self.id = index + 1;
        self.name = self.name.trim().intern();
        if self.name.is_empty() {
            return Err(TuliproxError::ConfigInput("name for input is mandatory".to_string()));
        }
        self.url = self.url.trim().to_string();
        if self.url.is_empty() {
            return Err(TuliproxError::ConfigInput(format!("url for input is mandatory (input: {})", self.name)));
        }
        check_input_credentials!(self, input_type, true, true);
        check_input_connections!(self, input_type, true);
        prepare_stalker_config(
            &self.name,
            input_type,
            &mut self.stalker,
            true,
            self.username.as_deref(),
            self.password.as_deref(),
        )?;

        Ok(self.id)
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none", serialize_with = "serialize_option_vec_flow_map_items")]
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

/// Normalize a user-supplied MAC address into the canonical lowercase
/// `xx:xx:xx:xx:xx:xx` form. Accepts colon-, dash- and bare-hex formats
/// (all three are common in portal provisioning exports). Returns `None`
/// when the value is not a valid 6-octet MAC.
fn normalize_mac_address(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let octets: Vec<String> = if trimmed.contains(':') || trimmed.contains('-') {
        trimmed.split(['-', ':']).map(str::to_string).collect()
    } else if trimmed.len() == 12 {
        trimmed.as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).to_string()).collect()
    } else {
        return None;
    };
    let valid = octets.len() == 6 && octets.iter().all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()));
    if !valid {
        return None;
    }
    Some(octets.join(":").to_ascii_lowercase())
}

fn prepare_stalker_config(
    input_name: &str,
    input_type: &InputType,
    stalker: &mut Option<StalkerInputConfigDto>,
    is_alias: bool,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<(), TuliproxError> {
    if !input_type.is_stalker() {
        if stalker.is_some() {
            return Err(TuliproxError::ConfigInput(format!(
                "stalker configuration is only valid for stalker inputs (input: {input_name})"
            )));
        }
        return Ok(());
    }

    // Aliases inherit the parent's stalker block when they define none of
    // their own — never materialize a default here, otherwise the inherited
    // config would be shadowed by an empty one in `as_input`.
    let config = if is_alias {
        match stalker.as_mut() {
            Some(config) => config,
            None => return Ok(()),
        }
    } else {
        stalker.get_or_insert_with(StalkerInputConfigDto::default)
    };

    let mut has_mac = false;
    // Only normalize an existing device block — do not materialize an empty
    // one (it would be re-serialized as a noise `device: {}` block).
    if let Some(device) = config.device.as_mut() {
        // MAC validation — accept colon-, dash- and bare-hex formats and
        // normalize to lowercase `xx:xx:xx:xx:xx:xx`. Empty MAC is allowed
        // at this point (auth mode might be credentials-only).
        if let Some(mac) = device.mac_address.as_ref() {
            let trimmed = mac.trim();
            if !trimmed.is_empty() {
                let Some(normalized) = normalize_mac_address(trimmed) else {
                    return Err(TuliproxError::ConfigInput(format!(
                        "stalker.device.mac_address must be a MAC address in XX:XX:XX:XX:XX:XX, XX-XX-XX-XX-XX-XX or bare-hex format (input: {input_name})"
                    )));
                };
                let first_octet = u8::from_str_radix(&normalized[..2], 16).unwrap_or_default();
                if first_octet & 1 != 0 {
                    return Err(TuliproxError::ConfigInput(format!(
                        "stalker.device.mac_address must be a unicast MAC address (input: {input_name})"
                    )));
                }
                device.mac_address = Some(normalized);
                has_mac = true;
            }
        }

        // Trim locale/timezone if the user supplied them.
        if let Some(timezone) = device.timezone.as_mut() {
            *timezone = timezone.trim().to_string();
            if timezone.is_empty() {
                device.timezone = None;
            }
        }
        if let Some(locale) = device.locale.as_mut() {
            *locale = locale.trim().to_string();
            if locale.is_empty() {
                device.locale = None;
            }
        }
        if let Some(profile) = device.device_profile.as_mut() {
            *profile = profile.trim().to_string();
            if profile.is_empty() {
                device.device_profile = None;
            }
        }
    }

    // Aliases may inherit the parent's identity, so only main inputs are checked.
    if !is_alias && !input_type.is_batch() {
        let has_credentials = username.is_some_and(|value| !value.trim().is_empty())
            && password.is_some_and(|value| !value.trim().is_empty());
        let requirement = match config.auth_mode {
            StalkerAuthMode::Auto if !has_mac && !has_credentials => {
                Some("stalker.device.mac_address or username/password")
            }
            StalkerAuthMode::MacOnly if !has_mac => Some("stalker.device.mac_address for auth_mode mac_only"),
            StalkerAuthMode::CredentialsOnly if !has_credentials => {
                Some("username and password for auth_mode credentials_only")
            }
            StalkerAuthMode::MacPlusCredentials if !has_mac || !has_credentials => {
                Some("stalker.device.mac_address, username and password for auth_mode mac_plus_credentials")
            }
            _ => None,
        };
        if let Some(requirement) = requirement {
            return Err(TuliproxError::ConfigInput(format!("Stalker input '{input_name}' requires {requirement}")));
        }
    }

    Ok(())
}

impl ConfigInputDto {
    fn normalize_input_type_from_batch_url(&mut self) {
        let is_batch_url = self.url.trim().starts_with(BATCH_SCHEME_PREFIX);
        self.input_type = match self.input_type {
            InputType::M3u | InputType::M3uBatch => {
                if is_batch_url {
                    InputType::M3uBatch
                } else {
                    InputType::M3u
                }
            }
            InputType::Xtream | InputType::XtreamBatch => {
                if is_batch_url {
                    InputType::XtreamBatch
                } else {
                    InputType::Xtream
                }
            }
            InputType::Stalker | InputType::StalkerBatch => {
                if is_batch_url {
                    InputType::StalkerBatch
                } else {
                    InputType::Stalker
                }
            }
            InputType::Library => InputType::Library,
            InputType::Emby => InputType::Emby,
            InputType::Jellyfin => InputType::Jellyfin,
            InputType::Plex => InputType::Plex,
            InputType::Staged => InputType::Staged,
        };
    }

    fn prepare_media_server_input(&mut self) -> Result<(), TuliproxError> {
        if !self.input_type.is_media_server() {
            return Ok(());
        }

        let trimmed_url = self.url.trim();
        if trimmed_url.starts_with(BATCH_SCHEME_PREFIX) || trimmed_url.starts_with(PROVIDER_SCHEME_PREFIX) {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input does not support batch:// or provider:// URLs (input: {})",
                self.name
            )));
        }
        if self.aliases.as_ref().is_some_and(|aliases| !aliases.is_empty()) {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input does not support aliases (input: {})",
                self.name
            )));
        }
        if self.epg.is_some() {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input does not support EPG configuration (input: {})",
                self.name
            )));
        }
        if self.panel_api.is_some() {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input does not support panel_api configuration (input: {})",
                self.name
            )));
        }
        if self.provider.as_ref().is_some_and(|provider| !provider.is_empty()) {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input does not support provider failover definitions (input: {})",
                self.name
            )));
        }
        let Some(media_server) = self.media_server.as_mut() else {
            return Err(TuliproxError::ConfigInput(format!(
                "media_server configuration is mandatory for input type {} (input: {})",
                self.input_type, self.name
            )));
        };
        media_server.prepare(&self.name)?;
        if media_server.libraries.is_empty() {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input requires at least one selected library (input: {})",
                self.name
            )));
        }

        match self.input_type {
            InputType::Emby | InputType::Jellyfin => {
                if trimmed_url.is_empty() {
                    return Err(TuliproxError::ConfigInput(format!(
                        "url is mandatory for input type {} (input: {})",
                        self.input_type, self.name
                    )));
                }
                let has_login = self.username.as_ref().is_some_and(|u| !u.trim().is_empty())
                    && self.password.as_ref().is_some_and(|p| !p.trim().is_empty());
                if !media_server.has_any_emby_jellyfin_auth() && !has_login {
                    return Err(TuliproxError::ConfigInput(format!(
                        "media-server input type {} requires media_server token/api_key or username/password bootstrap credentials (input: {})",
                        self.input_type, self.name
                    )));
                }
            }
            InputType::Plex => {
                if trimmed_url.is_empty() {
                    if !is_non_blank_optional_string(&media_server.account_token) {
                        return Err(TuliproxError::ConfigInput(format!(
                            "media-server input type plex without input.url requires media_server.account_token for MyPlex discovery (input: {})",
                            self.name
                        )));
                    }
                    if !media_server.has_plex_server_selector() {
                        return Err(TuliproxError::ConfigInput(format!(
                            "media-server input type plex requires a server selector such as media_server.server_id or media_server.server_name when input.url is omitted (input: {})",
                            self.name
                        )));
                    }
                } else if !is_non_blank_optional_string(&media_server.token) {
                    return Err(TuliproxError::ConfigInput(format!(
                        "media-server input type plex with input.url requires media_server.token for direct PMS access (input: {})",
                        self.name
                    )));
                }
            }
            InputType::M3u
            | InputType::Xtream
            | InputType::M3uBatch
            | InputType::XtreamBatch
            | InputType::Stalker
            | InputType::StalkerBatch
            | InputType::Library
            | InputType::Staged => {}
        }

        Ok(())
    }

    fn validate_staged_self(&mut self) -> Result<(), TuliproxError> {
        if self.input_type.is_staged() {
            let Some(staged) = self.staged.as_mut() else {
                return Err(TuliproxError::ConfigInput(format!(
                    "staged input requires a provider input (input: {})",
                    self.name
                )));
            };
            if let Some(for_input) = staged.for_input.as_ref().map(|p| p.trim()).filter(|p| !p.is_empty()) {
                staged.for_input = Some(for_input.intern());
            } else {
                return Err(TuliproxError::ConfigInput(format!(
                    "staged input requires a provider input (input: {})",
                    self.name
                )));
            }
            if staged.clusters.is_empty() {
                return Err(TuliproxError::ConfigInput(format!(
                    "staged input requires at least one staged cluster (input: {})",
                    self.name
                )));
            }
            if self.url.trim().is_empty() {
                return Err(TuliproxError::ConfigInput(format!(
                    "url for staged input is mandatory (input: {})",
                    self.name
                )));
            }
            if self.staged_type == StagedInputType::Xtream {
                let has_credentials = self.username.as_ref().is_some_and(|u| !u.trim().is_empty())
                    && self.password.as_ref().is_some_and(|p| !p.trim().is_empty());
                if !has_credentials {
                    return Err(TuliproxError::ConfigInput(format!(
                        "staged xtream input requires username and password (input: {})",
                        self.name
                    )));
                }
            }
            // R7
            if self.media_server.is_some() {
                return Err(TuliproxError::ConfigInput(format!(
                    "staged input does not support media_server configuration (input: {})",
                    self.name
                )));
            }
            if self.panel_api.is_some() {
                return Err(TuliproxError::ConfigInput(format!(
                    "staged input does not support panel_api configuration (input: {})",
                    self.name
                )));
            }
        } else if self.staged.is_some() {
            return Err(TuliproxError::ConfigInput(format!(
                "staged configuration is only allowed for staged inputs (input: {})",
                self.name
            )));
        }
        Ok(())
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn prepare(
        &mut self,
        index: u16,
        _include_computed: bool,
        provider_names: &HashSet<String>,
        templates: Option<&[PatternTemplate]>,
    ) -> Result<u16, TuliproxError> {
        self.name = self.name.trim().intern();
        if self.name.is_empty() {
            return Err(TuliproxError::ConfigInput("name for input is mandatory".to_string()));
        }

        if self.sequential_group == Some(0) {
            return Err(TuliproxError::ConfigInput(format!(
                "sequential_group must be greater than zero (input: {})",
                self.name
            )));
        }
        if self.input_type.is_staged() && self.sequential_group.is_some() {
            return Err(TuliproxError::ConfigInput(format!(
                "staged input does not support sequential_group (input: {})",
                self.name
            )));
        }

        if self.input_type.is_staged() {
            self.priority = 0;
            self.max_connections = 0;
            self.cache_duration = None;
            self.cache_duration_seconds = 0;
        }

        if let Some(duration_str) = &self.cache_duration {
            self.cache_duration_seconds = self.parse_duration(duration_str)?;
        } else {
            self.cache_duration_seconds = 0;
        }

        self.url = self.url.trim().to_string();
        self.normalize_input_type_from_batch_url();
        if let Some(options) = self.options.as_ref() {
            if options.disable_hls_streaming && !self.input_type.is_xtream() {
                return Err(TuliproxError::ConfigInput(format!(
                    "`disable_hls_streaming` is supported only for Xtream inputs (input: {})",
                    self.name
                )));
            }
        }
        if let Some(media_server) = self.media_server.as_mut() {
            media_server.normalize();
        }
        if self.enabled {
            self.prepare_media_server_input()?;
        }
        if self.url.starts_with(PROVIDER_SCHEME_PREFIX) && self.input_type.is_batch() {
            return Err(TuliproxError::ConfigInput(format!(
                "input type {} does not support provider:// URLs for batch definitions; use batch:// URL (input: {})",
                self.input_type, self.name
            )));
        }

        check_input_credentials!(self, self.input_type, true, false);
        check_input_connections!(self, self.input_type, false);
        // Always run stalker validation: it rejects a stray `stalker` block on
        // non-stalker inputs and surfaces a malformed MAC at config load even
        // when the input is currently disabled (consistent with aliases).
        self.prepare_stalker_input()?;
        self.validate_staged_self()?;

        self.persist = get_trimmed_string(self.persist.as_deref());
        check_provider_scheme_url!(self.url, provider_names);

        let mut current_index = index + 1;
        self.id = current_index;
        if let Some(aliases) = self.aliases.as_mut() {
            let input_type = &self.input_type;
            for alias in aliases {
                current_index = alias.prepare(current_index, input_type)?;
                check_provider_scheme_url!(alias.url.as_str(), provider_names);
            }
        }

        if let Some(panel_api) = self.panel_api.as_mut() {
            panel_api.prepare(&self.name)?;
        }

        // Validate provider:// URLs in EPG sources
        if let Some(epg) = self.epg.as_ref() {
            if let Some(sources) = epg.sources.as_ref() {
                for epg_source in sources {
                    let url = epg_source.url.trim();
                    check_provider_scheme_url!(url, provider_names);
                }
            }
        }

        // Prepare filter options
        if let Some(options) = self.options.as_mut() {
            options.prepare(templates)?;
        }

        Ok(current_index)
    }

    fn parse_duration(&self, duration_str: &str) -> Result<u64, TuliproxError> {
        match parse_duration_seconds(duration_str, false) {
            Some(seconds) => Ok(seconds),
            None => Err(TuliproxError::ConfigInput(format!(
                "Invalid cache_duration format in '{}': {}",
                self.name, duration_str
            ))),
        }
    }

    // Neue ausgelagerte Methode für die URL-Generierung
    fn generate_auto_epg_url(&self) -> Result<String, String> {
        let get_creds = || {
            if self.username.is_some() && self.password.is_some() {
                return (self.username.clone(), self.password.clone(), Some(self.url.clone()));
            }

            let (u, p, r) = self
                .aliases
                .as_ref()
                .and_then(|aliases| aliases.iter().find(|a| a.enabled))
                .map(|alias| (alias.username.clone(), alias.password.clone(), Some(alias.url.clone())))
                .unwrap_or((None, None, None));

            if u.is_some() && p.is_some() && r.is_some() {
                return (u, p, r);
            }

            let (u, p) = get_credentials_from_url_str(&self.url);
            if u.is_some() && p.is_some() {
                return (u, p, Some(self.url.clone()));
            }

            self.aliases
                .as_ref()
                .and_then(|aliases| aliases.iter().find(|a| a.enabled))
                .map(|alias| {
                    let (u, p) = get_credentials_from_url_str(alias.url.as_str());
                    (u, p, Some(alias.url.clone()))
                })
                .unwrap_or((None, None, None))
        };

        let (username, password, base_url) = get_creds();

        if username.is_none() || password.is_none() || base_url.is_none() {
            Err(format!("auto_epg is enabled for input {}, but no credentials could be extracted", self.name))
        } else if let Some(base) = base_url {
            let clean_base = base.split('?').next().unwrap_or(&base);

            let provider_epg_url = format!(
                "{}/xmltv.php?username={}&password={}",
                trim_last_slash(clean_base),
                username.unwrap_or_default(),
                password.unwrap_or_default()
            );
            Ok(provider_epg_url)
        } else {
            Err(format!(
                "auto_epg is enabled for input {}, but url could not be parsed {}",
                self.name,
                sanitize_sensitive_info(&self.url)
            ))
        }
    }

    pub fn prepare_epg(&mut self, include_computed: bool) -> Result<(), TuliproxError> {
        if let Some(mut epg) = self.epg.take() {
            if self.input_type == InputType::Library {
                warn!("EPG is not supported for library inputs {}, skipping", self.name);
                self.epg = None;
                return Ok(());
            }

            epg.prepare(|| self.generate_auto_epg_url(), include_computed)?;
            epg.t_sources = {
                let mut seen_sources = HashSet::new();
                epg.t_sources.drain(..).filter(|src| seen_sources.insert(src.source_identity())).collect()
            };
            self.epg = Some(epg);
        }
        Ok(())
    }

    pub fn prepare_batch(
        &mut self,
        batch_aliases: Vec<ConfigInputAliasDto>,
        index: u16,
    ) -> Result<Option<u16>, TuliproxError> {
        let idx = apply_batch_aliases!(self, batch_aliases, Some(index));
        Ok(idx)
    }

    pub fn prepare_type(&mut self) -> Result<(), TuliproxError> {
        self.url = self.url.trim().to_string();
        self.normalize_input_type_from_batch_url();
        if self.url.starts_with(PROVIDER_SCHEME_PREFIX) && self.input_type.is_batch() {
            return Err(TuliproxError::ConfigInput(format!(
                "input type {} does not support provider:// URLs for batch definitions; use batch:// URL",
                self.input_type
            )));
        }
        Ok(())
    }

    pub fn upsert_alias(&mut self, mut alias: ConfigInputAliasDto) -> Result<(), TuliproxError> {
        check_input_credentials!(alias, self.input_type, true, true);
        check_input_connections!(alias, self.input_type, true);
        let aliases = self.aliases.get_or_insert_with(Vec::new);
        if let Some(existing) = aliases.iter_mut().find(|a| a.id == alias.id) {
            *existing = alias;
        } else {
            aliases.push(alias);
        }
        Ok(())
    }

    pub fn update_account_expiration_date(
        &mut self,
        account_name: &str,
        exp_date: i64,
        disable: bool,
    ) -> Result<bool, TuliproxError> {
        let (expiration, enabled) = if self.name.as_ref() == account_name {
            (&mut self.exp_date, &mut self.enabled)
        } else if let Some(alias) = self
            .aliases
            .as_mut()
            .and_then(|aliases| aliases.iter_mut().find(|alias| alias.name.as_ref() == account_name))
        {
            (&mut alias.exp_date, &mut alias.enabled)
        } else {
            return Err(TuliproxError::ConfigInput(format!("No input or alias found for account '{account_name}'")));
        };
        let changed = *expiration != Some(exp_date) || disable && *enabled;
        *expiration = Some(exp_date);
        if disable {
            *enabled = false;
        }
        Ok(changed)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StalkerDeviceProfileDto;

    fn create_test_dto() -> ConfigInputDto {
        ConfigInputDto { name: "test_input".intern(), ..ConfigInputDto::default() }
    }

    fn prepare_dto(dto: &mut ConfigInputDto) -> Result<u16, TuliproxError> {
        dto.prepare(0, false, &HashSet::new(), None)
    }

    #[test]
    fn sequential_group_round_trips_yaml_and_json() {
        let yaml = "name: grouped\ntype: m3u\nurl: http://example.com/list.m3u\nsequential_group: 7\n";
        let dto: ConfigInputDto = serde_saphyr::from_str(yaml).expect("sequential group should deserialize from yaml");
        assert_eq!(dto.sequential_group, Some(7));

        let encoded_yaml = serde_saphyr::to_string(&dto).expect("sequential group should serialize to yaml");
        assert!(encoded_yaml.contains("sequential_group: 7"));

        let encoded_json = serde_json::to_string(&dto).expect("sequential group should serialize to json");
        let decoded_json: ConfigInputDto =
            serde_json::from_str(&encoded_json).expect("sequential group should deserialize from json");
        assert_eq!(decoded_json.sequential_group, Some(7));
    }

    #[test]
    fn sequential_group_rejects_zero_and_staged_inputs() {
        let mut zero = ConfigInputDto {
            name: "zero_group".intern(),
            url: "http://example.com/list.m3u".to_string(),
            sequential_group: Some(0),
            ..ConfigInputDto::default()
        };
        let err = prepare_dto(&mut zero).expect_err("group zero must be rejected");
        assert!(err.to_string().contains("sequential_group"), "Error: {err}");

        let mut staged = ConfigInputDto {
            name: "staged_group".intern(),
            input_type: InputType::Staged,
            url: "http://example.com/list.m3u".to_string(),
            staged: Some(ConfigInputStagedDto {
                for_input: Some("provider_a".intern()),
                ..ConfigInputStagedDto::default()
            }),
            sequential_group: Some(1),
            ..ConfigInputDto::default()
        };
        let err = prepare_dto(&mut staged).expect_err("staged groups must be rejected");
        assert!(err.to_string().contains("sequential_group"), "Error: {err}");
    }

    #[test]
    fn stalker_options_reject_retired_playback_resolution_switches() -> Result<(), serde_json::Error> {
        let dto: ConfigInputOptionsDto = serde_json::from_str(r#"{"stalker_bulk_epg":true}"#)?;
        assert!(dto.stalker_bulk_epg);
        assert!(serde_json::from_str::<ConfigInputOptionsDto>(r#"{"stalker_pre_resolve_playback":true}"#).is_err());
        assert!(serde_json::from_str::<ConfigInputOptionsDto>(r#"{"stalker_runtime_resolve_playback":false}"#).is_err());
        Ok(())
    }

    #[test]
    fn test_epg_url_from_explicit_main_credentials() {
        let mut dto = create_test_dto();
        // Hier testen wir auch gleich mit, ob der Trailing Slash sauber entfernt wird!
        dto.url = "http://myprovider.com/".to_string();
        dto.username = Some("hello".to_string());
        dto.password = Some("mello".to_string());

        let result = dto.generate_auto_epg_url().unwrap();
        assert_eq!(result, "http://myprovider.com/xmltv.php?username=hello&password=mello");
    }

    #[test]
    fn test_epg_url_from_enabled_alias_explicit_credentials() {
        let mut dto = create_test_dto();
        dto.url = "http://main.com".to_string();

        let alias = ConfigInputAliasDto {
            enabled: true,
            url: "http://alias.com".to_string(),
            username: Some("alias_user".to_string()),
            password: Some("alias_pass".to_string()),
            ..ConfigInputAliasDto::default()
        };

        dto.aliases = Some(vec![alias]);

        let result = dto.generate_auto_epg_url().unwrap();
        // Er muss die URL und die Credentials vom Alias nehmen
        assert_eq!(result, "http://alias.com/xmltv.php?username=alias_user&password=alias_pass");
    }

    #[test]
    fn test_epg_url_skips_disabled_aliases() {
        let mut dto = create_test_dto();

        let alias = ConfigInputAliasDto {
            enabled: false, // Alias ist deaktiviert!
            url: "http://alias.com".to_string(),
            username: Some("alias_user".to_string()),
            password: Some("alias_pass".to_string()),
            ..ConfigInputAliasDto::default()
        };

        dto.aliases = Some(vec![alias]);

        let result = dto.generate_auto_epg_url();
        // Since the main DTO is empty and alias is disabled, an error must occur
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no credentials could be extracted"));
    }

    #[test]
    fn test_epg_url_fails_without_credentials() {
        let mut dto = create_test_dto();
        dto.url = "http://nocreds.com".to_string();

        let result = dto.generate_auto_epg_url();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no credentials could be extracted"));
    }

    #[test]
    fn test_epg_url_from_main_url_query_credentials() {
        let mut dto = create_test_dto();
        // Credentials stecken als Query-Parameter in der URL
        dto.url = "http://myprovider.com?username=hello&password=mello".to_string();

        let result = dto.generate_auto_epg_url().unwrap();

        // Durch unseren sauberen "clean_base" Fix sieht die URL jetzt richtig aus!
        assert_eq!(result, "http://myprovider.com/xmltv.php?username=hello&password=mello");
    }

    #[test]
    fn test_epg_url_from_alias_url_query_credentials() {
        let mut dto = create_test_dto();
        dto.url = "http://main.com".to_string();

        let alias = ConfigInputAliasDto {
            enabled: true,
            // Credentials im Alias als Query-Parameter
            url: "http://alias.com?username=alias_user&password=alias_pass".to_string(),
            ..ConfigInputAliasDto::default()
        };

        dto.aliases = Some(vec![alias]);

        let result = dto.generate_auto_epg_url().unwrap();
        assert_eq!(result, "http://alias.com/xmltv.php?username=alias_user&password=alias_pass");
    }

    #[test]
    fn test_epg_url_from_provider_scheme_url_query_credentials() {
        let mut dto = create_test_dto();
        dto.url = "provider://myprovider".to_string();
        dto.username = Some("test".to_string());
        dto.password = Some("secret".to_string());

        let result = dto.generate_auto_epg_url().unwrap();
        assert_eq!(result, "provider://myprovider/xmltv.php?username=test&password=secret");
    }

    #[test]
    fn test_provider_dns_defaults() {
        let dns = ProviderDnsDto::default();
        assert!(!dns.enabled);
        assert_eq!(dns.refresh_secs, 300);
        assert_eq!(dns.prefer, DnsPrefer::System);
        assert_eq!(dns.on_resolve_error, OnResolveErrorPolicy::KeepLastGood);
        assert_eq!(dns.on_connect_error, OnConnectErrorPolicy::TryNextIp);
        assert!(dns.schemes.is_none());
    }

    #[test]
    fn test_provider_url_selection_policy_defaults_to_resume_last_working() {
        let provider = ConfigProviderDto {
            name: "provider-a".intern(),
            urls: vec!["http://primary.example.com".intern()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::default(),
            dns: None,
        };

        assert_eq!(provider.provider_url_selection_policy, ProviderUrlSelectionPolicy::ResumeLastWorking);
    }

    #[test]
    fn test_provider_url_selection_policy_can_be_set_to_restart_from_first() {
        let provider = ConfigProviderDto {
            name: "provider-a".intern(),
            urls: vec!["http://primary.example.com".intern()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
            dns: None,
        };

        assert_eq!(provider.provider_url_selection_policy, ProviderUrlSelectionPolicy::RestartFromFirst);
    }

    #[test]
    fn test_provider_url_selection_policy_deserializes_default_when_omitted() {
        let provider: ConfigProviderDto =
            serde_json::from_str(r#"{"name":"provider-a","urls":["http://primary.example.com"]}"#)
                .expect("provider dto should deserialize");

        assert_eq!(provider.provider_url_selection_policy, ProviderUrlSelectionPolicy::ResumeLastWorking);
    }

    #[test]
    fn test_provider_url_selection_policy_deserializes_restart_from_first() {
        let provider: ConfigProviderDto = serde_json::from_str(
            r#"{"name":"provider-a","urls":["http://primary.example.com"],"provider_url_selection_policy":"restart_from_first"}"#,
        )
            .expect("provider dto should deserialize");

        assert_eq!(provider.provider_url_selection_policy, ProviderUrlSelectionPolicy::RestartFromFirst);
    }

    #[test]
    fn test_provider_url_selection_policy_default_is_omitted_on_serialize() {
        let provider = ConfigProviderDto {
            name: "provider-a".intern(),
            urls: vec!["http://primary.example.com".intern()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        };

        let json = serde_json::to_string(&provider).expect("provider dto should serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("serialized provider should be valid json");

        assert!(value.get("provider_url_selection_policy").is_none());
    }

    #[test]
    fn test_provider_dns_prepare_normalizes_overrides_and_clamps_refresh() {
        let mut dns = ProviderDnsDto {
            refresh_secs: 1,
            schemes: Some(vec![DnsScheme::Http, DnsScheme::Http, DnsScheme::Https]),
            overrides: Some(HashMap::from([(
                "  EXAMPLE.COM ".to_string(),
                vec![
                    "203.0.113.10".parse::<IpAddr>().expect("valid ip"),
                    "203.0.113.10".parse::<IpAddr>().expect("valid ip"),
                ],
            )])),
            ..ProviderDnsDto::default()
        };

        dns.prepare().expect("dns prepare should succeed");

        assert_eq!(dns.refresh_secs, 10);
        assert_eq!(dns.schemes, Some(vec![DnsScheme::Http, DnsScheme::Https]));
        let overrides = dns.overrides.expect("overrides should exist");
        assert_eq!(overrides.len(), 1);
        assert!(overrides.contains_key("example.com"));
        assert_eq!(overrides["example.com"].len(), 1);
    }

    #[test]
    fn prepare_switches_xtream_to_xtream_batch_when_alias_exists() {
        let mut dto = ConfigInputDto {
            name: "input_alias".intern(),
            input_type: InputType::Xtream,
            url: "batch:///tmp/input_alias.csv".to_string(),
            aliases: Some(vec![ConfigInputAliasDto {
                id: 1,
                name: "alias_1".intern(),
                url: "http://provider.example/stream".to_string(),
                username: Some("u".to_string()),
                password: Some("p".to_string()),
                enabled: true,
                ..ConfigInputAliasDto::default()
            }]),
            ..ConfigInputDto::default()
        };

        dto.prepare_type().expect("prepare type should succeed");
        dto.prepare(0, true, &HashSet::new(), None)
            .expect("prepare should succeed and infer batch type from batch:// URL");
        assert_eq!(dto.input_type, InputType::XtreamBatch);
    }

    #[test]
    fn prepare_keeps_xtream_type_when_alias_exists_without_batch_url() {
        let mut dto = ConfigInputDto {
            name: "input_alias_http".intern(),
            input_type: InputType::XtreamBatch,
            url: "http://localhost:3001".to_string(),
            username: Some("root_user".to_string()),
            password: Some("root_pass".to_string()),
            aliases: Some(vec![ConfigInputAliasDto {
                id: 1,
                name: "alias_1".intern(),
                url: "http://provider.example/stream".to_string(),
                username: Some("u".to_string()),
                password: Some("p".to_string()),
                enabled: true,
                ..ConfigInputAliasDto::default()
            }]),
            ..ConfigInputDto::default()
        };

        dto.prepare_type().expect("prepare type should normalize non-batch URL to xtream");
        assert_eq!(dto.input_type, InputType::Xtream);
        dto.prepare(0, true, &HashSet::new(), None).expect("prepare should succeed for regular URL with aliases");
        assert_eq!(dto.input_type, InputType::Xtream);
    }

    #[test]
    fn prepare_type_does_not_validate_media_server_config() {
        let mut dto = ConfigInputDto {
            name: "emby_media_server".intern(),
            input_type: InputType::Emby,
            url: "https://media.example.invalid".to_string(),
            ..ConfigInputDto::default()
        };

        dto.prepare_type().expect("prepare_type only normalizes type/url");
        let err = prepare_dto(&mut dto).expect_err("full prepare should validate missing media_server block");
        assert!(err.to_string().contains("media_server configuration is mandatory"));
    }

    #[test]
    fn prepare_batch_url_does_not_require_xtream_credentials() {
        let mut dto = ConfigInputDto {
            name: "batch_no_creds".intern(),
            input_type: InputType::Xtream,
            url: "batch:///tmp/no-creds.csv".to_string(),
            username: None,
            password: None,
            ..ConfigInputDto::default()
        };

        dto.prepare(0, true, &HashSet::new(), None)
            .expect("batch:// input must be normalized before credential validation");
        assert_eq!(dto.input_type, InputType::XtreamBatch);
    }

    #[test]
    fn prepare_provider_scheme_url_is_not_treated_as_batch_input() {
        let mut dto = ConfigInputDto {
            name: "batch_provider".intern(),
            input_type: InputType::XtreamBatch,
            url: "provider://myprovider".to_string(),
            username: Some("root_user".to_string()),
            password: Some("root_pass".to_string()),
            aliases: Some(vec![ConfigInputAliasDto {
                id: 1,
                name: "alias_1".intern(),
                url: "http://provider.example/stream".to_string(),
                username: Some("u".to_string()),
                password: Some("p".to_string()),
                enabled: true,
                ..ConfigInputAliasDto::default()
            }]),
            ..ConfigInputDto::default()
        };

        let err = dto
            .prepare(0, true, &HashSet::new(), None)
            .expect_err("prepare should treat provider:// URL as regular input (non-batch) and validate provider");
        assert!(err.to_string().contains("Provider name myprovider is not defined"), "Error: {err}");
    }

    #[test]
    fn disable_hls_streaming_rejects_non_xtream_input() -> Result<(), String> {
        let mut input = ConfigInputDto {
            name: "hls-only".intern(),
            input_type: InputType::M3u,
            url: "http://provider.example/list.m3u".to_string(),
            options: Some(ConfigInputOptionsDto { disable_hls_streaming: true, ..ConfigInputOptionsDto::default() }),
            ..ConfigInputDto::default()
        };

        let Err(error) = prepare_dto(&mut input) else {
            return Err("non-Xtream input accepted disable_hls_streaming".to_string());
        };
        let message = error.to_string();

        assert!(message.contains("disable_hls_streaming"), "Error: {message}");
        assert!(message.contains("Xtream"), "Error: {message}");
        Ok(())
    }

    #[test]
    fn disable_hls_streaming_allows_extensionless_live_streams() -> Result<(), String> {
        let mut input = ConfigInputDto {
            name: "xtream".intern(),
            input_type: InputType::Xtream,
            url: "http://provider.example".to_string(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            options: Some(ConfigInputOptionsDto {
                disable_hls_streaming: true,
                xtream_live_stream_without_extension: true,
                ..ConfigInputOptionsDto::default()
            }),
            ..ConfigInputDto::default()
        };

        prepare_dto(&mut input).map(|_| ()).map_err(|error| error.to_string())
    }

    #[test]
    fn prepare_rejects_missing_input_url_even_with_aliases() {
        let mut dto = ConfigInputDto {
            name: "xtream_missing_root_url".intern(),
            input_type: InputType::Xtream,
            url: "".to_string(),
            username: Some("root_user".to_string()),
            password: Some("root_pass".to_string()),
            aliases: Some(vec![ConfigInputAliasDto {
                id: 1,
                name: "alias_1".intern(),
                url: "http://alias.example".to_string(),
                username: Some("alias_user".to_string()),
                password: Some("alias_pass".to_string()),
                enabled: true,
                ..ConfigInputAliasDto::default()
            }]),
            ..ConfigInputDto::default()
        };

        let err = dto
            .prepare(0, true, &HashSet::new(), None)
            .expect_err("prepare must require root input url even when aliases are present");
        assert!(err.to_string().contains("url for input is mandatory"), "Error: {err}");
        assert!(err.to_string().contains("xtream_missing_root_url"), "Error: {err}");
    }

    #[test]
    fn prepare_rejects_missing_root_credentials_for_non_batch_url_even_with_aliases() {
        let mut dto = ConfigInputDto {
            name: "xtream_batch_missing_root_creds".intern(),
            input_type: InputType::XtreamBatch,
            url: "http://root.example".to_string(),
            aliases: Some(vec![ConfigInputAliasDto {
                id: 1,
                name: "alias_1".intern(),
                url: "http://alias.example".to_string(),
                username: Some("alias_user".to_string()),
                password: Some("alias_pass".to_string()),
                enabled: true,
                ..ConfigInputAliasDto::default()
            }]),
            ..ConfigInputDto::default()
        };

        let err = dto
            .prepare(0, true, &HashSet::new(), None)
            .expect_err("prepare must require root credentials for non-batch URL");
        assert!(err.to_string().contains("for input type xtream: username and password are mandatory"), "Error: {err}");
        assert!(err.to_string().contains("xtream_batch_missing_root_creds"), "Error: {err}");
    }

    #[test]
    fn prepare_rejects_xtream_batch_batch_url_with_root_credentials_even_with_aliases() {
        let mut dto = ConfigInputDto {
            name: "xtream_batch_with_root_creds".intern(),
            input_type: InputType::XtreamBatch,
            url: "batch:///tmp/aliases.csv".to_string(),
            username: Some("root_user".to_string()),
            password: Some("root_pass".to_string()),
            aliases: Some(vec![ConfigInputAliasDto {
                id: 1,
                name: "alias_1".intern(),
                url: "http://alias.example".to_string(),
                username: Some("alias_user".to_string()),
                password: Some("alias_pass".to_string()),
                enabled: true,
                ..ConfigInputAliasDto::default()
            }]),
            ..ConfigInputDto::default()
        };

        let err = dto
            .prepare(0, true, &HashSet::new(), None)
            .expect_err("prepare must reject root credentials when using batch:// for xtream-batch");
        assert!(err.to_string().contains("with batch:// URL should not define username or password"), "Error: {err}");
        assert!(err.to_string().contains("xtream_batch_with_root_creds"), "Error: {err}");
    }

    #[test]
    fn test_staged_requires_provider() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Staged;
        dto.url = "http://staged.com/playlist.m3u".to_string();

        let err = prepare_dto(&mut dto).expect_err("staged input without provider must be rejected");
        assert!(err.to_string().contains("requires a provider input"), "Error: {err}");
    }

    #[test]
    fn test_staged_requires_url() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Staged;
        dto.staged =
            Some(ConfigInputStagedDto { for_input: Some("provider_a".intern()), ..ConfigInputStagedDto::default() });
        dto.url = String::new();

        let err = prepare_dto(&mut dto).expect_err("staged input without url must be rejected");
        assert!(err.to_string().contains("url for staged input is mandatory"), "Error: {err}");
    }

    #[test]
    fn test_staged_requires_non_empty_clusters() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Staged;
        dto.staged =
            Some(ConfigInputStagedDto { for_input: Some("provider_a".intern()), clusters: ClusterFlags::empty() });
        dto.url = "http://staged.com/playlist.m3u".to_string();

        let err = prepare_dto(&mut dto).expect_err("staged input without clusters must be rejected");
        assert!(err.to_string().contains("requires at least one staged cluster"), "Error: {err}");
    }

    #[test]
    fn test_staged_config_only_allowed_for_staged() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::M3u;
        dto.url = "http://main.com/playlist.m3u".to_string();
        dto.staged =
            Some(ConfigInputStagedDto { for_input: Some("provider_a".intern()), ..ConfigInputStagedDto::default() });

        let err = prepare_dto(&mut dto).expect_err("non-staged input with staged config must be rejected");
        assert!(err.to_string().contains("staged configuration is only allowed for staged inputs"), "Error: {err}");
    }

    #[test]
    fn test_staged_rejects_media_server() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Staged;
        dto.staged =
            Some(ConfigInputStagedDto { for_input: Some("provider_a".intern()), ..ConfigInputStagedDto::default() });
        dto.url = "http://staged.com/playlist.m3u".to_string();
        dto.media_server = Some(MediaServerInputConfigDto::default());

        let err = prepare_dto(&mut dto).expect_err("staged input with media_server must be rejected");
        assert!(err.to_string().contains("does not support media_server configuration"), "Error: {err}");
    }

    #[test]
    fn test_staged_valid() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Staged;
        dto.staged =
            Some(ConfigInputStagedDto { for_input: Some(" provider_a ".intern()), ..ConfigInputStagedDto::default() });
        dto.url = "http://staged.com/playlist.m3u".to_string();

        prepare_dto(&mut dto).expect("valid staged input should prepare successfully");
        assert!(dto.input_type.is_staged());
        assert_eq!(dto.staged.as_ref().and_then(|staged| staged.for_input.as_deref()), Some("provider_a"));
    }

    #[test]
    fn test_staged_provider_alias_deserializes_as_for_input() {
        let staged: ConfigInputStagedDto =
            serde_json::from_str(r#"{"provider":"provider_a"}"#).expect("legacy provider field should deserialize");

        assert_eq!(staged.for_input.as_deref(), Some("provider_a"));
    }

    #[test]
    fn test_staged_ignores_stream_runtime_fields() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Staged;
        dto.url = "http://staged.com/playlist.m3u".to_string();
        dto.staged =
            Some(ConfigInputStagedDto { for_input: Some("provider_a".intern()), ..ConfigInputStagedDto::default() });
        dto.priority = -10;
        dto.max_connections = 2;
        dto.cache_duration = Some("not-a-duration".to_string());

        prepare_dto(&mut dto).expect("staged stream runtime fields should be ignored");

        assert_eq!(dto.priority, 0);
        assert_eq!(dto.max_connections, 0);
        assert_eq!(dto.cache_duration, None);
        assert_eq!(dto.cache_duration_seconds, 0);
    }

    #[test]
    fn test_config_input_options_dto_filter_prepare_parses_valid_filter() {
        let mut dto = ConfigInputOptionsDto {
            resolve_filter: Some(r#"name ~ "test""#.to_string()),
            ..ConfigInputOptionsDto::default()
        };
        dto.prepare(None).expect("valid filter should parse");
        assert!(dto.t_resolve_filter.is_some());
    }

    #[test]
    fn test_config_input_options_dto_filter_prepare_rejects_invalid_filter() {
        let mut dto = ConfigInputOptionsDto {
            resolve_filter: Some(r#"name ~ "["#.to_string()), // invalid regex
            ..ConfigInputOptionsDto::default()
        };
        let result = dto.prepare(None);
        assert!(result.is_err());
    }

    #[test]
    fn test_config_input_options_dto_filter_prepare_with_unknown_template_placeholder() {
        let mut dto = ConfigInputOptionsDto {
            resolve_filter: Some(r#"name ~ "!UNKNOWN!""#.to_string()),
            ..ConfigInputOptionsDto::default()
        };
        let result = dto.prepare(None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown template placeholder"));
    }

    #[test]
    fn test_config_input_options_dto_filter_none_prepares_successfully() {
        let mut dto = ConfigInputOptionsDto { resolve_filter: None, ..ConfigInputOptionsDto::default() };
        dto.prepare(None).expect("None filter should prepare successfully");
        assert!(dto.t_resolve_filter.is_none());
    }

    #[test]
    fn config_input_options_deserializes_legacy_prefixed_skip_aliases() {
        let dto: ConfigInputOptionsDto = serde_json::from_str(
            r#"{
                "xtream_skip_live": true,
                "stalker_skip_vod": true,
                "skip_series": true
            }"#,
        )
        .expect("legacy aliases should deserialize");

        assert!(dto.skip_live);
        assert!(dto.skip_vod);
        assert!(dto.skip_series);
    }

    #[test]
    fn config_input_options_serializes_shared_skip_names() {
        let dto = ConfigInputOptionsDto {
            skip_live: true,
            skip_vod: true,
            skip_series: false,
            ..ConfigInputOptionsDto::default()
        };

        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&dto).expect("config input options should serialize"))
                .expect("serialized json should parse");

        assert_eq!(value.get("skip_live"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(value.get("skip_vod"), Some(&serde_json::Value::Bool(true)));
        assert!(value.get("xtream_skip_live").is_none());
        assert!(value.get("stalker_skip_vod").is_none());
    }

    #[test]
    fn disable_hls_streaming_defaults_to_false() -> Result<(), serde_json::Error> {
        let options: ConfigInputOptionsDto = serde_json::from_str("{}")?;
        assert!(!options.disable_hls_streaming);
        Ok(())
    }

    #[test]
    fn disable_hls_streaming_keeps_options_non_empty_and_clean_resets_it() {
        let mut options = ConfigInputOptionsDto { disable_hls_streaming: true, ..ConfigInputOptionsDto::default() };

        assert!(!options.is_empty());
        options.clean();
        assert!(!options.disable_hls_streaming);
        assert!(options.is_empty());
    }

    #[test]
    fn disable_hls_streaming_round_trips() -> Result<(), serde_json::Error> {
        let options = ConfigInputOptionsDto { disable_hls_streaming: true, ..ConfigInputOptionsDto::default() };
        let json = serde_json::to_string(&options)?;
        let restored: ConfigInputOptionsDto = serde_json::from_str(&json)?;

        assert!(restored.disable_hls_streaming);
        Ok(())
    }

    #[test]
    fn new_stalker_input_has_configuration_block() {
        let input = ConfigInputDto::new_with_type(InputType::Stalker);

        assert!(input.stalker.is_some());
        assert!(ConfigInputDto::new_with_type(InputType::M3u).stalker.is_none());
    }

    #[test]
    fn stalker_mac_is_normalized_and_multicast_is_rejected() {
        let mut input = ConfigInputDto::new_with_type(InputType::Stalker);
        input.name = "portal".into();
        input.stalker.as_mut().expect("stalker config").device =
            Some(StalkerDeviceProfileDto { mac_address: Some("00:1A:79:AA:BB:CC".to_string()), ..Default::default() });
        input.prepare_stalker_input().expect("unicast MAC should be valid");
        assert_eq!(
            input
                .stalker
                .as_ref()
                .and_then(|config| config.device.as_ref())
                .and_then(|device| device.mac_address.as_deref()),
            Some("00:1a:79:aa:bb:cc")
        );

        input.stalker.as_mut().expect("stalker config").device.as_mut().expect("device").mac_address =
            Some("01:00:00:00:00:00".to_string());
        assert!(input.prepare_stalker_input().is_err());
    }

    #[test]
    fn stalker_main_input_requires_identity_but_batch_aliases_may_inherit() {
        let mut input = ConfigInputDto::new_with_type(InputType::Stalker);
        input.name = "portal".into();
        assert!(input.prepare_stalker_input().is_err());

        let mut alias = ConfigInputAliasDto {
            name: "alias".into(),
            url: "http://portal.example/c/".to_string(),
            ..Default::default()
        };
        assert!(alias.prepare(0, &InputType::Stalker).is_ok());
    }

    #[test]
    fn stalker_auth_mode_requires_matching_identity() {
        for (auth_mode, has_mac, has_credentials, valid) in [
            (StalkerAuthMode::Auto, true, false, true),
            (StalkerAuthMode::Auto, false, true, true),
            (StalkerAuthMode::MacOnly, true, false, true),
            (StalkerAuthMode::MacOnly, false, true, false),
            (StalkerAuthMode::CredentialsOnly, false, true, true),
            (StalkerAuthMode::CredentialsOnly, true, false, false),
            (StalkerAuthMode::MacPlusCredentials, true, true, true),
            (StalkerAuthMode::MacPlusCredentials, true, false, false),
        ] {
            let mut input = ConfigInputDto::new_with_type(InputType::Stalker);
            input.name = "portal".into();
            input.username = has_credentials.then(|| "user".to_string());
            input.password = has_credentials.then(|| "password".to_string());
            input.stalker = Some(StalkerInputConfigDto {
                auth_mode,
                device: has_mac.then(|| StalkerDeviceProfileDto {
                    mac_address: Some("00:1a:79:aa:bb:cc".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            });

            assert_eq!(input.prepare_stalker_input().is_ok(), valid, "auth mode: {auth_mode}");
        }
    }

    #[test]
    fn non_stalker_input_rejects_stalker_configuration() {
        let mut input = ConfigInputDto {
            name: "m3u".into(),
            stalker: Some(StalkerInputConfigDto::default()),
            ..Default::default()
        };
        assert!(input.prepare_stalker_input().is_err());
    }

    #[test]
    fn stalker_config_round_trips_extended_device_and_size_caps() {
        let mut input = ConfigInputDto::new_with_type(InputType::Stalker);
        input.name = "portal".into();
        input.stalker = Some(StalkerInputConfigDto {
            device: Some(StalkerDeviceProfileDto {
                mac_address: Some("00:1a:79:aa:bb:cc".to_string()),
                device_profile: Some("MAG254".to_string()),
                serial_number: Some("serial-1".to_string()),
                device_id: Some("device-1".to_string()),
                device_id2: Some("device-2".to_string()),
                signature: Some("sig-1".to_string()),
                ..Default::default()
            }),
            size_caps: Some(crate::model::stalker::StalkerActionSizeCapDto {
                create_link_kb: 96,
                ordered_list_mb: 12,
                get_epg_mb: 80,
            }),
            catalog_max_pages: Some(2048),
            ..Default::default()
        });

        let value = serde_json::to_value(&input).expect("serialize stalker input");
        let decoded: ConfigInputDto = serde_json::from_value(value).expect("deserialize stalker input");
        let stalker = decoded.stalker.expect("stalker config");
        let device = stalker.device.expect("device");
        assert_eq!(device.device_profile.as_deref(), Some("MAG254"));
        assert_eq!(device.serial_number.as_deref(), Some("serial-1"));
        assert_eq!(device.device_id.as_deref(), Some("device-1"));
        assert_eq!(device.device_id2.as_deref(), Some("device-2"));
        assert_eq!(device.signature.as_deref(), Some("sig-1"));
        let caps = stalker.size_caps.expect("size caps");
        assert_eq!(caps.create_link_kb, 96);
        assert_eq!(caps.ordered_list_mb, 12);
        assert_eq!(caps.get_epg_mb, 80);
        assert_eq!(stalker.catalog_max_pages, Some(2048));
    }
}
