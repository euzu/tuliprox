use super::*;

#[test]
fn prospective_origin_uri_bytes_counts_duplicate_resource_ids_once() {
    let state = TransientPassthroughState::default();
    let resource = TransientResourceRef::new(
        TransientResourceKind::Segment,
        "https://origin.example/archive/segment.ts",
        b"secret",
        0,
        300_000,
        Some("ts".to_string()),
    );

    assert_eq!(
        state.prospective_origin_uri_bytes(&[resource.clone(), resource.clone()]),
        resource.resolved_origin_uri.len()
    );
}
