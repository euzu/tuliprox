use super::{
    build_segment_origin_headers, run_hls_origin_resource_retry_loop_with_attempt_prepare, HlsCacheObjectKey,
    HlsOriginByteRangeExpectation, HlsOriginResourceClients, HlsOriginResourceFetchError, HlsOriginResourceFetchTarget,
    HlsResourceFetchKind, HlsResourceFetchSource, HlsSwitchResourceKind, SegmentFetchContext, SegmentFetchPolicy,
    StagedCacheObject,
};
use futures::FutureExt;
use log::warn;
use std::sync::Arc;
use tuliprox_parser::hls::origin_manifest::ParsedByteRange;

/// Fetches one candidate-bound switch resource into an uncommitted cache object.
///
/// The caller owns the returned object and must either commit it to the previewed cache key or remove it. This helper
/// shares the normal origin-resource Range/content-coding/retry implementation and performs no session mutation.
pub async fn stage_hls_switch_resource<K>(
    context: &SegmentFetchContext,
    policy: &SegmentFetchPolicy,
    cache_key: K,
    origin_url: String,
    byte_range: Option<ParsedByteRange>,
    kind: HlsSwitchResourceKind,
) -> Result<StagedCacheObject, HlsOriginResourceFetchError>
where
    K: HlsCacheObjectKey + Clone + Send + Sync + 'static,
{
    let headers = build_segment_origin_headers(&context.headers, &context.origin_provider_session_headers, byte_range)?;
    let resource_kind = match kind {
        HlsSwitchResourceKind::Segment => HlsResourceFetchKind::Segment,
        HlsSwitchResourceKind::Map => HlsResourceFetchKind::Map,
    };
    let target = HlsOriginResourceFetchTarget {
        kind: resource_kind,
        source: HlsResourceFetchSource::Normal,
        object_id: match kind {
            HlsSwitchResourceKind::Segment => "switch-segment".to_string(),
            HlsSwitchResourceKind::Map => "switch-map".to_string(),
        },
        origin_url,
        headers,
        byte_range_expectation: if byte_range.is_some() {
            HlsOriginByteRangeExpectation::PartialContent
        } else {
            HlsOriginByteRangeExpectation::FullObject
        },
        media_duration_ms: None,
    };
    let clients = HlsOriginResourceClients {
        retry_work: crate::HlsOriginRetryWork::for_resource(
            &context.session,
            context.origin_io.as_ref().map(|io| &io.ctx.active_provider),
            policy,
            resource_kind,
            HlsResourceFetchSource::Normal,
        )
        .await,
        session: Some(context.session.clone()),
        client: context.client.clone(),
        no_redirect_client: context.no_redirect_client.clone(),
        use_manual_redirects: context.use_manual_redirects,
    };
    let log_identity = {
        let session = context.session.read().await;
        super::super::HlsLogIdentity::from_session(&session)
    };
    let cache = Arc::clone(&context.segment_cache);
    // The first cache write scans the whole cache directory; do it before any per-attempt deadline starts.
    cache.prepare_capacity_accounting().await.map_err(|error| HlsOriginResourceFetchError::cache_body(&error))?;
    run_hls_origin_resource_retry_loop_with_attempt_prepare(
        target,
        clients,
        policy,
        &log_identity,
        |_attempt| async { Ok(()) }.boxed(),
        |()| async {}.boxed(),
        move |response, _attempt, body_deadline, ()| {
            let cache = Arc::clone(&cache);
            let cache_key = cache_key.clone();
            async move {
                let staged = cache
                    .stage_temp_with_deadline(&cache_key, response.body, body_deadline.deadline())
                    .await
                    .map_err(|err| HlsOriginResourceFetchError::cache_body(&err))?;
                if staged.size == 0 {
                    if let Err(err) = cache.remove_staged(staged).await {
                        warn!("HLS empty staged switch resource cleanup failed: error={err}");
                    }
                    return Err(HlsOriginResourceFetchError::cache_body(&std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "empty switch resource",
                    )));
                }
                Ok(staged)
            }
            .boxed()
        },
    )
    .await
}
