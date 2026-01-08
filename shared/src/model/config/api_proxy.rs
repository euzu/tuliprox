use crate::utils::is_false;
use crate::utils::is_blank_optional_string;
use std::collections::HashSet;
use crate::error::{info_err_res, TuliproxError};
use crate::model::{ProxyUserCredentialsDto};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct TargetUserDto {
    pub target: String,
    pub credentials: Vec<ProxyUserCredentialsDto>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
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
#[serde(deny_unknown_fields)]
pub struct ApiProxyConfigDto {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub server: Vec<ApiProxyServerInfoDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user: Vec<TargetUserDto>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub use_user_db: bool,
}

impl ApiProxyServerInfoDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            return info_err_res!("Server info name is empty ");
        }
        self.protocol = self.protocol.trim().to_string();
        if self.protocol.is_empty() {
            return info_err_res!("protocol cant be empty for api server config");
        }
        self.host = self.host.trim().to_string();
        if self.host.is_empty() {
            return info_err_res!("host cant be empty for api server config");
        }
        if let Some(port) = self.port.as_ref() {
            let port = port.trim().to_string();
            if port.is_empty() {
                self.port = None;
            } else if port.parse::<u16>().is_err() {
                return info_err_res!("invalid port for api server config");
            } else {
                self.port = Some(port);
            }
        }

        self.timezone = self.timezone.trim().to_string();
        if self.timezone.is_empty() {
            self.timezone = "UTC".to_string();
        }
        if self.message.is_empty() {
            self.message = "Welcome to tuliprox".to_string();
        }
        if let Some(path) = &self.path {
            if path.trim().is_empty() {
                self.path = None;
            }
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
    pub fn validate(&mut self) -> bool {
        self.prepare().is_ok()
    }
}

impl ApiProxyConfigDto {

    fn prepare_server_config(&mut self, errors: &mut Vec<String>) {
        let mut name_set = HashSet::new();
        for server in &mut self.server {
            if let Err(err) = server.prepare() {
                errors.push(err.to_string());
            }
            if name_set.contains(server.name.as_str()) {
                errors.push(format!("Non-unique server info name found {}", &server.name));
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
                user.prepare();
                if usernames.contains(&user.username) {
                    errors.push(format!("Non unique username found {}", &user.username));
                } else {
                    usernames.insert(user.username.to_string());
                }
                if let Some(token) = &user.token {
                    if token.is_empty() {
                        user.token = None;
                    } else if tokens.contains(token) {
                        errors.push(format!("Non unique user token found {} for user {}", &user.token.as_ref().map_or_else(String::new, ToString::to_string), &user.username));
                    } else {
                        tokens.insert(token.to_string());
                    }
                }

                if let Some(server_info_name) = &user.server {
                    if !&self.server.iter()
                        .any(|server_info| server_info.name.eq(server_info_name))
                    {
                        errors.push(format!(
                            "No server info with name {} found for user {}",
                            server_info_name, &user.username
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
        if errors.is_empty() {
            Ok(())
        } else {
            info_err_res!("{}", errors.join("\n"))
        }
    }
}