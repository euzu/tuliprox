use super::error::safe_stalker_url;
use crate::clock::system_epoch_secs;
use std::{fmt, time::Duration};

/// A successful Stalker handshake. The token is sent as `Authorization: Bearer <token>` on
/// every subsequent API call; portal cookies live in the client's shared cookie jar; the
/// referer is the `Referer` header the portal expects; the `fingerprint_evidence` is a
/// list of `js.*` keys the portal echoed back so that callers can decide which
/// `StalkerPortalFingerprint` the portal matches.
#[derive(Clone)]
pub struct StalkerSession {
    pub token: String,
    pub referer: String,
    pub load_url: String,
    pub fingerprint_evidence: Vec<String>,
    /// Unix-epoch seconds at the moment the handshake response was received. Used by
    /// the client to enforce `STALKER_SESSION_TTL` on the cached session.
    pub created_at_epoch_secs: u64,
}

impl fmt::Debug for StalkerSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let referer = safe_stalker_url(&self.referer);
        let load_url = safe_stalker_url(&self.load_url);
        f.debug_struct("StalkerSession")
            .field("token", &"[redacted]")
            .field("referer", &referer)
            .field("load_url", &load_url)
            .field("fingerprint_evidence", &self.fingerprint_evidence)
            .field("created_at_epoch_secs", &self.created_at_epoch_secs)
            .finish()
    }
}

impl StalkerSession {
    /// Stamp the session with the system clock. Callers that already hold an instant
    /// should use [`Self::new_at`] instead.
    pub fn new(token: String, referer: String, load_url: String) -> Self {
        Self::new_at(token, referer, load_url, system_epoch_secs())
    }

    /// Stamp the session with a caller-supplied instant.
    pub fn new_at(token: String, referer: String, load_url: String, now_epoch_secs: u64) -> Self {
        Self { token, referer, load_url, fingerprint_evidence: Vec::new(), created_at_epoch_secs: now_epoch_secs }
    }

    pub fn with_evidence(mut self, evidence: Vec<String>) -> Self {
        self.fingerprint_evidence = evidence;
        self
    }

    /// True when the cached session is older than `STALKER_SESSION_TTL`. The portal
    /// may invalidate tokens earlier; this is a soft hint to re-handshake, not a
    /// hard expiry.
    pub fn is_stale(&self, ttl: Duration) -> bool { self.is_stale_at(system_epoch_secs(), ttl) }

    /// [`Self::is_stale`] against a caller-supplied instant.
    pub fn is_stale_at(&self, now_epoch_secs: u64, ttl: Duration) -> bool {
        now_epoch_secs.saturating_sub(self.created_at_epoch_secs) >= ttl.as_secs()
    }
}

/// How long a `StalkerSession` should be considered fresh. The portal invalidates tokens
/// aggressively (typically after 5–30 minutes of inactivity); the client treats a session
/// older than this as a hint to re-handshake before issuing any new calls.
pub const STALKER_SESSION_TTL: Duration = Duration::from_mins(15);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_starts_with_empty_evidence() {
        let session = StalkerSession::new(
            "abc".to_string(),
            "http://portal/c/".to_string(),
            "http://portal/server/load.php".to_string(),
        );
        assert!(session.fingerprint_evidence.is_empty());
        assert_eq!(session.token, "abc");
    }

    #[test]
    fn with_evidence_overrides() {
        let session = StalkerSession::new(
            "abc".to_string(),
            "http://portal/c/".to_string(),
            "http://portal/server/load.php".to_string(),
        )
        .with_evidence(vec!["js.keyA".to_string(), "js.keyB".to_string()]);
        assert_eq!(session.fingerprint_evidence.len(), 2);
    }

    #[test]
    fn debug_redacts_url_credentials_and_query_secrets() {
        let session = StalkerSession::new(
            "token-secret".to_string(),
            "https://user:pass@portal.example/c/?mac=00:11:22:33:44:55".to_string(),
            "https://user:pass@portal.example/server/load.php?token=query-secret".to_string(),
        );

        let debug = format!("{session:?}");
        for secret in ["token-secret", "user", "pass", "00:11:22:33:44:55", "query-secret"] {
            assert!(!debug.contains(secret), "debug output leaked {secret}");
        }
        assert!(debug.contains("https://portal.example/c/"));
        assert!(debug.contains("https://portal.example/server/load.php"));
    }

    #[test]
    fn ttl_is_in_fifteen_minute_range() {
        assert!(STALKER_SESSION_TTL.as_secs() >= 60);
        assert!(STALKER_SESSION_TTL.as_secs() <= 3600);
    }

    #[test]
    fn staleness_is_measured_against_the_supplied_instant() {
        // No back-dating of the struct and no sleeping: the instant is a parameter.
        let session = StalkerSession::new_at("t".into(), "r".into(), "l".into(), 10_000);
        assert!(!session.is_stale_at(10_000, STALKER_SESSION_TTL));
        assert!(!session.is_stale_at(10_000 + STALKER_SESSION_TTL.as_secs() - 1, STALKER_SESSION_TTL));
        assert!(session.is_stale_at(10_000 + STALKER_SESSION_TTL.as_secs(), STALKER_SESSION_TTL));
    }

    #[test]
    fn a_clock_that_runs_backwards_does_not_report_a_stale_session() {
        let session = StalkerSession::new_at("t".into(), "r".into(), "l".into(), 10_000);
        assert!(!session.is_stale_at(1, STALKER_SESSION_TTL));
    }
}
