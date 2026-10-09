use super::{
    ActiveUserManager, PromotionAction, UserConnectionData, UserConnections, UserConnectionsWriteGuard, UserSession,
    ANON_SOCKET_TTL, DEFAULT_ACTIVE_SOCKET_TTL_SECS, USER_CON_TTL, USER_GC_TTL, USER_SESSION_LIMIT,
};
use crate::{connection_manager::CleanupEvent, stream::DIRECT_BODY_IDLE_TIMEOUT_SECS};
use log::debug;
use shared::{
    model::{ActiveUserConnectionChange, EventMessage, PlaylistItemType, StreamInfo},
    utils::{current_time_secs, is_catchup_session_token},
};
use std::{
    cmp::Reverse,
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

impl UserConnectionData {
    pub(super) fn gc(&mut self) {
        if self.sessions.len() > USER_SESSION_LIMIT {
            self.sessions.sort_by_key(|e| std::cmp::Reverse(e.ts));
            self.sessions.truncate(USER_SESSION_LIMIT);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(super) struct AdaptiveExpiryEntry {
    pub(super) expires_at: u64,
    pub(super) username: String,
    pub(super) session_token: String,
    pub(super) uid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(super) struct AdaptiveExpiryKey {
    pub(super) username: String,
    pub(super) session_token: String,
    pub(super) uid: u32,
}

impl ActiveUserManager {
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

    pub(super) fn build_preserved_stream_expiry(
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

    pub(super) async fn enqueue_adaptive_expiry(&self, entry: AdaptiveExpiryEntry) {
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

    pub fn active_socket_ttl_secs(&self) -> u64 {
        let configured_ttl = self.adaptive_session_ttl_secs.load(Ordering::Relaxed);
        if configured_ttl == 0 {
            DEFAULT_ACTIVE_SOCKET_TTL_SECS
        } else {
            configured_ttl
        }
    }

    pub async fn socket_expiry_deadline(&self, addr: &SocketAddr) -> Option<u64> {
        let connections = self.connections.read().await;
        let registration = connections.key_by_addr.get(addr)?;
        if registration.is_empty() {
            return None;
        }

        let ttl_secs = self.effective_socket_ttl_secs(&connections, addr);
        Some(registration.ts.saturating_add(ttl_secs))
    }

    /// Direct VOD/series bodies may stop reading while the player drains its buffer.
    /// Their socket expiry must not fire before the direct-body idle timeout, or a
    /// buffering player is disconnected mid-playback even though the stream is alive.
    pub(super) fn effective_socket_ttl_secs(&self, connections: &UserConnections, addr: &SocketAddr) -> u64 {
        let base_ttl = self.active_socket_ttl_secs();
        if Self::addr_has_direct_body_stream(connections, addr) {
            base_ttl.max(DIRECT_BODY_IDLE_TIMEOUT_SECS)
        } else {
            base_ttl
        }
    }

    pub(super) fn is_preserved_stream_expired(&self, stream: &StreamInfo, sessions: &[UserSession], now: u64) -> bool {
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

    pub(super) async fn run_adaptive_expiry_worker(self: Arc<Self>) {
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
    pub(super) async fn process_due_adaptive_expiry_entries(&self, now: u64) {
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
            let mut user_connections = self.write_connections().await;
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
                            connection_data.remove_stream_request_claims(removed_stream.uid);
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

    pub(super) fn gc(&self) {
        if let Some(gc_ts) = &self.gc_ts {
            let ts = gc_ts.load(Ordering::Acquire);
            let now = current_time_secs();

            if now.saturating_sub(ts) > USER_GC_TTL
                && gc_ts.compare_exchange(ts, now, Ordering::AcqRel, Ordering::Relaxed).is_ok()
            {
                if let Ok(guard) = self.connections.try_write() {
                    let mut user_connections = UserConnectionsWriteGuard::new(guard, &self.deferred_wakes);
                    user_connections.kicked.retain(|_, (expires_at, _)| *expires_at > now);
                    let now_instant = Instant::now();
                    user_connections
                        .recently_evicted_sessions
                        .retain(|_, protection| protection.expires_at > now_instant);
                    user_connections.ended_sessions.retain(|_, expires_at| *expires_at > now_instant);
                    user_connections
                        .recent_socket_reentry_guards
                        .retain(|_, protection| protection.expires_at > now_instant);
                    for connection_data in user_connections.by_key.values_mut() {
                        Self::release_expired_session_reservations(connection_data, now);
                        connection_data.sessions.retain(|s| now.saturating_sub(s.ts) < USER_CON_TTL);
                    }
                    user_connections.by_key.retain(|_k, v| {
                        v.connections > 0 || !v.streams.is_empty() || now.saturating_sub(v.ts) < USER_CON_TTL
                    });
                    let UserConnections { ref by_key, ref mut key_by_addr, .. } = *user_connections;
                    key_by_addr.retain(|addr, registration| {
                        registration.usernames.retain(|u| {
                            by_key
                                .get(u)
                                .is_some_and(|d| d.has_session_addr(addr) || d.streams.iter().any(|s| s.addr == *addr))
                        });
                        !(registration.is_empty() && now.saturating_sub(registration.ts) >= ANON_SOCKET_TTL)
                    });
                } else {
                    // Lock contention: release the GC claim so a subsequent caller can retry immediately.
                    let _ = gc_ts.compare_exchange(now, ts, Ordering::AcqRel, Ordering::Relaxed);
                }
            }
        }
    }
}
