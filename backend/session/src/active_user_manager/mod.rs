use crate::{
    active_provider_manager::ConnectionKind, connection_manager::CleanupEvent, ActiveProviderManager, EventManager,
};
use arc_swap::ArcSwapOption;
use jsonwebtoken::get_current_timestamp;
use log::{debug, info, log_enabled};
use lru::LruCache;
use shared::{
    defaults::{
        default_grace_period_millis, default_grace_period_timeout_secs, default_hls_session_ttl_secs, DASH_EXT, HLS_EXT,
    },
    model::{
        ActiveUserConnectionChange, CustomVideoStreamType, EventMessage, PlaylistItemType, StreamChannel, StreamInfo,
        StreamTechnicalInfo, UserConnectionPermission, VirtualId,
    },
    utils::{
        current_time_secs, extract_extension_from_url, is_catchup_session_token, sanitize_sensitive_info, strip_port,
        Internable,
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
use tokio::sync::{mpsc, Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    model::{Config, Fingerprint, ProxyUserCredentials},
    utils::{debug_if_enabled, utc_day_from_secs},
};
use tuliprox_repository::GeoIp;

/// Capacity of the per-user divergence cache. A constant so the conversion
/// cannot fail at runtime.
const DIVERGENCE_CACHE_CAPACITY: NonZeroUsize = NonZeroUsize::new(256).unwrap();

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

fn stream_history_session_id(ts: u64, uid: u32) -> u64 { (ts << 32) | u64::from(uid) }

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
    PendingProvider {
        data: PendingProviderState,
    },
    Active,
    /// Provisional counted state for `GraceMode::Instant`. Counts against limits immediately
    /// while the grace window resolves (success -> Active, failure -> Expired).
    GraceActive,
    Preserved,
    Expired,
}

impl PlaybackLifecycle {
    /// Returns true for lifecycle states that own a counted admission lease.
    /// Both `Active` and `GraceActive` count — `GraceActive` is a provisional
    /// counted state for `GraceMode::Instant` sessions.
    pub fn is_counted(&self) -> bool { matches!(self, Self::Active | Self::GraceActive) }
}

#[derive(Clone, Debug)]
pub struct UserSession {
    pub token: String,
    pub transition_version: u64,
    pub virtual_id: u32,
    pub provider: Arc<str>,
    pub stream_url: Arc<str>,
    pub provider_session_headers: HashMap<String, String>,
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
pub struct ConnectionAdmission {
    pub permission: UserConnectionPermission,
    pub kind: Option<ConnectionKind>,
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
        self.sessions.iter().any(|session| session.addr == *addr || session.active_addrs.contains(addr))
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

    fn remove_streams_for_session_and_release_counted(
        &mut self,
        session_token: &str,
        counted_kind: Option<ConnectionKind>,
    ) -> (u32, bool) {
        let mut removed_count = 0;
        let mut connection_changed = false;
        let mut released_stream_kind = false;
        let mut stream_idx = 0;
        while stream_idx < self.streams.len() {
            if self.streams[stream_idx].session_token.as_deref() != Some(session_token) {
                stream_idx += 1;
                continue;
            }

            let uid = self.streams[stream_idx].uid;
            if let Some(kind) = self.stream_kinds.remove(&uid) {
                self.decrement_kind(kind);
                released_stream_kind = true;
                connection_changed = true;
            }
            self.stream_normal_priorities.remove(&uid);
            self.streams.swap_remove(stream_idx);
            removed_count += 1;
        }

        if let Some(kind) = counted_kind.filter(|_| !released_stream_kind) {
            self.decrement_kind(kind);
            connection_changed = true;
        }

        (removed_count, connection_changed)
    }

    fn try_promote_soft_stream(&mut self) -> Option<PromotionAction> {
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

    fn try_promote_soft_session_reservation(&mut self) -> bool {
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

fn uses_session_reentry_guard(stream: &StreamInfo) -> bool {
    stream.channel.item_type.requires_provider_affinity()
        || matches!(
            extract_extension_from_url(stream.channel.url.as_ref()),
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
    pub disconnected_users: Vec<String>,
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
    fn anonymous() -> Self { Self { username: String::new(), ts: current_time_secs() } }
}

struct UserSessionParams<'a> {
    session_token: &'a str,
    virtual_id: u32,
    provider: &'a str,
    stream_url: &'a str,
    addr: &'a SocketAddr,
    connection_permission: UserConnectionPermission,
    connection_kind: Option<ConnectionKind>,
    socket_bound: bool,
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
    pub dropped_cleanup_events: AtomicU64,
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
    pub fn shutdown(&self) { self.adaptive_expiry_cancel.cancel(); }

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
        (*geoip).as_ref().and_then(|geoip_db| geoip_db.lookup(&strip_port(client_ip)))
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

    /// The bus this manager publishes on.
    ///
    /// Exposed so the admission path can report a refusal without
    /// `AdmissionCtx` growing a second handle to the same manager.
    #[must_use]
    pub fn events(&self) -> &Arc<EventManager> { &self.event_manager }

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
            divergence_cache: Mutex::new(LruCache::new(DIVERGENCE_CACHE_CAPACITY)),
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

    fn admission_gate_key(username: &str) -> String {
        let mut key = String::with_capacity(username.len() + 11);
        key.push_str("admission");
        key.push('\0');
        key.push_str(username);
        key
    }

    fn cleanup_idle_transition_gates(transition_gates: &mut HashMap<String, Arc<Mutex<()>>>) {
        transition_gates.retain(|_, gate| Arc::strong_count(gate) > 1);
    }

    pub async fn acquire_playback_transition(&self, username: &str, token: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let key = Self::transition_gate_key(username, token);
        let gate = {
            let mut transition_gates = self.transition_gates.lock().await;
            Self::cleanup_idle_transition_gates(&mut transition_gates);
            Arc::clone(transition_gates.entry(key).or_insert_with(|| Arc::new(Mutex::new(()))))
        };
        gate.lock_owned().await
    }

    pub async fn acquire_user_admission(&self, username: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let key = Self::admission_gate_key(username);
        let gate = {
            let mut transition_gates = self.transition_gates.lock().await;
            Self::cleanup_idle_transition_gates(&mut transition_gates);
            Arc::clone(transition_gates.entry(key).or_insert_with(|| Arc::new(Mutex::new(()))))
        };
        gate.lock_owned().await
    }

    fn should_reuse_stream_for_session(existing_stream: &StreamInfo, incoming_channel: &StreamChannel) -> bool {
        existing_stream.channel.item_type.requires_provider_affinity()
            || incoming_channel.item_type.requires_provider_affinity()
    }

    pub fn set_cleanup_sender(&self, tx: mpsc::Sender<CleanupEvent>) { let _ = self.cleanup_tx.set(tx); }

    pub fn set_provider_manager(&self, provider_manager: Arc<ActiveProviderManager>) {
        let _ = self.provider_manager.set(provider_manager);
    }

    /// Collect a snapshot of all currently active streams for shutdown history recording.
    pub async fn get_all_active_streams(&self) -> Vec<shared::model::StreamInfo> {
        let connections = self.connections.read().await;
        connections
            .by_key
            .values()
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

    async fn emit_promotion_update(&self, username: &str, action: PromotionAction) {
        if let Some(provider_manager) = self.provider_manager.get() {
            provider_manager.reclassify_connection(&action.addr, ConnectionKind::Normal, action.new_priority).await;
        }

        let maybe_stream = {
            let user_connections = self.connections.read().await;
            user_connections.by_key.get(username).and_then(|connection_data| {
                connection_data.streams.iter().find(|stream| stream.uid == action.uid).cloned()
            })
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
        self.release_stream_inner(addr, None).await
    }

    #[allow(clippy::too_many_lines)]
    pub async fn release_stream_by_uid(&self, addr: &SocketAddr, stream_uid: u32) -> Option<StreamInfo> {
        self.release_stream_inner(addr, Some(stream_uid)).await
    }

    #[allow(clippy::too_many_lines)]
    async fn release_stream_inner(&self, addr: &SocketAddr, stream_uid: Option<u32>) -> Option<StreamInfo> {
        let (
            removed_stream,
            username,
            expiry_entry,
            preserved_update,
            connection_changed,
            promotion,
            divergence_snapshot,
        ) = {
            let mut user_connections = self.connections.write().await;

            let username = match stream_uid {
                Some(uid) => user_connections.by_key.iter().find_map(|(username, connection_data)| {
                    connection_data
                        .streams
                        .iter()
                        .any(|stream| !stream.preserved && stream.uid == uid && stream.addr == *addr)
                        .then(|| username.clone())
                }),
                None => user_connections
                    .key_by_addr
                    .get(addr)
                    .filter(|reg| !reg.username.is_empty())
                    .map(|reg| reg.username.clone())
                    .or_else(|| {
                        user_connections.by_key.iter().find_map(|(username, connection_data)| {
                            connection_data
                                .streams
                                .iter()
                                .any(|stream| !stream.preserved && stream.addr == *addr)
                                .then(|| username.clone())
                        })
                    }),
            }?;

            let mut removed_stream = None;
            let mut expiry_entry = None;
            let mut preserved_update = None;
            let mut connection_changed = false;
            let mut promotion = None;
            if let Some(connection_data) = user_connections.by_key.get_mut(&username) {
                let migrated_session_addrs = connection_data.release_addr_from_sessions(addr);
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

        if let Some(stream_info) = preserved_update {
            self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(stream_info)));
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
        let (
            addr_removed,
            connection_count_changed,
            disconnected_users,
            removed_streams,
            expiry_entries,
            preserved_updates,
            promotions,
        ) = {
            let mut user_connections = self.connections.write().await;

            let registration = user_connections.key_by_addr.remove(addr);
            let had_registration = registration.is_some();
            let mut disconnected_users = registration
                .map(|registration| registration.username)
                .filter(|username| !username.is_empty())
                .into_iter()
                .collect::<Vec<_>>();
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
            disconnected_users.sort_unstable();
            disconnected_users.dedup();

            let mut removed_streams = Vec::new();
            let mut expiry_entries = Vec::new();
            let mut preserved_updates = Vec::new();
            let mut promotions = Vec::new();
            let mut connection_count_changed = false;
            for username in &disconnected_users {
                if let Some(connection_data) = user_connections.by_key.get_mut(username) {
                    let previous_connection_count = connection_data.connections;
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

        ReleasedConnection { addr_removed, removed_streams, disconnected_users }
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

    fn check_connection_admission_with_counts(
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
            return ConnectionAdmission { permission: UserConnectionPermission::Allowed, kind: Some(kind) };
        }

        let now = get_current_timestamp();
        // Check if user already used a grace period
        if connection_data.granted_grace {
            if effective_connections >= connection_data.max_connections
                && now - connection_data.grace_ts <= self.grace_period_timeout_secs.load(Ordering::Relaxed)
            {
                // Grace timeout, still active, deny connection
                debug!("User access denied, grace exhausted, too many connections: {username}");
                return ConnectionAdmission { permission: UserConnectionPermission::Exhausted, kind: None };
            }
            // Grace timeout expired, reset grace counters
            if effective_connections < connection_data.max_connections {
                connection_data.granted_grace = false;
                connection_data.grace_ts = 0;
            }
        }

        debug!("User access denied, too many connections: {username}");
        ConnectionAdmission { permission: UserConnectionPermission::Exhausted, kind: None }
    }

    fn check_connection_admission(
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
            if let Some(connection_data) = self.connections.write().await.by_key.get_mut(username) {
                connection_data.max_connections = max_connections;
                connection_data.soft_connections = soft_connections;
                return self.check_connection_admission(username, connection_data);
            }
        }
        ConnectionAdmission { permission: UserConnectionPermission::Allowed, kind: Some(ConnectionKind::Normal) }
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

        let Some(session_index) = connection_data.sessions.iter().position(|session| session.token == session_token)
        else {
            return self.check_connection_admission(username, connection_data);
        };

        if connection_data.sessions[session_index].lifecycle.is_counted() {
            return ConnectionAdmission {
                permission: UserConnectionPermission::Allowed,
                kind: connection_data.sessions[session_index].connection_kind.or(Some(ConnectionKind::Normal)),
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
            let mut connections = self.connections.write().await;
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
                if admission.permission == UserConnectionPermission::Allowed {
                    if let Some(kind) = admission.kind {
                        Self::update_session_admission(
                            &mut connection_data.sessions[session_index],
                            admission.permission,
                            Some(kind),
                        );
                    }
                    admission.kind
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

    pub async fn get_eviction_candidates(&self, username: &str, _client_ip: &str) -> Vec<crate::EvictionCandidate> {
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
                    connection_data.sessions.iter().any(|s| s.token == token && s.lifecycle.is_counted())
                        || stream.preserved
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
            .map(|s| crate::EvictionCandidate { addr: s.addr, client_ip: s.client_ip.clone(), ts: s.ts })
            .collect();
        candidates
    }

    pub async fn grant_grace(&self, username: &str) -> bool {
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
        let (stream_info, divergence_snapshot, connection_count_changed) = {
            let mut user_connections = self.connections.write().await;

            let now = current_time_secs();
            if let Some(registration) = user_connections.key_by_addr.get_mut(&fingerprint.addr) {
                registration.username = username.to_string();
                registration.ts = now;
            } else {
                user_connections
                    .key_by_addr
                    .insert(fingerprint.addr, SocketRegistration { username: username.to_string(), ts: now });
            }

            let tracked_socket_count = user_connections.key_by_addr.len();
            let connection_data = user_connections
                .by_key
                .entry(username.to_string())
                .or_insert_with(|| UserConnectionData::new(0, max_connections, soft_connections));
            connection_data.max_connections = max_connections;
            connection_data.soft_connections = soft_connections;
            let previous_connection_count = connection_data.connections;

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
                    Some(token) => {
                        stream_info.session_token.as_deref() == Some(token)
                            && Self::should_reuse_stream_for_session(stream_info, stream_channel)
                    }
                    None => stream_info.addr == fingerprint.addr && stream_info.session_token.is_none(),
                })
                .map(|stream_idx| {
                    let session_started_at = session_token.and_then(|token| {
                        connection_data.sessions.iter().find(|s| s.token == token).map(|s| s.started_at)
                    });

                    let stream_info = &mut connection_data.streams[stream_idx];
                    let client_ip = fingerprint.client_ip.clone();
                    let preserve_started_at = stream_info.session_token.is_some()
                        && (stream_info.channel.item_type.is_live_adaptive()
                            || stream_channel.item_type.is_live_adaptive());
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
            let (stream_info, divergence_snapshot) = if let Some((stream_info, was_preserved)) = existing_stream_info {
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

                let mut stream_info = StreamInfo::new(shared::model::StreamInfoParams {
                    uid,
                    meter_uid,
                    username,
                    addr: &fingerprint.addr,
                    client_ip: &fingerprint.client_ip,
                    provider,
                    stream_channel: stream_channel.clone(),
                    user_agent: user_agent_string,
                    country_code,
                    session_token,
                });

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
            };
            let connection_count_changed = connection_data.connections != previous_connection_count;
            (stream_info, divergence_snapshot, connection_count_changed)
        };

        self.log_divergence_snapshot(divergence_snapshot).await;

        if connection_count_changed {
            self.log_active_user().await;
        }

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
        // Catchup segment gaps can briefly lose the UserSession row; still preserve the panel
        // row using the stream timestamp so Streams does not blink between archive chunks.
        let session_ts = if let Some(session) = sessions.iter().find(|session| session.token == session_token) {
            session.ts
        } else if stream.channel.item_type == PlaylistItemType::Catchup || is_catchup_session_token(session_token) {
            stream.ts
        } else {
            return None;
        };

        let ttl_secs = self.adaptive_session_ttl_secs.load(Ordering::Relaxed);
        let expires_at = session_ts.saturating_add(ttl_secs);
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

    fn new_user_session(params: &UserSessionParams<'_>) -> UserSession {
        let now = current_time_secs();
        UserSession {
            token: params.session_token.to_string(),
            transition_version: 1,
            virtual_id: params.virtual_id,
            provider: params.provider.intern(),
            stream_url: params.stream_url.intern(),
            provider_session_headers: HashMap::new(),
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

    fn promote_session_for_stream(connection_data: &mut UserConnectionData, stream: &StreamInfo) {
        if let Some(token) = stream.session_token.as_deref() {
            if let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == token) {
                Self::mark_session_committed(session, ConnectionKind::Normal);
            }
        }
    }

    fn collect_promotions_after_capacity_release(connection_data: &mut UserConnectionData) -> Vec<PromotionAction> {
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

    fn promote_counted_soft_session_to_normal_if_available(
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
        connection_data.streams.iter().any(|stream| stream.session_token.as_deref() == Some(session_token))
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

    pub async fn connection_admission_for_session_activation(
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
            return ConnectionAdmission {
                permission: UserConnectionPermission::Allowed,
                kind: connection_data.sessions[session_index].connection_kind.or(Some(ConnectionKind::Normal)),
            };
        }

        // Uncounted session with no stream row: run normal admission.
        // Same-token soft-preserve is handled above via `session_has_stream` (3.3.78 semantics).
        // Do not return Exhausted for own preserved rows — that forced self-eviction on HLS gaps.
        let admission = self.check_connection_admission_with_counts(
            username,
            connection_data,
            connection_data.effective_counts_for_admission(Some(session_token)),
        );
        if admission.permission == UserConnectionPermission::Allowed {
            let session = &mut connection_data.sessions[session_index];
            Self::update_session_admission(session, admission.permission, admission.kind);
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

        let session = Self::new_user_session(&UserSessionParams {
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
                let kind = connection_data.sessions[session_index].connection_kind.unwrap_or(ConnectionKind::Normal);
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

    pub async fn release_session_streams_and_counted_reservation(&self, username: &str, session_token: &str) -> bool {
        let (connection_changed, user_removed, promotions, divergence_snapshot) = {
            let mut user_connections = self.connections.write().await;
            let Some(connection_data) = user_connections.by_key.get_mut(username) else {
                return false;
            };

            let counted_kind = connection_data
                .sessions
                .iter()
                .find(|session| session.token == session_token && session.lifecycle.is_counted())
                .and_then(|session| session.connection_kind);
            let (_removed_streams, mut connection_changed) =
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
        connection_changed || user_removed
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
            let session = Self::new_user_session(&UserSessionParams {
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
                    session.stream_url = stream_url.intern();
                    reset_provider_session_headers = true;
                }
                if &*session.provider != provider {
                    session.provider = provider.intern();
                    reset_provider_session_headers = true;
                }
                if reset_provider_session_headers {
                    session.provider_session_headers.clear();
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
                    user_connections
                        .key_by_addr
                        .insert(*addr, SocketRegistration { username: username.to_string(), ts: now });
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
        if !cleared {
            let can_remove_registration = !connection_data.has_session_addr(addr)
                && !connection_data.streams.iter().any(|stream| stream.addr == *addr);
            if can_remove_registration {
                let can_remove = user_connections
                    .key_by_addr
                    .get(addr)
                    .is_some_and(|registration| registration.username.is_empty() || registration.username == username);
                if can_remove {
                    user_connections.key_by_addr.remove(addr);
                }
            }
            return;
        }

        let can_remove_registration = !connection_data.has_session_addr(addr)
            && !connection_data.streams.iter().any(|stream| stream.addr == *addr);
        if can_remove_registration {
            let can_remove = user_connections
                .key_by_addr
                .get(addr)
                .is_some_and(|registration| registration.username.is_empty() || registration.username == username);
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
    /// - `activate_grace_active` confirms it (grace window succeeded -> `GraceActive -> Active`)
    /// - `expire_grace_active` expires it (grace window failed -> `GraceActive -> Expired`)
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
                if let Some(stream_idx) =
                    connection_data.streams.iter().position(|stream| stream.addr == *addr && !stream.preserved)
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
    pub async fn terminate_session(&self, username: &str, session_token: &str) {
        let (connection_changed, removed_count, promotions) = {
            let mut user_connections = self.connections.write().await;
            let Some(connection_data) = user_connections.by_key.get_mut(username) else {
                return;
            };

            let Some(session_index) =
                connection_data.sessions.iter().position(|session| session.token == session_token)
            else {
                return;
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
    }

    /// Terminates all sessions associated with a given socket address for a user.
    ///
    /// This is used when a connection is explicitly kicked — the session should be
    /// expired and removed immediately rather than waiting for TTL-based GC cleanup.
    ///
    /// Removes all sessions whose `addr` or `active_addrs` contains `kick_addr`.
    pub async fn terminate_sessions_for_addr(&self, username: &str, kick_addr: &SocketAddr) {
        let (connection_changed, removed_count, promotions) = {
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
            Some(connection_data.sessions[session_index].connection_kind.unwrap_or(ConnectionKind::Normal))
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

    pub fn active_socket_ttl_secs(&self) -> u64 {
        let configured_ttl = self.adaptive_session_ttl_secs.load(Ordering::Relaxed);
        if configured_ttl == 0 {
            DEFAULT_ACTIVE_SOCKET_TTL_SECS
        } else {
            configured_ttl
        }
    }

    pub async fn socket_expiry_deadline(&self, addr: &SocketAddr) -> Option<u64> {
        let ttl_secs = self.active_socket_ttl_secs();
        let connections = self.connections.read().await;
        let registration = connections.key_by_addr.get(addr)?;
        if registration.username.is_empty() {
            return None;
        }

        Some(registration.ts.saturating_add(ttl_secs))
    }

    pub async fn touch_socket_activity(&self, addr: &SocketAddr) {
        let now = current_time_secs();
        let mut user_connections = self.connections.write().await;
        let Some(username) = user_connections.key_by_addr.get_mut(addr).and_then(|registration| {
            if registration.username.is_empty() {
                None
            } else {
                registration.ts = now;
                Some(registration.username.clone())
            }
        }) else {
            return;
        };

        if let Some(connection_data) = user_connections.by_key.get_mut(&username) {
            connection_data.ts = now;
        }
    }

    pub async fn touch_http_activity(&self, username: &str, token: &str, addr: &SocketAddr) {
        let now = current_time_secs();
        let mut user_connections = self.connections.write().await;

        let registration = user_connections.key_by_addr.entry(*addr).or_insert_with(SocketRegistration::anonymous);
        registration.username = username.to_string();
        registration.ts = now;

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

    pub async fn get_and_update_user_session(&self, username: &str, token: &str) -> Option<UserSession> {
        self.update_user_session(username, token).await
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

    pub async fn update_session_provider_headers(
        &self,
        username: &str,
        token: &str,
        provider_session_headers: &HashMap<String, String>,
    ) -> bool {
        let mut user_connections = self.connections.write().await;
        let Some(connection_data) = user_connections.by_key.get_mut(username) else {
            return false;
        };
        let Some(session) = connection_data.sessions.iter_mut().find(|session| session.token == token) else {
            return false;
        };
        session.provider_session_headers.clone_from(provider_session_headers);
        session.ts = current_time_secs();
        true
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
            connection_data.sessions.iter().any(|session| session.token == session_token).then_some(username.as_str())
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
        connections.recently_evicted_sessions.retain(|_, protection| protection.expires_at > now);
        connections.recent_socket_reentry_guards.retain(|_, protection| protection.expires_at > now);

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

        let protection = RecentWinnerProtection { protected_addr, expires_at: now + ttl_secs };
        let mut session_tokens = Vec::new();
        let mut socket_guard_keys = Vec::new();

        for stream in connection_data.streams.iter().filter(|stream| stream.addr == *addr) {
            if uses_session_reentry_guard(stream) && stream.session_token.is_some() {
                let Some(session_token) = stream.session_token.clone() else {
                    continue;
                };
                session_tokens.push(session_token);
            } else {
                socket_guard_keys.push(create_socket_reentry_guard_key(
                    &username,
                    &stream.client_ip,
                    shared::model::VirtualId::new(stream.channel.virtual_id),
                ));
            }
        }

        for session_token in session_tokens {
            connections.recently_evicted_sessions.insert(session_token, protection);
        }
        for key in socket_guard_keys {
            connections.recent_socket_reentry_guards.insert(key, protection);
        }
    }

    pub async fn get_username_for_addr(&self, addr: &SocketAddr) -> Option<String> {
        self.connections.read().await.key_by_addr.get(addr).map(|registration| registration.username.clone())
    }

    fn should_preserve_session_stream(stream: &StreamInfo) -> bool {
        stream.session_token.is_some() && is_stable_session_stream(stream)
    }

    fn is_preserved_stream_expired(&self, stream: &StreamInfo, sessions: &[UserSession], now: u64) -> bool {
        if !stream.preserved || !Self::should_preserve_session_stream(stream) {
            return false;
        }

        let ttl_secs = self.adaptive_session_ttl_secs.load(Ordering::Relaxed);
        let Some(session_token) = stream.session_token.as_deref() else {
            return true;
        };

        let session_ts =
            sessions.iter().find(|session| session.token == session_token).map_or(stream.ts, |session| session.ts);

        now.saturating_sub(session_ts) >= ttl_secs
    }

    fn collect_divergence_snapshot(connection_data: &UserConnectionData, username: &str) -> Option<DivergenceSnapshot> {
        log_enabled!(log::Level::Debug).then(|| Self::build_divergence_snapshot(connection_data, username))
    }

    fn build_divergence_snapshot(connection_data: &UserConnectionData, username: &str) -> DivergenceSnapshot {
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

    async fn log_divergence_snapshot(&self, snapshot: Option<DivergenceSnapshot>) {
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
                    let stream_idx_opt = connection_data.streams.iter().position(|stream| {
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
                connections.by_key.get(username).and_then(|data| Self::collect_divergence_snapshot(data, username))
            };
            self.log_divergence_snapshot(snapshot).await;
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
            self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Disconnected(addr)));
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
                    user_connections.recently_evicted_sessions.retain(|_, protection| protection.expires_at > now);
                    user_connections.recent_socket_reentry_guards.retain(|_, protection| protection.expires_at > now);
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
mod tests;
