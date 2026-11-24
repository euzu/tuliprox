use crate::model::{macros, AppConfig, Config, UserPlan};
use arc_swap::{access::Access, ArcSwap};
use chrono::Local;
use log::{debug, error, warn};
use shared::{
    foundation::{get_filter, Filter, FilterBuilder},
    model::{
        permission::{Permission, PermissionSet},
        Claims, ClusterFlags, NetworkAccessDto, PlayerFeaturesDto, PlayerRecordingLevel, ProxyType,
        ProxyUserCredentialsDto, ProxyUserStatus, RoleSet, TargetUserDto, UserId, XtreamCluster,
        CURRENT_PERMISSION_SCHEMA_VERSION,
    },
};
use std::{collections::HashMap, sync::Arc};
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
    pub fn is_empty(&self) -> bool { self.allowed_countries.is_empty() && self.allowed_networks.is_empty() }
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
        let allowed_countries =
            if instance.allowed_countries.is_empty() { None } else { Some(instance.allowed_countries.clone()) };
        let allowed_networks = if instance.allowed_networks.is_empty() {
            None
        } else {
            Some(instance.allowed_networks.iter().map(std::string::ToString::to_string).collect())
        };
        Self { allowed_countries, allowed_networks }
    }
}

/// Resolved player feature flags and limits for an API user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct PlayerFeatures {
    pub source_switch: bool,
    pub joinable_streams: bool,
    pub recording: PlayerRecordingLevel,
    pub recording_quota_bytes: Option<u64>,
    pub max_concurrent_recordings: Option<u32>,
    pub max_recording_rules: u32,
    pub free_shared_joins: bool,
    pub max_free_shared_joins: u32,
    pub watch_progress: bool,
    pub event_stream: bool,
    pub api_enabled: Option<bool>,
}

impl Default for PlayerFeatures {
    fn default() -> Self {
        Self {
            source_switch: false,
            joinable_streams: false,
            recording: PlayerRecordingLevel::None,
            recording_quota_bytes: None,
            max_concurrent_recordings: None,
            max_recording_rules: 10,
            free_shared_joins: false,
            max_free_shared_joins: 1,
            watch_progress: false,
            event_stream: false,
            api_enabled: None,
        }
    }
}

/// Compute permissions granted to an API user by their current player features.
#[must_use]
pub fn api_user_permissions(features: &PlayerFeatures) -> PermissionSet {
    match features.recording {
        PlayerRecordingLevel::None => PermissionSet::new(),
        PlayerRecordingLevel::Read => Permission::RecordingRead.into(),
        PlayerRecordingLevel::Private => {
            Permission::RecordingRead
                | Permission::RecordingCreate
                | Permission::RecordingManage
                | Permission::RecordingDelete
        }
        PlayerRecordingLevel::Shared => {
            Permission::RecordingRead
                | Permission::RecordingCreate
                | Permission::RecordingManage
                | Permission::RecordingDelete
                | Permission::RecordingShare
        }
    }
}

/// Claims that describe an API user the way a bearer token would.
///
/// The recording visibility rule is shared with the authenticated routes, so an
/// API user is described by their API subject, their effective recording
/// permissions and the current schema version.
#[must_use]
pub fn api_user_claims(user: &ProxyUserCredentials) -> Claims {
    Claims {
        username: user.username.clone(),
        iss: "tuliprox".to_string(),
        iat: 0,
        exp: i64::MAX,
        roles: RoleSet::new(),
        permissions: api_user_permissions(&user.player_features),
        pwd_version: 0,
        subject_id: Some(UserId::api(&user.username)),
        permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
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
    /// Effective player features resolved from user and plan settings.
    pub player_features: PlayerFeatures,
    /// Raw configured player features before plan resolution.
    pub raw_player_features: Option<PlayerFeaturesDto>,
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
            player_features: PlayerFeatures::default(),
            raw_player_features: dto.player_features.clone(),
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
            player_features: instance.raw_player_features.clone(),
        }
    }
}

/// Synthetic user for internal recording playback. Its HLS URLs carry this
/// identity, so follow-up requests stay on the local listener.
pub const RECORDING_PROXY_USERNAME: &str = "recording_user";

impl ProxyUserCredentials {
    pub fn is_recording_proxy_user(&self) -> bool { self.t_is_api_user && self.username == RECORDING_PROXY_USERNAME }

    pub fn player_api_enabled(&self) -> bool { self.player_features.api_enabled.unwrap_or(self.ui_enabled) }

    /// Fill unset capability values from the referenced plan and compile the
    /// combined content filter. Idempotent; call after any load/conversion.
    pub fn resolve_plan(&mut self, plans: &HashMap<String, Arc<UserPlan>>) {
        self.resolve_plan_with_templates(plans, None);
    }

    pub fn resolve_plan_with_templates(
        &mut self,
        plans: &HashMap<String, Arc<UserPlan>>,
        templates: Option<&[shared::model::PatternTemplate]>,
    ) {
        let plan = self.plan.as_ref().and_then(|name| plans.get(name));
        self.t_has_unresolved_plan = self.plan.is_some() && plan.is_none();
        if self.t_has_unresolved_plan {
            error!(
                "Unknown user plan {:?} for user {}; access is denied until the plan exists or the reference is removed",
                self.plan, self.username
            );
        }
        self.output_clusters =
            self.raw_output_clusters.or_else(|| plan.and_then(|p| p.output_clusters)).unwrap_or_else(ClusterFlags::all);
        if let Some(plan_proxy) = plan.and_then(|p| p.proxy) {
            self.proxy = self.raw_proxy.unwrap_or(plan_proxy);
        }
        self.max_connections =
            if self.raw_max_connections > 0 { self.raw_max_connections } else { plan.map_or(0, |p| p.max_connections) };
        self.soft_connections = if self.raw_soft_connections > 0 {
            self.raw_soft_connections
        } else {
            plan.map_or(0, |p| p.soft_connections)
        };

        let user_features = self.raw_player_features.as_ref();
        let plan_features = plan.and_then(|p| p.player_features.as_ref());
        // A user value wins field by field; an unset field inherits the plan value.
        macro_rules! pick {
            ($field:ident) => {
                user_features.and_then(|f| f.$field).or_else(|| plan_features.and_then(|f| f.$field))
            };
        }
        let default = PlayerFeatures::default();
        let recording = pick!(recording).unwrap_or(default.recording);
        let source_switch = pick!(source_switch).unwrap_or(default.source_switch);
        let joinable_streams = pick!(joinable_streams).unwrap_or(default.joinable_streams);
        let free_shared_joins = pick!(free_shared_joins).unwrap_or(default.free_shared_joins);
        let recording_quota_bytes = user_features
            .and_then(|f| f.recording_quota.as_ref())
            .or_else(|| plan_features.and_then(|f| f.recording_quota.as_ref()))
            .and_then(|quota| shared::model::parse_positive_size(quota.as_str()));
        let max_concurrent_recordings = pick!(max_concurrent_recordings).or(matches!(
            recording,
            PlayerRecordingLevel::Private | PlayerRecordingLevel::Shared
        )
        .then_some(1));
        let max_free_shared_joins = pick!(max_free_shared_joins).unwrap_or(default.max_free_shared_joins);
        let max_recording_rules = pick!(max_recording_rules).unwrap_or(default.max_recording_rules);
        let watch_progress = pick!(watch_progress).unwrap_or(default.watch_progress);
        let event_stream = pick!(event_stream).unwrap_or(default.event_stream);
        let api_enabled = pick!(api_enabled);

        self.player_features = PlayerFeatures {
            source_switch,
            joinable_streams,
            recording,
            recording_quota_bytes,
            max_concurrent_recordings,
            max_recording_rules,
            free_shared_joins,
            max_free_shared_joins,
            watch_progress,
            event_stream,
            api_enabled,
        };

        let plan_filter = plan.and_then(|p| p.t_filter.as_ref().map(Arc::clone));
        self.t_has_invalid_filter = false;
        let user_filter = self.filter.as_ref().and_then(|raw| match get_filter(raw, templates) {
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
            (Some(plan), Some(user)) => {
                let mut builder = FilterBuilder::new();
                // `plan`/`user` are `Arc<Filter>`; the builder copies each tree into its arena.
                // A copy only fails for a filter with an unresolvable root, which this crate
                // does not produce. The user is denied rather than served an uncombined filter,
                // because the combination is what restricts them.
                if let (Some(plan_node), Some(user_node)) =
                    (builder.append(plan.as_ref()), builder.append(user.as_ref()))
                {
                    let left = builder.group(plan_node);
                    let right = builder.group(user_node);
                    let root = builder.and(left, right);
                    Some(Arc::new(builder.finish(root)))
                } else {
                    error!(
                        "Cannot combine the plan and user filters for user {}; access is denied until this is fixed",
                        self.username
                    );
                    self.t_has_invalid_filter = true;
                    None
                }
            }
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
            return crate::utils::constant_time_eq(tkn.as_bytes(), token.as_bytes());
        }
        false
    }

    pub fn matches(&self, username: &str, password: &str) -> bool {
        self.username.eq(username) && crate::utils::constant_time_eq(self.password.as_bytes(), password.as_bytes())
    }

    #[inline]
    pub fn has_permissions(&self, app_config: &AppConfig) -> bool {
        self.permission_denied_reason(app_config).is_none()
    }

    #[inline]
    pub fn permission_denied(&self, app_config: &AppConfig) -> bool { !self.has_permissions(app_config) }

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

    pub fn allows_cluster(&self, cluster: XtreamCluster) -> bool { self.output_clusters.has_cluster(cluster.into()) }

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
        Self { target: dto.target.clone(), credentials: dto.credentials.iter().map(|c| Arc::new(c.into())).collect() }
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
    fn user_filter_recompiles_with_changed_templates_and_preserves_raw_dto() {
        use shared::model::{PatternTemplate, PlaylistItem, PlaylistItemHeader, TemplateValue};
        let raw = r#"Group = "!groups!""#;
        let dto = ProxyUserCredentialsDto { filter: Some(raw.to_owned()), ..Default::default() };
        let mut user = ProxyUserCredentials::from(&dto);
        let mut template = PatternTemplate {
            name: "groups".to_owned(),
            value: TemplateValue::Single("Sports".to_owned()),
            placeholder: String::new(),
        };
        template.prepare();
        let sports = PlaylistItem { header: PlaylistItemHeader { group: "Sports".into(), ..Default::default() } };
        let news = PlaylistItem { header: PlaylistItemHeader { group: "News".into(), ..Default::default() } };
        user.resolve_plan_with_templates(&HashMap::new(), Some(std::slice::from_ref(&template)));
        assert!(!user.t_has_invalid_filter);
        assert!(user.allows_content(&sports));
        assert!(!user.allows_content(&news));
        template.value = TemplateValue::Single("News".to_owned());
        user.resolve_plan_with_templates(&HashMap::new(), Some(std::slice::from_ref(&template)));
        assert!(!user.allows_content(&sports));
        assert!(user.allows_content(&news));
        let saved = ProxyUserCredentialsDto::from(&user);
        assert_eq!(saved.filter.as_deref(), Some(raw));
        user.resolve_plan_with_templates(&HashMap::new(), None);
        assert!(user.t_has_invalid_filter);
    }

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

    #[test]
    fn player_features_user_overrides_plan() {
        let plan_dto = UserPlanDto {
            name: "plan_override".to_string(),
            player_features: Some(PlayerFeaturesDto {
                source_switch: Some(false),
                joinable_streams: Some(false),
                recording: Some(PlayerRecordingLevel::Read),
                recording_quota: Some(shared::model::ByteSize::new("20GB")),
                max_concurrent_recordings: Some(2),
                max_recording_rules: Some(4),
                free_shared_joins: Some(false),
                max_free_shared_joins: Some(3),
                watch_progress: Some(false),
                event_stream: Some(false),
                api_enabled: None,
            }),
            ..Default::default()
        };
        let mut plans = HashMap::new();
        plans.insert("plan_override".to_string(), Arc::new(UserPlan::from(&plan_dto)));

        let user_dto = ProxyUserCredentialsDto {
            username: "u1".to_string(),
            password: "p".to_string(),
            plan: Some("plan_override".to_string()),
            player_features: Some(PlayerFeaturesDto {
                source_switch: Some(true),
                joinable_streams: Some(true),
                recording: Some(PlayerRecordingLevel::Shared),
                recording_quota: Some(shared::model::ByteSize::new("1GB")),
                max_concurrent_recordings: Some(5),
                max_recording_rules: Some(20),
                free_shared_joins: Some(true),
                max_free_shared_joins: Some(10),
                watch_progress: Some(true),
                event_stream: Some(true),
                api_enabled: None,
            }),
            ..Default::default()
        };
        let mut user = ProxyUserCredentials::from(&user_dto);
        user.resolve_plan(&plans);
        assert_eq!(
            user.player_features,
            PlayerFeatures {
                source_switch: true,
                joinable_streams: true,
                recording: PlayerRecordingLevel::Shared,
                recording_quota_bytes: Some(1_073_741_824),
                max_concurrent_recordings: Some(5),
                max_recording_rules: 20,
                free_shared_joins: true,
                max_free_shared_joins: 10,
                watch_progress: true,
                event_stream: true,
                api_enabled: None,
            }
        );
    }

    #[test]
    fn player_features_plan_used_when_user_unset() {
        let plan_dto = UserPlanDto {
            name: "plan_inherit".to_string(),
            player_features: Some(PlayerFeaturesDto {
                source_switch: Some(true),
                joinable_streams: Some(true),
                recording: Some(PlayerRecordingLevel::Private),
                recording_quota: Some(shared::model::ByteSize::new("20GB")),
                max_concurrent_recordings: Some(4),
                max_recording_rules: Some(15),
                free_shared_joins: Some(true),
                max_free_shared_joins: Some(8),
                watch_progress: Some(true),
                event_stream: Some(true),
                api_enabled: None,
            }),
            ..Default::default()
        };
        let mut plans = HashMap::new();
        plans.insert("plan_inherit".to_string(), Arc::new(UserPlan::from(&plan_dto)));

        let user_dto = ProxyUserCredentialsDto {
            username: "u2".to_string(),
            password: "p".to_string(),
            plan: Some("plan_inherit".to_string()),
            player_features: None,
            ..Default::default()
        };
        let mut user = ProxyUserCredentials::from(&user_dto);
        user.resolve_plan(&plans);
        assert_eq!(
            user.player_features,
            PlayerFeatures {
                source_switch: true,
                joinable_streams: true,
                recording: PlayerRecordingLevel::Private,
                recording_quota_bytes: Some(21_474_836_480),
                max_concurrent_recordings: Some(4),
                max_recording_rules: 15,
                free_shared_joins: true,
                max_free_shared_joins: 8,
                watch_progress: true,
                event_stream: true,
                api_enabled: None,
            }
        );
    }

    #[test]
    fn player_features_default_when_both_unset() {
        let plan_dto = UserPlanDto { name: "plan_empty".to_string(), player_features: None, ..Default::default() };
        let mut plans = HashMap::new();
        plans.insert("plan_empty".to_string(), Arc::new(UserPlan::from(&plan_dto)));

        let user_dto = ProxyUserCredentialsDto {
            username: "u3".to_string(),
            password: "p".to_string(),
            plan: Some("plan_empty".to_string()),
            player_features: None,
            ..Default::default()
        };
        let mut user = ProxyUserCredentials::from(&user_dto);
        user.resolve_plan(&plans);
        assert_eq!(
            user.player_features,
            PlayerFeatures {
                source_switch: false,
                joinable_streams: false,
                recording: PlayerRecordingLevel::None,
                recording_quota_bytes: None,
                max_concurrent_recordings: None,
                max_recording_rules: 10,
                free_shared_joins: false,
                max_free_shared_joins: 1,
                watch_progress: false,
                event_stream: false,
                api_enabled: None,
            }
        );
        assert_eq!(user.player_features, PlayerFeatures::default());
    }

    #[test]
    fn player_features_max_concurrent_recordings_defaults() {
        let test_concurrent = |level: PlayerRecordingLevel, expected: Option<u32>| {
            let u_dto = ProxyUserCredentialsDto {
                username: "u_conc".to_string(),
                password: "p".to_string(),
                player_features: Some(PlayerFeaturesDto {
                    recording: Some(level),
                    max_concurrent_recordings: None,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let mut u = ProxyUserCredentials::from(&u_dto);
            u.resolve_plan(&HashMap::new());
            assert_eq!(u.player_features.max_concurrent_recordings, expected);
        };
        test_concurrent(PlayerRecordingLevel::None, None);
        test_concurrent(PlayerRecordingLevel::Read, None);
        test_concurrent(PlayerRecordingLevel::Private, Some(1));
        test_concurrent(PlayerRecordingLevel::Shared, Some(1));
    }

    #[test]
    fn player_features_field_wise_inheritance_new_fields() {
        let plan_dto = UserPlanDto {
            name: "plan_test".to_string(),
            player_features: Some(PlayerFeaturesDto {
                max_recording_rules: Some(25),
                watch_progress: Some(true),
                event_stream: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut plans = HashMap::new();
        plans.insert("plan_test".to_string(), Arc::new(UserPlan::from(&plan_dto)));

        // User overrides only watch_progress
        let user_dto = ProxyUserCredentialsDto {
            username: "u_override".to_string(),
            password: "p".to_string(),
            plan: Some("plan_test".to_string()),
            player_features: Some(PlayerFeaturesDto { watch_progress: Some(false), ..Default::default() }),
            ..Default::default()
        };
        let mut user = ProxyUserCredentials::from(&user_dto);
        user.resolve_plan(&plans);
        assert_eq!(user.player_features.max_recording_rules, 25);
        assert!(!user.player_features.watch_progress);
        assert!(user.player_features.event_stream);

        // User overrides only max_recording_rules with no plan
        let user_dto2 = ProxyUserCredentialsDto {
            username: "u_no_plan".to_string(),
            password: "p".to_string(),
            player_features: Some(PlayerFeaturesDto { max_recording_rules: Some(5), ..Default::default() }),
            ..Default::default()
        };
        let mut user2 = ProxyUserCredentials::from(&user_dto2);
        user2.resolve_plan(&HashMap::new());
        assert_eq!(user2.player_features.max_recording_rules, 5);
        assert!(!user2.player_features.watch_progress);
        assert!(!user2.player_features.event_stream);
    }

    #[test]
    fn player_features_re_resolution_idempotent() {
        let plan_dto = UserPlanDto {
            name: "plan_inherit".to_string(),
            player_features: Some(PlayerFeaturesDto {
                source_switch: Some(true),
                joinable_streams: Some(true),
                recording: Some(PlayerRecordingLevel::Private),
                max_free_shared_joins: Some(8),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut plans = HashMap::new();
        plans.insert("plan_inherit".to_string(), Arc::new(UserPlan::from(&plan_dto)));

        let user_dto = ProxyUserCredentialsDto {
            username: "u5".to_string(),
            password: "p".to_string(),
            plan: Some("plan_inherit".to_string()),
            player_features: Some(PlayerFeaturesDto {
                source_switch: Some(false),
                recording: Some(PlayerRecordingLevel::Private),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut user = ProxyUserCredentials::from(&user_dto);
        user.resolve_plan(&plans);
        let first_resolved = user.player_features;
        user.resolve_plan(&plans);
        assert_eq!(user.player_features, first_resolved);
        let saved_dto = ProxyUserCredentialsDto::from(&user);
        assert_eq!(saved_dto.player_features, user_dto.player_features);
    }

    #[test]
    fn api_user_permissions_exact_sets() {
        // Level None -> empty
        let none_features = PlayerFeatures { recording: PlayerRecordingLevel::None, ..Default::default() };
        assert_eq!(api_user_permissions(&none_features), PermissionSet::new());

        // Level Read -> RecordingRead
        let read_features = PlayerFeatures { recording: PlayerRecordingLevel::Read, ..Default::default() };
        assert_eq!(api_user_permissions(&read_features), Permission::RecordingRead.into());

        // Level Private -> RecordingRead | RecordingCreate | RecordingManage | RecordingDelete
        let private_features = PlayerFeatures { recording: PlayerRecordingLevel::Private, ..Default::default() };
        let private_expected = Permission::RecordingRead
            | Permission::RecordingCreate
            | Permission::RecordingManage
            | Permission::RecordingDelete;
        assert_eq!(api_user_permissions(&private_features), private_expected);

        // Level Shared -> Private set | RecordingShare
        let shared_features = PlayerFeatures { recording: PlayerRecordingLevel::Shared, ..Default::default() };
        let shared_expected = private_expected | Permission::RecordingShare;
        assert_eq!(api_user_permissions(&shared_features), shared_expected);
    }

    #[test]
    fn player_features_api_enabled_inheritance_and_effective() {
        let test_cases = [
            (None, None, true, None, true),
            (None, None, false, None, false),
            (None, Some(true), false, Some(true), true),
            (None, Some(false), true, Some(false), false),
            (Some(true), Some(false), false, Some(true), true),
            (Some(false), Some(true), true, Some(false), false),
            (Some(false), None, true, Some(false), false),
            (Some(true), None, false, Some(true), true),
        ];

        for (user_api, plan_api, ui_enabled, expected_feature, expected_effective) in test_cases {
            let plan_dto = UserPlanDto {
                name: "test_plan".to_string(),
                player_features: plan_api.map(|api| PlayerFeaturesDto { api_enabled: Some(api), ..Default::default() }),
                ..Default::default()
            };
            let mut plans = HashMap::new();
            plans.insert("test_plan".to_string(), Arc::new(UserPlan::from(&plan_dto)));

            let user_dto = ProxyUserCredentialsDto {
                username: "alice".to_string(),
                password: "pass".to_string(),
                ui_enabled,
                plan: Some("test_plan".to_string()),
                player_features: user_api.map(|api| PlayerFeaturesDto { api_enabled: Some(api), ..Default::default() }),
                ..Default::default()
            };
            let mut user = ProxyUserCredentials::from(&user_dto);
            user.resolve_plan(&plans);

            assert_eq!(user.player_features.api_enabled, expected_feature);
            assert_eq!(user.player_api_enabled(), expected_effective);
        }
    }
}
