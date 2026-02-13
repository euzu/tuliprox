use super::{check_dummy_token, get_base_href, request_post, set_token};
use crate::error::{Error, Error::Unauthorized};
use base64::{engine::general_purpose, Engine as _};
use futures_signals::signal::Mutable;
use futures_signals::signal::SignalExt;
use log::warn;
use shared::model::{Claims, TokenResponse, UserCredential, ROLE_ADMIN, ROLE_USER, TOKEN_NO_AUTH};
use shared::utils::{concat_path, concat_path_leading_slash};
use std::cell::RefCell;
use std::future::Future;

fn decode_jwt_payload(token: &str) -> Option<Claims> {
    let payload_enc = token.split('.').nth(1)?;
    let payload_bytes = general_purpose::URL_SAFE_NO_PAD.decode(payload_enc).ok()?;
    serde_json::from_slice::<Claims>(&payload_bytes).ok()
}

pub struct AuthService {
    auth_path: String,
    username: RefCell<String>,
    roles: RefCell<Vec<String>>,
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
        }
    }

    pub fn get_username(&self) -> String {
        self.username.borrow().to_string()
    }
    pub fn is_admin(&self) -> bool {
        self.roles.borrow().iter().any(|r| r == ROLE_ADMIN)
    }

    pub fn is_user(&self) -> bool {
        self.roles.borrow().iter().any(|r| r == ROLE_USER)
    }

    pub fn is_authenticated(&self) -> bool {
        self.auth_channel.get()
    }

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

    pub async fn authenticate(
        &self,
        username: String,
        password: String,
    ) -> Result<TokenResponse, Error> {
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
                self.auth_channel.set(true);
                set_token(Some(&token.token));
                self.handle_token(&token.token);
                Ok(token)
            }
            _ => self.unauthorized(),
        }
    }

    pub async fn refresh(&self) -> Result<TokenResponse, Error> {
        check_dummy_token();
        match request_post::<(), TokenResponse>(
            &concat_path(&self.auth_path, "refresh"),
            (),
            None,
            None,
        )
        .await
        {
            Ok(Some(token)) => {
                self.username.replace(token.username.to_string());
                self.auth_channel.set(true);
                set_token(Some(&token.token));
                self.handle_token(&token.token);
                Ok(token)
            }
            _ => self.unauthorized(),
        }
    }

    fn handle_token(&self, token: &str) {
        let mut roles = self.roles.borrow_mut();
        roles.clear();

        if token == TOKEN_NO_AUTH {
            roles.push(ROLE_ADMIN.to_string());
        }

        if let Some(claims) = decode_jwt_payload(token) {
            for role in claims.roles.iter() {
                roles.push(role.clone());
            }
        } else {
            warn!("no claims");
        }
    }
}

impl Default for AuthService {
    fn default() -> Self {
        Self::new()
    }
}
