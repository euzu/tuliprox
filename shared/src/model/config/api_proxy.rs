use crate::{
    defaults::{default_auth_error_status, is_default_auth_error_status, is_false},
    error::TuliproxError,
    foundation::get_filter,
    model::{ClusterFlags, ProxyType, ProxyUserCredentialsDto},
    utils::is_blank_optional_string,
};
use std::collections::HashSet;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct TargetUserDto {
    pub target: String,
    pub credentials: Vec<ProxyUserCredentialsDto>,
}

/// Reusable capability tier referenced by users via `plan: <name>`.
/// User-level values always override plan values; unset user values inherit.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UserPlanDto {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_clusters: Option<ClusterFlags>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyType>,
    #[serde(default)]
    pub max_connections: u32,
    #[serde(default)]
    pub soft_connections: u16,
    /// Filter DSL expression restricting visible content for plan members.
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub filter: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub comment: Option<String>,
}

impl UserPlanDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            return Err(TuliproxError::ConfigApiProxy("User plan name is empty".to_string()));
        }
        if let Some(filter) = &self.filter {
            let trimmed = filter.trim();
            if trimmed.is_empty() {
                self.filter = None;
            } else {
                // Templates are not available in api-proxy; fail fast on syntax errors.
                get_filter(trimmed, None).map_err(|err| {
                    TuliproxError::ConfigApiProxy(format!("Invalid filter in user plan {}: {err}", self.name))
                })?;
                self.filter = Some(trimmed.to_string());
            }
        }
        Ok(())
    }
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ApiProxyServerInfoDto {
    pub name: String,
    pub protocol: String,
    pub host: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub port: Option<String>,
    pub timezone: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ApiProxyConfigDto {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub server: Vec<ApiProxyServerInfoDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user: Vec<TargetUserDto>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub use_user_db: bool,
    /// HTTP status code returned for authentication failures (default: 403).
    #[serde(default = "default_auth_error_status", skip_serializing_if = "is_default_auth_error_status")]
    pub auth_error_status: u16,
}

impl Default for ApiProxyConfigDto {
    fn default() -> Self {
        Self {
            server: Vec::new(),
            user: Vec::new(),
            use_user_db: false,
            auth_error_status: default_auth_error_status(),
        }
    }
}

impl ApiProxyServerInfoDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            return Err(TuliproxError::ConfigApiProxy("Server info name is empty ".to_string()));
        }
        self.protocol = self.protocol.trim().to_string();
        if self.protocol.is_empty() {
            return Err(TuliproxError::ConfigApiProxy("protocol can't be empty for api server config".to_string()));
        }
        self.host = self.host.trim().to_string();
        if self.host.is_empty() {
            return Err(TuliproxError::ConfigApiProxy("host can't be empty for api server config".to_string()));
        }
        if let Some(port) = self.port.as_ref() {
            let port = port.trim().to_string();
            if port.is_empty() {
                self.port = None;
            } else if port.parse::<u16>().is_err() {
                return Err(TuliproxError::ConfigApiProxy("invalid port for api server config".to_string()));
            } else {
                self.port = Some(port);
            }
        }

        self.timezone = self.timezone.trim().to_string();
        if self.timezone.is_empty() {
            self.timezone = "UTC".to_string();
        }
        self.message = self.message.trim().to_string();
        if self.message.is_empty() {
            self.message = "Welcome to tuliprox".to_string();
        }
        if let Some(path) = &self.path {
            let trimmed_path = path.trim();
            if trimmed_path.is_empty() {
                self.path = None;
            } else {
                self.path = Some(trimmed_path.to_string());
            }
        }

        Ok(())
    }
    pub fn validate(&mut self) -> bool { self.prepare().is_ok() }
}

impl ApiProxyConfigDto {
    fn prepare_server_config(&mut self, errors: &mut Vec<String>) {
        let mut name_set = HashSet::new();
        for server in &mut self.server {
            if let Err(err) = server.prepare() {
                errors.push(err.to_string());
            }
            if name_set.contains(server.name.as_str()) {
                errors.push(format!("Non-unique server info name found {}", server.name));
            } else {
                name_set.insert(server.name.clone());
            }
        }
    }

    fn prepare_target_user(&mut self, errors: &mut Vec<String>) {
        let mut usernames = HashSet::new();
        let mut tokens = HashSet::new();
        for target_user in &mut self.user {
            for user in &mut target_user.credentials {
                if let Err(err) = user.prepare() {
                    errors.push(err.to_string());
                }
                if usernames.contains(&user.username) {
                    errors.push(format!("Non unique username found {}", user.username));
                } else {
                    usernames.insert(user.username.to_string());
                }
                if let Some(token) = &user.token {
                    if token.is_empty() {
                        user.token = None;
                    } else if tokens.contains(token) {
                        errors.push(format!(
                            "Non unique user token found {} for user {}",
                            user.token.as_ref().map_or_else(String::new, ToString::to_string),
                            user.username
                        ));
                    } else {
                        tokens.insert(token.to_string());
                    }
                }

                if let Some(server_info_name) = &user.server {
                    if !&self.server.iter().any(|server_info| server_info.name.eq(server_info_name)) {
                        errors.push(format!(
                            "No server info with name {} found for user {}",
                            server_info_name, user.username
                        ));
                    }
                }
            }
        }
    }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        let mut errors = Vec::new();
        if self.server.is_empty() {
            errors.push("No server info defined".to_string());
        } else {
            self.prepare_server_config(&mut errors);
        }
        self.prepare_target_user(&mut errors);
        // A success or redirect code here would make auth failures look like valid responses
        if !(400..=599).contains(&self.auth_error_status) {
            errors.push(format!(
                "auth_error_status must be a client or server error code (400-599), got {}",
                self.auth_error_status
            ));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(TuliproxError::ConfigApiProxy(errors.join("\n")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_plan_dto_proxy_serde_roundtrip() {
        let plan = UserPlanDto {
            name: "test_plan".to_string(),
            proxy: Some(ProxyType::Reverse(Some(ClusterFlags::Live | ClusterFlags::Vod))),
            ..Default::default()
        };

        let serialized = serde_json::to_string(&plan).expect("should serialize");
        let deserialized: UserPlanDto = serde_json::from_str(&serialized).expect("should deserialize");
        assert_eq!(plan, deserialized);

        let plan_redirect = UserPlanDto {
            name: "redirect_plan".to_string(),
            proxy: Some(ProxyType::Redirect),
            ..Default::default()
        };
        let serialized_red = serde_json::to_string(&plan_redirect).expect("should serialize");
        let deserialized_red: UserPlanDto = serde_json::from_str(&serialized_red).expect("should deserialize");
        assert_eq!(plan_redirect, deserialized_red);

        let plan_none = UserPlanDto {
            name: "test_none".to_string(),
            proxy: None,
            ..Default::default()
        };
        let serialized_none = serde_json::to_string(&plan_none).expect("should serialize");
        assert!(!serialized_none.contains("\"proxy\""));
        let deserialized_none: UserPlanDto = serde_json::from_str(&serialized_none).expect("should deserialize");
        assert_eq!(plan_none, deserialized_none);
    }
}
