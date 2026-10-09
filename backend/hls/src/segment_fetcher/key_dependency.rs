use super::{
    fetch_hls_transient_origin_response_with_attempt_prepare, finish_segment_origin_attempt, mark_segment_discovered,
    prepare_segment_origin_attempt, transient_object_expires_at, HlsOriginResourceClients, HlsSegmentEncryption,
    HlsTransientObjectFetchFinalizer, HlsTransientOriginFetchRequest, ProxySessionId, SegmentCacheKey,
    SegmentFetchContext, SegmentFetchError, SegmentFetchPolicy, SegmentFetchSnapshot, SegmentKeyFetchDependency,
    SegmentOriginWorkFinish, StagedCacheObject, TransientObjectFetchDecision, TransientObjectFetchToken,
    TransientObjectUnavailableState, TransientPassthroughState, TransientResourceFile, TransientResourceId,
    TransientResourceKind, TransientResourceRef,
};
use futures::FutureExt;
use log::warn;
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::Notify,
    time::{timeout_at, Instant},
};
use tuliprox_core::utils::current_time_millis;

#[derive(Clone)]
pub(super) struct SegmentKeyBindingSnapshot {
    pub(super) proxy_seq: u64,
    pub(super) cache_key: SegmentCacheKey,
    pub(super) origin_work_generation: u64,
}

impl From<&SegmentFetchSnapshot> for SegmentKeyBindingSnapshot {
    fn from(snapshot: &SegmentFetchSnapshot) -> Self {
        Self {
            proxy_seq: snapshot.proxy_seq,
            cache_key: snapshot.cache_key.clone(),
            origin_work_generation: snapshot.origin_work_generation,
        }
    }
}

pub(super) struct ReadySegmentKeyFetchSnapshot {
    pub(super) binding: SegmentKeyBindingSnapshot,
    pub(super) dependency: SegmentKeyFetchDependency,
}

#[derive(Clone)]
pub(super) enum SegmentKeyDependency {
    Fetch(Box<SegmentKeyFetchDependency>),
    Wait { notifier: Arc<Notify>, resource_id: TransientResourceId, resource_extension: String },
}

pub(super) enum SegmentKeyDependencySelection {
    Ready(Option<SegmentKeyDependency>),
    Unavailable,
}

pub(super) fn select_segment_key_dependency(
    session: &mut super::super::HlsSession,
    proxy_session_id: &ProxySessionId,
    proxy_seq: u64,
    encryption: Option<&HlsSegmentEncryption>,
    now_ms: u64,
) -> SegmentKeyDependencySelection {
    let Some(encryption) = encryption else {
        return SegmentKeyDependencySelection::Ready(None);
    };
    let selection = select_key_dependency(session, proxy_session_id, encryption, now_ms);
    if matches!(&selection, SegmentKeyDependencySelection::Unavailable) {
        mark_segment_discovered(session, proxy_seq);
    }
    selection
}

pub(super) fn select_key_dependency(
    session: &mut super::super::HlsSession,
    proxy_session_id: &ProxySessionId,
    encryption: &HlsSegmentEncryption,
    now_ms: u64,
) -> SegmentKeyDependencySelection {
    let Some(resource) = session.transient.resolve_current_resource(&encryption.resource_id, now_ms) else {
        return SegmentKeyDependencySelection::Unavailable;
    };
    if resource.kind != TransientResourceKind::Key
        || resource.file_ext_hint.as_deref() != Some(encryption.resource_extension.as_str())
    {
        return SegmentKeyDependencySelection::Unavailable;
    }
    let resource_file = TransientResourceFile {
        resource_id: encryption.resource_id.clone(),
        extension: encryption.resource_extension.clone(),
    };
    let cache_duration_ms = session.transient.resource_ttl_ms;
    let decision = session.transient.begin_object_fetch(
        proxy_session_id,
        &resource,
        &encryption.resource_extension,
        now_ms,
        cache_duration_ms,
    );
    let dependency = match decision {
        TransientObjectFetchDecision::Ready => None,
        TransientObjectFetchDecision::Fetch(token) => {
            Some(SegmentKeyDependency::Fetch(Box::new(SegmentKeyFetchDependency {
                token: *token,
                resource,
                resource_file,
            })))
        }
        TransientObjectFetchDecision::Wait(notifier) => Some(SegmentKeyDependency::Wait {
            notifier,
            resource_id: encryption.resource_id.clone(),
            resource_extension: encryption.resource_extension.clone(),
        }),
    };
    SegmentKeyDependencySelection::Ready(dependency)
}

pub(super) async fn ensure_segment_key_dependency_ready(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    policy: &SegmentFetchPolicy,
) -> Result<(), SegmentFetchError> {
    let Some(dependency) = snapshot.key_dependency.clone() else {
        return Ok(());
    };
    match dependency {
        SegmentKeyDependency::Wait { notifier, resource_id, resource_extension } => {
            wait_for_segment_key_dependency(
                context,
                notifier,
                resource_id,
                resource_extension,
                policy.origin_object_wait_timeout(),
            )
            .await
        }
        SegmentKeyDependency::Fetch(dependency) => {
            let SegmentKeyFetchDependency { token, resource, resource_file } = *dependency;
            let binding = SegmentKeyBindingSnapshot::from(snapshot);
            fetch_segment_key_dependency_into_cache(context, &binding, policy, token, resource, resource_file).await
        }
    }
}

pub(super) async fn wait_for_segment_key_dependency(
    context: &SegmentFetchContext,
    notifier: Arc<Notify>,
    resource_id: TransientResourceId,
    resource_extension: String,
    wait_timeout: Duration,
) -> Result<(), SegmentFetchError> {
    let deadline = Instant::now() + wait_timeout;
    loop {
        let wake = notifier.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let should_wait = {
            let now_ms = current_time_millis();
            let session = context.session.read().await;
            if session
                .transient
                .ready_key_object_valid_until_ms(&session.proxy_session_id, &resource_id, &resource_extension, now_ms)
                .is_some()
            {
                return Ok(());
            }
            let key = TransientPassthroughState::transient_object_key(
                &session.proxy_session_id,
                &resource_id,
                resource_extension.clone(),
            );
            matches!(
                session.transient.object_unavailable_state(&key, now_ms),
                TransientObjectUnavailableState::Fetching
            )
        };
        if !should_wait {
            return Err(SegmentFetchError::Timeout);
        }
        timeout_at(deadline, wake).await.map_err(|_| SegmentFetchError::Timeout)?;
    }
}

pub(super) async fn fetch_segment_key_dependency_into_cache(
    context: &SegmentFetchContext,
    binding: &SegmentKeyBindingSnapshot,
    policy: &SegmentFetchPolicy,
    token: TransientObjectFetchToken,
    resource: TransientResourceRef,
    resource_file: TransientResourceFile,
) -> Result<(), SegmentFetchError> {
    let mut fetch_finalizer = HlsTransientObjectFetchFinalizer::new(
        context.session.clone(),
        Arc::clone(&context.segment_cache),
        token.clone(),
        1_000,
    );
    let (staged, origin_work) =
        stage_segment_key_dependency(context, policy, &token, &resource, &resource_file).await?;
    let generation_valid = origin_work.generation_valid && {
        let session = context.session.read().await;
        segment_key_dependency_generation_matches(
            &session,
            binding,
            &token,
            &resource,
            &resource_file,
            current_time_millis(),
        )
    };
    if !generation_valid {
        if let Err(err) = context.segment_cache.remove_staged(staged).await {
            warn!("HLS stale staged AES key cleanup failed: error={err}");
        }
        context.session.write().await.fail_transient_object_retryable_if_current(&token, current_time_millis(), 1_000);
        return Err(SegmentFetchError::Timeout);
    }
    let metadata = match context.segment_cache.commit_staged(token.cache_key(), staged).await {
        Ok(metadata) => metadata,
        Err(err) => {
            context.session.write().await.fail_transient_object_retryable_if_current(
                &token,
                current_time_millis(),
                1_000,
            );
            return Err(SegmentFetchError::cache_commit(&err));
        }
    };
    let ready_at_ms = current_time_millis();
    super::super::transient_fetcher::delete_superseded_transient_object(&context.segment_cache, &token).await;
    let mut session = context.session.write().await;
    if !segment_key_dependency_generation_matches(&session, binding, &token, &resource, &resource_file, ready_at_ms) {
        session.fail_transient_object_retryable_if_current(&token, ready_at_ms, 1_000);
        return Err(SegmentFetchError::Timeout);
    }
    let expires_at_ms = transient_object_expires_at(ready_at_ms, session.transient.resource_ttl_ms);
    if !session.commit_transient_object_ready_if_current(
        TransientResourceKind::Key,
        &token,
        resource.content_type_hint.clone().unwrap_or_else(|| "application/octet-stream".to_string()),
        metadata.size,
        ready_at_ms,
        expires_at_ms,
    ) {
        return Err(SegmentFetchError::Timeout);
    }
    fetch_finalizer.complete();
    Ok(())
}

async fn stage_segment_key_dependency(
    context: &SegmentFetchContext,
    policy: &SegmentFetchPolicy,
    token: &TransientObjectFetchToken,
    resource: &TransientResourceRef,
    resource_file: &TransientResourceFile,
) -> Result<(StagedCacheObject, SegmentOriginWorkFinish), SegmentFetchError> {
    let request = HlsTransientOriginFetchRequest {
        resolved_origin_uri: resource.resolved_origin_uri.clone(),
        origin_headers: context.headers.clone(),
        origin_provider_session_headers: context.origin_provider_session_headers.clone(),
        range_header: None,
        resource_file: resource_file.clone(),
        resource_kind: TransientResourceKind::Key,
        clients: HlsOriginResourceClients {
            client: context.client.clone(),
            no_redirect_client: context.no_redirect_client.clone(),
            use_manual_redirects: context.use_manual_redirects,
        },
        policy: policy.clone(),
        log_identity: {
            let session = context.session.read().await;
            super::super::HlsLogIdentity::from_session(&session)
        },
    };
    let prepare_context = context.clone();
    let prepare_policy = policy.clone();
    let response = fetch_hls_transient_origin_response_with_attempt_prepare(request, move |_attempt| {
        let context = prepare_context.clone();
        let policy = prepare_policy.clone();
        async move { prepare_segment_origin_attempt(context, policy).await }.boxed()
    })
    .await;
    let response = match response {
        Ok(response) => response,
        Err(err) => {
            context.session.write().await.fail_transient_object_retryable_if_current(
                token,
                current_time_millis(),
                1_000,
            );
            return Err(err);
        }
    };
    let staged = context
        .segment_cache
        .stage_temp_with_deadline(token.cache_key(), response.decoded.body, response.body_deadline.deadline())
        .await;
    let origin_work = finish_segment_origin_attempt(context.clone(), response.guard).await;
    let staged = match staged {
        Ok(staged) if staged.size == 16 => staged,
        Ok(staged) => {
            if let Err(err) = context.segment_cache.remove_staged(staged).await {
                warn!("HLS invalid staged AES key cleanup failed: error={err}");
            }
            context.session.write().await.fail_transient_object_permanent_if_current(
                token,
                current_time_millis(),
                Some(axum::http::StatusCode::BAD_GATEWAY),
            );
            return Err(SegmentFetchError::NonRetryableStatus(axum::http::StatusCode::BAD_GATEWAY));
        }
        Err(err) => {
            context.session.write().await.fail_transient_object_retryable_if_current(
                token,
                current_time_millis(),
                1_000,
            );
            return Err(SegmentFetchError::cache_body(&err));
        }
    };
    Ok((staged, origin_work))
}

fn segment_key_dependency_generation_matches(
    session: &super::super::HlsSession,
    binding: &SegmentKeyBindingSnapshot,
    token: &TransientObjectFetchToken,
    resource: &TransientResourceRef,
    resource_file: &TransientResourceFile,
    now_ms: u64,
) -> bool {
    if session.activity.origin_work_generation != binding.origin_work_generation
        || !session.transient.object_fetch_token_matches(token)
    {
        return false;
    }
    let segment_matches = session.segments.get(&binding.proxy_seq).is_some_and(|segment| {
        segment.cache_key == binding.cache_key
            && segment.encryption.as_ref().is_some_and(|encryption| {
                encryption.resource_id == resource_file.resource_id
                    && encryption.resource_extension == resource_file.extension
            })
    });
    let resource_matches = resource.id == resource_file.resource_id
        && resource.kind == TransientResourceKind::Key
        && resource.file_ext_hint.as_deref() == Some(resource_file.extension.as_str())
        && session.transient.resource_matches_current(resource, now_ms);
    segment_matches && resource_matches
}
