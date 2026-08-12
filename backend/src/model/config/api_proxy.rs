use crate::model::{macros, AppConfig, Config, ProxyUserCredentials, TargetUser};
use crate::repository::{backup_api_user_db_file, get_api_user_db_path, load_api_user, merge_api_user};
use crate::utils;
use crate::utils::file_exists_async;
use arc_swap::access::Access;
use arc_swap::ArcSwap;
use log::{debug, error};
use shared::foundation::{get_filter, Filter};
use shared::model::{
    ApiProxyConfigDto, ApiProxyServerInfoDto, ClusterFlags, ConfigPaths, ProxyType, TargetUserDto, UserPlanDto,
};
use std::cmp::PartialEq;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::Arc;

const API_USER: &str = "api";
const TEST_USER: &str = "test";

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
            name: dto.name.clone(),
            protocol: dto.protocol.clone(),
            host: dto.host.clone(),
            port: dto.port.clone(),
            timezone: dto.timezone.clone(),
            message: dto.message.clone(),
            path: dto.path.clone(),
        }
    }
}

impl From<&ApiProxyServerInfo> for ApiProxyServerInfoDto {
    fn from(instance: &ApiProxyServerInfo) -> Self {
        Self {
            name: instance.name.clone(),
            protocol: instance.protocol.clone(),
            host: instance.host.clone(),
            port: instance.port.clone(),
            timezone: instance.timezone.clone(),
            message: instance.message.clone(),
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
            Some(path) => format!("{base_url}/{}", path.trim_matches('/')),
        }
    }
}

#[derive(Debug, Clone)]
pub struct UserPlan {
    pub name: String,
    pub output_clusters: Option<ClusterFlags>,
    pub proxy: Option<ProxyType>,
    pub max_connections: u32,
    pub soft_connections: u16,
    pub filter: Option<String>,
    pub comment: Option<String>,
    pub t_filter: Option<Arc<Filter>>,
}

macros::from_impl!(UserPlan);
impl From<&UserPlanDto> for UserPlan {
    fn from(dto: &UserPlanDto) -> Self {
        // The DTO prepare() already validated the filter; a failure here means
        // an unprepared DTO, so log and serve without the plan filter.
        let t_filter = dto.filter.as_ref().and_then(|raw| match get_filter(raw, None) {
            Ok(filter) => Some(Arc::new(filter)),
            Err(err) => {
                error!("Invalid filter in user plan {}: {err}", dto.name);
                None
            }
        });
        Self {
            name: dto.name.clone(),
            output_clusters: dto.output_clusters,
            proxy: dto.proxy,
            max_connections: dto.max_connections,
            soft_connections: dto.soft_connections,
            filter: dto.filter.clone(),
            comment: dto.comment.clone(),
            t_filter,
        }
    }
}

impl From<&UserPlan> for UserPlanDto {
    fn from(instance: &UserPlan) -> Self {
        Self {
            name: instance.name.clone(),
            output_clusters: instance.output_clusters,
            proxy: instance.proxy,
            max_connections: instance.max_connections,
            soft_connections: instance.soft_connections,
            filter: instance.filter.clone(),
            comment: instance.comment.clone(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ApiProxyConfig {
    pub server: Vec<ApiProxyServerInfo>,
    pub plans: Vec<Arc<UserPlan>>,
    pub user: Vec<TargetUser>,
    pub use_user_db: bool,
    /// HTTP status code for auth failures. 0 means default (403).
    pub auth_error_status: u16,
}

macros::from_impl!(ApiProxyConfig);
impl From<&ApiProxyConfigDto> for ApiProxyConfig {
    fn from(dto: &ApiProxyConfigDto) -> Self {
        // Plans live in plans.yml now and are injected via `set_plans` after load.
        let plan_map: HashMap<String, Arc<UserPlan>> = HashMap::new();
        let user = dto
            .user
            .iter()
            .map(|target_user| TargetUser {
                target: target_user.target.clone(),
                credentials: target_user
                    .credentials
                    .iter()
                    .map(|credentials| {
                        let mut user = ProxyUserCredentials::from(credentials);
                        user.resolve_plan(&plan_map);
                        Arc::new(user)
                    })
                    .collect(),
            })
            .collect();
        Self {
            server: dto.server.iter().map(ApiProxyServerInfo::from).collect(),
            plans: Vec::new(),
            user,
            use_user_db: dto.use_user_db,
            auth_error_status: dto.auth_error_status,
        }
    }
}

impl From<&ApiProxyConfig> for ApiProxyConfigDto {
    fn from(instance: &ApiProxyConfig) -> Self {
        Self {
            server: instance.server.iter().map(ApiProxyServerInfoDto::from).collect(),
            user: instance.user.iter().map(TargetUserDto::from).collect(),
            use_user_db: instance.use_user_db,
            auth_error_status: instance.auth_error_status,
        }
    }
}

fn serialize_api_proxy_config(config: &ApiProxyConfigDto) -> Result<String, String> {
    let mut serialized = String::new();
    let options = serde_saphyr::ser_options! {prefer_block_scalars: false};
    serde_saphyr::to_fmt_writer_with_options(&mut serialized, config, options)
        .map_err(|err| format!("Could not serialize api proxy config: {err}"))?;
    Ok(serialized)
}

async fn api_proxy_file_would_change(api_proxy_file: &str, config: &ApiProxyConfigDto) -> Result<bool, String> {
    let serialized = serialize_api_proxy_config(config)?;
    match tokio::fs::read_to_string(api_proxy_file).await {
        Ok(existing) => Ok(existing != serialized),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(true),
        Err(err) => Err(format!("Could not read api proxy file {api_proxy_file}: {err}")),
    }
}

impl ApiProxyConfig {
    pub fn plan_map(&self) -> HashMap<String, Arc<UserPlan>> {
        self.plans.iter().map(|plan| (plan.name.clone(), Arc::clone(plan))).collect()
    }

    /// Replace the plan set (loaded from plans.yml) and re-resolve every user's
    /// inherited capabilities and combined content filter.
    pub fn set_plans(&mut self, plans: Vec<Arc<UserPlan>>) {
        self.plans = plans;
        let plan_map = self.plan_map();
        for target_user in &mut self.user {
            for credentials in &mut target_user.credentials {
                Arc::make_mut(credentials).resolve_plan(&plan_map);
            }
        }
    }

    /// Re-resolve plan capabilities for users loaded outside the DTO path (user db).
    pub fn resolve_target_users(&self, users: &mut [TargetUser]) {
        let plan_map = self.plan_map();
        for target_user in users {
            for credentials in &mut target_user.credentials {
                Arc::make_mut(credentials).resolve_plan(&plan_map);
            }
        }
    }

    async fn backfill_output_clusters_to_file(&self, cfg: &AppConfig, errors: &mut Vec<String>) {
        if self.user.is_empty() {
            return;
        }
        let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&cfg.paths);
        let api_proxy_file = paths.api_proxy_file_path.as_str();
        let dto = ApiProxyConfigDto::from(self);
        match api_proxy_file_would_change(api_proxy_file, &dto).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(err) => {
                errors.push(err);
                return;
            }
        }
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&cfg.config);
        let backup_dir = config.get_backup_dir();
        if let Err(err) = utils::save_api_proxy(api_proxy_file, backup_dir.as_ref(), &dto).await {
            errors.push(format!("Error saving api proxy file: {err}"));
        }
    }

    // we have the option to store user in the config file or in the user_db
    // When we switch from one to other we need to migrate the existing data.
    /// # Panics
    pub async fn migrate_api_user(&mut self, cfg: &AppConfig, errors: &mut Vec<String>) {
        let paths = <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&cfg.paths);
        let api_proxy_file = paths.api_proxy_file_path.as_str();
        if self.use_user_db {
            // we have user defined in config file.
            // we migrate them to the db and delete them from the config file
            if !&self.user.is_empty() {
                if let Err(err) = merge_api_user(cfg, &self.user).await {
                    errors.push(err.to_string());
                } else {
                    let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&cfg.config);
                    let backup_dir = config.get_backup_dir();
                    self.user = vec![];
                    if let Err(err) =
                        utils::save_api_proxy(api_proxy_file, backup_dir.as_ref(), &ApiProxyConfigDto::from(&*self))
                            .await
                    {
                        errors.push(format!("Error saving api proxy file: {err}"));
                    }
                }
            }
            match load_api_user(cfg).await {
                Ok(users) => {
                    let mut users = users;
                    self.resolve_target_users(&mut users);
                    self.user = users;
                }
                Err(err) => {
                    println!("{err}");
                    errors.push(err.to_string());
                }
            }
        } else {
            self.backfill_output_clusters_to_file(cfg, errors).await;
            let user_db_path = get_api_user_db_path(cfg);
            if file_exists_async(&user_db_path).await {
                // we can't have user defined in db file.
                // we need to load them and save them into the config file
                if let Ok(stored_users) = load_api_user(cfg).await {
                    let mut stored_users = stored_users;
                    self.resolve_target_users(&mut stored_users);
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
                let dto = ApiProxyConfigDto::from(&*self);
                match api_proxy_file_would_change(api_proxy_file, &dto).await {
                    Ok(true) => {
                        if let Err(err) = utils::save_api_proxy(api_proxy_file, backup_dir.as_ref(), &dto).await {
                            errors.push(format!("Error saving api proxy file: {err}"));
                        } else {
                            backup_api_user_db_file(cfg, &user_db_path).await;
                            let _ = tokio::fs::remove_file(&user_db_path).await;
                        }
                    }
                    Ok(false) => {
                        backup_api_user_db_file(cfg, &user_db_path).await;
                        let _ = tokio::fs::remove_file(&user_db_path).await;
                    }
                    Err(err) => errors.push(err),
                }
            }
        }
    }

    pub fn get_target_name(&self, username: &str, password: &str) -> Option<(Arc<ProxyUserCredentials>, String)> {
        for target_user in &self.user {
            if let Some((credentials, target_name)) = target_user.get_target_name(username, password) {
                return Some((Arc::clone(&credentials), target_name.to_string()));
            }
        }
        if log::log_enabled!(log::Level::Debug) && !username.eq(API_USER) {
            debug!("Could not find any target for user {username}");
        }
        None
    }

    pub fn get_target_name_by_token(&self, token: &str) -> Option<(Arc<ProxyUserCredentials>, String)> {
        for target_user in &self.user {
            if let Some((credentials, target_name)) = target_user.get_target_name_by_token(token) {
                return Some((Arc::clone(&credentials), target_name.to_string()));
            }
        }
        None
    }

    pub fn get_user_credentials(&self, username: &str) -> Option<Arc<ProxyUserCredentials>> {
        let result = self
            .user
            .iter()
            .flat_map(|target_user| target_user.credentials.iter())
            .find(|credential| credential.username == username)
            .map(Arc::clone);
        if result.is_none() && (username != TEST_USER && username != API_USER) {
            debug!("Could not find any user credentials for: {username}");
        }
        result
    }
}
