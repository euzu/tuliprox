use std::{fmt, time::{Duration, SystemTime, UNIX_EPOCH}};

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
        f.debug_struct("StalkerSession")
            .field("token", &"[redacted]")
            .field("referer", &self.referer)
            .field("load_url", &self.load_url)
            .field("fingerprint_evidence", &self.fingerprint_evidence)
            .field("created_at_epoch_secs", &self.created_at_epoch_secs)
            .finish()
    }
}

impl StalkerSession {
    pub fn new(token: String, referer: String, load_url: String) -> Self {
        Self {
            token,
            referer,
            load_url,
            fingerprint_evidence: Vec::new(),
            created_at_epoch_secs: now_epoch_secs(),
        }
    }

    pub fn with_evidence(mut self, evidence: Vec<String>) -> Self {
        self.fingerprint_evidence = evidence;
        self
    }

    /// True when the cached session is older than `STALKER_SESSION_TTL`. The portal
    /// may invalidate tokens earlier; this is a soft hint to re-handshake, not a
    /// hard expiry.
    pub fn is_stale(&self, ttl: Duration) -> bool {
        let now = now_epoch_secs();
        now.saturating_sub(self.created_at_epoch_secs) >= ttl.as_secs()
    }
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
        let session = StalkerSession::new("abc".to_string(), "http://portal/c/".to_string(), "http://portal/server/load.php".to_string());
        assert!(session.fingerprint_evidence.is_empty());
        assert_eq!(session.token, "abc");
    }

    #[test]
    fn with_evidence_overrides() {
        let session = StalkerSession::new("abc".to_string(), "http://portal/c/".to_string(), "http://portal/server/load.php".to_string())
            .with_evidence(vec!["js.keyA".to_string(), "js.keyB".to_string()]);
        assert_eq!(session.fingerprint_evidence.len(), 2);
    }

    #[test]
    fn ttl_is_in_fifteen_minute_range() {
        assert!(STALKER_SESSION_TTL.as_secs() >= 60);
        assert!(STALKER_SESSION_TTL.as_secs() <= 3600);
    }

    #[test]
    fn fresh_session_is_not_stale() {
        let session = StalkerSession::new("t".into(), "r".into(), "l".into());
        assert!(!session.is_stale(STALKER_SESSION_TTL));
    }

    #[test]
    fn stale_session_is_detected() {
        let mut session = StalkerSession::new("t".into(), "r".into(), "l".into());
        // Pretend the session is one hour old.
        session.created_at_epoch_secs = now_epoch_secs().saturating_sub(3600);
        assert!(session.is_stale(STALKER_SESSION_TTL));
    }
}
