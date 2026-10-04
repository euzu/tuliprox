use tuliprox_repository::token_revocations::TokenRevocations;

/// Sign-in and token revocation.
///
/// Subject identities need no state: a principal's `UserId` is derived from
/// its configured username (`web:<name>` / `api:<name>`).
pub struct AuthState {
    /// Backoff for repeated failed sign-ins.
    ///
    /// `/auth/token` used to answer 401 and forget, so a password list could
    /// be worked against it as fast as argon2 allows.
    pub login_throttle: crate::auth::LoginThrottle,
    /// Revocation watermarks for already-issued tokens.
    ///
    /// The tokens this server mints are stateless, so nothing could take one
    /// back: a leak stayed valid until it expired.
    pub token_revocations: TokenRevocations,
}

impl AuthState {
    pub fn new(token_revocations: TokenRevocations) -> Self {
        Self { login_throttle: crate::auth::LoginThrottle::new(), token_revocations }
    }

    /// An empty, unpersisted revocation list.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self { Self::new(TokenRevocations::empty(std::path::PathBuf::new())) }
}
