use crate::model::{macros, AppConfig, Config, ProxyUserCredentials, TargetUser};
use crate::repository::user_repository::{backup_api_user_db_file, get_api_user_db_path, load_api_user, merge_api_user};
use crate::utils::{save_api_proxy};
use log::debug;
use std::cmp::PartialEq;
use std::fs;
use std::sync::Arc;
use arc_swap::access::Access;
use arc_swap::ArcSwap;
use shared::model::{ApiProxyConfigDto, ApiProxyServerInfoDto, ConfigPaths, TargetUserDto};
use crate::{utils};

#[derive(Debug, Clone)]
pub struct ApiProxyServerInfo {
    pub name: String,
    pub protocol: String,
    pub host: String,
    pub port: Option<String>,
    pub timezone: String,
    pub message: String,
    pub path: Option<String>,
}

macros::from_impl!(ApiProxyServerInfo);
impl From<&ApiProxyServerInfoDto> for ApiProxyServerInfo {
    fn from(dto: &ApiProxyServerInfoDto) -> Self {
        Self {
            name: dto.name.to_string(),
            protocol: dto.protocol.to_string(),
            host: dto.host.to_string(),
            port: dto.port.clone(),
            timezone: dto.timezone.to_string(),
            message: dto.message.to_string(),
            path: dto.path.clone(),
        }
    }
}

impl From<&ApiProxyServerInfo> for ApiProxyServerInfoDto {
    fn from(instance: &ApiProxyServerInfo) -> Self {
        Self {
            name: instance.name.to_string(),
            protocol: instance.protocol.to_string(),
            host: instance.host.to_string(),
            port: instance.port.clone(),
            timezone: instance.timezone.to_string(),
            message: instance.message.to_string(),
            path: instance.path.clone(),
        }
    }
}

impl ApiProxyServerInfo {
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

#[derive(Debug, Clone)]
pub struct ApiProxyConfig {
    pub server: Vec<ApiProxyServerInfo>,
    pub user: Vec<TargetUser>,
    pub use_user_db: bool,
}

macros::from_impl!(ApiProxyConfig);
impl From<&ApiProxyConfigDto> for ApiProxyConfig {
    fn from(dto: &ApiProxyConfigDto) -> Self {
        Self {
            server: dto.server.iter().map(ApiProxyServerInfo::from).collect(),
            user: dto.user.iter().map(TargetUser::from).collect(),
            use_user_db: dto.use_user_db,
        }
    }
}

impl From<&ApiProxyConfig> for ApiProxyConfigDto {
    fn from(instance: &ApiProxyConfig) -> Self {
        Self {
            server: instance.server.iter().map(ApiProxyServerInfoDto::from).collect(),
            user: instance.user.iter().map(TargetUserDto::from).collect(),
            use_user_db: instance.use_user_db,
        }
    }
}

impl ApiProxyConfig {
    // we have the option to store user in the config file or in the user_db
    // When we switch from one to other we need to migrate the existing data.
    /// # Panics
    pub fn migrate_api_user(&mut self, cfg: &AppConfig, errors: &mut Vec<String>) {
        let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&cfg.paths);
        let api_proxy_file = paths.api_proxy_file_path.as_str();
        if self.use_user_db {
            // we have user defined in config file.
            // we migrate them to the db and delete them from the config file
            if !&self.user.is_empty() {
                if let Err(err) = merge_api_user(cfg, &self.user) {
                    errors.push(err.to_string());
                } else {
                    let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&cfg.config);
                    let backup_dir = config.get_backup_dir();
                    self.user = vec![];
                    if let Err(err) = utils::save_api_proxy(api_proxy_file, backup_dir.as_ref(), &ApiProxyConfigDto::from(&*self)) {
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

                let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&cfg.config);
                let backup_dir = config.get_backup_dir();
                if let Err(err) = save_api_proxy(api_proxy_file, backup_dir.as_ref(), &ApiProxyConfigDto::from(&*self)) {
                    errors.push(format!("Error saving api proxy file: {err}"));
                } else {
                    backup_api_user_db_file(cfg, &user_db_path);
                    let _ = fs::remove_file(&user_db_path);
                }
            }
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
        if result.is_none() && (username != "test" || username != "api") {
            debug!("Could not find any user credentials for: {username}");
        }
        result
    }
}
