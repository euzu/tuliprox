use axum::http::StatusCode;

mod authenticator;
mod password;
mod auth_bearer;
mod auth_basic;
mod access_token;
mod fingerprint;
type Rejection = (StatusCode, &'static str);

pub use self::authenticator::*;
pub use self::access_token::*;
pub use self::password::*;
pub use self::fingerprint::*;
pub use self::auth_basic::*;
pub use self::auth_bearer::*;