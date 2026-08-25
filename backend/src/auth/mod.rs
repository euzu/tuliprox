use axum::http::StatusCode;

mod authenticator;
mod password;
mod auth_bearer;
mod auth_basic;
mod access_token;
mod fingerprint;
mod api_user_context;
mod recording_auth;

type Rejection = (StatusCode, &'static str);

pub use self::authenticator::*;
pub use self::access_token::*;
pub use self::password::*;
pub use self::fingerprint::*;
pub use self::auth_basic::*;
pub use self::auth_bearer::*;
pub use self::api_user_context::*;
pub use self::recording_auth::*;