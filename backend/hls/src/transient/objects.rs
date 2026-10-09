use super::{
    transient_object_expires_at, HlsSession, ProxySessionId, TransientObjectCacheEntry, TransientObjectCacheKey,
    TransientObjectCacheStatus, TransientObjectFetchDecision, TransientObjectFetchToken, TransientObjectRemoval,
    TransientObjectResourceBinding, TransientPassthroughState, TransientResourceId, TransientResourceKind,
    TransientResourceRef, MAX_FAILED_TRANSIENT_OBJECT_ENTRIES,
};
use axum::http::StatusCode;
use std::{collections::HashSet, sync::Arc};
use tokio::sync::Notify;

impl TransientPassthroughState {
    /// Builds the object-cache key for an already registered transient resource.
    ///
    /// The key is intentionally based on the opaque resource ID. The concrete fetch URI remains in
    /// `TransientResourceRef::resolved_origin_uri` and must not be reconstructed from this key.
    pub fn transient_object_key(
        proxy_session_id: &ProxySessionId,
        resource_id: &TransientResourceId,
        file_ext: impl Into<String>,
    ) -> TransientObjectCacheKey {
        TransientObjectCacheKey::new(proxy_session_id.clone(), resource_id.clone(), file_ext)
    }

    pub fn ready_object(
        &mut self,
        key: &TransientObjectCacheKey,
        resource_kind: TransientResourceKind,
        now_ms: u64,
    ) -> Option<TransientObjectCacheEntry> {
        let binding = {
            let entry = self.object_cache.get(key)?;
            let resource = self.resources.get(key.transient_resource_id())?;
            if resource.kind != resource_kind {
                return None;
            }
            let binding = TransientObjectResourceBinding::from_resource(resource, &entry.binding.file_extension);
            (binding.matches_resource_identity(resource) && self.resource_is_valid_at(resource, now_ms))
                .then_some(binding)?
        };
        let entry = self.object_cache.get_mut(key)?;
        if !matches!(entry.status, TransientObjectCacheStatus::Ready { .. })
            || entry.binding != binding
            || (resource_kind == TransientResourceKind::Key && entry.ready_content_length() != Some(16))
            || entry.expires_at_ms < now_ms
        {
            return None;
        }
        entry.last_accessed_at_ms = now_ms;
        entry.access.reader_started(now_ms);
        entry.access.reader_finished();
        Some(entry.clone())
    }

    pub(super) fn current_resource_binding(
        &self,
        resource: &TransientResourceRef,
        file_extension: &str,
        now_ms: u64,
    ) -> Option<TransientObjectResourceBinding> {
        let current = self.resources.get(&resource.id)?;
        let binding = TransientObjectResourceBinding::from_resource(current, file_extension);
        (current.has_same_cache_identity(resource)
            && binding.matches_resource_identity(current)
            && self.resource_is_valid_at(current, now_ms))
        .then_some(binding)
    }

    pub(super) fn binding_is_current(&self, binding: &TransientObjectResourceBinding, now_ms: u64) -> bool {
        self.resources.get(&binding.resource_id).is_some_and(|resource| {
            binding.matches_resource_identity(resource) && self.resource_is_valid_at(resource, now_ms)
        })
    }

    pub fn begin_object_fetch(
        &mut self,
        proxy_session_id: &ProxySessionId,
        resource: &TransientResourceRef,
        file_ext: &str,
        now_ms: u64,
        cache_duration_ms: u64,
    ) -> TransientObjectFetchDecision {
        let lookup_key = Self::transient_object_key(proxy_session_id, &resource.id, file_ext.to_string());
        let current_binding = self.current_resource_binding(resource, file_ext, now_ms);
        let binding_is_current = current_binding.is_some();
        let binding =
            current_binding.unwrap_or_else(|| TransientObjectResourceBinding::from_resource(resource, file_ext));
        match self.object_cache.get(&lookup_key) {
            Some(entry)
                if binding_is_current
                    && entry.is_ready_at(now_ms)
                    && entry.binding == binding
                    && (resource.kind != TransientResourceKind::Key || entry.ready_content_length() == Some(16)) =>
            {
                return TransientObjectFetchDecision::Ready;
            }
            Some(entry)
                if binding_is_current
                    && entry.binding == binding
                    && entry.expires_at_ms >= now_ms
                    && matches!(entry.status, TransientObjectCacheStatus::Fetching { .. }) =>
            {
                let notifier =
                    self.object_fetch_notifiers.entry(lookup_key).or_insert_with(|| Arc::new(Notify::new())).clone();
                return TransientObjectFetchDecision::Wait(notifier);
            }
            Some(_) | None => {}
        }
        let expires_at_ms = transient_object_expires_at(now_ms, cache_duration_ms);
        let content_type = resource.content_type_hint.clone().unwrap_or_else(|| "application/octet-stream".to_string());
        let fetch_generation = self.next_object_fetch_generation;
        self.next_object_fetch_generation = self.next_object_fetch_generation.saturating_add(1);
        let cache_key = Self::transient_object_key(
            proxy_session_id,
            &resource.id,
            format!("{file_ext}.fill-{fetch_generation:016x}"),
        );
        let superseded_object = self.remove_object_entry(&lookup_key);
        let _previous = self.object_cache.insert(
            lookup_key.clone(),
            TransientObjectCacheEntry::new_fetching(
                cache_key.clone(),
                binding.clone(),
                now_ms,
                expires_at_ms,
                content_type,
            ),
        );
        self.object_fetch_notifiers.entry(lookup_key.clone()).or_insert_with(|| Arc::new(Notify::new()));
        let Some(entry) = self.object_cache.get(&lookup_key) else {
            return TransientObjectFetchDecision::Wait(
                self.object_fetch_notifiers.entry(lookup_key).or_insert_with(|| Arc::new(Notify::new())).clone(),
            );
        };
        TransientObjectFetchDecision::Fetch(Box::new(TransientObjectFetchToken {
            lookup_key,
            cache_key,
            entry_access: Arc::clone(&entry.access),
            binding,
            superseded_object,
        }))
    }

    pub(super) fn object_fetch_entry_matches(&self, token: &TransientObjectFetchToken) -> bool {
        self.object_cache.get(&token.lookup_key).is_some_and(|entry| {
            matches!(entry.status, TransientObjectCacheStatus::Fetching { .. })
                && entry.key == token.cache_key
                && Arc::ptr_eq(&entry.access, &token.entry_access)
                && entry.binding == token.binding
        })
    }

    pub fn object_fetch_token_matches(&self, token: &TransientObjectFetchToken) -> bool {
        self.object_fetch_entry_matches(token)
    }

    pub(super) fn mark_object_ready(
        &mut self,
        token: &TransientObjectFetchToken,
        content_type: String,
        content_length: u64,
        now_ms: u64,
        expires_at_ms: u64,
    ) {
        let notify_waiters = self.object_fetch_notifiers.remove(&token.lookup_key);
        if let Some(entry) = self.object_cache.get_mut(&token.lookup_key) {
            entry.status = TransientObjectCacheStatus::Ready { content_length, ready_at_ms: now_ms };
            entry.content_type = content_type;
            entry.last_accessed_at_ms = now_ms;
            entry.expires_at_ms = expires_at_ms;
        }
        if let Some(notifier) = notify_waiters {
            notifier.notify_waiters();
        }
    }

    pub fn mark_object_ready_if_current(
        &mut self,
        token: &TransientObjectFetchToken,
        content_type: String,
        content_length: u64,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> bool {
        if !self.object_fetch_entry_matches(token) {
            return false;
        }
        if !self.binding_is_current(&token.binding, now_ms) {
            let _removed = self.remove_object_entry(&token.lookup_key);
            return false;
        }
        self.mark_object_ready(token, content_type, content_length, now_ms, expires_at_ms);
        true
    }

    pub(super) fn mark_object_failed_retryable(
        &mut self,
        key: &TransientObjectCacheKey,
        now_ms: u64,
        retry_after_ms: u64,
    ) {
        let failed_expires_at_ms = self.failed_object_metadata_expires_at(now_ms);
        let notify_waiters = self.object_fetch_notifiers.remove(key);
        if let Some(entry) = self.object_cache.get_mut(key) {
            entry.status = TransientObjectCacheStatus::FailedRetryable { failed_at_ms: now_ms, retry_after_ms };
            entry.last_accessed_at_ms = now_ms;
            entry.expires_at_ms = entry.expires_at_ms.min(failed_expires_at_ms);
        }
        if let Some(notifier) = notify_waiters {
            notifier.notify_waiters();
        }
        self.enforce_failed_object_metadata_bound();
    }

    pub(super) fn mark_object_failed_permanent(
        &mut self,
        key: &TransientObjectCacheKey,
        now_ms: u64,
        status: Option<StatusCode>,
    ) {
        let failed_expires_at_ms = self.failed_object_metadata_expires_at(now_ms);
        let notify_waiters = self.object_fetch_notifiers.remove(key);
        if let Some(entry) = self.object_cache.get_mut(key) {
            entry.status = TransientObjectCacheStatus::FailedPermanent { failed_at_ms: now_ms, status };
            entry.last_accessed_at_ms = now_ms;
            entry.expires_at_ms = entry.expires_at_ms.min(failed_expires_at_ms);
        }
        if let Some(notifier) = notify_waiters {
            notifier.notify_waiters();
        }
        self.enforce_failed_object_metadata_bound();
    }

    pub(super) fn failed_object_metadata_expires_at(&self, now_ms: u64) -> u64 {
        now_ms.saturating_add(self.resource_ttl_ms)
    }

    pub fn mark_object_failed_retryable_if_current(
        &mut self,
        token: &TransientObjectFetchToken,
        now_ms: u64,
        retry_after_ms: u64,
    ) -> bool {
        if !self.object_fetch_entry_matches(token) {
            return false;
        }
        if !self.binding_is_current(&token.binding, now_ms) {
            let _removed = self.remove_object_entry(&token.lookup_key);
            return false;
        }
        self.mark_object_failed_retryable(&token.lookup_key, now_ms, retry_after_ms);
        true
    }

    pub fn mark_object_failed_permanent_if_current(
        &mut self,
        token: &TransientObjectFetchToken,
        now_ms: u64,
        status: Option<StatusCode>,
    ) -> bool {
        if !self.object_fetch_entry_matches(token) {
            return false;
        }
        if !self.binding_is_current(&token.binding, now_ms) {
            let _removed = self.remove_object_entry(&token.lookup_key);
            return false;
        }
        self.mark_object_failed_permanent(&token.lookup_key, now_ms, status);
        true
    }

    /// Returns the finite READY lifetime of an AES key dependency without touching access accounting.
    pub fn ready_key_object_valid_until_ms(
        &self,
        proxy_session_id: &ProxySessionId,
        resource_id: &TransientResourceId,
        file_ext: &str,
        now_ms: u64,
    ) -> Option<u64> {
        let resource = self.resources.get(resource_id)?;
        if resource.kind != TransientResourceKind::Key
            || resource.file_ext_hint.as_deref() != Some(file_ext)
            || !self.resource_is_valid_at(resource, now_ms)
        {
            return None;
        }
        let key = Self::transient_object_key(proxy_session_id, resource_id, file_ext.to_string());
        let entry = self.object_cache.get(&key)?;
        let binding = TransientObjectResourceBinding::from_resource(resource, file_ext);
        let valid_aes128_key = matches!(entry.status, TransientObjectCacheStatus::Ready { content_length: 16, .. });
        let valid_until_ms = if self.protected_finalized_resource_refcounts.contains_key(resource_id) {
            entry.expires_at_ms
        } else {
            entry.expires_at_ms.min(resource.expires_at_ms)
        };
        (valid_aes128_key && entry.binding == binding && entry.is_ready_at(now_ms)).then_some(valid_until_ms)
    }

    pub fn ready_object_cache_size(&self) -> u64 {
        self.object_cache.values().filter_map(TransientObjectCacheEntry::ready_content_length).sum()
    }

    pub fn take_expired_object_removals_except(
        &mut self,
        now_ms: u64,
        protected: &HashSet<TransientResourceId>,
        limit: usize,
    ) -> Vec<TransientObjectRemoval> {
        let mut keys = self
            .object_cache
            .iter()
            .filter_map(|(key, entry)| {
                let ready_is_protected =
                    entry.ready_content_length().is_some() && protected.contains(key.transient_resource_id());
                if !ready_is_protected && entry.access.active_readers() == 0 && entry.expires_at_ms < now_ms {
                    return Some((key.clone(), entry.expires_at_ms, entry.created_at_ms, key.stable_value()));
                }
                None
            })
            .collect::<Vec<_>>();
        keys.sort_by(|left, right| (left.1, left.2, &left.3).cmp(&(right.1, right.2, &right.3)));
        keys.into_iter().take(limit).filter_map(|(key, _, _, _)| self.remove_object_entry(&key)).collect()
    }

    pub fn remove_oldest_ready_object_except(
        &mut self,
        protected: &HashSet<TransientResourceId>,
    ) -> Option<TransientObjectRemoval> {
        let candidate = self
            .object_cache
            .iter()
            .filter_map(|(key, entry)| {
                if !protected.contains(key.transient_resource_id()) && entry.access.active_readers() == 0 {
                    return entry.ready_content_length().map(|content_length| {
                        (key.clone(), content_length, entry.last_accessed_at_ms, entry.created_at_ms)
                    });
                }
                None
            })
            .min_by_key(|(_, _, last_accessed_at_ms, created_at_ms)| (*last_accessed_at_ms, *created_at_ms))?;
        self.remove_object_entry(&candidate.0)
    }

    pub(super) fn remove_object_entry(
        &mut self,
        lookup_key: &TransientObjectCacheKey,
    ) -> Option<TransientObjectRemoval> {
        let entry = self.object_cache.remove(lookup_key)?;
        if let Some(notifier) = self.object_fetch_notifiers.remove(lookup_key) {
            notifier.notify_waiters();
        }
        let content_length = entry.ready_content_length().unwrap_or_default();
        Some(TransientObjectRemoval { key: entry.key, content_length })
    }

    pub(super) fn enforce_failed_object_metadata_bound(&mut self) {
        let mut failed = self
            .object_cache
            .iter()
            .filter_map(|(key, entry)| match entry.status {
                TransientObjectCacheStatus::FailedRetryable { failed_at_ms, .. }
                | TransientObjectCacheStatus::FailedPermanent { failed_at_ms, .. } => {
                    Some((key.clone(), failed_at_ms, entry.created_at_ms, key.stable_value()))
                }
                TransientObjectCacheStatus::Fetching { .. } | TransientObjectCacheStatus::Ready { .. } => None,
            })
            .collect::<Vec<_>>();
        let overflow = failed.len().saturating_sub(MAX_FAILED_TRANSIENT_OBJECT_ENTRIES);
        if overflow == 0 {
            return;
        }
        failed.sort_by(|left, right| (left.1, left.2, &left.3).cmp(&(right.1, right.2, &right.3)));
        for (key, _, _, _) in failed.into_iter().take(overflow) {
            let _removed = self.remove_object_entry(&key);
        }
    }
}

impl HlsSession {
    pub fn commit_transient_object_ready_if_current(
        &mut self,
        resource_kind: TransientResourceKind,
        token: &TransientObjectFetchToken,
        content_type: String,
        content_length: u64,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> bool {
        let committed =
            self.transient.mark_object_ready_if_current(token, content_type, content_length, now_ms, expires_at_ms);
        if committed && resource_kind == TransientResourceKind::Key {
            self.advance_media_readiness_generation();
        }
        committed
    }

    pub fn fail_transient_object_retryable_if_current(
        &mut self,
        token: &TransientObjectFetchToken,
        now_ms: u64,
        retry_after_ms: u64,
    ) -> bool {
        self.transient.mark_object_failed_retryable_if_current(token, now_ms, retry_after_ms)
    }

    pub fn fail_transient_object_permanent_if_current(
        &mut self,
        token: &TransientObjectFetchToken,
        now_ms: u64,
        status: Option<StatusCode>,
    ) -> bool {
        self.transient.mark_object_failed_permanent_if_current(token, now_ms, status)
    }
}
