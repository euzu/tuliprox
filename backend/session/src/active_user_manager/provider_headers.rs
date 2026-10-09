use super::{ActiveUserManager, SessionIdentity, SessionProviderHeaders};
use shared::utils::current_time_secs;
use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc},
};

impl ActiveUserManager {
    /// Provider session headers valid for `target_url` now, for the session object and account
    /// binding of `identity` in any lifecycle state: a manifest refresh may revive an expired
    /// reservation.
    pub async fn provider_headers_for_session_identity(
        &self,
        username: &str,
        token: &str,
        identity: SessionIdentity,
        target_url: &str,
    ) -> SessionProviderHeaders {
        let users = self.connections.read().await;
        users
            .by_key
            .get(username)
            .and_then(|data| {
                data.sessions.iter().find(|session| session.token == token && session.identity() == identity)
            })
            .map_or(SessionProviderHeaders::NoSession, |session| {
                SessionProviderHeaders::for_target(session, target_url)
            })
    }

    /// Provider session headers currently valid for `target_url`, read at send time so cookies
    /// rotated or expired while the request waited are honored.
    pub async fn current_session_provider_headers(
        &self,
        username: &str,
        token: &str,
        identity: SessionIdentity,
        target_url: &str,
    ) -> SessionProviderHeaders {
        let users = self.connections.read().await;
        users.current_session(username, token, identity).map_or(SessionProviderHeaders::NoSession, |(_, session)| {
            SessionProviderHeaders::for_target(session, target_url)
        })
    }

    pub async fn update_session_provider_headers(
        &self,
        username: &str,
        token: &str,
        provider_session_headers: &HashMap<String, String>,
    ) -> bool {
        let mut user_connections = self.write_connections().await;
        let Some(connection_data) = user_connections.by_key.get_mut(username) else {
            return false;
        };
        let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == token) else {
            return false;
        };
        session.provider_session_headers.clone_from(provider_session_headers);
        session.provider_session_headers_host = None;
        session.clear_provider_session_cookies();
        session.ts = current_time_secs();
        true
    }

    pub async fn update_session_provider_response_headers_from(
        &self,
        username: &str,
        token: &str,
        response: &crate::ProviderSessionHeaders,
        source_url: &str,
    ) -> bool {
        self.store_session_provider_response_headers(username, token, None, response, source_url).await
    }

    /// Stores response cookies only while the session still has the expected account binding.
    pub async fn update_current_session_provider_response_headers_from(
        &self,
        username: &str,
        token: &str,
        identity: SessionIdentity,
        response: &crate::ProviderSessionHeaders,
        source_url: &str,
    ) -> bool {
        self.store_session_provider_response_headers(username, token, Some(identity), response, source_url).await
    }

    pub(super) async fn store_session_provider_response_headers(
        &self,
        username: &str,
        token: &str,
        expected: Option<SessionIdentity>,
        response: &crate::ProviderSessionHeaders,
        source_url: &str,
    ) -> bool {
        if response.is_empty() {
            return false;
        }
        let Ok(source) = url::Url::parse(source_url) else {
            return false;
        };
        let mut users = self.write_connections().await;
        if expected.is_some() && users.is_token_ended(token) {
            return false;
        }
        let Some(session) = users
            .by_key
            .get_mut(username)
            .and_then(|data| data.sessions.iter_mut().find(|session| session.token == token))
        else {
            return false;
        };
        if expected.is_some_and(|identity| session.identity() != identity || !session.is_live()) {
            return false;
        }
        Arc::make_mut(&mut session.provider_session_cookies).update(&source, response);
        // The origin-scoped cookie store is the only source once a provider response was stored.
        session.provider_session_headers.clear();
        session.provider_session_headers_host = None;
        session.ts = current_time_secs();
        true
    }

    /// Returns the stable User-Agent suffix for a playback session, assigning one on first use.
    pub async fn get_or_assign_user_agent_stream_index(&self, username: &str, token: &str) -> Option<u64> {
        let mut user_connections = self.write_connections().await;
        let session =
            user_connections.by_key.get_mut(username)?.sessions.iter_mut().find(|session| session.token == token)?;
        Some(*session.user_agent_stream_index.get_or_insert_with(|| self.next_user_agent_stream_index()))
    }

    /// Allocates the next process-local, globally unique User-Agent stream index.
    pub fn next_user_agent_stream_index(&self) -> u64 {
        let mut current = self.next_user_agent_stream_index.load(Ordering::Relaxed);
        loop {
            let next = if current == 0 || current == u64::MAX { 1 } else { current + 1 };
            match self.next_user_agent_stream_index.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(previous) => return if previous == 0 { 1 } else { previous },
                Err(observed) => current = observed,
            }
        }
    }

    /// Persists a previously allocated index without replacing an existing session identity.
    pub async fn set_user_agent_stream_index_if_absent(&self, username: &str, token: &str, index: u64) -> bool {
        let mut user_connections = self.write_connections().await;
        let Some(session) = user_connections
            .by_key
            .get_mut(username)
            .and_then(|connection| connection.sessions.iter_mut().find(|session| session.token == token))
        else {
            return false;
        };
        session.user_agent_stream_index.get_or_insert(index);
        true
    }
}
