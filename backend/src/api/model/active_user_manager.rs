use crate::{
    api::model::{CustomVideoStreamType, EventManager, EventMessage},
    auth::Fingerprint,
    model::{Config, ProxyUserCredentials},
    utils::{debug_if_enabled, GeoIp},
};
use arc_swap::ArcSwapOption;
use jsonwebtoken::get_current_timestamp;
use log::{debug, info};
use shared::{
    model::{ActiveUserConnectionChange, StreamChannel, StreamInfo, StreamTechnicalInfo, UserConnectionPermission, VirtualId},
    utils::{
        current_time_secs, default_grace_period_millis, default_grace_period_timeout_secs,
        default_hls_session_ttl_secs, sanitize_sensitive_info, strip_port, Internable,
    },
};
use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Mutex, Notify, RwLock};
use crate::api::model::connection_manager::CleanupEvent;
use tokio_util::sync::CancellationToken;

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

#[derive(Clone, Debug)]
pub struct UserSession {
    pub token: String,
    pub virtual_id: u32,
    pub provider: Arc<str>,
    pub stream_url: Arc<str>,
    pub addr: SocketAddr,
    pub ts: u64,
    pub permission: UserConnectionPermission,
}

#[derive(Debug)]
struct UserConnectionData {
    max_connections: u32,
    connections: u32,
    granted_grace: bool,
    grace_ts: u64,
    sessions: Vec<UserSession>,
    streams: Vec<StreamInfo>,
    ts: u64,
}

impl UserConnectionData {
    fn new(connections: u32, max_connections: u32) -> Self {
        Self {
            max_connections,
            connections,
            granted_grace: false,
            grace_ts: 0,
            sessions: Vec::new(),
            streams: Vec::new(),
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
}

#[derive(Debug, Default)]
struct UserConnections {
    kicked: HashMap<String, (u64, VirtualId)>,
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
}

pub struct ActiveUserConnectionParams<'a> {
    pub uid: u32,
    pub meter_uid: u32,
    pub username: &'a str,
    pub max_connections: u32,
    pub fingerprint: &'a Fingerprint,
    pub provider: &'a str,
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
        }
    }

    pub(crate) fn set_cleanup_sender(&self, tx: mpsc::Sender<CleanupEvent>) {
        let _ = self.cleanup_tx.set(tx);
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

    /// Releases an active stream for the given socket address without removing the
    /// socket registration (`key_by_addr`). This is used when a stream ends while
    /// the underlying HTTP connection may still remain open.
    pub async fn release_stream(&self, addr: &SocketAddr) -> Option<StreamInfo> {
        let (removed_stream, username, expiry_entry, connection_changed) = {
            let mut user_connections = self.connections.write().await;

            let username = user_connections.key_by_addr.get(addr).map(|reg| reg.username.clone())?;

            let mut removed_stream = None;
            let mut expiry_entry = None;
            let mut connection_changed = false;
            if let Some(connection_data) = user_connections.by_key.get_mut(&username) {
                if let Some(stream_idx) = connection_data
                    .streams
                    .iter()
                    .position(|stream| stream.addr == *addr && !stream.preserved)
                {
                    if Self::should_preserve_session_stream(&connection_data.streams[stream_idx]) {
                        if let Some(entry) = self.build_preserved_stream_expiry(
                            &username,
                            &connection_data.streams[stream_idx],
                            &connection_data.sessions,
                        ) {
                            connection_data.streams[stream_idx].preserved = true;
                            expiry_entry = Some(entry);
                        } else {
                            removed_stream = Some(connection_data.streams.swap_remove(stream_idx));
                        }
                    } else {
                        removed_stream = Some(connection_data.streams.swap_remove(stream_idx));
                    }
                    if connection_data.connections > 0 {
                        connection_data.connections -= 1;
                        connection_changed = true;
                    }
                    if connection_data.connections < connection_data.max_connections {
                        connection_data.granted_grace = false;
                        connection_data.grace_ts = 0;
                    }
                }
            }
            (removed_stream, username, expiry_entry, connection_changed)
        };

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

        removed_stream
    }

    pub async fn release_connection(&self, addr: &SocketAddr) -> ReleasedConnection {
        let (addr_removed, disconnected_user, removed_streams, expiry_entries) = {
            let mut user_connections = self.connections.write().await;

            if let Some(registration) = user_connections.key_by_addr.remove(addr) {
                let username = registration.username;
                let mut removed_streams = Vec::new();
                let mut expiry_entries = Vec::new();
                if let Some(connection_data) = user_connections.by_key.get_mut(&username) {
                    let mut remaining_streams = Vec::with_capacity(connection_data.streams.len());
                    let mut released_count = 0_u32;
                    for mut stream_info in connection_data.streams.drain(..) {
                        if stream_info.addr == *addr {
                            if Self::should_preserve_session_stream(&stream_info) {
                                if let Some(entry) =
                                    self.build_preserved_stream_expiry(&username, &stream_info, &connection_data.sessions)
                                {
                                    if !stream_info.preserved {
                                        released_count = released_count.saturating_add(1);
                                    }
                                    stream_info.preserved = true;
                                    expiry_entries.push(entry);
                                    remaining_streams.push(stream_info);
                                } else {
                                    if !stream_info.preserved {
                                        released_count = released_count.saturating_add(1);
                                    }
                                    removed_streams.push(stream_info);
                                }
                            } else {
                                if !stream_info.preserved {
                                    released_count = released_count.saturating_add(1);
                                }
                                removed_streams.push(stream_info);
                            }
                        } else {
                            remaining_streams.push(stream_info);
                        }
                    }
                    connection_data.streams = remaining_streams;
                    if released_count > 0 && connection_data.connections > 0 {
                        connection_data.connections = connection_data.connections.saturating_sub(released_count);
                    }

                    if connection_data.connections < connection_data.max_connections {
                        connection_data.granted_grace = false;
                        connection_data.grace_ts = 0;
                    }
                }
                (true, Some(username), removed_streams, expiry_entries)
            } else {
                (false, None, Vec::new(), Vec::new())
            }
        };

        for entry in expiry_entries {
            self.enqueue_adaptive_expiry(entry).await;
        }

        if let Some(username) = disconnected_user {
            if !username.is_empty() {
                debug_if_enabled!(
                    "Released connection for user {username} at {}",
                    sanitize_sensitive_info(&addr.to_string())
                );
            }
        }

        if addr_removed {
            self.log_active_user().await;
        }

        ReleasedConnection { addr_removed, removed_streams }
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

    fn check_connection_permission(
        &self,
        username: &str,
        connection_data: &mut UserConnectionData,
    ) -> UserConnectionPermission {
        let current_connections = connection_data.connections;

        if current_connections < connection_data.max_connections {
            // Reset grace period because the user is back under max_connections
            connection_data.granted_grace = false;
            connection_data.grace_ts = 0;
            return UserConnectionPermission::Allowed;
        }

        let now = get_current_timestamp();
        // Check if user already used a grace period
        if connection_data.granted_grace {
            if current_connections > connection_data.max_connections
                && now - connection_data.grace_ts <= self.grace_period_timeout_secs.load(Ordering::Relaxed)
            {
                // Grace timeout, still active, deny connection
                debug!("User access denied, grace exhausted, too many connections: {username}");
                return UserConnectionPermission::Exhausted;
            }
            // Grace timeout expired, reset grace counters
            connection_data.granted_grace = false;
            connection_data.grace_ts = 0;
        }

        if self.grace_period_millis.load(Ordering::Relaxed) > 0
            && current_connections == connection_data.max_connections
        {
            // Intentional asymmetry: grace is granted when current == max (AT limit), while
            // Exhausted is returned when current > max (OVER limit after the grace window).
            // This allows exactly one extra connection during the grace window — the new
            // connection is accepted now, and a background check evicts it if the count is
            // still over max after the grace period elapses.
            connection_data.granted_grace = true;
            connection_data.grace_ts = now;
            debug!("Granted a grace period for user access: {username}");
            return UserConnectionPermission::GracePeriod;
        }

        // Too many connections, no grace allowed
        debug!("User access denied, too many connections: {username}");
        UserConnectionPermission::Exhausted
    }

    pub async fn connection_permission(&self, username: &str, max_connections: u32) -> UserConnectionPermission {
        if max_connections > 0 {
            if let Some(connection_data) = self.connections.write().await.by_key.get_mut(username) {
                return self.check_connection_permission(username, connection_data);
            }
        }
        UserConnectionPermission::Allowed
    }

    pub async fn connection_permission_for_session(
        &self,
        username: &str,
        max_connections: u32,
        session_token: &str,
    ) -> UserConnectionPermission {
        if max_connections == 0 {
            return UserConnectionPermission::Allowed;
        }

        {
            let connections = self.connections.read().await;
            let Some(connection_data) = connections.by_key.get(username) else {
                return UserConnectionPermission::Allowed;
            };

            if connection_data
                .streams
                .iter()
                .any(|stream| stream.session_token.as_deref() == Some(session_token))
            {
                return UserConnectionPermission::Allowed;
            }
        }

        let mut connections = self.connections.write().await;
        let Some(connection_data) = connections.by_key.get_mut(username) else {
            return UserConnectionPermission::Allowed;
        };

        if connection_data
            .streams
            .iter()
            .any(|stream| stream.session_token.as_deref() == Some(session_token))
        {
            return UserConnectionPermission::Allowed;
        }

        self.check_connection_permission(username, connection_data)
    }

    pub async fn active_users_and_connections(&self) -> (usize, usize) {
        self.gc();
        let user_connections = self.connections.read().await;
        user_connections
            .by_key
            .values()
            .filter(|c| c.connections > 0)
            .fold((0usize, 0usize), |(user_count, conn_count), c| (user_count + 1, conn_count + c.connections as usize))
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
                    stream.provider = String::from("tuliprox");
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

    pub async fn update_connection(&self, update: ActiveUserConnectionParams<'_>) -> Option<StreamInfo> {
        let ActiveUserConnectionParams {
            uid,
            meter_uid,
            username,
            max_connections,
            fingerprint,
            provider,
            stream_channel,
            user_agent,
            session_token,
        } = update;
        let stream_info = {
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

            let connection_data = user_connections
                .by_key
                .entry(username.to_string())
                .or_insert_with(|| UserConnectionData::new(0, max_connections));
            connection_data.max_connections = max_connections;

            let user_agent_string = user_agent.to_string();

            let existing_stream_info = connection_data
                .streams
                .iter_mut()
                .find(|stream_info| {
                    match session_token {
                        Some(token) => stream_info.session_token.as_deref() == Some(token),
                        None => stream_info.addr == fingerprint.addr && stream_info.session_token.is_none(),
                    }
                })
                .map(|stream_info| {
                    let client_ip = fingerprint.client_ip.clone();
                    let preserve_started_at = stream_info.session_token.is_some()
                        && (stream_info.channel.item_type.is_live_adaptive() || stream_channel.item_type.is_live_adaptive());
                    let was_preserved = stream_info.preserved;
                    stream_info.meter_uid = meter_uid;
                    stream_info.addr = fingerprint.addr;
                    stream_info.client_ip.clone_from(&client_ip);
                    stream_info.country_code = self.lookup_country(&client_ip);
                    stream_info.channel = stream_channel.clone();
                    stream_info.provider = provider.to_string();
                    stream_info.user_agent.clone_from(&user_agent_string);
                    if !preserve_started_at {
                        stream_info.ts = current_time_secs();
                    }

                    if let Some(token) = session_token {
                        stream_info.session_token = Some(token.to_string());
                    }
                    if was_preserved {
                        stream_info.preserved = false;
                        connection_data.connections = connection_data.connections.saturating_add(1);
                    }
                    stream_info.clone()
                });

            if let Some(stream_info) = existing_stream_info {
                stream_info
            } else {
                let country_code = self.lookup_country(&fingerprint.client_ip);

                let stream_info = StreamInfo::new(
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

                let tracked_socket_count = user_connections.key_by_addr.len();

                if let Some(connection_data) = user_connections.by_key.get_mut(username) {
                    connection_data.connections += 1;
                    connection_data.streams.push(stream_info.clone());
                    Self::log_connection_added(username, &fingerprint.addr, connection_data, tracked_socket_count);
                }

                stream_info
            }
        };

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

    fn new_user_session(
        session_token: &str,
        virtual_id: u32,
        provider: &str,
        stream_url: &str,
        addr: &SocketAddr,
        connection_permission: UserConnectionPermission,
    ) -> UserSession {
        UserSession {
            token: session_token.to_string(),
            virtual_id,
            provider: provider.intern(),
            stream_url: stream_url.intern(),
            addr: *addr,
            ts: current_time_secs(),
            permission: connection_permission,
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
        } = request;
        self.gc();

        let username = user.username.clone();
        let mut user_connections = self.connections.write().await;
        let connection_data = user_connections.by_key.entry(username.clone()).or_insert_with(|| {
            debug_if_enabled!("Creating first session for user {username} {}", sanitize_sensitive_info(stream_url));
            let mut data = UserConnectionData::new(0, user.max_connections);
            let session =
                Self::new_user_session(session_token, virtual_id, provider, stream_url, addr, connection_permission);
            data.add_session(session);
            data
        });

        // If a session exists, update it
        for session in &mut connection_data.sessions {
            if session.token == session_token {
                session.ts = current_time_secs();
                session.addr = *addr;
                if &*session.stream_url != stream_url {
                    session.stream_url = stream_url.intern();
                }
                if &*session.provider != provider {
                    session.provider = provider.intern();
                }
                session.permission = connection_permission;
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
        let session =
            Self::new_user_session(session_token, virtual_id, provider, stream_url, addr, connection_permission);
        let token = session.token.clone();
        connection_data.add_session(session);
        token
    }

    pub async fn update_session_addr(&self, username: &str, token: &str, addr: &SocketAddr) {
        let now = current_time_secs();
        let mut user_connections = self.connections.write().await;
        if let Some(connection_data) = user_connections.by_key.get_mut(username) {
            let update_result = if let Some(session) = connection_data.sessions.iter_mut().find(|s| s.token == token) {
                let previous_addr = session.addr;
                session.addr = *addr;
                session.ts = now;
                for stream in &mut connection_data.streams {
                    if stream.addr == previous_addr {
                        stream.addr = *addr;
                        stream.ts = now;
                    }
                }
                let prune_previous_registration = previous_addr != *addr
                    && !connection_data.sessions.iter().any(|active_session| active_session.addr == previous_addr)
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
                session.addr = *addr;
                touched_session = true;
                break;
            }
        }

        if !touched_session {
            return;
        }

        for stream in &mut connection_data.streams {
            if stream.session_token.as_deref() == Some(token) || stream.addr == *addr {
                stream.ts = now;
                if stream.session_token.as_deref() == Some(token) {
                    stream.addr = *addr;
                }
            }
        }
    }

    pub async fn get_and_update_user_session(&self, username: &str, token: &str) -> Option<UserSession> {
        self.update_user_session(username, token).await
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
        {
            let new_permission = self.check_connection_permission(username, connection_data);
            connection_data.sessions[session_index].permission = new_permission;
        }

        Some(connection_data.sessions[session_index].clone())
    }

    pub async fn active_streams(&self) -> Vec<StreamInfo> {
        self.gc();
        let user_connections = self.connections.read().await;
        let mut streams = Vec::new();
        for connection_data in user_connections.by_key.values() {
            for stream in &connection_data.streams {
                streams.push(stream.clone());
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

    pub async fn get_username_for_addr(&self, addr: &SocketAddr) -> Option<String> {
        self.connections
            .read()
            .await
            .key_by_addr
            .get(addr)
            .map(|registration| registration.username.clone())
    }

    fn should_preserve_session_stream(stream: &StreamInfo) -> bool {
        stream.session_token.is_some() && stream.channel.item_type.is_live_adaptive()
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

        let mut expiry_index = self.adaptive_expiry_index.lock().await;
        let mut user_connections = self.connections.write().await;
        for entry in due_entries {
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
                        // Clone the stream before removal so the cleanup event
                        // can be sent reliably. Only remove state after the
                        // event is delivered to avoid silent history loss.
                        if let Some(tx) = self.cleanup_tx.get() {
                            let stream_clone = Box::new(connection_data.streams[stream_idx].clone());
                            match tx.send(CleanupEvent::AdaptiveSessionExpired {
                                stream_info: stream_clone,
                            }).await {
                                Ok(()) => {}
                                Err(_) => {
                                    debug!("Cleanup channel closed, dropping adaptive session expiry");
                                }
                            }
                        }
                        connection_data.streams.swap_remove(stream_idx);
                        expiry_index.remove(&key);
                    } else if let Some(replacement_entry) = self.build_preserved_stream_expiry(
                        &entry.username,
                        &connection_data.streams[stream_idx],
                        &connection_data.sessions,
                    ) {
                        if replacement_entry.expires_at != current_expires_at {
                            expiry_index.insert(key.clone(), replacement_entry.expires_at);
                            self.adaptive_expiry_queue.lock().await.push(Reverse(replacement_entry));
                            self.adaptive_expiry_notify.notify_one();
                        }
                    }
                }

                remove_user = connection_data.connections == 0
                    && connection_data.streams.is_empty()
                    && connection_data.sessions.is_empty();
            }

            if remove_user {
                user_connections.by_key.remove(&entry.username);
            }
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
                    user_connections.by_key.retain(|_k, v| {
                        v.connections > 0 || !v.streams.is_empty() || now.saturating_sub(v.ts) < USER_CON_TTL
                    });
                    for connection_data in user_connections.by_key.values_mut() {
                        connection_data.sessions.retain(|s| now.saturating_sub(s.ts) < USER_CON_TTL);
                    }
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
        model::{PlaylistItemType, StreamChannel, XtreamCluster},
        utils::Internable,
    };
    use std::{borrow::Cow, sync::Arc};

    fn test_channel(virtual_id: u32) -> StreamChannel {
        StreamChannel {
            target_id: 1,
            virtual_id,
            provider_id: 1,
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            group: "group".intern(),
            title: "title".intern(),
            url: "http://localhost/stream.ts".intern(),
            shared: false,
            technical: None,
        }
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
                uid: 0,
                meter_uid: 0,
                username,
                max_connections: 1,
                fingerprint: &fingerprint,
                provider: "provider-a",
                stream_channel: &test_channel(1001),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-1"),
            })
            .await;
        assert!(first.is_some());
        assert_eq!(manager.user_connections(username).await, 1);
        assert_eq!(
            manager.connection_permission(username, 1).await,
            UserConnectionPermission::GracePeriod
        );

        let second = manager
            .update_connection(ActiveUserConnectionParams {
                uid: 0,
                meter_uid: 0,
                username,
                max_connections: 1,
                fingerprint: &fingerprint,
                provider: "provider-b",
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
    async fn test_same_session_token_on_new_addr_reuses_logical_connection() {
        let config = Config::default();
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let event_manager = Arc::new(EventManager::new());
        let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

        let first_addr: SocketAddr = "127.0.0.1:55021".parse().unwrap();
        let second_addr: SocketAddr = "127.0.0.1:55022".parse().unwrap();
        let first = Fingerprint::new("fp-key-1".to_string(), "127.0.0.1".to_string(), first_addr);
        let second = Fingerprint::new("fp-key-2".to_string(), "127.0.0.1".to_string(), second_addr);

        manager.add_connection(&first_addr).await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 0,
                meter_uid: 0,
                username: "user1",
                max_connections: 1,
                fingerprint: &first,
                provider: "provider-a",
                stream_channel: &test_channel(2001),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-hls"),
            })
            .await;

        assert_eq!(
            manager.connection_permission_for_session("user1", 1, "tok-hls").await,
            UserConnectionPermission::Allowed
        );

        manager.add_connection(&second_addr).await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 0,
                meter_uid: 0,
                username: "user1",
                max_connections: 1,
                fingerprint: &second,
                provider: "provider-a",
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
                fingerprint: &fingerprint,
                provider: "provider-a",
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
                fingerprint: &fingerprint,
                provider: "provider-b",
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
        assert_eq!(streams[0].provider, "provider-b");
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
            })
            .await;
        let first = manager
            .update_connection(ActiveUserConnectionParams {
                uid: 44,
                meter_uid: 144,
                username: "user1",
                max_connections: 1,
                fingerprint: &fingerprint,
                provider: "provider-a",
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

        let streams = manager.active_streams().await;
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].uid, 44);
        assert_eq!(streams[0].ts, first.ts);
        assert!(streams[0].preserved);

        manager.add_connection(&next_addr).await;
        let second = manager
            .update_connection(ActiveUserConnectionParams {
                uid: 55,
                meter_uid: 155,
                username: "user1",
                max_connections: 1,
                fingerprint: &next_fingerprint,
                provider: "provider-b",
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
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 66,
                meter_uid: 166,
                username: "user1",
                max_connections: 1,
                fingerprint: &fingerprint,
                provider: "provider-a",
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
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 77,
                meter_uid: 177,
                username: "user1",
                max_connections: 1,
                fingerprint: &fingerprint,
                provider: "provider-a",
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
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 88,
                meter_uid: 188,
                username: "user1",
                max_connections: 1,
                fingerprint: &fp_a,
                provider: "provider-a",
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
                fingerprint: &fp_b,
                provider: "provider-b",
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
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 111,
                meter_uid: 211,
                username: "user1",
                max_connections: 1,
                fingerprint: &fingerprint,
                provider: "provider-a",
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

        let event = events.recv().await.unwrap();
        assert_eq!(event, EventMessage::ActiveUser(ActiveUserConnectionChange::Connections(0, 0)));
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
                fingerprint: &fingerprint,
                provider: "provider-a",
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
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 133,
                meter_uid: 233,
                username: "user1",
                max_connections: 1,
                fingerprint: &fingerprint,
                provider: "provider-a",
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
        assert_eq!(manager.active_streams().await.len(), 1);
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
                fingerprint: &stale_fp,
                provider: "provider-a",
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
                fingerprint: &fresh_fp,
                provider: "provider-b",
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
    async fn update_session_addr_prunes_previous_registration_when_session_moves_to_new_socket() {
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
                stream_url: "http://localhost/movie.mkv",
                addr: &old_addr,
                connection_permission: UserConnectionPermission::Allowed,
            })
            .await;
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 301,
                meter_uid: 401,
                username: "user1",
                max_connections: 1,
                fingerprint: &old_fingerprint,
                provider: "provider-a",
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::Video,
                    ..test_channel(9101)
                },
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-move"),
            })
            .await
            .expect("initial movie stream should register");

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
                fingerprint: &fingerprint,
                provider: "provider-a",
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
