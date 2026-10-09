use super::{
    build_transient_resource_id, extract_transient_resource_ids, transient_object_expires_at,
    HlsPublishedTransientResourceIds, HlsTransientManifestCommitError, TransientManifestLeaseBinding,
    TransientObjectCacheStatus, TransientPassthroughState, TransientResourceId, TransientResourceKind,
    TransientResourceRef, MAX_FAILED_TRANSIENT_OBJECT_ENTRIES,
};
use crate::{
    manifest_limits::{
        HlsManifestLimitKind, HlsManifestLimitViolation, MAX_ESTIMATED_TRANSIENT_METADATA_BYTES,
        MAX_RETAINED_FINALIZED_MANIFEST_GENERATIONS, MAX_TRANSIENT_GENERATION_MEMBERSHIPS,
        MAX_TRANSIENT_MANIFEST_RESOURCES, MAX_TRANSIENT_ORIGIN_URI_BYTES_PER_SESSION,
        MAX_TRANSIENT_RESOURCE_ENTRIES_PER_SESSION, MAX_TRANSIENT_REWRITTEN_MANIFEST_BYTES,
    },
    transient_manifest::{TransientManifestRewriter, TransientRewriteResult},
    HlsAccessLeaseId, ProxySessionId,
};
use std::{collections::HashSet, fmt::Write as _, sync::Arc, task::Poll};
use tuliprox_parser::hls::origin_manifest::{parse_manifest_semantics, HlsManifestLifecycle, HlsManifestWindowPolicy};

fn local_representation_limit(error: HlsTransientManifestCommitError) -> HlsManifestLimitViolation {
    let HlsTransientManifestCommitError::LocalRepresentationLimit(violation) = error else {
        panic!("expected local representation limit, got {error:?}");
    };
    violation
}

fn finalized_manifest_body(resource_id: &TransientResourceId, extension: &str) -> String {
    format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:1,\n/hls/shared/live/session/lease/r/{}.{extension}\n#EXT-X-ENDLIST\n",
            resource_id.0
        )
}

fn replace_with_finalized_manifest(
    state: &mut TransientPassthroughState,
    resource_id: &TransientResourceId,
    extension: &str,
    rendered_at_ms: u64,
) {
    state.replace_manifest_with_semantics(finalized_manifest_body(resource_id, extension), rendered_at_ms, Some(1_000));
}

fn rewritten_finalized_manifest(segment_count: usize, identity: &str) -> TransientRewriteResult {
    let mut body =
        String::from("#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:1\n");
    for index in 0..segment_count {
        writeln!(body, "#EXTINF:6,\n{identity}-{index}.ts").expect("synthetic manifest renders");
    }
    body.push_str("#EXT-X-ENDLIST\n");
    TransientManifestRewriter::rewrite(
        &body,
        "https://origin.example/archive/index.m3u8",
        &ProxySessionId("proxy-session".to_string()),
        b"secret",
        0,
        300_000,
    )
}

mod http;
mod playlist;
mod policy;
mod publication;
mod storage;
mod streaming;
mod transport;
