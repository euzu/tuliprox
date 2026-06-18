use crate::model::{macros, WebAuthConfig};
use shared::error::TuliproxError;
use shared::model::view_type::ViewType;
use shared::model::{ContentSecurityPolicyConfigDto, StreamInfoConfigDto, StreamInfoFields, StreamInfoFieldsSet, WebUiConfigDto};
use shared::utils::default_kick_secs;

#[derive(Debug, Clone)]
pub struct StreamInfoConfig {
    pub flags: StreamInfoFieldsSet,
}

impl StreamInfoConfig {
    pub fn is_none(&self) -> bool {
        self.flags.is_empty()
    }

    #[inline]
    pub fn hide_group(&self) -> bool {
        self.flags.contains(StreamInfoFields::HideGroup)
    }

    #[inline]
    pub fn hide_ip(&self) -> bool {
        self.flags.contains(StreamInfoFields::HideIp)
    }

    #[inline]
    pub fn hide_country(&self) -> bool {
        self.flags.contains(StreamInfoFields::HideCountry)
    }

    #[inline]
    pub fn hide_shared(&self) -> bool {
        self.flags.contains(StreamInfoFields::HideShared)
    }

    #[inline]
    pub fn hide_duration(&self) -> bool {
        self.flags.contains(StreamInfoFields::HideDuration)
    }

    #[inline]
    pub fn hide_bandwidth(&self) -> bool {
        self.flags.contains(StreamInfoFields::HideBandwidth)
    }

    #[inline]
    pub fn hide_transferred(&self) -> bool { self.flags.contains(StreamInfoFields::HideTransferred) }

    #[inline]
    pub fn hide_player(&self) -> bool {
        self.flags.contains(StreamInfoFields::HidePlayer)
    }

    #[inline]
    pub fn hide_user_comment(&self) -> bool { self.flags.contains(StreamInfoFields::HideUserComment) }

    #[inline]
    pub fn hide_epg(&self) -> bool {
        self.flags.contains(StreamInfoFields::HideEpg)
    }
}

#[derive(Debug, Clone)]
pub struct ContentSecurityPolicyConfig {
    pub enabled: bool,
    pub custom_attributes: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct WebUiConfig {
    pub enabled: bool,
    pub user_ui_enabled: bool,
    pub content_security_policy: Option<ContentSecurityPolicyConfig>,
    pub path: Option<String>,
    pub auth: Option<WebAuthConfig>,
    pub player_server: Option<String>,
    pub kick_secs: u64,
    pub combine_views_stats_streams: bool,
    pub landing_page: Option<ViewType>,
    pub stream_info: Option<StreamInfoConfig>,
}

impl WebUiConfig {
    pub fn prepare(&mut self, config_path: &str) -> Result<(), TuliproxError> {
        if let Some(web_auth) = &mut self.auth {
            if web_auth.enabled {
                web_auth.prepare(config_path)?;
            } else {
                self.auth = None;
            }
        }
        if self.kick_secs == 0 {
            self.kick_secs = default_kick_secs();
        }
        Ok(())
    }
}

macros::from_impl!(ContentSecurityPolicyConfig);

impl From<&ContentSecurityPolicyConfigDto> for ContentSecurityPolicyConfig {
    fn from(dto: &ContentSecurityPolicyConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            custom_attributes: dto.custom_attributes.clone(),
        }
    }
}

impl From<&ContentSecurityPolicyConfig> for ContentSecurityPolicyConfigDto {
    fn from(e: &ContentSecurityPolicyConfig) -> Self {
        Self {
            enabled: e.enabled,
            custom_attributes: e.custom_attributes.clone(),
        }
    }
}

macros::from_impl!(StreamInfoConfig);

impl From<&StreamInfoConfigDto> for StreamInfoConfig {
    fn from(dto: &StreamInfoConfigDto) -> Self {
        let flags = dto.get_flags();
        Self { flags }
    }
}

impl From<&StreamInfoConfig> for StreamInfoConfigDto {
    fn from(cfg: &StreamInfoConfig) -> Self {
        Self {
            hide_group: cfg.hide_group(),
            hide_ip: cfg.hide_ip(),
            hide_country: cfg.hide_country(),
            hide_shared: cfg.hide_shared(),
            hide_duration: cfg.hide_duration(),
            hide_bandwidth: cfg.hide_bandwidth(),
            hide_transferred: cfg.hide_transferred(),
            hide_player: cfg.hide_player(),
            hide_user_comment: cfg.hide_user_comment(),
            hide_epg: cfg.hide_epg(),
        }
    }
}

macros::from_impl!(WebUiConfig);
impl From<&WebUiConfigDto> for WebUiConfig {
    fn from(dto: &WebUiConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            user_ui_enabled: dto.user_ui_enabled,
            content_security_policy: dto.content_security_policy.as_ref().map(Into::into),
            path: dto.path.as_ref().and_then(|path| {
                let trimmed = path.trim();
                let normalized = trimmed.trim_matches('/');
                if normalized.is_empty() {
                    None
                } else {
                    Some(normalized.to_string())
                }
            }),
            auth: dto.auth.as_ref().map(Into::into),
            player_server: dto.player_server.clone(),
            kick_secs: dto.kick_secs,
            combine_views_stats_streams: dto.combine_views_stats_streams,
            landing_page: dto.landing_page,
            stream_info: dto.stream_info.as_ref().map(Into::into),
        }
    }
}
impl From<&WebUiConfig> for WebUiConfigDto {
    fn from(instance: &WebUiConfig) -> Self {
        let stream_info = instance.stream_info.as_ref().and_then(|cfg| {
            if cfg.is_none() {
                None
            } else {
                Some(cfg.into())
            }
        });
        Self {
            enabled: instance.enabled,
            user_ui_enabled: instance.user_ui_enabled,
            content_security_policy: instance.content_security_policy.as_ref().map(Into::into),
            path: instance.path.clone(),
            auth: instance.auth.as_ref().map(Into::into),
            player_server: instance.player_server.clone(),
            kick_secs: instance.kick_secs,
            combine_views_stats_streams: instance.combine_views_stats_streams,
            landing_page: instance.landing_page,
            stream_info,
        }
    }
}
