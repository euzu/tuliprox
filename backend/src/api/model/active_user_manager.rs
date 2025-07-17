use crate::model::Config;
use crate::model::{ProxyUserCredentials};
use shared::utils::{current_time_secs, default_grace_period_millis, default_grace_period_timeout_secs, sanitize_sensitive_info};
use jsonwebtoken::get_current_timestamp;
use log::{debug, info};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use shared::model::UserConnectionPermission;

macro_rules! active_user_manager_shared_impl {
    () => {
          #[inline]
        async fn get_active_connections(user: &Arc<RwLock<HashMap<String, UserConnectionData>>>) -> usize {
            user.read().await.values().map(|c| c.connections as usize).sum()
        }

        fn log_active_user(&self) {
            if self.is_log_user_enabled() {
                let user = Arc::clone(&self.user);
                tokio::spawn(async move {
                    let user_count = user.read().await.len();
                    let user_connection_count = Self::get_active_connections(&user).await;
                    info!("Active Users: {user_count}, Active User Connections: {user_connection_count}");
                });
            }
        }

        pub async fn remove_connection(&self, addr: &str) {
            let mut addr_lock = self.user_by_addr.write().await;
            let username_opt = addr_lock.get(addr).cloned();
            if let Some(username) = username_opt {
                addr_lock.remove(addr);
                drop(addr_lock);
                let mut lock = self.user.write().await;
                if let Some(connection_data) = lock.get_mut(&username) {
                    if connection_data.connections > 0 {
                        connection_data.connections -= 1;
                    }
                    if connection_data.connections == 0 {
                        lock.remove(&username);
                    } else if connection_data.connections < connection_data.max_connections {
                        // Grace timeout expired, reset grace counters
                        connection_data.granted_grace = false;
                        connection_data.grace_ts = 0;
                    }

                }

                drop(lock);
            }
            self.log_active_user();
        }
    };
}

const USER_CON_TTL: u64 = 10_800;  // 3 hours
const USER_SESSION_LIMIT: usize = 50;

fn get_grace_options(config: &Config) -> (u64, u64) {
    let (grace_period_millis, grace_period_timeout_secs) = config.reverse_proxy.as_ref()
        .and_then(|r| r.stream.as_ref())
        .map_or_else(|| (default_grace_period_millis(), default_grace_period_timeout_secs()), |s| (s.grace_period_millis, s.grace_period_timeout_secs));
    (grace_period_millis, grace_period_timeout_secs)
}

struct ConnectionGuardUserManager {
    log_active_user: bool,
    user: Arc<RwLock<HashMap<String, UserConnectionData>>>,
    user_by_addr: Arc<RwLock<HashMap<String, String>>>,
}

impl ConnectionGuardUserManager {
    active_user_manager_shared_impl!();
    fn is_log_user_enabled(&self)-> bool {
        self.log_active_user
    }
}

pub struct UserConnectionGuard {
    manager: Arc<ConnectionGuardUserManager>,
    // username: String,
    addr: String,
}
impl Drop for UserConnectionGuard {
    fn drop(&mut self) {
        let manager = self.manager.clone();
        let addr = self.addr.clone();
        tokio::spawn(async move {
            manager.remove_connection(&addr).await;
        });
    }
}

#[derive(Clone, Debug)]
pub struct UserSession {
    pub token: String,
    pub virtual_id: u32,
    pub provider: String,
    pub stream_url: String,
    pub addr: String,
    pub ts: u64,
    pub permission: UserConnectionPermission,
}

struct UserConnectionData {
    max_connections: u32,
    connections: u32,
    granted_grace: bool,
    grace_ts: u64,
    sessions: Vec<UserSession>,
}

impl UserConnectionData {
    fn new(connections: u32, max_connections: u32) -> Self {
        Self {
            max_connections,
            connections,
            granted_grace: false,
            grace_ts: 0,
            sessions: Vec::new(),
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

pub struct ActiveUserManager {
    grace_period_millis: AtomicU64,
    grace_period_timeout_secs: AtomicU64,
    log_active_user: AtomicBool,
    user: Arc<RwLock<HashMap<String, UserConnectionData>>>,
    user_by_addr: Arc<RwLock<HashMap<String, String>>>,
    gc_ts: Option<AtomicU64>,
    close_signal_tx: tokio::sync::broadcast::Sender<String>,
}

impl ActiveUserManager {
    pub fn new(config: &Config) -> Self {
        let log_active_user = config.log.as_ref().is_some_and(|l| l.log_active_user);
        let (grace_period_millis, grace_period_timeout_secs) = get_grace_options(config);
        let (close_signal_tx, _) = tokio::sync::broadcast::channel(10);
        Self {
            grace_period_millis : AtomicU64::new(grace_period_millis),
            grace_period_timeout_secs: AtomicU64::new(grace_period_timeout_secs),
            log_active_user: AtomicBool::new(log_active_user),
            user: Arc::new(RwLock::new(HashMap::new())),
            user_by_addr: Arc::new(RwLock::new(HashMap::new())),
            gc_ts: Some(AtomicU64::new(current_time_secs())),
            close_signal_tx
        }
    }

    pub fn update_config(&self, config: &Config) {
        let log_active_user = config.log.as_ref().is_some_and(|l| l.log_active_user);
        let (grace_period_millis, grace_period_timeout_secs) = get_grace_options(config);
        self.grace_period_millis.store(grace_period_millis, Ordering::Relaxed);
        self.grace_period_timeout_secs.store(grace_period_timeout_secs, Ordering::Relaxed);
        self.log_active_user.store(log_active_user, Ordering::Relaxed);
    }

    fn clone_inner(&self) -> ConnectionGuardUserManager {
        ConnectionGuardUserManager {
            log_active_user: self.log_active_user.load(Ordering::Relaxed),
            user: Arc::clone(&self.user),
            user_by_addr: Arc::clone(&self.user_by_addr),
        }
    }

    pub async fn user_connections(&self, username: &str) -> u32 {
        if let Some(connection_data) = self.user.read().await.get(username) {
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
            if let Some(connection_data) = self.user.write().await.get_mut(username) {
                return self.check_connection_permission(username, connection_data);
            }
        }
        UserConnectionPermission::Allowed
    }


    pub async fn active_users(&self) -> usize {
        self.user.read().await.len()
    }

    pub async fn active_connections(&self) -> usize {
        Self::get_active_connections(&self.user).await
    }

    pub async fn add_connection(&self, username: &str, max_connections: u32, addr: &str) -> UserConnectionGuard {
        let mut lock = self.user.write().await;
        if let Some(connection_data) = lock.get_mut(username) {
            connection_data.connections += 1;
            connection_data.max_connections = max_connections;
        } else {
            lock.insert(username.to_string(), UserConnectionData::new(1, max_connections));
        }
        drop(lock);
        let mut lock = self.user_by_addr.write().await;
        lock.insert(addr.to_string(), username.to_string());
        drop(lock);

        self.log_active_user();

        UserConnectionGuard {
            manager: Arc::new(self.clone_inner()),
            // username: username.to_string(),
            addr: addr.to_string(),
        }
    }

    active_user_manager_shared_impl!();
    fn is_log_user_enabled(&self)-> bool {
        self.log_active_user.load(Ordering::Relaxed)
    }

    fn find_user_session<'a>(token: &'a str, sessions: &'a [UserSession]) -> Option<&'a UserSession> {
        sessions.iter().find(|&session| session.token.eq(token))
    }

    fn new_user_session(session_token: &str, virtual_id: u32, provider: &str, stream_url: &str, addr: &str,
                        connection_permission: UserConnectionPermission) -> UserSession {
        UserSession {
            token: session_token.to_string(),
            virtual_id,
            provider: provider.to_string(),
            stream_url: stream_url.to_string(),
            addr: addr.to_string(),
            ts: current_time_secs(),
            permission: connection_permission,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_user_session(&self, user: &ProxyUserCredentials, session_token: &str, virtual_id: u32,
                                     provider: &str, stream_url: &str, addr: &str,
                                     connection_permission: UserConnectionPermission) -> Option<String> {
        self.gc().await;
        let mut lock = self.user.write().await;
        if let Some(connection_data) = lock.get_mut(&user.username) {
            // check existing session
            for session in &mut connection_data.sessions {
                if session.token.eq(&session_token) {
                    session.ts = current_time_secs();
                    if !session.stream_url.eq(&stream_url) {
                        session.stream_url = stream_url.to_string();
                    }
                    if !provider.eq(&session.provider) {
                        session.provider = provider.to_string();
                    }
                    session.permission = connection_permission;
                    debug!("Using session for user {} with token {session_token} {}", user.username, sanitize_sensitive_info(stream_url));
                    return Some(session.token.to_string());
                }
            }

            // no session creates new one
            debug!("Creating session for user {} with token {session_token} {}", user.username, sanitize_sensitive_info(stream_url));
            let session = Self::new_user_session(session_token, virtual_id, provider, stream_url, addr, connection_permission);
            let token = session.token.clone();
            connection_data.add_session(session);
            Some(token)
        } else {
            debug!("Creating session for user {} with token {session_token} {}", user.username, sanitize_sensitive_info(stream_url));
            let mut connection_data = UserConnectionData::new(0, user.max_connections);
            let session = Self::new_user_session(session_token, virtual_id, provider, stream_url, addr, connection_permission);
            let token = session.token.clone();
            connection_data.add_session(session);
            lock.insert(user.username.to_string(), connection_data);
            Some(token)
        }
    }

    pub async fn update_session_addr(&self, username: &str, token: &str, addr: &str) {
        let mut lock = self.user.write().await;
        if let Some(connection_data) = lock.get_mut(username) {
            for session in &mut connection_data.sessions {
                if session.token.eq(token) {
                    self.drop_connection(&session.addr);
                    session.addr = addr.to_string();
                }
            }
        }
    }

    fn drop_connection(&self, addr: &str) {
        let _ = self.close_signal_tx.send(addr.to_string());
    }

    pub fn get_close_connection_channel(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.close_signal_tx.subscribe()
    }

    pub async fn get_user_session(&self, username: &str, token: &str) -> Option<UserSession> {
        self.update_user_session(username, token).await
    }

    async fn update_user_session(&self, username: &str, token: &str) -> Option<UserSession> {
        let mut lock = self.user.write().await;
        if let Some(connection_data) = lock.get_mut(username) {
            if connection_data.max_connections == 0 {
                return Self::find_user_session(token, &connection_data.sessions).cloned();
            }

            // Separate mutable borrow of the session
            let mut found_session_index = None;
            for (i, session) in connection_data.sessions.iter().enumerate() {
                if session.token == token {
                    found_session_index = Some(i);
                    break;
                }
            }

            if let Some(index) = found_session_index {
                let session_permission = connection_data.sessions[index].permission;
                if session_permission == UserConnectionPermission::GracePeriod {
                    let new_permission = self.check_connection_permission(username, connection_data);
                    connection_data.sessions[index].permission = new_permission;
                }
                return Some(connection_data.sessions[index].clone());
            }
        }
        None
    }

    async fn gc(&self) {
        if let Some(gc_ts) = &self.gc_ts {
            let ts = gc_ts.load(Ordering::Acquire);
            let now = current_time_secs();
            if now - ts > USER_CON_TTL {
                let mut lock = self.user.write().await;
                for (_, connection_data) in lock.iter_mut() {
                    connection_data.sessions.retain(|s| now - s.ts < USER_CON_TTL);
                }
                gc_ts.store(now, Ordering::Release);
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
