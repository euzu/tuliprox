use crate::model::{macros, AppConfig, Config, UserPlan};
use arc_swap::access::Access;
use arc_swap::ArcSwap;
use chrono::Local;
use log::{debug, error, warn};
use shared::foundation::{get_filter, BinaryOperator, Filter};
use shared::model::{
    ClusterFlags, NetworkAccessDto, ProxyType, ProxyUserCredentialsDto, ProxyUserStatus, TargetUserDto, XtreamCluster,
};
use std::collections::HashMap;
use std::sync::Arc;
use zeroize::Zeroize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyUserPermissionDenyReason {
    Expired,
    Disabled,
    Banned,
    ExpiredStatus,
    Inactive,
    UnresolvedPlan,
    InvalidFilter,
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
#[allow(clippy::struct_excessive_bools)]
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
    /// Capability tier name; resolved via `resolve_plan`.
    pub plan: Option<String>,
    /// Raw user-level content filter (DSL); AND-combined with the plan filter.
    pub filter: Option<String>,
    // Raw configured values (None/0 = inherit from plan). The sibling
    // non-raw fields hold the resolved serving values after resolve_plan();
    // persistence (YAML/DB) always writes the raw values so plan edits
    // keep propagating to members.
    pub raw_output_clusters: Option<ClusterFlags>,
    pub raw_max_connections: u32,
    pub raw_soft_connections: u16,
    pub raw_proxy: Option<ProxyType>,
    /// Compiled content filter (plan AND user), applied at serve time.
    pub t_filter: Option<Arc<Filter>>,
    /// True when `plan` references a plan that no longer exists; the user is denied.
    pub t_has_unresolved_plan: bool,
    /// True when the configured user filter failed to compile; the user is denied.
    pub t_has_invalid_filter: bool,
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
            plan: dto.plan.clone(),
            filter: dto.filter.clone(),
            raw_output_clusters: dto.output_clusters,
            raw_max_connections: dto.max_connections,
            raw_soft_connections: dto.soft_connections,
            raw_proxy: if dto.proxy == ProxyType::default() && dto.plan.is_some() { None } else { Some(dto.proxy) },
            t_filter: None,
            t_has_unresolved_plan: false,
            t_has_invalid_filter: false,
        }
    }
}

impl From<&ProxyUserCredentials> for ProxyUserCredentialsDto {
    fn from(instance: &ProxyUserCredentials) -> Self {
        Self {
            username: instance.username.clone(),
            password: instance.password.clone(),
            token: instance.token.clone(),
            proxy: instance.raw_proxy.unwrap_or(instance.proxy),
            server: instance.server.clone(),
            epg_timeshift: instance.epg_timeshift.clone(),
            epg_request_timeshift: instance.epg_request_timeshift.clone(),
            created_at: instance.created_at,
            exp_date: instance.exp_date,
            // Persist raw (pre-plan-resolution) values so plan edits keep propagating.
            max_connections: instance.raw_max_connections,
            status: instance.status,
            output_clusters: instance.raw_output_clusters.filter(|flags| !flags.is_all()),
            ui_enabled: instance.ui_enabled,
            comment: instance.comment.clone(),
            priority: instance.priority,
            soft_connections: instance.raw_soft_connections,
            soft_priority: instance.soft_priority,
            network_access: instance.network_access.as_ref().map(NetworkAccessDto::from),
            plan: instance.plan.clone(),
            filter: instance.filter.clone(),
        }
    }
}

impl ProxyUserCredentials {
    /// Fill unset capability values from the referenced plan and compile the
    /// combined content filter. Idempotent; call after any load/conversion.
    pub fn resolve_plan(&mut self, plans: &HashMap<String, Arc<UserPlan>>) {
        let plan = self.plan.as_ref().and_then(|name| plans.get(name));
        self.t_has_unresolved_plan = self.plan.is_some() && plan.is_none();
        if self.t_has_unresolved_plan {
            error!(
                "Unknown user plan {:?} for user {}; access is denied until the plan exists or the reference is removed",
                self.plan, self.username
            );
        }
        self.output_clusters = self
            .raw_output_clusters
            .or_else(|| plan.and_then(|p| p.output_clusters))
            .unwrap_or_else(ClusterFlags::all);
        if let Some(plan_proxy) = plan.and_then(|p| p.proxy) {
            self.proxy = self.raw_proxy.unwrap_or(plan_proxy);
        }
        self.max_connections = if self.raw_max_connections > 0 {
            self.raw_max_connections
        } else {
            plan.map_or(0, |p| p.max_connections)
        };
        self.soft_connections = if self.raw_soft_connections > 0 {
            self.raw_soft_connections
        } else {
            plan.map_or(0, |p| p.soft_connections)
        };

        let plan_filter = plan.and_then(|p| p.t_filter.as_ref().map(Arc::clone));
        self.t_has_invalid_filter = false;
        let user_filter = self.filter.as_ref().and_then(|raw| match get_filter(raw, None) {
            Ok(filter) => Some(Arc::new(filter)),
            Err(err) => {
                error!(
                    "Invalid filter for user {}; access is denied until the filter is fixed or removed: {err}",
                    self.username
                );
                self.t_has_invalid_filter = true;
                None
            }
        });
        self.t_filter = match (plan_filter, user_filter) {
            // Group both sides so OR expressions keep their intended precedence.
            (Some(plan), Some(user)) => Some(Arc::new(Filter::BinaryExpression(
                Box::new(Filter::Group(Box::new((*plan).clone()))),
                BinaryOperator::And,
                Box::new(Filter::Group(Box::new((*user).clone()))),
            ))),
            (Some(filter), None) | (None, Some(filter)) => Some(filter),
            (None, None) => None,
        };
    }

    /// True when the compiled content filter (plan AND user) permits this item.
    /// Users without a filter see everything.
    pub fn allows_content(&self, pli: &shared::model::PlaylistItem) -> bool {
        self.t_filter.as_ref().is_none_or(|filter| {
            let provider = shared::foundation::ValueProvider { pli, match_as_ascii: false };
            filter.filter(&provider)
        })
    }

    pub fn matches_token(&self, token: &str) -> bool {
        if let Some(tkn) = &self.token {
            return crate::auth::constant_time_eq(tkn.as_bytes(), token.as_bytes());
        }
        false
    }

    pub fn matches(&self, username: &str, password: &str) -> bool {
        self.username.eq(username) && crate::auth::constant_time_eq(self.password.as_bytes(), password.as_bytes())
    }

    #[inline]
    pub fn has_permissions(&self, app_config: &AppConfig) -> bool {
        self.permission_denied_reason(app_config).is_none()
    }

    #[inline]
    pub fn permission_denied(&self, app_config: &AppConfig) -> bool {
        !self.has_permissions(app_config)
    }

    pub fn permission_denied_reason(&self, app_config: &AppConfig) -> Option<ProxyUserPermissionDenyReason> {
        // A plan reference that cannot be resolved must never fall back to
        // default clusters, unlimited connections and no filter.
        if self.t_has_unresolved_plan {
            debug!("User access denied, unresolved plan {:?}: {}", self.plan, self.username);
            return Some(ProxyUserPermissionDenyReason::UnresolvedPlan);
        }
        // A filter that fails to compile must deny instead of serving unfiltered content.
        if self.t_has_invalid_filter {
            debug!("User access denied, invalid filter: {}", self.username);
            return Some(ProxyUserPermissionDenyReason::InvalidFilter);
        }
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_config.config);
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::UserPlanDto;

    #[test]
    fn test_resolve_plan_inherits_proxy() {
        let plan_dto = UserPlanDto {
            name: "reverse_plan".to_string(),
            proxy: Some(ProxyType::Reverse(Some(ClusterFlags::Live))),
            max_connections: 3,
            ..Default::default()
        };
        let plan = Arc::new(UserPlan::from(&plan_dto));
        let mut plans = HashMap::new();
        plans.insert("reverse_plan".to_string(), plan);

        // User with plan, default redirect proxy (inherited from plan)
        let user_dto = ProxyUserCredentialsDto {
            username: "alice".to_string(),
            password: "123".to_string(),
            plan: Some("reverse_plan".to_string()),
            proxy: ProxyType::Redirect,
            ..Default::default()
        };
        let mut user = ProxyUserCredentials::from(&user_dto);
        assert_eq!(user.proxy, ProxyType::Redirect);
        user.resolve_plan(&plans);
        assert_eq!(user.proxy, ProxyType::Reverse(Some(ClusterFlags::Live)));
        assert_eq!(user.max_connections, 3);

        // User with explicit reverse proxy override
        let user_dto2 = ProxyUserCredentialsDto {
            username: "bob".to_string(),
            password: "456".to_string(),
            plan: Some("reverse_plan".to_string()),
            proxy: ProxyType::Reverse(Some(ClusterFlags::Vod)),
            ..Default::default()
        };
        let mut user2 = ProxyUserCredentials::from(&user_dto2);
        user2.resolve_plan(&plans);
        assert_eq!(user2.proxy, ProxyType::Reverse(Some(ClusterFlags::Vod)));
    }
}
