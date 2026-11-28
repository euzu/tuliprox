use super::{
    decide_connection_kind, ActiveUserManager, AdmissionRejectionReason, ConnectionAdmission, CreateUserSessionParams,
    PendingProviderWakeSource, PlaybackLifecycle, PromotionAction, SessionIdentity, UserConnectionCounts,
    UserConnectionData, UserSession, USER_CON_TTL,
};
use crate::active_provider_manager::{ConnectionKind, ProviderReleaseSnapshot};
use jsonwebtoken::get_current_timestamp;
use log::debug;
use shared::model::{StreamInfo, UserConnectionPermission};
use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
};
use tuliprox_core::model::AllocationId;

/// Panel identity of a removed stream row, as carried by `DisconnectedStream`.
pub(super) type RemovedStreamKey = (SocketAddr, u32);

pub(crate) struct RemovedPlaybackIdStream {
    pub(crate) stream_info: StreamInfo,
    pub(crate) close_socket: bool,
    pub(crate) release_provider: bool,
    pub(crate) addr_disconnected: bool,
}

pub(crate) struct PlaybackIdReplacementEffect {
    pub(crate) removed_streams: Vec<RemovedPlaybackIdStream>,
    pub(crate) ended_session_tokens: Vec<String>,
}

impl PlaybackLifecycle {
    /// Returns true for lifecycle states that own a counted admission lease.
    /// Both `Active` and `GraceActive` count — `GraceActive` is a provisional
    /// counted state for `GraceMode::Instant` sessions.
    pub fn is_counted(&self) -> bool { matches!(self, Self::Active | Self::GraceActive) }
}

impl UserSession {
    pub(super) fn set_permission(&mut self, permission: UserConnectionPermission) {
        let exhausted_now = permission == UserConnectionPermission::Exhausted && self.permission != permission;
        self.permission = permission;
        if exhausted_now {
            self.change_signal.signal();
        }
    }
}

impl ConnectionAdmission {
    /// Maps a stored session permission onto an admission. An exhausted session is
    /// classified as a genuine connection-limit rejection so the
    /// `Exhausted ⇒ rejection_reason.is_some()` invariant always holds.
    pub fn from_permission(permission: UserConnectionPermission, kind: Option<ConnectionKind>) -> Self {
        match permission {
            UserConnectionPermission::Allowed => Self::allowed(kind),
            UserConnectionPermission::GracePeriod => Self::grace_period(kind),
            UserConnectionPermission::Exhausted => {
                Self::exhausted(AdmissionRejectionReason::UserConnectionsExhausted, kind)
            }
        }
    }

    pub fn permission(&self) -> UserConnectionPermission { self.permission }
}

impl UserConnectionData {
    /// Removes one stream row together with its kind, priority, playback id, and request-claim accounting.
    /// Returns the removed `StreamInfo` and whether a counted connection kind was released.
    pub(super) fn remove_stream_info_at(&mut self, stream_idx: usize) -> (StreamInfo, bool) {
        let uid = self.streams[stream_idx].uid;
        let released_kind = self.stream_kinds.remove(&uid).inspect(|kind| self.decrement_kind(*kind)).is_some();
        self.stream_normal_priorities.remove(&uid);
        self.stream_playback_ids.remove(&uid);
        self.remove_stream_request_claims(uid);
        let removed = self.streams.swap_remove(stream_idx);
        (removed, released_kind)
    }

    pub(super) fn remove_session_stream_infos_and_release_counted(
        &mut self,
        session_token: &str,
        counted_kind: Option<ConnectionKind>,
    ) -> (Vec<StreamInfo>, bool) {
        let mut removed_streams = Vec::new();
        let mut released_stream_kind = false;
        let mut stream_idx = 0;
        while stream_idx < self.streams.len() {
            if self.streams[stream_idx].session_token.as_deref() != Some(session_token) {
                stream_idx += 1;
                continue;
            }
            let (removed, released_kind) = self.remove_stream_info_at(stream_idx);
            released_stream_kind |= released_kind;
            removed_streams.push(removed);
        }
        let mut connection_changed = released_stream_kind;

        if let Some(kind) = counted_kind.filter(|_| !released_stream_kind) {
            self.decrement_kind(kind);
            connection_changed = true;
        }

        (removed_streams, connection_changed)
    }

    pub(super) fn remove_streams_for_session_and_release_counted(
        &mut self,
        session_token: &str,
        counted_kind: Option<ConnectionKind>,
    ) -> (Vec<RemovedStreamKey>, bool) {
        let (removed_streams, connection_changed) =
            self.remove_session_stream_infos_and_release_counted(session_token, counted_kind);
        let keys = removed_streams.into_iter().map(|s| (s.addr, s.uid)).collect();
        (keys, connection_changed)
    }

    pub(super) fn try_promote_soft_stream(&mut self) -> Option<PromotionAction> {
        if self.counts.normal >= self.max_connections
            || (u32::from(self.counts.soft)) <= u32::from(self.soft_connections)
        {
            return None;
        }

        let candidate_uid = self
            .streams
            .iter()
            .filter(|stream| !stream.preserved)
            .filter_map(|stream| {
                let kind = self.stream_kinds.get(&stream.uid).copied()?;
                if kind != ConnectionKind::Soft {
                    return None;
                }
                let normal_priority = self.stream_normal_priorities.get(&stream.uid).copied().unwrap_or_default();
                Some((normal_priority, stream.ts, stream.uid, stream.addr))
            })
            .min_by_key(|(normal_priority, ts, uid, _)| (*normal_priority, *ts, *uid));

        let (new_priority, _ts, uid, addr) = candidate_uid?;

        self.counts.normal = self.counts.normal.saturating_add(1);
        if self.counts.soft > 0 {
            self.counts.soft -= 1;
        }
        self.stream_kinds.insert(uid, ConnectionKind::Normal);

        Some(PromotionAction { addr, uid, new_priority })
    }

    pub(super) fn try_promote_soft_session_reservation(&mut self) -> bool {
        if self.counts.normal >= self.max_connections
            || (u32::from(self.counts.soft)) <= u32::from(self.soft_connections)
        {
            return false;
        }

        let active_tokens =
            self.streams.iter().filter_map(|stream| stream.session_token.as_deref()).collect::<HashSet<_>>();

        let candidate_index = self.sessions.iter().position(|session| {
            session.lifecycle.is_counted()
                && session.connection_kind == Some(ConnectionKind::Soft)
                && !active_tokens.contains(session.token.as_str())
        });

        let Some(candidate_index) = candidate_index else {
            return false;
        };

        self.counts.normal = self.counts.normal.saturating_add(1);
        if self.counts.soft > 0 {
            self.counts.soft -= 1;
        }
        self.sessions[candidate_index].connection_kind = Some(ConnectionKind::Normal);
        true
    }

    pub(super) fn effective_counts_for_admission(&self, exclude_session_token: Option<&str>) -> UserConnectionCounts {
        let mut counts = self.counts;
        let counted_tokens = self
            .sessions
            .iter()
            .filter(|session| session.lifecycle.is_counted())
            .map(|session| session.token.as_str())
            .collect::<HashSet<_>>();
        let mut reserved_tokens = HashSet::new();

        for stream in self.streams.iter().filter(|stream| stream.preserved) {
            // Orphan preserved stream: no session token means no session to evict.
            // Do not count it — it has no bearing on admission decisions.
            let Some(session_token) = stream.session_token.as_deref() else {
                continue;
            };
            if exclude_session_token.is_some_and(|token| token == session_token)
                || counted_tokens.contains(session_token)
                || !reserved_tokens.insert(session_token)
            {
                continue;
            }

            let kind = self
                .sessions
                .iter()
                .find(|session| session.token == session_token)
                .and_then(|session| session.connection_kind)
                .unwrap_or(ConnectionKind::Normal);
            match kind {
                ConnectionKind::Normal => counts.normal = counts.normal.saturating_add(1),
                ConnectionKind::Soft => counts.soft = counts.soft.saturating_add(1),
            }
        }

        counts
    }
}

impl ActiveUserManager {
    pub async fn pending_release_allocations(&self, username: &str) -> Vec<AllocationId> {
        self.pending_provider_release(username).await.map_or_else(Vec::new, |snapshot| snapshot.single_allocations)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_pending_release_for_test(&self, username: &str, allocations: &[AllocationId]) {
        self.set_pending_provider_release(
            username,
            crate::active_provider_manager::ProviderReleaseSnapshot::from_single_allocations(
                SocketAddr::from(([127, 0, 0, 1], 0)),
                allocations.to_vec(),
            ),
        )
        .await;
    }

    pub(crate) async fn pending_provider_release(&self, username: &str) -> Option<ProviderReleaseSnapshot> {
        self.pending_provider_releases.lock().await.get(username).cloned()
    }

    pub(crate) async fn set_pending_provider_release(&self, username: &str, snapshot: ProviderReleaseSnapshot) {
        self.pending_provider_releases.lock().await.insert(username.to_owned(), snapshot);
    }

    pub(crate) async fn clear_pending_provider_release(
        &self,
        username: &str,
        snapshot: &ProviderReleaseSnapshot,
    ) -> bool {
        let mut pending = self.pending_provider_releases.lock().await;
        if pending.get(username) != Some(snapshot) {
            return false;
        }
        pending.remove(username);
        true
    }

    pub(super) fn check_connection_admission_with_counts(
        &self,
        username: &str,
        connection_data: &mut UserConnectionData,
        counts: UserConnectionCounts,
    ) -> ConnectionAdmission {
        let selected_kind =
            decide_connection_kind(counts, connection_data.max_connections, connection_data.soft_connections);
        let effective_connections = counts.normal.saturating_add(u32::from(counts.soft));

        if let Some(kind) = selected_kind {
            // Reset grace only once the user is back below the hard limit.
            if effective_connections < connection_data.max_connections {
                connection_data.granted_grace = false;
                connection_data.grace_ts = 0;
            }
            return ConnectionAdmission::allowed(Some(kind));
        }

        let now = get_current_timestamp();
        // Check if user already used a grace period
        if connection_data.granted_grace {
            if effective_connections >= connection_data.max_connections
                && now - connection_data.grace_ts <= self.grace_period_timeout_secs.load(Ordering::Relaxed)
            {
                // Grace timeout, still active, deny connection
                debug!("User access denied, grace exhausted, too many connections: {username}");
                return ConnectionAdmission::exhausted(AdmissionRejectionReason::UserConnectionsExhausted, None);
            }
            // Grace timeout expired, reset grace counters
            if effective_connections < connection_data.max_connections {
                connection_data.granted_grace = false;
                connection_data.grace_ts = 0;
            }
        }

        debug!("User access denied, too many connections: {username}");
        ConnectionAdmission::exhausted(AdmissionRejectionReason::UserConnectionsExhausted, None)
    }

    pub(super) fn check_connection_admission(
        &self,
        username: &str,
        connection_data: &mut UserConnectionData,
    ) -> ConnectionAdmission {
        self.check_connection_admission_with_counts(
            username,
            connection_data,
            connection_data.effective_counts_for_admission(None),
        )
    }

    pub async fn connection_admission(
        &self,
        username: &str,
        max_connections: u32,
        soft_connections: u16,
    ) -> ConnectionAdmission {
        if max_connections > 0 || soft_connections > 0 {
            if let Some(connection_data) = self.write_connections().await.by_key.get_mut(username) {
                connection_data.max_connections = max_connections;
                connection_data.soft_connections = soft_connections;
                return self.check_connection_admission(username, connection_data);
            }
        }
        ConnectionAdmission::allowed(Some(ConnectionKind::Normal))
    }

    pub async fn connection_permission(
        &self,
        username: &str,
        max_connections: u32,
        soft_connections: u16,
    ) -> UserConnectionPermission {
        self.connection_admission(username, max_connections, soft_connections).await.permission
    }

    pub async fn connection_admission_for_session(
        &self,
        username: &str,
        max_connections: u32,
        soft_connections: u16,
        session_token: &str,
    ) -> ConnectionAdmission {
        if max_connections == 0 && soft_connections == 0 {
            return ConnectionAdmission::allowed(Some(ConnectionKind::Normal));
        }

        let mut connections = self.write_connections().await;
        let Some(connection_data) = connections.by_key.get_mut(username) else {
            return ConnectionAdmission::allowed(Some(ConnectionKind::Normal));
        };
        connection_data.max_connections = max_connections;
        connection_data.soft_connections = soft_connections;

        let Some(session_index) = connection_data.sessions.iter().position(|session| session.token == session_token)
        else {
            return self.check_connection_admission(username, connection_data);
        };

        if connection_data.sessions[session_index].lifecycle.is_counted() {
            return ConnectionAdmission::allowed(
                connection_data.sessions[session_index].connection_kind.or(Some(ConnectionKind::Normal)),
            );
        }

        self.check_connection_admission_with_counts(
            username,
            connection_data,
            connection_data.effective_counts_for_admission(Some(session_token)),
        )
    }

    pub async fn connection_permission_for_session(
        &self,
        username: &str,
        max_connections: u32,
        soft_connections: u16,
        session_token: &str,
    ) -> UserConnectionPermission {
        self.connection_admission_for_session(username, max_connections, soft_connections, session_token)
            .await
            .permission()
    }

    pub async fn grant_grace(&self, username: &str) -> bool {
        if self.grace_period_millis.load(Ordering::Relaxed) == 0 {
            debug!("Grace grant denied, grace_period_millis is zero for {username}");
            return false;
        }
        let mut connections = self.write_connections().await;
        if let Some(connection_data) = connections.by_key.get_mut(username) {
            let now = get_current_timestamp();
            if connection_data.connections < connection_data.max_connections {
                debug!(
                    "Grace grant denied for {username}, user not at connection limit ({}/{})",
                    connection_data.connections, connection_data.max_connections
                );
                return false;
            }
            if connection_data.granted_grace
                && connection_data.connections >= connection_data.max_connections
                && now - connection_data.grace_ts <= self.grace_period_timeout_secs.load(Ordering::Relaxed)
            {
                debug!("Grace grant denied, still within active grace timeout for {username}");
                return false;
            }
            connection_data.granted_grace = true;
            connection_data.grace_ts = now;
            debug!("Granted a grace period for user access: {username}");
            return true;
        }
        false
    }

    pub(super) fn promote_session_for_stream(connection_data: &mut UserConnectionData, stream: &StreamInfo) {
        if let Some(token) = stream.session_token.as_deref() {
            if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == token) {
                Self::mark_session_committed(session, ConnectionKind::Normal);
            }
        }
    }

    pub(super) fn collect_promotions_after_capacity_release(
        connection_data: &mut UserConnectionData,
    ) -> Vec<PromotionAction> {
        let mut promotions = Vec::new();
        while let Some(action) = connection_data.try_promote_soft_stream() {
            let promoted_stream = connection_data.streams.iter().find(|stream| stream.uid == action.uid).cloned();
            if let Some(stream) = promoted_stream.as_ref() {
                Self::promote_session_for_stream(connection_data, stream);
            }
            promotions.push(action);
        }
        while connection_data.try_promote_soft_session_reservation() {}
        promotions
    }

    pub(super) fn promote_counted_soft_session_to_normal_if_available(
        connection_data: &mut UserConnectionData,
        session_token: &str,
    ) -> Vec<PromotionAction> {
        if connection_data.max_connections > 0 && connection_data.counts.normal >= connection_data.max_connections {
            return Vec::new();
        }

        let Some(session_index) = connection_data.sessions.iter().position(|session| {
            session.token == session_token
                && session.lifecycle.is_counted()
                && session.connection_kind == Some(ConnectionKind::Soft)
        }) else {
            return Vec::new();
        };

        if connection_data.counts.soft == 0 {
            return Vec::new();
        }

        connection_data.counts.normal = connection_data.counts.normal.saturating_add(1);
        connection_data.counts.soft = connection_data.counts.soft.saturating_sub(1);
        connection_data.sessions[session_index].connection_kind = Some(ConnectionKind::Normal);
        Self::bump_session_transition_version(&mut connection_data.sessions[session_index]);

        let mut promotions = Vec::new();
        for stream in
            connection_data.streams.iter().filter(|stream| stream.session_token.as_deref() == Some(session_token))
        {
            if connection_data.stream_kinds.get(&stream.uid) != Some(&ConnectionKind::Soft) {
                continue;
            }
            let new_priority = connection_data.stream_normal_priorities.get(&stream.uid).copied().unwrap_or_default();
            connection_data.stream_kinds.insert(stream.uid, ConnectionKind::Normal);
            promotions.push(PromotionAction { addr: stream.addr, uid: stream.uid, new_priority });
        }
        promotions
    }

    pub(super) fn update_session_admission(
        session: &mut UserSession,
        permission: UserConnectionPermission,
        kind: Option<ConnectionKind>,
    ) {
        session.set_permission(permission);
        if let Some(kind) = kind {
            session.connection_kind = Some(kind);
        }
    }

    pub(super) fn clear_session_pending_with_permission(
        session: &mut UserSession,
        permission: UserConnectionPermission,
        wake_source: PendingProviderWakeSource,
    ) {
        if let PlaybackLifecycle::PendingProvider { data } = &mut session.lifecycle {
            data.wake_source = Some(wake_source);
        }
        Self::bump_session_transition_version(session);
        session.set_permission(permission);
    }

    pub(super) fn clear_session_counted_without_stream(connection_data: &mut UserConnectionData, session_token: &str) {
        if Self::session_has_stream(connection_data, session_token) {
            return;
        }
        if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == session_token) {
            match session.lifecycle {
                PlaybackLifecycle::Active => {
                    session.lifecycle = PlaybackLifecycle::Preserved;
                }
                // GraceActive without a stream: grace failed, expire the session.
                // This can happen when the grace window times out while the client
                // is still connecting but hasn't opened a stream yet.
                PlaybackLifecycle::GraceActive => {
                    session.expire();
                }
                _ => {}
            }
        }
    }

    pub(super) fn clear_session_counted(connection_data: &mut UserConnectionData, session_token: &str) {
        if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == session_token) {
            match session.lifecycle {
                PlaybackLifecycle::Active => {
                    session.lifecycle = PlaybackLifecycle::Preserved;
                }
                PlaybackLifecycle::GraceActive => {
                    // GraceActive without stream: the grace failed. The stream was already
                    // removed (this function is called after stream removal), so expire the session.
                    session.expire();
                }
                _ => {}
            }
        }
    }

    pub(super) fn release_expired_session_reservations(connection_data: &mut UserConnectionData, now: u64) {
        let expired_counted = connection_data
            .sessions
            .iter()
            .filter(|session| session.lifecycle.is_counted())
            .filter(|session| now.saturating_sub(session.ts) >= USER_CON_TTL)
            .filter(|session| !Self::session_has_stream(connection_data, session.token.as_str()))
            .map(|session| (session.token.clone(), session.connection_kind.unwrap_or(ConnectionKind::Normal)))
            .collect::<Vec<_>>();

        for (_, kind) in &expired_counted {
            connection_data.decrement_kind(*kind);
        }
        for (token, _) in expired_counted {
            Self::clear_session_counted_without_stream(connection_data, &token);
        }
        while connection_data.try_promote_soft_session_reservation() {}
    }

    pub async fn connection_admission_for_session_activation(
        &self,
        username: &str,
        max_connections: u32,
        soft_connections: u16,
        session_token: &str,
    ) -> ConnectionAdmission {
        if max_connections == 0 && soft_connections == 0 {
            return ConnectionAdmission::allowed(Some(ConnectionKind::Normal));
        }

        let mut connections = self.write_connections().await;
        let Some(connection_data) = connections.by_key.get_mut(username) else {
            return ConnectionAdmission::allowed(Some(ConnectionKind::Normal));
        };
        connection_data.max_connections = max_connections;
        connection_data.soft_connections = soft_connections;

        let Some(session_index) = connection_data.sessions.iter().position(|session| session.token == session_token)
        else {
            return self.check_connection_admission(username, connection_data);
        };

        // Existing counted session or any stream row for this token (including soft-preserved
        // HLS/Catchup between segments): entitled to its slot. #807 switched this to
        // active-only stream checks, so every LiveHls segment gap returned Exhausted
        // and kick-evicted/terminated the same session (retry storm since v3.3.79).
        if connection_data.sessions[session_index].lifecycle.is_counted()
            || Self::session_has_stream(connection_data, session_token)
        {
            return ConnectionAdmission::allowed(
                connection_data.sessions[session_index].connection_kind.or(Some(ConnectionKind::Normal)),
            );
        }

        // Uncounted session with no stream row: run normal admission.
        // Same-token soft-preserve is handled above via `session_has_stream` (3.3.78 semantics).
        // Do not return Exhausted for own preserved rows — that forced self-eviction on HLS gaps.
        let admission = self.evaluate_admission_for_request_in_data(
            username,
            connection_data,
            max_connections,
            soft_connections,
            true,
            Some(session_token),
            true,
        );
        if admission.permission() == UserConnectionPermission::Allowed {
            if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == session_token) {
                Self::update_session_admission(session, admission.permission(), admission.kind());
            }
        }
        admission
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Admission decision depends on limits and session activation flags under one lock."
    )]
    pub(super) fn evaluate_admission_for_request_in_data(
        &self,
        username: &str,
        connection_data: &mut UserConnectionData,
        max_connections: u32,
        soft_connections: u16,
        use_session_admission: bool,
        session_token: Option<&str>,
        activate_unbound_session: bool,
    ) -> ConnectionAdmission {
        if max_connections == 0 && soft_connections == 0 {
            return ConnectionAdmission::allowed(Some(ConnectionKind::Normal));
        }

        connection_data.max_connections = max_connections;
        connection_data.soft_connections = soft_connections;

        if use_session_admission {
            if let Some(token) = session_token {
                if let Some(session_index) = connection_data.sessions.iter().position(|session| session.token == token)
                {
                    if activate_unbound_session {
                        if connection_data.sessions[session_index].lifecycle.is_counted()
                            || Self::session_has_stream(connection_data, token)
                        {
                            return ConnectionAdmission::allowed(
                                connection_data.sessions[session_index]
                                    .connection_kind
                                    .or(Some(ConnectionKind::Normal)),
                            );
                        }
                    } else if connection_data.sessions[session_index].lifecycle.is_counted() {
                        return ConnectionAdmission::allowed(
                            connection_data.sessions[session_index].connection_kind.or(Some(ConnectionKind::Normal)),
                        );
                    }
                    return self.check_connection_admission_with_counts(
                        username,
                        connection_data,
                        connection_data.effective_counts_for_admission(Some(token)),
                    );
                }
            }
        }

        self.check_connection_admission(username, connection_data)
    }

    #[expect(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "Replacing an old playback-id connection, creating/tagging the caller placeholder, and admitting it must share one write_connections critical section."
    )]
    pub(crate) async fn replace_playback_id_and_connection_admission_with_placeholder(
        &self,
        username: &str,
        max_connections: u32,
        soft_connections: u16,
        request_addr: &SocketAddr,
        playback_id: &str,
        use_session_admission: bool,
        session_token: Option<&str>,
        activate_unbound_session: bool,
        placeholder: Option<CreateUserSessionParams<'_>>,
    ) -> (ConnectionAdmission, PlaybackIdReplacementEffect, Option<(u64, SessionIdentity)>) {
        self.gc();

        let (
            admission,
            removed_streams,
            matched_session_tokens,
            disconnected_addrs,
            connection_changed,
            promotions,
            divergence_snapshot,
            placeholder_info,
        ) = {
            let mut user_connections = self.write_connections().await;
            let mut matched_session_tokens: Vec<String> = Vec::new();
            let mut raw_removed_streams: Vec<StreamInfo> = Vec::new();
            let mut touched_addrs: Vec<SocketAddr> = Vec::new();
            let mut connection_changed = false;
            let mut promotions = Vec::new();
            let mut divergence_snapshot = None;
            let mut admission = ConnectionAdmission::allowed(Some(ConnectionKind::Normal));

            if let Some(connection_data) = user_connections.by_key.get_mut(username) {
                for session in &connection_data.sessions {
                    if session.playback_id.as_deref() == Some(playback_id)
                        && session_token.is_none_or(|tok| session.token != tok)
                        && !matched_session_tokens.contains(&session.token)
                    {
                        matched_session_tokens.push(session.token.clone());
                    }
                }
                for stream in &connection_data.streams {
                    if connection_data.stream_playback_ids.get(&stream.uid).is_some_and(|id| id.as_ref() == playback_id)
                        && session_token.is_none_or(|tok| stream.session_token.as_deref() != Some(tok))
                    {
                        if let Some(tok) = stream.session_token.as_ref() {
                            if !matched_session_tokens.contains(tok) {
                                matched_session_tokens.push(tok.clone());
                            }
                        }
                    }
                }

                for tok in &matched_session_tokens {
                    let session_index = connection_data.sessions.iter().position(|s| s.token == *tok);
                    let counted_kind = session_index.and_then(|idx| {
                        connection_data.sessions[idx]
                            .lifecycle
                            .is_counted()
                            .then(|| connection_data.sessions[idx].connection_kind.unwrap_or(ConnectionKind::Normal))
                    });
                    let (removed, changed) =
                        connection_data.remove_session_stream_infos_and_release_counted(tok, counted_kind);
                    connection_changed |= changed;
                    raw_removed_streams.extend(removed);
                    if let Some(idx) = session_index {
                        let removed_session = connection_data.sessions.swap_remove(idx);
                        touched_addrs.push(removed_session.addr);
                        touched_addrs.extend(removed_session.active_addrs);
                    }
                }

                let mut stream_idx = 0;
                while stream_idx < connection_data.streams.len() {
                    let uid = connection_data.streams[stream_idx].uid;
                    let matches_id =
                        connection_data.stream_playback_ids.get(&uid).is_some_and(|id| id.as_ref() == playback_id);
                    let matches_caller = session_token
                        .is_some_and(|tok| connection_data.streams[stream_idx].session_token.as_deref() == Some(tok));
                    if !matches_id || matches_caller {
                        stream_idx += 1;
                        continue;
                    }
                    let (removed, released_kind) = connection_data.remove_stream_info_at(stream_idx);
                    connection_changed |= released_kind;
                    raw_removed_streams.push(removed);
                }

                for stream in &raw_removed_streams {
                    touched_addrs.push(stream.addr);
                }

                if !raw_removed_streams.is_empty() || !matched_session_tokens.is_empty() {
                    if connection_data.connections == 0 && connection_data.streams.is_empty() {
                        connection_data.granted_grace = false;
                        connection_data.grace_ts = 0;
                    }
                    promotions = Self::collect_promotions_after_capacity_release(connection_data);
                }
            }

            for tok in &matched_session_tokens {
                user_connections.pending_member_bindings.remove(tok.as_str());
            }
            if !matched_session_tokens.is_empty() {
                user_connections.mark_sessions_ended(matched_session_tokens.iter().cloned());
            }

            let placeholder_info = if let Some(ref placeholder_params) = placeholder {
                let (version, identity, placeholder_divergence) = self.ensure_user_session_placeholder_in_data(
                    &mut user_connections,
                    placeholder_params,
                    Some(playback_id),
                    true,
                );
                divergence_snapshot = placeholder_divergence;
                Some((version, identity))
            } else {
                None
            };

            if let Some(connection_data) = user_connections.by_key.get_mut(username) {
                divergence_snapshot = Self::collect_divergence_snapshot(connection_data, username);
                admission = self.evaluate_admission_for_request_in_data(
                    username,
                    connection_data,
                    max_connections,
                    soft_connections,
                    use_session_admission,
                    session_token,
                    activate_unbound_session,
                );
                if activate_unbound_session
                    && use_session_admission
                    && admission.permission() == UserConnectionPermission::Allowed
                {
                    if let Some(tok) = session_token {
                        if let Some(session) = connection_data.sessions.iter_mut().find(|s| s.token == tok) {
                            Self::update_session_admission(session, admission.permission(), admission.kind());
                            if session.playback_id.is_none() {
                                session.playback_id = Some(Arc::from(playback_id));
                            }
                        }
                    }
                }
            }

            let mut unique_addrs = touched_addrs;
            unique_addrs.sort_unstable_by_key(|a| (a.ip(), a.port()));
            unique_addrs.dedup();
            let mut disconnected_addrs = HashSet::new();
            for addr in unique_addrs {
                let still_for_user = user_connections
                    .by_key
                    .get(username)
                    .is_some_and(|d| d.has_session_addr(&addr) || d.streams.iter().any(|s| s.addr == addr));
                if !still_for_user {
                    let remove_entry =
                        user_connections.key_by_addr.get_mut(&addr).is_some_and(|reg| reg.remove_user(username));
                    if remove_entry {
                        user_connections.key_by_addr.remove(&addr);
                    }
                    disconnected_addrs.insert(addr);
                }
            }

            let removed_streams = raw_removed_streams
                .into_iter()
                .map(|stream_info| {
                    let other_active_at_addr = user_connections.by_key.values().any(|d| {
                        d.streams.iter().any(|s| s.addr == stream_info.addr) || d.has_session_addr(&stream_info.addr)
                    });
                    RemovedPlaybackIdStream {
                        close_socket: stream_info.addr != *request_addr && !other_active_at_addr,
                        release_provider: !other_active_at_addr,
                        addr_disconnected: disconnected_addrs.contains(&stream_info.addr),
                        stream_info,
                    }
                })
                .collect::<Vec<_>>();

            (
                admission,
                removed_streams,
                matched_session_tokens,
                disconnected_addrs,
                connection_changed,
                promotions,
                divergence_snapshot,
                placeholder_info,
            )
        };

        let _ = disconnected_addrs;
        self.log_divergence_snapshot(divergence_snapshot).await;
        if connection_changed {
            self.log_active_user().await;
        }
        for action in promotions {
            self.emit_promotion_update(username, action).await;
        }

        (
            admission,
            PlaybackIdReplacementEffect { removed_streams, ended_session_tokens: matched_session_tokens },
            placeholder_info,
        )
    }

    pub async fn release_unbound_session_reservation(
        &self,
        username: &str,
        session_token: &str,
        expected_transition_version: Option<u64>,
        remove_session_if_unbound: bool,
    ) {
        let (connection_changed, user_removed, promotions, divergence_snapshot) = {
            let mut user_connections = self.write_connections().await;
            user_connections.pending_member_bindings.remove(session_token);
            let (connection_changed, user_removed, promotions, divergence_snapshot) = {
                let Some(connection_data) = user_connections.by_key.get_mut(username) else {
                    return;
                };

                if Self::session_has_stream(connection_data, session_token) {
                    return;
                }

                let Some(session_index) =
                    connection_data.sessions.iter().position(|session| session.token == session_token)
                else {
                    return;
                };

                if expected_transition_version
                    .is_some_and(|expected| connection_data.sessions[session_index].transition_version != expected)
                {
                    return;
                }

                let mut connection_changed = false;
                if connection_data.sessions[session_index].lifecycle.is_counted() {
                    let kind =
                        connection_data.sessions[session_index].connection_kind.unwrap_or(ConnectionKind::Normal);
                    connection_data.decrement_kind(kind);
                    connection_data.sessions[session_index].expire();
                    connection_changed = true;
                }
                connection_data.sessions[session_index].transition_version =
                    connection_data.sessions[session_index].transition_version.saturating_add(1);

                if remove_session_if_unbound {
                    connection_data.sessions.swap_remove(session_index);
                }

                if connection_data.connections < connection_data.max_connections {
                    connection_data.granted_grace = false;
                    connection_data.grace_ts = 0;
                }

                let mut promotions = Vec::new();
                while let Some(action) = connection_data.try_promote_soft_stream() {
                    let promoted_stream =
                        connection_data.streams.iter().find(|stream| stream.uid == action.uid).cloned();
                    if let Some(stream) = promoted_stream.as_ref() {
                        Self::promote_session_for_stream(connection_data, stream);
                    }
                    promotions.push(action);
                }
                while connection_data.try_promote_soft_session_reservation() {}

                let user_removed = connection_data.connections == 0
                    && connection_data.streams.is_empty()
                    && connection_data.sessions.is_empty();
                let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, username);

                (connection_changed, user_removed, promotions, divergence_snapshot)
            };

            if user_removed {
                user_connections.by_key.remove(username);
            }

            (connection_changed, user_removed, promotions, divergence_snapshot)
        };

        self.log_divergence_snapshot(divergence_snapshot).await;

        if connection_changed || user_removed {
            self.log_active_user().await;
        }
        for action in promotions {
            self.emit_promotion_update(username, action).await;
        }
    }

    pub async fn release_session_streams_and_counted_reservation(&self, username: &str, session_token: &str) -> bool {
        let (connection_changed, user_removed, promotions, divergence_snapshot, removed_streams) = {
            let mut user_connections = self.write_connections().await;
            let (connection_changed, user_removed, promotions, divergence_snapshot, removed_streams) = {
                let Some(connection_data) = user_connections.by_key.get_mut(username) else {
                    return false;
                };

                let counted_kind = connection_data
                    .sessions
                    .iter()
                    .find(|session| session.token == session_token && session.lifecycle.is_counted())
                    .and_then(|session| session.connection_kind);
                let (removed_streams, mut connection_changed) =
                    connection_data.remove_streams_for_session_and_release_counted(session_token, counted_kind);
                Self::clear_session_counted_without_stream(connection_data, session_token);

                if connection_data.connections < connection_data.max_connections {
                    connection_data.granted_grace = false;
                    connection_data.grace_ts = 0;
                }

                let promotions = Self::collect_promotions_after_capacity_release(connection_data);
                let user_removed = connection_data.connections == 0
                    && connection_data.streams.is_empty()
                    && connection_data.sessions.is_empty();
                let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, username);
                connection_changed |= !promotions.is_empty();

                (connection_changed, user_removed, promotions, divergence_snapshot, removed_streams)
            };

            if user_removed {
                user_connections.by_key.remove(username);
            }

            (connection_changed, user_removed, promotions, divergence_snapshot, removed_streams)
        };

        self.log_divergence_snapshot(divergence_snapshot).await;
        self.emit_removed_streams(&removed_streams);

        if connection_changed || user_removed {
            self.log_active_user().await;
        }
        for action in promotions {
            self.emit_promotion_update(username, action).await;
        }
        connection_changed || user_removed
    }

    pub fn set_shared_stream_manager(&self, manager: &Arc<crate::streams::SharedStreamManager>) {
        let _ = self.shared_stream_manager.set(Arc::downgrade(manager));
    }

    pub async fn is_free_subscriber(&self, uid: u32) -> bool {
        if let Some(shared_manager) = self.shared_stream_manager.get().and_then(std::sync::Weak::upgrade) {
            shared_manager.is_subscriber_free(tuliprox_core::model::SharedSubscriberId::from_stream_uid(uid)).await
        } else {
            false
        }
    }

    pub async fn try_admit_converted_free_join(&self, username: &str, session_token: &str, stream_uid: u32) -> bool {
        let mut user_connections = self.write_connections().await;
        let Some(connection_data) = user_connections.by_key.get_mut(username) else {
            return false;
        };
        let counts = connection_data.effective_counts_for_admission(Some(session_token));
        let selected_kind =
            decide_connection_kind(counts, connection_data.max_connections, connection_data.soft_connections);
        if let Some(kind) = selected_kind {
            connection_data.increment_kind(kind);
            connection_data.stream_kinds.insert(stream_uid, kind);
            if let Some(session) = connection_data.sessions.iter_mut().find(|s| s.token == session_token) {
                session.connection_kind = Some(kind);
                session.lifecycle = PlaybackLifecycle::Active;
                session.permission = UserConnectionPermission::Allowed;
                Self::bump_session_transition_version(session);
            }
            true
        } else {
            if let Some(session) = connection_data.sessions.iter_mut().find(|s| s.token == session_token) {
                session.set_permission(UserConnectionPermission::Exhausted);
            }
            false
        }
    }
}
