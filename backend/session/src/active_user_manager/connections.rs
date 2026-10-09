use super::{
    get_adaptive_session_ttl_secs, release_session_addr, ActiveUserConnectionParams, ActiveUserManager,
    AdmissionRejectionReason, ConnectionAdmission, DeferredWakes, PlaybackLifecycle, ReleasedConnection,
    SessionChangeSignal, SessionIdentity, SessionProviderHeaders, StreamRequestClaim, StreamRequestDetach, UserSession,
    DIVERGENCE_CACHE_CAPACITY, ENDED_SESSION_RECREATE_BLOCK,
};
use crate::{
    active_provider_manager::ConnectionKind, connection_manager::CleanupEvent, stream::uses_direct_body_idle_timeout,
    ActiveProviderManager, EventManager,
};
use arc_swap::ArcSwapOption;
use lru::LruCache;
use shared::{
    defaults::{default_grace_period_millis, default_grace_period_timeout_secs},
    model::{
        ActiveUserConnectionChange, CustomVideoStreamType, EventMessage, StreamChannel, StreamInfo,
        UserConnectionPermission, VirtualId,
    },
    utils::{current_time_secs, sanitize_sensitive_info, strip_port, Internable},
};
use std::{
    collections::{BinaryHeap, HashMap, HashSet},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Instant,
};
use tokio::sync::{mpsc, Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;
use tuliprox_core::{model::Config, utils::debug_if_enabled};
use tuliprox_repository::GeoIp;

pub(super) fn get_grace_options(config: &Config) -> (u64, u64) {
    let (grace_period_millis, grace_period_timeout_secs) =
        config.reverse_proxy.as_ref().and_then(|r| r.stream.as_ref()).map_or_else(
            || (default_grace_period_millis(), default_grace_period_timeout_secs()),
            |s| (s.grace_period_millis, s.grace_period_timeout_secs),
        );
    (grace_period_millis, grace_period_timeout_secs)
}

pub(super) fn decide_connection_kind(
    counts: UserConnectionCounts,
    max_connections: u32,
    soft_connections: u16,
) -> Option<ConnectionKind> {
    if max_connections == 0 || counts.normal < max_connections {
        return Some(ConnectionKind::Normal);
    }
    if soft_connections > 0 && counts.soft < soft_connections {
        return Some(ConnectionKind::Soft);
    }
    None
}

impl DeferredWakes {
    pub(super) fn push(&self, notify: Arc<Notify>) {
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(notify);
        self.has_pending.store(true, Ordering::Release);
    }

    pub(super) fn wake(&self) {
        if !self.has_pending.swap(false, Ordering::AcqRel) {
            return;
        }
        let pending = std::mem::take(&mut *self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        for notify in pending {
            notify.notify_waiters();
        }
    }
}

impl SessionChangeSignal {
    pub(super) fn new(deferred: &Arc<DeferredWakes>) -> Self {
        Self { notify: Arc::new(Notify::new()), deferred: Some(Arc::clone(deferred)), owner: true }
    }

    pub(super) fn signal(&self) {
        match &self.deferred {
            Some(deferred) => deferred.push(Arc::clone(&self.notify)),
            None => self.notify.notify_waiters(),
        }
    }
}

impl SessionProviderHeaders {
    pub(super) fn for_target(session: &UserSession, target_url: &str) -> Self {
        session
            .provider_session_headers_for(target_url)
            .map_or(Self::NoHeaders, |headers| Self::Headers(headers.into_owned()))
    }
}

/// `host:port` of a URL, used to scope provider session cookies.
pub(super) fn url_host_key(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    Some(format!("{}://{}:{}", url.scheme(), url.host_str()?, url.port_or_known_default()?))
}

impl UserSession {
    pub const fn identity(&self) -> SessionIdentity {
        SessionIdentity { incarnation: self.incarnation, binding_generation: self.binding_generation }
    }

    pub(super) fn switch_provider(&mut self, provider: Arc<str>) {
        self.provider = provider;
        self.binding_generation = self.binding_generation.wrapping_add(1);
        self.change_signal.signal();
        self.provider_session_headers.clear();
        self.provider_session_headers_host = None;
        self.clear_provider_session_cookies();
    }

    pub(super) fn expire(&mut self) {
        self.lifecycle = PlaybackLifecycle::Expired;
        self.change_signal.signal();
    }

    /// Whether the session may still serve requests: neither expired nor exhausted.
    pub(super) fn is_live(&self) -> bool {
        !matches!(self.lifecycle, PlaybackLifecycle::Expired) && self.permission != UserConnectionPermission::Exhausted
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct UserConnectionCounts {
    pub(super) normal: u32,
    pub(super) soft: u16,
}

impl ConnectionAdmission {
    pub fn allowed(kind: Option<ConnectionKind>) -> Self {
        Self { permission: UserConnectionPermission::Allowed, kind, rejection_reason: None }
    }

    pub fn grace_period(kind: Option<ConnectionKind>) -> Self {
        Self { permission: UserConnectionPermission::GracePeriod, kind, rejection_reason: None }
    }

    pub fn exhausted(reason: AdmissionRejectionReason, kind: Option<ConnectionKind>) -> Self {
        Self { permission: UserConnectionPermission::Exhausted, kind, rejection_reason: Some(reason) }
    }

    pub fn kind(&self) -> Option<ConnectionKind> { self.kind }

    pub fn rejection_reason(&self) -> Option<AdmissionRejectionReason> { self.rejection_reason }

    pub fn is_reentry_suppressed(&self) -> bool {
        self.rejection_reason == Some(AdmissionRejectionReason::RecentEvictionReentry)
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PromotionAction {
    pub(super) addr: SocketAddr,
    pub(super) uid: u32,
    pub(super) new_priority: i8,
}

impl StreamRequestDetach {
    pub fn request_was_detached(&self) -> bool { !matches!(self, Self::NotFound) }

    pub fn stream_info(&self) -> Option<&StreamInfo> {
        match self {
            Self::NotFound => None,
            Self::Retained(stream) | Self::Preserved(stream) | Self::Removed(stream) => Some(stream),
        }
    }

    pub(super) fn into_removed(self) -> Option<StreamInfo> {
        match self {
            Self::Removed(stream) => Some(stream),
            Self::NotFound | Self::Retained(_) | Self::Preserved(_) => None,
        }
    }
}

#[derive(Debug)]
pub(super) struct UserConnectionData {
    pub(super) max_connections: u32,
    pub(super) soft_connections: u16,
    pub(super) counts: UserConnectionCounts,
    pub(super) connections: u32,
    pub(super) granted_grace: bool,
    pub(super) grace_ts: u64,
    pub(super) sessions: Vec<UserSession>,
    pub(super) streams: Vec<StreamInfo>,
    pub(super) stream_request_claims: HashMap<u32, StreamRequestClaim>,
    pub(super) stream_kinds: HashMap<u32, ConnectionKind>,
    pub(super) stream_normal_priorities: HashMap<u32, i8>,
    pub(super) ts: u64,
}

impl UserConnectionData {
    pub(super) fn new(connections: u32, max_connections: u32, soft_connections: u16) -> Self {
        Self {
            max_connections,
            soft_connections,
            counts: UserConnectionCounts::default(),
            connections,
            granted_grace: false,
            grace_ts: 0,
            sessions: Vec::new(),
            streams: Vec::new(),
            stream_request_claims: HashMap::new(),
            stream_kinds: HashMap::new(),
            stream_normal_priorities: HashMap::new(),
            ts: current_time_secs(),
        }
    }

    pub(super) fn add_session(&mut self, session: UserSession) {
        self.gc();
        self.sessions.push(session);
    }

    pub(super) fn release_addr_from_sessions(&mut self, addr: &SocketAddr) -> HashMap<String, Option<SocketAddr>> {
        let mut migrated_addrs = HashMap::new();
        for session in &mut self.sessions {
            if session.addr == *addr || session.active_addrs.contains(addr) {
                migrated_addrs.insert(session.token.clone(), release_session_addr(session, addr));
            }
        }
        migrated_addrs
    }

    pub(super) fn release_addr_from_stream_session(
        &mut self,
        addr: &SocketAddr,
        uid: u32,
    ) -> HashMap<String, Option<SocketAddr>> {
        let Some(token) =
            self.streams.iter().find(|stream| stream.uid == uid).and_then(|stream| stream.session_token.as_deref())
        else {
            return HashMap::new();
        };
        if self.streams.iter().any(|stream| {
            stream.uid != uid
                && !stream.preserved
                && stream.addr == *addr
                && stream.session_token.as_deref() == Some(token)
        }) {
            return HashMap::new();
        }
        let Some(session) = self.sessions.iter_mut().find(|session| session.token == token) else {
            return HashMap::new();
        };
        HashMap::from([(session.token.clone(), release_session_addr(session, addr))])
    }

    pub(super) fn attach_stream_request(&mut self, request_uid: u32, stream_uid: u32, addr: SocketAddr) {
        self.stream_request_claims.insert(request_uid, StreamRequestClaim { stream_uid, addr });
    }

    pub(super) fn detach_stream_request(&mut self, request_uid: u32) -> Option<(u32, bool)> {
        let claim = self.stream_request_claims.remove(&request_uid)?;
        let is_last = !self.stream_request_claims.values().any(|other| other.stream_uid == claim.stream_uid);
        Some((claim.stream_uid, is_last))
    }

    pub(super) fn increment_kind(&mut self, kind: ConnectionKind) {
        self.connections = self.connections.saturating_add(1);
        match kind {
            ConnectionKind::Normal => {
                self.counts.normal = self.counts.normal.saturating_add(1);
            }
            ConnectionKind::Soft => {
                self.counts.soft = self.counts.soft.saturating_add(1);
            }
        }
    }

    pub(super) fn decrement_kind(&mut self, kind: ConnectionKind) {
        self.connections = self.connections.saturating_sub(1);
        match kind {
            ConnectionKind::Normal => {
                self.counts.normal = self.counts.normal.saturating_sub(1);
            }
            ConnectionKind::Soft => {
                self.counts.soft = self.counts.soft.saturating_sub(1);
            }
        }
    }
}

pub(super) fn create_socket_reentry_guard_key(username: &str, client_ip: &str, virtual_id: VirtualId) -> String {
    shared::concat_string!(username, "|", client_ip, "|", &virtual_id.to_string())
}

impl UserConnections {
    pub(super) fn mark_sessions_ended<I: IntoIterator<Item = String>>(&mut self, tokens: I) {
        let now = Instant::now();
        self.ended_sessions.retain(|_, expires_at| *expires_at > now);
        let expires_at = now + ENDED_SESSION_RECREATE_BLOCK;
        let tokens = tokens.into_iter().collect::<Vec<_>>();
        for session in self.by_key.values().flat_map(|data| data.sessions.iter()) {
            if tokens.contains(&session.token) {
                session.change_signal.signal();
            }
        }
        self.ended_sessions.extend(tokens.into_iter().map(|token| (token, expires_at)));
    }

    pub(super) fn is_token_ended(&self, token: &str) -> bool {
        self.ended_sessions.get(token).is_some_and(|expiry| *expiry > Instant::now())
    }

    /// Returns the session while it may still serve requests: not ended, expired or exhausted.
    pub(super) fn live_session(&self, username: &str, token: &str) -> Option<(&UserConnectionData, &UserSession)> {
        if self.is_token_ended(token) {
            return None;
        }
        let data = self.by_key.get(username)?;
        let session = data.sessions.iter().find(|session| session.token == token && session.is_live())?;
        Some((data, session))
    }

    /// Returns the live session only while this exact incarnation and binding may still serve requests.
    pub(super) fn current_session(
        &self,
        username: &str,
        token: &str,
        identity: SessionIdentity,
    ) -> Option<(&UserConnectionData, &UserSession)> {
        self.live_session(username, token).filter(|(_, session)| session.identity() == identity)
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RecentWinnerProtection {
    pub(super) protected_addr: SocketAddr,
    pub(super) expires_at: Instant,
}

#[derive(Debug, Default)]
pub(super) struct UserConnections {
    pub(super) kicked: HashMap<String, (u64, VirtualId)>,
    pub(super) recently_evicted_sessions: HashMap<String, RecentWinnerProtection>,
    /// Session tokens ended by eviction, kick or explicit terminate, with their expiry. A sealed
    /// playlist token of such a session must not recreate it; an expired session may.
    pub(super) ended_sessions: HashMap<String, Instant>,
    pub(super) recent_socket_reentry_guards: HashMap<String, RecentWinnerProtection>,
    pub(super) by_key: HashMap<String, UserConnectionData>,
    pub(super) key_by_addr: HashMap<SocketAddr, SocketRegistration>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct SocketRegistration {
    pub(super) usernames: HashSet<String>,
    pub(super) ts: u64,
}

impl SocketRegistration {
    pub(super) fn anonymous() -> Self { Self { usernames: HashSet::new(), ts: current_time_secs() } }

    pub(super) fn add_user(&mut self, username: &str, ts: u64) {
        if !username.is_empty() && !self.usernames.contains(username) {
            self.usernames.insert(username.to_string());
        }
        self.ts = ts;
    }

    pub(super) fn remove_user(&mut self, username: &str) -> bool {
        self.usernames.remove(username);
        self.usernames.is_empty()
    }

    pub(super) fn is_empty(&self) -> bool { self.usernames.is_empty() }

    pub(super) fn primary_username(&self) -> Option<&str> { self.usernames.iter().map(String::as_str).min() }

    pub(super) fn all_usernames(&self) -> Vec<String> { self.usernames.iter().cloned().collect() }

    pub(super) fn into_usernames(self) -> HashSet<String> { self.usernames }
}

/// Write access to `UserConnections` that delivers session change signals after release.
///
/// Fields drop in declaration order: `guard` releases the write lock before `_wake` notifies
/// the waiters signalled during this write.
pub(super) struct UserConnectionsWriteGuard<'a> {
    pub(super) guard: tokio::sync::RwLockWriteGuard<'a, UserConnections>,
    pub(super) _wake: WakeDeferred<'a>,
}

pub(super) struct WakeDeferred<'a>(pub(super) &'a DeferredWakes);

impl Drop for WakeDeferred<'_> {
    fn drop(&mut self) { self.0.wake(); }
}

impl<'a> UserConnectionsWriteGuard<'a> {
    pub(super) fn new(guard: tokio::sync::RwLockWriteGuard<'a, UserConnections>, wakes: &'a DeferredWakes) -> Self {
        Self { guard, _wake: WakeDeferred(wakes) }
    }
}

impl std::ops::Deref for UserConnectionsWriteGuard<'_> {
    type Target = UserConnections;
    fn deref(&self) -> &Self::Target { &self.guard }
}

impl std::ops::DerefMut for UserConnectionsWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.guard }
}

impl ActiveUserManager {
    pub(super) async fn write_connections(&self) -> UserConnectionsWriteGuard<'_> {
        UserConnectionsWriteGuard::new(self.connections.write().await, &self.deferred_wakes)
    }

    pub fn shutdown(&self) { self.adaptive_expiry_cancel.cancel(); }

    pub(super) fn lookup_country(&self, client_ip: &str) -> Option<String> {
        let geoip = self.geo_ip.load();
        (*geoip).as_ref().and_then(|geoip_db| geoip_db.lookup(&strip_port(client_ip)))
    }

    pub fn new(config: &Config, geoip: &Arc<ArcSwapOption<GeoIp>>, event_manager: &Arc<EventManager>) -> Self {
        let log_active_user: bool = config.log.as_ref().is_some_and(|l| l.log_active_user);
        let (grace_period_millis, grace_period_timeout_secs) = get_grace_options(config);

        Self {
            grace_period_millis: AtomicU64::new(grace_period_millis),
            grace_period_timeout_secs: AtomicU64::new(grace_period_timeout_secs),
            adaptive_session_ttl_secs: AtomicU64::new(get_adaptive_session_ttl_secs(config)),
            log_active_user: AtomicBool::new(log_active_user),
            next_user_agent_stream_index: AtomicU64::new(1),
            connections: RwLock::new(UserConnections::default()),
            adaptive_expiry_queue: Arc::new(Mutex::new(BinaryHeap::new())),
            adaptive_expiry_index: Arc::new(Mutex::new(HashMap::new())),
            adaptive_expiry_notify: Arc::new(Notify::new()),
            adaptive_expiry_cancel: CancellationToken::new(),
            adaptive_expiry_worker_started: AtomicBool::new(false),
            gc_ts: Some(AtomicU64::new(current_time_secs())),
            geo_ip: Arc::clone(geoip),
            event_manager: Arc::clone(event_manager),
            last_logged_user_count: AtomicUsize::new(0),
            last_logged_user_connection_count: AtomicUsize::new(0),
            cleanup_tx: tokio::sync::OnceCell::new(),
            provider_manager: tokio::sync::OnceCell::new(),
            transition_gates: Mutex::new(HashMap::new()),
            pending_provider_releases: Mutex::new(HashMap::new()),
            dropped_cleanup_events: AtomicU64::new(0),
            reentry_suppressed_total: AtomicU64::new(0),
            deferred_wakes: Arc::default(),
            divergence_cache: Mutex::new(LruCache::new(DIVERGENCE_CACHE_CAPACITY)),
            divergence_cooldown_secs: 300,
        }
    }

    pub(super) fn should_reuse_stream_for_session(
        existing_stream: &StreamInfo,
        incoming_channel: &StreamChannel,
    ) -> bool {
        existing_stream.channel.item_type.requires_provider_affinity()
            || incoming_channel.item_type.requires_provider_affinity()
    }

    pub fn set_cleanup_sender(&self, tx: mpsc::Sender<CleanupEvent>) { let _ = self.cleanup_tx.set(tx); }

    pub fn set_provider_manager(&self, provider_manager: Arc<ActiveProviderManager>) {
        let _ = self.provider_manager.set(provider_manager);
    }

    /// Atomically removes all user requests and sessions during terminal shutdown.
    pub async fn drain_for_shutdown(&self) -> Vec<shared::model::StreamInfo> {
        let connections = std::mem::take(&mut *self.write_connections().await);
        self.adaptive_expiry_queue.lock().await.clear();
        self.adaptive_expiry_index.lock().await.clear();
        self.transition_gates.lock().await.clear();
        connections
            .by_key
            .into_values()
            .flat_map(|data| data.streams.into_iter().filter(|stream| !stream.preserved))
            .collect()
    }

    /// Releases an active stream for the given socket address without removing the
    /// socket registration (`key_by_addr`). This is used when a stream ends while
    /// the underlying HTTP connection may still remain open.
    pub async fn release_stream(&self, addr: &SocketAddr) -> Option<StreamInfo> {
        self.release_stream_inner(addr, None).await.into_removed()
    }

    pub async fn release_stream_by_uid(&self, addr: &SocketAddr, stream_uid: u32) -> Option<StreamInfo> {
        self.release_stream_request_by_uid(addr, stream_uid).await.into_removed()
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn release_stream_inner(&self, addr: &SocketAddr, stream_uid: Option<u32>) -> StreamRequestDetach {
        let (
            removed_stream,
            username,
            expiry_entry,
            preserved_update,
            connection_changed,
            promotion,
            divergence_snapshot,
        ) = {
            let mut user_connections = self.write_connections().await;

            let username = if let Some(uid) = stream_uid {
                user_connections.by_key.iter().find_map(|(username, connection_data)| {
                    connection_data
                        .stream_request_claims
                        .get(&uid)
                        .is_some_and(|claim| {
                            claim.addr == *addr
                                && connection_data
                                    .streams
                                    .iter()
                                    .any(|stream| !stream.preserved && stream.uid == claim.stream_uid)
                        })
                        .then(|| username.clone())
                })
            } else {
                let mut matching_users = user_connections
                    .by_key
                    .iter()
                    .filter(|(_, connection_data)| {
                        connection_data.streams.iter().any(|stream| !stream.preserved && stream.addr == *addr)
                    })
                    .map(|(username, _)| username.clone());
                let first = matching_users.next();
                let second = matching_users.next();
                if second.is_some() {
                    None
                } else {
                    first
                }
            };
            let Some(username) = username else {
                return StreamRequestDetach::NotFound;
            };

            let mut removed_stream = None;
            let mut expiry_entry = None;
            let mut preserved_update = None;
            let mut connection_changed = false;
            let mut promotion = None;
            if let Some(connection_data) = user_connections.by_key.get_mut(&username) {
                let stream_uid = if let Some(request_uid) = stream_uid {
                    let Some((display_uid, is_last)) = connection_data.detach_stream_request(request_uid) else {
                        return StreamRequestDetach::NotFound;
                    };
                    if !is_last {
                        let has_claim_on_addr = connection_data
                            .stream_request_claims
                            .values()
                            .any(|claim| claim.stream_uid == display_uid && claim.addr == *addr);
                        let migrated_session_addrs = if has_claim_on_addr {
                            HashMap::new()
                        } else {
                            connection_data.release_addr_from_stream_session(addr, display_uid)
                        };
                        let next_addr = connection_data
                            .stream_request_claims
                            .values()
                            .find(|claim| claim.stream_uid == display_uid)
                            .map(|claim| claim.addr);
                        if let Some(stream) =
                            connection_data.streams.iter_mut().find(|stream| stream.uid == display_uid)
                        {
                            stream.addr = next_addr
                                .or_else(|| {
                                    stream
                                        .session_token
                                        .as_deref()
                                        .and_then(|token| migrated_session_addrs.get(token))
                                        .copied()
                                        .flatten()
                                })
                                .unwrap_or(stream.addr);
                            stream.ts = current_time_secs();
                        }
                        let retained = connection_data.streams.iter().find(|stream| stream.uid == display_uid).cloned();
                        return retained.map_or(StreamRequestDetach::NotFound, StreamRequestDetach::Retained);
                    }
                    Some(display_uid)
                } else {
                    None
                };
                let migrated_session_addrs = if let Some(uid) = stream_uid {
                    connection_data.release_addr_from_stream_session(addr, uid)
                } else {
                    connection_data.release_addr_from_sessions(addr)
                };
                if let Some(stream_idx) = connection_data.streams.iter().position(|stream| {
                    !stream.preserved
                        && stream_uid.map_or(stream.addr == *addr, |uid| stream.uid == uid && stream.addr == *addr)
                }) {
                    let migrated_addr = connection_data.streams[stream_idx]
                        .session_token
                        .as_deref()
                        .and_then(|token| migrated_session_addrs.get(token))
                        .copied()
                        .flatten();
                    if let Some(next_addr) = migrated_addr {
                        connection_data.streams[stream_idx].addr = next_addr;
                        connection_data.streams[stream_idx].ts = current_time_secs();
                    } else if Self::should_preserve_session_stream(&connection_data.streams[stream_idx]) {
                        let preserved_session_token = connection_data.streams[stream_idx].session_token.clone();
                        if let Some(entry) = self.build_preserved_stream_expiry(
                            &username,
                            &connection_data.streams[stream_idx],
                            &connection_data.sessions,
                        ) {
                            if let Some(kind) =
                                connection_data.stream_kinds.remove(&connection_data.streams[stream_idx].uid)
                            {
                                connection_data.decrement_kind(kind);
                                connection_changed = true;
                            }
                            connection_data.stream_normal_priorities.remove(&connection_data.streams[stream_idx].uid);
                            if let Some(session_token) = preserved_session_token.as_deref() {
                                Self::clear_session_counted(connection_data, session_token);
                            }
                            connection_data.streams[stream_idx].preserved = true;
                            preserved_update = Some(connection_data.streams[stream_idx].clone());
                            expiry_entry = Some(entry);
                        } else {
                            removed_stream = Some(connection_data.streams.swap_remove(stream_idx));
                        }
                    } else {
                        removed_stream = Some(connection_data.streams.swap_remove(stream_idx));
                    }
                    if let Some(removed_stream) = removed_stream.as_ref() {
                        connection_data.remove_stream_request_claims(removed_stream.uid);
                        if let Some(kind) = connection_data.stream_kinds.remove(&removed_stream.uid) {
                            connection_data.decrement_kind(kind);
                        }
                        connection_data.stream_normal_priorities.remove(&removed_stream.uid);
                        connection_changed = true;
                    }
                    if connection_data.connections < connection_data.max_connections {
                        connection_data.granted_grace = false;
                        connection_data.grace_ts = 0;
                    }
                    if removed_stream.is_some() {
                        if let Some(action) = connection_data.try_promote_soft_stream() {
                            let promoted_stream =
                                connection_data.streams.iter().find(|stream| stream.uid == action.uid).cloned();
                            if let Some(stream) = promoted_stream.as_ref() {
                                Self::promote_session_for_stream(connection_data, stream);
                            }
                            promotion = Some(action);
                        }
                        if let Some(session_token) =
                            removed_stream.as_ref().and_then(|stream| stream.session_token.as_deref())
                        {
                            Self::clear_session_counted_without_stream(connection_data, session_token);
                        }
                        while connection_data.try_promote_soft_session_reservation() {}
                    }
                }
                let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, &username);
                (
                    removed_stream,
                    username,
                    expiry_entry,
                    preserved_update,
                    connection_changed,
                    promotion,
                    divergence_snapshot,
                )
            } else {
                (None, username, None, None, false, None, None)
            }
        };

        self.log_divergence_snapshot(divergence_snapshot).await;

        if let Some(entry) = expiry_entry {
            self.enqueue_adaptive_expiry(entry).await;
        }

        if let Some(stream_info) = preserved_update.as_ref() {
            self.event_manager
                .send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(stream_info.clone())));
        }

        if connection_changed {
            if !username.is_empty() {
                debug_if_enabled!(
                    "Released stream for user {username} at {}",
                    sanitize_sensitive_info(&addr.to_string())
                );
            }
            self.log_active_user().await;
        }

        if let Some(action) = promotion {
            self.emit_promotion_update(&username, action).await;
        }

        if let Some(stream_info) = removed_stream {
            StreamRequestDetach::Removed(stream_info)
        } else if let Some(stream_info) = preserved_update {
            StreamRequestDetach::Preserved(stream_info)
        } else {
            StreamRequestDetach::NotFound
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn release_connection_inner(
        &self,
        addr: &SocketAddr,
        preserve_session_streams: bool,
    ) -> ReleasedConnection {
        let (
            addr_removed,
            connection_count_changed,
            disconnected_users,
            removed_streams,
            expiry_entries,
            preserved_updates,
            promotions,
        ) = {
            let mut user_connections = self.write_connections().await;

            let registration = user_connections.key_by_addr.remove(addr);
            let had_registration = registration.is_some();
            let mut disconnected_users: HashSet<String> =
                registration.map(SocketRegistration::into_usernames).unwrap_or_default();
            disconnected_users.extend(
                user_connections
                    .by_key
                    .iter()
                    .filter(|(_, connection_data)| {
                        connection_data.has_session_addr(addr)
                            || connection_data.streams.iter().any(|stream| stream.addr == *addr)
                    })
                    .map(|(username, _)| username.clone()),
            );

            let mut removed_streams = Vec::new();
            let mut expiry_entries = Vec::new();
            let mut preserved_updates = Vec::new();
            let mut promotions = Vec::new();
            let mut connection_count_changed = false;
            for username in &disconnected_users {
                if let Some(connection_data) = user_connections.by_key.get_mut(username) {
                    let previous_connection_count = connection_data.connections;
                    let migrated_session_addrs = connection_data.release_addr_from_sessions(addr);
                    connection_data.stream_request_claims.retain(|_, claim| claim.addr != *addr);
                    let mut remaining_streams = Vec::with_capacity(connection_data.streams.len());
                    let mut released_kinds = Vec::new();
                    let mut removed_session_tokens = HashSet::new();
                    let mut preserved_session_tokens = Vec::new();
                    let now = current_time_secs();
                    for mut stream_info in connection_data.streams.drain(..) {
                        if stream_info.addr == *addr {
                            let migrated_addr = stream_info
                                .session_token
                                .as_deref()
                                .and_then(|token| migrated_session_addrs.get(token))
                                .copied()
                                .flatten();
                            if let Some(next_addr) = migrated_addr {
                                stream_info.addr = next_addr;
                                stream_info.ts = now;
                                remaining_streams.push(stream_info);
                            } else if preserve_session_streams && Self::should_preserve_session_stream(&stream_info) {
                                if let Some(entry) = self.build_preserved_stream_expiry(
                                    username,
                                    &stream_info,
                                    &connection_data.sessions,
                                ) {
                                    if let Some(kind) = connection_data.stream_kinds.remove(&stream_info.uid) {
                                        released_kinds.push(kind);
                                    }
                                    connection_data.stream_normal_priorities.remove(&stream_info.uid);
                                    if let Some(token) = stream_info.session_token.as_ref() {
                                        preserved_session_tokens.push(token.clone());
                                    }
                                    if !stream_info.preserved {
                                        stream_info.preserved = true;
                                        preserved_updates.push(stream_info.clone());
                                    }
                                    expiry_entries.push(entry);
                                    remaining_streams.push(stream_info);
                                } else {
                                    if let Some(kind) = connection_data.stream_kinds.remove(&stream_info.uid) {
                                        released_kinds.push(kind);
                                    }
                                    connection_data.stream_normal_priorities.remove(&stream_info.uid);
                                    if let Some(token) = stream_info.session_token.as_ref() {
                                        removed_session_tokens.insert(token.clone());
                                    }
                                    removed_streams.push(stream_info);
                                }
                            } else {
                                if let Some(kind) = connection_data.stream_kinds.remove(&stream_info.uid) {
                                    released_kinds.push(kind);
                                }
                                connection_data.stream_normal_priorities.remove(&stream_info.uid);
                                if let Some(token) = stream_info.session_token.as_ref() {
                                    removed_session_tokens.insert(token.clone());
                                }
                                removed_streams.push(stream_info);
                            }
                        } else {
                            remaining_streams.push(stream_info);
                        }
                    }
                    connection_data.streams = remaining_streams;
                    let streams = &connection_data.streams;
                    connection_data
                        .stream_request_claims
                        .retain(|_, claim| streams.iter().any(|stream| stream.uid == claim.stream_uid));
                    if !preserve_session_streams && !removed_session_tokens.is_empty() {
                        connection_data.sessions.retain(|session| !removed_session_tokens.contains(&session.token));
                    }
                    for kind in released_kinds {
                        connection_data.decrement_kind(kind);
                    }
                    while let Some(action) = connection_data.try_promote_soft_stream() {
                        let promoted_stream =
                            connection_data.streams.iter().find(|stream| stream.uid == action.uid).cloned();
                        if let Some(stream) = promoted_stream.as_ref() {
                            Self::promote_session_for_stream(connection_data, stream);
                        }
                        promotions.push((username.clone(), action));
                    }
                    for session_token in &removed_session_tokens {
                        Self::clear_session_counted_without_stream(connection_data, session_token);
                    }
                    for session_token in &preserved_session_tokens {
                        Self::clear_session_counted(connection_data, session_token);
                    }
                    while connection_data.try_promote_soft_session_reservation() {}

                    if connection_data.connections < connection_data.max_connections {
                        connection_data.granted_grace = false;
                        connection_data.grace_ts = 0;
                    }
                    connection_count_changed |= connection_data.connections != previous_connection_count;
                }
            }
            let state_changed = had_registration || !disconnected_users.is_empty();
            (
                state_changed,
                connection_count_changed,
                disconnected_users,
                removed_streams,
                expiry_entries,
                preserved_updates,
                promotions,
            )
        };

        for entry in expiry_entries {
            self.enqueue_adaptive_expiry(entry).await;
        }

        for stream_info in preserved_updates {
            self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(stream_info)));
        }

        for username in &disconnected_users {
            if !username.is_empty() {
                debug_if_enabled!(
                    "Released connection for user {username} at {}",
                    sanitize_sensitive_info(&addr.to_string())
                );
            }
        }
        if connection_count_changed {
            self.log_active_user().await;
        }
        if addr_removed {
            for (username, action) in promotions {
                self.emit_promotion_update(&username, action).await;
            }
        }

        ReleasedConnection {
            addr_removed,
            removed_streams,
            disconnected_users: disconnected_users.into_iter().collect(),
        }
    }

    pub async fn release_connection(&self, addr: &SocketAddr) -> ReleasedConnection {
        let released = self.release_connection_inner(addr, true).await;
        // divergence check after connection release
        if released.addr_removed {
            for username in &released.disconnected_users {
                self.check_and_log_divergence_for_user(username).await;
            }
        }
        released
    }

    pub async fn release_connection_as_kicked(&self, addr: &SocketAddr) -> ReleasedConnection {
        let released = self.release_connection_inner(addr, false).await;
        // divergence check after connection release
        if released.addr_removed {
            for username in &released.disconnected_users {
                self.check_and_log_divergence_for_user(username).await;
            }
        }
        released
    }

    pub fn update_config(&self, config: &Config) {
        let log_active_user = config.log.as_ref().is_some_and(|l| l.log_active_user);
        let (grace_period_millis, grace_period_timeout_secs) = get_grace_options(config);
        self.grace_period_millis.store(grace_period_millis, Ordering::Relaxed);
        self.grace_period_timeout_secs.store(grace_period_timeout_secs, Ordering::Relaxed);
        self.adaptive_session_ttl_secs.store(get_adaptive_session_ttl_secs(config), Ordering::Relaxed);
        self.log_active_user.store(log_active_user, Ordering::Relaxed);
    }

    pub async fn user_connections(&self, username: &str) -> u32 {
        if let Some(connection_data) = self.connections.read().await.by_key.get(username) {
            return connection_data.connections;
        }
        0
    }

    pub async fn update_stream_detail(
        &self,
        addr: &SocketAddr,
        video_type: CustomVideoStreamType,
    ) -> Option<StreamInfo> {
        let uid = {
            let connections = self.connections.read().await;
            let mut streams =
                connections.by_key.values().flat_map(|data| &data.streams).filter(|stream| &stream.addr == addr);
            let uid = streams.next()?.uid;
            if streams.next().is_some() {
                return None;
            }
            uid
        };
        self.update_stream_detail_by_uid(uid, video_type).await
    }

    pub async fn update_stream_detail_by_uid(&self, uid: u32, video_type: CustomVideoStreamType) -> Option<StreamInfo> {
        let mut user_connections = self.write_connections().await;
        for connection_data in user_connections.by_key.values_mut() {
            for stream in &mut connection_data.streams {
                if stream.uid == uid {
                    // IMPORTANT: `resolve_disconnect_reason` in connection_manager.rs parses
                    // `channel.title` back via `CustomVideoStreamType::from_str` to determine QoS
                    // disconnect reasons. If these values change, update that function too.
                    stream.provider = "tuliprox".intern();
                    stream.channel.title = video_type.to_string().into();
                    stream.channel.group = "".intern();
                    stream.channel.technical = Some(Self::custom_stream_technical_info());
                    return Some(stream.clone());
                }
            }
        }
        None
    }

    pub async fn add_connection(&self, addr: &SocketAddr) {
        self.gc();
        let mut user_connections = self.write_connections().await;
        user_connections
            .key_by_addr
            .entry(*addr)
            .and_modify(|registration| registration.ts = current_time_secs())
            .or_insert_with(SocketRegistration::anonymous);
    }

    pub async fn update_connection(&self, update: ActiveUserConnectionParams<'_>) -> Option<StreamInfo> {
        self.update_connection_with_session_registration(update, None).await
    }

    pub(super) fn addr_has_direct_body_stream(connections: &UserConnections, addr: &SocketAddr) -> bool {
        let Some(registration) = connections.key_by_addr.get(addr) else {
            return false;
        };
        registration.usernames.iter().any(|username| {
            connections.by_key.get(username).is_some_and(|connection_data| {
                connection_data
                    .streams
                    .iter()
                    .any(|stream| stream.addr == *addr && uses_direct_body_idle_timeout(&stream.channel))
            })
        })
    }

    pub async fn get_username_for_addr(&self, addr: &SocketAddr) -> Option<String> {
        self.connections
            .read()
            .await
            .key_by_addr
            .get(addr)
            .and_then(SocketRegistration::primary_username)
            .map(str::to_string)
    }

    pub async fn get_usernames_for_addr(&self, addr: &SocketAddr) -> Vec<String> {
        self.connections.read().await.key_by_addr.get(addr).map(SocketRegistration::all_usernames).unwrap_or_default()
    }
}
