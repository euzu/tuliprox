use super::*;

#[tokio::test]
async fn transient_resource_mappings_expire_unless_active() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let resource_id = {
        let mut session = session.write().await;
        let resource = crate::TransientResourceRef::new(
            crate::TransientResourceKind::Segment,
            "http://origin.example.com/live/seg.ts",
            b"secret",
            0,
            10,
            Some("ts".to_string()),
        );
        let resource_id = resource.id.clone();
        session.transient.upsert_resources([resource]);
        resource_id
    };

    let report = gc.run_once(20).await.expect("gc should run");

    assert_eq!(report.transient_resources_pruned, 1);
    assert!(!session.read().await.transient.resources.contains_key(&resource_id));
}
