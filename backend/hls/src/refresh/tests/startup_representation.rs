use super::{
    fetched_manifest, manifest_fetch_context, repeated_transient_manifest, test_manifest_origin_binding,
    test_origin_refresh_request, test_session, OriginManifestFetchError,
};
use crate::{HlsSessionMode, TransientPassthroughReason};
use std::sync::Arc;
use url::Url;

#[tokio::test]
async fn initial_local_representation_limit_does_not_enter_origin_recovery() {
    let session = test_session();
    session.write().await.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    let request = test_origin_refresh_request(Arc::clone(&session));
    let fetch_context = manifest_fetch_context(&request);
    let fetched =
        fetched_manifest(&repeated_transient_manifest(crate::manifest_limits::MAX_HLS_LEASE_SNAPSHOT_SEGMENTS + 1));
    let recovery_binding =
        test_manifest_origin_binding(Url::parse("http://127.0.0.1:9/recovery.m3u8").expect("test recovery URL"));

    let result =
        super::super::commit_initial_fetched_manifest(&request, &fetch_context, fetched, Some(recovery_binding), false)
            .await;

    assert!(matches!(
        result,
        Err(OriginManifestFetchError::LocalRepresentationLimit(violation))
            if violation.kind == crate::manifest_limits::HlsManifestLimitKind::LeaseSnapshotSegments
    ));
}
