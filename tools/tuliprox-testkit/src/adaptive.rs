use crate::TestkitError;
use std::{collections::HashMap, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptiveSessionState {
    Prepared,
    Active,
    Preserved,
}

#[derive(Debug)]
struct Session {
    state: AdaptiveSessionState,
    expires_at: tokio::time::Instant,
}

#[derive(Debug, Default)]
pub struct AdaptiveSessionTracker {
    sessions: HashMap<String, Session>,
}

impl AdaptiveSessionTracker {
    pub fn manifest(&mut self, session_group: String, ttl: Duration) -> Result<AdaptiveSessionState, TestkitError> {
        self.reap_expired();
        if self.sessions.contains_key(&session_group) {
            return Err(TestkitError::Protocol("adaptive manifest created a duplicate logical session".to_owned()));
        }
        self.sessions.insert(
            session_group,
            Session { state: AdaptiveSessionState::Prepared, expires_at: tokio::time::Instant::now() + ttl },
        );
        Ok(AdaptiveSessionState::Prepared)
    }

    pub fn segment(&mut self, session_group: &str, ttl: Duration) -> Result<AdaptiveSessionState, TestkitError> {
        let session = self.sessions.get_mut(session_group).ok_or_else(|| {
            TestkitError::Configuration(format!("adaptive segment has no prepared session {session_group}"))
        })?;
        if tokio::time::Instant::now() > session.expires_at {
            return Err(TestkitError::Protocol("adaptive session TTL expired before segment activation".to_owned()));
        }
        session.state = AdaptiveSessionState::Active;
        session.expires_at = tokio::time::Instant::now() + ttl;
        Ok(session.state)
    }

    pub fn preserve(&mut self, session_group: &str, ttl: Duration) -> Result<(), TestkitError> {
        self.reap_expired();
        let session = self.sessions.get_mut(session_group).ok_or_else(|| {
            TestkitError::Configuration(format!("cannot preserve unknown adaptive session {session_group}"))
        })?;
        session.state = AdaptiveSessionState::Preserved;
        session.expires_at = tokio::time::Instant::now() + ttl;
        Ok(())
    }

    pub fn reap_expired(&mut self) {
        self.sessions.retain(|_, session| tokio::time::Instant::now() <= session.expires_at);
    }

    #[must_use]
    pub fn state(&self, session_group: &str) -> Option<AdaptiveSessionState> {
        self.sessions.get(session_group).map(|session| session.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn segment_activates_and_reacquisition_preserves_one_session() {
        let mut tracker = AdaptiveSessionTracker::default();
        assert_eq!(
            tracker.manifest("hls-a".to_owned(), Duration::from_secs(10)).unwrap(),
            AdaptiveSessionState::Prepared
        );
        assert_eq!(tracker.segment("hls-a", Duration::from_secs(10)).unwrap(), AdaptiveSessionState::Active);
        tracker.preserve("hls-a", Duration::from_secs(10)).unwrap();
        assert_eq!(tracker.segment("hls-a", Duration::from_secs(10)).unwrap(), AdaptiveSessionState::Active);
    }

    #[tokio::test(start_paused = true)]
    async fn manifest_reaps_expired_session_before_duplicate_check() {
        let mut tracker = AdaptiveSessionTracker::default();
        tracker.manifest("hls-a".to_owned(), Duration::from_secs(5)).unwrap();
        tokio::time::advance(Duration::from_secs(6)).await;

        assert_eq!(
            tracker.manifest("hls-a".to_owned(), Duration::from_secs(10)).unwrap(),
            AdaptiveSessionState::Prepared
        );
    }

    #[tokio::test(start_paused = true)]
    async fn preserve_rejects_expired_unknown_session() {
        let mut tracker = AdaptiveSessionTracker::default();
        tracker.manifest("hls-a".to_owned(), Duration::from_secs(5)).unwrap();
        tokio::time::advance(Duration::from_secs(6)).await;

        assert!(matches!(tracker.preserve("hls-a", Duration::from_secs(10)), Err(TestkitError::Configuration(_))));
    }
}
