use super::*;

#[test]
fn transient_rewritten_body_limit_rejects_before_commit() {
    let mut state = TransientPassthroughState::default();
    let oversized_body = "x".repeat(MAX_TRANSIENT_REWRITTEN_MANIFEST_BYTES + 1);

    let violation = local_representation_limit(
        state
            .commit_rewritten_manifest_with_semantics(
                oversized_body,
                Vec::new(),
                1,
                None,
                parse_manifest_semantics("#EXTM3U\n"),
            )
            .expect_err("rewritten body overflow is rejected"),
    );

    assert_eq!(violation.kind, HlsManifestLimitKind::TransientRewrittenBytes);
    assert!(state.last_manifest_body.is_none());
    assert!(state.resources.is_empty());
}
