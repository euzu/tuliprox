use super::{
    decide_connection_kind, remember_session_addr, stream_history_session_id, ActiveUserConnectionParams,
    ActiveUserManager, PlaybackSessionRegistration, SocketRegistration, StreamRequestDetach, UserConnectionData,
};
use shared::{model::StreamInfo, utils::current_time_secs};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::sync::Mutex;
use tuliprox_core::utils::utc_day_from_secs;

#[derive(Debug, Clone, Copy)]
pub(super) struct StreamRequestClaim {
    pub(super) stream_uid: u32,
    pub(super) addr: SocketAddr,
}

impl UserConnectionData {
    pub(super) fn remove_stream_request_claims(&mut self, stream_uid: u32) {
        self.stream_request_claims.retain(|_, claim| claim.stream_uid != stream_uid);
    }
}

impl ActiveUserManager {
    pub(super) fn transition_gate_key(username: &str, token: &str) -> String {
        let mut key = String::with_capacity(username.len() + token.len() + 1);
        key.push_str(username);
        key.push('\0');
        key.push_str(token);
        key
    }

    pub(super) fn admission_gate_key(username: &str) -> String {
        let mut key = String::with_capacity(username.len() + 11);
        key.push_str("admission");
        key.push('\0');
        key.push_str(username);
        key
    }

    pub(super) fn cleanup_idle_transition_gates(transition_gates: &mut HashMap<String, Arc<Mutex<()>>>) {
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

    pub async fn release_stream_request_by_uid(&self, addr: &SocketAddr, request_uid: u32) -> StreamRequestDetach {
        self.release_stream_inner(addr, Some(request_uid)).await
    }

    #[allow(clippy::too_many_lines)]
    pub async fn update_connection_with_session_registration(
        &self,
        update: ActiveUserConnectionParams<'_>,
        session_registration: Option<&PlaybackSessionRegistration>,
    ) -> Option<StreamInfo> {
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
            let mut user_connections = self.write_connections().await;
            if let Some(requirement) = session_registration {
                let token = session_token?;
                let (data, session) = user_connections.current_session(username, token, requirement.identity)?;
                if requirement.enforce_limits
                    && !requirement.grace_admitted
                    && !session.lifecycle.is_counted()
                    && !Self::session_has_stream(data, token)
                    && decide_connection_kind(
                        data.effective_counts_for_admission(Some(token)),
                        max_connections,
                        soft_connections,
                    ) != Some(connection_kind)
                {
                    return None;
                }
            }

            let now = current_time_secs();
            let registration =
                user_connections.key_by_addr.entry(fingerprint.addr).or_insert_with(SocketRegistration::anonymous);
            registration.add_user(username, now);

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
                .position(|stream_info| {
                    !stream_channel.shared
                        && match session_token {
                            Some(token) => {
                                stream_info.session_token.as_deref() == Some(token)
                                    && Self::should_reuse_stream_for_session(stream_info, stream_channel)
                            }
                            None => stream_info.uid == uid && stream_info.session_token.is_none(),
                        }
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
                connection_data.attach_stream_request(uid, stream_info.uid, fingerprint.addr);
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
                connection_data.attach_stream_request(uid, stream_info.uid, fingerprint.addr);
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
}
