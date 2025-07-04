use crate::api::model::app_state::AppState;
use shared::error::{info_err, TuliproxError, TuliproxErrorKind};
use crate::model::{Config};
use crate::repository::user_repository::{backup_api_user_db_file, get_api_user_db_path, load_api_user, merge_api_user};
use crate::utils::{save_api_proxy};
use shared::utils::{default_as_true};
use chrono::Local;
use log::debug;
use std::cmp::PartialEq;
use std::collections::HashSet;
use std::fs;
use shared::model::{ProxyType, ProxyUserStatus, UserConnectionPermission};
use crate::utils;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyUserCredentials {
    pub username: String,
    pub password: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default = "ProxyType::default")]
    pub proxy: ProxyType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epg_timeshift: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp_date: Option<i64>,
    #[serde(default)]
    pub max_connections: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ProxyUserStatus>,
    #[serde(default = "default_as_true")]
    pub ui_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

impl ProxyUserCredentials {
    pub fn prepare(&mut self) {
        self.trim();
    }

    pub fn matches_token(&self, token: &str) -> bool {
        if let Some(tkn) = &self.token {
            return tkn.eq(token);
        }
        false
    }

    pub fn matches(&self, username: &str, password: &str) -> bool {
        self.username.eq(username) && self.password.eq(password)
    }

    pub fn trim(&mut self) {
        self.username = self.username.trim().to_string();
        self.password = self.password.trim().to_string();
        match &self.token {
            None => {}
            Some(tkn) => {
                self.token = Some(tkn.trim().to_string());
            }
        }
    }

    pub fn validate(&self) -> Result<(), TuliproxError> {
        if self.username.is_empty() {
            return Err(TuliproxError::new(TuliproxErrorKind::Info, "Username required".to_string()));
        }
        if self.password.is_empty() {
            return Err(TuliproxError::new(TuliproxErrorKind::Info, "Password required".to_string()));
        }
        Ok(())
    }

    pub fn has_permissions(&self, app_state: &AppState) -> bool {
        if app_state.config.user_access_control {
            if let Some(exp_date) = self.exp_date.as_ref() {
                let now = Local::now();
                if (exp_date - now.timestamp()) < 0 {
                    debug!("User access denied, expired: {}", self.username);
                    return false;
                }
            }

            if let Some(status) = &self.status {
                if !matches!(status, ProxyUserStatus::Active | ProxyUserStatus::Trial) {
                    debug!("User access denied, status invalid: {status} for user: {}", self.username);
                    return false;
                }
            } // NO STATUS SET, ok admins fault, we take this as a valid status
        }
        true
    }

    #[inline]
    pub fn permission_denied(&self, app_state: &AppState) -> bool {
        !self.has_permissions(app_state)
    }

    pub async fn connection_permission(&self, app_state: &AppState) -> UserConnectionPermission {
        if self.max_connections > 0 && app_state.config.user_access_control {
            // we allow requests with max connection reached, but we should block streaming after grace period
            return app_state.get_connection_permission(&self.username, self.max_connections).await;
        }
        UserConnectionPermission::Allowed
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TargetUser {
    pub target: String,
    pub credentials: Vec<ProxyUserCredentials>,
}

impl TargetUser {
    pub fn get_target_name(
        &self,
        username: &str,
        password: &str,
    ) -> Option<(&ProxyUserCredentials, &str)> {
        self.credentials
            .iter()
            .find(|c| c.matches(username, password))
            .map(|credentials| (credentials, self.target.as_str()))
    }
    pub fn get_target_name_by_token(&self, token: &str) -> Option<(&ProxyUserCredentials, &str)> {
        self.credentials
            .iter()
            .find(|c| c.matches_token(token))
            .map(|credentials| (credentials, self.target.as_str()))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiProxyServerInfo {
    pub name: String,
    pub protocol: String,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    pub timezone: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl ApiProxyServerInfo {

   pub fn prepare(&mut self ) -> Result<(), TuliproxError> {
       self.name = self.name.trim().to_string();
       if self.name.is_empty() {
           return Err(info_err!("Server info name is empty ".to_string()));
       }
       self.protocol = self.protocol.trim().to_string();
       if self.protocol.is_empty() {
           return Err(info_err!("protocol cant be empty for api server config".to_string()));
       }
       self.host = self.host.trim().to_string();
       if self.host.is_empty() {
           return Err(info_err!("host cant be empty for api server config".to_string()));
       }
       if let Some(port)= self.port.as_ref() {
           let port = port.trim().to_string();
           if port.is_empty() {
               self.port = None;
           } else if port.parse::<u16>().is_err() {
               return Err(info_err!("invalid port for api server config".to_string()));
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

    pub fn get_base_url(&self) -> String {
        let base_url = if let Some(port) = self.port.as_ref() {
            format!("{}://{}:{port}", self.protocol, self.host)
        } else {
            format!("{}://{}", self.protocol, self.host)
        };

        match &self.path {
            None => base_url,
            Some(path) => format!("{base_url}/{}", path.trim_matches('/'))
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiProxyConfig {
    pub server: Vec<ApiProxyServerInfo>,
    pub user: Vec<TargetUser>,
    #[serde(default)]
    pub use_user_db: bool,
}

impl ApiProxyConfig {
    // we have the option to store user in the config file or in the user_db
    // When we switch from one to other we need to migrate the existing data.
    /// # Panics
    pub fn migrate_api_user(&mut self, cfg: &Config, errors: &mut Vec<String>) {
        if self.use_user_db {
            // we have user defined in config file.
            // we migrate them to the db and delete them from the config file
            if !&self.user.is_empty() {
                if let Err(err) = merge_api_user(cfg, &self.user) {
                    errors.push(err.to_string());
                } else {
                    let api_proxy_file = cfg.t_api_proxy_file_path.as_str();
                    let backup_dir = cfg.backup_dir.as_ref().unwrap().as_str();
                    self.user = vec![];
                    if let Err(err) = utils::save_api_proxy(api_proxy_file, backup_dir, self) {
                        errors.push(format!("Error saving api proxy file: {err}"));
                    }
                }
            }
            match load_api_user(cfg) {
                Ok(users) => {
                    self.user = users;
                }
                Err(err) => {
                    println!("{err}");
                    errors.push(err.to_string());
                }
            }
        } else {
            let user_db_path = get_api_user_db_path(cfg);
            if user_db_path.exists() {
                // we cant have user defined in db file.
                // we need to load them and save them into the config file
                if let Ok(stored_users) = load_api_user(cfg) {
                    for stored_user in stored_users {
                        if let Some(target_user) = self.user.iter_mut().find(|t| t.target == stored_user.target) {
                            for stored_credential in &stored_user.credentials {
                                if !target_user.credentials.iter().any(|c| c.username == stored_credential.username) {
                                    target_user.credentials.push(stored_credential.clone());
                                }
                            }
                        } else {
                            self.user.push(stored_user);
                        }
                    }
                }
                let api_proxy_file = cfg.t_api_proxy_file_path.as_str();
                let backup_dir = cfg.backup_dir.as_ref().unwrap().as_str();
                if let Err(err) = save_api_proxy(api_proxy_file, backup_dir, self) {
                    errors.push(format!("Error saving api proxy file: {err}"));
                } else {
                    backup_api_user_db_file(cfg, &user_db_path);
                    let _ = fs::remove_file(&user_db_path);
                }
            }
        }
    }

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
                        errors.push(format!("Non unique token found {}", &user.username));
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
            Err(info_err!(errors.join("\n")))
        }
    }

    pub fn get_target_name(
        &self,
        username: &str,
        password: &str,
    ) -> Option<(ProxyUserCredentials, String)> {
        for target_user in &self.user {
            if let Some((credentials, target_name)) =
                target_user.get_target_name(username, password)
            {
                return Some((credentials.clone(), target_name.to_string()));
            }
        }
        if log::log_enabled!(log::Level::Debug) && !username.eq("api") {
           debug!("Could not find any target for user {username}");
        }
        None
    }

    pub fn get_target_name_by_token(&self, token: &str) -> Option<(ProxyUserCredentials, String)> {
        for target_user in &self.user {
            if let Some((credentials, target_name)) = target_user.get_target_name_by_token(token) {
                return Some((credentials.clone(), target_name.to_string()));
            }
        }
        None
    }

    pub fn get_user_credentials(&self, username: &str) -> Option<ProxyUserCredentials> {
        let result = self.user.iter()
            .flat_map(|target_user| &target_user.credentials)
            .find(|credential| credential.username == username)
            .cloned();
        if result.is_none() && username != "test" {
            debug!("Could not find any user {username}");
        }
        result
    }
}
