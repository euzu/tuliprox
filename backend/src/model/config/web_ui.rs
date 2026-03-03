use shared::error::TuliproxError;
use shared::model::{ContentSecurityPolicyConfigDto, WebUiConfigDto};
use shared::utils::default_kick_secs;
use crate::model::{macros, WebAuthConfig};

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
            combine_views_stats_streams: dto.combine_views_stats_streams
        }
    }
}
impl From<&WebUiConfig> for WebUiConfigDto {
    fn from(instance: &WebUiConfig) -> Self {
        Self {
            enabled: instance.enabled,
            user_ui_enabled: instance.user_ui_enabled,
            content_security_policy: instance.content_security_policy.as_ref().map(Into::into),
            path: instance.path.clone(),
            auth: instance.auth.as_ref().map(Into::into),
            player_server: instance.player_server.clone(),
            kick_secs: instance.kick_secs,
            combine_views_stats_streams: instance.combine_views_stats_streams
        }
    }
}
