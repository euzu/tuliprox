use tuliprox_repository::{identity_registry::IdentityRegistry, token_revocations::TokenRevocations};

/// Sign-in, subject identity and token revocation.
pub struct AuthState {
    /// Stable subject identities for web and API users.
    ///
    /// The registry existed but was never wired in, so token minting derived
    /// the subject from the username - `web:<name>` / `api:<name>` - and a
    /// rename silently reassigned everything the old subject owned.
    pub identity_registry: IdentityRegistry,
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
    pub fn new(identity_registry: IdentityRegistry, token_revocations: TokenRevocations) -> Self {
        Self { identity_registry, login_throttle: crate::auth::LoginThrottle::new(), token_revocations }
    }

    /// Empty, unpersisted registries.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self::new(
            IdentityRegistry::empty(std::path::PathBuf::new()),
            TokenRevocations::empty(std::path::PathBuf::new()),
        )
    }
}
