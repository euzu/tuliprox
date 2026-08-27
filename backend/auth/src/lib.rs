use axum::http::StatusCode;

mod access_token;
mod api_user_context;
mod auth_basic;
mod auth_bearer;
mod authenticator;
// Client identity moved to `tuliprox-core`: the connection layer keys on it
// too. Re-exported so auth call sites keep their name.
pub use tuliprox_core::model::Fingerprint;
mod password;
mod recording_auth;

type Rejection = (StatusCode, &'static str);

pub use self::{
    access_token::*, api_user_context::*, auth_basic::*, auth_bearer::*, authenticator::*, password::*,
    recording_auth::*,
};
