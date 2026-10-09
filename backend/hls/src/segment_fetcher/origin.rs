use super::{
    begin_hls_origin_account_io_bounded, build_hls_origin_resource_headers, commit_startup_response,
    ensure_segment_key_dependency_ready, finish_hls_origin_account_io, hls_object_body_deadline,
    reliable_decoded_content_length, run_hls_origin_resource_retry_loop_with_attempt_prepare, CachedSegmentMetadata,
    HlsBoundAccountAcquireErrorKind, HlsOriginAccountIoLeaseGuard, HlsOriginByteRangeExpectation, HlsOriginIoContext,
    HlsOriginResourceBodyDeadline, HlsOriginResourceClients, HlsOriginResourceFetchTarget, HlsRepairRenderedObjectId,
    HlsResourceFetchKind, HlsResourceFetchSource, HlsSegmentRepairObjectContext, HlsSegmentRepairSource,
    SegmentFetchCommit, SegmentFetchContext, SegmentFetchError, SegmentFetchPolicy, SegmentFetchSnapshot,
    SegmentOriginWorkFinish,
};
use axum::http::HeaderMap;
use futures::FutureExt;
use tuliprox_core::utils::{content_coding::DecodedHttpResponse, current_time_millis};
use tuliprox_parser::hls::origin_manifest::ParsedByteRange;

pub(super) async fn fetch_segment_into_cache(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    policy: &SegmentFetchPolicy,
) -> Result<SegmentFetchCommit, SegmentFetchError> {
    ensure_segment_key_dependency_ready(context, snapshot, policy).await?;
    fetch_segment_with_retries_into_cache(context, snapshot, policy).await
}

pub(super) struct SegmentOriginAttemptGuard {
    started_generation: Option<u64>,
    provider_lease: Option<(HlsOriginIoContext, HlsOriginAccountIoLeaseGuard)>,
}

pub(super) async fn prepare_segment_origin_attempt(
    context: SegmentFetchContext,
    policy: SegmentFetchPolicy,
) -> Result<SegmentOriginAttemptGuard, SegmentFetchError> {
    let started_generation = start_segment_origin_work(&context).await;
    let binding =
        if context.origin_io.is_some() { context.session.read().await.origin_account_binding.clone() } else { None };
    let provider_lease = if let (Some(origin_io), Some(binding)) = (context.origin_io.as_ref(), binding.as_ref()) {
        if binding.is_detached() {
            finish_segment_origin_work(&context, started_generation).await;
            touch_segment_origin_account_binding(&context, false).await;
            return Err(SegmentFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::Detached));
        }
        let guard = match begin_hls_origin_account_io_bounded(
            origin_io,
            &context.session,
            binding,
            hls_object_body_deadline(policy.origin_segment_timeout_ms),
        )
        .await
        {
            Ok(guard) => guard,
            Err(err) => {
                finish_segment_origin_work(&context, started_generation).await;
                touch_segment_origin_account_binding(&context, false).await;
                return Err(SegmentFetchError::ProviderUnavailable(err));
            }
        };
        Some((origin_io.clone(), guard))
    } else {
        None
    };
    Ok(SegmentOriginAttemptGuard { started_generation, provider_lease })
}

pub(super) async fn finish_segment_origin_attempt(
    context: SegmentFetchContext,
    guard: SegmentOriginAttemptGuard,
) -> SegmentOriginWorkFinish {
    finish_segment_origin_io(&context, guard.started_generation, guard.provider_lease).await
}

async fn finish_segment_origin_io(
    context: &SegmentFetchContext,
    started_generation: Option<u64>,
    provider_lease: Option<(HlsOriginIoContext, HlsOriginAccountIoLeaseGuard)>,
) -> SegmentOriginWorkFinish {
    let origin_work = finish_segment_origin_work(context, started_generation).await;
    if let Some((origin_io, guard)) = provider_lease {
        finish_hls_origin_account_io(
            &origin_io,
            &context.session,
            guard,
            origin_work.generation_valid && origin_work.refresh_reservation,
        )
        .await;
        touch_segment_origin_account_binding(context, origin_work.generation_valid && origin_work.refresh_reservation)
            .await;
    }
    origin_work
}

async fn start_segment_origin_work(context: &SegmentFetchContext) -> Option<u64> {
    context.origin_io.as_ref()?;
    let mut session = context.session.write().await;
    Some(session.start_origin_work())
}

async fn finish_segment_origin_work(
    context: &SegmentFetchContext,
    started_generation: Option<u64>,
) -> SegmentOriginWorkFinish {
    let Some(started_generation) = started_generation else {
        return SegmentOriginWorkFinish { generation_valid: true, refresh_reservation: false };
    };
    let mut session = context.session.write().await;
    let generation_valid = session.finish_origin_work(started_generation);
    let refresh_reservation = session.should_refresh_origin_reservation(current_time_millis());
    SegmentOriginWorkFinish { generation_valid, refresh_reservation }
}

async fn touch_segment_origin_account_binding(context: &SegmentFetchContext, reservation_refreshed: bool) {
    let mut session = context.session.write().await;
    if let Some(binding) = session.origin_account_binding.as_mut() {
        let now_ms = current_time_millis();
        binding.last_origin_io_at_ms = Some(now_ms);
        if reservation_refreshed {
            binding.last_reservation_refresh_at_ms = Some(now_ms);
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(super) async fn fetch_segment_with_retries_into_cache(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    policy: &SegmentFetchPolicy,
) -> Result<SegmentFetchCommit, SegmentFetchError> {
    let headers = build_segment_origin_headers(
        &context.headers,
        &context.origin_provider_session_headers,
        snapshot.fetch_ref.byte_range,
    )?;
    let target = HlsOriginResourceFetchTarget {
        kind: HlsResourceFetchKind::Segment,
        source: HlsResourceFetchSource::Normal,
        object_id: snapshot.proxy_seq_log.clone(),
        origin_url: snapshot.fetch_ref.resolved_origin_url.clone(),
        headers,
        byte_range_expectation: if snapshot.fetch_ref.byte_range.is_some() {
            HlsOriginByteRangeExpectation::PartialContent
        } else {
            HlsOriginByteRangeExpectation::FullObject
        },
    };
    let clients = HlsOriginResourceClients {
        client: context.client.clone(),
        no_redirect_client: context.no_redirect_client.clone(),
        use_manual_redirects: context.use_manual_redirects,
    };
    let log_identity = {
        let session = context.session.read().await;
        super::super::HlsLogIdentity::from_session(&session)
    };
    let context = context.clone();
    let snapshot = snapshot.clone();
    let policy_for_prepare = policy.clone();
    let prepare_context = context.clone();
    let cleanup_context = context.clone();
    run_hls_origin_resource_retry_loop_with_attempt_prepare(
        target,
        clients,
        policy,
        &log_identity,
        move |_attempt| {
            let context = prepare_context.clone();
            let policy = policy_for_prepare.clone();
            async move { prepare_segment_origin_attempt(context, policy).await }.boxed()
        },
        move |guard| {
            let context = cleanup_context.clone();
            async move {
                finish_segment_origin_attempt(context, guard).await;
            }
            .boxed()
        },
        move |response, _attempt, body_deadline, guard| {
            let context = context.clone();
            let snapshot = snapshot.clone();
            async move {
                let commit_result =
                    commit_segment_response_into_cache(&context, &snapshot, response, body_deadline).await;
                let origin_work = finish_segment_origin_attempt(context, guard).await;
                commit_result.map(|metadata| SegmentFetchCommit {
                    content_length: metadata.size,
                    generation_valid: origin_work.generation_valid,
                })
            }
            .boxed()
        },
    )
    .await
}

async fn commit_segment_response_into_cache(
    context: &SegmentFetchContext,
    snapshot: &SegmentFetchSnapshot,
    response: DecodedHttpResponse,
    body_deadline: HlsOriginResourceBodyDeadline,
) -> Result<CachedSegmentMetadata, SegmentFetchError> {
    let (proxy_session_id, log_identity, accelerated) = {
        let session = context.session.read().await;
        (
            session.proxy_session_id.clone(),
            super::super::HlsLogIdentity::from_session(&session),
            session.startup.is_some(),
        )
    };
    let repair_context = HlsSegmentRepairObjectContext {
        source: HlsSegmentRepairSource::Normal,
        log_identity,
        proxy_session_id,
        hls_access_lease_id: context.repair_access_lease_id.clone(),
        rendered_object_id: HlsRepairRenderedObjectId::Normal { proxy_seq: snapshot.proxy_seq },
        resource_id: format!("{:06}", snapshot.proxy_seq),
        file_ext: snapshot.proxy_file_ext.clone(),
        // Segment repair uses the concrete fetch URL for diagnostics/postprocess metadata only.
        origin_fetch_uri_for_diagnostics: snapshot.fetch_ref.resolved_origin_url.clone(),
        media_sequence: Some(snapshot.origin_seq),
        discontinuity_sequence: None,
        complete_object: snapshot.complete_object,
        encrypted: snapshot.encryption.is_some(),
        custom_response: false,
    };
    if accelerated {
        return commit_startup_response(context, snapshot, response, body_deadline.deadline(), repair_context).await;
    }
    if let Some(content_length) = reliable_decoded_content_length(&response) {
        context
            .segment_cache
            .ensure_projected_write_capacity(&snapshot.cache_key, content_length)
            .await
            .map_err(|error| SegmentFetchError::cache_body(&error))?;
    }
    context
        .segment_repair
        .commit_origin_response(
            &context.segment_cache,
            &snapshot.cache_key,
            response.body,
            body_deadline.deadline(),
            repair_context,
        )
        .await
        .map_err(|err| SegmentFetchError::cache_body(&err))
}

pub(super) fn build_segment_origin_headers(
    source_headers: &HeaderMap,
    provider_session_headers: &HeaderMap,
    byte_range: Option<ParsedByteRange>,
) -> Result<HeaderMap, SegmentFetchError> {
    build_hls_origin_resource_headers(source_headers, provider_session_headers, byte_range)
}
