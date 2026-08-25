use axum::http::StatusCode;

mod access_token;
mod api_user_context;
mod auth_basic;
mod auth_bearer;
mod authenticator;
mod fingerprint;
mod password;
mod recording_auth;

type Rejection = (StatusCode, &'static str);

pub use self::{
    access_token::*, api_user_context::*, auth_basic::*, auth_bearer::*, authenticator::*, fingerprint::*, password::*,
    recording_auth::*,
};
