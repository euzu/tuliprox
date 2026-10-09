use super::{lease, manifest_snapshot, publish_manifest_snapshot, HlsAccessLeaseId, HlsAccessLeaseStore};
use crate::ProxySessionId;
use std::sync::Arc;

#[test]
fn retired_revision_bindings_cannot_be_rebound_after_the_hard_limit() -> std::io::Result<()> {
    let store = crate::SegmentRevisionStore::default();
    let session = ProxySessionId("limit".into());
    let mut lease = lease(HlsAccessLeaseId("bounded".into()), &session.0, 1000);
    for seq in 0..1030 {
        let revision = store.create(session.clone(), seq, crate::SegmentRevisionKind::Raw)?;
        revision.revision().prefix_available.store(1, std::sync::atomic::Ordering::Release);
        let mut snapshot = manifest_snapshot(seq);
        snapshot.startup_revisions = Some(Arc::new(crate::HlsManifestRevisions {
            mode: shared::model::HlsStartupMode::Progressive,
            retained_start_seq: 0,
            revisions: [(seq, revision)].into_iter().collect(),
        }));
        assert!(super::super::commit_lease_revision_bindings(&mut lease, &snapshot));
    }
    let bindings = lease.revision_bindings.as_ref().ok_or_else(|| std::io::Error::other("bindings"))?;
    assert_eq!(bindings.revisions.len(), 1024);
    assert_eq!(bindings.retired_through, Some(5));
    let new_attempt = store.create(session, 0, crate::SegmentRevisionKind::Raw)?;
    new_attempt.revision().prefix_available.store(1, std::sync::atomic::Ordering::Release);
    let mut old_uri = manifest_snapshot(1031);
    old_uri.startup_revisions = Some(Arc::new(crate::HlsManifestRevisions {
        mode: shared::model::HlsStartupMode::Progressive,
        retained_start_seq: 0,
        revisions: [(0, new_attempt)].into_iter().collect(),
    }));
    assert!(!super::super::commit_lease_revision_bindings(&mut lease, &old_uri));
    Ok(())
}

#[test]
fn repeated_lease_retirement_releases_publication_revision_guards() -> std::io::Result<()> {
    let session = ProxySessionId("lease-churn".into());
    let revisions = crate::SegmentRevisionStore::default();
    let mut leases = HlsAccessLeaseStore::default();
    for generation in 0..1000 {
        let lease_id = HlsAccessLeaseId(format!("lease-{generation}"));
        assert!(leases.prepare_access_lease(lease(lease_id.clone(), &session.0, 1000)));
        let mut owners = std::collections::BTreeMap::new();
        for seq in [40, 41] {
            let owner = revisions.create(session.clone(), seq, crate::SegmentRevisionKind::Raw)?;
            owner.revision().prefix_available.store(8, std::sync::atomic::Ordering::Release);
            owners.insert(seq, owner);
        }
        let mut snapshot = manifest_snapshot(generation + 1);
        snapshot.startup_revisions = Some(Arc::new(crate::HlsManifestRevisions {
            mode: shared::model::HlsStartupMode::Progressive,
            retained_start_seq: 40,
            revisions: owners,
        }));
        let guard = leases
            .prepare_manifest_publication(&lease_id, &session, 2000)
            .ok_or_else(|| std::io::Error::other("publication guard"))?;
        assert!(leases.commit_manifest_publication(&lease_id, &session, guard, snapshot, 2000).is_committed());
        assert!(revisions.retire_unpinned()?.is_empty());
        assert!(leases.remove_access_lease(&lease_id).is_some());
        assert_eq!(revisions.retire_unpinned()?.len(), 2);
    }
    assert!(leases.by_lease_id.is_empty());
    assert!(revisions.retire_unpinned()?.is_empty());
    Ok(())
}

#[test]
fn startup_claim_only_accepts_the_published_head_once() {
    let session = ProxySessionId("proxy".into());
    let lease_id = HlsAccessLeaseId("claim-lease".into());
    let mut leases = HlsAccessLeaseStore::default();
    leases.prepare_access_lease(lease(lease_id.clone(), &session.0, 1000));
    assert!(publish_manifest_snapshot(&mut leases, &lease_id, &session, manifest_snapshot(10), 2000).is_committed());
    assert!(!leases.try_claim_progressive_startup(&lease_id, 41, 2100));
    if let Some(lease) = leases.by_lease_id.get_mut(&lease_id) {
        lease.playback_cursor.first_requested_proxy_seq = Some(41);
    }
    assert!(leases.try_claim_progressive_startup(&lease_id, 40, 2101));
    assert!(!leases.try_claim_progressive_startup(&lease_id, 40, 2102));
    assert!(!leases.try_claim_progressive_startup(&lease_id, 41, 2102));
}
