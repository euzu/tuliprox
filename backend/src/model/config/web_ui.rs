use shared::error::TuliproxError;
use shared::model::WebUiConfigDto;
use crate::model::{macros, WebAuthConfig};

#[derive(Debug, Clone)]
pub struct WebUiConfig {
    pub enabled: bool,
    pub user_ui_enabled: bool,
    pub path: Option<String>,
    pub auth: Option<WebAuthConfig>,
    pub player_server: Option<String>,
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
        Ok(())
    }
}

macros::from_impl!(WebUiConfig);
impl From<&WebUiConfigDto> for WebUiConfig {
    fn from(dto: &WebUiConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            user_ui_enabled: dto.user_ui_enabled,
            path: dto.path.clone(),
            auth: dto.auth.as_ref().map(Into::into),
            player_server: dto.player_server.clone(),
        }
    }
}
impl From<&WebUiConfig> for WebUiConfigDto {
    fn from(instance: &WebUiConfig) -> Self {
        Self {
            enabled: instance.enabled,
            user_ui_enabled: instance.user_ui_enabled,
            path: instance.path.clone(),
            auth: instance.auth.as_ref().map(Into::into),
            player_server: instance.player_server.clone(),
        }
    }
}