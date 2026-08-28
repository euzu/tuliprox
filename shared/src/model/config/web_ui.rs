use crate::{
    apply_flags, create_bitset,
    defaults::{default_as_true, default_kick_secs, is_default_kick_secs, is_false, is_true},
    error::TuliproxError,
    model::{view_type::ViewType, WebAuthConfigDto},
    utils::{is_blank_optional_str, is_blank_optional_string},
};

const RESERVED_PATHS: &[&str] = &[
    "cvs",
    "live",
    "movie",
    "series",
    "m3u-stream",
    "healthcheck",
    "status",
    "player_api.php",
    "panel_api.php",
    "xtream",
    "timeshift",
    "timeshift.php",
    "streaming",
    "get.php",
    "apiget",
    "m3u",
    "resource",
];

fn default_web_ui_path() -> Option<String> {
    Some("/".to_string())
}

create_bitset!(
    u16,
    StreamInfoFields,
    HideGroup,
    HideIp,
    HideCountry,
    HideShared,
    HideDuration,
    HideBandwidth,
    HideTransferred,
    HidePlayer,
    HideUserComment,
    HideEpg
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct StreamInfoConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_group: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_ip: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_country: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_shared: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_duration: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_bandwidth: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_transferred: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_player: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_user_comment: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_epg: bool,
}

impl StreamInfoConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.hide_group
            && !self.hide_ip
            && !self.hide_country
            && !self.hide_shared
            && !self.hide_duration
            && !self.hide_bandwidth
            && !self.hide_transferred
            && !self.hide_player
            && !self.hide_user_comment
            && !self.hide_epg
    }

    pub fn get_flags(&self) -> StreamInfoFieldsSet {
        let mut flags = StreamInfoFieldsSet::new();
        apply_flags!(
            self, flags, StreamInfoFields;
            (hide_group, HideGroup),
            (hide_ip, HideIp),
            (hide_country, HideCountry),
            (hide_shared, HideShared),
            (hide_duration, HideDuration),
            (hide_bandwidth, HideBandwidth),
            (hide_transferred, HideTransferred),
            (hide_player, HidePlayer),
            (hide_user_comment, HideUserComment),
            (hide_epg, HideEpg)
        );
        flags
    }
}

fn is_blank_or_default_web_ui_path(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| {
        let trimmed = value.trim();
        trimmed.is_empty() || trimmed.chars().all(|c| c == '/')
    })
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ContentSecurityPolicyConfigDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_attributes: Option<Vec<String>>,
}

impl ContentSecurityPolicyConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.enabled
            && (self.custom_attributes.is_none()
                || self.custom_attributes.as_ref().is_some_and(std::vec::Vec::is_empty))
    }

    pub fn validate(&self) -> Result<(), TuliproxError> {
        if let Some(attrs) = self.custom_attributes.as_ref() {
            for (i, attr) in attrs.iter().enumerate() {
                // Prohibit CR/LF/NUL (header injection)
                if attr.contains('\r') || attr.contains('\n') || attr.contains('\0') {
                    return Err(TuliproxError::ConfigWebUi(format!(
                        "custom-attributes[{i}] contains forbidden control characters"
                    )));
                }
                //Optional: prohibit additional CTLs (except HTAB)
                if attr.chars().any(|c| {
                    let u = c as u32;
                    (u < 0x20 && c != '\t') || u == 0x7F
                }) {
                    return Err(TuliproxError::ConfigWebUi(format!(
                        "custom-attributes[{i}] contains control characters"
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WebUiConfigDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub user_ui_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_security_policy: Option<ContentSecurityPolicyConfigDto>,
    #[serde(default = "default_web_ui_path", skip_serializing_if = "is_blank_or_default_web_ui_path")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<WebAuthConfigDto>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub player_server: Option<String>,
    #[serde(default = "default_kick_secs", skip_serializing_if = "is_default_kick_secs")]
    pub kick_secs: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub combine_views_stats_streams: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing_page: Option<ViewType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_info: Option<StreamInfoConfigDto>,
}

impl Default for WebUiConfigDto {
    fn default() -> Self {
        WebUiConfigDto {
            enabled: default_as_true(),
            user_ui_enabled: default_as_true(),
            content_security_policy: None,
            path: default_web_ui_path(),
            auth: None,
            player_server: None,
            kick_secs: default_kick_secs(),
            combine_views_stats_streams: false,
            landing_page: None,
            stream_info: None,
        }
    }
}

impl WebUiConfigDto {
    pub fn is_empty(&self) -> bool {
        let empty = WebUiConfigDto::default();
        self.enabled == empty.enabled
            && self.user_ui_enabled == empty.user_ui_enabled
            && !self.combine_views_stats_streams
            && self.landing_page.is_none()
            && is_blank_or_default_web_ui_path(&self.path)
            && is_blank_optional_str(self.player_server.as_deref())
            && self.kick_secs == default_kick_secs()
            && (self.content_security_policy.is_none()
                || self.content_security_policy.as_ref().is_some_and(ContentSecurityPolicyConfigDto::is_empty))
            && (self.auth.is_none() || self.auth.as_ref().is_some_and(super::web_auth::WebAuthConfigDto::is_empty))
            && (self.stream_info.is_none() || self.stream_info.as_ref().is_some_and(StreamInfoConfigDto::is_empty))
    }

    pub fn clean(&mut self) {
        if self.content_security_policy.as_ref().is_some_and(ContentSecurityPolicyConfigDto::is_empty) {
            self.content_security_policy = None;
        }
        if self.auth.as_ref().is_some_and(super::web_auth::WebAuthConfigDto::is_empty) {
            self.auth = None;
        }
        if self.stream_info.as_ref().is_some_and(StreamInfoConfigDto::is_empty) {
            self.stream_info = None;
        }

        if is_blank_or_default_web_ui_path(&self.path) {
            self.path = None;
        }
        if is_blank_optional_str(self.player_server.as_deref()) {
            self.player_server = None;
        }
        self.kick_secs = default_kick_secs();
    }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if !self.enabled {
            self.auth = None;
        }

        if let Some(web_ui_path) = self.path.as_ref() {
            let web_path = web_ui_path.trim();
            if web_path.is_empty() {
                self.path = None;
            } else {
                let normalized_path = web_path.trim_start_matches('/').trim_end_matches('/');
                if normalized_path.is_empty() {
                    self.path = default_web_ui_path();
                } else {
                    let normalized_path = normalized_path.to_string();
                    if RESERVED_PATHS.contains(&normalized_path.to_lowercase().as_str()) {
                        return Err(TuliproxError::ConfigWebUi(format!(
                            "web ui path is a reserved path. Do not use {RESERVED_PATHS:?}"
                        )));
                    }
                    self.path = Some(normalized_path);
                }
            }
        }
        if let Some(csp) = &self.content_security_policy {
            csp.validate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_info_config_dto_default_has_all_flags_false() {
        let dto = StreamInfoConfigDto::default();
        assert!(!dto.hide_group);
        assert!(!dto.hide_ip);
        assert!(!dto.hide_country);
        assert!(!dto.hide_shared);
        assert!(!dto.hide_duration);
        assert!(!dto.hide_bandwidth);
        assert!(!dto.hide_transferred);
        assert!(!dto.hide_player);
        assert!(!dto.hide_user_comment);
        assert!(!dto.hide_epg);
    }

    #[test]
    fn stream_info_config_dto_is_empty_when_all_flags_false() {
        let dto = StreamInfoConfigDto::default();
        assert!(dto.is_empty());
    }

    #[test]
    fn stream_info_config_dto_is_not_empty_when_any_flag_true() {
        let dto = StreamInfoConfigDto { hide_ip: true, ..StreamInfoConfigDto::default() };
        assert!(!dto.is_empty());
    }

    #[test]
    fn web_ui_config_dto_clean_normalizes_all_false_stream_info_to_none() {
        let dto = WebUiConfigDto { stream_info: Some(StreamInfoConfigDto::default()), ..WebUiConfigDto::default() };
        let mut dto = dto;
        dto.clean();
        assert!(dto.stream_info.is_none());
    }

    #[test]
    fn web_ui_config_dto_is_empty_treats_none_stream_info_as_absent() {
        let dto = WebUiConfigDto::default();
        assert!(dto.stream_info.is_none());
        assert!(dto.is_empty());
    }

    #[test]
    fn web_ui_config_dto_default_uses_last_page_landing_page() {
        let dto = WebUiConfigDto::default();

        assert_eq!(dto.landing_page, None);
    }

    #[test]
    fn web_ui_config_dto_is_not_empty_when_explicit_landing_page_is_set() {
        let dto = WebUiConfigDto { landing_page: Some(ViewType::Streams), ..WebUiConfigDto::default() };

        assert!(!dto.is_empty());
    }
}
