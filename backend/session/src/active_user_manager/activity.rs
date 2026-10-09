use super::{
    next_session_incarnation, remember_session_addr, url_host_key, ActiveUserManager, CreateUserSessionParams,
    PlaybackLifecycle, SessionChangeSignal, SessionIdentity, SocketRegistration, UserConnectionData, UserSession,
    UserSessionParams,
};
use shared::{
    model::UserConnectionPermission,
    utils::{current_time_secs, sanitize_sensitive_info, Internable},
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{atomic::AtomicBool, Arc},
    time::Instant,
};
use tuliprox_core::utils::debug_if_enabled;

impl ActiveUserManager {
    pub(super) fn new_user_session(params: &UserSessionParams<'_>) -> UserSession {
        let now = current_time_secs();
        UserSession {
            token: params.session_token.to_string(),
            transition_version: 1,
            virtual_id: params.virtual_id,
            provider: params.provider.intern(),
            stream_url: params.stream_url.intern(),
            provider_session_headers: HashMap::new(),
            provider_session_headers_host: None,
            provider_session_cookies: Arc::default(),
            incarnation: next_session_incarnation(),
            binding_generation: 0,
            change_signal: SessionChangeSignal::new(params.change_wakes),
            media_started: Arc::new(AtomicBool::new(false)),
            user_agent_stream_index: None,
            addr: *params.addr,
            socket_bound: params.socket_bound,
            active_addrs: vec![*params.addr],
            ts: now,
            started_at: now,
            permission: params.connection_permission,
            connection_kind: params.connection_kind,
            lifecycle: PlaybackLifecycle::Prepared,
        }
    }

    pub async fn create_user_session(&self, request: CreateUserSessionParams<'_>) -> String {
        let CreateUserSessionParams {
            user,
            session_token,
            virtual_id,
            provider,
            stream_url,
            addr,
            connection_permission,
            connection_kind,
            socket_bound,
        } = request;
        self.gc();

        let username = user.username.clone();
        let mut user_connections = self.write_connections().await;
        let connection_data = user_connections.by_key.entry(username.clone()).or_insert_with(|| {
            debug_if_enabled!("Creating first session for user {username} {}", sanitize_sensitive_info(stream_url));
            let mut data = UserConnectionData::new(0, user.max_connections, user.soft_connections);
            let session = Self::new_user_session(&UserSessionParams {
                change_wakes: &self.deferred_wakes,
                session_token,
                virtual_id,
                provider,
                stream_url,
                addr,
                connection_permission,
                connection_kind,
                socket_bound,
            });
            data.add_session(session);
            data
        });

        // If a session exists, update it
        for session in &mut connection_data.sessions {
            if session.token == session_token {
                session.ts = current_time_secs();
                session.socket_bound = socket_bound;
                remember_session_addr(session, *addr);
                Self::bump_session_transition_version(session);
                let mut reset_provider_session_headers = false;
                if &*session.stream_url != stream_url {
                    // Provider cookies are host scoped: alternating child playlists on one host keep them.
                    reset_provider_session_headers |= url_host_key(&session.stream_url) != url_host_key(stream_url);
                    session.stream_url = stream_url.intern();
                }
                if &*session.provider != provider {
                    session.switch_provider(provider.intern());
                }
                if reset_provider_session_headers {
                    session.provider_session_headers.clear();
                    session.provider_session_headers_host = None;
                }
                // Normalize stale lifecycle states on session refresh.
                // Expired, PendingProvider, and Preserved sessions cannot stay in those states
                // when a new request arrives for the same session token - the request is either
                // a reactivation (Activate) or a follow-up on a still-valid logical playback.
                match session.lifecycle {
                    PlaybackLifecycle::Expired => {
                        session.lifecycle = PlaybackLifecycle::Prepared;
                    }
                    // PendingProvider: pending wait continues until explicitly resolved.
                    // Preserved: stays preserved until explicit reactivation via activation path.
                    // Prepared: placeholder session, no counted lease.
                    // Active: session is already in a valid counted state.
                    // All these keep their current state - session.refresh() alone does not advance it.
                    #[allow(clippy::match_same_arms)]
                    PlaybackLifecycle::PendingProvider { .. }
                    | PlaybackLifecycle::Preserved
                    | PlaybackLifecycle::Prepared
                    | PlaybackLifecycle::Active => {}
                    PlaybackLifecycle::GraceActive => {
                        // GraceActive refresh keeps the provisional state. Grace window is still
                        // running — refresh does not advance it. The grace task will resolve it.
                    }
                }
                Self::update_session_admission(session, connection_permission, connection_kind);
                debug_if_enabled!(
                    "Using session for user {} with url: {}",
                    user.username,
                    sanitize_sensitive_info(stream_url)
                );
                return session.token.clone();
            }
        }

        // If no session exists, create one
        debug_if_enabled!(
            "Creating session for user {} with url: {}",
            user.username,
            sanitize_sensitive_info(stream_url)
        );
        let session = Self::new_user_session(&UserSessionParams {
            change_wakes: &self.deferred_wakes,
            session_token,
            virtual_id,
            provider,
            stream_url,
            addr,
            connection_permission,
            connection_kind,
            socket_bound,
        });
        let token = session.token.clone();
        connection_data.add_session(session);
        let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, &username);
        drop(user_connections);
        self.log_divergence_snapshot(divergence_snapshot).await;
        token
    }

    pub async fn touch_socket_activity(&self, addr: &SocketAddr) {
        let now = current_time_secs();
        let mut user_connections = self.write_connections().await;
        // Transport activity keeps the transport alive, not every playback that
        // has previously used it. Authenticated HTTP activity updates its own user.
        if let Some(registration) = user_connections.key_by_addr.get_mut(addr) {
            registration.ts = now;
        }
    }

    pub async fn touch_http_activity(&self, username: &str, token: &str, addr: &SocketAddr) {
        let now = current_time_secs();
        let mut user_connections = self.write_connections().await;

        let registration = user_connections.key_by_addr.entry(*addr).or_insert_with(SocketRegistration::anonymous);
        registration.add_user(username, now);

        let Some(connection_data) = user_connections.by_key.get_mut(username) else {
            return;
        };

        connection_data.ts = now;

        for session in &mut connection_data.sessions {
            if session.token == token {
                // Lightweight HTTP activity (for example HLS manifest reloads) refreshes
                // continuity metadata only. It must not become an active stream socket:
                // otherwise a manifest or probe request can steal the visible stream addr,
                // and the real segment socket later migrates to that stale addr instead of
                // being released/preserved.
                session.ts = now;
                break;
            }
        }
    }

    /// Marks a session as ended on purpose (eviction, kick, explicit terminate), so it is not
    /// recreated from a sealed playlist token.
    pub async fn mark_session_ended(&self, token: &str) {
        self.write_connections().await.mark_sessions_ended([token.to_string()]);
    }

    /// True while a session token is blocked from recreation after an eviction, kick or terminate.
    pub async fn is_session_ended(&self, token: &str) -> bool {
        self.connections.read().await.ended_sessions.get(token).is_some_and(|expires_at| *expires_at > Instant::now())
    }

    pub async fn get_and_update_user_session(&self, username: &str, token: &str) -> Option<UserSession> {
        self.update_user_session(username, token).await
    }

    /// Checks an in-flight resource against the same session and account binding without touching admission.
    pub async fn playback_session_is_current(&self, username: &str, token: &str, identity: SessionIdentity) -> bool {
        self.connections.read().await.current_session(username, token, identity).is_some()
    }

    /// Resolves once the session ended or switched accounts. Only writes that end this
    /// session wake the waiter, after the write lock is released.
    pub async fn wait_for_playback_session_end(&self, username: &str, token: &str, identity: SessionIdentity) {
        loop {
            let notify = {
                let users = self.connections.read().await;
                let Some((_, session)) = users.current_session(username, token, identity) else {
                    return;
                };
                Arc::clone(&session.change_signal.notify)
            };
            let changed = notify.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            // A change between the lookup and `enable` was not observed: re-check before waiting.
            if !self.playback_session_is_current(username, token, identity).await {
                return;
            }
            changed.await;
        }
    }

    /// Session object and account binding currently stored for `token`, in any lifecycle state.
    pub async fn session_identity(&self, username: &str, token: &str) -> Option<SessionIdentity> {
        let users = self.connections.read().await;
        users.by_key.get(username)?.sessions.iter().find(|session| session.token == token).map(UserSession::identity)
    }

    pub async fn media_started_flag(&self, username: &str, token: &str) -> Option<Arc<AtomicBool>> {
        let users = self.connections.read().await;
        users
            .by_key
            .get(username)?
            .sessions
            .iter()
            .find(|session| session.token == token)
            .map(|session| Arc::clone(&session.media_started))
    }

    /// Session for target-scoped `virtual_id` and request token (used to recover leaked relative DVR segment paths).
    pub async fn find_latest_session_for_target_stream(
        &self,
        username: &str,
        target_id: u16,
        input_name: &str,
        virtual_id: u32,
        session_token: &str,
    ) -> Option<UserSession> {
        let user_connections = self.connections.read().await;
        let connection_data = user_connections.by_key.get(username)?;
        connection_data
            .streams
            .iter()
            .any(|stream| {
                stream.channel.target_id == target_id
                    && stream.channel.input_name.as_ref() == input_name
                    && stream.channel.virtual_id == virtual_id
                    && stream.session_token.as_deref() == Some(session_token)
            })
            .then_some(())?;

        connection_data
            .sessions
            .iter()
            .find(|session| session.token == session_token && session.virtual_id == virtual_id)
            .cloned()
    }

    pub(super) async fn update_user_session(&self, username: &str, token: &str) -> Option<UserSession> {
        let mut user_connections = self.write_connections().await;

        let connection_data = user_connections.by_key.get_mut(username)?;
        let now = current_time_secs();

        connection_data.ts = now;

        let session_index = connection_data.sessions.iter().position(|s| s.token == token)?;

        connection_data.sessions[session_index].ts = now;

        if connection_data.max_connections > 0
            && connection_data.sessions[session_index].permission == UserConnectionPermission::GracePeriod
            && !matches!(connection_data.sessions[session_index].lifecycle, PlaybackLifecycle::PendingProvider { .. })
        {
            let admission = self.check_connection_admission(username, connection_data);
            connection_data.sessions[session_index].set_permission(admission.permission());
            if admission.kind().is_some() {
                connection_data.sessions[session_index].connection_kind = admission.kind();
            }
        }

        Some(connection_data.sessions[session_index].clone())
    }
}
