use crate::api::model::{CustomVideoStreamType, EventManager, EventMessage};
use crate::auth::Fingerprint;
use crate::model::Config;
use crate::model::ProxyUserCredentials;
use crate::utils::GeoIp;
use arc_swap::ArcSwapOption;
use jsonwebtoken::get_current_timestamp;
use log::{debug, info};
use shared::model::{ActiveUserConnectionChange, StreamChannel, StreamInfo, UserConnectionPermission, VirtualId};
use shared::utils::{current_time_secs, default_grace_period_millis, default_grace_period_timeout_secs, intern, sanitize_sensitive_info, strip_port};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use crate::utils::debug_if_enabled;

const USER_GC_TTL: u64 = 900;  // 15 Min
const USER_CON_TTL: u64 = 10_800;  // 3 hours
const USER_SESSION_LIMIT: usize = 50;

fn get_grace_options(config: &Config) -> (u64, u64) {
    let (grace_period_millis, grace_period_timeout_secs) = config.reverse_proxy.as_ref()
        .and_then(|r| r.stream.as_ref())
        .map_or_else(|| (default_grace_period_millis(), default_grace_period_timeout_secs()), |s| (s.grace_period_millis, s.grace_period_timeout_secs));
    (grace_period_millis, grace_period_timeout_secs)
}

#[derive(Clone, Debug)]
pub struct UserSession {
    pub token: String,
    pub virtual_id: u32,
    pub provider: String,
    pub stream_url: String,
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
    key_by_addr: HashMap<SocketAddr, String>,
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
    pub fn new(config: &Config,
               geoip: &Arc<ArcSwapOption<GeoIp>>,
               event_manager: &Arc<EventManager>, ) -> Self {
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
        let (user_count, user_connection_count) = {
            self.active_users_and_connections().await
        };
        self.event_manager.send_event(EventMessage::ActiveUser(ActiveUserConnectionChange::Connections(user_count, user_connection_count)));
        if is_log_user_enabled {
            let last_user_count = self.last_logged_user_count.load(Ordering::Relaxed);
            let last_connection_count = self.last_logged_user_connection_count.load(Ordering::Relaxed);
            if last_user_count != user_count && last_connection_count != user_connection_count {
                self.last_logged_user_count.store(last_user_count, Ordering::Relaxed);
                self.last_logged_user_connection_count.store(last_connection_count, Ordering::Relaxed);
                info!("Active Users: {user_count}, Active User Connections: {user_connection_count}");
            }
        }
    }

    pub async fn release_connection(&self, addr: &SocketAddr) {
        let (log_active_user, disconnected_user) = {
            let mut user_connections = self.connections.write().await;

            if let Some(username) = user_connections.key_by_addr.remove(addr) {
                if let Some(connection_data) = user_connections.by_key.get_mut(&username) {
                    if connection_data.connections > 0 {
                        connection_data.connections -= 1;
                    }

                    if connection_data.connections < connection_data.max_connections {
                        connection_data.granted_grace = false;
                        connection_data.grace_ts = 0;
                    }
                    connection_data.streams.retain(|c| c.addr != *addr);
                }
                (true, Some(username))
            } else {
                (false, None)
            }
        };

        if let Some(username) = disconnected_user {
            if !username.is_empty() {
                debug_if_enabled!("Released connection for user {username} at {}", sanitize_sensitive_info(&addr.to_string()));
            }
        }

        if log_active_user {
            self.log_active_user().await;
        }
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

    fn check_connection_permission(&self, username: &str, connection_data: &mut UserConnectionData) -> UserConnectionPermission {
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
            if current_connections > connection_data.max_connections && now - connection_data.grace_ts <= self.grace_period_timeout_secs.load(Ordering::Relaxed) {
                // Grace timeout, still active, deny connection
                debug!("User access denied, grace exhausted, too many connections: {username}");
                return UserConnectionPermission::Exhausted;
            }
            // Grace timeout expired, reset grace counters
            connection_data.granted_grace = false;
            connection_data.grace_ts = 0;
        }

        if self.grace_period_millis.load(Ordering::Relaxed) > 0 && current_connections == connection_data.max_connections {
            // Allow a grace period once
            connection_data.granted_grace = true;
            connection_data.grace_ts = now;
            debug!("Granted a grace period for user access: {username}");
            return UserConnectionPermission::GracePeriod;
        }

        // Too many connections, no grace allowed
        debug!("User access denied, too many connections: {username}");
        UserConnectionPermission::Exhausted
    }

    pub async fn connection_permission(
        &self,
        username: &str,
        max_connections: u32,
    ) -> UserConnectionPermission {
        if max_connections > 0 {
            if let Some(connection_data) = self.connections.write().await.by_key.get_mut(username) {
                return self.check_connection_permission(username, connection_data);
            }
        }
        UserConnectionPermission::Allowed
    }

    pub async fn active_users_and_connections(&self) -> (usize, usize) {
        let user_connections = self.connections.read().await;
        user_connections
            .by_key
            .values()
            .filter(|c| c.connections > 0)
            .fold((0usize, 0usize), |(user_count, conn_count), c| {
                (user_count + 1, conn_count + c.connections as usize)
            })
    }

    pub async fn update_stream_detail(&self, addr: &SocketAddr, video_type: CustomVideoStreamType) -> Option<StreamInfo> {
        let mut user_connections = self.connections.write().await;
        let username = {
            match user_connections.key_by_addr.get(addr) {
                Some(username) => username.clone(),
                None => return None,
            }
        };
        if let Some(connection_data) = user_connections.by_key.get_mut(&username) {
            for stream in &mut connection_data.streams {
                if &stream.addr == addr {
                    stream.provider = String::from("tuliprox");
                    stream.channel.title = video_type.to_string();
                    stream.channel.group = intern("");
                    return Some(stream.clone());
                }
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn add_connection(&self, addr: &SocketAddr) {
        let mut user_connections = self.connections.write().await;
        if !user_connections.key_by_addr.contains_key(addr) {
            user_connections.key_by_addr.insert(*addr, String::new());
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_connection(&self, username: &str, max_connections: u32, fingerprint: &Fingerprint,
                                   provider: &str, stream_channel: StreamChannel, user_agent: Cow<'_, str>, session_token: Option<&str>) -> Option<StreamInfo> {
        let stream_info = {
            let mut user_connections = self.connections.write().await;

            // needs to be registered through socket connection to avoid race time conditions through short disconnect
            if !user_connections.key_by_addr.contains_key(&fingerprint.addr) {
                return None;
            }

            user_connections.key_by_addr.insert(fingerprint.addr, username.to_string());

            let connection_data = user_connections.by_key
                .entry(username.to_string())
                .or_insert_with(|| UserConnectionData::new(0, max_connections));
            connection_data.max_connections = max_connections;

            let user_agent_string = user_agent.to_string();

            let existing_stream_info = connection_data
                .streams
                .iter_mut()
                .find(|s| s.addr == fingerprint.addr)
                .map(|stream_info| {
                    stream_info.channel = stream_channel.clone();
                    stream_info.provider = provider.to_string();
                    stream_info.user_agent.clone_from(&user_agent_string);
                    stream_info.ts = current_time_secs();

                    if let Some(token) = session_token {
                        stream_info.session_token = Some(token.to_string());
                    }
                    stream_info.clone()
                });

            if let Some(stream_info) = existing_stream_info { stream_info } else {
                let country = {
                    let geoip = self.geo_ip.load();
                    if let Some(geoip_db) = (*geoip).as_ref() {
                        geoip_db.lookup(&strip_port(&fingerprint.client_ip))
                    } else {
                        None
                    }
                };

                let stream_info = StreamInfo::new(
                    username,
                    &fingerprint.addr,
                    &fingerprint.client_ip,
                    provider,
                    stream_channel,
                    user_agent_string,
                    country,
                    session_token,
                );

                user_connections.key_by_addr.insert(fingerprint.addr, username.to_string());
                let total_active = user_connections.key_by_addr.len();

                if let Some(connection_data) = user_connections.by_key.get_mut(username) {
                    connection_data.connections += 1;
                    connection_data.streams.push(stream_info.clone());
                    Self::log_connection_added(username, &fingerprint.addr, connection_data, total_active);
                }

                stream_info
            }
        };

        self.log_active_user().await;

        Some(stream_info)
    }

    fn is_log_user_enabled(&self) -> bool {
        self.log_active_user.load(Ordering::Relaxed)
    }

    fn new_user_session(session_token: &str, virtual_id: u32, provider: &str, stream_url: &str, addr: &SocketAddr,
                        connection_permission: UserConnectionPermission) -> UserSession {
        UserSession {
            token: session_token.to_string(),
            virtual_id,
            provider: provider.to_string(),
            stream_url: stream_url.to_string(),
            addr: *addr,
            ts: current_time_secs(),
            permission: connection_permission,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_user_session(&self, user: &ProxyUserCredentials, session_token: &str, virtual_id: u32,
                                     provider: &str, stream_url: &str, addr: &SocketAddr,
                                     connection_permission: UserConnectionPermission) -> String {
        self.gc();

        let username = user.username.clone();
        let mut user_connections = self.connections.write().await;
        let connection_data = user_connections.by_key.entry(username.clone()).or_insert_with(|| {
            debug_if_enabled!("Creating first session for user {username} {}", sanitize_sensitive_info(stream_url));
            let mut data = UserConnectionData::new(0, user.max_connections);
            let session = Self::new_user_session(session_token, virtual_id, provider, stream_url, addr, connection_permission);
            data.add_session(session);
            data
        });

        // If a session exists, update it
        for session in &mut connection_data.sessions {
            if session.token == session_token {
                session.ts = current_time_secs();
                if session.stream_url != stream_url {
                    session.stream_url = stream_url.to_string();
                }
                if session.provider != provider {
                    session.provider = provider.to_string();
                }
                session.permission = connection_permission;
                debug_if_enabled!("Using session for user {} with url: {}", user.username, sanitize_sensitive_info(stream_url));
                return session.token.clone();
            }
        }

        // If no session exists, create one
        debug_if_enabled!("Creating session for user {} with url: {}",
            user.username, sanitize_sensitive_info(stream_url));
        let session = Self::new_user_session(session_token, virtual_id, provider, stream_url, addr, connection_permission);
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
                debug_if_enabled!("Updated session {token} for {username} address {} -> {}",
                    sanitize_sensitive_info(&previous_addr.to_string()),
                    sanitize_sensitive_info(&addr.to_string()));
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
        total_active_sockets: usize,
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
                let recent_sockets = if recent_sockets.is_empty() {
                    String::from("n/a")
                } else {
                    recent_sockets
                };
                let unique_clients = connection_data
                    .streams
                    .iter()
                    .map(|stream| &stream.client_ip)
                    .collect::<HashSet<_>>()
                    .len();
                debug!(
                    "User {username} exceeded configured max connections ({}/{}). Unique clients: {}, recent sockets [{}]",
                    active_for_user,
                    connection_data.max_connections,
                    unique_clients,
                    recent_sockets
                );
            } else {
                debug_if_enabled!(
                    "Added new connection for {username} at {} (active user connections={active_for_user}, total connections={total_active_sockets})",
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
            if let Some(username) = connections.key_by_addr.get(addr).cloned() {
                let expires_at = now + block_for_secs;
                connections.kicked.insert(username, (expires_at, virtual_id));
            }
        }
    }

    pub async fn get_username_for_addr(&self, addr: &SocketAddr) -> Option<String> {
        self.connections.read().await.key_by_addr.get(addr).cloned()
    }

    fn gc(&self) {
        if let Some(gc_ts) = &self.gc_ts {
            let ts = gc_ts.load(Ordering::Acquire);
            let now = current_time_secs();

            if now - ts > USER_GC_TTL {
                if let Ok(mut user_connections) = self.connections.try_write() {
                    user_connections.kicked.retain(|_, (expires_at, _)| *expires_at > now);
                    user_connections.by_key.retain(|_k, v| now - v.ts < USER_CON_TTL && v.connections > 0);
                    for connection_data in user_connections.by_key.values_mut() {
                        connection_data.sessions.retain(|s| now - s.ts < USER_CON_TTL);
                    }

                    gc_ts.store(now, Ordering::Release);
                }
            }
        }
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
