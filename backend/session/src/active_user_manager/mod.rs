use crate::{
    active_provider_manager::{ConnectionKind, ProviderReleaseSnapshot},
    connection_manager::CleanupEvent,
    ActiveProviderManager, EventManager,
};
use arc_swap::ArcSwapOption;
use lru::LruCache;
use shared::model::{StreamChannel, StreamInfo, UserConnectionPermission};
use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    net::SocketAddr,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{Fingerprint, ProxyUserCredentials};
use tuliprox_repository::GeoIp;

/// Capacity of the per-user divergence cache. A constant so the conversion
/// cannot fail at runtime.
const DIVERGENCE_CACHE_CAPACITY: NonZeroUsize = NonZeroUsize::new(256).unwrap();

const USER_GC_TTL: u64 = 900;
// 15 Min
const USER_CON_TTL: u64 = 1_800;
// 30 minutes
const USER_SESSION_LIMIT: usize = 50;

const ANON_SOCKET_TTL: u64 = 300;
// 5 Min
const DEFAULT_ACTIVE_SOCKET_TTL_SECS: u64 = 90;

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

static NEXT_SESSION_INCARNATION: AtomicU64 = AtomicU64::new(1);

/// The session object and provider binding a finite resource request was authorized for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionIdentity {
    pub incarnation: u64,
    pub binding_generation: u64,
}

/// Notifications collected while the connection write lock is held and delivered after release.
#[derive(Default)]
pub(crate) struct DeferredWakes {
    pending: std::sync::Mutex<Vec<Arc<Notify>>>,
    has_pending: AtomicBool,
}

/// Wakes the in-flight requests of one session when it ends or switches accounts.
///
/// Only the session stored in the connection table owns the signal: dropping it, by any removal
/// path, signals the end. Snapshot clones share the notification but never signal on drop.
pub struct SessionChangeSignal {
    notify: Arc<Notify>,
    deferred: Option<Arc<DeferredWakes>>,
    owner: bool,
}

/// Provider session headers of one playback session for one target URL.
#[derive(Debug, PartialEq, Eq)]
pub enum SessionProviderHeaders {
    /// The session ended, was replaced or switched provider accounts.
    NoSession,
    /// The session is valid but has no headers for the target.
    NoHeaders,
    Headers(HashMap<String, String>),
}

#[derive(Clone, Debug)]
pub struct UserSession {
    pub token: String,
    pub transition_version: u64,
    pub virtual_id: u32,
    pub provider: Arc<str>,
    pub stream_url: Arc<str>,
    pub provider_session_headers: HashMap<String, String>,
    /// Origin (`scheme://host:port`) whose response set `provider_session_headers`; `None` when unknown.
    pub provider_session_headers_host: Option<String>,
    /// Shared copy-on-write so per-request session snapshots do not copy the cookie jar.
    pub provider_session_cookies: Arc<crate::ProviderSessionCookieStore>,
    /// Unique per session object, so a reused token never authorizes requests of an older session.
    pub incarnation: u64,
    /// Bumped on every provider account switch; child URLs belong to the account that issued them.
    pub binding_generation: u64,
    pub change_signal: SessionChangeSignal,
    /// Shared with the response body so media confirmation survives a released VOD lease.
    pub media_started: Arc<AtomicBool>,
    /// Stable suffix appended to upstream User-Agent headers for this playback session.
    pub user_agent_stream_index: Option<u64>,
    pub addr: SocketAddr,
    pub socket_bound: bool,
    pub active_addrs: Vec<SocketAddr>,
    pub ts: u64,
    pub started_at: u64,
    pub permission: UserConnectionPermission,
    pub connection_kind: Option<ConnectionKind>,
    pub lifecycle: PlaybackLifecycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionRejectionReason {
    UserConnectionsExhausted,
    RecentEvictionReentry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionAdmission {
    permission: UserConnectionPermission,
    kind: Option<ConnectionKind>,
    rejection_reason: Option<AdmissionRejectionReason>,
}

#[derive(Debug)]
pub enum StreamRequestDetach {
    NotFound,
    Retained(StreamInfo),
    Preserved(StreamInfo),
    Removed(StreamInfo),
}

/// How long an ended session token blocks its recreation from a sealed playlist token.
const ENDED_SESSION_RECREATE_BLOCK: Duration = Duration::from_secs(600);

pub struct ReleasedConnection {
    pub addr_removed: bool,
    pub removed_streams: Vec<StreamInfo>,
    pub disconnected_users: Vec<String>,
}

/// Identity of the session whose admission authorized a finite resource request.
#[derive(Clone, Copy, Debug)]
pub struct PlaybackSessionRegistration {
    pub identity: SessionIdentity,
    pub enforce_limits: bool,
    pub grace_admitted: bool,
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

pub struct ActiveUserManager {
    grace_period_millis: AtomicU64,
    grace_period_timeout_secs: AtomicU64,
    adaptive_session_ttl_secs: AtomicU64,
    log_active_user: AtomicBool,
    next_user_agent_stream_index: AtomicU64,
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
    // An evicted stream can keep its provider slot after its user count is released.
    // Retain the handoff across bounded admission attempts until the slot is gone.
    pending_provider_releases: Mutex<HashMap<String, ProviderReleaseSnapshot>>,
    pub dropped_cleanup_events: AtomicU64,
    reentry_suppressed_total: AtomicU64,
    divergence_cache: Mutex<LruCache<String, DivergenceEntry>>,
    divergence_cooldown_secs: u64,
    // Session change signals raised under the write lock, delivered once it is released.
    deferred_wakes: Arc<DeferredWakes>,
}

#[cfg(test)]
mod tests;

mod activity;
mod admission;
mod connections;
mod eviction;
mod expiry;
mod observability;
mod provider_headers;
mod requests;
mod session_lifecycle;

#[cfg(test)]
use self::observability::divergence_key;
#[cfg(test)]
use self::observability::DivergenceKind;
pub use self::session_lifecycle::next_session_incarnation;
use self::{
    connections::{
        create_socket_reentry_guard_key, decide_connection_kind, url_host_key, PromotionAction, RecentWinnerProtection,
        SocketRegistration, UserConnectionCounts, UserConnectionData, UserConnections, UserConnectionsWriteGuard,
    },
    expiry::{AdaptiveExpiryEntry, AdaptiveExpiryKey},
    observability::DivergenceEntry,
    requests::StreamRequestClaim,
    session_lifecycle::{
        get_adaptive_session_ttl_secs, release_session_addr, remember_session_addr, stream_history_session_id,
        uses_session_reentry_guard, UserSessionParams,
    },
};
