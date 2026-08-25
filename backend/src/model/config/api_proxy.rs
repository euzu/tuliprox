use crate::model::{macros, ProxyUserCredentials, TargetUser};
use log::{debug, error};
use shared::foundation::{get_filter, Filter};
use shared::model::{
    ApiProxyConfigDto, ApiProxyServerInfoDto, ClusterFlags, ProxyType, TargetUserDto, UserPlanDto,
    UserPlanTrialDto,
};
use std::cmp::PartialEq;
use std::collections::HashMap;
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
    pub trial: Option<UserPlanTrialDto>,
    pub comment: Option<String>,
    pub t_filter: Option<Arc<Filter>>,
    pub t_trial_duration_secs: Option<u64>,
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
            trial: dto.trial.clone(),
            comment: dto.comment.clone(),
            t_filter,
            t_trial_duration_secs: dto.trial.as_ref().and_then(UserPlanTrialDto::duration_secs),
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
            trial: instance.trial.clone(),
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
