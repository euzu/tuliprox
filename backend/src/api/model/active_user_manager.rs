use crate::{
    api::model::{active_provider_manager::ConnectionKind, ActiveProviderManager, CustomVideoStreamType, EventManager, EventMessage},
    auth::Fingerprint,
    model::{Config, ProxyUserCredentials},
    utils::{debug_if_enabled, GeoIp},
};
use arc_swap::ArcSwapOption;
use jsonwebtoken::get_current_timestamp;
use log::{debug, info};
use shared::{
    model::{
        ActiveUserConnectionChange, PlaylistItemType, StreamChannel, StreamInfo, StreamTechnicalInfo,
        UserConnectionPermission, VirtualId,
    },
    utils::{
        current_time_secs, default_grace_period_millis, default_grace_period_timeout_secs, default_hls_session_ttl_secs,
        extract_extension_from_url, sanitize_sensitive_info, strip_port, Internable, DASH_EXT, HLS_EXT,
    },
};
use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    net::SocketAddr,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use lru::LruCache;
use tokio::sync::{mpsc, Mutex, Notify, RwLock};
use crate::api::model::connection_manager::CleanupEvent;
use tokio_util::sync::CancellationToken;
use crate::utils::utc_day_from_secs;

const USER_GC_TTL: u64 = 900; // 15 Min
const USER_CON_TTL: u64 = 1_800; // 30 minutes
const USER_SESSION_LIMIT: usize = 50;
const ANON_SOCKET_TTL: u64 = 300; // 5 Min
const DEFAULT_ACTIVE_SOCKET_TTL_SECS: u64 = 90;

fn get_grace_options(config: &Config) -> (u64, u64) {
    let (grace_period_millis, grace_period_timeout_secs) =
        config.reverse_proxy.as_ref().and_then(|r| r.stream.as_ref()).map_or_else(
            || (default_grace_period_millis(), default_grace_period_timeout_secs()),
            |s| (s.grace_period_millis, s.grace_period_timeout_secs),
        );
    (grace_period_millis, grace_period_timeout_secs)
}

fn get_adaptive_session_ttl_secs(config: &Config) -> u64 {
    config
        .reverse_proxy
        .as_ref()
        .and_then(|r| r.stream.as_ref())
        .map_or_else(default_hls_session_ttl_secs, |s| s.hls_session_ttl_secs)
}

fn stream_history_session_id(ts: u64, uid: u32) -> u64 {
    (ts << 32) | u64::from(uid)
}

fn decide_connection_kind(
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingProviderReason {
    GraceHold,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingProviderWakeSource {
    Activated,
    Timeout,
    CapacityNotify,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingProviderState {
    pub reason_code: PendingProviderReason,
    pub created_at: u64,
    pub deadline: u64,
    pub version: u64,
    pub wake_source: Option<PendingProviderWakeSource>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PlaybackLifecycle {
    #[default]
    Prepared,
    /// Waiting for a provider slot (`GraceMode::Hold`). The `data` field holds the pending state.
    PendingProvider { data: PendingProviderState },
    Active,
    /// Provisional counted state for `GraceMode::Instant`. Counts against limits immediately
    /// while the grace window resolves (success → Active, failure → Expired).
    GraceActive,
    Preserved,
    Expired,
}

impl PlaybackLifecycle {
    /// Returns true for lifecycle states that own a counted admission lease.
    /// Both `Active` and `GraceActive` count — `GraceActive` is a provisional
    /// counted state for `GraceMode::Instant` sessions.
    pub fn is_counted(&self) -> bool {
        matches!(self, Self::Active | Self::GraceActive)
    }
}

#[derive(Clone, Debug)]
pub struct UserSession {
    pub token: String,
    pub transition_version: u64,
    pub virtual_id: u32,
    pub provider: Arc<str>,
    pub stream_url: Arc<str>,
    pub addr: SocketAddr,
    pub socket_bound: bool,
    pub active_addrs: Vec<SocketAddr>,
    pub ts: u64,
    pub started_at: u64,
    pub permission: UserConnectionPermission,
    pub connection_kind: Option<ConnectionKind>,
    pub lifecycle: PlaybackLifecycle,
}

#[derive(Debug, Default, Clone, Copy)]
struct UserConnectionCounts {
    normal: u32,
    soft: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectionAdmission {
    pub(crate) permission: UserConnectionPermission,
    pub(crate) kind: Option<ConnectionKind>,
}

#[derive(Debug, Clone, Copy)]
struct PromotionAction {
    addr: SocketAddr,
    uid: u32,
    new_priority: i8,
}

#[derive(Debug)]
struct UserConnectionData {
    max_connections: u32,
    soft_connections: u16,
    counts: UserConnectionCounts,
    connections: u32,
    granted_grace: bool,
    grace_ts: u64,
    sessions: Vec<UserSession>,
    streams: Vec<StreamInfo>,
    stream_kinds: HashMap<u32, ConnectionKind>,
    stream_normal_priorities: HashMap<u32, i8>,
    ts: u64,
}

impl UserConnectionData {
    fn new(connections: u32, max_connections: u32, soft_connections: u16) -> Self {
        Self {
            max_connections,
            soft_connections,
            counts: UserConnectionCounts::default(),
            connections,
            granted_grace: false,
            grace_ts: 0,
            sessions: Vec::new(),
            streams: Vec::new(),
            stream_kinds: HashMap::new(),
            stream_normal_priorities: HashMap::new(),
            ts: current_time_secs(),
        }
    }

    fn add_session(&mut self, session: UserSession) {
        self.gc();
        self.sessions.push(session);
    }
    fn gc(&mut self) {
        if self.sessions.len() > USER_SESSION_LIMIT {
            self.sessions.sort_by_key(|e| std::cmp::Reverse(e.ts));
            self.sessions.truncate(USER_SESSION_LIMIT);
        }
    }

    fn has_session_addr(&self, addr: &SocketAddr) -> bool {
        self.sessions
            .iter()
            .any(|session| session.addr == *addr || session.active_addrs.contains(addr))
    }

    fn release_addr_from_sessions(&mut self, addr: &SocketAddr) -> HashMap<String, Option<SocketAddr>> {
        let mut migrated_addrs = HashMap::new();
        for session in &mut self.sessions {
            if session.addr == *addr || session.active_addrs.contains(addr) {
                migrated_addrs.insert(session.token.clone(), release_session_addr(session, addr));
            }
        }
        migrated_addrs
    }

    fn increment_kind(&mut self, kind: ConnectionKind) {
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

    fn decrement_kind(&mut self, kind: ConnectionKind) {
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

    fn try_promote_soft_stream(&mut self) -> Option<PromotionAction> {
        if self.counts.normal >= self.max_connections || (u32::from(self.counts.soft)) <= u32::from(self.soft_connections) {
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

        Some(PromotionAction {
            addr,
            uid,
            new_priority,
        })
    }

    fn try_promote_soft_session_reservation(&mut self) -> bool {
        if self.counts.normal >= self.max_connections || (u32::from(self.counts.soft)) <= u32::from(self.soft_connections) {
            return false;
        }

        let active_tokens = self
            .streams
            .iter()
            .filter_map(|stream| stream.session_token.as_deref())
            .collect::<HashSet<_>>();

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

    fn effective_counts_for_admission(&self, exclude_session_token: Option<&str>) -> UserConnectionCounts {
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

fn create_socket_reentry_guard_key(username: &str, client_ip: &str, virtual_id: VirtualId) -> String {
    shared::concat_string!(username, "|", client_ip, "|", &virtual_id.to_string())
}

fn is_stable_session_stream(stream: &StreamInfo) -> bool {
    stream.channel.item_type == PlaylistItemType::Catchup
        || stream.channel.item_type.is_live_adaptive()
        || matches!(
            extract_extension_from_url(stream.channel.url.as_ref()).as_deref(),
            Some(ext) if ext == HLS_EXT || ext == DASH_EXT
        )
}

#[derive(Clone, Copy, Debug)]
struct RecentWinnerProtection {
    protected_addr: SocketAddr,
    expires_at: u64,
}

#[derive(Debug, Default)]
struct UserConnections {
    kicked: HashMap<String, (u64, VirtualId)>,
    recently_evicted_sessions: HashMap<String, RecentWinnerProtection>,
    recent_socket_reentry_guards: HashMap<String, RecentWinnerProtection>,
    by_key: HashMap<String, UserConnectionData>,
    key_by_addr: HashMap<SocketAddr, SocketRegistration>,
}

#[derive(Clone, Debug)]
struct SocketRegistration {
    username: String,
    ts: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct AdaptiveExpiryEntry {
    expires_at: u64,
    username: String,
    session_token: String,
    uid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct AdaptiveExpiryKey {
    username: String,
    session_token: String,
    uid: u32,
}

pub struct ReleasedConnection {
    pub addr_removed: bool,
    pub removed_streams: Vec<StreamInfo>,
    pub disconnected_user: Option<String>,
}

pub struct ActiveUserConnectionParams<'a> {
    pub uid: u32,
    pub meter_uid: u32,
    pub username: &'a str,
    pub max_connections: u32,
    pub soft_connections: u16,
    pub connection_kind: ConnectionKind,
    pub priority: i8,
    pub soft_priority: i8,
    pub fingerprint: &'a Fingerprint,
    pub provider: Arc<str>,
    pub stream_channel: &'a StreamChannel,
    pub user_agent: Cow<'a, str>,
    pub session_token: Option<&'a str>,
}

pub struct CreateUserSessionParams<'a> {
    pub user: &'a ProxyUserCredentials,
    pub session_token: &'a str,
    pub virtual_id: u32,
    pub provider: &'a str,
    pub stream_url: &'a str,
    pub addr: &'a SocketAddr,
    pub connection_permission: UserConnectionPermission,
    pub connection_kind: Option<ConnectionKind>,
    pub socket_bound: bool,
}

fn remember_session_addr(session: &mut UserSession, addr: SocketAddr) {
    if session.socket_bound {
        session.active_addrs.clear();
    } else if let Some(position) = session.active_addrs.iter().position(|active_addr| *active_addr == addr) {
        session.active_addrs.remove(position);
    }
    session.active_addrs.push(addr);
    session.addr = addr;
}

fn release_session_addr(session: &mut UserSession, addr: &SocketAddr) -> Option<SocketAddr> {
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

fn clear_session_addr(session: &mut UserSession, addr: &SocketAddr) -> bool {
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

impl SocketRegistration {
    fn anonymous() -> Self {
        Self {
            username: String::new(),
            ts: current_time_secs(),
        }
    }
}

pub struct ActiveUserManager {
    grace_period_millis: AtomicU64,
    grace_period_timeout_secs: AtomicU64,
    adaptive_session_ttl_secs: AtomicU64,
    log_active_user: AtomicBool,
    gc_ts: Option<AtomicU64>,
    connections: RwLock<UserConnections>,
    adaptive_expiry_queue: Arc<Mutex<BinaryHeap<Reverse<AdaptiveExpiryEntry>>>>,
    adaptive_expiry_index: Arc<Mutex<HashMap<AdaptiveExpiryKey, u64>>>,
    adaptive_expiry_notify: Arc<Notify>,
    adaptive_expiry_cancel: CancellationToken,
    adaptive_expiry_worker_started: AtomicBool,
    event_manager: Arc<EventManager>,
    geo_ip: Arc<ArcSwapOption<GeoIp>>,
    last_logged_user_count: AtomicUsize,
    last_logged_user_connection_count: AtomicUsize,
    cleanup_tx: tokio::sync::OnceCell<mpsc::Sender<CleanupEvent>>,
    provider_manager: tokio::sync::OnceCell<Arc<ActiveProviderManager>>,
    transition_gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    pub(crate) dropped_cleanup_events: AtomicU64,
    divergence_cache: Mutex<LruCache<String, DivergenceEntry>>,
    divergence_cooldown_secs: u64,
}

struct DivergenceEntry {
    last_logged: Instant,
    count_since_last_log: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum DivergenceKind {
    CountedSessionWithoutStream,
    StreamWithoutCountedSession,
    ConnectionCountMismatch { legacy: u32, counted: u32 },
}

fn divergence_key(username: &str, kind: &DivergenceKind) -> String {
    match kind {
        DivergenceKind::CountedSessionWithoutStream => format!("{username}:CountedSessionWithoutStream"),
        DivergenceKind::StreamWithoutCountedSession => format!("{username}:StreamWithoutCountedSession"),
        DivergenceKind::ConnectionCountMismatch { legacy, counted } => {
            format!("{username}:ConnectionCountMismatch:{legacy}+{counted}")
        }
    }
}

struct DivergenceSnapshot {
    username: String,
    connections: u32,
    counted_sessions: usize,
    streams_count: usize,
    kinds: Vec<DivergenceKind>,
}

impl ActiveUserManager {
    pub fn shutdown(&self) {
        self.adaptive_expiry_cancel.cancel();
    }

    pub fn start_adaptive_expiry_worker(self: &Arc<Self>) {
        if self
            .adaptive_expiry_worker_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager.run_adaptive_expiry_worker().await;
        });
    }

    fn lookup_country(&self, client_ip: &str) -> Option<String> {
        let geoip = self.geo_ip.load();
        (*geoip)
            .as_ref()
            .and_then(|geoip_db| geoip_db.lookup(&strip_port(client_ip)))
    }

    fn custom_stream_technical_info() -> StreamTechnicalInfo {
        StreamTechnicalInfo {
            container: String::from("mpegts"),
            resolution: String::new(),
            fps: String::from("30"),
            video_codec: String::from("H.264"),
            audio_codec: String::from("AAC"),
            audio_channels: String::from("Stereo"),
        }
    }

    pub fn new(config: &Config, geoip: &Arc<ArcSwapOption<GeoIp>>, event_manager: &Arc<EventManager>) -> Self {
        let log_active_user: bool = config.log.as_ref().is_some_and(|l| l.log_active_user);
        let (grace_period_millis, grace_period_timeout_secs) = get_grace_options(config);

        Self {
            grace_period_millis: AtomicU64::new(grace_period_millis),
            grace_period_timeout_secs: AtomicU64::new(grace_period_timeout_secs),
            adaptive_session_ttl_secs: AtomicU64::new(get_adaptive_session_ttl_secs(config)),
            log_active_user: AtomicBool::new(log_active_user),
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
            dropped_cleanup_events: AtomicU64::new(0),
            divergence_cache: Mutex::new(LruCache::new(NonZeroUsize::new(256).unwrap())),
            divergence_cooldown_secs: 300,
        }
    }

    fn transition_gate_key(username: &str, token: &str) -> String {
        let mut key = String::with_capacity(username.len() + token.len() + 1);
        key.push_str(username);
        key.push('\0');
        key.push_str(token);
        key
    }

    pub(crate) async fn acquire_playback_transition(&self, username: &str, token: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let key = Self::transition_gate_key(username, token);
        let gate = {
            let mut transition_gates = self.transition_gates.lock().await;
            Arc::clone(
                transition_gates
                    .entry(key)
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        gate.lock_owned().await
    }

    pub(crate) fn set_cleanup_sender(&self, tx: mpsc::Sender<CleanupEvent>) {
        let _ = self.cleanup_tx.set(tx);
    }

    pub(crate) fn set_provider_manager(&self, provider_manager: Arc<ActiveProviderManager>) {
        let _ = self.provider_manager.set(provider_manager);
    }

    /// Collect a snapshot of all currently active streams for shutdown history recording.
    pub(crate) async fn get_all_active_streams(&self) -> Vec<shared::model::StreamInfo> {
        let connections = self.connections.read().await;
        connections.by_key.values()
            .flat_map(|data| data.streams.iter().filter(|stream| !stream.preserved).cloned())
            .collect()
    }

    async fn log_active_user(&self) {
        let is_log_user_enabled = self.is_log_user_enabled();
        let (user_count, user_connection_count) = { self.active_users_and_connections().await };
        self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Connections(
            user_count,
            user_connection_count,
        )));
        if is_log_user_enabled {
            let last_user_count = self.last_logged_user_count.load(Ordering::Relaxed);
            let last_connection_count = self.last_logged_user_connection_count.load(Ordering::Relaxed);
            if last_user_count != user_count || last_connection_count != user_connection_count {
                self.last_logged_user_count.store(user_count, Ordering::Relaxed);
                self.last_logged_user_connection_count.store(user_connection_count, Ordering::Relaxed);
                info!("Active Users: {user_count}, Active User Connections: {user_connection_count}");
            }
        }
    }

    async fn emit_promotion_update(&self, username: &str, action: PromotionAction) {
        if let Some(provider_manager) = self.provider_manager.get() {
            provider_manager
                .reclassify_connection(&action.addr, ConnectionKind::Normal, action.new_priority)
                .await;
        }

        let maybe_stream = {
            let user_connections = self.connections.read().await;
            user_connections
                .by_key
                .get(username)
                .and_then(|connection_data| connection_data.streams.iter().find(|stream| stream.uid == action.uid).cloned())
        };
        if let Some(stream_info) = maybe_stream {
            self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(stream_info)));
        }
    }

    /// Releases an active stream for the given socket address without removing the
    /// socket registration (`key_by_addr`). This is used when a stream ends while
    /// the underlying HTTP connection may still remain open.
    #[allow(clippy::too_many_lines)]
    pub async fn release_stream(&self, addr: &SocketAddr) -> Option<StreamInfo> {
        let (removed_stream, username, expiry_entry, connection_changed, promotion, divergence_snapshot) = {
            let mut user_connections = self.connections.write().await;

            let username = user_connections.key_by_addr.get(addr).map(|reg| reg.username.clone())?;

            let mut removed_stream = None;
            let mut expiry_entry = None;
            let mut connection_changed = false;
            let mut promotion = None;
            if let Some(connection_data) = user_connections.by_key.get_mut(&username) {
                let migrated_session_addrs = connection_data.release_addr_from_sessions(addr);
                if let Some(stream_idx) = connection_data
                    .streams
                    .iter()
                    .position(|stream| stream.addr == *addr && !stream.preserved)
                {
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
                        let preserved_session_token =
                            connection_data.streams[stream_idx].session_token.clone();
                        if let Some(entry) = self.build_preserved_stream_expiry(
                            &username,
                            &connection_data.streams[stream_idx],
                            &connection_data.sessions,
                        ) {
                            if let Some(kind) = connection_data.stream_kinds.remove(&connection_data.streams[stream_idx].uid) {
                                connection_data.decrement_kind(kind);
                                connection_changed = true;
                            }
                            connection_data
                                .stream_normal_priorities
                                .remove(&connection_data.streams[stream_idx].uid);
                            if let Some(session_token) = preserved_session_token.as_deref() {
                                Self::clear_session_counted(connection_data, session_token);
                            }
                            connection_data.streams[stream_idx].preserved = true;
                            expiry_entry = Some(entry);
                        } else {
                            removed_stream = Some(connection_data.streams.swap_remove(stream_idx));
                        }
                    } else {
                        removed_stream = Some(connection_data.streams.swap_remove(stream_idx));
                    }
                    if let Some(removed_stream) = removed_stream.as_ref() {
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
                        if let Some(session_token) = removed_stream
                            .as_ref()
                            .and_then(|stream| stream.session_token.as_deref())
                        {
                            Self::clear_session_counted_without_stream(connection_data, session_token);
                        }
                        while connection_data.try_promote_soft_session_reservation() {}
                    }
                }
                let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, &username);
                (removed_stream, username, expiry_entry, connection_changed, promotion, Some(divergence_snapshot))
            } else {
                (None, username, None, false, None, None)
            }
        };

        if let Some(snapshot) = divergence_snapshot {
            self.log_divergence_snapshot(snapshot).await;
        }

        if let Some(entry) = expiry_entry {
            self.enqueue_adaptive_expiry(entry).await;
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

        removed_stream
    }

    #[allow(clippy::too_many_lines)]
    async fn release_connection_inner(&self, addr: &SocketAddr, preserve_session_streams: bool) -> ReleasedConnection {
        let (addr_removed, disconnected_user, removed_streams, expiry_entries, promotions) = {
            let mut user_connections = self.connections.write().await;

            let registration = user_connections.key_by_addr.remove(addr);
            let had_registration = registration.is_some();
            let fallback_username = if had_registration {
                None
            } else {
                user_connections.by_key.iter().find_map(|(username, connection_data)| {
                    connection_data
                        .streams
                        .iter()
                        .any(|stream| stream.addr == *addr)
                        .then(|| username.clone())
                })
            };

            let username = registration.map(|registration| registration.username).or(fallback_username);

            if let Some(username) = username {
                let mut removed_streams = Vec::new();
                let mut expiry_entries = Vec::new();
                let mut promotions = Vec::new();
                if let Some(connection_data) = user_connections.by_key.get_mut(&username) {
                    let migrated_session_addrs = connection_data.release_addr_from_sessions(addr);
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
                                if let Some(entry) =
                                self.build_preserved_stream_expiry(&username, &stream_info, &connection_data.sessions)
                            {
                                if let Some(kind) = connection_data.stream_kinds.remove(&stream_info.uid) {
                                    released_kinds.push(kind);
                                }
                                connection_data.stream_normal_priorities.remove(&stream_info.uid);
                                if let Some(token) = stream_info.session_token.as_ref() {
                                    preserved_session_tokens.push(token.clone());
                                }
                                    stream_info.preserved = true;
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
                    if !preserve_session_streams && !removed_session_tokens.is_empty() {
                        connection_data
                            .sessions
                            .retain(|session| !removed_session_tokens.contains(&session.token));
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
                        promotions.push(action);
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
                }
                let state_changed = had_registration || !removed_streams.is_empty();
                (state_changed, Some(username), removed_streams, expiry_entries, promotions)
            } else {
                (false, None, Vec::new(), Vec::new(), Vec::new())
            }
        };

        for entry in expiry_entries {
            self.enqueue_adaptive_expiry(entry).await;
        }

        if let Some(ref username) = disconnected_user {
            if !username.is_empty() {
                debug_if_enabled!(
                    "Released connection for user {username} at {}",
                    sanitize_sensitive_info(&addr.to_string())
                );
            }
            if addr_removed {
                self.log_active_user().await;
                for action in promotions {
                    self.emit_promotion_update(username, action).await;
                }
            }
        }

        ReleasedConnection { addr_removed, removed_streams, disconnected_user }
    }

    pub async fn release_connection(&self, addr: &SocketAddr) -> ReleasedConnection {
        let released = self.release_connection_inner(addr, true).await;
        // divergence check after connection release
        if released.addr_removed {
            if let Some(ref username) = released.disconnected_user {
                self.check_and_log_divergence_for_user(username).await;
            }
        }
        released
    }

    pub async fn release_connection_as_kicked(&self, addr: &SocketAddr) -> ReleasedConnection {
        let released = self.release_connection_inner(addr, false).await;
        // divergence check after connection release
        if released.addr_removed {
            if let Some(ref username) = released.disconnected_user {
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
        self.adaptive_session_ttl_secs
            .store(get_adaptive_session_ttl_secs(config), Ordering::Relaxed);
        self.log_active_user.store(log_active_user, Ordering::Relaxed);
    }

    pub async fn user_connections(&self, username: &str) -> u32 {
        if let Some(connection_data) = self.connections.read().await.by_key.get(username) {
            return connection_data.connections;
        }
        0
    }

    fn check_connection_admission_with_counts(
        &self,
        username: &str,
        connection_data: &mut UserConnectionData,
        counts: UserConnectionCounts,
    ) -> ConnectionAdmission {
        let selected_kind = decide_connection_kind(counts, connection_data.max_connections, connection_data.soft_connections);
        let effective_connections = counts.normal.saturating_add(u32::from(counts.soft));

        if let Some(kind) = selected_kind {
            // Reset grace only once the user is back below the hard limit.
            if effective_connections < connection_data.max_connections {
                connection_data.granted_grace = false;
                connection_data.grace_ts = 0;
            }
            return ConnectionAdmission {
                permission: UserConnectionPermission::Allowed,
                kind: Some(kind),
            };
        }

        let now = get_current_timestamp();
        // Check if user already used a grace period
        if connection_data.granted_grace {
            if effective_connections >= connection_data.max_connections
                && now - connection_data.grace_ts <= self.grace_period_timeout_secs.load(Ordering::Relaxed)
            {
                // Grace timeout, still active, deny connection
                debug!("User access denied, grace exhausted, too many connections: {username}");
                return ConnectionAdmission {
                    permission: UserConnectionPermission::Exhausted,
                    kind: None,
                };
            }
            // Grace timeout expired, reset grace counters
            if effective_connections < connection_data.max_connections {
                connection_data.granted_grace = false;
                connection_data.grace_ts = 0;
            }
        }

        debug!("User access denied, too many connections: {username}");
        ConnectionAdmission {
            permission: UserConnectionPermission::Exhausted,
            kind: None,
        }
    }

    fn check_connection_admission(&self, username: &str, connection_data: &mut UserConnectionData) -> ConnectionAdmission {
        self.check_connection_admission_with_counts(
            username,
            connection_data,
            connection_data.effective_counts_for_admission(None),
        )
    }

    pub(crate) async fn connection_admission(
        &self,
        username: &str,
        max_connections: u32,
        soft_connections: u16,
    ) -> ConnectionAdmission {
        if max_connections > 0 || soft_connections > 0 {
            if let Some(connection_data) = self.connections.write().await.by_key.get_mut(username) {
                connection_data.max_connections = max_connections;
                connection_data.soft_connections = soft_connections;
                return self.check_connection_admission(username, connection_data);
            }
        }
        ConnectionAdmission {
            permission: UserConnectionPermission::Allowed,
            kind: Some(ConnectionKind::Normal),
        }
    }

    pub async fn connection_permission(
        &self,
        username: &str,
        max_connections: u32,
        soft_connections: u16,
    ) -> UserConnectionPermission {
        self.connection_admission(username, max_connections, soft_connections).await.permission
    }

    pub(crate) async fn connection_admission_for_session(
        &self,
        username: &str,
        max_connections: u32,
        soft_connections: u16,
        session_token: &str,
    ) -> ConnectionAdmission {
        if max_connections == 0 && soft_connections == 0 {
            return ConnectionAdmission {
                permission: UserConnectionPermission::Allowed,
                kind: Some(ConnectionKind::Normal),
            };
        }

        let mut connections = self.connections.write().await;
        let Some(connection_data) = connections.by_key.get_mut(username) else {
            return ConnectionAdmission {
                permission: UserConnectionPermission::Allowed,
                kind: Some(ConnectionKind::Normal),
            };
        };
        connection_data.max_connections = max_connections;
        connection_data.soft_connections = soft_connections;

        let Some(session_index) = connection_data.sessions.iter().position(|session| session.token == session_token) else {
            return self.check_connection_admission(username, connection_data);
        };

        if connection_data.sessions[session_index].lifecycle.is_counted() {
            return ConnectionAdmission {
                permission: UserConnectionPermission::Allowed,
                kind: connection_data.sessions[session_index]
                    .connection_kind
                    .or(Some(ConnectionKind::Normal)),
            };
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
            .permission
    }

    pub(crate) async fn get_eviction_candidates(
        &self,
        username: &str,
        _client_ip: &str,
    ) -> Vec<crate::api::model::EvictionCandidate> {
        let connections = self.connections.read().await;
        let Some(connection_data) = connections.by_key.get(username) else {
            return Vec::new();
        };
        let mut addr_counts = HashMap::new();
        for stream in &connection_data.streams {
            // Preserved streams do not occupy a counted slot — exclude from addr counts.
            // They are still valid eviction candidates (see filter below), but they don't
            // consume connection capacity, so they don't contribute to the "singleton addr" logic.
            let contributes_to_count = if stream.preserved {
                false
            } else if let Some(token) = stream.session_token.as_deref() {
                connection_data.sessions.iter().any(|s| s.token == token && s.lifecycle.is_counted())
            } else {
                true // orphan streams without a session are counted
            };
            if contributes_to_count {
                addr_counts
                    .entry(stream.addr)
                    .and_modify(|count: &mut u8| *count = count.saturating_add(1))
                    .or_insert(1_u8);
            }
        }
        let candidates: Vec<_> = connection_data
            .streams
            .iter()
            .filter(|stream| {
                if let Some(token) = stream.session_token.as_deref() {
                    connection_data.sessions.iter().any(|s| s.token == token && s.lifecycle.is_counted()) || stream.preserved
                } else {
                    true
                }
            })
            .filter(|stream| {
                let addr_count = addr_counts.get(&stream.addr).copied().unwrap_or(0);
                if stream.preserved {
                    // Preserved streams are always valid eviction candidates — they hold no counted
                    // slot. addr_count is 0 for preserved-only addresses, 1+ for addresses with
                    // counted competition. Either way they can be evicted.
                    true
                } else {
                    // Non-preserved streams: only on singleton counted addresses.
                    // addr_count 0 = no counted streams at this address (shouldn't happen since
                    // non-preserved streams aren't preserved, but addr_count would be >= 1).
                    // addr_count 1 = single counted stream at address — candidate.
                    // addr_count > 1 = multiple counted streams — not a singleton, not candidate.
                    addr_count == 1
                }
            })
            .map(|s| crate::api::model::EvictionCandidate {
                addr: s.addr,
                client_ip: s.client_ip.clone(),
                ts: s.ts,
            })
            .collect();
        candidates
    }

    pub(crate) async fn grant_grace(&self, username: &str) -> bool {
        if self.grace_period_millis.load(Ordering::Relaxed) == 0 {
            debug!("Grace grant denied, grace_period_millis is zero for {username}");
            return false;
        }
        let mut connections = self.connections.write().await;
        if let Some(connection_data) = connections.by_key.get_mut(username) {
            let now = get_current_timestamp();
            if connection_data.connections < connection_data.max_connections {
                debug!(
                    "Grace grant denied for {username}, user not at connection limit ({}/{})",
                    connection_data.connections,
                    connection_data.max_connections
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

    pub async fn active_users_and_connections(&self) -> (usize, usize) {
        self.gc();
        let user_connections = self.connections.read().await;
        user_connections
            .by_key
            .values()
            .filter_map(|c| {
                let effective = c.connections as usize;
                if effective > 0 { Some(effective) } else { None }
            })
            .fold((0usize, 0usize), |(user_count, conn_count), effective| (user_count + 1, conn_count + effective))
    }

    pub async fn update_stream_detail(
        &self,
        addr: &SocketAddr,
        video_type: CustomVideoStreamType,
    ) -> Option<StreamInfo> {
        let mut user_connections = self.connections.write().await;
        let username = {
            match user_connections.key_by_addr.get(addr) {
                Some(registration) => registration.username.clone(),
                None => return None,
            }
        };
        if let Some(connection_data) = user_connections.by_key.get_mut(&username) {
            for stream in &mut connection_data.streams {
                if &stream.addr == addr {
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
        let mut user_connections = self.connections.write().await;
        user_connections
            .key_by_addr
            .entry(*addr)
            .and_modify(|registration| registration.ts = current_time_secs())
            .or_insert_with(SocketRegistration::anonymous);
    }

    #[allow(clippy::too_many_lines)]
    pub async fn update_connection(&self, update: ActiveUserConnectionParams<'_>) -> Option<StreamInfo> {
        let ActiveUserConnectionParams {
            uid,
            meter_uid,
            username,
            max_connections,
            soft_connections,
            connection_kind,
            priority,
            soft_priority: _,
            fingerprint,
            provider,
            stream_channel,
            user_agent,
            session_token,
        } = update;
        let (stream_info, divergence_snapshot) = {
            let mut user_connections = self.connections.write().await;

            let now = current_time_secs();
            if let Some(registration) = user_connections.key_by_addr.get_mut(&fingerprint.addr) {
                registration.username = username.to_string();
                registration.ts = now;
            } else {
                user_connections.key_by_addr.insert(
                    fingerprint.addr,
                    SocketRegistration {
                        username: username.to_string(),
                        ts: now,
                    },
                );
            }

            let tracked_socket_count = user_connections.key_by_addr.len();
            let connection_data = user_connections
                .by_key
                .entry(username.to_string())
                .or_insert_with(|| UserConnectionData::new(0, max_connections, soft_connections));
            connection_data.max_connections = max_connections;
            connection_data.soft_connections = soft_connections;

            if let Some(token) = session_token {
                if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == token) {
                    session.ts = now;
                    remember_session_addr(session, fingerprint.addr);
                    Self::bump_session_transition_version(session);
                }
            }

            let user_agent_string = user_agent.to_string();
            let reserved_session_kind = session_token.and_then(|token| {
                connection_data
                    .sessions
                    .iter()
                    .find(|session| session.token == token && session.lifecycle.is_counted())
                    .map(|session| session.connection_kind.unwrap_or(connection_kind))
            });

            let existing_stream_info = connection_data
                .streams
                .iter()
                .position(|stream_info| match session_token {
                    Some(token) => stream_info.session_token.as_deref() == Some(token),
                    None => stream_info.addr == fingerprint.addr && stream_info.session_token.is_none(),
                })
                .map(|stream_idx| {
                    let session_started_at = session_token.and_then(|token| {
                        connection_data.sessions.iter()
                            .find(|s| s.token == token)
                            .map(|s| s.started_at)
                    });

                    let stream_info = &mut connection_data.streams[stream_idx];
                    let client_ip = fingerprint.client_ip.clone();
                    let preserve_started_at = stream_info.session_token.is_some()
                        && (stream_info.channel.item_type.is_live_adaptive() || stream_channel.item_type.is_live_adaptive());
                    let was_preserved = stream_info.preserved;
                    let old_session_id = stream_history_session_id(stream_info.ts, stream_info.uid);
                    stream_info.meter_uid = meter_uid;
                    stream_info.addr = fingerprint.addr;
                    stream_info.client_ip.clone_from(&client_ip);
                    stream_info.country_code = self.lookup_country(&client_ip);
                    stream_info.channel = stream_channel.clone();
                    stream_info.provider = provider.clone();
                    stream_info.user_agent.clone_from(&user_agent_string);
                    
                    if let Some(started_at) = session_started_at {
                        stream_info.started_at = started_at;
                    }

                    if preserve_started_at {
                        let now = current_time_secs();
                        if utc_day_from_secs(stream_info.ts) != utc_day_from_secs(now) {
                            stream_info.ts = now;
                            stream_info.previous_session_id = Some(old_session_id);
                        }
                    } else {
                        stream_info.ts = current_time_secs();
                    }

                    if let Some(token) = session_token {
                        stream_info.session_token = Some(token.to_string());
                    }
                    if was_preserved {
                        stream_info.preserved = false;
                    }
                    connection_data.stream_normal_priorities.insert(stream_info.uid, priority);
                    let result = stream_info.clone();
                    stream_info.previous_session_id = None;
                    (result, was_preserved)
                });
            if let Some((stream_info, was_preserved)) = existing_stream_info {
                let effective_connection_kind = reserved_session_kind.unwrap_or(connection_kind);
                if was_preserved {
                    connection_data.increment_kind(effective_connection_kind);
                }
                connection_data.stream_kinds.insert(stream_info.uid, effective_connection_kind);
                connection_data.stream_normal_priorities.insert(stream_info.uid, priority);
                if let Some(token) = session_token {
                    if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == token) {
                        Self::mark_session_committed(session, effective_connection_kind);
                    }
                }
                let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, username);
                (stream_info, divergence_snapshot)
            } else {
                let effective_connection_kind = reserved_session_kind.unwrap_or(connection_kind);
                let country_code = self.lookup_country(&fingerprint.client_ip);

                let mut stream_info = StreamInfo::new(
                    uid,
                    meter_uid,
                    username,
                    &fingerprint.addr,
                    &fingerprint.client_ip,
                    provider,
                    stream_channel.clone(),
                    user_agent_string,
                    country_code,
                    session_token,
                );

                if let Some(token) = session_token {
                    if let Some(session) = connection_data.sessions.iter().find(|s| s.token == token) {
                        stream_info.started_at = session.started_at;
                    }
                }

                if reserved_session_kind.is_none() {
                    connection_data.increment_kind(effective_connection_kind);
                }
                connection_data.streams.push(stream_info.clone());
                connection_data.stream_kinds.insert(stream_info.uid, effective_connection_kind);
                connection_data.stream_normal_priorities.insert(stream_info.uid, priority);
                if let Some(token) = session_token {
                    if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == token) {
                        Self::mark_session_committed(session, effective_connection_kind);
                    }
                }
                Self::log_connection_added(username, &fingerprint.addr, connection_data, tracked_socket_count);
                let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, username);
                (stream_info, divergence_snapshot)
            }
        };

        self.log_divergence_snapshot(divergence_snapshot).await;

        self.log_active_user().await;

        Some(stream_info)
    }

    fn is_log_user_enabled(&self) -> bool { self.log_active_user.load(Ordering::Relaxed) }

    fn build_preserved_stream_expiry(
        &self,
        username: &str,
        stream: &StreamInfo,
        sessions: &[UserSession],
    ) -> Option<AdaptiveExpiryEntry> {
        let session_token = stream.session_token.as_deref()?;
        let session = sessions.iter().find(|session| session.token == session_token)?;

        let ttl_secs = self.adaptive_session_ttl_secs.load(Ordering::Relaxed);
        let expires_at = session.ts.saturating_add(ttl_secs);
        Some(AdaptiveExpiryEntry {
            expires_at,
            username: username.to_string(),
            session_token: session_token.to_string(),
            uid: stream.uid,
        })
    }

    async fn enqueue_adaptive_expiry(&self, entry: AdaptiveExpiryEntry) {
        let key = AdaptiveExpiryKey {
            username: entry.username.clone(),
            session_token: entry.session_token.clone(),
            uid: entry.uid,
        };

        let mut expiry_index = self.adaptive_expiry_index.lock().await;
        expiry_index.insert(key, entry.expires_at);
        drop(expiry_index);

        let mut queue = self.adaptive_expiry_queue.lock().await;
        let wake_worker = queue.peek().is_none_or(|current| entry.expires_at < current.0.expires_at);
        queue.push(Reverse(entry));
        if wake_worker {
            self.adaptive_expiry_notify.notify_one();
        }
    }


    #[allow(clippy::too_many_arguments)]
    fn new_user_session(
        session_token: &str,
        virtual_id: u32,
        provider: &str,
        stream_url: &str,
        addr: &SocketAddr,
        connection_permission: UserConnectionPermission,
        connection_kind: Option<ConnectionKind>,
        socket_bound: bool,
    ) -> UserSession {
        let now = current_time_secs();
        UserSession {
            token: session_token.to_string(),
            transition_version: 1,
            virtual_id,
            provider: provider.intern(),
            stream_url: stream_url.intern(),
            addr: *addr,
            socket_bound,
            active_addrs: vec![*addr],
            ts: now,
            started_at: now,
            permission: connection_permission,
            connection_kind,
            lifecycle: PlaybackLifecycle::Prepared,
        }
    }

    fn promote_session_for_stream(connection_data: &mut UserConnectionData, stream: &StreamInfo) {
        if let Some(token) = stream.session_token.as_deref() {
            if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == token) {
                Self::mark_session_committed(session, ConnectionKind::Normal);
            }
        }
    }

    fn bump_session_transition_version(session: &mut UserSession) -> u64 {
        session.transition_version = session.transition_version.saturating_add(1);
        session.transition_version
    }

    fn mark_session_committed(session: &mut UserSession, kind: ConnectionKind) {
        session.connection_kind = Some(kind);
        session.lifecycle = PlaybackLifecycle::Active;
        Self::bump_session_transition_version(session);
    }

    fn update_session_admission(
        session: &mut UserSession,
        permission: UserConnectionPermission,
        kind: Option<ConnectionKind>,
    ) {
        session.permission = permission;
        if let Some(kind) = kind {
            session.connection_kind = Some(kind);
        }
    }

    fn clear_session_pending_with_permission(
        session: &mut UserSession,
        permission: UserConnectionPermission,
        wake_source: PendingProviderWakeSource,
    ) {
        if let PlaybackLifecycle::PendingProvider { data } = &mut session.lifecycle {
            data.wake_source = Some(wake_source);
        }
        Self::bump_session_transition_version(session);
        session.permission = permission;
    }

    fn session_has_stream(connection_data: &UserConnectionData, session_token: &str) -> bool {
        connection_data
            .streams
            .iter()
            .any(|stream| stream.session_token.as_deref() == Some(session_token))
    }

    fn clear_session_counted_without_stream(connection_data: &mut UserConnectionData, session_token: &str) {
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
                    session.lifecycle = PlaybackLifecycle::Expired;
                }
                _ => {}
            }
        }
    }

    fn clear_session_counted(connection_data: &mut UserConnectionData, session_token: &str) {
        if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == session_token) {
            match session.lifecycle {
                PlaybackLifecycle::Active => {
                    session.lifecycle = PlaybackLifecycle::Preserved;
                }
                PlaybackLifecycle::GraceActive => {
                    // GraceActive without stream: the grace failed. The stream was already
                    // removed (this function is called after stream removal), so expire the session.
                    session.lifecycle = PlaybackLifecycle::Expired;
                }
                _ => {}
            }
        }
    }

    fn release_expired_session_reservations(connection_data: &mut UserConnectionData, now: u64) {
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

    pub(crate) async fn connection_admission_for_session_activation(
        &self,
        username: &str,
        max_connections: u32,
        soft_connections: u16,
        session_token: &str,
    ) -> ConnectionAdmission {
        if max_connections == 0 && soft_connections == 0 {
            return ConnectionAdmission {
                permission: UserConnectionPermission::Allowed,
                kind: Some(ConnectionKind::Normal),
            };
        }

        let mut connections = self.connections.write().await;
        let Some(connection_data) = connections.by_key.get_mut(username) else {
            return ConnectionAdmission {
                permission: UserConnectionPermission::Allowed,
                kind: Some(ConnectionKind::Normal),
            };
        };
        connection_data.max_connections = max_connections;
        connection_data.soft_connections = soft_connections;

        let Some(session_index) = connection_data.sessions.iter().position(|session| session.token == session_token) else {
            return self.check_connection_admission(username, connection_data);
        };

        // Existing counted session or session with an active stream: entitled to its slot.
        if connection_data.sessions[session_index].lifecycle.is_counted() || Self::session_has_stream(connection_data, session_token) {
            return ConnectionAdmission {
                permission: UserConnectionPermission::Allowed,
                kind: connection_data.sessions[session_index]
                    .connection_kind
                    .or(Some(ConnectionKind::Normal)),
            };
        }

        // Existing uncounted session without a stream: check for a preserved stream.
        // A preserved stream consumed a slot in the past and must remain evictable.
        // Returning Exhausted here ensures eviction strategies are evaluated to free the
        // preserved slot. This fixes the HLS->TS transition bug where preserved HLS
        // streams were never evicted because strategy evaluation was skipped.
        let has_preserved = connection_data
            .streams
            .iter()
            .any(|s| s.session_token.as_deref() == Some(session_token) && s.preserved);

        let admission = self.check_connection_admission_with_counts(
            username,
            connection_data,
            connection_data.effective_counts_for_admission(Some(session_token)),
        );
        let kind = admission.kind.unwrap_or(ConnectionKind::Normal);
        if admission.permission == UserConnectionPermission::Allowed {
            let session = &mut connection_data.sessions[session_index];
            Self::update_session_admission(session, admission.permission, Some(kind));
        }
        if has_preserved {
            return ConnectionAdmission { permission: UserConnectionPermission::Exhausted, kind: admission.kind };
        }
        // Only consume the slot if admission was granted AND the session has a preserved stream
        // that needs to be accounted for. Fresh uncounted sessions without streams stay
        // uncounted until the provider commits a stream (via mark_session_committed).
        if admission.permission == UserConnectionPermission::Allowed && has_preserved {
            connection_data.increment_kind(kind);
        }
        admission
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
        let mut user_connections = self.connections.write().await;
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

        let session = Self::new_user_session(
            session_token,
            virtual_id,
            provider,
            stream_url,
            addr,
            connection_permission,
            connection_kind,
            socket_bound,
        );
        let version = session.transition_version;
        connection_data.add_session(session);
        let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, &username);
        drop(user_connections);
        self.log_divergence_snapshot(divergence_snapshot).await;
        version
    }

    pub async fn release_unbound_session_reservation(
        &self,
        username: &str,
        session_token: &str,
        expected_transition_version: Option<u64>,
        remove_session_if_unbound: bool,
    ) {
        let (connection_changed, user_removed, promotions, divergence_snapshot) = {
            let mut user_connections = self.connections.write().await;
            let Some(connection_data) = user_connections.by_key.get_mut(username) else {
                return;
            };

            if Self::session_has_stream(connection_data, session_token) {
                return;
            }

            let Some(session_index) = connection_data.sessions.iter().position(|session| session.token == session_token) else {
                return;
            };

            if expected_transition_version.is_some_and(|expected| {
                connection_data.sessions[session_index].transition_version != expected
            }) {
                return;
            }

            let mut connection_changed = false;
            if connection_data.sessions[session_index].lifecycle.is_counted() {
                let kind = connection_data.sessions[session_index]
                    .connection_kind
                    .unwrap_or(ConnectionKind::Normal);
                connection_data.decrement_kind(kind);
                connection_data.sessions[session_index].lifecycle = PlaybackLifecycle::Expired;
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
                let promoted_stream = connection_data.streams.iter().find(|stream| stream.uid == action.uid).cloned();
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

        self.log_divergence_snapshot(divergence_snapshot).await;

        if user_removed {
            let mut user_connections = self.connections.write().await;
            user_connections.by_key.remove(username);
        }
        if connection_changed || user_removed {
            self.log_active_user().await;
        }
        for action in promotions {
            self.emit_promotion_update(username, action).await;
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
        let mut user_connections = self.connections.write().await;
        let connection_data = user_connections.by_key.entry(username.clone()).or_insert_with(|| {
            debug_if_enabled!("Creating first session for user {username} {}", sanitize_sensitive_info(stream_url));
            let mut data = UserConnectionData::new(0, user.max_connections, user.soft_connections);
            let session =
                Self::new_user_session(
                    session_token,
                    virtual_id,
                    provider,
                    stream_url,
                    addr,
                    connection_permission,
                    connection_kind,
                    socket_bound,
                );
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
                if &*session.stream_url != stream_url {
                    session.stream_url = stream_url.intern();
                }
                if &*session.provider != provider {
                    session.provider = provider.intern();
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
        let session = Self::new_user_session(
            session_token,
            virtual_id,
            provider,
            stream_url,
            addr,
            connection_permission,
            connection_kind,
            socket_bound,
        );
        let token = session.token.clone();
        connection_data.add_session(session);
        let divergence_snapshot = Self::collect_divergence_snapshot(connection_data, &username);
        drop(user_connections);
        self.log_divergence_snapshot(divergence_snapshot).await;
        token
    }

    pub async fn update_session_addr(&self, username: &str, token: &str, addr: &SocketAddr) {
        let now = current_time_secs();
        let mut user_connections = self.connections.write().await;
        if let Some(connection_data) = user_connections.by_key.get_mut(username) {
            let update_result = if let Some(session) = connection_data.sessions.iter_mut().find(|s| s.token == token) {
                let previous_addr = session.addr;
                remember_session_addr(session, *addr);
                session.ts = now;
                Self::bump_session_transition_version(session);
                for stream in &mut connection_data.streams {
                    if stream.addr == previous_addr {
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
                if let Some(registration) = user_connections.key_by_addr.get_mut(addr) {
                    registration.ts = now;
                    registration.username = username.to_string();
                } else {
                    user_connections.key_by_addr.insert(
                        *addr,
                        SocketRegistration {
                            username: username.to_string(),
                            ts: now,
                        },
                    );
                }
                if prune_previous_registration {
                    let can_remove_previous = user_connections
                        .key_by_addr
                        .get(&previous_addr)
                        .is_some_and(|registration| registration.username == username);
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

    pub async fn clear_unbound_session_addr(&self, username: &str, token: &str, addr: &SocketAddr) {
        let now = current_time_secs();
        let mut user_connections = self.connections.write().await;
        let Some(connection_data) = user_connections.by_key.get_mut(username) else {
            return;
        };
        if Self::session_has_stream(connection_data, token) {
            return;
        }

        let cleared = if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == token) {
            let changed = clear_session_addr(session, addr);
            if changed {
                Self::bump_session_transition_version(session);
            }
            changed
        } else {
            false
        };
        if !cleared {
            return;
        }

        let can_remove_registration = !connection_data.has_session_addr(addr)
            && !connection_data.streams.iter().any(|stream| stream.addr == *addr);
        if can_remove_registration {
            let can_remove = user_connections
                .key_by_addr
                .get(addr)
                .is_some_and(|registration| registration.username == username);
            if can_remove {
                user_connections.key_by_addr.remove(addr);
            }
        } else if let Some(registration) = user_connections.key_by_addr.get_mut(addr) {
            registration.ts = now;
        }
    }

    pub async fn mark_pending_provider(
        &self,
        username: &str,
        token: &str,
        reason_code: PendingProviderReason,
        deadline: u64,
    ) -> Option<u64> {
        let mut user_connections = self.connections.write().await;
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
                data: PendingProviderState {
                    reason_code,
                    created_at: now,
                    deadline,
                    version,
                    wake_source: None,
                },
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
        let mut user_connections = self.connections.write().await;
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
    /// - `activate_grace_active` confirms it (grace window succeeded → `GraceActive -> Active`)
    /// - `expire_grace_active` expires it (grace window failed → `GraceActive -> Expired`)
    pub async fn mark_grace_active(&self, username: &str, token: &str) {
        let mut user_connections = self.connections.write().await;
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
        let kind = connection_data.sessions[session_index]
            .connection_kind
            .unwrap_or(ConnectionKind::Normal);
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
        let mut user_connections = self.connections.write().await;
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
            let mut user_connections = self.connections.write().await;
            let Some(connection_data) = user_connections.by_key.get_mut(username) else {
                return;
            };
            let Some(session_index) = connection_data
                .sessions
                .iter()
                .position(|session| session.token == token)
            else {
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
            connection_data.sessions[session_index].lifecycle = PlaybackLifecycle::Expired;
            connection_data.sessions[session_index].permission = UserConnectionPermission::Exhausted;
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
                if let Some(stream_idx) = connection_data
                    .streams
                    .iter()
                    .position(|stream| stream.addr == *addr && !stream.preserved)
                {
                    if let Some(kind) = connection_data.stream_kinds.remove(&connection_data.streams[stream_idx].uid) {
                        connection_data.decrement_kind(kind);
                    }
                    connection_data.stream_normal_priorities.remove(&connection_data.streams[stream_idx].uid);
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
        debug!(
            "GraceActive expired for session {token} in {username}, released {removed_count} streams"
        );
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
    pub async fn terminate_session(&self, username: &str, session_token: &str) {
        let (connection_changed, removed_count) = {
            let mut user_connections = self.connections.write().await;
            let Some(connection_data) = user_connections.by_key.get_mut(username) else {
                return;
            };

            let Some(session_index) = connection_data
                .sessions
                .iter()
                .position(|session| session.token == session_token)
            else {
                return;
            };

            // Collect all addresses associated with this session.
            let mut addrs = Vec::new();
            let session = &connection_data.sessions[session_index];
            if !session.addr.ip().is_unspecified() {
                addrs.push(session.addr);
            }
            for addr in &session.active_addrs {
                if *addr != session.addr && !addrs.contains(addr) {
                    addrs.push(*addr);
                }
            }

            // Release all streams for these addresses.
            let mut removed_count = 0;
            for addr in &addrs {
                if let Some(stream_idx) = connection_data
                    .streams
                    .iter()
                    .position(|stream| stream.addr == *addr && !stream.preserved)
                {
                    if let Some(kind) = connection_data.stream_kinds.remove(&connection_data.streams[stream_idx].uid) {
                        connection_data.decrement_kind(kind);
                    }
                    connection_data.stream_normal_priorities.remove(&connection_data.streams[stream_idx].uid);
                    connection_data.streams.swap_remove(stream_idx);
                    removed_count += 1;
                }
            }

            // Release counted lease if held.
            let mut connection_changed = false;
            if connection_data.sessions[session_index].lifecycle.is_counted() {
                let kind = connection_data.sessions[session_index]
                    .connection_kind
                    .unwrap_or(ConnectionKind::Normal);
                connection_data.decrement_kind(kind);
                connection_changed = true;
            }

            // Expire and remove the session immediately. Unlike `release_unbound_session_reservation`
            // which keeps the expired session for TTL-based GC cleanup, terminate_session explicitly
            // removes the session from the list so `get_and_update_user_session` returns None.
            connection_data.sessions.swap_remove(session_index);

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
        debug!(
            "Terminated session {session_token} for user {username}, released {removed_count} streams"
        );
    }

    /// Terminates all sessions associated with a given socket address for a user.
    ///
    /// This is used when a connection is explicitly kicked — the session should be
    /// expired and removed immediately rather than waiting for TTL-based GC cleanup.
    ///
    /// Removes all sessions whose `addr` or `active_addrs` contains `kick_addr`.
    pub async fn terminate_sessions_for_addr(&self, username: &str, kick_addr: &SocketAddr) {
        let (connection_changed, removed_count) = {
            let mut user_connections = self.connections.write().await;
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
                let Some(session_index) =
                    connection_data.sessions.iter().position(|s| s.token == *token)
                else {
                    continue;
                };

                // Release counted lease if held.
                if connection_data.sessions[session_index].lifecycle.is_counted() {
                    let kind = connection_data.sessions[session_index]
                        .connection_kind
                        .unwrap_or(ConnectionKind::Normal);
                    connection_data.decrement_kind(kind);
                    connection_changed = true;
                }

                // Release stream kinds for all streams belonging to this session.
                let session_streams: Vec<_> = connection_data
                    .streams
                    .iter()
                    .filter(|stream| stream.session_token.as_deref() == Some(token))
                    .map(|stream| stream.uid)
                    .collect();
                for uid in session_streams {
                    if let Some(kind) = connection_data.stream_kinds.remove(&uid) {
                        connection_data.decrement_kind(kind);
                        connection_changed = true;
                    }
                    connection_data.stream_normal_priorities.remove(&uid);
                    // Remove non-preserved streams.
                    if let Some(idx) =
                        connection_data.streams.iter().position(|s| s.uid == uid && !s.preserved)
                    {
                        connection_data.streams.swap_remove(idx);
                    }
                }

                // Expire and remove the session.
                connection_data.sessions.swap_remove(session_index);
                removed_count += 1;
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
        debug!(
            "Terminated {removed_count} sessions for user {username} at addr {kick_addr}"
        );
    }

    pub async fn expire_pending_provider(
        &self,
        username: &str,
        token: &str,
        expected_version: u64,
        wake_source: PendingProviderWakeSource,
    ) {
        let mut user_connections = self.connections.write().await;
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
            Some(
                connection_data.sessions[session_index]
                    .connection_kind
                    .unwrap_or(ConnectionKind::Normal),
            )
        } else {
            None
        };
        let session = &mut connection_data.sessions[session_index];
        Self::clear_session_pending_with_permission(session, UserConnectionPermission::Exhausted, wake_source);
        session.lifecycle = PlaybackLifecycle::Expired;
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
        for stream in connection_data
            .streams
            .iter()
            .filter(|stream| stream.session_token.as_deref() == Some(session_token))
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

    pub fn active_socket_ttl_secs(&self) -> u64 {
        let configured_ttl = self.adaptive_session_ttl_secs.load(Ordering::Relaxed);
        if configured_ttl == 0 { DEFAULT_ACTIVE_SOCKET_TTL_SECS } else { configured_ttl }
    }

    pub async fn socket_expiry_deadline(&self, addr: &SocketAddr) -> Option<u64> {
        let ttl_secs = self.active_socket_ttl_secs();
        self.connections.read().await.key_by_addr.get(addr).and_then(|registration| {
            if registration.username.is_empty() {
                None
            } else {
                Some(registration.ts.saturating_add(ttl_secs))
            }
        })
    }

    pub async fn touch_http_activity(&self, username: &str, token: &str, addr: &SocketAddr) {
        let now = current_time_secs();
        let mut user_connections = self.connections.write().await;

        let registration = user_connections
            .key_by_addr
            .entry(*addr)
            .or_insert_with(SocketRegistration::anonymous);
        registration.username = username.to_string();
        registration.ts = now;

        let Some(connection_data) = user_connections.by_key.get_mut(username) else {
            return;
        };

        connection_data.ts = now;

        let mut touched_session = false;
        for session in &mut connection_data.sessions {
            if session.token == token {
                session.ts = now;
                remember_session_addr(session, *addr);
                touched_session = true;
                break;
            }
        }

        if !touched_session {
            return;
        }

        for stream in &mut connection_data.streams {
            if stream.session_token.as_deref() == Some(token) {
                // Only update the addr to keep it current; do NOT reset stream.ts,
                // which represents the session start time shown as "Duration" in the dashboard.
                stream.addr = *addr;
            }
        }
    }

    pub async fn get_and_update_user_session(&self, username: &str, token: &str) -> Option<UserSession> {
        self.update_user_session(username, token).await
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

    async fn update_user_session(&self, username: &str, token: &str) -> Option<UserSession> {
        let mut user_connections = self.connections.write().await;

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
            connection_data.sessions[session_index].permission = admission.permission;
            if admission.kind.is_some() {
                connection_data.sessions[session_index].connection_kind = admission.kind;
            }
        }

        Some(connection_data.sessions[session_index].clone())
    }

    pub async fn active_streams(&self) -> Vec<StreamInfo> {
        self.gc();
        let user_connections = self.connections.read().await;
        let mut streams = Vec::new();
        for connection_data in user_connections.by_key.values() {
            for stream in &connection_data.streams {
                if !stream.preserved {
                    streams.push(stream.clone());
                }
            }
        }
        streams
    }

    fn log_connection_added(
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

    pub async fn is_user_blocked_for_stream(&self, username: &str, virtual_id: VirtualId) -> bool {
        let connections = self.connections.read().await;
        let now = current_time_secs();
        matches!(connections.kicked.get(username), Some((expires_at, vid)) if *vid == virtual_id && *expires_at > now)
    }

    pub async fn recently_evicted_session_protected_addr(&self, session_token: &str) -> Option<SocketAddr> {
        let connections = self.connections.read().await;
        let now = current_time_secs();
        let protection = connections.recently_evicted_sessions.get(session_token)?;
        if protection.expires_at > now {
            return Some(protection.protected_addr);
        }

        let username = connections.by_key.iter().find_map(|(username, connection_data)| {
            connection_data
                .sessions
                .iter()
                .any(|session| session.token == session_token)
                .then_some(username.as_str())
        })?;
        connections
            .key_by_addr
            .get(&protection.protected_addr)
            .filter(|registration| registration.username == username)
            .map(|_| protection.protected_addr)
    }

    pub async fn recent_socket_reentry_protected_addr(
        &self,
        username: &str,
        client_ip: &str,
        virtual_id: VirtualId,
    ) -> Option<SocketAddr> {
        let connections = self.connections.read().await;
        let now = current_time_secs();
        let key = create_socket_reentry_guard_key(username, client_ip, virtual_id);
        let protection = connections.recent_socket_reentry_guards.get(&key)?;
        if protection.expires_at > now {
            return Some(protection.protected_addr);
        }

        connections
            .key_by_addr
            .get(&protection.protected_addr)
            .filter(|registration| registration.username == username)
            .map(|_| protection.protected_addr)
    }

    pub async fn block_user_for_stream(&self, addr: &SocketAddr, virtual_id: VirtualId, blocked_secs: u64) {
        let block_for_secs = blocked_secs.clamp(0, 86_400); // max 1 day;
        if block_for_secs > 0 {
            let mut connections = self.connections.write().await;
            let now = current_time_secs();
            connections.kicked.retain(|_, (expires_at, _)| *expires_at > now);
            if let Some(username) = connections
                .key_by_addr
                .get(addr)
                .map(|registration| registration.username.clone())
                .filter(|username| !username.is_empty())
            {
                let expires_at = now + block_for_secs;
                connections.kicked.insert(username, (expires_at, virtual_id));
            }
        }
    }

    pub async fn mark_recent_eviction_guard_for_addr(
        &self,
        addr: &SocketAddr,
        protected_addr: SocketAddr,
        ttl_secs: u64,
    ) {
        if ttl_secs == 0 {
            return;
        }

        let mut connections = self.connections.write().await;
        let now = current_time_secs();
        connections
            .recently_evicted_sessions
            .retain(|_, protection| protection.expires_at > now);
        connections
            .recent_socket_reentry_guards
            .retain(|_, protection| protection.expires_at > now);

        let Some(username) = connections
            .key_by_addr
            .get(addr)
            .map(|registration| registration.username.clone())
            .filter(|username| !username.is_empty())
        else {
            return;
        };

        let Some(connection_data) = connections.by_key.get(&username) else {
            return;
        };

        let protection = RecentWinnerProtection {
            protected_addr,
            expires_at: now + ttl_secs,
        };
        let mut session_tokens = Vec::new();
        let mut socket_guard_keys = Vec::new();

        for stream in connection_data.streams.iter().filter(|stream| stream.addr == *addr) {
            if is_stable_session_stream(stream) {
                if let Some(session_token) = stream.session_token.clone() {
                    session_tokens.push(session_token);
                }
            } else {
                socket_guard_keys.push(create_socket_reentry_guard_key(
                    &username,
                    &stream.client_ip,
                    stream.channel.virtual_id,
                ));
            }
        }

        for session_token in session_tokens {
            connections
                .recently_evicted_sessions
                .insert(session_token, protection);
        }
        for key in socket_guard_keys {
            connections
                .recent_socket_reentry_guards
                .insert(key, protection);
        }
    }

    pub async fn get_username_for_addr(&self, addr: &SocketAddr) -> Option<String> {
        self.connections
            .read()
            .await
            .key_by_addr
            .get(addr)
            .map(|registration| registration.username.clone())
    }

    fn should_preserve_session_stream(stream: &StreamInfo) -> bool {
        stream.session_token.is_some() && is_stable_session_stream(stream)
    }

    fn is_preserved_stream_expired(
        &self,
        stream: &StreamInfo,
        sessions: &[UserSession],
        now: u64,
    ) -> bool {
        if !stream.preserved || !Self::should_preserve_session_stream(stream) {
            return false;
        }

        let ttl_secs = self.adaptive_session_ttl_secs.load(Ordering::Relaxed);
        let Some(session_token) = stream.session_token.as_deref() else {
            return true;
        };

        let Some(session) = sessions.iter().find(|session| session.token == session_token) else {
            return true;
        };

        now.saturating_sub(session.ts) >= ttl_secs
    }

    fn collect_divergence_snapshot(connection_data: &UserConnectionData, username: &str) -> DivergenceSnapshot {
        let connections = connection_data.connections;
        let counted_sessions = connection_data.sessions.iter().filter(|s| s.lifecycle.is_counted()).count();
        let streams_count = connection_data.streams.len();
        let mut kinds = Vec::new();

        for session in &connection_data.sessions {
            if !session.lifecycle.is_counted() {
                continue;
            }
            if matches!(session.lifecycle, PlaybackLifecycle::PendingProvider { ref data } if data.reason_code == PendingProviderReason::GraceHold) {
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
            let has_counted_session = connection_data.sessions.iter().any(|s| s.token == token && s.lifecycle.is_counted());
            if !has_counted_session {
                kinds.push(DivergenceKind::StreamWithoutCountedSession);
            }
        }

        #[allow(clippy::cast_possible_truncation)]
        let counted_sessions_u32 = counted_sessions as u32;
        if connections != counted_sessions_u32 {
            kinds.push(DivergenceKind::ConnectionCountMismatch {
                legacy: connections,
                counted: counted_sessions_u32,
            });
        }

        DivergenceSnapshot {
            username: username.to_string(),
            connections,
            counted_sessions,
            streams_count,
            kinds,
        }
    }

    async fn log_divergence_snapshot(&self, snapshot: DivergenceSnapshot) {
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
                    cache.push(
                        key,
                        DivergenceEntry {
                            last_logged: Instant::now(),
                            count_since_last_log: 0,
                        },
                    );
                    true
                }
            };

            if should_log {
                debug!(
                    "ADMISSION DIVERGENCE user={} kind={kind:?} connections={} counted_sessions={} streams={}",
                    snapshot.username,
                    snapshot.connections,
                    snapshot.counted_sessions,
                    snapshot.streams_count,
                );
            }
        }
    }

    async fn check_and_log_divergence_for_user(&self, username: &str) {
        let snapshot = {
            let connections = self.connections.read().await;
            let Some(data) = connections.by_key.get(username) else {
                return;
            };
            Self::collect_divergence_snapshot(data, username)
        };
        self.log_divergence_snapshot(snapshot).await;
    }

    async fn run_adaptive_expiry_worker(self: Arc<Self>) {
        loop {
            let next_expiry = {
                let queue = self.adaptive_expiry_queue.lock().await;
                queue.peek().map(|entry| entry.0.expires_at)
            };

            match next_expiry {
                None => {
                    tokio::select! {
                        () = self.adaptive_expiry_notify.notified() => {}
                        () = self.adaptive_expiry_cancel.cancelled() => break,
                    }
                }
                Some(expires_at) => {
                    let now = current_time_secs();
                    if expires_at <= now {
                        self.process_due_adaptive_expiry_entries(now).await;
                        continue;
                    }

                    tokio::select! {
                        () = tokio::time::sleep(Duration::from_secs(expires_at.saturating_sub(now))) => {}
                        () = self.adaptive_expiry_notify.notified() => {}
                        () = self.adaptive_expiry_cancel.cancelled() => break,
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn process_due_adaptive_expiry_entries(&self, now: u64) {
        let mut due_entries = Vec::new();
        {
            let mut queue = self.adaptive_expiry_queue.lock().await;
            while let Some(entry) = queue.peek() {
                if entry.0.expires_at > now {
                    break;
                }
                if let Some(Reverse(entry)) = queue.pop() {
                    due_entries.push(entry);
                }
            }
        }

        if due_entries.is_empty() {
            return;
        }

        let usernames_to_check: std::collections::HashSet<_> = due_entries.iter().map(|e| &e.username).collect();

        let mut removed_addrs: Vec<std::net::SocketAddr> = Vec::new();
        let mut cleanup_events: Vec<(std::net::SocketAddr, Box<StreamInfo>)> = Vec::new();
        let mut replacement_entries: Vec<AdaptiveExpiryEntry> = Vec::new();
        let mut promotions: Vec<(String, PromotionAction)> = Vec::new();
        {
            let mut expiry_index = self.adaptive_expiry_index.lock().await;
            let mut user_connections = self.connections.write().await;
            for entry in &due_entries {
                let key = AdaptiveExpiryKey {
                    username: entry.username.clone(),
                    session_token: entry.session_token.clone(),
                    uid: entry.uid,
                };
                let Some(current_expires_at) = expiry_index.get(&key).copied() else {
                    continue;
                };
                if current_expires_at != entry.expires_at {
                    continue;
                }

                let mut remove_user = false;
                if let Some(connection_data) = user_connections.by_key.get_mut(&entry.username) {
                    let stream_idx_opt = connection_data
                        .streams
                        .iter()
                        .position(|stream| {
                            stream.uid == entry.uid
                                && stream.preserved
                                && stream.session_token.as_deref() == Some(entry.session_token.as_str())
                        });

                    if let Some(stream_idx) = stream_idx_opt {
                        let should_remove = self.is_preserved_stream_expired(
                            &connection_data.streams[stream_idx],
                            &connection_data.sessions,
                            now,
                        );

                        if should_remove {
                            let addr = connection_data.streams[stream_idx].addr;
                            let session_token = connection_data.streams[stream_idx].session_token.clone();
                            if self.cleanup_tx.get().is_some() {
                                cleanup_events.push((addr, Box::new(connection_data.streams[stream_idx].clone())));
                            } else {
                                removed_addrs.push(addr);
                            }
                            let removed_stream = connection_data.streams.swap_remove(stream_idx);
                            if let Some(kind) = connection_data.stream_kinds.remove(&removed_stream.uid) {
                                connection_data.decrement_kind(kind);
                            }
                            connection_data.stream_normal_priorities.remove(&removed_stream.uid);
                            if let Some(action) = connection_data.try_promote_soft_stream() {
                                let promoted_stream =
                                    connection_data.streams.iter().find(|stream| stream.uid == action.uid).cloned();
                                if let Some(stream) = promoted_stream.as_ref() {
                                    Self::promote_session_for_stream(connection_data, stream);
                                }
                                promotions.push((entry.username.clone(), action));
                            }
                            if let Some(session_token) = session_token.as_deref() {
                                Self::clear_session_counted_without_stream(connection_data, session_token);
                            }
                            while connection_data.try_promote_soft_session_reservation() {}
                            expiry_index.remove(&key);
                        } else if let Some(replacement_entry) = self.build_preserved_stream_expiry(
                            &entry.username,
                            &connection_data.streams[stream_idx],
                            &connection_data.sessions,
                        ) {
                            if replacement_entry.expires_at != current_expires_at {
                                replacement_entries.push(replacement_entry);
                            }
                        }
                    } else {
                        expiry_index.remove(&key);
                    }

                    remove_user = connection_data.connections == 0
                        && connection_data.streams.is_empty()
                        && connection_data.sessions.is_empty();
                } else {
                    expiry_index.remove(&key);
                }

                if remove_user {
                    user_connections.by_key.remove(&entry.username);
                }
            }
        } // locks released here

        // divergence check after adaptive expiry processing
        for username in usernames_to_check {
            let snapshot = {
                let connections = self.connections.read().await;
                connections
                    .by_key
                    .get(username)
                    .map(|data| Self::collect_divergence_snapshot(data, username))
            };
            if let Some(snapshot) = snapshot {
                self.log_divergence_snapshot(snapshot).await;
            }
        }

        if let Some(tx) = self.cleanup_tx.get() {
            for (addr, stream_info) in cleanup_events {
                if tx.try_send(CleanupEvent::AdaptiveSessionExpired { stream_info }).is_err() {
                    self.dropped_cleanup_events.fetch_add(1, Ordering::Relaxed);
                    debug!("Cleanup channel unavailable, dropping adaptive session expiry");
                    removed_addrs.push(addr);
                }
            }
        }

        for entry in replacement_entries {
            self.enqueue_adaptive_expiry(entry).await;
        }

        for (username, action) in promotions {
            self.emit_promotion_update(&username, action).await;
        }

        let had_removals = !removed_addrs.is_empty();
        for addr in removed_addrs {
            self.event_manager
                .send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Disconnected(addr)));
        }
        if had_removals {
            self.log_active_user().await;
        }
    }

    fn gc(&self) {
        if let Some(gc_ts) = &self.gc_ts {
            let ts = gc_ts.load(Ordering::Acquire);
            let now = current_time_secs();

            if now.saturating_sub(ts) > USER_GC_TTL
                && gc_ts.compare_exchange(ts, now, Ordering::AcqRel, Ordering::Relaxed).is_ok()
            {
                if let Ok(mut user_connections) = self.connections.try_write() {
                    user_connections.kicked.retain(|_, (expires_at, _)| *expires_at > now);
                    user_connections
                        .recently_evicted_sessions
                        .retain(|_, protection| protection.expires_at > now);
                    user_connections
                        .recent_socket_reentry_guards
                        .retain(|_, protection| protection.expires_at > now);
                    for connection_data in user_connections.by_key.values_mut() {
                        Self::release_expired_session_reservations(connection_data, now);
                        connection_data.sessions.retain(|s| now.saturating_sub(s.ts) < USER_CON_TTL);
                    }
                    user_connections.by_key.retain(|_k, v| {
                        v.connections > 0 || !v.streams.is_empty() || now.saturating_sub(v.ts) < USER_CON_TTL
                    });
                    user_connections.key_by_addr.retain(|_, registration| {
                        !(registration.username.is_empty() && now.saturating_sub(registration.ts) >= ANON_SOCKET_TTL)
                    });
                } else {
                    // Lock contention: release the GC claim so a subsequent caller can retry immediately.
                    let _ = gc_ts.compare_exchange(now, ts, Ordering::AcqRel, Ordering::Relaxed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::model::EventManager, auth::Fingerprint, model::{Config, ProxyUserCredentials}};
    use arc_swap::ArcSwapOption;
    use shared::{
        model::{PlaylistItemType, StreamChannel, StreamInfo, XtreamCluster},
        utils::Internable,
    };
    use std::{borrow::Cow, sync::Arc};
    use shared::model::ProxyType;

    fn test_channel(virtual_id: u32) -> StreamChannel {
        StreamChannel {
            target_id: 1,
            virtual_id,
            provider_id: 1,
            input_name: "input".intern(),
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            group: "group".intern(),
            title: "title".intern(),
            url: "http://localhost/stream.ts".intern(),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: None,
            epg_channel_id: None,
        }
    }

    fn test_adaptive_channel(virtual_id: u32) -> StreamChannel {
        StreamChannel {
            target_id: 1,
            virtual_id,
            provider_id: 1,
            input_name: "input".intern(),
            item_type: PlaylistItemType::LiveHls,
            cluster: XtreamCluster::Live,
            group: "group".intern(),
            title: "title".intern(),
            url: "http://localhost/stream.ts".intern(),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: None,
            epg_channel_id: None,
        }
    }

    /// Session refresh normalizes Expired -> Prepared.
    /// When a new request arrives on an expired session, the lifecycle should be
    /// reset to Prepared so that full activation evaluation happens.
    #[tokio::test]
    async fn create_user_session_normalizes_expired_lifecycle() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55400".parse().unwrap();
        let mut user = ProxyUserCredentials::default();
        user.username = "user-lifecycle-refresh".to_string();

        // Create a session in Expired state directly via session manipulation
        {
            let mut connections = manager.connections.write().await;
            let data = connections.by_key.entry(user.username.clone())
                .or_insert_with(|| UserConnectionData::new(0, 1, 0));
            data.add_session(UserSession {
                token: "tok-refresh-expired".to_string(),
                transition_version: 1,
                virtual_id: 7001,
                provider: "provider-a".intern(),
                stream_url: "http://localhost/live.m3u8".intern(),
                addr,
                socket_bound: false,
                active_addrs: vec![addr],
                ts: current_time_secs(),
                started_at: current_time_secs(),
                permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                lifecycle: PlaybackLifecycle::Expired,
            });
        }

        // Refresh the session via create_user_session
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-refresh-expired",
                virtual_id: 7001,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        let sessions = manager.connections.read().await;
        let data = sessions.by_key.get(&user.username).expect("user data should exist");
        let session = data.sessions.iter().find(|s| s.token == "tok-refresh-expired").expect("session");
        assert_eq!(
            session.lifecycle,
            PlaybackLifecycle::Prepared,
            "Expired session should normalize to Prepared on refresh"
        );
    }

    /// Session refresh does NOT normalize `PendingProvider`.
    /// A `PendingProvider` session must not be reset — pending state must continue
    /// until explicitly resolved via `activate_pending_provider` or `expire_pending_provider`.
    #[tokio::test]
    async fn create_user_session_does_not_normalize_pending_provider_lifecycle() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55401".parse().unwrap();
        let mut user = ProxyUserCredentials::default();
        user.username = "user-pending-lifecycle".to_string();

        // Create a session in PendingProvider state
        {
            let mut connections = manager.connections.write().await;
            let data = connections.by_key.entry(user.username.clone())
                .or_insert_with(|| UserConnectionData::new(0, 1, 0));
            data.add_session(UserSession {
                token: "tok-refresh-pending".to_string(),
                transition_version: 1,
                virtual_id: 7002,
                provider: "provider-a".intern(),
                stream_url: "http://localhost/live.m3u8".intern(),
                addr,
                socket_bound: false,
                active_addrs: vec![addr],
                ts: current_time_secs(),
                started_at: current_time_secs(),
                permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                lifecycle: PlaybackLifecycle::PendingProvider {
                    data: PendingProviderState {
                        reason_code: PendingProviderReason::GraceHold,
                        created_at: current_time_secs(),
                        deadline: current_time_secs() + 30,
                        version: 1,
                        wake_source: None,
                    }
                },
            });
        }

        // Refresh the session via create_user_session
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-refresh-pending",
                virtual_id: 7002,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        let sessions = manager.connections.read().await;
        let data = sessions.by_key.get(&user.username).expect("user data should exist");
        let session = data.sessions.iter().find(|s| s.token == "tok-refresh-pending").expect("session");
        assert!(
            matches!(session.lifecycle, PlaybackLifecycle::PendingProvider { .. }),
            "PendingProvider session should NOT be normalized on refresh - pending wait must continue"
        );
    }

    /// `terminate_session` expires a session and removes it.
    #[tokio::test]
    async fn terminate_session_expires_and_removes_session() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55410".parse().unwrap();
        let mut user = ProxyUserCredentials::default();
        user.username = "user-terminate".to_string();
        user.max_connections = 2;

        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-terminate-test",
                virtual_id: 8001,
                provider: "provider-terminate",
                stream_url: "http://localhost/test.ts",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        // Verify session exists.
        let before = manager
            .get_and_update_user_session(&user.username, &token)
            .await;
        assert!(before.is_some(), "session should exist before terminate");
        assert_eq!(before.as_ref().unwrap().lifecycle, PlaybackLifecycle::Prepared);

        // Terminate the session.
        manager.terminate_session(&user.username, &token).await;

        // Session should be gone.
        let after = manager
            .get_and_update_user_session(&user.username, &token)
            .await;
        assert!(after.is_none(), "session should be removed after terminate");
    }

    /// `terminate_session` releases counted lease.
    #[tokio::test]
    async fn terminate_session_releases_counted_lease() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55411".parse().unwrap();
        let mut user = ProxyUserCredentials::default();
        user.username = "user-terminate-counted".to_string();
        user.max_connections = 2;

        let token = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-terminate-counted",
                virtual_id: 8002,
                provider: "provider-terminate-counted",
                stream_url: "http://localhost/test.ts",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        // Mark the session as counted and active (simulating post-admission state).
        {
            let mut connections = manager.connections.write().await;
            let data = connections.by_key.get_mut(&user.username).unwrap();
            let session = data.sessions.iter_mut().find(|s| s.token == token).unwrap();
            // Simulate counted state by setting lifecycle to Active.
            session.lifecycle = PlaybackLifecycle::Active;
            data.increment_kind(ConnectionKind::Normal);
        }

        // Verify counted before terminate.
        {
            let before = manager
                .get_and_update_user_session(&user.username, &token)
                .await
                .unwrap();
            assert!(before.lifecycle.is_counted(), "session should be counted before terminate");
        }

        // Terminate.
        manager.terminate_session(&user.username, &token).await;

        // Session should be gone.
        let after = manager
            .get_and_update_user_session(&user.username, &token)
            .await;
        assert!(after.is_none(), "session should be removed after terminate");
    }

    #[tokio::test]
    async fn test_multi_session_same_addr_counts_and_releases_individually() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55001".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-key".to_string(), "127.0.0.1".to_string(), addr);
        let username = "user1";

        manager.add_connection(&addr).await;

        let first = manager
            .update_connection(ActiveUserConnectionParams {
                uid: 1,
                meter_uid: 0,
                username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(1001),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-1"),
            })
            .await;
        assert!(first.is_some());
        assert_eq!(manager.user_connections(username).await, 1);
        assert_eq!(
            manager.connection_permission(username, 1, 0).await,
            UserConnectionPermission::Exhausted
        );

        let second = manager
            .update_connection(ActiveUserConnectionParams {
                uid: 2,
                meter_uid: 0,
                username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-b".intern(),
                stream_channel: &test_channel(1002),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-2"),
            })
            .await;
        assert!(second.is_some());
        assert_eq!(manager.user_connections(username).await, 2);

        assert!(manager.release_stream(&addr).await.is_some());
        assert_eq!(manager.user_connections(username).await, 1);

        assert!(manager.release_stream(&addr).await.is_some());
        assert_eq!(manager.user_connections(username).await, 0);
    }

    #[tokio::test]
    async fn mark_pending_provider_tracks_metadata_on_session() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55021".parse().unwrap_or_else(|_| unreachable!());
        let mut user = ProxyUserCredentials::default();
        user.username = "pending-user".to_string();

        let _ = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-pending",
                virtual_id: 1001,
                provider: "provider-a",
                stream_url: "http://provider/live/1001.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        let _ = manager
            .mark_pending_provider(&user.username, "tok-pending", PendingProviderReason::GraceHold, 12_345)
            .await;

        let session = manager
            .get_and_update_user_session(&user.username, "tok-pending")
            .await
            .expect("session should exist");
        let PlaybackLifecycle::PendingProvider { data: pending } = &session.lifecycle else {
            panic!("pending provider should be tracked")
        };
        assert!(matches!(pending.reason_code, PendingProviderReason::GraceHold));
        assert_eq!(pending.deadline, 12_345);
        assert!(pending.created_at > 0);
        assert_eq!(pending.version, 1);
        assert!(pending.wake_source.is_none());
    }

    #[tokio::test]
    async fn activate_pending_provider_clears_pending_metadata() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55022".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = Fingerprint::new("fp-pending".to_string(), "127.0.0.1".to_string(), addr);
        let mut user = ProxyUserCredentials::default();
        user.username = "pending-activate".to_string();

        let _ = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-pending-activate",
                virtual_id: 1002,
                provider: "provider-a",
                stream_url: "http://provider/live/1002.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        let _ = manager
            .mark_pending_provider(
                &user.username,
                "tok-pending-activate",
                PendingProviderReason::GraceHold,
                current_time_secs().saturating_add(30),
            )
            .await;

        let _ = manager
            .update_connection(ActiveUserConnectionParams {
                uid: 12,
                meter_uid: 0,
                username: &user.username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_adaptive_channel(1002),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-pending-activate"),
            })
            .await;
        manager
            .activate_pending_provider(
                &user.username,
                "tok-pending-activate",
                1,
                PendingProviderWakeSource::Activated,
            )
            .await;

        let session = manager
            .get_and_update_user_session(&user.username, "tok-pending-activate")
            .await
            .expect("session should exist");
        assert!(session.lifecycle.is_counted());
        assert!(!matches!(session.lifecycle, PlaybackLifecycle::PendingProvider { .. }), "explicit pending resolution must clear pending provider state");
    }

    #[tokio::test]
    async fn activate_pending_provider_ignores_stale_version_after_replacement() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55023".parse().unwrap_or_else(|_| unreachable!());
        let mut user = ProxyUserCredentials::default();
        user.username = "pending-stale".to_string();

        let _ = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-pending-stale",
                virtual_id: 1003,
                provider: "provider-a",
                stream_url: "http://provider/live/1003.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        let first_version = manager
            .mark_pending_provider(
                &user.username,
                "tok-pending-stale",
                PendingProviderReason::GraceHold,
                5_000,
            )
            .await
            .expect("first pending version should be created");
        let second_version = manager
            .mark_pending_provider(
                &user.username,
                "tok-pending-stale",
                PendingProviderReason::GraceHold,
                6_000,
            )
            .await
            .expect("second pending version should replace the first");
        assert!(second_version > first_version);

        manager
            .activate_pending_provider(
                &user.username,
                "tok-pending-stale",
                first_version,
                PendingProviderWakeSource::CapacityNotify,
            )
            .await;

        let session = manager
            .get_and_update_user_session(&user.username, "tok-pending-stale")
            .await
            .expect("session should still exist");
        let PlaybackLifecycle::PendingProvider { data: pending_data } = &session.lifecycle else {
            panic!("session should still be in PendingProvider after stale wakeup")
        };
        assert_eq!(pending_data.version, second_version);
        assert!(pending_data.wake_source.is_none());
        assert_eq!(session.permission, UserConnectionPermission::GracePeriod);
    }

    #[tokio::test]
    async fn expire_pending_provider_marks_session_exhausted() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55024".parse().unwrap_or_else(|_| unreachable!());
        let mut user = ProxyUserCredentials::default();
        user.username = "pending-expire".to_string();

        let _ = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-pending-expire",
                virtual_id: 1004,
                provider: "provider-a",
                stream_url: "http://provider/live/1004.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        let version = manager
            .mark_pending_provider(
                &user.username,
                "tok-pending-expire",
                PendingProviderReason::GraceHold,
                6_000,
            )
            .await
            .expect("pending version should be created");

        manager
            .expire_pending_provider(
                &user.username,
                "tok-pending-expire",
                version,
                PendingProviderWakeSource::Timeout,
            )
            .await;

        let session = manager
            .get_and_update_user_session(&user.username, "tok-pending-expire")
            .await
            .expect("session should still exist");
        assert_eq!(session.permission, UserConnectionPermission::Exhausted);
        assert!(!matches!(session.lifecycle, PlaybackLifecycle::PendingProvider { .. }));
        assert!(!session.lifecycle.is_counted());
    }

    #[tokio::test]
    async fn expire_pending_provider_releases_counted_slot_for_pending_session() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55025".parse().unwrap_or_else(|_| unreachable!());
        let mut user = ProxyUserCredentials::default();
        user.username = "pending-expire-counted".to_string();
        user.max_connections = 1;

        let _ = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-pending-expire-counted",
                virtual_id: 1005,
                provider: "provider-a",
                stream_url: "http://provider/live/1005.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        {
            let mut connections = manager.connections.write().await;
            let connection_data = connections
                .by_key
                .get_mut(&user.username)
                .expect("session should have created connection data");
            connection_data.increment_kind(ConnectionKind::Normal);
            let session = connection_data
                .sessions
                .iter_mut()
                .find(|session| session.token == "tok-pending-expire-counted")
                .expect("session should exist");
            // Simulate a previously-counted session transitioning to PendingProvider.
            // Set lifecycle to Active (is_counted() = true). The kind count is already
            // incremented above via connection_data.increment_kind().
            session.lifecycle = PlaybackLifecycle::Active;
        }

        assert_eq!(manager.user_connections(&user.username).await, 1);

        let version = manager
            .mark_pending_provider(
                &user.username,
                "tok-pending-expire-counted",
                PendingProviderReason::GraceHold,
                6_500,
            )
            .await
            .expect("pending version should be created");

        manager
            .expire_pending_provider(
                &user.username,
                "tok-pending-expire-counted",
                version,
                PendingProviderWakeSource::Timeout,
            )
            .await;

        let session = manager
            .get_and_update_user_session(&user.username, "tok-pending-expire-counted")
            .await
            .expect("session should still exist");
        assert_eq!(session.permission, UserConnectionPermission::Exhausted);
        assert!(!matches!(session.lifecycle, PlaybackLifecycle::PendingProvider { .. }));
        assert!(!session.lifecycle.is_counted());
        assert_eq!(manager.user_connections(&user.username).await, 0);
    }

    /// `terminate_sessions_for_addr` expires all sessions at a given addr and releases counted leases.
    #[tokio::test]
    async fn terminate_sessions_for_addr_expires_all_sessions_at_addr() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr_kick: SocketAddr = "127.0.0.1:55420".parse().unwrap();
        let addr_keep: SocketAddr = "127.0.0.1:55421".parse().unwrap();
        let mut user = ProxyUserCredentials::default();
        user.username = "user-kick-addr".to_string();
        user.max_connections = 4;

        // Create session at kicked addr.
        let tok_kick = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-kick",
                virtual_id: 1,
                provider: "provider-a",
                stream_url: "http://provider/live/1.m3u8",
                addr: &addr_kick,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        // Create session at kept addr.
        let tok_keep = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-keep",
                virtual_id: 2,
                provider: "provider-b",
                stream_url: "http://provider/live/2.m3u8",
                addr: &addr_keep,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        // Mark both sessions as counted and active.
        {
            let mut connections = manager.connections.write().await;
            let data = connections.by_key.get_mut(&user.username).unwrap();
            for session in &mut data.sessions {
                // Simulate counted state by setting lifecycle to Active.
                session.lifecycle = PlaybackLifecycle::Active;
            }
            data.increment_kind(ConnectionKind::Normal);
            data.increment_kind(ConnectionKind::Normal);
        }

        assert_eq!(manager.user_connections(&user.username).await, 2);

        // Kick the addr — should terminate only the sessions at that addr.
        manager.terminate_sessions_for_addr(&user.username, &addr_kick).await;

        // Session at kicked addr should be gone.
        assert!(
            manager.get_and_update_user_session(&user.username, &tok_kick).await.is_none(),
            "kicked session should be removed"
        );

        // Session at kept addr should remain.
        let kept = manager
            .get_and_update_user_session(&user.username, &tok_keep)
            .await
            .expect("kept session should still exist");
        assert_eq!(kept.token, tok_keep);
        assert_eq!(kept.addr, addr_keep);

        // Connection count should drop by 1.
        assert_eq!(manager.user_connections(&user.username).await, 1);
    }

    #[tokio::test]
    async fn test_grant_grace_succeeds_at_and_above_limit_without_prior_grace() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let at_limit_addr: SocketAddr = "127.0.0.1:55011".parse().unwrap();
        let at_limit_fingerprint = Fingerprint::new("fp-limit".to_string(), "127.0.0.1".to_string(), at_limit_addr);
        let over_limit_addr: SocketAddr = "127.0.0.1:55012".parse().unwrap();
        let over_limit_fingerprint = Fingerprint::new("fp-over".to_string(), "127.0.0.1".to_string(), over_limit_addr);

        manager.add_connection(&at_limit_addr).await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 10,
                meter_uid: 0,
                username: "at-limit",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &at_limit_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(1010),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-limit"),
            })
            .await;

        assert!(manager.grant_grace("at-limit").await);

        manager.add_connection(&over_limit_addr).await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 11,
                meter_uid: 0,
                username: "over-limit",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &over_limit_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(1011),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-over-1"),
            })
            .await;
        manager.add_connection(&"127.0.0.1:55013".parse().unwrap()).await;
        let second_fingerprint =
            Fingerprint::new("fp-over-2".to_string(), "127.0.0.1".to_string(), "127.0.0.1:55013".parse().unwrap());
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 12,
                meter_uid: 0,
                username: "over-limit",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &second_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(1012),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-over-2"),
            })
            .await;

        assert!(manager.grant_grace("over-limit").await);
    }

    fn test_user_credentials(username: &str, max_connections: u32, soft_connections: u16) -> ProxyUserCredentials {
        ProxyUserCredentials {
            username: username.to_string(),
            password: "test".to_string(),
            token: None,
            proxy: ProxyType::default(),
            server: None,
            epg_timeshift: None,
            epg_request_timeshift: None,
            created_at: None,
            exp_date: None,
            max_connections,
            status: None,
            output_clusters: shared::model::ClusterFlags::all(),
            ui_enabled: true,
            comment: None,
            priority: 0,
            soft_connections,
            soft_priority: 0,
            t_is_api_user: false,
        }
    }

    #[tokio::test]
    async fn eviction_candidates_ignore_ambiguous_socket_addrs() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let shared_addr: SocketAddr = "127.0.0.1:55031".parse().unwrap();
        let unique_addr: SocketAddr = "127.0.0.1:55032".parse().unwrap();
        let shared_fp = Fingerprint::new("fp-shared".to_string(), "127.0.0.1".to_string(), shared_addr);
        let unique_fp = Fingerprint::new("fp-unique".to_string(), "127.0.0.1".to_string(), unique_addr);

        manager.add_connection(&shared_addr).await;
        manager.add_connection(&unique_addr).await;

        // Create sessions first so update_connection can mark them as counted.
        let user = test_user_credentials("same-user", 3, 0);
        for (token, addr, channel_id) in [
            ("tok-31", shared_addr, 1031u32),
            ("tok-32", shared_addr, 1032),
            ("tok-33", unique_addr, 1033),
        ] {
            manager.create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: token,
                virtual_id: channel_id,
                provider: "provider-a",
                stream_url: "",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            }).await;
        }

        // update_connection marks the session as counted.
        for (uid, token, fp, channel_id) in [(31, "tok-31", &shared_fp, 1031u32), (32, "tok-32", &shared_fp, 1032)] {
            manager
                .update_connection(ActiveUserConnectionParams {
                    uid,
                    meter_uid: 0,
                    username: "same-user",
                    max_connections: 3,
                    soft_connections: 0,
                    connection_kind: ConnectionKind::Normal,
                    priority: 0,
                    soft_priority: 0,
                    fingerprint: fp,
                    provider: "provider-a".intern(),
                    stream_channel: &test_channel(channel_id),
                    user_agent: Cow::Borrowed("ua"),
                    session_token: Some(token),
                })
                .await;
        }

        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 33,
                meter_uid: 0,
                username: "same-user",
                max_connections: 3,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &unique_fp,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(1033),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-33"),
            })
            .await;

        let candidates = manager.get_eviction_candidates("same-user", "127.0.0.1").await;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].addr, unique_addr);
    }

    #[tokio::test]
    async fn eviction_candidates_include_other_ips_for_user_wide_rules() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let first_addr: SocketAddr = "127.0.0.1:55041".parse().unwrap();
        let second_addr: SocketAddr = "127.0.0.1:55042".parse().unwrap();
        let first_fp = Fingerprint::new("fp-user-wide-1".to_string(), "10.0.0.1".to_string(), first_addr);
        let second_fp = Fingerprint::new("fp-user-wide-2".to_string(), "10.0.0.2".to_string(), second_addr);

        manager.add_connection(&first_addr).await;
        manager.add_connection(&second_addr).await;

        let user = test_user_credentials("same-user", 2, 0);
        for (token, addr, channel_id) in [
            ("tok-41", first_addr, 1041u32),
            ("tok-42", second_addr, 1042),
        ] {
            manager.create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: token,
                virtual_id: channel_id,
                provider: "provider-a",
                stream_url: "",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            }).await;
        }

        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 41,
                meter_uid: 0,
                username: "same-user",
                max_connections: 2,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &first_fp,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(1041),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-41"),
            })
            .await;

        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 42,
                meter_uid: 0,
                username: "same-user",
                max_connections: 2,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &second_fp,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(1042),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-42"),
            })
            .await;

        let candidates = manager.get_eviction_candidates("same-user", "10.0.0.1").await;
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|candidate| candidate.addr == first_addr));
        assert!(candidates.iter().any(|candidate| candidate.addr == second_addr));
    }

    #[tokio::test]
    async fn eviction_candidates_include_preserved_adaptive_streams() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55043".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-preserved".to_string(), "127.0.0.1".to_string(), addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("same-user");
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-preserved",
                virtual_id: 1043,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 43,
                meter_uid: 0,
                username: "same-user",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveHls,
                    ..test_channel(1043)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-preserved"),
            })
            .await;

        let released = manager.release_connection(&addr).await;
        assert!(released.addr_removed);
        assert!(released.removed_streams.is_empty(), "adaptive stream should stay logically active");

        let candidates = manager.get_eviction_candidates("same-user", "127.0.0.1").await;
        assert_eq!(candidates.len(), 1, "preserved adaptive streams must remain evictable");
        assert_eq!(candidates[0].addr, addr);
    }

    #[tokio::test]
    async fn test_kicked_release_does_not_preserve_adaptive_stream() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55014".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-adaptive".to_string(), "127.0.0.1".to_string(), addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user-adaptive");
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-adaptive",
                virtual_id: 2014,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 14,
                meter_uid: 0,
                username: &user.username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_adaptive_channel(2014),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-adaptive"),
            })
            .await;

        let removed = manager.release_connection_as_kicked(&addr).await;
        assert!(removed.addr_removed);
        assert_eq!(removed.removed_streams.len(), 1);
        assert!(manager.active_streams().await.is_empty());
    }

    #[tokio::test]
    async fn kicked_release_removes_preserved_adaptive_stream_without_socket_registration() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55017".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-preserved-kick".to_string(), "127.0.0.1".to_string(), addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user-preserved-kick");
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-preserved-kick",
                virtual_id: 2017,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 17,
                meter_uid: 0,
                username: &user.username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_adaptive_channel(2017),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-preserved-kick"),
            })
            .await;

        let released = manager.release_connection(&addr).await;
        assert!(released.addr_removed);
        assert!(released.removed_streams.is_empty());
        assert!(manager.active_streams().await.is_empty());

        let kicked = manager.release_connection_as_kicked(&addr).await;
        assert!(kicked.addr_removed);
        assert_eq!(kicked.removed_streams.len(), 1);
        assert!(manager.active_streams().await.is_empty());
    }

    #[tokio::test]
    async fn test_kicked_release_invalidates_removed_session_tokens() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let kicked_addr: SocketAddr = "127.0.0.1:55015".parse().unwrap();
        let survivor_addr: SocketAddr = "127.0.0.1:55016".parse().unwrap();
        let kicked_fingerprint = Fingerprint::new("fp-kicked".to_string(), "127.0.0.1".to_string(), kicked_addr);
        let survivor_fingerprint = Fingerprint::new("fp-survivor".to_string(), "127.0.0.1".to_string(), survivor_addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("kicked-user");
        user.max_connections = 1;

        manager.add_connection(&kicked_addr).await;
        manager.add_connection(&survivor_addr).await;

        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-kicked",
                virtual_id: 2015,
                provider: "provider-a",
                stream_url: "http://localhost/live-1.ts",
                addr: &kicked_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: true,
            })
            .await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-survivor",
                virtual_id: 2016,
                provider: "provider-a",
                stream_url: "http://localhost/live-2.ts",
                addr: &survivor_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: true,
            })
            .await;

        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 15,
                meter_uid: 0,
                username: &user.username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &kicked_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(2015),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-kicked"),
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 16,
                meter_uid: 0,
                username: &user.username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &survivor_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(2016),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-survivor"),
            })
            .await;

        let removed = manager.release_connection_as_kicked(&kicked_addr).await;
        assert!(removed.addr_removed);
        assert_eq!(removed.removed_streams.len(), 1);
        assert_eq!(
            manager
                .connection_admission_for_session(&user.username, 1, 0, "tok-kicked")
                .await
                .permission,
            UserConnectionPermission::Exhausted
        );
        assert_eq!(
            manager
                .connection_admission_for_session(&user.username, 1, 0, "tok-survivor")
                .await
                .permission,
            UserConnectionPermission::Allowed
        );
    }

    #[tokio::test]
    async fn test_grace_at_limit_remains_active_until_connections_drop_below_limit() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55017".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-grace".to_string(), "127.0.0.1".to_string(), addr);

        manager.add_connection(&addr).await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 17,
                meter_uid: 0,
                username: "grace-at-limit",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(2017),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-grace"),
            })
            .await;

        assert!(manager.grant_grace("grace-at-limit").await);
        assert_eq!(
            manager.connection_admission("grace-at-limit", 1, 0).await.permission,
            UserConnectionPermission::Exhausted
        );
        assert!(!manager.grant_grace("grace-at-limit").await);
    }

    #[tokio::test]
    async fn test_same_session_token_on_new_addr_reuses_logical_connection() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let first_addr: SocketAddr = "127.0.0.1:55021".parse().unwrap();
        let second_addr: SocketAddr = "127.0.0.1:55022".parse().unwrap();
        let first = Fingerprint::new("fp-key-1".to_string(), "127.0.0.1".to_string(), first_addr);
        let second = Fingerprint::new("fp-key-2".to_string(), "127.0.0.1".to_string(), second_addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;

        manager.add_connection(&first_addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-hls",
                virtual_id: 2001,
                provider: "provider-a",
                stream_url: "http://localhost/live.ts",
                addr: &first_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: true,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 0,
                meter_uid: 0,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &first,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(2001),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-hls"),
            })
            .await;

        assert_eq!(
            manager.connection_permission_for_session("user1", 1, 0, "tok-hls").await,
            UserConnectionPermission::Allowed
        );

        manager.add_connection(&second_addr).await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 0,
                meter_uid: 0,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &second,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(2001),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-hls"),
            })
            .await;

        assert_eq!(manager.user_connections("user1").await, 1);

        let streams = manager.active_streams().await;
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].addr, second_addr);
        assert_eq!(streams[0].session_token.as_deref(), Some("tok-hls"));
    }

    #[tokio::test]
    async fn adaptive_session_stream_cleanup_addrs_excludes_manifest_addr_and_current_addr() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let manifest_addr: SocketAddr = "127.0.0.1:55091".parse().unwrap();
        let first_segment_addr: SocketAddr = "10.41.41.89:55092".parse().unwrap();
        let next_segment_addr: SocketAddr = "10.41.41.89:55093".parse().unwrap();
        let first_segment = Fingerprint::new("fp-segment-1".to_string(), "10.41.41.89".to_string(), first_segment_addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;

        manager.add_connection(&manifest_addr).await;
        manager.add_connection(&first_segment_addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-hls-cleanup",
                virtual_id: 2002,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &manifest_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 0,
                meter_uid: 0,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &first_segment,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveHls,
                    ..test_channel(2002)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-hls-cleanup"),
            })
            .await;

        assert_eq!(
            manager
                .adaptive_session_stream_cleanup_addrs("user1", "tok-hls-cleanup", &next_segment_addr)
                .await,
            vec![first_segment_addr]
        );
    }

    #[tokio::test]
    async fn adaptive_session_stream_cleanup_addrs_falls_back_to_same_ip_session_addrs() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let manifest_addr: SocketAddr = "127.0.0.1:55101".parse().unwrap();
        let first_segment_addr: SocketAddr = "10.41.41.89:55102".parse().unwrap();
        let next_segment_addr: SocketAddr = "10.41.41.89:55103".parse().unwrap();
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user2");
        user.max_connections = 1;

        manager.add_connection(&manifest_addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-hls-cleanup-fallback",
                virtual_id: 2003,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &manifest_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager.touch_http_activity("user2", "tok-hls-cleanup-fallback", &first_segment_addr).await;
        manager.touch_http_activity("user2", "tok-hls-cleanup-fallback", &next_segment_addr).await;

        assert_eq!(
            manager
                .adaptive_session_stream_cleanup_addrs("user2", "tok-hls-cleanup-fallback", &next_segment_addr)
                .await,
            vec![first_segment_addr]
        );
    }

    #[tokio::test]
    async fn recently_evicted_session_guard_survives_ttl_while_protected_addr_is_still_active() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let evicted_addr: SocketAddr = "127.0.0.1:55111".parse().unwrap();
        let protected_addr: SocketAddr = "127.0.0.1:55112".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-guard-session".to_string(), "127.0.0.1".to_string(), evicted_addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("guard-user");
        user.max_connections = 1;

        manager.add_connection(&evicted_addr).await;
        manager.add_connection(&protected_addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-guard-session",
                virtual_id: 2018,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &evicted_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 18,
                meter_uid: 0,
                username: &user.username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_adaptive_channel(2018),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-guard-session"),
            })
            .await;

        manager
            .mark_recent_eviction_guard_for_addr(&evicted_addr, protected_addr, 1)
            .await;
        {
            let mut connections = manager.connections.write().await;
            if let Some(registration) = connections.key_by_addr.get_mut(&protected_addr) {
                registration.username = user.username.clone();
            }
            let protection = connections
                .recently_evicted_sessions
                .get_mut("tok-guard-session")
                .expect("recent eviction guard should exist");
            protection.expires_at = current_time_secs().saturating_sub(1);
        }

        assert_eq!(
            manager
                .recently_evicted_session_protected_addr("tok-guard-session")
                .await,
            Some(protected_addr)
        );
    }

    #[tokio::test]
    async fn test_reused_logical_stream_refreshes_normal_priority() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55023".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-key-2a".to_string(), "127.0.0.1".to_string(), addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-prio",
                virtual_id: 2002,
                provider: "provider-a",
                stream_url: "http://localhost/live-prio.ts",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Soft),
                socket_bound: true,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 201,
                meter_uid: 0,
                username: "user1",
                max_connections: 1,
                soft_connections: 1,
                connection_kind: ConnectionKind::Soft,
                priority: 8,
                soft_priority: 8,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(2002),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-prio"),
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 201,
                meter_uid: 0,
                username: "user1",
                max_connections: 1,
                soft_connections: 1,
                connection_kind: ConnectionKind::Soft,
                priority: -7,
                soft_priority: 8,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(2002),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-prio"),
            })
            .await;

        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get("user1").unwrap();
        assert_eq!(connection_data.stream_normal_priorities.get(&201), Some(&-7));
    }

    #[tokio::test]
    async fn test_same_session_token_refreshes_meter_metadata_on_reuse() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55031".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-key-3".to_string(), "127.0.0.1".to_string(), addr);

        manager.add_connection(&addr).await;
        let first = manager
            .update_connection(ActiveUserConnectionParams {
                uid: 11,
                meter_uid: 101,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(3001),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-meter"),
            })
            .await
            .expect("initial stream should register");
        assert_eq!(first.uid, 11);
        assert_eq!(first.meter_uid, 101);

        let second = manager
            .update_connection(ActiveUserConnectionParams {
                uid: 22,
                meter_uid: 202,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-b".intern(),
                stream_channel: &test_channel(3002),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-meter"),
            })
            .await
            .expect("reused stream should register");

        assert_eq!(second.uid, 11, "logical stream identity should stay stable on session reuse");
        assert_eq!(second.meter_uid, 202, "reused stream must refresh its meter mapping");

        let streams = manager.active_streams().await;
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].uid, 11);
        assert_eq!(streams[0].meter_uid, 202);
        assert_eq!(streams[0].provider.as_ref(), "provider-b");
        assert_eq!(streams[0].channel.virtual_id, 3002);
    }

    #[tokio::test]
    async fn test_adaptive_session_release_connection_preserves_logical_stream_and_start_time() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55041".parse().unwrap();
        let next_addr: SocketAddr = "127.0.0.1:55042".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-key-4".to_string(), "127.0.0.1".to_string(), addr);
        let next_fingerprint = Fingerprint::new("fp-key-5".to_string(), "127.0.0.1".to_string(), next_addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-hls",
                virtual_id: 4001,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        let first = manager
            .update_connection(ActiveUserConnectionParams {
                uid: 44,
                meter_uid: 144,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveHls,
                    ..test_channel(4001)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-hls"),
            })
            .await
            .expect("initial adaptive session should register");

        let released = manager.release_connection(&addr).await;
        assert!(released.addr_removed);
        assert!(released.removed_streams.is_empty(), "adaptive session should remain logically active");
        assert_eq!(manager.user_connections("user1").await, 0);
        assert_eq!(manager.active_users_and_connections().await, (0, 0));
        assert!(manager.active_streams().await.is_empty());

        let connections = manager.connections.read().await;
        let preserved_stream = connections
            .by_key
            .get("user1")
            .and_then(|data| data.streams.iter().find(|stream| stream.uid == 44))
            .expect("preserved adaptive stream should stay internally tracked");
        assert_eq!(preserved_stream.ts, first.ts);
        assert!(preserved_stream.preserved);
        drop(connections);

        manager.add_connection(&next_addr).await;
        let second = manager
            .update_connection(ActiveUserConnectionParams {
                uid: 55,
                meter_uid: 155,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &next_fingerprint,
                provider: "provider-b".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveDash,
                    ..test_channel(4002)
                },
                user_agent: Cow::Borrowed("ua-2"),
                session_token: Some("tok-hls"),
            })
            .await
            .expect("adaptive session should reuse logical stream");

        assert_eq!(second.uid, 44);
        assert_eq!(second.ts, first.ts, "adaptive session duration must stay session-based");
        assert_eq!(second.addr, next_addr);
        assert_eq!(second.meter_uid, 155);
        assert_eq!(manager.user_connections("user1").await, 1);

        let streams = manager.active_streams().await;
        assert_eq!(streams.len(), 1);
        assert!(!streams[0].preserved);
    }

    #[tokio::test]
    async fn test_release_stream_ignores_preserved_adaptive_entry() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55051".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-key-6".to_string(), "127.0.0.1".to_string(), addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-hls",
                virtual_id: 5001,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 66,
                meter_uid: 166,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveHls,
                    ..test_channel(5001)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-hls"),
            })
            .await;

        let released = manager.release_connection(&addr).await;
        assert!(released.addr_removed);
        assert!(released.removed_streams.is_empty());
        assert!(manager.release_stream(&addr).await.is_none());
    }

    #[tokio::test]
    async fn test_preserved_adaptive_stream_is_pruned_after_session_ttl() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55061".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-key-7".to_string(), "127.0.0.1".to_string(), addr);

        manager.add_connection(&addr).await;
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-expire",
                virtual_id: 6001,
                provider: "provider-a",
                stream_url: "http://localhost/hls.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 77,
                meter_uid: 177,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveHls,
                    ..test_channel(6001)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-expire"),
            })
            .await;
        let released = manager.release_connection(&addr).await;
        assert!(released.addr_removed);

        {
            let mut connections = manager.connections.write().await;
            let connection_data = connections.by_key.get_mut("user1").unwrap();
            let session = connection_data
                .sessions
                .iter_mut()
                .find(|session| session.token == "tok-expire")
                .unwrap();
            session.ts = session.ts.saturating_sub(default_hls_session_ttl_secs() + 1);
        }
        if let Some(gc_ts) = &manager.gc_ts {
            gc_ts.store(current_time_secs().saturating_sub(USER_GC_TTL + 1), Ordering::Release);
        }

        manager
            .process_due_adaptive_expiry_entries(current_time_secs().saturating_add(default_hls_session_ttl_secs() + 1))
            .await;
        assert!(manager.active_streams().await.is_empty());
    }

    #[tokio::test]
    async fn test_due_adaptive_expiry_removal_promotes_soft_stream() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let normal_addr: SocketAddr = "127.0.0.1:55062".parse().unwrap();
        let soft_addr: SocketAddr = "127.0.0.1:55063".parse().unwrap();
        let normal_fp = Fingerprint::new("fp-key-7a".to_string(), "127.0.0.1".to_string(), normal_addr);
        let soft_fp = Fingerprint::new("fp-key-7b".to_string(), "127.0.0.1".to_string(), soft_addr);

        manager.add_connection(&normal_addr).await;
        manager.add_connection(&soft_addr).await;

        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;
        user.soft_connections = 1;

        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-expire-normal",
                virtual_id: 6002,
                provider: "provider-a",
                stream_url: "http://localhost/hls-normal.m3u8",
                addr: &normal_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 78,
                meter_uid: 178,
                username: "user1",
                max_connections: 1,
                soft_connections: 1,
                connection_kind: ConnectionKind::Normal,
                priority: -1,
                soft_priority: 9,
                fingerprint: &normal_fp,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveHls,
                    ..test_channel(6002)
                },
                user_agent: Cow::Borrowed("ua-normal"),
                session_token: Some("tok-expire-normal"),
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 79,
                meter_uid: 179,
                username: "user1",
                max_connections: 1,
                soft_connections: 1,
                connection_kind: ConnectionKind::Soft,
                priority: -5,
                soft_priority: 9,
                fingerprint: &soft_fp,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(6003),
                user_agent: Cow::Borrowed("ua-soft"),
                session_token: None,
            })
            .await;

        let released = manager.release_connection(&normal_addr).await;
        assert!(released.addr_removed);

        {
            let mut connections = manager.connections.write().await;
            let connection_data = connections.by_key.get_mut("user1").unwrap();
            let session = connection_data
                .sessions
                .iter_mut()
                .find(|session| session.token == "tok-expire-normal")
                .unwrap();
            session.ts = session.ts.saturating_sub(default_hls_session_ttl_secs() + 1);
        }

        manager
            .process_due_adaptive_expiry_entries(current_time_secs().saturating_add(default_hls_session_ttl_secs() + 1))
            .await;

        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get("user1").unwrap();
        assert_eq!(connection_data.stream_kinds.get(&79), Some(&ConnectionKind::Soft));
        assert!(!connection_data.stream_normal_priorities.contains_key(&78));
    }

    #[tokio::test]
    async fn test_repeated_preserve_for_same_adaptive_session_keeps_single_current_expiry_index() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr_a: SocketAddr = "127.0.0.1:55071".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:55072".parse().unwrap();
        let fp_a = Fingerprint::new("fp-key-a".to_string(), "127.0.0.1".to_string(), addr_a);
        let fp_b = Fingerprint::new("fp-key-b".to_string(), "127.0.0.1".to_string(), addr_b);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;

        manager.add_connection(&addr_a).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-reuse",
                virtual_id: 7001,
                provider: "provider-a",
                stream_url: "http://localhost/live-a.m3u8",
                addr: &addr_a,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 88,
                meter_uid: 188,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fp_a,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveHls,
                    ..test_channel(7001)
                },
                user_agent: Cow::Borrowed("ua-a"),
                session_token: Some("tok-reuse"),
            })
            .await;
        let released = manager.release_connection(&addr_a).await;
        assert!(released.addr_removed);

        manager.add_connection(&addr_b).await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 99,
                meter_uid: 199,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fp_b,
                provider: "provider-b".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveDash,
                    ..test_channel(7002)
                },
                user_agent: Cow::Borrowed("ua-b"),
                session_token: Some("tok-reuse"),
            })
            .await;
        let released = manager.release_connection(&addr_b).await;
        assert!(released.addr_removed);

        let expiry_index = manager.adaptive_expiry_index.lock().await;
        assert_eq!(expiry_index.len(), 1);
        assert!(expiry_index.contains_key(&AdaptiveExpiryKey {
            username: String::from("user1"),
            session_token: String::from("tok-reuse"),
            uid: 88,
        }));
    }

    #[tokio::test]
    async fn test_release_stream_preserved_path_emits_connection_update_event() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);
        let mut events = event_manager.get_event_channel();

        let addr: SocketAddr = "127.0.0.1:55081".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-key-8".to_string(), "127.0.0.1".to_string(), addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-event",
                virtual_id: 8001,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 111,
                meter_uid: 211,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveHls,
                    ..test_channel(8001)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-event"),
            })
            .await;
        let _ = events.try_recv();

        let released = manager.release_stream(&addr).await;
        assert!(released.is_none(), "adaptive stream should remain logically preserved");

        let event = events.try_recv().expect("preserved release should emit an ActiveUser event");
        assert!(matches!(event, EventMessage::ActiveUser(_)));
    }

    #[tokio::test]
    async fn test_release_stream_without_session_removes_adaptive_stream_instead_of_preserving() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55082".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-key-9".to_string(), "127.0.0.1".to_string(), addr);

        manager.add_connection(&addr).await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 122,
                meter_uid: 222,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveHls,
                    ..test_channel(8002)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("missing-session"),
            })
            .await;

        let released = manager.release_stream(&addr).await;
        assert!(released.is_some(), "stream without schedulable expiry must be removed");
        assert!(manager.active_streams().await.is_empty());
    }

    #[tokio::test]
    async fn test_due_adaptive_expiry_reschedules_when_session_timestamp_changes() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55083".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-key-10".to_string(), "127.0.0.1".to_string(), addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-reschedule",
                virtual_id: 8003,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 133,
                meter_uid: 233,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveHls,
                    ..test_channel(8003)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-reschedule"),
            })
            .await;
        let released = manager.release_connection(&addr).await;
        assert!(released.addr_removed);

        let key = AdaptiveExpiryKey {
            username: String::from("user1"),
            session_token: String::from("tok-reschedule"),
            uid: 133,
        };
        let old_expires_at = {
            let expiry_index = manager.adaptive_expiry_index.lock().await;
            *expiry_index.get(&key).unwrap()
        };

        {
            let mut connections = manager.connections.write().await;
            let session = connections
                .by_key
                .get_mut("user1")
                .unwrap()
                .sessions
                .iter_mut()
                .find(|session| session.token == "tok-reschedule")
                .unwrap();
            session.ts = session.ts.saturating_add(30);
        }

        manager.process_due_adaptive_expiry_entries(old_expires_at).await;

        let new_expires_at = {
            let expiry_index = manager.adaptive_expiry_index.lock().await;
            *expiry_index.get(&key).unwrap()
        };
        assert!(new_expires_at > old_expires_at);
        assert!(manager.active_streams().await.is_empty());
    }

    #[tokio::test]
    async fn test_due_adaptive_expiry_removes_stale_index_when_preserved_stream_missing() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55085".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-key-11a".to_string(), "127.0.0.1".to_string(), addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-stale",
                virtual_id: 8004,
                provider: "provider-a",
                stream_url: "http://localhost/stale.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 134,
                meter_uid: 234,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveHls,
                    ..test_channel(8004)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-stale"),
            })
            .await;
        let released = manager.release_connection(&addr).await;
        assert!(released.addr_removed);

        let key = AdaptiveExpiryKey {
            username: String::from("user1"),
            session_token: String::from("tok-stale"),
            uid: 134,
        };
        let old_expires_at = {
            let expiry_index = manager.adaptive_expiry_index.lock().await;
            *expiry_index.get(&key).unwrap()
        };

        {
            let mut connections = manager.connections.write().await;
            let connection_data = connections.by_key.get_mut("user1").unwrap();
            connection_data.streams.clear();
        }

        manager.process_due_adaptive_expiry_entries(old_expires_at).await;

        let expiry_index = manager.adaptive_expiry_index.lock().await;
        assert!(!expiry_index.contains_key(&key));
    }

    #[tokio::test]
    async fn test_due_adaptive_expiry_does_not_block_on_full_cleanup_channel() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55084".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-key-11".to_string(), "127.0.0.1".to_string(), addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-full-channel",
                virtual_id: 8004,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 144,
                meter_uid: 244,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveHls,
                    ..test_channel(8004)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-full-channel"),
            })
            .await;
        let released = manager.release_connection(&addr).await;
        assert!(released.addr_removed);

        {
            let mut connections = manager.connections.write().await;
            let session = connections
                .by_key
                .get_mut("user1")
                .unwrap()
                .sessions
                .iter_mut()
                .find(|session| session.token == "tok-full-channel")
                .unwrap();
            session.ts = session.ts.saturating_sub(default_hls_session_ttl_secs() + 1);
        }

        let (cleanup_tx, mut cleanup_rx) = mpsc::channel(1);
        cleanup_tx
            .send(CleanupEvent::ReleaseConnection { addr })
            .await
            .expect("prefill cleanup channel");
        manager.set_cleanup_sender(cleanup_tx);

        let process_result = tokio::time::timeout(
            Duration::from_millis(100),
            manager.process_due_adaptive_expiry_entries(current_time_secs().saturating_add(default_hls_session_ttl_secs() + 1)),
        )
        .await;

        assert!(process_result.is_ok(), "adaptive expiry processing must not await while holding locks");

        let queued_event = cleanup_rx.try_recv().expect("prefilled cleanup event should remain queued");
        assert!(matches!(queued_event, CleanupEvent::ReleaseConnection { .. }));
        assert!(manager.active_streams().await.is_empty());
    }

    #[tokio::test]
    async fn test_preserved_adaptive_stream_reconnect_across_day_sets_previous_session_id() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55085".parse().unwrap();
        let next_addr: SocketAddr = "127.0.0.1:55086".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-rollover-a".to_string(), "127.0.0.1".to_string(), addr);
        let next_fingerprint = Fingerprint::new("fp-rollover-b".to_string(), "127.0.0.1".to_string(), next_addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-rollover",
                virtual_id: 8005,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        let first = manager
            .update_connection(ActiveUserConnectionParams {
                uid: 145,
                meter_uid: 245,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveHls,
                    ..test_channel(8005)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-rollover"),
            })
            .await
            .expect("initial adaptive session should register");

        let released = manager.release_connection(&addr).await;
        assert!(released.addr_removed);

        let forced_old_ts = {
            let mut connections = manager.connections.write().await;
            let stream = connections
                .by_key
                .get_mut("user1")
                .unwrap()
                .streams
                .iter_mut()
                .find(|stream| stream.session_token.as_deref() == Some("tok-rollover"))
                .unwrap();
            stream.ts = stream.ts.saturating_sub(86_400);
            stream.ts
        };

        manager.add_connection(&next_addr).await;
        let second = manager
            .update_connection(ActiveUserConnectionParams {
                uid: 146,
                meter_uid: 246,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &next_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveDash,
                    ..test_channel(8005)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-rollover"),
            })
            .await
            .expect("adaptive session should reconnect");

        assert_eq!(second.previous_session_id, Some((forced_old_ts << 32) | u64::from(first.uid)));
        assert!(second.ts > forced_old_ts);
        assert_eq!(utc_day_from_secs(second.ts), utc_day_from_secs(current_time_secs()));
    }

    #[tokio::test]
    async fn stale_anonymous_socket_registration_is_pruned_by_gc() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let stale_addr: SocketAddr = "127.0.0.1:55011".parse().unwrap();
        let fresh_addr: SocketAddr = "127.0.0.1:55012".parse().unwrap();

        manager.add_connection(&stale_addr).await;
        {
            let mut connections = manager.connections.write().await;
            let registration = connections.key_by_addr.get_mut(&stale_addr).expect("socket registration should exist");
            registration.ts = registration.ts.saturating_sub(ANON_SOCKET_TTL + 1);
        }

        if let Some(gc_ts) = &manager.gc_ts {
            gc_ts.store(current_time_secs().saturating_sub(USER_GC_TTL + 1), Ordering::Release);
        }

        manager.add_connection(&fresh_addr).await;

        let connections = manager.connections.read().await;
        assert!(!connections.key_by_addr.contains_key(&stale_addr));
        assert!(connections.key_by_addr.contains_key(&fresh_addr));
    }

    #[tokio::test]
    async fn named_socket_registration_exposes_expiry_deadline() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let stale_addr: SocketAddr = "127.0.0.1:55021".parse().unwrap();
        let fresh_addr: SocketAddr = "127.0.0.1:55022".parse().unwrap();
        let stale_fp = Fingerprint::new("fp-stale".to_string(), "127.0.0.1".to_string(), stale_addr);
        let fresh_fp = Fingerprint::new("fp-fresh".to_string(), "127.0.0.1".to_string(), fresh_addr);

        manager.add_connection(&stale_addr).await;
        manager.add_connection(&fresh_addr).await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 201,
                meter_uid: 301,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &stale_fp,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(9201),
                user_agent: Cow::Borrowed("ua"),
                session_token: None,
            })
            .await
            .expect("stale stream should register");
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 202,
                meter_uid: 302,
                username: "user2",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fresh_fp,
                provider: "provider-b".intern(),
                stream_channel: &test_channel(9202),
                user_agent: Cow::Borrowed("ua"),
                session_token: None,
            })
            .await
            .expect("fresh stream should register");

        {
            let mut connections = manager.connections.write().await;
            let stale_registration = connections
                .key_by_addr
                .get_mut(&stale_addr)
                .expect("stale registration should exist");
            stale_registration.ts = stale_registration.ts.saturating_sub(DEFAULT_ACTIVE_SOCKET_TTL_SECS + 1);
        }

        let stale_deadline = manager
            .socket_expiry_deadline(&stale_addr)
            .await
            .expect("stale named socket should have an expiry deadline");
        let fresh_deadline = manager
            .socket_expiry_deadline(&fresh_addr)
            .await
            .expect("fresh named socket should have an expiry deadline");
        assert!(stale_deadline < fresh_deadline);
    }

    #[tokio::test]
    async fn touch_http_activity_refreshes_session_and_registration_without_stream() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55024".parse().unwrap();
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-http-touch",
                virtual_id: 9302,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        let previous_ts = {
            let mut connections = manager.connections.write().await;
            let previous_ts = {
                let registration = connections.key_by_addr.get_mut(&addr).expect("registration should exist");
                registration.ts = registration.ts.saturating_sub(DEFAULT_ACTIVE_SOCKET_TTL_SECS + 5);
                registration.ts
            };
            let connection_data = connections.by_key.get_mut("user1").expect("user should exist");
            connection_data.sessions[0].ts = connection_data.sessions[0].ts.saturating_sub(DEFAULT_ACTIVE_SOCKET_TTL_SECS + 5);
            previous_ts
        };

        manager.touch_http_activity("user1", "tok-http-touch", &addr).await;

        let connections = manager.connections.read().await;
        let registration = connections.key_by_addr.get(&addr).expect("registration should still exist");
        let connection_data = connections.by_key.get("user1").expect("user should still exist");
        assert!(registration.ts > previous_ts);
        assert!(connection_data.sessions[0].ts >= registration.ts);
    }

    #[tokio::test]
    async fn touch_http_activity_does_not_reset_stream_started_at_ts() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr1: SocketAddr = "127.0.0.1:55030".parse().unwrap();
        let addr2: SocketAddr = "127.0.0.1:55031".parse().unwrap();
        let fingerprint = Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), addr1);
        let mut user = ProxyUserCredentials::default();
        user.username = "user-touch-ts".to_string();

        manager.add_connection(&addr1).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-hls-ts",
                virtual_id: 7777,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr1,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        // Simulate first HLS segment: creates the stream entry with ts = now
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 601,
                meter_uid: 701,
                username: "user-touch-ts",
                max_connections: 0,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_adaptive_channel(7777),
                user_agent: Cow::Borrowed("player/1.0"),
                session_token: Some("tok-hls-ts"),
            })
            .await
            .expect("stream should be created");

        // Record the original stream start timestamp
        let original_ts = {
            let connections = manager.connections.read().await;
            connections
                .by_key
                .get("user-touch-ts")
                .and_then(|data| data.streams.iter().find(|s| s.session_token.as_deref() == Some("tok-hls-ts")))
                .map(|s| s.ts)
                .expect("stream should exist")
        };

        // Simulate manifest re-fetch (touch_http_activity called with a new addr)
        manager.touch_http_activity("user-touch-ts", "tok-hls-ts", &addr2).await;

        // stream.ts must NOT have been reset — it represents session start time shown as Duration
        let connections = manager.connections.read().await;
        let stream = connections
            .by_key
            .get("user-touch-ts")
            .and_then(|data| data.streams.iter().find(|s| s.session_token.as_deref() == Some("tok-hls-ts")))
            .expect("stream should still exist");
        assert_eq!(stream.ts, original_ts, "touch_http_activity must not reset the stream start timestamp");
        // addr should be updated to reflect the latest manifest request
        assert_eq!(stream.addr, addr2, "touch_http_activity should update the stream addr");
    }

    #[tokio::test]
    async fn update_session_addr_prunes_previous_registration_for_socket_bound_session() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let old_addr: SocketAddr = "127.0.0.1:55121".parse().unwrap();
        let new_addr: SocketAddr = "127.0.0.1:55122".parse().unwrap();
        let old_fingerprint = Fingerprint::new("fp-old".to_string(), "127.0.0.1".to_string(), old_addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;

        manager.add_connection(&old_addr).await;
        manager.add_connection(&new_addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-move",
                virtual_id: 9101,
                provider: "provider-a",
                stream_url: "http://localhost/live.ts",
                addr: &old_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: true,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 301,
                meter_uid: 401,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &old_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::Live,
                    ..test_channel(9101)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-move"),
            })
            .await
            .expect("initial live stream should register");

        manager.update_session_addr("user1", "tok-move", &new_addr).await;

        let connections = manager.connections.read().await;
        assert!(
            !connections.key_by_addr.contains_key(&old_addr),
            "previous range-request socket registration should be pruned once the session moved"
        );
        assert!(connections.key_by_addr.contains_key(&new_addr));

        let connection_data = connections.by_key.get("user1").expect("user connection data");
        assert_eq!(connection_data.sessions.len(), 1);
        assert_eq!(connection_data.sessions[0].addr, new_addr);
        assert_eq!(connection_data.streams.len(), 1);
        assert_eq!(connection_data.streams[0].addr, new_addr);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn vod_session_survives_overlapping_and_seek_sockets() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let base_addr: SocketAddr = "127.0.0.1:55131".parse().unwrap();
        let range_addr: SocketAddr = "127.0.0.1:55132".parse().unwrap();
        let seek_addr: SocketAddr = "127.0.0.1:55133".parse().unwrap();
        let base_fingerprint = Fingerprint::new("fp-vod-base".to_string(), "127.0.0.1".to_string(), base_addr);
        let range_fingerprint = Fingerprint::new("fp-vod-range".to_string(), "127.0.0.1".to_string(), range_addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user1");
        user.max_connections = 1;

        manager.add_connection(&base_addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-vod",
                virtual_id: 9102,
                provider: "provider-a",
                stream_url: "http://localhost/movie.mkv",
                addr: &base_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 302,
                meter_uid: 402,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &base_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::Video,
                    ..test_channel(9102)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-vod"),
            })
            .await
            .expect("initial vod stream should register");

        manager.add_connection(&range_addr).await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 303,
                meter_uid: 403,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &range_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::Video,
                    ..test_channel(9102)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-vod"),
            })
            .await
            .expect("overlapping range request should reuse the same vod session");

        assert_eq!(manager.user_connections("user1").await, 1);
        assert!(manager.release_stream(&range_addr).await.is_none());
        let released = manager.release_connection(&range_addr).await;
        assert!(released.addr_removed);
        assert!(released.removed_streams.is_empty());

        {
            let connections = manager.connections.read().await;
            assert!(connections.key_by_addr.contains_key(&base_addr));
            let connection_data = connections.by_key.get("user1").expect("user connection data");
            assert_eq!(connection_data.sessions[0].addr, base_addr);
            assert_eq!(connection_data.streams[0].addr, base_addr);
        }

        manager.add_connection(&seek_addr).await;
        manager.update_session_addr("user1", "tok-vod", &seek_addr).await;

        {
            let connections = manager.connections.read().await;
            assert!(
                connections.key_by_addr.contains_key(&base_addr),
                "existing vod socket must remain registered while the session spans multiple requests"
            );
            assert!(connections.key_by_addr.contains_key(&seek_addr));

            let connection_data = connections.by_key.get("user1").expect("user connection data");
            assert_eq!(connection_data.sessions[0].addr, seek_addr);
            assert_eq!(connection_data.streams[0].addr, seek_addr);
        }

        assert!(manager.release_stream(&seek_addr).await.is_none());
        let released = manager.release_connection(&seek_addr).await;
        assert!(released.addr_removed);
        assert!(released.removed_streams.is_empty());

        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get("user1").expect("user connection data");
        assert_eq!(connection_data.sessions[0].addr, base_addr);
        assert_eq!(connection_data.streams[0].addr, base_addr);
    }

    #[tokio::test]
    async fn catchup_release_connection_preserves_logical_stream_until_session_expires() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55141".parse().unwrap();
        let next_addr: SocketAddr = "127.0.0.1:55142".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-catchup-1".to_string(), "127.0.0.1".to_string(), addr);
        let next_fingerprint = Fingerprint::new("fp-catchup-2".to_string(), "127.0.0.1".to_string(), next_addr);
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user-catchup");
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-catchup",
                virtual_id: 9103,
                provider: "provider-a",
                stream_url: "http://localhost/archive.ts",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        let first = manager
            .update_connection(ActiveUserConnectionParams {
                uid: 304,
                meter_uid: 404,
                username: &user.username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::Catchup,
                    ..test_channel(9103)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-catchup"),
            })
            .await
            .expect("initial catchup stream should register");

        let released = manager.release_connection(&addr).await;
        assert!(released.addr_removed);
        assert!(
            released.removed_streams.is_empty(),
            "catchup stream should remain logically active between range requests"
        );

        assert_eq!(manager.user_connections(&user.username).await, 0);
        assert!(manager.active_streams().await.is_empty());

        let connections = manager.connections.read().await;
        let preserved_stream = connections
            .by_key
            .get(&user.username)
            .and_then(|data| data.streams.iter().find(|stream| stream.uid == first.uid))
            .expect("preserved catchup stream should stay internally tracked");
        assert!(preserved_stream.preserved);
        drop(connections);

        manager.add_connection(&next_addr).await;
        let second = manager
            .update_connection(ActiveUserConnectionParams {
                uid: 305,
                meter_uid: 405,
                username: &user.username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &next_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::Catchup,
                    ..test_channel(9103)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-catchup"),
            })
            .await
            .expect("catchup stream should reconnect");

        assert_eq!(second.uid, first.uid);
        assert_eq!(second.started_at, first.started_at);
        assert!(!second.preserved);
    }

    #[tokio::test]
    async fn gc_keeps_active_ts_streams_even_when_user_timestamp_is_stale() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55013".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-key-ts".to_string(), "127.0.0.1".to_string(), addr);

        manager.add_connection(&addr).await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 144,
                meter_uid: 244,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(9001),
                user_agent: Cow::Borrowed("ua"),
                session_token: None,
            })
            .await
            .expect("ts stream should register");

        {
            let mut connections = manager.connections.write().await;
            let connection_data = connections.by_key.get_mut("user1").expect("user entry should exist");
            connection_data.ts = connection_data.ts.saturating_sub(USER_CON_TTL + 1);
        }

        if let Some(gc_ts) = &manager.gc_ts {
            gc_ts.store(current_time_secs().saturating_sub(USER_GC_TTL + 1), Ordering::Release);
        }

        manager.active_streams().await;

        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get("user1").expect("active user entry must survive gc");
        assert_eq!(connection_data.connections, 1);
        assert_eq!(connection_data.streams.len(), 1);
    }

    #[tokio::test]
    async fn session_activation_keeps_first_hls_slot_uncommitted_before_stream_registration() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user-hls-reserve");
        user.max_connections = 1;

        let first_addr: SocketAddr = "127.0.0.1:55180".parse().unwrap();
        let second_addr: SocketAddr = "127.0.0.1:55181".parse().unwrap();

        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-first",
                virtual_id: 9201,
                provider: "provider-a",
                stream_url: "http://localhost/live-a.m3u8",
                addr: &first_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-second",
                virtual_id: 9202,
                provider: "provider-a",
                stream_url: "http://localhost/live-b.m3u8",
                addr: &second_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        let first_admission = manager
            .connection_admission_for_session_activation(&user.username, user.max_connections, 0, "tok-first")
            .await;
        let second_admission = manager
            .connection_admission_for_session_activation(&user.username, user.max_connections, 0, "tok-second")
            .await;

        assert_eq!(first_admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(first_admission.kind, Some(ConnectionKind::Normal));
        assert_eq!(second_admission.permission, UserConnectionPermission::Allowed);

        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get(&user.username).expect("user connection data");
        assert_eq!(connection_data.connections, 0);
        assert_eq!(connection_data.counts.normal, 0);
        assert_eq!(connection_data.streams.len(), 0);
        assert!(
            connection_data
                .sessions
                .iter()
                .find(|session| session.token == "tok-first")
                .is_some_and(|session| !session.lifecycle.is_counted())
        );
        assert!(
            connection_data
                .sessions
                .iter()
                .find(|session| session.token == "tok-second")
                .is_some_and(|session| !session.lifecycle.is_counted())
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn binding_reserved_sessions_keeps_hard_and_soft_counts_stable() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user-hls-soft");
        user.max_connections = 1;
        user.soft_connections = 1;

        let first_addr: SocketAddr = "127.0.0.1:55182".parse().unwrap();
        let second_addr: SocketAddr = "127.0.0.1:55183".parse().unwrap();
        let first_fingerprint = Fingerprint::new("fp-hls-1".to_string(), "127.0.0.1".to_string(), first_addr);
        let second_fingerprint = Fingerprint::new("fp-hls-2".to_string(), "127.0.0.1".to_string(), second_addr);

        manager.add_connection(&first_addr).await;
        manager.add_connection(&second_addr).await;

        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-normal",
                virtual_id: 9203,
                provider: "provider-a",
                stream_url: "http://localhost/live-normal.m3u8",
                addr: &first_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-soft",
                virtual_id: 9204,
                provider: "provider-a",
                stream_url: "http://localhost/live-soft.m3u8",
                addr: &second_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        let first_admission = manager
            .connection_admission_for_session_activation(&user.username, user.max_connections, user.soft_connections, "tok-normal")
            .await;
        assert_eq!(first_admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(first_admission.kind, Some(ConnectionKind::Normal));

        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 401,
                meter_uid: 0,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &first_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_adaptive_channel(9203),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-normal"),
            })
            .await
            .expect("reserved normal session should bind");

        let second_admission = manager
            .connection_admission_for_session_activation(&user.username, user.max_connections, user.soft_connections, "tok-soft")
            .await;
        assert_eq!(second_admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(second_admission.kind, Some(ConnectionKind::Soft));

        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 402,
                meter_uid: 0,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: ConnectionKind::Soft,
                priority: 0,
                soft_priority: 0,
                fingerprint: &second_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_adaptive_channel(9204),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-soft"),
            })
            .await
            .expect("reserved soft session should bind");

        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get(&user.username).expect("user connection data");
        assert_eq!(connection_data.connections, 2);
        assert_eq!(connection_data.counts.normal, 1);
        assert_eq!(connection_data.counts.soft, 1);
        assert_eq!(connection_data.streams.len(), 2);
        assert_eq!(
            connection_data.stream_kinds.get(&401),
            Some(&ConnectionKind::Normal),
            "binding a reserved normal session must not increment counts twice"
        );
        assert_eq!(
            connection_data.stream_kinds.get(&402),
            Some(&ConnectionKind::Soft),
            "binding a reserved soft session must keep the soft classification"
        );
    }

    #[tokio::test]
    async fn release_unbound_session_reservation_frees_reserved_slot() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user-release-reservation");
        user.max_connections = 1;

        let addr: SocketAddr = "127.0.0.1:55184".parse().unwrap();
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-release",
                virtual_id: 9205,
                provider: "provider-a",
                stream_url: "http://localhost/live-release.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        let admission = manager
            .connection_admission_for_session_activation(&user.username, user.max_connections, 0, "tok-release")
            .await;
        assert_eq!(admission.permission, UserConnectionPermission::Allowed);

        manager
            .release_unbound_session_reservation(&user.username, "tok-release", None, false)
            .await;

        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get(&user.username).expect("user connection data");
        assert_eq!(connection_data.connections, 0);
        assert_eq!(connection_data.counts.normal, 0);
        assert_eq!(connection_data.streams.len(), 0);
        assert!(
            connection_data
                .sessions
                .iter()
                .find(|session| session.token == "tok-release")
                .is_some_and(|session| !session.lifecycle.is_counted())
        );
    }

    #[tokio::test]
    async fn release_unbound_session_reservation_ignores_stale_transition_version() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user-stale-release");

        let addr: SocketAddr = "127.0.0.1:55194".parse().unwrap();
        let stale_version = manager
            .ensure_user_session_placeholder(CreateUserSessionParams {
                user: &user,
                session_token: "tok-stale-release",
                virtual_id: 9206,
                provider: "provider-a",
                stream_url: "http://localhost/live-stale.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        let _ = manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-stale-release",
                virtual_id: 9206,
                provider: "provider-b",
                stream_url: "http://localhost/live-stale-updated.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        manager
            .release_unbound_session_reservation(&user.username, "tok-stale-release", Some(stale_version), true)
            .await;

        let session = manager
            .get_and_update_user_session(&user.username, "tok-stale-release")
            .await
            .expect("stale rollback must not remove the newer session");
        assert!(session.transition_version > stale_version);
        assert_eq!(session.provider.as_ref(), "provider-b");
        assert_eq!(session.stream_url.as_ref(), "http://localhost/live-stale-updated.m3u8");
    }

    #[tokio::test]
    async fn clear_unbound_session_addr_prunes_manifest_addr_without_stream() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let first_addr: SocketAddr = "127.0.0.1:55185".parse().unwrap();
        let second_addr: SocketAddr = "127.0.0.1:55186".parse().unwrap();
        let mut user = ProxyUserCredentials::default();
        user.username = String::from("user-clear-addr");
        user.max_connections = 1;

        manager.add_connection(&first_addr).await;
        manager.add_connection(&second_addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-clear-addr",
                virtual_id: 9206,
                provider: "provider-a",
                stream_url: "http://localhost/live-clear.m3u8",
                addr: &first_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-clear-addr",
                virtual_id: 9206,
                provider: "provider-a",
                stream_url: "http://localhost/live-clear.m3u8",
                addr: &second_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        manager
            .clear_unbound_session_addr(&user.username, "tok-clear-addr", &second_addr)
            .await;

        let connections = manager.connections.read().await;
        let session = connections
            .by_key
            .get(&user.username)
            .and_then(|connection_data| connection_data.sessions.iter().find(|session| session.token == "tok-clear-addr"))
            .expect("session should remain");
        assert_eq!(session.addr, first_addr);
        assert_eq!(session.active_addrs, vec![first_addr]);
    }

    #[tokio::test]
    async fn get_eviction_candidates_keeps_preserved_streams_evictable() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55300".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-key".to_string(), "192.168.1.100".to_string(), addr);
        let username = "user-eviction-addr";
        let mut user = ProxyUserCredentials::default();
        user.username = username.to_string();
        user.max_connections = 1;
        user.soft_connections = 0;

        // Create session first (HLS type = preserved after release)
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-preserved-1",
                virtual_id: 5001,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        // Create stream + register connection
        manager.add_connection(&addr).await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 1,
                meter_uid: 0,
                username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_adaptive_channel(5001),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-preserved-1"),
            })
            .await
            .expect("first stream");
        assert_eq!(manager.user_connections(username).await, 1);

        // Release -> stream becomes preserved, session becomes uncounted
        manager.release_stream(&addr).await;
        assert_eq!(manager.user_connections(username).await, 0, "preserved stream should not count");

        let candidates = manager.get_eviction_candidates(username, "192.168.1.100").await;
        assert!(
            candidates.iter().any(|candidate| candidate.addr == addr),
            "preserved stream should remain a direct eviction candidate"
        );
    }

    #[tokio::test]
    async fn get_eviction_candidates_does_not_count_preserved_streams_in_addr_counts() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55801".parse().unwrap();
        let fingerprint =
            Fingerprint::new("fp-preserved-no-count".to_string(), "10.0.0.5".to_string(), addr);
        let username = "user-preserved-addr-count";
        let mut user = ProxyUserCredentials::default();
        user.username = username.to_string();
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-preserved-addr-count",
                virtual_id: 7000,
                provider: "provider-preserved",
                stream_url: "http://localhost/preserved.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 7000,
                meter_uid: 0,
                username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-preserved".intern(),
                stream_channel: &test_adaptive_channel(7000),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-preserved-addr-count"),
            })
            .await
            .expect("stream should be created");

        // Release -> stream becomes preserved, session becomes uncounted
        manager.release_stream(&addr).await;

        // Preserved streams do not consume a counted slot — user_connections should be 0
        assert_eq!(
            manager.user_connections(username).await, 0,
            "preserved stream should not count toward active connections"
        );

        // But the preserved stream is still a valid eviction candidate (valid victim)
        let candidates = manager.get_eviction_candidates(username, "10.0.0.5").await;
        assert!(
            candidates.iter().any(|c| c.addr == addr),
            "preserved stream should be an eviction candidate"
        );
    }

    #[tokio::test]
    async fn connection_admission_treats_preserved_stream_as_reserved_capacity() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55305".parse().unwrap();
        let fingerprint = Fingerprint::new("fp-preserved-admission".to_string(), "192.168.1.100".to_string(), addr);
        let username = "user-preserved-admission";
        let mut user = ProxyUserCredentials::default();
        user.username = username.to_string();
        user.max_connections = 1;

        manager.add_connection(&addr).await;
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "tok-preserved-admission",
                virtual_id: 6000,
                provider: "provider-a",
                stream_url: "http://localhost/live-preserved.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 6000,
                meter_uid: 0,
                username,
                max_connections: user.max_connections,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_adaptive_channel(6000),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-preserved-admission"),
            })
            .await
            .expect("preserved stream should be created");

        manager.release_connection(&addr).await;
        assert_eq!(manager.user_connections(username).await, 0, "preserved stream stays uncounted for active snapshots");

        let admission = manager.connection_admission(username, user.max_connections, 0).await;
        assert_eq!(
            admission.permission,
            UserConnectionPermission::Exhausted,
            "a preserved stream should still reserve capacity against unrelated playback admissions"
        );
    }

    #[tokio::test]
    async fn connection_admission_for_session_evaluates_admission_for_uncounted_session() {
        // Bug: connection_admission_for_session returns Allowed for any existing session,
        // even if it's uncounted (preserved). This causes strategy evaluation to be skipped.
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let addr: SocketAddr = "127.0.0.1:55310".parse().unwrap();
        let username = "user-uncounted-admission";
        let mut user = ProxyUserCredentials::default();
        user.username = username.to_string();
        user.max_connections = 1;
        user.soft_connections = 0;

        // Create session + counted stream (HLS type = preserved after release)
        manager.add_connection(&addr).await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 1,
                meter_uid: 0,
                username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &Fingerprint::new("fp".to_string(), "192.168.1.50".to_string(), addr),
                provider: "provider-a".intern(),
                stream_channel: &test_adaptive_channel(6001),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-uncounted"),
            })
            .await
            .expect("first stream");

        // Release to preserve (uncounted session, but counts.normal still = 1 from the stream)
        manager.release_stream(&addr).await;
        // After preserve: session is uncounted, stream is preserved, connections=0
        // BUT the stream was removed, so counts.normal is decremented -> counts=0
        assert_eq!(manager.user_connections(username).await, 0);

        // Add a second stream first - this uses a different session token and consumes the slot
        let second_addr: SocketAddr = "192.168.1.100:55311".parse().unwrap();
        manager.add_connection(&second_addr).await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 2,
                meter_uid: 0,
                username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &Fingerprint::new("fp2".to_string(), "192.168.1.100".to_string(), second_addr),
                provider: "provider-b".intern(),
                stream_channel: &test_channel(6002),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-second"),
            })
            .await
            .expect("second stream");
        // Now user is at limit: connections=1, counts.normal=1, max_connections=1
        assert_eq!(manager.user_connections(username).await, 1);

        // connection_admission_for_session for the PRESERVED session token should return
        // Exhausted so that eviction strategies can run and evict the preserved stream,
        // freeing a slot for the uncounted session to reactivate
        let admission = manager
            .connection_admission_for_session(username, 1, 0, "tok-uncounted")
            .await;
        assert_eq!(
            admission.permission,
            UserConnectionPermission::Exhausted,
            "uncounted session should not bypass admission when user is at limit; \
             bug: session exists -> Allowed -> strategy evaluation skipped"
        );
    }

    #[tokio::test]
    async fn playback_transition_gate_serializes_same_session() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));

        let first_guard = manager.acquire_playback_transition("user-gated", "tok-gated").await;
        let second_manager = Arc::clone(&manager);
        let waiting = tokio::spawn(async move {
            let _second_guard = second_manager.acquire_playback_transition("user-gated", "tok-gated").await;
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            !waiting.is_finished(),
            "same-session transition gate should block a concurrent transition until the first completes"
        );

        drop(first_guard);
        tokio::time::timeout(Duration::from_millis(100), waiting)
            .await
            .expect("second transition should proceed once the first guard is released")
            .expect("second transition task should complete");
    }

    #[tokio::test]
    async fn check_divergence_detects_connection_count_mismatch() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));

        let addr: SocketAddr = "127.0.0.1:55902".parse().unwrap();
        let mut user = ProxyUserCredentials::default();
        user.username = "div-user-2".to_string();

        // Create session + increment legacy counter without a counted session
        {
            let mut connections = manager.connections.write().await;
            let data = connections.by_key.entry(user.username.clone())
                .or_insert_with(|| UserConnectionData::new(0, 1, 0));
            // Manually set legacy connections to 1 without any counted session
            data.increment_kind(ConnectionKind::Normal); // connections = 1
            data.add_session(UserSession {
                token: "tok-div-2".to_string(),
                transition_version: 1,
                virtual_id: 9002,
                provider: "provider-a".intern(),
                stream_url: "http://localhost/stream.ts".intern(),
                addr,
                socket_bound: false,
                active_addrs: vec![addr],
                ts: current_time_secs(),
                started_at: current_time_secs(),
                permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                lifecycle: PlaybackLifecycle::Prepared,
            });
        }

        let connections = manager.connections.read().await;
        let data = connections.by_key.get(&user.username).expect("user connection data");
        let snapshot = ActiveUserManager::collect_divergence_snapshot(data, &user.username);
        drop(connections);
        manager.log_divergence_snapshot(snapshot).await;
    }

    #[tokio::test]
    async fn check_divergence_detects_stream_without_counted_session() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));

        let addr: SocketAddr = "127.0.0.1:55903".parse().unwrap();
        let mut user = ProxyUserCredentials::default();
        user.username = "div-user-3".to_string();

        {
            let mut connections = manager.connections.write().await;
            let data = connections.by_key.entry(user.username.clone())
                .or_insert_with(|| UserConnectionData::new(0, 1, 0));

            // Add a session with GraceHold pending — exempt from Invariant 1
            data.add_session(UserSession {
                token: "tok-div-3".to_string(),
                transition_version: 1,
                virtual_id: 9003,
                provider: "provider-a".intern(),
                stream_url: "http://localhost/stream.ts".intern(),
                addr,
                socket_bound: false,
                active_addrs: vec![addr],
                ts: current_time_secs(),
                started_at: current_time_secs(),
                permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                lifecycle: PlaybackLifecycle::PendingProvider {
                    data: PendingProviderState {
                        reason_code: PendingProviderReason::GraceHold,
                        created_at: current_time_secs(),
                        deadline: current_time_secs() + 30,
                        version: 1,
                        wake_source: None,
                    }
                },
            });
            data.increment_kind(ConnectionKind::Normal);

            // Add a stream whose session_token doesn't match any counted session
            let orphan_stream = StreamInfo::new(
                903, 0, &user.username, &addr, "127.0.0.1", "provider-a".intern(),
                StreamChannel {
                    target_id: 1, virtual_id: 9003, provider_id: 1,
                    input_name: "provider-a".intern(), item_type: PlaylistItemType::Live,
                    cluster: XtreamCluster::Live, group: "g".intern(), title: "t".intern(),
                    url: "http://localhost/stream.ts".intern(),
                    shared: false, shared_joined_existing: None, shared_stream_id: None, technical: None,
                    epg_channel_id: None,
                },
                "ua".to_string(), None, Some("tok-orphan"),
            );
            data.streams.push(orphan_stream);
            data.stream_kinds.insert(903, ConnectionKind::Normal);
        }

        let connections = manager.connections.read().await;
        let data = connections.by_key.get(&user.username).expect("user connection data");
        let snapshot = ActiveUserManager::collect_divergence_snapshot(data, &user.username);
        drop(connections);
        manager.log_divergence_snapshot(snapshot).await;
    }

    #[tokio::test]
    async fn divergence_log_rate_limited_within_cooldown_window() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));

        let addr: SocketAddr = "127.0.0.1:55904".parse().unwrap();
        let mut user = ProxyUserCredentials::default();
        user.username = "div-user-4".to_string();

        // Create mismatch
        {
            let mut connections = manager.connections.write().await;
            let data = connections.by_key.entry(user.username.clone())
                .or_insert_with(|| UserConnectionData::new(0, 1, 0));
            data.increment_kind(ConnectionKind::Normal);
            data.add_session(UserSession {
                token: "tok-div-4".to_string(),
                transition_version: 1,
                virtual_id: 9004,
                provider: "provider-a".intern(),
                stream_url: "http://localhost/stream.ts".intern(),
                addr,
                socket_bound: false,
                active_addrs: vec![addr],
                ts: current_time_secs(),
                started_at: current_time_secs(),
                permission: UserConnectionPermission::Allowed,
                connection_kind: None,
                lifecycle: PlaybackLifecycle::Prepared,
            });
        }

        let connections = manager.connections.read().await;
        let data = connections.by_key.get(&user.username).expect("user connection data");
        let snapshot = ActiveUserManager::collect_divergence_snapshot(data, &user.username);
        drop(connections);
        manager.log_divergence_snapshot(snapshot).await;
        let connections = manager.connections.read().await;
        let data = connections.by_key.get(&user.username).expect("user connection data");
        let snapshot = ActiveUserManager::collect_divergence_snapshot(data, &user.username);
        drop(connections);
        manager.log_divergence_snapshot(snapshot).await;
        let connections = manager.connections.read().await;
        let data = connections.by_key.get(&user.username).expect("user connection data");
        let snapshot = ActiveUserManager::collect_divergence_snapshot(data, &user.username);
        drop(connections);
        manager.log_divergence_snapshot(snapshot).await;
    }

}

//
// mod tests {
//     use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
//     use std::time::Instant;
//     use std::thread;
//
//     fn benchmark(ordering: Ordering, iterations: usize) -> u128 {
//         let counter = Arc::new(AtomicUsize::new(0));
//         let start = Instant::now();
//
//         let handles: Vec<_> = (0..32)
//             .map(|_| {
//                 let counter_ref = Arc::clone(&counter);
//                 thread::spawn(move || {
//                     for _ in 0..iterations {
//                         counter_ref.fetch_add(1, ordering);
//                     }
//                 })
//             })
//             .collect();
//

//         for handle in handles {
//             handle.join().unwrap();
//         }
//
//         let duration = start.elapsed();
//         duration.as_millis()
//     }
//
//     #[test]
//     fn test_ordering() {
//         let iterations = 1_000_000;
//
//         let time_acqrel = benchmark(Ordering::SeqCst, iterations);
//         println!("AcqRel: {} ms", time_acqrel);
//
//         let time_seqcst = benchmark(Ordering::SeqCst, iterations);
//         println!("SeqCst: {} ms", time_seqcst);
//     }
//
// }
