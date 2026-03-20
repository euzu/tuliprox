use super::{check_dummy_token, get_base_href, request_post, set_token};
use crate::error::{Error, Error::Unauthorized};
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

pub struct AuthService {
    auth_path: String,
    username: RefCell<String>,
    roles: RefCell<Vec<String>>,
    permissions: RefCell<PermissionSet>,
    auth_channel: Mutable<bool>,
}

impl AuthService {
    pub fn new() -> Self {
        let base_href = get_base_href();
        Self {
            auth_path: concat_path_leading_slash(&base_href, "auth"),
            username: RefCell::new(String::new()),
            auth_channel: Mutable::new(false),
            roles: RefCell::new(vec![]),
            permissions: RefCell::new(PermissionSet::new()),
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

    pub async fn auth_subscribe<F, U>(&self, callback: &mut F)
    where
        U: Future<Output = ()>,
        F: FnMut(bool) -> U,
    {
        let fut = self.auth_channel.signal_cloned().for_each(callback);
        fut.await
    }

    pub fn logout(&self) {
        set_token(None);
        self.username.borrow_mut().clear();
        self.auth_channel.set(false);
    }

    fn unauthorized(&self) -> Result<TokenResponse, Error> {
        self.auth_channel.set(false);
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
                self.username.replace(token.username.to_string());
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
                self.username.replace(token.username.to_string());
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

        if token == TOKEN_NO_AUTH {
            roles.push(ROLE_ADMIN.to_string());
            *permissions = PERM_ALL;
            return;
        }

        if let Some(claims) = decode_jwt_payload(token) {
            for role in claims.roles.iter() {
                roles.push(role.clone());
            }
            *permissions = claims.permissions;
        } else {
            warn!("no claims");
        }
    }
}

impl Default for AuthService {
    fn default() -> Self { Self::new() }
}
