use super::{
    create_socket_reentry_guard_key, uses_session_reentry_guard, ActiveUserManager, RecentWinnerProtection,
    SocketRegistration,
};
use shared::{model::VirtualId, utils::current_time_secs};
use std::{
    collections::HashMap,
    net::SocketAddr,
    time::{Duration, Instant},
};

impl ActiveUserManager {
    pub async fn get_eviction_candidates(&self, username: &str, _client_ip: &str) -> Vec<crate::EvictionCandidate> {
        let connections = self.connections.read().await;
        let Some(connection_data) = connections.by_key.get(username) else {
            return Vec::new();
        };
        let mut addr_counts = HashMap::new();
        for stream in &connection_data.streams {
            // The stream kind is the charged slot. Session lifecycle may lag behind
            // the stream registration, so it cannot determine eviction eligibility.
            if !stream.preserved && connection_data.stream_kinds.contains_key(&stream.uid) {
                addr_counts
                    .entry(stream.addr)
                    .and_modify(|count: &mut u8| *count = count.saturating_add(1))
                    .or_insert(1_u8);
            }
        }
        let candidates: Vec<_> = connection_data
            .streams
            .iter()
            .filter(|stream| stream.preserved || connection_data.stream_kinds.contains_key(&stream.uid))
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
            .map(|s| crate::EvictionCandidate {
                addr: s.addr,
                client_ip: s.client_ip.clone(),
                virtual_id: shared::model::VirtualId::new(s.channel.virtual_id),
                ts: s.ts,
                uid: s.uid,
            })
            .collect();
        candidates
    }

    pub async fn is_user_blocked_for_stream(&self, username: &str, virtual_id: VirtualId) -> bool {
        let connections = self.connections.read().await;
        let now = current_time_secs();
        matches!(connections.kicked.get(username), Some((expires_at, vid)) if *vid == virtual_id && *expires_at > now)
    }

    pub async fn recently_evicted_session_protected_addr(&self, session_token: &str) -> Option<SocketAddr> {
        let connections = self.connections.read().await;
        let protection = connections.recently_evicted_sessions.get(session_token)?;
        if protection.expires_at > Instant::now() {
            return Some(protection.protected_addr);
        }

        let username = connections.by_key.iter().find_map(|(username, connection_data)| {
            connection_data.sessions.iter().any(|session| session.token == session_token).then_some(username.as_str())
        })?;
        connections
            .key_by_addr
            .get(&protection.protected_addr)
            .filter(|registration| registration.usernames.contains(username))
            .map(|_| protection.protected_addr)
    }

    pub async fn recent_socket_reentry_protected_addr(
        &self,
        username: &str,
        client_ip: &str,
        virtual_id: VirtualId,
    ) -> Option<SocketAddr> {
        let connections = self.connections.read().await;
        let key = create_socket_reentry_guard_key(username, client_ip, virtual_id);
        let protection = connections.recent_socket_reentry_guards.get(&key)?;
        if protection.expires_at > Instant::now() {
            Some(protection.protected_addr)
        } else {
            None
        }
    }

    pub async fn block_user_for_stream(&self, addr: &SocketAddr, virtual_id: VirtualId, blocked_secs: u64) {
        let block_for_secs = blocked_secs.clamp(0, 86_400); // max 1 day;
        if block_for_secs > 0 {
            let mut connections = self.write_connections().await;
            let now = current_time_secs();
            connections.kicked.retain(|_, (expires_at, _)| *expires_at > now);
            let candidate_usernames: Vec<String> = connections
                .key_by_addr
                .get(addr)
                .map(|reg| reg.usernames.iter().cloned().collect())
                .unwrap_or_default();
            let target_vid = virtual_id.get();
            let expires_at = now + block_for_secs;
            for username in candidate_usernames {
                // Only block users who are actually watching the target channel
                // at this socket address. Behind reverse proxies / NAT, multiple
                // distinct users share the same SocketAddr; blocking all of them
                // would be a multi-tenant isolation violation.
                let is_watching_target = connections.by_key.get(&username).is_some_and(|data| {
                    data.streams.iter().any(|s| s.addr == *addr && s.channel.virtual_id == target_vid)
                });
                if is_watching_target {
                    connections.kicked.insert(username, (expires_at, virtual_id));
                }
            }
        }
    }

    pub async fn block_user_for_stream_uid(&self, uid: u32, virtual_id: VirtualId, blocked_secs: u64) {
        if blocked_secs == 0 {
            return;
        }
        let mut connections = self.write_connections().await;
        let username = connections
            .by_key
            .iter()
            .find_map(|(username, data)| data.streams.iter().any(|stream| stream.uid == uid).then(|| username.clone()));
        if let Some(username) = username {
            connections.kicked.insert(username, (current_time_secs() + blocked_secs.min(86_400), virtual_id));
        }
    }

    pub async fn mark_recent_eviction_guard_for_addr(
        &self,
        addr: &SocketAddr,
        protected_addr: SocketAddr,
        ttl: Duration,
    ) {
        let mut connections = self.write_connections().await;
        let now = Instant::now();
        connections.recently_evicted_sessions.retain(|_, protection| protection.expires_at > now);
        connections.recent_socket_reentry_guards.retain(|_, protection| protection.expires_at > now);

        let usernames = connections.key_by_addr.get(addr).map(SocketRegistration::all_usernames).unwrap_or_default();
        if usernames.is_empty() {
            return;
        }

        let protection = RecentWinnerProtection { protected_addr, expires_at: now + ttl };
        let mut session_tokens = Vec::new();
        let mut socket_guard_keys = Vec::new();

        for username in &usernames {
            if let Some(connection_data) = connections.by_key.get(username) {
                for stream in connection_data.streams.iter().filter(|stream| stream.addr == *addr) {
                    if uses_session_reentry_guard(stream) && stream.session_token.is_some() {
                        let Some(session_token) = stream.session_token.clone() else {
                            continue;
                        };
                        session_tokens.push(session_token);
                    } else {
                        socket_guard_keys.push(create_socket_reentry_guard_key(
                            username,
                            &stream.client_ip,
                            shared::model::VirtualId::new(stream.channel.virtual_id),
                        ));
                    }
                }
            }
        }

        // Evicted sessions stay ended even when the reentry guard itself is disabled.
        connections.mark_sessions_ended(session_tokens.iter().cloned());
        if ttl.is_zero() {
            return;
        }
        for session_token in session_tokens {
            connections.recently_evicted_sessions.insert(session_token, protection);
        }
        for key in socket_guard_keys {
            connections.recent_socket_reentry_guards.insert(key, protection);
        }
    }
}
