mod access_token;
mod api_user_context;
mod auth_basic;
mod auth_bearer;
mod authenticator;
mod login_throttle;
// Client identity moved to `tuliprox-core`: the connection layer keys on it
// too. Re-exported so auth call sites keep their name.
pub use tuliprox_core::model::Fingerprint;
mod password;
mod recording_auth;

// The single rejection type for every authentication extractor. It lives in
// `tuliprox-core` because `Fingerprint` - which is an extractor too - lives
// there and used to declare its own identical copy.
pub use tuliprox_core::model::{AuthRejection, AuthScheme};

type Rejection = AuthRejection;

pub use self::{
    access_token::*, api_user_context::*, auth_basic::*, auth_bearer::*, authenticator::*, login_throttle::*,
    password::*, recording_auth::*,
};
