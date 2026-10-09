use super::{
    url_host_key, ActiveUserManager, CreateUserSessionParams, DeferredWakes, PendingProviderReason,
    PendingProviderState, PendingProviderWakeSource, PlaybackLifecycle, SessionChangeSignal, SocketRegistration,
    UserConnectionData, UserSession, NEXT_SESSION_INCARNATION,
};
use crate::active_provider_manager::ConnectionKind;
use log::debug;
use shared::{
    defaults::{default_hls_session_ttl_secs, DASH_EXT, HLS_EXT},
    model::{PlaylistItemType, StreamInfo, UserConnectionPermission},
    utils::{
        current_time_secs, extract_extension_from_url, is_catchup_session_token, sanitize_sensitive_info, strip_port,
    },
};
use std::{
    borrow::Cow,
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::Notify;
use tuliprox_core::{model::Config, utils::debug_if_enabled};

pub(super) fn get_adaptive_session_ttl_secs(config: &Config) -> u64 {
    config
        .reverse_proxy
        .as_ref()
        .and_then(|r| r.stream.as_ref())
        .map_or_else(default_hls_session_ttl_secs, |s| s.hls_session_ttl_secs)
}

pub(super) fn stream_history_session_id(ts: u64, uid: u32) -> u64 { (ts << 32) | u64::from(uid) }

pub fn next_session_incarnation() -> u64 { NEXT_SESSION_INCARNATION.fetch_add(1, Ordering::Relaxed) }

impl Default for SessionChangeSignal {
    fn default() -> Self { Self { notify: Arc::new(Notify::new()), deferred: None, owner: true } }
}

impl Clone for SessionChangeSignal {
    fn clone(&self) -> Self { Self { notify: Arc::clone(&self.notify), deferred: self.deferred.clone(), owner: false } }
}

impl Drop for SessionChangeSignal {
    fn drop(&mut self) {
        if self.owner {
            self.signal();
        }
    }
}

impl std::fmt::Debug for SessionChangeSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionChangeSignal").field("owner", &self.owner).finish_non_exhaustive()
    }
}

/// A detached session with a fresh incarnation, for fixtures and struct-update construction.
/// Sessions in the connection table come from `create_user_session`.
impl Default for UserSession {
    fn default() -> Self {
        Self {
            token: String::new(),
            transition_version: 1,
            virtual_id: 0,
            provider: Arc::from(""),
            stream_url: Arc::from(""),
            provider_session_headers: HashMap::new(),
            provider_session_headers_host: None,
            provider_session_cookies: Arc::default(),
            incarnation: next_session_incarnation(),
            binding_generation: 0,
            change_signal: SessionChangeSignal::default(),
            media_started: Arc::new(AtomicBool::new(false)),
            user_agent_stream_index: None,
            addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            socket_bound: false,
            active_addrs: Vec::new(),
            ts: 0,
            started_at: 0,
            permission: UserConnectionPermission::Allowed,
            connection_kind: None,
            lifecycle: PlaybackLifecycle::default(),
        }
    }
}

impl UserSession {
    pub(super) fn clear_provider_session_cookies(&mut self) {
        if !self.provider_session_cookies.is_empty() {
            self.provider_session_cookies = Arc::default();
        }
    }

    /// Provider session headers that may be sent to `target_url`: only to the origin (scheme,
    /// host and port) that set them, or unconditionally when that origin is unknown.
    pub fn provider_session_headers_for(&self, target_url: &str) -> Option<Cow<'_, HashMap<String, String>>> {
        if !self.provider_session_cookies.is_empty() {
            return self.provider_session_cookies.headers_for(target_url);
        }
        if self.provider_session_headers.is_empty() {
            return None;
        }
        match self.provider_session_headers_host.as_deref() {
            Some(host) => (url_host_key(target_url).as_deref() == Some(host))
                .then_some(Cow::Borrowed(&self.provider_session_headers)),
            None => Some(Cow::Borrowed(&self.provider_session_headers)),
        }
    }
}

impl UserConnectionData {
    pub(super) fn has_session_addr(&self, addr: &SocketAddr) -> bool {
        self.sessions.iter().any(|session| session.addr == *addr || session.active_addrs.contains(addr))
    }
}

pub(super) fn is_stable_session_stream(stream: &StreamInfo) -> bool {
    // Catchup-token Live/.ts segment sockets must preserve too; otherwise archive panel rows
    // hard-remove every HLS chunk and Streams blinks even when frontend soft-preserve is present.
    stream.channel.item_type == PlaylistItemType::Catchup
        || stream.channel.item_type.is_live_adaptive()
        || stream.session_token.as_deref().is_some_and(is_catchup_session_token)
        || matches!(
            extract_extension_from_url(stream.channel.url.as_ref()),
            Some(ext) if ext == HLS_EXT || ext == DASH_EXT
        )
}

pub(super) fn uses_session_reentry_guard(stream: &StreamInfo) -> bool {
    stream.channel.item_type.requires_provider_affinity()
        || matches!(
            extract_extension_from_url(stream.channel.url.as_ref()),
            Some(ext) if ext == HLS_EXT || ext == DASH_EXT
        )
}

pub(super) fn remember_session_addr(session: &mut UserSession, addr: SocketAddr) {
    if session.socket_bound {
        session.active_addrs.clear();
    } else if let Some(position) = session.active_addrs.iter().position(|active_addr| *active_addr == addr) {
        session.active_addrs.remove(position);
    }
    session.active_addrs.push(addr);
    session.addr = addr;
}

pub(super) fn release_session_addr(session: &mut UserSession, addr: &SocketAddr) -> Option<SocketAddr> {
    if let Some(position) = session.active_addrs.iter().position(|active_addr| active_addr == addr) {
        session.active_addrs.remove(position);
    } else if session.addr != *addr {
        return None;
    }

    if session.addr == *addr {
        if let Some(next_addr) = session.active_addrs.last().copied() {
            session.addr = next_addr;
            return Some(next_addr);
        }
    }

    None
}

pub(super) fn clear_session_addr(session: &mut UserSession, addr: &SocketAddr) -> bool {
    let mut changed = false;
    if let Some(position) = session.active_addrs.iter().position(|active_addr| active_addr == addr) {
        session.active_addrs.remove(position);
        changed = true;
    }

    if session.addr == *addr {
        if let Some(next_addr) = session.active_addrs.last().copied() {
            session.addr = next_addr;
        } else {
            session.addr = SocketAddr::from(([0, 0, 0, 0], 0));
        }
        changed = true;
    }

    changed
}

pub(super) struct UserSessionParams<'a> {
    pub(super) change_wakes: &'a Arc<DeferredWakes>,
    pub(super) session_token: &'a str,
    pub(super) virtual_id: u32,
    pub(super) provider: &'a str,
    pub(super) stream_url: &'a str,
    pub(super) addr: &'a SocketAddr,
    pub(super) connection_permission: UserConnectionPermission,
    pub(super) connection_kind: Option<ConnectionKind>,
    pub(super) socket_bound: bool,
}

impl ActiveUserManager {
    pub async fn refresh_session_connection_kind_for_origin_policy(
        &self,
        username: &str,
        max_connections: u32,
        soft_connections: u16,
        session_token: &str,
    ) -> Option<ConnectionKind> {
        if max_connections == 0 && soft_connections == 0 {
            return Some(ConnectionKind::Normal);
        }

        let (connection_kind, promotions, divergence_snapshot) = {
            let mut connections = self.write_connections().await;
            let connection_data = connections.by_key.get_mut(username)?;
            connection_data.max_connections = max_connections;
            connection_data.soft_connections = soft_connections;

            let session_index = connection_data.sessions.iter().position(|session| session.token == session_token)?;

            let promotions = Self::promote_counted_soft_session_to_normal_if_available(connection_data, session_token);
            let connection_kind = if connection_data.sessions[session_index].lifecycle.is_counted()
                || Self::session_has_stream(connection_data, session_token)
            {
                Some(connection_data.sessions[session_index].connection_kind.unwrap_or(ConnectionKind::Normal))
            } else {
                let admission = self.check_connection_admission_with_counts(
                    username,
                    connection_data,
                    connection_data.effective_counts_for_admission(Some(session_token)),
                );
                if admission.permission() == UserConnectionPermission::Allowed {
                    if let Some(kind) = admission.kind() {
                        Self::update_session_admission(
                            &mut connection_data.sessions[session_index],
                            admission.permission(),
                            Some(kind),
                        );
                    }
                    admission.kind()
                } else {
                    None
                }
            };
            let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, username);

            (connection_kind, promotions, divergence_snapshot)
        };

        self.log_divergence_snapshot(divergence_snapshot).await;
        for action in promotions {
            self.emit_promotion_update(username, action).await;
        }

        connection_kind
    }

    pub(super) fn bump_session_transition_version(session: &mut UserSession) -> u64 {
        session.transition_version = session.transition_version.saturating_add(1);
        session.transition_version
    }

    pub(super) fn mark_session_committed(session: &mut UserSession, kind: ConnectionKind) {
        session.connection_kind = Some(kind);
        session.lifecycle = PlaybackLifecycle::Active;
        Self::bump_session_transition_version(session);
    }

    pub(super) fn session_has_stream(connection_data: &UserConnectionData, session_token: &str) -> bool {
        connection_data.streams.iter().any(|stream| stream.session_token.as_deref() == Some(session_token))
    }

    pub async fn ensure_user_session_placeholder(&self, request: CreateUserSessionParams<'_>) -> u64 {
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
        let connection_data = user_connections
            .by_key
            .entry(username.clone())
            .or_insert_with(|| UserConnectionData::new(0, user.max_connections, user.soft_connections));

        if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == session_token) {
            session.ts = current_time_secs();
            session.socket_bound = socket_bound;
            remember_session_addr(session, *addr);
            if session.connection_kind.is_none() {
                session.connection_kind = connection_kind;
            }
            if session.permission == UserConnectionPermission::Exhausted {
                Self::update_session_admission(session, connection_permission, None);
            }
            let version = Self::bump_session_transition_version(session);
            let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, &username);
            drop(user_connections);
            self.log_divergence_snapshot(divergence_snapshot).await;
            return version;
        }

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
        let version = session.transition_version;
        connection_data.add_session(session);
        let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, &username);
        drop(user_connections);
        self.log_divergence_snapshot(divergence_snapshot).await;
        version
    }

    pub async fn update_session_addr(&self, username: &str, token: &str, addr: &SocketAddr) {
        let now = current_time_secs();
        let mut user_connections = self.write_connections().await;
        if let Some(connection_data) = user_connections.by_key.get_mut(username) {
            let update_result = if let Some(session) = connection_data.sessions.iter_mut().find(|s| s.token == token) {
                let previous_addr = session.addr;
                remember_session_addr(session, *addr);
                session.ts = now;
                Self::bump_session_transition_version(session);
                for stream in &mut connection_data.streams {
                    if stream.addr == previous_addr && stream.session_token.as_deref() == Some(token) {
                        stream.addr = *addr;
                        stream.ts = now;
                    }
                }
                let prune_previous_registration = previous_addr != *addr
                    && !connection_data.has_session_addr(&previous_addr)
                    && !connection_data.streams.iter().any(|stream| stream.addr == previous_addr);
                Some((previous_addr, prune_previous_registration))
            } else {
                None
            };

            if let Some((previous_addr, prune_previous_registration)) = update_result {
                let registration =
                    user_connections.key_by_addr.entry(*addr).or_insert_with(SocketRegistration::anonymous);
                registration.add_user(username, now);
                if prune_previous_registration {
                    let can_remove_previous = user_connections
                        .key_by_addr
                        .get_mut(&previous_addr)
                        .is_some_and(|registration| registration.remove_user(username));
                    if can_remove_previous {
                        user_connections.key_by_addr.remove(&previous_addr);
                    }
                }
                debug_if_enabled!(
                    "Updated session {token} for {username} address {} -> {}",
                    sanitize_sensitive_info(&previous_addr.to_string()),
                    sanitize_sensitive_info(&addr.to_string())
                );
            }
        }
    }

    pub async fn update_session_provider_binding(
        &self,
        username: &str,
        token: &str,
        provider: Arc<str>,
        stream_url: Arc<str>,
    ) {
        let now = current_time_secs();
        let mut user_connections = self.write_connections().await;
        if let Some(connection_data) = user_connections.by_key.get_mut(username) {
            if let Some(session) = connection_data.sessions.iter_mut().find(|s| s.token == token) {
                let previous_provider = session.provider.clone();
                if session.provider != provider {
                    session.switch_provider(provider.clone());
                }
                session.stream_url = stream_url.clone();
                session.ts = now;
                Self::bump_session_transition_version(session);
                for stream in &mut connection_data.streams {
                    if stream.session_token.as_deref() == Some(token) {
                        stream.provider = provider.clone();
                        stream.ts = now;
                    }
                }
                debug_if_enabled!(
                    "Updated session {token} for {username} provider binding {} -> {}",
                    sanitize_sensitive_info(&previous_provider),
                    sanitize_sensitive_info(&provider)
                );
            }
        }
    }

    pub async fn clear_unbound_session_addr(&self, username: &str, token: &str, addr: &SocketAddr) {
        let now = current_time_secs();
        let mut user_connections = self.write_connections().await;
        let Some(connection_data) = user_connections.by_key.get_mut(username) else {
            return;
        };
        let addr_has_active_stream_for_session = connection_data
            .streams
            .iter()
            .any(|stream| stream.session_token.as_deref() == Some(token) && stream.addr == *addr && !stream.preserved);
        if addr_has_active_stream_for_session {
            return;
        }

        let cleared = if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == token)
        {
            let changed = clear_session_addr(session, addr);
            if changed {
                Self::bump_session_transition_version(session);
            }
            changed
        } else {
            false
        };
        let can_remove_registration = !connection_data.has_session_addr(addr)
            && !connection_data.streams.iter().any(|stream| stream.addr == *addr);
        if can_remove_registration {
            let can_remove = user_connections
                .key_by_addr
                .get_mut(addr)
                .is_some_and(|registration| registration.remove_user(username));
            if can_remove {
                user_connections.key_by_addr.remove(addr);
            }
        } else if cleared {
            if let Some(registration) = user_connections.key_by_addr.get_mut(addr) {
                registration.ts = now;
            }
        }
    }

    pub async fn mark_pending_provider(
        &self,
        username: &str,
        token: &str,
        reason_code: PendingProviderReason,
        deadline: u64,
    ) -> Option<u64> {
        let mut user_connections = self.write_connections().await;
        let connection_data = user_connections.by_key.get_mut(username)?;
        let now = current_time_secs();
        if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == token) {
            let version = match session.lifecycle {
                PlaybackLifecycle::PendingProvider { ref data } => data.version.saturating_add(1),
                _ => 1,
            };
            // Capture counted status BEFORE lifecycle transition to PendingProvider.
            // is_counted() returns false for PendingProvider, so we must check first.
            let kind = if session.lifecycle.is_counted() {
                Some(session.connection_kind.unwrap_or(ConnectionKind::Normal))
            } else {
                None
            };
            session.ts = now;
            Self::bump_session_transition_version(session);
            Self::update_session_admission(session, UserConnectionPermission::GracePeriod, None);
            session.lifecycle = PlaybackLifecycle::PendingProvider {
                data: PendingProviderState { reason_code, created_at: now, deadline, version, wake_source: None },
            };
            if let Some(kind) = kind {
                connection_data.decrement_kind(kind);
            }
            return Some(version);
        }
        None
    }

    pub async fn activate_pending_provider(
        &self,
        username: &str,
        token: &str,
        expected_version: u64,
        wake_source: PendingProviderWakeSource,
    ) {
        let mut user_connections = self.write_connections().await;
        let Some(connection_data) = user_connections.by_key.get_mut(username) else {
            return;
        };
        if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == token) {
            let PlaybackLifecycle::PendingProvider { data } = &mut session.lifecycle else {
                return;
            };
            if data.version != expected_version {
                return;
            }
            data.wake_source = Some(wake_source);
            Self::bump_session_transition_version(session);
            session.permission = UserConnectionPermission::Allowed;
            session.lifecycle = PlaybackLifecycle::Active;
        }
    }

    /// Returns the current `transition_version` if the session is in `GraceActive` lifecycle.
    /// Used by the grace task to confirm the session is still in `GraceActive` before committing.
    pub async fn grace_active_version(&self, username: &str, token: &str) -> Option<u64> {
        let connections = self.connections.read().await;
        let connection_data = connections.by_key.get(username)?;
        let session = connection_data.sessions.iter().find(|s| s.token == token)?;
        if session.lifecycle == PlaybackLifecycle::GraceActive {
            Some(session.transition_version)
        } else {
            None
        }
    }

    /// Marks a session as `GraceActive` — the session was granted immediate grace
    /// (`GraceMode::Instant`) and is provisionally active. The session counts against
    /// admission limits in this state.
    ///
    /// This corresponds to `Prepared -> GraceActive` in the playback state machine.
    /// The session remains in `GraceActive` until either:
    /// - `activate_grace_active` confirms it (grace window succeeded -> `GraceActive -> Active`)
    /// - `expire_grace_active` expires it (grace window failed -> `GraceActive -> Expired`)
    pub async fn mark_grace_active(&self, username: &str, token: &str) {
        let mut user_connections = self.write_connections().await;
        let Some(connection_data) = user_connections.by_key.get_mut(username) else {
            return;
        };
        let Some(session_index) = connection_data.sessions.iter().position(|session| session.token == token) else {
            return;
        };
        if connection_data.sessions[session_index].lifecycle == PlaybackLifecycle::GraceActive {
            return; // already grace active
        }
        // Collect fields while only borrowing sessions.
        let kind = connection_data.sessions[session_index].connection_kind.unwrap_or(ConnectionKind::Normal);
        let needs_count = !connection_data.sessions[session_index].lifecycle.is_counted();
        let now = current_time_secs();
        // Now mutate. Use index access to avoid nested &mut borrows.
        connection_data.sessions[session_index].ts = now;
        Self::bump_session_transition_version(&mut connection_data.sessions[session_index]);
        if needs_count {
            connection_data.increment_kind(kind);
        }
        connection_data.sessions[session_index].lifecycle = PlaybackLifecycle::GraceActive;
    }

    /// Activates a `GraceActive` session when the grace window resolves successfully.
    ///
    /// This corresponds to `GraceActive -> Active` in the playback state machine.
    /// The session remains counted and the kind counts are already correct from
    /// the `GraceActive` provisional state.
    pub async fn activate_grace_active(&self, username: &str, token: &str, expected_version: u64) {
        let mut user_connections = self.write_connections().await;
        let Some(connection_data) = user_connections.by_key.get_mut(username) else {
            return;
        };
        if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == token) {
            if session.transition_version != expected_version {
                return;
            }
            if session.lifecycle != PlaybackLifecycle::GraceActive {
                return;
            }
            Self::bump_session_transition_version(session);
            session.lifecycle = PlaybackLifecycle::Active;
            session.permission = UserConnectionPermission::Allowed;
        }
    }

    /// Expires a `GraceActive` session when the grace window fails.
    ///
    /// This corresponds to `GraceActive -> Expired` in the playback state machine.
    /// Releases the provisional counted lease.
    pub async fn expire_grace_active(&self, username: &str, token: &str, expected_version: u64) {
        let (connection_changed, removed_count) = {
            let mut user_connections = self.write_connections().await;
            let Some(connection_data) = user_connections.by_key.get_mut(username) else {
                return;
            };
            let Some(session_index) = connection_data.sessions.iter().position(|session| session.token == token) else {
                return;
            };

            if connection_data.sessions[session_index].transition_version != expected_version {
                return;
            }
            if connection_data.sessions[session_index].lifecycle != PlaybackLifecycle::GraceActive {
                return;
            }

            // Release the provisional counted lease using index-based access
            // to avoid nested mutable borrows with connection_data methods.
            let mut connection_changed = false;
            let mut counted_kind: Option<ConnectionKind> = None;
            if connection_data.sessions[session_index].lifecycle.is_counted() {
                counted_kind = connection_data.sessions[session_index].connection_kind;
                connection_changed = true;
            }
            if let Some(kind) = counted_kind {
                connection_data.decrement_kind(kind);
            }

            // Expire the session. Lifecycle change alone handles counted state (Expired is not counted).
            connection_data.sessions[session_index].expire();
            connection_data.sessions[session_index].set_permission(UserConnectionPermission::Exhausted);
            Self::bump_session_transition_version(&mut connection_data.sessions[session_index]);

            // Collect addresses for stream cleanup.
            let mut addrs = Vec::new();
            let session_addr = connection_data.sessions[session_index].addr;
            if !session_addr.ip().is_unspecified() {
                addrs.push(session_addr);
            }
            for addr in &connection_data.sessions[session_index].active_addrs {
                if *addr != session_addr && !addrs.contains(addr) {
                    addrs.push(*addr);
                }
            }

            // Remove all streams for these addresses (never preserve on expire).
            let mut removed_count = 0;
            for addr in &addrs {
                if let Some(stream_idx) =
                    connection_data.streams.iter().position(|stream| stream.addr == *addr && !stream.preserved)
                {
                    if let Some(kind) = connection_data.stream_kinds.remove(&connection_data.streams[stream_idx].uid) {
                        connection_data.decrement_kind(kind);
                    }
                    connection_data.stream_normal_priorities.remove(&connection_data.streams[stream_idx].uid);
                    connection_data.remove_stream_request_claims(connection_data.streams[stream_idx].uid);
                    connection_data.streams.swap_remove(stream_idx);
                    removed_count += 1;
                }
            }

            // Reset grace if no connections remain.
            if connection_data.connections == 0 && connection_data.streams.is_empty() {
                connection_data.granted_grace = false;
                connection_data.grace_ts = 0;
            }

            let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, username);
            drop(user_connections);
            self.log_divergence_snapshot(divergence_snapshot).await;

            (connection_changed, removed_count)
        };

        if connection_changed {
            self.log_active_user().await;
        }
        debug!("GraceActive expired for session {token} in {username}, released {removed_count} streams");
    }

    /// Terminates the session and all associated streams for a playback.
    ///
    /// This is the explicit `Terminate` path from the playback state machine:
    /// - Removes all streams associated with this session token (never preserves)
    /// - Releases the counted lease if held
    /// - Sets lifecycle to `Expired`
    /// - Clears pending-provider state
    ///
    /// Unlike `release_unbound_session_reservation`, this terminates regardless of
    /// whether streams are currently active, and always removes associated streams.
    /// Returns `true` only when the session existed and was removed by this call.
    pub async fn terminate_session(&self, username: &str, session_token: &str) -> bool {
        let (connection_changed, removed_count, promotions) = {
            let mut user_connections = self.write_connections().await;
            let Some(connection_data) = user_connections.by_key.get_mut(username) else {
                return false;
            };

            let Some(session_index) =
                connection_data.sessions.iter().position(|session| session.token == session_token)
            else {
                return false;
            };

            let counted_kind = connection_data.sessions[session_index]
                .lifecycle
                .is_counted()
                .then(|| connection_data.sessions[session_index].connection_kind.unwrap_or(ConnectionKind::Normal));
            let (removed_count, connection_changed) =
                connection_data.remove_streams_for_session_and_release_counted(session_token, counted_kind);

            // Expire and remove the session immediately. Unlike `release_unbound_session_reservation`
            // which keeps the expired session for TTL-based GC cleanup, terminate_session explicitly
            // removes the session from the list so `get_and_update_user_session` returns None.
            connection_data.sessions.swap_remove(session_index);

            // Reset grace if no connections remain.
            if connection_data.connections == 0 && connection_data.streams.is_empty() {
                connection_data.granted_grace = false;
                connection_data.grace_ts = 0;
            }

            let promotions = Self::collect_promotions_after_capacity_release(connection_data);

            let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, username);
            drop(user_connections);
            self.log_divergence_snapshot(divergence_snapshot).await;

            (connection_changed, removed_count, promotions)
        };

        if connection_changed {
            self.log_active_user().await;
        }
        for action in promotions {
            self.emit_promotion_update(username, action).await;
        }
        debug!("Terminated session {session_token} for user {username}, released {removed_count} streams");
        true
    }

    /// Terminates all sessions associated with a given socket address for a user.
    ///
    /// This is used when a connection is explicitly kicked — the session should be
    /// expired and removed immediately rather than waiting for TTL-based GC cleanup.
    ///
    /// Removes all sessions whose `addr` or `active_addrs` contains `kick_addr`.
    pub async fn terminate_sessions_for_addr(&self, username: &str, kick_addr: &SocketAddr) {
        let (connection_changed, removed_count, promotions) = {
            let mut user_connections = self.write_connections().await;
            let Some(connection_data) = user_connections.by_key.get_mut(username) else {
                return;
            };

            // Collect tokens of sessions associated with the kicked addr.
            let tokens_to_remove: Vec<String> = connection_data
                .sessions
                .iter()
                .filter(|session| session.addr == *kick_addr || session.active_addrs.contains(kick_addr))
                .map(|session| session.token.clone())
                .collect();

            if tokens_to_remove.is_empty() {
                return;
            }

            let mut removed_count = 0;
            let mut connection_changed = false;

            for token in &tokens_to_remove {
                let Some(session_index) = connection_data.sessions.iter().position(|s| s.token == *token) else {
                    continue;
                };

                let counted_kind = connection_data.sessions[session_index]
                    .lifecycle
                    .is_counted()
                    .then(|| connection_data.sessions[session_index].connection_kind.unwrap_or(ConnectionKind::Normal));

                let (_, session_connection_changed) =
                    connection_data.remove_streams_for_session_and_release_counted(token, counted_kind);
                connection_changed |= session_connection_changed;

                // Expire and remove the session.
                connection_data.sessions.swap_remove(session_index);
                removed_count += 1;
            }

            // Reset grace if no connections remain.
            if connection_data.connections == 0 && connection_data.streams.is_empty() {
                connection_data.granted_grace = false;
                connection_data.grace_ts = 0;
            }

            let promotions = Self::collect_promotions_after_capacity_release(connection_data);

            let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, username);
            user_connections.mark_sessions_ended(tokens_to_remove);
            drop(user_connections);
            self.log_divergence_snapshot(divergence_snapshot).await;

            (connection_changed, removed_count, promotions)
        };

        if connection_changed {
            self.log_active_user().await;
        }
        for action in promotions {
            self.emit_promotion_update(username, action).await;
        }
        debug!("Terminated {removed_count} sessions for user {username} at addr {kick_addr}");
    }

    pub async fn expire_pending_provider(
        &self,
        username: &str,
        token: &str,
        expected_version: u64,
        wake_source: PendingProviderWakeSource,
    ) {
        let mut user_connections = self.write_connections().await;
        let Some(connection_data) = user_connections.by_key.get_mut(username) else {
            return;
        };
        let Some(session_index) = connection_data.sessions.iter().position(|session| session.token == token) else {
            return;
        };
        let pending_version = match &connection_data.sessions[session_index].lifecycle {
            PlaybackLifecycle::PendingProvider { data } => data.version,
            _ => return,
        };
        if pending_version != expected_version {
            return;
        }
        // Capture counted status BEFORE lifecycle changes.
        // PendingProvider is not counted (is_counted() = false), so checking here
        // captures whether there is a previously-counted lease to release.
        let kind_to_release = if connection_data.sessions[session_index].lifecycle.is_counted() {
            Some(connection_data.sessions[session_index].connection_kind.unwrap_or(ConnectionKind::Normal))
        } else {
            None
        };
        let session = &mut connection_data.sessions[session_index];
        Self::clear_session_pending_with_permission(session, UserConnectionPermission::Exhausted, wake_source);
        session.expire();
        if let Some(kind) = kind_to_release {
            connection_data.decrement_kind(kind);
        }
    }

    pub async fn adaptive_session_stream_cleanup_addrs(
        &self,
        username: &str,
        session_token: &str,
        current_addr: &SocketAddr,
    ) -> Vec<SocketAddr> {
        let connections = self.connections.read().await;
        let Some(connection_data) = connections.by_key.get(username) else {
            return Vec::new();
        };

        let mut addrs = Vec::new();
        for stream in
            connection_data.streams.iter().filter(|stream| stream.session_token.as_deref() == Some(session_token))
        {
            if stream.addr != *current_addr && !addrs.contains(&stream.addr) {
                addrs.push(stream.addr);
            }
        }
        let current_addr_string = current_addr.to_string();
        let current_ip = strip_port(&current_addr_string).to_string();
        if let Some(session) = connection_data.sessions.iter().find(|session| session.token == session_token) {
            for addr in &session.active_addrs {
                let addr_string = addr.to_string();
                let addr_ip = strip_port(&addr_string);
                if *addr != *current_addr && addr_ip == current_ip && !addrs.contains(addr) {
                    addrs.push(*addr);
                }
            }
        }
        addrs
    }

    pub async fn pending_provider_version(&self, username: &str, token: &str) -> Option<u64> {
        let user_connections = self.connections.read().await;
        let connection_data = user_connections.by_key.get(username)?;
        let session = connection_data.sessions.iter().find(|session| session.token == token)?;
        match &session.lifecycle {
            PlaybackLifecycle::PendingProvider { data } => Some(data.version),
            _ => None,
        }
    }

    pub(super) fn should_preserve_session_stream(stream: &StreamInfo) -> bool {
        stream.session_token.is_some() && is_stable_session_stream(stream)
    }
}
