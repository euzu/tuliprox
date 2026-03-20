use axum::http::StatusCode;

mod authenticator;
mod password;
mod auth_bearer;
mod auth_basic;
mod access_token;
mod fingerprint;
type Rejection = (StatusCode, &'static str);

#[macro_export]
macro_rules! permission_layer {
    ($app_state:expr, $permission:expr ) => {
        {
            let app_state = Arc::clone($app_state);
            axum::middleware::from_fn_with_state(app_state, move |state, auth, request, next| {
                require_permission_inner($permission, state, auth, request, next)
            })
        }
    };
}
pub use permission_layer;

pub use self::authenticator::*;
pub use self::access_token::*;
pub use self::password::*;
pub use self::fingerprint::*;
pub use self::auth_basic::*;
pub use self::auth_bearer::*;
