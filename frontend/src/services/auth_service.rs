use super::{check_dummy_token, get_base_href, request_post, set_token};
use crate::{
    error::{Error, Error::Unauthorized},
    model::WebConfig,
};
use base64::{engine::general_purpose, Engine as _};
use futures_signals::signal::{Mutable, SignalExt};
use log::warn;
use shared::{
    model::{
        permission::{Permission, PermissionSet, PERM_ALL},
        Claims, TokenResponse, UserCredential, ROLE_ADMIN, ROLE_API_USER, TOKEN_NO_AUTH,
    },
    utils::{concat_path, concat_path_leading_slash},
};
use std::{cell::RefCell, future::Future};

fn decode_jwt_payload(token: &str) -> Option<Claims> {
    let payload_enc = token.split('.').nth(1)?;
    let payload_bytes = general_purpose::URL_SAFE_NO_PAD.decode(payload_enc).ok()?;
    serde_json::from_slice::<Claims>(&payload_bytes).ok()
}

fn resolve_auth_path(config: &WebConfig) -> String {
    let auth_url = config.api.auth_url.trim();
    if !auth_url.is_empty() {
        return auth_url.to_string();
    }

    let base_href = get_base_href();
    concat_path_leading_slash(&base_href, "auth")
}

pub struct AuthService {
    auth_path: String,
    username: RefCell<String>,
    roles: RefCell<Vec<String>>,
    permissions: RefCell<PermissionSet>,
    token_exp: RefCell<Option<i64>>,
    auth_channel: Mutable<bool>,
}

impl AuthService {
    pub fn new(config: &WebConfig) -> Self {
        Self {
            auth_path: resolve_auth_path(config),
            username: RefCell::new(String::new()),
            auth_channel: Mutable::new(false),
            roles: RefCell::new(vec![]),
            permissions: RefCell::new(PermissionSet::new()),
            token_exp: RefCell::new(None),
        }
    }

    pub fn get_username(&self) -> String { self.username.borrow().to_string() }
    pub fn is_admin(&self) -> bool { self.roles.borrow().iter().any(|r| r == ROLE_ADMIN) }

    pub fn is_api_user(&self) -> bool { self.roles.borrow().iter().any(|r| r == ROLE_API_USER) }

    pub fn has_permission(&self, permission: Permission) -> bool {
        self.is_admin() || self.permissions.borrow().contains(permission)
    }

    pub fn has_all_permissions(&self, permissions: PermissionSet) -> bool {
        self.is_admin() || self.permissions.borrow().contains_all(&permissions)
    }

    pub fn has_any_permissions(&self, permissions: PermissionSet) -> bool {
        self.is_admin() || self.permissions.borrow().contains_any(&permissions)
    }

    pub fn is_authenticated(&self) -> bool { self.auth_channel.get() }

    pub fn token_exp_timestamp(&self) -> Option<i64> { *self.token_exp.borrow() }

    pub async fn auth_subscribe<F, U>(&self, callback: &mut F)
    where
        U: Future<Output = ()>,
        F: FnMut(bool) -> U,
    {
        let fut = self.auth_channel.signal_cloned().for_each(callback);
        fut.await;
    }

    fn reset_auth_state(&self) {
        self.username.borrow_mut().clear();
        self.roles.borrow_mut().clear();
        *self.permissions.borrow_mut() = PermissionSet::new();
        *self.token_exp.borrow_mut() = None;
        self.auth_channel.set(false);
    }

    pub fn logout(&self) {
        self.reset_auth_state();
        set_token(None);
    }

    fn unauthorized(&self) -> Result<TokenResponse, Error> {
        self.reset_auth_state();
        set_token(None);
        Err(Unauthorized)
    }

    pub async fn authenticate(&self, username: String, password: String) -> Result<TokenResponse, Error> {
        let credentials = UserCredential { username, password };
        match request_post::<UserCredential, TokenResponse>(
            &concat_path(&self.auth_path, "token"),
            credentials,
            None,
            None,
        )
        .await
        {
            Ok(Some(token)) => {
                self.username.replace(token.username.clone());
                self.handle_token(&token.token);
                set_token(Some(&token.token));
                self.auth_channel.set(true);
                Ok(token)
            }
            _ => self.unauthorized(),
        }
    }

    pub async fn refresh(&self) -> Result<TokenResponse, Error> {
        check_dummy_token();
        match request_post::<(), TokenResponse>(&concat_path(&self.auth_path, "refresh"), (), None, None).await {
            Ok(Some(token)) => {
                self.username.replace(token.username.clone());
                self.handle_token(&token.token);
                set_token(Some(&token.token));
                self.auth_channel.set(true);
                Ok(token)
            }
            _ => self.unauthorized(),
        }
    }

    fn handle_token(&self, token: &str) {
        let mut roles = self.roles.borrow_mut();
        roles.clear();
        let mut permissions = self.permissions.borrow_mut();
        *permissions = PermissionSet::new();
        *self.token_exp.borrow_mut() = None;

        if token == TOKEN_NO_AUTH {
            roles.push(ROLE_ADMIN.to_string());
            *permissions = PERM_ALL;
            return;
        }

        if let Some(claims) = decode_jwt_payload(token) {
            for role in claims.roles.names() {
                roles.push(role.to_string());
            }
            *permissions = claims.permissions;
            *self.token_exp.borrow_mut() = Some(claims.exp);
        } else {
            warn!("no claims");
        }
    }
}

impl Default for AuthService {
    fn default() -> Self { Self::new(&WebConfig::default()) }
}

#[cfg(test)]
mod tests {
    use super::resolve_auth_path;
    use crate::model::{ApiConfig, WebConfig};

    #[test]
    fn resolve_auth_path_prefers_configured_auth_url() {
        let config = WebConfig {
            api: ApiConfig { api_url: "/tuli/api/v1/".to_string(), auth_url: "/tuli/auth".to_string() },
            ..WebConfig::default()
        };

        assert_eq!(resolve_auth_path(&config), "/tuli/auth");
    }
}
