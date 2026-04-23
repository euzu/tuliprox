use crate::api::model::AppState;
use crate::model::{macros, Config};
use arc_swap::access::Access;
use arc_swap::ArcSwap;
use chrono::Local;
use log::{debug, warn};
use shared::model::{
    ClusterFlags, NetworkAccessDto, ProxyType, ProxyUserCredentialsDto, ProxyUserStatus, TargetUserDto,
    UserConnectionPermission, XtreamCluster,
};
use std::sync::Arc;
use zeroize::Zeroize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyUserPermissionDenyReason {
    Expired,
    Disabled,
    Banned,
    ExpiredStatus,
    Inactive,
}

#[derive(Debug, Clone, Default)]
pub struct NetworkAccess {
    pub allowed_countries: Vec<String>,
    pub allowed_networks: Vec<ipnet::IpNet>,
}

impl NetworkAccess {
    pub fn is_empty(&self) -> bool {
        self.allowed_countries.is_empty() && self.allowed_networks.is_empty()
    }
}

impl From<&NetworkAccessDto> for NetworkAccess {
    fn from(dto: &NetworkAccessDto) -> Self {
        let mut seen_countries = std::collections::HashSet::new();
        let allowed_countries: Vec<String> = dto
            .allowed_countries
            .as_ref()
            .map(|countries| {
                countries
                    .iter()
                    .filter_map(|c| {
                        let upper = c.trim().to_uppercase();
                        if upper.is_empty() {
                            None
                        } else if seen_countries.insert(upper.clone()) {
                            Some(upper)
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let allowed_networks: Vec<ipnet::IpNet> = dto
            .allowed_networks
            .as_ref()
            .map(|networks| {
                networks
                    .iter()
                    .filter_map(|n| match n.trim().parse::<ipnet::IpNet>() {
                        Ok(net) => Some(net),
                        Err(err) => {
                            warn!("Skipping invalid CIDR '{n}': {err}");
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        Self { allowed_countries, allowed_networks }
    }
}

impl From<&NetworkAccess> for NetworkAccessDto {
    fn from(instance: &NetworkAccess) -> Self {
        let allowed_countries = if instance.allowed_countries.is_empty() {
            None
        } else {
            Some(instance.allowed_countries.clone())
        };
        let allowed_networks = if instance.allowed_networks.is_empty() {
            None
        } else {
            Some(instance.allowed_networks.iter().map(std::string::ToString::to_string).collect())
        };
        Self { allowed_countries, allowed_networks }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProxyUserCredentials {
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub epg_request_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: u32,
    pub status: Option<ProxyUserStatus>,
    pub output_clusters: ClusterFlags,
    pub ui_enabled: bool,
    pub comment: Option<String>,
    pub priority: i8,
    pub soft_connections: u16,
    pub soft_priority: i8,
    pub t_is_api_user: bool,
    pub network_access: Option<NetworkAccess>,
}

macros::from_impl!(ProxyUserCredentials);
impl From<&ProxyUserCredentialsDto> for ProxyUserCredentials {
    fn from(dto: &ProxyUserCredentialsDto) -> Self {
        Self {
            username: dto.username.clone(),
            password: dto.password.clone(),
            token: dto.token.clone(),
            proxy: dto.proxy,
            server: dto.server.clone(),
            epg_timeshift: dto.epg_timeshift.clone(),
            epg_request_timeshift: dto.epg_request_timeshift.clone(),
            created_at: dto.created_at,
            exp_date: dto.exp_date,
            max_connections: dto.max_connections,
            status: dto.status,
            output_clusters: dto.output_clusters.unwrap_or_else(ClusterFlags::all),
            ui_enabled: dto.ui_enabled,
            comment: dto.comment.clone(),
            priority: dto.priority,
            soft_connections: dto.soft_connections,
            soft_priority: dto.soft_priority,
            t_is_api_user: false,
            network_access: dto
                .network_access
                .as_ref()
                .map(NetworkAccess::from)
                .filter(|network_access| !network_access.is_empty()),
        }
    }
}

impl From<&ProxyUserCredentials> for ProxyUserCredentialsDto {
    fn from(instance: &ProxyUserCredentials) -> Self {
        Self {
            username: instance.username.clone(),
            password: instance.password.clone(),
            token: instance.token.clone(),
            proxy: instance.proxy,
            server: instance.server.clone(),
            epg_timeshift: instance.epg_timeshift.clone(),
            epg_request_timeshift: instance.epg_request_timeshift.clone(),
            created_at: instance.created_at,
            exp_date: instance.exp_date,
            max_connections: instance.max_connections,
            status: instance.status,
            output_clusters: if instance.output_clusters.is_all() { None } else { Some(instance.output_clusters) },
            ui_enabled: instance.ui_enabled,
            comment: instance.comment.clone(),
            priority: instance.priority,
            soft_connections: instance.soft_connections,
            soft_priority: instance.soft_priority,
            network_access: instance.network_access.as_ref().map(NetworkAccessDto::from),
        }
    }
}

impl ProxyUserCredentials {
    pub fn matches_token(&self, token: &str) -> bool {
        if let Some(tkn) = &self.token {
            return tkn.eq(token);
        }
        false
    }

    pub fn matches(&self, username: &str, password: &str) -> bool {
        self.username.eq(username) && self.password.eq(password)
    }

    #[inline]
    pub fn has_permissions(&self, app_state: &AppState) -> bool {
        self.permission_denied_reason(app_state).is_none()
    }

    #[inline]
    pub fn permission_denied(&self, app_state: &AppState) -> bool {
        !self.has_permissions(app_state)
    }

    pub fn permission_denied_reason(&self, app_state: &AppState) -> Option<ProxyUserPermissionDenyReason> {
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_state.app_config.config);
        if config.user_access_control {
            if let Some(exp_date) = self.exp_date.as_ref() {
                let now = Local::now();
                if (exp_date - now.timestamp()) < 0 {
                    debug!("User access denied, expired: {}", self.username);
                    return Some(ProxyUserPermissionDenyReason::Expired);
                }
            }

            if let Some(status) = &self.status {
                match status {
                    ProxyUserStatus::Disabled => {
                        debug!("User access denied, status disabled: {}", self.username);
                        return Some(ProxyUserPermissionDenyReason::Disabled);
                    }
                    ProxyUserStatus::Banned => {
                        debug!("User access denied, status banned: {}", self.username);
                        return Some(ProxyUserPermissionDenyReason::Banned);
                    }
                    ProxyUserStatus::Expired => {
                        debug!("User access denied, status expired: {}", self.username);
                        return Some(ProxyUserPermissionDenyReason::ExpiredStatus);
                    }
                    ProxyUserStatus::Active | ProxyUserStatus::Trial => {}
                    ProxyUserStatus::Pending => {
                        debug!("User access denied, status pending: {}", self.username);
                        return Some(ProxyUserPermissionDenyReason::Inactive);
                    }
                }
            }
        }
        None
    }

    pub fn allows_cluster(&self, cluster: XtreamCluster) -> bool {
        self.output_clusters.has_cluster(cluster.into())
    }

    pub fn allows_item_type(&self, item_type: shared::model::PlaylistItemType) -> bool {
        self.output_clusters.has_cluster(item_type)
    }

    pub async fn connection_permission(&self, app_state: &AppState) -> UserConnectionPermission {
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_state.app_config.config);
        if (self.max_connections > 0 || self.soft_connections > 0) && config.user_access_control {
            return app_state
                .get_connection_permission(&self.username, self.max_connections, self.soft_connections)
                .await;
        }
        UserConnectionPermission::Allowed
    }
}

impl Drop for ProxyUserCredentials {
    fn drop(&mut self) {
        self.password.zeroize();
        if let Some(mut token) = self.token.take() {
            token.zeroize();
        }
    }
}

#[derive(Debug, Clone)]
pub struct TargetUser {
    pub target: String,
    pub credentials: Vec<Arc<ProxyUserCredentials>>,
}

macros::from_impl!(TargetUser);
impl From<&TargetUserDto> for TargetUser {
    fn from(dto: &TargetUserDto) -> Self {
        Self {
            target: dto.target.clone(),
            credentials: dto.credentials.iter().map(|c| Arc::new(c.into())).collect(),
        }
    }
}

impl From<&TargetUser> for TargetUserDto {
    fn from(instance: &TargetUser) -> Self {
        Self {
            target: instance.target.clone(),
            credentials: instance.credentials.iter().map(|c| c.as_ref().into()).collect(),
        }
    }
}

impl TargetUser {
    pub fn get_target_name(&self, username: &str, password: &str) -> Option<(Arc<ProxyUserCredentials>, &str)> {
        self.credentials
            .iter()
            .find(|c| c.matches(username, password))
            .map(|credentials| (Arc::clone(credentials), self.target.as_str()))
    }
    pub fn get_target_name_by_token(&self, token: &str) -> Option<(Arc<ProxyUserCredentials>, &str)> {
        self.credentials
            .iter()
            .find(|c| c.matches_token(token))
            .map(|credentials| (Arc::clone(credentials), self.target.as_str()))
    }
}
