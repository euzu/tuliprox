use crate::{
    error::TuliproxError,
    utils::{
        default_as_true, default_media_server_catalog_page_size, default_media_server_catalog_request_delay_ms,
        get_trimmed_string, is_blank_optional_string, is_default_media_server_catalog_page_size,
        is_default_media_server_catalog_request_delay_ms, is_false, is_non_blank_optional_string, is_true,
    },
};
use enum_iterator::Sequence;
use std::sync::Arc;

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq, Default)]
pub enum MediaServerCatalogRefreshModeDto {
    #[serde(rename = "manual")]
    #[default]
    Manual,
    #[serde(rename = "scheduled")]
    Scheduled,
}

pub fn is_default_media_server_catalog_refresh_mode(value: &MediaServerCatalogRefreshModeDto) -> bool {
    *value == MediaServerCatalogRefreshModeDto::default()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MediaServerCatalogConfigDto {
    #[serde(default, skip_serializing_if = "is_default_media_server_catalog_refresh_mode")]
    pub refresh_mode: MediaServerCatalogRefreshModeDto,
    #[serde(default, skip_serializing_if = "is_false")]
    pub refresh_on_startup: bool,
    #[serde(
        default = "default_media_server_catalog_page_size",
        skip_serializing_if = "is_default_media_server_catalog_page_size"
    )]
    pub page_size: u16,
    #[serde(
        default = "default_media_server_catalog_request_delay_ms",
        skip_serializing_if = "is_default_media_server_catalog_request_delay_ms"
    )]
    pub request_delay_ms: u64,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub include_media_sources: bool,
    #[serde(default, alias = "include_file_paths", skip_serializing_if = "is_false")]
    pub include_paths: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_user_state: bool,
}

impl Default for MediaServerCatalogConfigDto {
    fn default() -> Self {
        Self {
            refresh_mode: MediaServerCatalogRefreshModeDto::default(),
            refresh_on_startup: false,
            page_size: default_media_server_catalog_page_size(),
            request_delay_ms: default_media_server_catalog_request_delay_ms(),
            include_media_sources: default_as_true(),
            include_paths: false,
            include_user_state: false,
        }
    }
}

impl MediaServerCatalogConfigDto {
    pub fn is_default(&self) -> bool { self == &Self::default() }

    pub fn prepare(&self, input_name: &Arc<str>) -> Result<(), TuliproxError> {
        if self.page_size == 0 {
            return Err(TuliproxError::ConfigInput(format!(
                "media server catalog page_size must be greater than zero (input: {input_name})"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq, Default)]
pub enum MediaServerPlaybackInfoPolicyDto {
    #[serde(rename = "on_demand")]
    #[default]
    OnDemand,
    #[serde(rename = "disabled")]
    Disabled,
}

pub fn is_default_media_server_playback_info_policy(value: &MediaServerPlaybackInfoPolicyDto) -> bool {
    *value == MediaServerPlaybackInfoPolicyDto::default()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MediaServerPlaybackConfigDto {
    #[serde(default, skip_serializing_if = "is_default_media_server_playback_info_policy")]
    pub playback_info_policy: MediaServerPlaybackInfoPolicyDto,
    #[serde(default, skip_serializing_if = "is_false")]
    pub preflight_streams: bool,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub direct_play_only: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_transcode: bool,
}

impl Default for MediaServerPlaybackConfigDto {
    fn default() -> Self {
        Self {
            playback_info_policy: MediaServerPlaybackInfoPolicyDto::default(),
            preflight_streams: false,
            direct_play_only: default_as_true(),
            allow_transcode: false,
        }
    }
}

impl MediaServerPlaybackConfigDto {
    pub fn is_default(&self) -> bool { self == &Self::default() }
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq, Default)]
pub enum MediaServerImagePolicyDto {
    #[serde(rename = "proxy_on_demand")]
    #[default]
    ProxyOnDemand,
    #[serde(rename = "disabled")]
    Disabled,
}

pub fn is_default_media_server_image_policy(value: &MediaServerImagePolicyDto) -> bool {
    *value == MediaServerImagePolicyDto::default()
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq)]
pub enum MediaServerLibraryKindDto {
    #[serde(rename = "movies")]
    Movies,
    #[serde(rename = "tvshows", alias = "shows", alias = "series")]
    TvShows,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MediaServerLibrarySelectorDetailsDto {
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MediaServerLibraryKindDto>,
}

impl MediaServerLibrarySelectorDetailsDto {
    fn prepare(&mut self) {
        self.id = get_trimmed_string(self.id.as_deref());
        self.key = get_trimmed_string(self.key.as_deref());
        self.name = get_trimmed_string(self.name.as_deref());
    }

    fn is_empty(&self) -> bool {
        self.id.as_ref().is_none_or(|s| s.trim().is_empty())
            && self.key.as_ref().is_none_or(|s| s.trim().is_empty())
            && self.name.as_ref().is_none_or(|s| s.trim().is_empty())
            && self.kind.is_none()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum MediaServerLibrarySelectorDto {
    Name(String),
    Detailed(MediaServerLibrarySelectorDetailsDto),
}

impl MediaServerLibrarySelectorDto {
    fn prepare(&mut self) {
        match self {
            Self::Name(name) => *name = name.trim().to_string(),
            Self::Detailed(details) => details.prepare(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Name(name) => name.trim().is_empty(),
            Self::Detailed(details) => details.is_empty(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MediaServerInputConfigDto {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub libraries: Vec<MediaServerLibrarySelectorDto>,
    #[serde(default, skip_serializing_if = "MediaServerCatalogConfigDto::is_default")]
    pub catalog: MediaServerCatalogConfigDto,
    #[serde(default, skip_serializing_if = "MediaServerPlaybackConfigDto::is_default")]
    pub playback: MediaServerPlaybackConfigDto,
    #[serde(default, skip_serializing_if = "is_default_media_server_image_policy")]
    pub image_policy: MediaServerImagePolicyDto,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub account_token: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub server_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub machine_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub server_name: Option<String>,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub prefer_https: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_relay: bool,
}

impl Default for MediaServerInputConfigDto {
    fn default() -> Self {
        Self {
            libraries: Vec::new(),
            catalog: MediaServerCatalogConfigDto::default(),
            playback: MediaServerPlaybackConfigDto::default(),
            image_policy: MediaServerImagePolicyDto::default(),
            token: None,
            api_key: None,
            user_id: None,
            account_token: None,
            server_id: None,
            machine_id: None,
            server_name: None,
            prefer_https: default_as_true(),
            allow_relay: false,
        }
    }
}

impl MediaServerInputConfigDto {
    pub fn normalize(&mut self) {
        self.token = get_trimmed_string(self.token.as_deref());
        self.api_key = get_trimmed_string(self.api_key.as_deref());
        self.user_id = get_trimmed_string(self.user_id.as_deref());
        self.account_token = get_trimmed_string(self.account_token.as_deref());
        self.server_id = get_trimmed_string(self.server_id.as_deref());
        self.machine_id = get_trimmed_string(self.machine_id.as_deref());
        self.server_name = get_trimmed_string(self.server_name.as_deref());

        for library in &mut self.libraries {
            library.prepare();
        }
    }

    pub fn prepare(&mut self, input_name: &Arc<str>) -> Result<(), TuliproxError> {
        self.normalize();
        self.catalog.prepare(input_name)?;

        if self.libraries.iter().any(MediaServerLibrarySelectorDto::is_empty) {
            return Err(TuliproxError::ConfigInput(format!(
                "media_server library selectors must not be empty (input: {input_name})"
            )));
        }
        Ok(())
    }

    pub fn has_any_emby_jellyfin_auth(&self) -> bool {
        is_non_blank_optional_string(&self.token) || is_non_blank_optional_string(&self.api_key)
    }

    pub fn has_any_plex_token(&self) -> bool {
        is_non_blank_optional_string(&self.account_token) || is_non_blank_optional_string(&self.token)
    }

    pub fn has_plex_server_selector(&self) -> bool {
        is_non_blank_optional_string(&self.server_id)
            || is_non_blank_optional_string(&self.machine_id)
            || is_non_blank_optional_string(&self.server_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{
            ClusterSource, ConfigInputAliasDto, ConfigInputDto, ConfigInputOptionsDto, ConfigProviderDto, DnsPrefer,
            DnsScheme, InputType, OnConnectErrorPolicy, OnResolveErrorPolicy, ProviderDnsDto,
            ProviderUrlSelectionPolicy, StagedInputDto,
        },
        utils::Internable,
    };
    use std::{
        collections::{HashMap, HashSet},
        net::IpAddr,
    };

    fn create_test_dto() -> ConfigInputDto {
        ConfigInputDto { name: "test_input".intern(), ..ConfigInputDto::default() }
    }

    fn prepare_dto(dto: &mut ConfigInputDto) -> Result<u16, TuliproxError> {
        dto.prepare(0, false, &HashSet::new(), None)
    }

    fn media_server_config_with_library() -> MediaServerInputConfigDto {
        MediaServerInputConfigDto {
            libraries: vec![MediaServerLibrarySelectorDto::Name("Movies".to_string())],
            ..MediaServerInputConfigDto::default()
        }
    }

    #[test]
    fn prepare_rejects_blank_media_server_credentials_and_selectors() {
        let mut emby = ConfigInputDto {
            name: "emby_media_server".intern(),
            input_type: InputType::Emby,
            url: "https://media.example.invalid".to_string(),
            media_server: Some(MediaServerInputConfigDto {
                token: Some("   ".to_string()),
                api_key: Some("".to_string()),
                ..media_server_config_with_library()
            }),
            ..ConfigInputDto::default()
        };
        let err = prepare_dto(&mut emby).expect_err("blank token/api_key should be rejected");
        assert!(err.to_string().contains("requires media_server token/api_key"));

        let mut plex = ConfigInputDto {
            name: "plex_media_server".intern(),
            input_type: InputType::Plex,
            media_server: Some(MediaServerInputConfigDto {
                account_token: Some("   ".to_string()),
                server_id: Some("   ".to_string()),
                ..media_server_config_with_library()
            }),
            ..ConfigInputDto::default()
        };
        let err = prepare_dto(&mut plex).expect_err("blank plex token should be rejected");
        assert!(err.to_string().contains("requires media_server.account_token or media_server.token"));
    }

    #[test]
    fn prepare_accepts_media_server_max_connections_as_stream_limit() {
        let mut dto = ConfigInputDto {
            name: "emby_media_server".intern(),
            input_type: InputType::Emby,
            url: "https://media.example.invalid".to_string(),
            max_connections: 1,
            media_server: Some(MediaServerInputConfigDto {
                token: Some("token-value".to_string()),
                ..media_server_config_with_library()
            }),
            ..ConfigInputDto::default()
        };

        prepare_dto(&mut dto).expect("media_server inputs reuse max_connections stream-limit semantics");
    }

    #[test]
    fn media_server_defaults_are_conservative() {
        let media_server = MediaServerInputConfigDto::default();

        assert_eq!(media_server.catalog.page_size, 100);
        assert_eq!(media_server.catalog.request_delay_ms, 250);
        assert!(media_server.catalog.include_media_sources);
        assert!(!media_server.catalog.include_paths);
        assert!(!media_server.catalog.include_user_state);
        assert!(!media_server.catalog.refresh_on_startup);
        assert!(media_server.playback.direct_play_only);
        assert!(!media_server.playback.allow_transcode);
        assert!(!media_server.playback.preflight_streams);
        assert_eq!(media_server.image_policy, MediaServerImagePolicyDto::ProxyOnDemand);
        assert!(!media_server.allow_relay);
    }

    #[test]
    fn media_server_enrichment_block_is_not_part_of_schema() {
        let err = serde_json::from_str::<MediaServerInputConfigDto>(
            r#"{"libraries":["Movies"],"enrichment":{"ffprobe":true,"tmdb_lookup":true,"fetch_images":true}}"#,
        )
        .expect_err("media_server.enrichment must not be accepted");

        assert!(err.to_string().contains("unknown field `enrichment`"));
    }

    #[test]
    fn prepare_accepts_emby_media_server_with_token_and_library() {
        let mut dto = ConfigInputDto {
            name: "emby_media_server".intern(),
            input_type: InputType::Emby,
            url: " https://media.example.invalid/ ".to_string(),
            media_server: Some(MediaServerInputConfigDto {
                token: Some(" token-value ".to_string()),
                ..media_server_config_with_library()
            }),
            ..ConfigInputDto::default()
        };

        prepare_dto(&mut dto).expect("emby media_server config should prepare");

        assert_eq!(dto.url, "https://media.example.invalid/");
        assert!(dto.input_type.is_media_server());
        assert_eq!(
            dto.media_server.as_ref().and_then(|media_server| media_server.token.as_deref()),
            Some("token-value")
        );
    }

    #[test]
    fn prepare_rejects_media_server_without_media_server_block() {
        let mut dto = ConfigInputDto {
            name: "emby_media_server".intern(),
            input_type: InputType::Emby,
            url: "https://media.example.invalid".to_string(),
            ..ConfigInputDto::default()
        };

        let err = prepare_dto(&mut dto).expect_err("media_server block should be mandatory");
        assert!(err.to_string().contains("media_server configuration is mandatory"));
    }

    #[test]
    fn prepare_allows_disabled_media_server_input_with_incomplete_config() {
        let mut dto = ConfigInputDto {
            name: "disabled_plex".intern(),
            input_type: InputType::Plex,
            enabled: false,
            ..ConfigInputDto::default()
        };

        prepare_dto(&mut dto).expect("disabled media_server input should not require active playback/catalog config");
        assert_eq!(dto.input_type, InputType::Plex);
        assert!(!dto.enabled);
    }

    #[test]
    fn prepare_normalizes_disabled_media_server_config_without_enforcing_invariants() {
        let mut dto = ConfigInputDto {
            name: "disabled_emby".intern(),
            input_type: InputType::Emby,
            enabled: false,
            media_server: Some(MediaServerInputConfigDto {
                token: Some(" token-value ".to_string()),
                libraries: vec![MediaServerLibrarySelectorDto::Name("   ".to_string())],
                catalog: MediaServerCatalogConfigDto { page_size: 0, ..MediaServerCatalogConfigDto::default() },
                ..MediaServerInputConfigDto::default()
            }),
            ..ConfigInputDto::default()
        };

        prepare_dto(&mut dto).expect("disabled media_server input can preserve incomplete config for later repair");
        let media_server = dto.media_server.as_ref().expect("media_server config should be preserved");
        assert_eq!(media_server.token.as_deref(), Some("token-value"));
        assert!(media_server.libraries[0].is_empty());
    }

    #[test]
    fn prepare_rejects_emby_media_server_without_input_url() {
        let mut dto = ConfigInputDto {
            name: "emby_media_server".intern(),
            input_type: InputType::Emby,
            media_server: Some(MediaServerInputConfigDto {
                token: Some("token-value".to_string()),
                ..media_server_config_with_library()
            }),
            ..ConfigInputDto::default()
        };

        let err = prepare_dto(&mut dto).expect_err("emby media_server input should require a direct server URL");
        assert!(err.to_string().contains("url is mandatory for input type emby"));
    }

    #[test]
    fn prepare_rejects_media_server_provider_scheme_url() {
        let mut dto = ConfigInputDto {
            name: "emby_media_server".intern(),
            input_type: InputType::Emby,
            url: " provider://media-server ".to_string(),
            media_server: Some(MediaServerInputConfigDto {
                token: Some("token-value".to_string()),
                ..media_server_config_with_library()
            }),
            ..ConfigInputDto::default()
        };

        let err = prepare_dto(&mut dto).expect_err("media_server input must not use provider URLs");
        assert!(err.to_string().contains("does not support batch:// or provider://"));
    }

    #[test]
    fn prepare_rejects_media_server_staged_input() {
        let mut dto = ConfigInputDto {
            name: "emby_media_server".intern(),
            input_type: InputType::Emby,
            url: "https://media.example.invalid".to_string(),
            media_server: Some(MediaServerInputConfigDto {
                token: Some("token-value".to_string()),
                ..media_server_config_with_library()
            }),
            staged: Some(StagedInputDto { enabled: true, name: "staged".intern(), ..StagedInputDto::default() }),
            ..ConfigInputDto::default()
        };

        let err = prepare_dto(&mut dto).expect_err("media_server input must reject staged config");
        assert!(err.to_string().contains("does not support staged inputs"));
    }

    #[test]
    fn prepare_rejects_plex_without_token_or_server_selector() {
        let mut without_token = ConfigInputDto {
            name: "plex_media_server".intern(),
            input_type: InputType::Plex,
            media_server: Some(MediaServerInputConfigDto {
                machine_id: Some("machine".to_string()),
                ..media_server_config_with_library()
            }),
            ..ConfigInputDto::default()
        };
        let err = prepare_dto(&mut without_token).expect_err("plex token should be mandatory");
        assert!(err.to_string().contains("requires media_server.account_token or media_server.token"));

        let mut without_selector = ConfigInputDto {
            name: "plex_media_server".intern(),
            input_type: InputType::Plex,
            media_server: Some(MediaServerInputConfigDto {
                account_token: Some("token".to_string()),
                ..media_server_config_with_library()
            }),
            ..ConfigInputDto::default()
        };
        let err = prepare_dto(&mut without_selector).expect_err("plex server selector should be mandatory");
        assert!(err.to_string().contains("requires a server selector"));
    }

    #[test]
    fn prepare_accepts_plex_without_input_url_when_discovery_is_configured() {
        let mut dto = ConfigInputDto {
            name: "plex_media_server".intern(),
            input_type: InputType::Plex,
            media_server: Some(MediaServerInputConfigDto {
                account_token: Some("token".to_string()),
                machine_id: Some("machine".to_string()),
                ..media_server_config_with_library()
            }),
            ..ConfigInputDto::default()
        };

        prepare_dto(&mut dto).expect("plex discovery config should not require input.url");
        assert_eq!(dto.input_type, InputType::Plex);
    }

    #[test]
    fn prepare_accepts_plex_media_server_with_direct_url_without_selector() {
        let mut dto = ConfigInputDto {
            name: "plex_media_server".intern(),
            input_type: InputType::Plex,
            url: "https://plex.example.invalid".to_string(),
            media_server: Some(MediaServerInputConfigDto {
                token: Some("token".to_string()),
                ..media_server_config_with_library()
            }),
            ..ConfigInputDto::default()
        };

        prepare_dto(&mut dto).expect("direct Plex URL should not require MyPlex server selector");
        assert_eq!(dto.input_type, InputType::Plex);
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
    fn test_cluster_source_serde_roundtrip() {
        let json = r#""staged""#;
        let cs: ClusterSource = serde_json::from_str(json).expect("deserialize staged");
        assert_eq!(cs, ClusterSource::Staged);
        assert_eq!(serde_json::to_string(&cs).expect("serialize"), json);

        let cs: ClusterSource = serde_json::from_str(r#""input""#).expect("deserialize input");
        assert_eq!(cs, ClusterSource::Input);

        let cs: ClusterSource = serde_json::from_str(r#""skip""#).expect("deserialize skip");
        assert_eq!(cs, ClusterSource::Skip);
    }

    #[test]
    fn test_staged_m3u_vod_source_staged_rejected() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.staged = Some(StagedInputDto {
            name: "staged".into(),
            input_type: InputType::M3u,
            url: "http://staged.com/playlist.m3u".to_string(),
            vod_source: Some(ClusterSource::Staged),
            ..StagedInputDto::default()
        });

        let err =
            dto.prepare(0, true, &HashSet::new(), None).expect_err("should reject vod_source=staged for M3U staged");
        assert!(err.to_string().contains("Staged M3U input cannot provide VOD or Series"), "Error: {err}");
    }

    #[test]
    fn test_staged_m3u_series_source_staged_rejected() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.staged = Some(StagedInputDto {
            name: "staged".into(),
            input_type: InputType::M3u,
            url: "http://staged.com/playlist.m3u".to_string(),
            series_source: Some(ClusterSource::Staged),
            ..StagedInputDto::default()
        });

        let err =
            dto.prepare(0, true, &HashSet::new(), None).expect_err("should reject series_source=staged for M3U staged");
        assert!(err.to_string().contains("Staged M3U input cannot provide VOD or Series"), "Error: {err}");
    }

    #[test]
    fn test_staged_xtream_with_cluster_sources_accepted() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.staged = Some(StagedInputDto {
            name: "staged".into(),
            input_type: InputType::Xtream,
            url: "http://staged.com".to_string(),
            username: Some("su".to_string()),
            password: Some("sp".to_string()),
            live_source: Some(ClusterSource::Staged),
            vod_source: Some(ClusterSource::Input),
            series_source: Some(ClusterSource::Skip),
            ..StagedInputDto::default()
        });

        dto.prepare(0, true, &HashSet::new(), None).expect("xtream staged with all cluster sources should succeed");
    }

    #[test]
    fn test_staged_enabled_requires_at_least_one_staged_cluster_source() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.staged = Some(StagedInputDto {
            enabled: true,
            name: "staged".into(),
            input_type: InputType::Xtream,
            url: "http://staged.com".to_string(),
            username: Some("su".to_string()),
            password: Some("sp".to_string()),
            live_source: Some(ClusterSource::Input),
            vod_source: Some(ClusterSource::Skip),
            series_source: Some(ClusterSource::Input),
            ..StagedInputDto::default()
        });

        let err = dto
            .prepare(0, true, &HashSet::new(), None)
            .expect_err("expected validation error for missing staged source");
        assert!(err.to_string().contains("no cluster source uses 'staged'"), "Error: {err}");
    }

    #[test]
    fn test_staged_skip_flag_excludes_cluster_from_staged_requirement() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.options = Some(ConfigInputOptionsDto { xtream_skip_live: true, ..ConfigInputOptionsDto::default() });
        dto.staged = Some(StagedInputDto {
            enabled: true,
            name: "staged".into(),
            input_type: InputType::Xtream,
            url: "http://staged.com".to_string(),
            username: Some("su".to_string()),
            password: Some("sp".to_string()),
            live_source: Some(ClusterSource::Staged),
            vod_source: Some(ClusterSource::Input),
            series_source: Some(ClusterSource::Input),
            ..StagedInputDto::default()
        });

        let err = dto
            .prepare(0, true, &HashSet::new(), None)
            .expect_err("skipped staged cluster must not satisfy staged-source requirement");
        assert!(err.to_string().contains("no cluster source uses 'staged'"), "Error: {err}");
    }

    #[test]
    fn test_staged_m3u_vod_staged_allowed_when_vod_is_skipped() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.options = Some(ConfigInputOptionsDto { xtream_skip_vod: true, ..ConfigInputOptionsDto::default() });
        dto.staged = Some(StagedInputDto {
            name: "staged".into(),
            input_type: InputType::M3u,
            url: "http://staged.com/playlist.m3u".to_string(),
            vod_source: Some(ClusterSource::Staged),
            ..StagedInputDto::default()
        });

        dto.prepare(0, true, &HashSet::new(), None)
            .expect("staged M3U vod_source=staged is valid when VOD cluster is skipped");
    }

    #[test]
    fn test_staged_disabled_skips_cluster_source_validation() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.staged = Some(StagedInputDto {
            enabled: false,
            name: "staged".into(),
            input_type: InputType::Xtream,
            url: "http://staged.com".to_string(),
            live_source: Some(ClusterSource::Input),
            vod_source: Some(ClusterSource::Skip),
            series_source: Some(ClusterSource::Input),
            ..StagedInputDto::default()
        });

        dto.prepare(0, true, &HashSet::new(), None)
            .expect("disabled staged input should not enforce cluster source validation");
    }

    #[test]
    fn test_staged_disabled_skips_m3u_cluster_constraints() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.staged = Some(StagedInputDto {
            enabled: false,
            name: "staged".into(),
            input_type: InputType::M3u,
            url: "http://staged.com/playlist.m3u".to_string(),
            vod_source: Some(ClusterSource::Staged),
            series_source: Some(ClusterSource::Staged),
            ..StagedInputDto::default()
        });

        dto.prepare(0, true, &HashSet::new(), None)
            .expect("disabled staged input should not enforce staged M3U cluster validation");
    }

    #[test]
    fn test_staged_disabled_skips_provider_url_validation() {
        let mut dto = create_test_dto();
        dto.input_type = InputType::Xtream;
        dto.url = "http://main.com".to_string();
        dto.username = Some("u".to_string());
        dto.password = Some("p".to_string());
        dto.staged = Some(StagedInputDto {
            enabled: false,
            name: "staged".into(),
            input_type: InputType::Xtream,
            url: "provider://missing-provider".to_string(),
            ..StagedInputDto::default()
        });

        dto.prepare(0, true, &HashSet::new(), None)
            .expect("disabled staged input should not enforce provider URL validation");
    }

    #[test]
    fn test_staged_dto_defaults_none() {
        let staged = StagedInputDto::default();
        assert!(staged.live_source.is_none());
        assert!(staged.vod_source.is_none());
        assert!(staged.series_source.is_none());
    }

    #[test]
    fn test_staged_dto_is_empty_with_cluster_source() {
        let mut staged = StagedInputDto::default();
        assert!(staged.is_empty());

        staged.live_source = Some(ClusterSource::Input);
        assert!(!staged.is_empty());
    }

    #[test]
    fn test_staged_dto_clean_resets_cluster_sources() {
        let mut staged = StagedInputDto {
            live_source: Some(ClusterSource::Input),
            vod_source: Some(ClusterSource::Skip),
            series_source: Some(ClusterSource::Staged),
            ..StagedInputDto::default()
        };
        staged.clean();
        assert!(staged.live_source.is_none());
        assert!(staged.vod_source.is_none());
        assert!(staged.series_source.is_none());
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
}
