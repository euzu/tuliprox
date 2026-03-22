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
        current_time_secs, default_grace_period_millis, default_grace_period_timeout_secs, sanitize_sensitive_info,
        strip_port, Internable,
    },
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::RwLock;

const USER_GC_TTL: u64 = 900; // 15 Min
const USER_CON_TTL: u64 = 10_800; // 3 hours
const USER_SESSION_LIMIT: usize = 50;
const ANON_SOCKET_TTL: u64 = 300; // 5 Min

fn get_grace_options(config: &Config) -> (u64, u64) {
    let (grace_period_millis, grace_period_timeout_secs) =
        config.reverse_proxy.as_ref().and_then(|r| r.stream.as_ref()).map_or_else(
            || (default_grace_period_millis(), default_grace_period_timeout_secs()),
            |s| (s.grace_period_millis, s.grace_period_timeout_secs),
        );
    (grace_period_millis, grace_period_timeout_secs)
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
    log_active_user: AtomicBool,
    gc_ts: Option<AtomicU64>,
    connections: RwLock<UserConnections>,
    event_manager: Arc<EventManager>,
    geo_ip: Arc<ArcSwapOption<GeoIp>>,
    last_logged_user_count: AtomicUsize,
    last_logged_user_connection_count: AtomicUsize,
}

impl ActiveUserManager {
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
            log_active_user: AtomicBool::new(log_active_user),
            connections: RwLock::new(UserConnections::default()),
            gc_ts: Some(AtomicU64::new(current_time_secs())),
            geo_ip: Arc::clone(geoip),
            event_manager: Arc::clone(event_manager),
            last_logged_user_count: AtomicUsize::new(0),
            last_logged_user_connection_count: AtomicUsize::new(0),
        }
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
        let (removed_stream, username) = {
            let mut user_connections = self.connections.write().await;

            let username = user_connections.key_by_addr.get(addr).map(|reg| reg.username.clone())?;

            let mut removed_stream = None;
            if let Some(connection_data) = user_connections.by_key.get_mut(&username) {
                if let Some(stream_idx) = connection_data
                    .streams
                    .iter()
                    .position(|stream| stream.addr == *addr && !stream.preserved)
                {
                    if Self::should_preserve_session_stream(&connection_data.streams[stream_idx]) {
                        connection_data.streams[stream_idx].preserved = true;
                    } else {
                        removed_stream = Some(connection_data.streams.swap_remove(stream_idx));
                    }
                    if connection_data.connections > 0 {
                        connection_data.connections -= 1;
                    }
                    if connection_data.connections < connection_data.max_connections {
                        connection_data.granted_grace = false;
                        connection_data.grace_ts = 0;
                    }
                }
            }
            (removed_stream, username)
};

        if removed_stream.is_some() {
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
        let (addr_removed, disconnected_user, removed_streams) = {
            let mut user_connections = self.connections.write().await;

            if let Some(registration) = user_connections.key_by_addr.remove(addr) {
                let username = registration.username;
                let mut removed_streams = Vec::new();
                if let Some(connection_data) = user_connections.by_key.get_mut(&username) {
                    let mut remaining_streams = Vec::with_capacity(connection_data.streams.len());
                    let mut released_count = 0_u32;
                    for mut stream_info in connection_data.streams.drain(..) {
                        if stream_info.addr == *addr {
                            if Self::should_preserve_session_stream(&stream_info) {
                                if !stream_info.preserved {
                                    released_count = released_count.saturating_add(1);
                                }
                                stream_info.preserved = true;
                                remaining_streams.push(stream_info);
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
                (true, Some(username), removed_streams)
            } else {
                (false, None, Vec::new())
            }
        };

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
                    stream_info.country = self.lookup_country(&client_ip);
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
                let country = self.lookup_country(&fingerprint.client_ip);

                let stream_info = StreamInfo::new(
                    uid,
                    meter_uid,
                    username,
                    &fingerprint.addr,
                    &fingerprint.client_ip,
                    provider,
                    stream_channel.clone(),
                    user_agent_string,
                    country,
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
        let mut user_connections = self.connections.write().await;
        if let Some(connection_data) = user_connections.by_key.get_mut(username) {
            if let Some(session) = connection_data.sessions.iter_mut().find(|s| s.token == token) {
                let previous_addr = session.addr;

                session.addr = *addr;
                for stream in &mut connection_data.streams {
                    if stream.addr == previous_addr {
                        stream.addr = *addr;
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
                        .by_key
                        .retain(|_k, v| now.saturating_sub(v.ts) < USER_CON_TTL && v.connections > 0);
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
    use crate::{api::model::EventManager, auth::Fingerprint, model::Config};
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

        manager.add_connection(&addr).await;
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

        manager.add_connection(&addr).await;
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
