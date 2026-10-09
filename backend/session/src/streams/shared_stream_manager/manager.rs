use super::{
    buffer::SubscriberId, reader::resolve_min_burst_buffer_bytes, PendingJoinGuard, PendingSharedMeterRegistration,
    PendingSharedSubscriberCleanup, SharedMeterEntry, SharedStreamManager, SharedStreamState, SharedStreamsRegister,
    DEFAULT_SUBSCRIBER_IDLE_TIMEOUT_SECS, SHARED_CLEANUP_ADMISSION_TIMEOUT,
};
use crate::{
    streams::buffered_stream::CHANNEL_SIZE, ActiveProviderManager, BoxedProviderStream, ConnectionManager,
    ConnectionRejectionReason, ManagedProviderHandle, SharedCleanupCapability,
};
use bytes::Bytes;
use futures::Stream;
use log::warn;
use shared::utils::sanitize_sensitive_info;
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::{
    sync::RwLock,
    time::{timeout, Duration, Instant},
};
use tuliprox_core::{
    model::{AllocationId, AppConfig, SharedSubscriberId},
    utils::debug_if_enabled,
};

/// The four state handles the shared-stream paths need.
///
/// These functions used to take the whole `AppState` and reach into four of its
/// fields. Keeping this slice explicit avoids coupling the session crate to the
/// API server state; the composition root supplies the required handles.
#[derive(Clone, Copy)]
pub struct SharedStreamCtx<'a> {
    pub app_config: &'a Arc<AppConfig>,
    pub shared_stream_manager: &'a Arc<SharedStreamManager>,
    pub active_provider: &'a Arc<ActiveProviderManager>,
    pub connection_manager: &'a Arc<ConnectionManager>,
}

impl SharedStreamManager {
    pub async fn reserve_subscriber_cleanup(
        connection_manager: &ConnectionManager,
        subscriber_id: SharedSubscriberId,
        addr: SocketAddr,
    ) -> Result<PendingSharedSubscriberCleanup, ConnectionRejectionReason> {
        if connection_manager.is_shutting_down() {
            warn!("Shared stream cleanup rejected during shutdown for subscriber {subscriber_id}");
            return Err(ConnectionRejectionReason::CleanupReceiverClosed);
        }
        match timeout(SHARED_CLEANUP_ADMISSION_TIMEOUT, connection_manager.cleanup_tx().reserve_owned()).await {
            Ok(Ok(permit)) => Ok(PendingSharedSubscriberCleanup::new(permit, subscriber_id, addr)),
            Ok(Err(_)) => {
                warn!("Shared stream cleanup receiver closed; rejecting subscriber {subscriber_id}");
                Err(ConnectionRejectionReason::CleanupReceiverClosed)
            }
            Err(_) => {
                warn!("Shared stream cleanup admission timed out; rejecting subscriber {subscriber_id}");
                Err(ConnectionRejectionReason::CleanupAdmissionTimeout)
            }
        }
    }

    pub fn new(provider_manager: Arc<ActiveProviderManager>) -> Self {
        Self {
            provider_manager,
            shared_streams: RwLock::new(SharedStreamsRegister::default()),
            meter_uids: std::sync::Mutex::new(HashMap::new()),
            #[cfg(test)]
            test_preflight_barrier: std::sync::Mutex::new(None),
        }
    }

    pub async fn get_shared_state(&self, stream_url: &str) -> Option<Arc<SharedStreamState>> {
        self.shared_streams.read().await.by_key.get(stream_url).map(Arc::clone)
    }

    pub async fn get_shared_state_headers(&self, stream_url: &str) -> Option<Vec<(String, String)>> {
        self.get_shared_state(stream_url).await.map(|s| s.headers.clone())
    }

    pub async fn resource_counts(&self) -> (usize, usize) {
        let register = self.shared_streams.read().await;
        (register.by_key.len(), register.key_by_subscriber.len())
    }

    pub(super) fn lock_meter_uids(&self) -> std::sync::MutexGuard<'_, HashMap<Arc<str>, SharedMeterEntry>> {
        self.meter_uids.lock().unwrap_or_else(|poisoned| {
            warn!("Recovering poisoned shared-stream meter registry");
            poisoned.into_inner()
        })
    }

    pub fn reserve_meter_uid(
        self: &Arc<Self>,
        stream_url: &str,
        uid_factory: impl FnOnce() -> u32,
    ) -> (u32, Option<PendingSharedMeterRegistration>) {
        let mut uids = self.lock_meter_uids();
        if let Some(entry) = uids.get(stream_url) {
            return (entry.uid, None);
        }
        let stream_url: Arc<str> = Arc::from(stream_url);
        let meter_uid = uid_factory();
        uids.insert(Arc::clone(&stream_url), SharedMeterEntry { uid: meter_uid, owner: None });
        drop(uids);
        (
            meter_uid,
            Some(PendingSharedMeterRegistration { manager: Arc::clone(self), stream_url, meter_uid, armed: true }),
        )
    }

    /// Binds the reserved meter entry to the origin that just committed its shared
    /// origin. Called only while the registry write lock is held and only after the URL
    /// was confirmed absent from `by_key`, so the previous origin (if any) is already
    /// torn down and this new origin is the sole owner. Overwriting the owner is what
    /// keeps a successor's meter from being removed by a stale predecessor teardown.
    pub fn adopt_meter_uid(&self, stream_url: &str, allocation_id: AllocationId) {
        let mut uids = self.lock_meter_uids();
        if let Some(entry) = uids.get_mut(stream_url) {
            entry.owner = Some(allocation_id);
        }
    }

    pub(super) fn remove_meter_uid_if_pending(&self, stream_url: &str, meter_uid: u32) {
        let mut uids = self.lock_meter_uids();
        if let Some(entry) = uids.get(stream_url) {
            if entry.uid == meter_uid && entry.owner.is_none() {
                uids.remove(stream_url);
            }
        }
    }

    pub fn meter_count(&self) -> usize { self.lock_meter_uids().len() }

    pub(super) fn remove_meter_uid_if_owned_by(&self, stream_url: &str, allocation_id: Option<AllocationId>) {
        let mut uids = self.lock_meter_uids();
        let remove = uids.get(stream_url).is_some_and(|entry| match allocation_id {
            Some(id) => entry.owner == Some(id),
            None => entry.owner.is_none(),
        });
        if remove {
            uids.remove(stream_url);
        }
    }

    pub(super) async fn finish_unregister(&self, stream_url: &str, state: &SharedStreamState) {
        self.remove_meter_uid_if_owned_by(stream_url, state.provider_guard.as_ref().map(|handle| handle.allocation_id));
        state.cancel_subscribers().await;
        state.stop_token.cancel();
        if let Some(handle) = &state.provider_guard {
            self.provider_manager.release_handle(handle);
        }
    }

    pub(super) async fn unregister(&self, stream_url: &str, expected: &Arc<SharedStreamState>) {
        let mut register = self.shared_streams.write().await;
        let state = {
            if !register.by_key.get(stream_url).is_some_and(|state| Arc::ptr_eq(state, expected)) {
                return;
            }
            register.key_by_subscriber.retain(|_, url| url.as_ref() != stream_url);
            register.by_key.remove(stream_url)
        };
        drop(register);
        if let Some(state) = state {
            self.finish_unregister(stream_url, &state).await;
        }
    }

    pub async fn teardown_preempted_stream(&self, stream_url: &str, allocation_id: u64) {
        let mut register = self.shared_streams.write().await;
        if !register.by_key.get(stream_url).is_some_and(|state| {
            state.provider_guard.as_ref().is_some_and(|handle| handle.allocation_id == allocation_id)
        }) {
            return;
        }
        let state = {
            register.key_by_subscriber.retain(|_, url| url.as_ref() != stream_url);
            register.by_key.remove(stream_url)
        };
        drop(register);
        self.remove_meter_uid_if_owned_by(stream_url, Some(allocation_id));
        if let Some(state) = state {
            state.preempted_token.cancel();
            state.stop_token.cancel();
        }
    }

    /// Releases all subscribers on a closed transport. Playback cleanup uses the id variant.
    pub async fn release_connection(&self, addr: &SocketAddr, _send_stop_signal: bool) {
        let states: Vec<Arc<SharedStreamState>> = {
            let register = self.shared_streams.read().await;
            register.by_key.values().cloned().collect()
        };
        let mut ids = Vec::new();
        for state in states {
            ids.extend(
                state
                    .subscribers
                    .read()
                    .await
                    .iter()
                    .filter_map(|(id, subscriber)| (subscriber.addr == *addr).then_some(*id)),
            );
        }
        for id in ids {
            self.release_subscriber(id).await;
        }
    }

    pub async fn release_subscriber(&self, subscriber_id: SubscriberId) {
        let mut register = self.shared_streams.write().await;
        let stopped = {
            // Registry then subscribers is the shared-stream lock order. Joining and
            // removing the final subscriber must not race an origin replacement.
            if let Some(url) = register.key_by_subscriber.remove(&subscriber_id) {
                if let Some(state) = register.by_key.get(&url).cloned() {
                    let mut subscribers = state.subscribers.write().await;
                    if let Some(subscriber) = subscribers.remove(&subscriber_id) {
                        subscriber.cancel_token.cancel();
                    }
                    if subscribers.is_empty() && !state.has_pending_joins() {
                        register.by_key.remove(&url);
                        drop(subscribers);
                        Some((url, state))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        };
        drop(register);
        self.provider_manager.release_shared_connection(subscriber_id);
        if let Some((url, state)) = stopped {
            self.finish_unregister(&url, &state).await;
        }
    }

    pub async fn shutdown(&self) {
        let stopped = {
            let mut register = self.shared_streams.write().await;
            register.key_by_subscriber.clear();
            register.by_key.drain().collect::<Vec<_>>()
        };
        for (url, state) in stopped {
            self.finish_unregister(&url, &state).await;
            // Join the broadcast and forwarder tasks for this origin so teardown is
            // complete before the manager (and its runtime) is dropped. Cancellation has
            // already fired, so these tasks only finish their terminal cleanup.
            let handles = std::mem::take(&mut *state.lock_task_handles());
            for handle in handles {
                let _ = handle.await;
            }
        }
        self.lock_meter_uids().clear();
    }

    pub(super) async fn subscribe_stream(
        &self,
        stream_url: &str,
        addr: &SocketAddr,
        subscriber_id: SubscriberId,
        connection_manager: Arc<ConnectionManager>,
        user_priority: i8,
        connection_kind: crate::active_provider_manager::ConnectionKind,
    ) -> Result<Option<(BoxedProviderStream, Option<Arc<str>>, SharedCleanupCapability)>, ConnectionRejectionReason>
    {
        let Some(_admission) = connection_manager.begin_admission().await else {
            return Err(ConnectionRejectionReason::CleanupReceiverClosed);
        };
        {
            let register = self.shared_streams.read().await;
            if !register.by_key.contains_key(stream_url) {
                return Ok(None);
            }
        }
        #[cfg(test)]
        {
            let barrier = self.test_preflight_barrier.lock().ok().and_then(|g| g.clone());
            if let Some(barrier) = barrier {
                barrier.wait().await;
            }
        }
        let mut pending_cleanup = Self::reserve_subscriber_cleanup(&connection_manager, subscriber_id, *addr).await?;
        // Keep the global registry lock only for the lookup and the key_by_subscriber
        // commit. The origin's own subscriber/task locks are taken during subscribe, so a
        // slow origin must not block joins on other URLs.
        let (state, pending_join) = {
            let mut register = self.shared_streams.write().await;
            let Some((key, state)) =
                register.by_key.get_key_value(stream_url).map(|(key, state)| (Arc::clone(key), Arc::clone(state)))
            else {
                pending_cleanup.disarm();
                return Ok(None);
            };
            if let Err(err) = self.provider_manager.add_shared_connection(
                addr,
                subscriber_id,
                stream_url,
                user_priority,
                connection_kind,
            ) {
                warn!("Failed joining shared stream: {}", sanitize_sensitive_info(&err));
                pending_cleanup.disarm();
                return Ok(None);
            }
            register.key_by_subscriber.insert(subscriber_id, key);
            let pending_join = PendingJoinGuard::new(&state);
            (state, pending_join)
        };
        Ok(Some(
            state.subscribe(addr, subscriber_id, Arc::clone(&connection_manager), pending_cleanup, pending_join).await,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn register_shared_stream<S, E>(
        ctx: SharedStreamCtx<'_>,
        stream_url: &str,
        bytes_stream: S,
        addr: &SocketAddr,
        subscriber_id: SubscriberId,
        headers: Vec<(String, String)>,
        buffer_size: usize,
        mut provider_handle: Option<ManagedProviderHandle>,
        pending_cleanup: PendingSharedSubscriberCleanup,
        user_priority: i8,
        connection_kind: crate::active_provider_manager::ConnectionKind,
    ) -> Option<(BoxedProviderStream, Option<Arc<str>>, SharedCleanupCapability)>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static + Send,
        E: std::fmt::Debug + Send,
    {
        let _admission = ctx.connection_manager.begin_admission().await?;
        let registration_started_at = Instant::now();
        let buf_size = CHANNEL_SIZE.max(buffer_size);
        let config = ctx.app_config.config.load();
        let min_buffer_bytes = resolve_min_burst_buffer_bytes(&config);
        let low_priority_preempted =
            ctx.app_config.custom_stream_response.load().as_ref().and_then(|c| c.low_priority_preempted.clone());
        let mut register = ctx.shared_stream_manager.shared_streams.write().await;
        if let Some((key, existing)) =
            register.by_key.get_key_value(stream_url).map(|(key, state)| (Arc::clone(key), Arc::clone(state)))
        {
            drop(provider_handle.take());
            if ctx
                .active_provider
                .add_shared_connection(addr, subscriber_id, stream_url, user_priority, connection_kind)
                .is_err()
            {
                return None;
            }
            register.key_by_subscriber.insert(subscriber_id, key);
            let pending_join = PendingJoinGuard::new(&existing);
            drop(register);
            let response = existing
                .subscribe(addr, subscriber_id, Arc::clone(ctx.connection_manager), pending_cleanup, pending_join)
                .await;
            return Some(response);
        }
        let handle = provider_handle.as_ref().and_then(|managed| managed.handle())?;
        if !ctx.active_provider.make_shared_connection(handle, stream_url, subscriber_id) {
            drop(register);
            drop(provider_handle.take());
            return None;
        }
        let allocation_id = handle.allocation_id;
        let raw_handle = provider_handle.and_then(|mut managed| managed.disarm())?;
        let mut shared_state =
            SharedStreamState::new(headers, buf_size, Some(raw_handle), min_buffer_bytes, low_priority_preempted)
                .with_subscriber_idle_timeout_secs(
                    config
                        .reverse_proxy
                        .as_ref()
                        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
                        .map_or(DEFAULT_SUBSCRIBER_IDLE_TIMEOUT_SECS, |stream| {
                            stream.shared_subscriber_idle_timeout_secs
                        }),
                );
        shared_state.subscriber_max_duration =
            config.sleep_timer_mins.filter(|mins| *mins > 0).map(|mins| Duration::from_secs(u64::from(mins) * 60));
        let shared_state = Arc::new(shared_state);
        let stream_key: Arc<str> = Arc::from(stream_url);
        register.by_key.insert(Arc::clone(&stream_key), Arc::clone(&shared_state));
        register.key_by_subscriber.insert(subscriber_id, stream_key);
        ctx.shared_stream_manager.adopt_meter_uid(stream_url, allocation_id);
        let pending_join = PendingJoinGuard::new(&shared_state);
        drop(register);
        let subscribed_stream = shared_state
            .subscribe(addr, subscriber_id, Arc::clone(ctx.connection_manager), pending_cleanup, pending_join)
            .await;
        debug_if_enabled!(
            "Shared stream startup register+subscribe completed for {} in {} ms",
            sanitize_sensitive_info(stream_url),
            registration_started_at.elapsed().as_millis()
        );
        shared_state.broadcast(stream_url, bytes_stream, Arc::clone(ctx.shared_stream_manager));
        Some(subscribed_stream)
    }

    pub async fn subscribe_shared_stream(
        ctx: SharedStreamCtx<'_>,
        stream_url: &str,
        addr: &SocketAddr,
        subscriber_id: SubscriberId,
        user_priority: i8,
        connection_kind: crate::active_provider_manager::ConnectionKind,
    ) -> Result<Option<(BoxedProviderStream, Option<Arc<str>>, SharedCleanupCapability)>, ConnectionRejectionReason>
    {
        ctx.shared_stream_manager
            .subscribe_stream(
                stream_url,
                addr,
                subscriber_id,
                Arc::clone(ctx.connection_manager),
                user_priority,
                connection_kind,
            )
            .await
    }
}
