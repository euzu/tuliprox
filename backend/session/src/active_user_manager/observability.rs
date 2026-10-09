use super::{ActiveUserManager, PendingProviderReason, PlaybackLifecycle, PromotionAction, UserConnectionData};
use crate::{active_provider_manager::ConnectionKind, EventManager};
use log::{debug, info, log_enabled};
use shared::{
    model::{ActiveUserConnectionChange, EventMessage, StreamInfo, StreamTechnicalInfo},
    utils::sanitize_sensitive_info,
};
use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};
use tuliprox_core::utils::debug_if_enabled;

pub(super) struct DivergenceEntry {
    pub(super) last_logged: Instant,
    pub(super) count_since_last_log: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(super) enum DivergenceKind {
    CountedSessionWithoutStream,
    StreamWithoutCountedSession,
    ConnectionCountMismatch { legacy: u32, counted: u32 },
}

pub(super) fn divergence_key(username: &str, kind: &DivergenceKind) -> String {
    match kind {
        DivergenceKind::CountedSessionWithoutStream => format!("{username}:CountedSessionWithoutStream"),
        DivergenceKind::StreamWithoutCountedSession => format!("{username}:StreamWithoutCountedSession"),
        DivergenceKind::ConnectionCountMismatch { legacy, counted } => {
            format!("{username}:ConnectionCountMismatch:{legacy}+{counted}")
        }
    }
}

pub(super) struct DivergenceSnapshot {
    pub(super) username: String,
    pub(super) connections: u32,
    pub(super) counted_sessions: usize,
    pub(super) streams_count: usize,
    pub(super) kinds: Vec<DivergenceKind>,
}

impl ActiveUserManager {
    pub(super) fn custom_stream_technical_info() -> StreamTechnicalInfo {
        StreamTechnicalInfo {
            container: String::from("mpegts"),
            resolution: String::new(),
            fps: String::from("30"),
            video_codec: String::from("H.264"),
            audio_codec: String::from("AAC"),
            audio_channels: String::from("Stereo"),
        }
    }

    /// The bus this manager publishes on.
    ///
    /// Exposed so the admission path can report a refusal without
    /// `AdmissionCtx` growing a second handle to the same manager.
    #[must_use]
    pub fn events(&self) -> &Arc<EventManager> { &self.event_manager }

    /// Cumulative count of admission requests quietly suppressed by the recent-eviction
    /// reentry guard. A diagnostic counter, not a failure metric.
    pub fn reentry_suppressed_total(&self) -> u64 { self.reentry_suppressed_total.load(Ordering::Relaxed) }

    /// Records one suppressed reentry retry.
    pub fn record_reentry_suppressed(&self) { self.reentry_suppressed_total.fetch_add(1, Ordering::Relaxed); }

    /// Collect a snapshot of all currently active streams for shutdown history recording.
    pub async fn get_all_active_streams(&self) -> Vec<shared::model::StreamInfo> {
        let connections = self.connections.read().await;
        connections
            .by_key
            .values()
            .flat_map(|data| data.streams.iter().filter(|stream| !stream.preserved).cloned())
            .collect()
    }

    pub(super) async fn log_active_user(&self) {
        let is_log_user_enabled = self.is_log_user_enabled();
        let (user_count, user_connection_count) = { self.active_users_and_connections().await };
        self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Connections(
            user_count,
            user_connection_count,
        )));
        if !is_log_user_enabled {
            return;
        }
        let last_user_count = self.last_logged_user_count.load(Ordering::Relaxed);
        let last_connection_count = self.last_logged_user_connection_count.load(Ordering::Relaxed);
        if last_user_count != user_count || last_connection_count != user_connection_count {
            self.last_logged_user_count.store(user_count, Ordering::Relaxed);
            self.last_logged_user_connection_count.store(user_connection_count, Ordering::Relaxed);
            info!("Active Users: {user_count}, Active User Connections: {user_connection_count}");
        }
    }

    pub(super) async fn emit_promotion_update(&self, username: &str, action: PromotionAction) {
        let maybe_stream = {
            let user_connections = self.connections.read().await;
            user_connections.by_key.get(username).and_then(|connection_data| {
                connection_data.streams.iter().find(|stream| stream.uid == action.uid).cloned()
            })
        };
        if let Some(stream_info) = maybe_stream {
            if let Some(provider_manager) = self.provider_manager.get() {
                if stream_info.channel.shared {
                    provider_manager.reclassify_shared_connection(
                        tuliprox_core::model::SharedSubscriberId::from_stream_uid(action.uid),
                        ConnectionKind::Normal,
                        action.new_priority,
                    );
                } else {
                    provider_manager.reclassify_connection_for_owner(
                        &action.addr,
                        stream_info.session_token.as_deref(),
                        ConnectionKind::Normal,
                        action.new_priority,
                    );
                }
            }
            self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(stream_info)));
        }
    }

    pub async fn active_users_and_connections(&self) -> (usize, usize) {
        self.gc();
        let user_connections = self.connections.read().await;
        user_connections
            .by_key
            .values()
            .filter_map(|c| {
                let effective = c.connections as usize;
                if effective > 0 {
                    Some(effective)
                } else {
                    None
                }
            })
            .fold((0usize, 0usize), |(user_count, conn_count), effective| (user_count + 1, conn_count + effective))
    }

    pub async fn playback_resource_counts(&self) -> (usize, usize) {
        self.gc();
        let user_connections = self.connections.read().await;
        user_connections.by_key.values().fold((0, 0), |(requests, sessions), connection| {
            (requests + connection.stream_request_claims.len(), sessions + connection.sessions.len())
        })
    }

    pub(super) fn is_log_user_enabled(&self) -> bool { self.log_active_user.load(Ordering::Relaxed) }

    pub async fn active_streams(&self) -> Vec<StreamInfo> {
        self.gc();
        let user_connections = self.connections.read().await;
        let mut streams = Vec::new();
        for connection_data in user_connections.by_key.values() {
            for stream in &connection_data.streams {
                // Keep active_streams free of preserved rows — shared-HLS join detection and
                // connection accounting must not see stale archive segment leases (v3.3.81).
                if !stream.preserved {
                    streams.push(stream.clone());
                }
            }
        }
        streams
    }

    /// Streams for the `WebUI` / `StatusCheck` snapshot.
    ///
    /// Includes preserved Catchup/HLS/DASH session rows so the panel keeps showing archive
    /// playback between short segment sockets. Do not use this for shared-HLS accounting.
    pub async fn panel_streams(&self) -> Vec<StreamInfo> {
        self.gc();
        let user_connections = self.connections.read().await;
        let mut streams = Vec::new();
        for connection_data in user_connections.by_key.values() {
            for stream in &connection_data.streams {
                if !stream.preserved || Self::should_preserve_session_stream(stream) {
                    streams.push(stream.clone());
                }
            }
        }
        streams
    }

    pub(super) fn log_connection_added(
        username: &str,
        addr: &SocketAddr,
        connection_data: &UserConnectionData,
        tracked_socket_count: usize,
    ) {
        if log::log_enabled!(log::Level::Debug) {
            let active_for_user = connection_data.connections;
            if connection_data.max_connections > 0 && active_for_user > connection_data.max_connections {
                let recent_sockets = connection_data
                    .streams
                    .iter()
                    .rev()
                    .take(3)
                    .map(|stream| stream.addr.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                let recent_sockets = if recent_sockets.is_empty() { String::from("n/a") } else { recent_sockets };
                let unique_clients =
                    connection_data.streams.iter().map(|stream| &stream.client_ip).collect::<HashSet<_>>().len();
                debug!(
                    "User {username} exceeded configured max connections ({}/{}). Unique clients: {}, recent sockets [{}]",
                    active_for_user,
                    connection_data.max_connections,
                    unique_clients,
                    recent_sockets
                );
            } else {
                debug_if_enabled!(
                    "Added new connection for {username} at {} (active user connections={active_for_user}, tracked sockets={tracked_socket_count})",
                    sanitize_sensitive_info(&addr.to_string())
                );
            }
        }
    }

    pub(super) fn collect_divergence_snapshot(
        connection_data: &UserConnectionData,
        username: &str,
    ) -> Option<DivergenceSnapshot> {
        log_enabled!(log::Level::Debug).then(|| Self::build_divergence_snapshot(connection_data, username))
    }

    pub(super) fn build_divergence_snapshot(
        connection_data: &UserConnectionData,
        username: &str,
    ) -> DivergenceSnapshot {
        let connections = connection_data.connections;
        let counted_sessions = connection_data.sessions.iter().filter(|s| s.lifecycle.is_counted()).count();
        let streams_count = connection_data.streams.len();
        let mut kinds = Vec::new();

        for session in &connection_data.sessions {
            if !session.lifecycle.is_counted() {
                continue;
            }
            if matches!(session.lifecycle, PlaybackLifecycle::PendingProvider { ref data } if data.reason_code == PendingProviderReason::GraceHold)
            {
                continue;
            }
            let has_active_stream = connection_data
                .streams
                .iter()
                .any(|s| s.session_token.as_deref() == Some(&session.token) && !s.preserved);
            if !has_active_stream {
                kinds.push(DivergenceKind::CountedSessionWithoutStream);
            }
        }

        for stream in &connection_data.streams {
            if stream.preserved {
                continue;
            }
            let Some(token) = stream.session_token.as_deref() else {
                continue;
            };
            let has_counted_session =
                connection_data.sessions.iter().any(|s| s.token == token && s.lifecycle.is_counted());
            if !has_counted_session {
                kinds.push(DivergenceKind::StreamWithoutCountedSession);
            }
        }

        #[allow(clippy::cast_possible_truncation)]
        let counted_sessions_u32 = counted_sessions as u32;
        if connections != counted_sessions_u32 {
            kinds.push(DivergenceKind::ConnectionCountMismatch { legacy: connections, counted: counted_sessions_u32 });
        }

        DivergenceSnapshot { username: username.to_string(), connections, counted_sessions, streams_count, kinds }
    }

    pub(super) async fn log_divergence_snapshot(&self, snapshot: Option<DivergenceSnapshot>) {
        let Some(snapshot) = snapshot else {
            return;
        };
        let cooldown = Duration::from_secs(self.divergence_cooldown_secs);
        for kind in &snapshot.kinds {
            let key = divergence_key(&snapshot.username, kind);
            let should_log = {
                let mut cache = self.divergence_cache.lock().await;
                if let Some(entry) = cache.get_mut(&key) {
                    if entry.last_logged.elapsed() >= cooldown {
                        entry.last_logged = Instant::now();
                        entry.count_since_last_log = 0;
                        true
                    } else {
                        entry.count_since_last_log = entry.count_since_last_log.saturating_add(1);
                        false
                    }
                } else {
                    cache.push(key, DivergenceEntry { last_logged: Instant::now(), count_since_last_log: 0 });
                    true
                }
            };

            if should_log {
                debug!(
                    "ADMISSION DIVERGENCE user={} kind={kind:?} connections={} counted_sessions={} streams={}",
                    snapshot.username, snapshot.connections, snapshot.counted_sessions, snapshot.streams_count,
                );
            }
        }
    }

    pub(super) async fn check_and_log_divergence_for_user(&self, username: &str) {
        let snapshot = {
            let connections = self.connections.read().await;
            let Some(data) = connections.by_key.get(username) else {
                return;
            };
            Self::collect_divergence_snapshot(data, username)
        };
        self.log_divergence_snapshot(snapshot).await;
    }
}
