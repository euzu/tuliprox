use crate::api::model::AppState;
use crate::model::{macros, Config};
use arc_swap::access::Access;
use arc_swap::ArcSwap;
use chrono::Local;
use log::debug;
use shared::model::{ProxyType, ProxyUserCredentialsDto, ProxyUserStatus, TargetUserDto, UserConnectionPermission};
use std::sync::Arc;
use zeroize::Zeroize;

#[derive(Debug, Clone, Default)]
pub struct ProxyUserCredentials {
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: u32,
    pub status: Option<ProxyUserStatus>,
    pub ui_enabled: bool,
    pub comment: Option<String>,
    pub priority: u8,
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
            created_at: dto.created_at,
            exp_date: dto.exp_date,
            max_connections: dto.max_connections,
            status: dto.status,
            ui_enabled: dto.ui_enabled,
            comment: dto.comment.clone(),
            priority: dto.priority,
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
            created_at: instance.created_at,
            exp_date: instance.exp_date,
            max_connections: instance.max_connections,
            status: instance.status,
            ui_enabled: instance.ui_enabled,
            comment: instance.comment.clone(),
            priority: instance.priority,
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

    pub fn has_permissions(&self, app_state: &AppState) -> bool {
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_state.app_config.config);
        if config.user_access_control {
            if let Some(exp_date) = self.exp_date.as_ref() {
                let now = Local::now();
                if (exp_date - now.timestamp()) < 0 {
                    debug!("User access denied, expired: {}", self.username);
                    return false;
                }
            }

            if let Some(status) = &self.status {
                if !matches!(status, ProxyUserStatus::Active | ProxyUserStatus::Trial) {
                    debug!("User access denied, status invalid: {status} for user: {}",self.username);
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
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_state.app_config.config);
        if self.max_connections > 0 && config.user_access_control {
            return app_state.get_connection_permission(&self.username, self.max_connections).await;
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
    pub credentials: Vec<ProxyUserCredentials>,
}

macros::from_impl!(TargetUser);
impl From<&TargetUserDto> for TargetUser {
    fn from(dto: &TargetUserDto) -> Self {
        Self {
            target: dto.target.clone(),
            credentials: dto.credentials.iter().map(Into::into).collect(),
        }
    }
}

impl From<&TargetUser> for TargetUserDto {
    fn from(instance: &TargetUser) -> Self {
        Self {
            target: instance.target.clone(),
            credentials: instance.credentials.iter().map(Into::into).collect(),
        }
    }
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