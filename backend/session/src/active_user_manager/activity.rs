use super::{
    next_session_incarnation, remember_session_addr, ActiveUserManager, CreateUserSessionParams, FailoverBudget,
    LegacyHlsRebase, MemberBinding, PlaybackLifecycle, SessionChangeSignal, SessionIdentity, SocketRegistration,
    UserConnectionData, UserSession, UserSessionParams,
};
use shared::{
    model::{ActiveUserConnectionChange, EventMessage, UserConnectionPermission},
    utils::{current_time_secs, sanitize_sensitive_info, Internable},
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};
use tuliprox_core::utils::debug_if_enabled;

impl ActiveUserManager {
    pub(super) fn new_user_session(params: &UserSessionParams<'_>) -> UserSession {
        let now = current_time_secs();
        UserSession {
            token: params.session_token.to_string(),
            playback_id: params.playback_id.map(Arc::from),
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
            member_binding: None,
            legacy_rebase: None,
            legacy_failover_budget: None,
            member_failed_on_segment: false,
            member_failure_generation: 0,
            legacy_switch_in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn create_user_session(&self, request: CreateUserSessionParams<'_>) -> String {
        self.create_user_session_with_playback_id(request, None).await
    }

    pub async fn create_user_session_with_playback_id(
        &self,
        request: CreateUserSessionParams<'_>,
        playback_id: Option<&str>,
    ) -> String {
        // Without a fence no refresh is ever rejected; the session token is returned either way.
        self.create_user_session_inner(request, None, playback_id).await.map_or_else(|token| token, |(token, _)| token)
    }

    /// Creates a session, or refreshes the existing one with `session_token`, and returns its token.
    ///
    /// `fence` carries the session identity and member binding the caller saw before it started
    /// resolving the request. An existing session is refreshed only while it still matches that
    /// fence, checked under the same write lock as the update: a switch that published a new
    /// provider or member in between is never overwritten by this request's older view. A rejected
    /// refresh changes nothing and returns `None`; the caller answers with a retry. With a fence, a
    /// session that no longer exists is not recreated from the caller's older view: the call is
    /// rejected, and the next request without a session snapshot creates a fresh one.
    pub async fn create_user_session_checked(
        &self,
        request: CreateUserSessionParams<'_>,
        fence: Option<(SessionIdentity, Option<&MemberBinding>)>,
    ) -> Option<(String, SessionIdentity)> {
        self.create_user_session_checked_with_playback_id(request, fence, None).await
    }

    pub async fn create_user_session_checked_with_playback_id(
        &self,
        request: CreateUserSessionParams<'_>,
        fence: Option<(SessionIdentity, Option<&MemberBinding>)>,
        playback_id: Option<&str>,
    ) -> Option<(String, SessionIdentity)> {
        self.create_user_session_inner(request, fence, playback_id).await.ok()
    }

    /// Shared body of the unfenced and fenced session creation. Returns `Err(token)` for a refresh
    /// rejected by `fence`: either the session it named no longer matches, or no session with that
    /// token exists any more (evicted or ended while the request ran). A fenced call never creates
    /// a session; the session is left unchanged, and state this call created for that refresh is
    /// removed again before the error returns.
    #[expect(clippy::too_many_lines, reason = "Session lookup, fence, refresh and creation share one write lock.")]
    async fn create_user_session_inner(
        &self,
        request: CreateUserSessionParams<'_>,
        fence: Option<(SessionIdentity, Option<&MemberBinding>)>,
        playback_id: Option<&str>,
    ) -> Result<(String, SessionIdentity), String> {
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

        let stream_log_value = || sanitize_sensitive_info(stream_url);
        let username = user.username.clone();
        let mut user_connections = self.write_connections().await;
        if fence.is_some() && user_connections.is_token_ended(session_token) {
            return Err(session_token.to_string());
        }
        let entry_created = !user_connections.by_key.contains_key(&username);
        let connection_data = user_connections.by_key.entry(username.clone()).or_insert_with(|| {
            debug_if_enabled!("Creating first session for user {username} {}", stream_log_value());
            let mut data = UserConnectionData::new(0, user.max_connections, user.soft_connections);
            let session = Self::new_user_session(&UserSessionParams {
                change_wakes: &self.deferred_wakes,
                session_token,
                playback_id,
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
        let mut rejected_token = None;
        for session in &mut connection_data.sessions {
            if session.token == session_token {
                // The fence runs before any refresh bookkeeping: a request that lost the race owns
                // no part of the session it no longer matches.
                if fence.is_some_and(|(identity, binding)| {
                    session.identity() != identity || session.member_binding.as_ref() != binding
                }) {
                    debug_if_enabled!(
                        "Refusing to refresh stale HLS session {} for user {} (url: {})",
                        sanitize_sensitive_info(&session.token),
                        user.username,
                        stream_log_value()
                    );
                    rejected_token = Some(session.token.clone());
                    break;
                }
                session.ts = current_time_secs();
                session.socket_bound = socket_bound;
                remember_session_addr(session, *addr);
                if let Some(playback_id) = playback_id {
                    session.playback_id = Some(Arc::from(playback_id));
                }
                Self::bump_session_transition_version(session);
                session.set_stream_url(stream_url);
                if &*session.provider != provider {
                    session.switch_provider(provider.intern());
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
                debug_if_enabled!("Using session for user {} with url: {}", user.username, stream_log_value());
                return Ok((session.token.clone(), session.identity()));
            }
        }
        if let Some(token) = rejected_token {
            // A rejected refresh changes nothing. When this call created the user entry, it also
            // created the only session in it, so both exist only because of the rejected refresh
            // and are removed again; a pre-existing session keeps its state untouched.
            if entry_created {
                user_connections.by_key.remove(&username);
            }
            return Err(token);
        }

        // A fence always names a session its caller saw. Finding none with that token means the
        // session was evicted or ended while the request resolved, so recreating it here would
        // serve a view the evicting actor already invalidated - two devices at `max_connections`
        // would then evict each other forever. Reject without creating a replacement. A created
        // entry always holds the session with this token (the loop above matched it), so this path
        // only runs for a pre-existing entry; remove it defensively if that ever changes.
        if fence.is_some() {
            debug_if_enabled!(
                "Refusing to recreate missing HLS session {} for user {} (url: {})",
                sanitize_sensitive_info(session_token),
                user.username,
                stream_log_value()
            );
            if entry_created {
                user_connections.by_key.remove(&username);
            }
            return Err(session_token.to_string());
        }

        // If no session exists, create one
        debug_if_enabled!("Creating session for user {} with url: {}", user.username, stream_log_value());
        let session = Self::new_user_session(&UserSessionParams {
            change_wakes: &self.deferred_wakes,
            session_token,
            playback_id,
            virtual_id,
            provider,
            stream_url,
            addr,
            connection_permission,
            connection_kind,
            socket_bound,
        });
        let token = session.token.clone();
        let identity = session.identity();
        connection_data.add_session(session);
        let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, &username);
        drop(user_connections);
        self.log_divergence_snapshot(divergence_snapshot).await;
        Ok((token, identity))
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

    /// Returns a live user session without updating its timestamp or acquiring a write lock.
    pub async fn get_user_session(&self, username: &str, token: &str) -> Option<UserSession> {
        let users = self.connections.read().await;
        users.live_session(username, token).map(|(_, session)| session.clone())
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

    pub async fn bind_member(&self, username: &str, token: &str, binding: MemberBinding) {
        let mut user_connections = self.write_connections().await;
        self.apply_member_binding(&mut user_connections, username, token, binding, None);
    }

    pub async fn bind_member_with_identity(
        &self,
        username: &str,
        token: &str,
        binding: MemberBinding,
        expected_incarnation: u64,
    ) -> Option<SessionIdentity> {
        let mut user_connections = self.write_connections().await;
        self.apply_member_binding(&mut user_connections, username, token, binding, Some(expected_incarnation))
    }

    fn apply_member_binding(
        &self,
        user_connections: &mut super::UserConnectionsWriteGuard<'_>,
        username: &str,
        token: &str,
        binding: MemberBinding,
        expected_incarnation: Option<u64>,
    ) -> Option<SessionIdentity> {
        let member_info = binding.to_stream_member_info();
        let mut matched_stream = false;
        let mut updated_identity = None;
        if let Some(connection_data) = user_connections.by_key.get_mut(username) {
            if let Some(session) = connection_data.sessions.iter_mut().find(|s| s.token == token) {
                if expected_incarnation.is_some_and(|inc| session.incarnation != inc) {
                    return None;
                }
                // A member change advances the binding generation, so every fence taken before it
                // fails; the same binding written again is not a change.
                session.set_member_binding(binding.clone());
                updated_identity = Some(session.identity());
            } else if expected_incarnation.is_some() {
                return None;
            }
            for stream in &mut connection_data.streams {
                if stream.session_token.as_deref() == Some(token) {
                    stream.member_info = Some(member_info.clone());
                    matched_stream = true;
                    self.event_manager
                        .send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(stream.clone())));
                }
            }
        } else if expected_incarnation.is_some() {
            return None;
        }
        if matched_stream {
            user_connections.pending_member_bindings.remove(token);
        } else {
            user_connections
                .pending_member_bindings
                .insert(token.to_string(), (Instant::now() + Duration::from_secs(60), binding));
        }
        updated_identity
    }

    /// Rebinds a legacy HLS group session to a new member and provider in one write, marks a
    /// pending timeline rebase and applies the switch accounting. A concurrent media playlist
    /// request never sees the new binding without the marker, and the switch is counted even when
    /// the caller's later plan update loses its fence. The provider and stream URL come from the
    /// binding itself.
    ///
    /// `identity` and `expected_binding` are the trigger's fence. Resolving the new member and
    /// probing its manifest happens without the session lock, so the token may name a session that
    /// was terminated and recreated in between; the fence is re-checked under this write lock and a
    /// mismatch writes nothing. The caller answers `None` with a retry.
    ///
    /// Returns the `SessionIdentity` this write published, so the caller fences its later plan
    /// update on the session object it wrote instead of a fresh read that a concurrent provider
    /// revert or member change could already have changed. `None` when the session is gone or the
    /// trigger fence no longer matches it.
    #[expect(clippy::too_many_arguments, reason = "The trigger fence and the published switch belong to one write.")]
    pub async fn switch_legacy_group_member_binding(
        &self,
        username: &str,
        token: &str,
        identity: SessionIdentity,
        expected_binding: &MemberBinding,
        binding: MemberBinding,
        stream_url: Arc<str>,
        on_switched: impl FnOnce(&mut Option<FailoverBudget>),
    ) -> Option<SessionIdentity> {
        let mut user_connections = self.write_connections().await;
        {
            let session = user_connections.by_key.get(username)?.sessions.iter().find(|s| s.token == token)?;
            if session.identity() != identity || session.member_binding.as_ref() != Some(expected_binding) {
                return None;
            }
        }
        let provider = Arc::clone(&binding.provider);
        Self::apply_session_provider_binding(&mut user_connections, username, token, &provider, &stream_url, None);
        // The member write runs before the identity is read: it may advance the binding
        // generation, and the caller fences its later plan on the identity this write published.
        self.apply_member_binding(&mut user_connections, username, token, binding, None);
        user_connections
            .by_key
            .get_mut(username)
            .and_then(|connection_data| connection_data.sessions.iter_mut().find(|s| s.token == token))
            .map(|session| {
                session.legacy_rebase.get_or_insert_with(LegacyHlsRebase::default).pending_member_rebase = true;
                // The switch is counted in the same write that publishes the binding: a caller
                // whose later plan update loses its fence can no longer lose the accounting of a
                // switch that already happened.
                on_switched(&mut session.legacy_failover_budget);
                session.identity()
            })
    }

    /// Runs `update` on the session named by `identity` and `expected_binding` under the caller's
    /// write lock. Returns `None`, before anything is written, when the session is gone or the
    /// fence fails.
    fn update_fenced_session<R>(
        user_connections: &mut super::UserConnectionsWriteGuard<'_>,
        username: &str,
        token: &str,
        identity: SessionIdentity,
        expected_binding: &MemberBinding,
        update: impl FnOnce(&mut UserSession) -> R,
    ) -> Option<R> {
        let session = user_connections.by_key.get_mut(username)?.sessions.iter_mut().find(|s| s.token == token)?;
        if session.identity() != identity || session.member_binding.as_ref() != Some(expected_binding) {
            return None;
        }
        Some(update(session))
    }

    /// Updates the legacy failover state (timeline rebase and incident budget) of a bound session
    /// under the write lock, after checking the session identity and member binding. The closure runs
    /// synchronously while the lock is held, so a plan built from a request-start snapshot can never
    /// overwrite a commit another request made in between. The identity carries the binding
    /// generation, which every provider write (`switch_provider`) and every actual member change
    /// (`set_member_binding`) advances: a reverted provider or a member switched away and back
    /// fails the identity check, so the provider is not compared here. A member bind without a
    /// provider write (cache and TS paths with their own token) leaves the session and binding on
    /// different providers, and that state stays committable. Returns `None` when the session is
    /// gone or the fence fails.
    pub async fn update_legacy_failover_state_if_bound<R>(
        &self,
        username: &str,
        token: &str,
        identity: SessionIdentity,
        expected_binding: &MemberBinding,
        update: impl FnOnce(&mut LegacyHlsRebase, &mut Option<FailoverBudget>) -> R,
    ) -> Option<R> {
        let mut user_connections = self.write_connections().await;
        Self::update_fenced_session(&mut user_connections, username, token, identity, expected_binding, |session| {
            let rebase = session.legacy_rebase.get_or_insert_with(LegacyHlsRebase::default);
            update(rebase, &mut session.legacy_failover_budget)
        })
    }

    /// Fenced media commit of a legacy HLS group member. Runs `update` under the write lock like
    /// [`Self::update_legacy_failover_state_if_bound`] and, when `serves` reports that the returned
    /// plan serves the window, clears `member_failed_on_segment` in the same write. A served window
    /// proves the bound member works again, so the next manifest poll must not switch away from it.
    /// Returns `None` when the session is gone or the fence fails; nothing is written then.
    #[expect(
        clippy::too_many_arguments,
        reason = "The fence, the failure generation and the plan belong to one write."
    )]
    pub async fn commit_legacy_media_serve_if_bound<R>(
        &self,
        username: &str,
        token: &str,
        identity: SessionIdentity,
        expected_binding: &MemberBinding,
        captured_failure_generation: u64,
        update: impl FnOnce(&mut LegacyHlsRebase, &mut Option<FailoverBudget>) -> R,
        serves: impl FnOnce(&R) -> bool,
    ) -> Option<R> {
        let mut user_connections = self.write_connections().await;
        Self::update_fenced_session(&mut user_connections, username, token, identity, expected_binding, |session| {
            let rebase = session.legacy_rebase.get_or_insert_with(LegacyHlsRebase::default);
            let result = update(rebase, &mut session.legacy_failover_budget);
            if serves(&result) && session.member_failure_generation == captured_failure_generation {
                session.member_failed_on_segment = false;
            }
            result
        })
    }

    /// Marks the bound member of a legacy HLS group session failed by a hard segment request, so
    /// the next manifest refresh switches instead of serving the failed member. Validates that the
    /// session identity and member binding match under the same write lock.
    pub async fn mark_member_failed_on_segment(
        &self,
        username: &str,
        token: &str,
        identity: SessionIdentity,
        expected_binding: &MemberBinding,
    ) -> bool {
        let mut user_connections = self.write_connections().await;
        if let Some(connection_data) = user_connections.by_key.get_mut(username) {
            if let Some(session) = connection_data.sessions.iter_mut().find(|s| s.token == token) {
                if session.identity() == identity && session.member_binding.as_ref() == Some(expected_binding) {
                    session.member_failed_on_segment = true;
                    session.member_failure_generation = session.member_failure_generation.wrapping_add(1);
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventManager;
    use arc_swap::ArcSwapOption;
    use tuliprox_core::model::{Config, ProxyUserCredentials};
    use tuliprox_repository::GeoIp;

    fn test_manager() -> ActiveUserManager {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        ActiveUserManager::new(&config, &geoip, &event_manager)
    }

    fn test_user() -> ProxyUserCredentials {
        let mut user = ProxyUserCredentials::default();
        user.username = "test-user".to_string();
        user.password = "password".to_string();
        user.max_connections = 5;
        user.soft_connections = 0;
        user
    }

    #[tokio::test]
    async fn manifest_refresh_preserves_pending_member_failed_on_segment() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-1",
                virtual_id: 100,
                provider: "prov-1",
                stream_url: "http://example.com/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;

        let binding = MemberBinding {
            group_vid: 100,
            member_vid: 101,
            member_index: 0,
            input_name: Arc::from("input-1"),
            member_title: Arc::from("title-1"),
            provider: Arc::from("prov-1"),
        };
        manager.bind_member(&user.username, &token, binding.clone()).await;

        let identity = {
            let connections = manager.connections.read().await;
            connections
                .by_key
                .get(&user.username)
                .unwrap()
                .sessions
                .iter()
                .find(|s| s.token == token)
                .unwrap()
                .identity()
        };

        let marked = manager.mark_member_failed_on_segment(&user.username, &token, identity, &binding).await;
        assert!(marked, "mark should succeed with matching identity and binding");

        {
            let connections = manager.connections.read().await;
            let session =
                connections.by_key.get(&user.username).unwrap().sessions.iter().find(|s| s.token == token).unwrap();
            assert!(session.member_failed_on_segment, "session should have member_failed_on_segment set");
        }

        // Ordinary manifest refresh (a fenced no-op update) preserves the flag
        let refreshed =
            manager.update_legacy_failover_state_if_bound(&user.username, &token, identity, &binding, |_, _| {}).await;
        assert_eq!(refreshed, Some(()), "an ordinary manifest refresh keeps the bound session's state");

        {
            let connections = manager.connections.read().await;
            let session =
                connections.by_key.get(&user.username).unwrap().sessions.iter().find(|s| s.token == token).unwrap();
            assert!(session.member_failed_on_segment, "manifest refresh must not clear pending segment failure");
        }

        // Switch path rebinds to next member, clearing the pending failure flag
        let next_binding = MemberBinding {
            group_vid: 100,
            member_vid: 102,
            member_index: 1,
            input_name: Arc::from("input-2"),
            member_title: Arc::from("title-2"),
            provider: Arc::from("prov-2"),
        };
        manager.bind_member(&user.username, &token, next_binding).await;

        {
            let connections = manager.connections.read().await;
            let session =
                connections.by_key.get(&user.username).unwrap().sessions.iter().find(|s| s.token == token).unwrap();
            assert!(!session.member_failed_on_segment, "switch path must clear segment failure flag");
        }
    }

    #[tokio::test]
    async fn failure_generation_prevents_in_flight_serve_from_clearing_newer_segment_failure() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-fail-gen",
                virtual_id: 100,
                provider: "prov-1",
                stream_url: "http://example.com/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;

        let binding = MemberBinding {
            group_vid: 10,
            member_vid: 101,
            member_index: 0,
            input_name: Arc::from("input-1"),
            member_title: Arc::from("title-1"),
            provider: Arc::from("prov-1"),
        };
        manager.bind_member(&user.username, &token, binding.clone()).await;
        let identity = manager.session_identity(&user.username, &token).await.unwrap();

        // 1. Poll starts: capture failure generation when member has not failed
        let poll_failure_generation =
            manager.get_and_update_user_session(&user.username, &token).await.unwrap().member_failure_generation;

        // 2. While poll is in flight, a segment request on the same member fails hard
        assert!(manager.mark_member_failed_on_segment(&user.username, &token, identity, &binding).await);
        let session = manager.get_and_update_user_session(&user.username, &token).await.unwrap();
        assert!(session.member_failed_on_segment);
        assert_eq!(session.member_failure_generation, poll_failure_generation.wrapping_add(1));

        // 3. The in-flight poll finishes and commits a media serve with the captured generation.
        // Because a segment failure occurred in between, the failure generation bumped and the serve
        // must NOT clear member_failed_on_segment!
        manager
            .commit_legacy_media_serve_if_bound(
                &user.username,
                &token,
                identity,
                &binding,
                poll_failure_generation,
                |_rebase, _budget| true,
                |_served| true,
            )
            .await;

        let session_after = manager.get_and_update_user_session(&user.username, &token).await.unwrap();
        assert!(
            session_after.member_failed_on_segment,
            "an in-flight poll from an older failure generation must not clear a newer segment failure"
        );

        // 4. A subsequent poll captures the current failure generation. When it serves, it clears the failure.
        let fresh_poll_failure_generation = session_after.member_failure_generation;
        manager
            .commit_legacy_media_serve_if_bound(
                &user.username,
                &token,
                identity,
                &binding,
                fresh_poll_failure_generation,
                |_rebase, _budget| true,
                |_served| true,
            )
            .await;

        let session_final = manager.get_and_update_user_session(&user.username, &token).await.unwrap();
        assert!(
            !session_final.member_failed_on_segment,
            "a poll started after the failure clears member_failed_on_segment on successful serve"
        );
    }

    #[tokio::test]
    async fn mark_member_failed_on_segment_validates_identity_and_binding() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-2",
                virtual_id: 200,
                provider: "prov-1",
                stream_url: "http://example.com/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;

        let binding_a = MemberBinding {
            group_vid: 200,
            member_vid: 201,
            member_index: 0,
            input_name: Arc::from("input-a"),
            member_title: Arc::from("title-a"),
            provider: Arc::from("prov-a"),
        };
        manager.bind_member(&user.username, &token, binding_a.clone()).await;

        let identity = {
            let connections = manager.connections.read().await;
            connections
                .by_key
                .get(&user.username)
                .unwrap()
                .sessions
                .iter()
                .find(|s| s.token == token)
                .unwrap()
                .identity()
        };

        // Stale identity: must not mark
        let stale_identity = SessionIdentity {
            incarnation: identity.incarnation.wrapping_add(1),
            binding_generation: identity.binding_generation,
        };
        let marked = manager.mark_member_failed_on_segment(&user.username, &token, stale_identity, &binding_a).await;
        assert!(!marked, "stale identity must not mark segment failure");

        // Stale binding: must not mark
        let wrong_binding = MemberBinding {
            group_vid: 200,
            member_vid: 999,
            member_index: 5,
            input_name: Arc::from("input-wrong"),
            member_title: Arc::from("title-wrong"),
            provider: Arc::from("prov-wrong"),
        };
        let marked = manager.mark_member_failed_on_segment(&user.username, &token, identity, &wrong_binding).await;
        assert!(!marked, "wrong binding must not mark segment failure");

        {
            let connections = manager.connections.read().await;
            let session =
                connections.by_key.get(&user.username).unwrap().sessions.iter().find(|s| s.token == token).unwrap();
            assert!(!session.member_failed_on_segment, "flag should still be false");
        }

        // Matching identity and binding: marks successfully
        let marked = manager.mark_member_failed_on_segment(&user.username, &token, identity, &binding_a).await;
        assert!(marked, "matching identity and binding must mark segment failure");

        {
            let connections = manager.connections.read().await;
            let session =
                connections.by_key.get(&user.username).unwrap().sessions.iter().find(|s| s.token == token).unwrap();
            assert!(session.member_failed_on_segment, "flag must be true");
        }
    }

    #[tokio::test]
    async fn switch_legacy_group_member_binding_publishes_binding_and_pending_rebase_atomically() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-switch",
                virtual_id: 300,
                provider: "prov-1",
                stream_url: "http://example.com/live1.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;
        let member = |member_vid: u32, name: &str| MemberBinding {
            group_vid: 300,
            member_vid,
            member_index: 0,
            input_name: Arc::from(name),
            member_title: Arc::from(name),
            provider: Arc::from(name),
        };
        // A bound member always runs on the provider of its binding; the fence rejects anything else.
        let binding_a = MemberBinding { provider: "prov-1".intern(), ..member(301, "input-a") };
        manager.bind_member(&user.username, &token, binding_a.clone()).await;
        let initial_rebase = LegacyHlsRebase {
            last_served_sequence: 100,
            media_sequence_offset: 10,
            discontinuity_sequence: 1,
            pending_member_rebase: false,
            ..LegacyHlsRebase::default()
        };
        let identity = manager.session_identity(&user.username, &token).await.expect("identity");
        let seeded = manager
            .update_legacy_failover_state_if_bound(&user.username, &token, identity, &binding_a, |state, _| {
                *state = initial_rebase;
            })
            .await;
        assert_eq!(seeded, Some(()), "seed the timeline for the switch");
        manager.mark_member_failed_on_segment(&user.username, &token, identity, &binding_a).await;

        let binding_b = MemberBinding { provider: "prov-2".intern(), ..member(302, "input-b") };
        let new_provider: Arc<str> = Arc::clone(&binding_b.provider);
        let new_url: Arc<str> = "http://example.com/live2.m3u8".intern();

        // Switch to member B; the binding carries the new provider.
        let _ = manager
            .switch_legacy_group_member_binding(
                &user.username,
                &token,
                identity,
                &binding_a,
                binding_b.clone(),
                Arc::clone(&new_url),
                |_| {},
            )
            .await;

        let session = manager.get_and_update_user_session(&user.username, &token).await.expect("session");
        assert_eq!(session.member_binding, Some(binding_b.clone()));
        assert_eq!(session.provider, new_provider);
        assert_eq!(session.stream_url, new_url);
        assert!(!session.member_failed_on_segment);
        let expected_rebase = LegacyHlsRebase {
            last_served_sequence: 100,
            media_sequence_offset: 10,
            discontinuity_sequence: 1,
            pending_member_rebase: true,
            ..LegacyHlsRebase::default()
        };
        assert_eq!(session.legacy_rebase, Some(expected_rebase));
    }

    #[tokio::test]
    async fn switch_legacy_group_member_binding_applies_the_switch_accounting_in_the_same_write() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-accounting",
                virtual_id: 500,
                provider: "prov-1",
                stream_url: "http://example.com/live1.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;
        let binding_a = MemberBinding {
            group_vid: 500,
            member_vid: 501,
            member_index: 0,
            input_name: Arc::from("input-a"),
            member_title: Arc::from("title-a"),
            provider: Arc::from("prov-a"),
        };
        manager.bind_member(&user.username, &token, binding_a.clone()).await;
        let identity = manager.session_identity(&user.username, &token).await.expect("identity");
        let binding_b = MemberBinding {
            group_vid: 500,
            member_vid: 502,
            member_index: 1,
            input_name: Arc::from("input-b"),
            member_title: Arc::from("title-b"),
            provider: Arc::from("prov-b"),
        };
        let new_url: Arc<str> = "http://example.com/live2.m3u8".intern();
        let switched_at = Instant::now();
        let config = shared::model::FailoverGroupRuntimeConfig { max_switches: 1, ..Default::default() };

        let _ = manager
            .switch_legacy_group_member_binding(
                &user.username,
                &token,
                identity,
                &binding_a,
                binding_b.clone(),
                Arc::clone(&new_url),
                |budget| {
                    budget.get_or_insert_with(|| FailoverBudget::new(&config)).on_switched(
                        binding_b.member_index,
                        0,
                        switched_at,
                    );
                },
            )
            .await;
        // One call publishes the binding, the pending marker and the switch accounting; the flap
        // guard of one switch is already spent by that same recorded switch.
        let session = manager.get_and_update_user_session(&user.username, &token).await.expect("session");
        assert_eq!(session.member_binding, Some(binding_b));
        assert_eq!(session.stream_url, new_url);
        assert_eq!(
            session.legacy_rebase.as_ref().map(|rebase| rebase.pending_member_rebase),
            Some(true),
            "the binding and the pending rebase marker come from the same write"
        );
        let budget = session.legacy_failover_budget.expect("the switch accounting is stored with the binding");
        assert!(budget.would_give_up(switched_at), "the recorded switch must count against the flap guard");
    }

    #[tokio::test]
    async fn update_legacy_failover_state_if_bound_fences_identity_and_binding() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-fence",
                virtual_id: 400,
                provider: "prov-1",
                stream_url: "http://example.com/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;
        let binding = MemberBinding {
            group_vid: 400,
            member_vid: 401,
            member_index: 0,
            input_name: Arc::from("input-a"),
            member_title: Arc::from("title-a"),
            provider: "prov-1".intern(),
        };
        manager.bind_member(&user.username, &token, binding.clone()).await;
        let identity = manager.session_identity(&user.username, &token).await.expect("identity");

        let stale_identity = SessionIdentity { incarnation: identity.incarnation.wrapping_add(1), ..identity };
        let wrong_binding = MemberBinding { member_vid: 999, ..binding.clone() };

        for (rejected_identity, rejected_binding) in [(stale_identity, &binding), (identity, &wrong_binding)] {
            let applied = manager
                .update_legacy_failover_state_if_bound(
                    &user.username,
                    &token,
                    rejected_identity,
                    rejected_binding,
                    |state, budget| {
                        state.last_served_sequence = 7;
                        *budget = Some(FailoverBudget::new(&shared::model::FailoverGroupRuntimeConfig::default()));
                        "applied"
                    },
                )
                .await;
            assert_eq!(applied, None, "wrong identity or binding must not run the update");
            let session = manager.get_and_update_user_session(&user.username, &token).await.expect("session");
            assert_eq!(session.legacy_rebase, None, "a fenced update must not create timeline state");
            assert!(session.legacy_failover_budget.is_none(), "a fenced update must not create a budget");
        }

        let applied = manager
            .update_legacy_failover_state_if_bound(&user.username, &token, identity, &binding, |state, budget| {
                state.last_served_sequence = 7;
                *budget = Some(FailoverBudget::new(&shared::model::FailoverGroupRuntimeConfig::default()));
                "applied"
            })
            .await;
        assert_eq!(applied, Some("applied"));
        let session = manager.get_and_update_user_session(&user.username, &token).await.expect("session");
        assert_eq!(
            session.legacy_rebase,
            Some(LegacyHlsRebase { last_served_sequence: 7, ..LegacyHlsRebase::default() })
        );
        assert!(session.legacy_failover_budget.is_some(), "the accepted update stores the budget");
    }

    #[tokio::test]
    async fn update_legacy_failover_state_if_bound_keeps_sequential_updates() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-sequential",
                virtual_id: 500,
                provider: "prov-1",
                stream_url: "http://example.com/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;
        let binding = MemberBinding {
            group_vid: 500,
            member_vid: 501,
            member_index: 0,
            input_name: Arc::from("input-a"),
            member_title: Arc::from("title-a"),
            provider: "prov-1".intern(),
        };
        manager.bind_member(&user.username, &token, binding.clone()).await;
        let identity = manager.session_identity(&user.username, &token).await.expect("identity");

        let first = manager
            .update_legacy_failover_state_if_bound(&user.username, &token, identity, &binding, |state, budget| {
                state.last_served_sequence = 10;
                state.commit_generation = state.commit_generation.saturating_add(1);
                *budget = Some(FailoverBudget::new(&shared::model::FailoverGroupRuntimeConfig::default()));
            })
            .await;
        assert_eq!(first, Some(()));

        // The second request must build on the state the first one committed, not on a snapshot
        // taken before it.
        let observed = manager
            .update_legacy_failover_state_if_bound(&user.username, &token, identity, &binding, |state, budget| {
                let seen = state.last_served_sequence;
                assert!(budget.is_some(), "the first update's budget must still be stored");
                state.last_served_sequence = seen.saturating_add(1);
                seen
            })
            .await;
        assert_eq!(observed, Some(10), "the second update sees the first update's commit");

        let session = manager.get_and_update_user_session(&user.username, &token).await.expect("session");
        assert_eq!(session.legacy_rebase.as_ref().map(|rebase| rebase.last_served_sequence), Some(11));
        assert_eq!(session.legacy_rebase.as_ref().map(|rebase| rebase.commit_generation), Some(1));
        assert!(session.legacy_failover_budget.is_some());
    }

    fn bound_member(group_vid: u32, member_vid: u32, provider: &str) -> MemberBinding {
        MemberBinding {
            group_vid,
            member_vid,
            member_index: 0,
            input_name: Arc::from(provider),
            member_title: Arc::from(provider),
            provider: provider.intern(),
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn create_user_session_checked_does_not_revert_a_session_a_switch_rebound() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-fenced-refresh",
                virtual_id: 600,
                provider: "prov-a",
                stream_url: "http://example.com/a.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;
        let binding_a = bound_member(600, 601, "prov-a");
        manager.bind_member(&user.username, &token, binding_a.clone()).await;
        // The manifest request that resolved provider A from this snapshot is about to download.
        let snapshot = manager.get_and_update_user_session(&user.username, &token).await.expect("session");

        // A concurrent switch publishes provider B and member B.
        let binding_b = bound_member(600, 602, "prov-b");
        let url_b: Arc<str> = "http://example.com/b.m3u8".intern();
        let _ = manager
            .switch_legacy_group_member_binding(
                &user.username,
                &token,
                snapshot.identity(),
                snapshot.member_binding.as_ref().expect("binding"),
                binding_b.clone(),
                Arc::clone(&url_b),
                |_| {},
            )
            .await;

        // The older request now refreshes the session with its stale view; it must not write
        // provider A and URL A over the published switch.
        let outcome = manager
            .create_user_session_checked(
                CreateUserSessionParams {
                    user: &user,
                    session_token: &token,
                    virtual_id: 600,
                    provider: "prov-a",
                    stream_url: "http://example.com/a.m3u8",
                    addr: &addr,
                    connection_permission: UserConnectionPermission::Allowed,
                    connection_kind: None,
                    socket_bound: false,
                },
                Some((snapshot.identity(), snapshot.member_binding.as_ref())),
            )
            .await;
        assert_eq!(outcome, None, "the stale refresh must report the mismatch");
        let session = manager.get_and_update_user_session(&user.username, &token).await.expect("session");
        assert_eq!(session.provider, "prov-b".intern(), "the switch's provider must survive the stale refresh");
        assert_eq!(session.stream_url, url_b, "the switch's URL must survive the stale refresh");
        assert_eq!(session.member_binding, Some(binding_b.clone()));

        // A refresh that matches the stored session, and a refresh that creates a new session, keep
        // today's behaviour.
        let current = manager.session_identity(&user.username, &token).await.expect("identity");
        let outcome = manager
            .create_user_session_checked(
                CreateUserSessionParams {
                    user: &user,
                    session_token: &token,
                    virtual_id: 600,
                    provider: "prov-c",
                    stream_url: "http://example.com/c.m3u8",
                    addr: &addr,
                    connection_permission: UserConnectionPermission::Allowed,
                    connection_kind: None,
                    socket_bound: false,
                },
                Some((current, Some(&binding_b))),
            )
            .await;
        let (out_token, out_identity) = outcome.expect("a matching fence must apply the refresh");
        assert_eq!(out_token, token);
        let session = manager.get_and_update_user_session(&user.username, &token).await.expect("session");
        assert_eq!(session.provider, "prov-c".intern());
        assert_eq!(out_identity, session.identity());

        let outcome = manager
            .create_user_session_checked(
                CreateUserSessionParams {
                    user: &user,
                    session_token: "tok-new-fenced",
                    virtual_id: 601,
                    provider: "prov-a",
                    stream_url: "http://example.com/new.m3u8",
                    addr: &addr,
                    connection_permission: UserConnectionPermission::Allowed,
                    connection_kind: None,
                    socket_bound: false,
                },
                Some((current, Some(&binding_b))),
            )
            .await;
        // A fence always names a session the caller saw. A token the entry does not hold is a
        // stale view (the session was evicted or ended) and is rejected instead of recreated.
        assert_eq!(outcome, None, "a fenced call never creates a session the entry does not hold");
        assert_eq!(manager.session_identity(&user.username, "tok-new-fenced").await, None);
    }

    #[tokio::test]
    async fn create_user_session_checked_rejection_leaves_no_session_it_created() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        // The caller refreshed a session that ended while it resolved the request: the user has no
        // entry any more and the fence carries the identity of the vanished session. Incarnations
        // start at 1, so incarnation 0 never matches a session this call creates.
        let vanished = (SessionIdentity { incarnation: 0, binding_generation: 0 }, None);

        let outcome = manager
            .create_user_session_checked(
                CreateUserSessionParams {
                    user: &user,
                    session_token: "tok-rejected-create",
                    virtual_id: 602,
                    provider: "prov-a",
                    stream_url: "http://example.com/rejected.m3u8",
                    addr: &addr,
                    connection_permission: UserConnectionPermission::Allowed,
                    connection_kind: None,
                    socket_bound: false,
                },
                Some(vanished),
            )
            .await;
        assert_eq!(outcome, None, "the fence rejects the refresh");

        // Both the entry and the session in it were created by the rejected call, so neither may
        // survive it: a rejected refresh changes nothing.
        let connections = manager.connections.read().await;
        assert!(!connections.by_key.contains_key(&user.username), "a rejected refresh leaves no user entry");
    }

    #[tokio::test]
    async fn create_user_session_checked_rejects_an_evicted_session_and_creates_nothing() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-evicted",
                virtual_id: 603,
                provider: "prov-a",
                stream_url: "http://example.com/evicted.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;
        let identity = manager.session_identity(&user.username, &token).await.expect("identity");

        // Admission eviction at `max_connections` removes the session and leaves the user entry
        // behind with no session in it. The caller resolved the request against that session, so
        // its fence still names the evicted identity.
        {
            let mut connections = manager.connections.write().await;
            let entry = connections.by_key.get_mut(&user.username).expect("user entry");
            entry.sessions.clear();
        }
        assert_eq!(manager.session_identity(&user.username, &token).await, None, "the session is evicted");

        let outcome = manager
            .create_user_session_checked(
                CreateUserSessionParams {
                    user: &user,
                    session_token: &token,
                    virtual_id: 603,
                    provider: "prov-a",
                    stream_url: "http://example.com/evicted.m3u8",
                    addr: &addr,
                    connection_permission: UserConnectionPermission::Allowed,
                    connection_kind: None,
                    socket_bound: false,
                },
                Some((identity, None)),
            )
            .await;
        assert_eq!(outcome, None, "a fenced call must reject an evicted session");

        // The rejected call changed nothing: the pre-existing entry survives without a session, so
        // the evicting device keeps its session and the two cannot evict each other in a loop.
        {
            let connections = manager.connections.read().await;
            let entry = connections.by_key.get(&user.username).expect("the pre-existing entry stays");
            assert_eq!(entry.sessions.len(), 0, "a rejected fenced call creates no session");
        }

        // The unfenced path keeps today's behaviour and creates the session again.
        let recreated = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: &token,
                virtual_id: 603,
                provider: "prov-a",
                stream_url: "http://example.com/evicted.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;
        assert_eq!(recreated, token);
        assert!(manager.session_identity(&user.username, &token).await.is_some());
    }

    #[tokio::test]
    async fn update_legacy_failover_state_if_bound_accepts_a_member_bind_without_a_provider_write() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-provider-fence",
                virtual_id: 700,
                provider: "prov-1",
                stream_url: "http://example.com/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;
        // Cache and TS paths carry their own token and bind the member only: the binding names
        // another provider while the session keeps prov-1. That state must stay committable; the
        // provider comparison it trips rejected every later plan and pinned the session to 503.
        // A provider rewrite always goes through `switch_provider`, which bumps the binding
        // generation, so a revert is rejected by the identity fence alone
        // (`switch_legacy_group_member_binding_returns_the_identity_its_later_fence_requires`).
        let binding = bound_member(700, 701, "prov-2");
        manager.bind_member(&user.username, &token, binding.clone()).await;
        let identity = manager.session_identity(&user.username, &token).await.expect("identity");

        let applied = manager
            .update_legacy_failover_state_if_bound(&user.username, &token, identity, &binding, |state, _| {
                state.last_served_sequence = 7;
            })
            .await;
        assert_eq!(applied, Some(()), "a member bind without a provider write keeps the plan committable");
        let session = manager.get_and_update_user_session(&user.username, &token).await.expect("session");
        assert_eq!(session.provider, "prov-1".intern(), "the member bind does not rewrite the provider");
        assert_eq!(
            session.legacy_rebase.as_ref().map(|rebase| rebase.last_served_sequence),
            Some(7),
            "the accepted plan commits into the session's timeline"
        );
    }

    #[tokio::test]
    async fn switch_legacy_group_member_binding_returns_the_identity_its_later_fence_requires() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-written-identity",
                virtual_id: 800,
                provider: "prov-a",
                stream_url: "http://example.com/a.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;
        let binding_a = bound_member(800, 801, "prov-a");
        manager.bind_member(&user.username, &token, binding_a.clone()).await;
        let initial_rebase = LegacyHlsRebase { last_served_sequence: 100, ..LegacyHlsRebase::default() };
        let identity = manager.session_identity(&user.username, &token).await.expect("identity");
        let seeded = manager
            .update_legacy_failover_state_if_bound(&user.username, &token, identity, &binding_a, |state, _| {
                *state = initial_rebase;
            })
            .await;
        assert_eq!(seeded, Some(()), "seed the timeline for the switch");

        let binding_b = bound_member(800, 802, "prov-b");
        let switched = manager
            .switch_legacy_group_member_binding(
                &user.username,
                &token,
                identity,
                &binding_a,
                binding_b.clone(),
                "http://example.com/b.m3u8".intern(),
                |_| {},
            )
            .await
            .expect("the switch publishes the session");
        assert_eq!(
            Some(switched),
            manager.session_identity(&user.username, &token).await,
            "the returned identity is the one the switch wrote"
        );

        // The stale-manifest race rewrites the provider of the rebound session after the switch.
        manager
            .update_session_provider_binding(
                &user.username,
                &token,
                "prov-a".intern(),
                "http://example.com/a.m3u8".intern(),
            )
            .await;

        let applied = manager
            .update_legacy_failover_state_if_bound(&user.username, &token, switched, &binding_b, |state, _| {
                state.last_served_sequence = 200;
            })
            .await;
        assert_eq!(applied, None, "the switch's later plan must not commit over a reverted provider");
        let session = manager.get_and_update_user_session(&user.username, &token).await.expect("session");
        assert_eq!(session.provider, "prov-a".intern());
        assert_eq!(session.member_binding, Some(binding_b));
        assert_eq!(
            session.legacy_rebase.as_ref().map(|rebase| (rebase.last_served_sequence, rebase.pending_member_rebase)),
            Some((100, true)),
            "the pending rebase marker stays set until a fenced bridge commits"
        );
    }

    #[tokio::test]
    async fn rewriting_the_same_member_binding_keeps_the_session_identity() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-same-binding",
                virtual_id: 900,
                provider: "prov-1",
                stream_url: "http://example.com/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;
        let binding = bound_member(900, 901, "prov-1");
        manager.bind_member(&user.username, &token, binding.clone()).await;
        let bound = manager.session_identity(&user.username, &token).await.expect("identity");
        assert_eq!(bound.binding_generation, 1, "the first member binding write advances the generation");

        // Segment and refresh paths rebind the member they already run on; that is not a change and
        // must not invalidate the identity their requests already hold.
        manager.bind_member(&user.username, &token, binding).await;
        assert_eq!(
            manager.session_identity(&user.username, &token).await,
            Some(bound),
            "rewriting the same member binding keeps the identity"
        );
    }

    #[tokio::test]
    async fn same_provider_member_switch_away_and_back_rejects_a_pre_switch_fence() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-same-provider-aba",
                virtual_id: 910,
                provider: "prov-1",
                stream_url: "http://example.com/a.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;
        // Both members run on the same provider, so no provider switch bumps the generation: only
        // the member change can tell the two views apart.
        let binding_a = bound_member(910, 911, "prov-1");
        manager.bind_member(&user.username, &token, binding_a.clone()).await;
        let snapshot = manager.get_and_update_user_session(&user.username, &token).await.expect("session");
        assert_eq!(snapshot.identity().binding_generation, 1);

        let binding_b = bound_member(910, 912, "prov-1");
        let first = manager
            .switch_legacy_group_member_binding(
                &user.username,
                &token,
                snapshot.identity(),
                snapshot.member_binding.as_ref().expect("binding"),
                binding_b.clone(),
                "http://example.com/b.m3u8".intern(),
                |_| {},
            )
            .await
            .expect("the switch publishes the session");
        assert_eq!(first.binding_generation, 2, "switching member advances the generation");
        let second = manager
            .switch_legacy_group_member_binding(
                &user.username,
                &token,
                first,
                &binding_b,
                binding_a.clone(),
                "http://example.com/a2.m3u8".intern(),
                |_| {},
            )
            .await
            .expect("the switch publishes the session");
        assert_eq!(second.binding_generation, 3, "switching back advances the generation again");
        assert_eq!(second.incarnation, snapshot.identity().incarnation, "the session object never changed");

        // The stored member binding equals the snapshot's again; only the generation distinguishes
        // the two views, which is exactly the ABA case the fences must reject.
        let stored = manager.get_and_update_user_session(&user.username, &token).await.expect("session");
        assert_eq!(stored.member_binding, Some(binding_a.clone()));
        assert_eq!(stored.identity(), second);

        let stale_identity = snapshot.identity();
        assert_eq!(
            manager
                .update_legacy_failover_state_if_bound(
                    &user.username,
                    &token,
                    stale_identity,
                    &binding_a,
                    |state, _| state.last_served_sequence = 9,
                )
                .await,
            None,
            "a snapshot from before the first switch must not commit the pending member rebase"
        );
        assert!(
            !manager.mark_member_failed_on_segment(&user.username, &token, stale_identity, &binding_a).await,
            "a snapshot from before the first switch must not mark the rebound member failed"
        );
        let refreshed = manager
            .create_user_session_checked(
                CreateUserSessionParams {
                    user: &user,
                    session_token: &token,
                    virtual_id: 910,
                    provider: "prov-1",
                    stream_url: "http://example.com/a2.m3u8",
                    addr: &addr,
                    connection_permission: UserConnectionPermission::Allowed,
                    connection_kind: None,
                    socket_bound: false,
                },
                Some((stale_identity, snapshot.member_binding.as_ref())),
            )
            .await;
        assert_eq!(refreshed, None, "a stale playlist refresh answers the retry path");

        // The fence read after the switches is current and still commits.
        let committed = manager
            .update_legacy_failover_state_if_bound(&user.username, &token, stored.identity(), &binding_a, |state, _| {
                state.last_served_sequence = 11;
            })
            .await;
        assert_eq!(committed, Some(()), "a current fence keeps committing");
        let session = manager.get_and_update_user_session(&user.username, &token).await.expect("session");
        assert_eq!(session.legacy_rebase.as_ref().map(|rebase| rebase.last_served_sequence), Some(11));
    }

    /// The switch publishes onto the session object the trigger fenced: a session terminated and
    /// recreated under the same token while the switch resolves its member must not receive it.
    #[tokio::test]
    async fn switch_legacy_group_member_binding_rejects_a_recreated_session() {
        let manager = test_manager();
        let user = test_user();
        let addr = "127.0.0.1:8000".parse().unwrap();
        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-recreated",
                virtual_id: 700,
                provider: "prov-1",
                stream_url: "http://example.com/old.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;
        let binding_a = bound_member(700, 701, "prov-1");
        manager.bind_member(&user.username, &token, binding_a.clone()).await;
        let trigger_identity = manager.session_identity(&user.username, &token).await.expect("identity");

        // The session ends and a new request recreates it under the same token while the switch
        // resolves its member.
        assert!(manager.terminate_session(&user.username, &token).await, "the trigger's session ends");
        let recreated = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-recreated",
                virtual_id: 700,
                provider: "prov-1",
                stream_url: "http://example.com/old.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                socket_bound: false,
            })
            .await;
        assert_eq!(recreated, token, "the same token names the recreated session");
        let recreated_identity = manager.session_identity(&user.username, &token).await.expect("identity");
        assert_ne!(
            recreated_identity.incarnation, trigger_identity.incarnation,
            "the recreated session is a new object"
        );

        let mut counted = false;
        let switched = manager
            .switch_legacy_group_member_binding(
                &user.username,
                &token,
                trigger_identity,
                &binding_a,
                bound_member(700, 702, "prov-2"),
                "http://example.com/new.m3u8".intern(),
                |_| counted = true,
            )
            .await;
        assert_eq!(switched, None, "a switch whose trigger fence no longer matches the session must not publish");
        assert!(!counted, "a rejected switch must not apply its accounting");

        let session = manager.get_and_update_user_session(&user.username, &token).await.expect("session");
        assert_eq!(session.identity(), recreated_identity, "the recreated session keeps its identity");
        assert_eq!(session.member_binding, None, "the recreated session stays unbound");
        assert_eq!(session.provider, "prov-1".intern());
        assert_eq!(session.stream_url, "http://example.com/old.m3u8".intern());
        assert_eq!(session.legacy_rebase, None, "the rejected switch creates no timeline marker");
        assert!(session.legacy_failover_budget.is_none(), "the rejected switch applies no accounting");
    }
}
