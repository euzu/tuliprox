use crate::error::{TuliproxError, TuliproxErrorKind};
use crate::model::WebAuthConfigDto;
use crate::utils::{default_as_true};

const RESERVED_PATHS: &[&str] = &[
    "live", "movie", "series", "m3u-stream", "healthcheck", "status",
    "player_api.php", "panel_api.php", "xtream", "timeshift", "timeshift.php", "streaming",
    "get.php", "apiget", "m3u", "resource"
];


#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WebUiConfigDto {
    #[serde(default = "default_as_true")]
    pub enabled: bool,
    #[serde(default = "default_as_true")]
    pub user_ui_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<WebAuthConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player_server: Option<String>,
}

impl WebUiConfigDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if !self.enabled {
            self.auth = None;
        }

        if let Some(web_ui_path) = self.path.as_ref() {
            let web_path = web_ui_path.trim();
            if web_path.is_empty() {
                self.path = None;
            } else {
                let web_path = web_path.trim().trim_start_matches('/').trim_end_matches('/').to_string();
                if RESERVED_PATHS.contains(&web_path.to_lowercase().as_str()) {
                    return Err(TuliproxError::new(TuliproxErrorKind::Info, format!("web ui path is a reserved path. Do not use {RESERVED_PATHS:?}")));
                }
                self.path = Some(web_path);
            }
        }
        Ok(())
    }
}