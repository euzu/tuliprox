use std::cell::RefCell;
use super::request_post;
use crate::error::Error;
use crate::services::requests::set_token;
use futures_signals::signal::Mutable;
use futures_signals::signal::SignalExt;
use shared::model::{TokenResponse, UserCredential};
use std::future::Future;

#[derive(Debug)]
pub struct AuthService {
    username: RefCell<String>,
    auth_channel: Mutable<bool>,
}

const AUTH_PATH: &str = "/auth";

impl AuthService {
    pub fn new() -> Self {
        Self {
            username: RefCell::new(String::new()),
            auth_channel: Mutable::new(false),
        }
    }

    pub fn get_username(&self) -> String {
      self.username.borrow().to_string()
    }

    pub async fn auth_subscribe<F, U>(&self, callback: &mut F)
    where
        U: Future<Output=()>,
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

    pub async fn authenticate(&self, username: String, password: String) -> Result<TokenResponse, Error> {
        let credentials = UserCredential {
            username,
            password,
        };
        match request_post::<UserCredential, TokenResponse>(&format!("{AUTH_PATH}/token"), credentials).await {
            Ok(token) => {
                self.username.replace(token.username.to_string());
                self.auth_channel.set(true);
                set_token(Some(&token.token));
                Ok(token)
            }
            Err(e) => {
                self.username.borrow_mut().clear();
                self.auth_channel.set(false);
                set_token(None);
                Err(e)
            }
        }
    }

    pub async fn refresh(&self) -> Result<TokenResponse, Error> {
        match request_post::<(), TokenResponse>(&format!("{AUTH_PATH}/refresh"), ()).await {
            Ok(token) => {
                self.username.replace(token.username.to_string());
                self.auth_channel.set(true);
                set_token(Some(&token.token));
                Ok(token)
            }
            Err(e) => {
                // self.username.borrow_mut().clear();
                self.auth_channel.set(false);
                set_token(None);
                Err(e)
            }
        }
    }
}

impl Default for AuthService {
    fn default() -> Self {
        Self::new()
    }
}
