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
            window_plan: None,
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
        window_plan: None,
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
            window_plan: None,
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

fn startup_claim_fixture() -> std::io::Result<(HlsAccessLeaseStore, HlsAccessLeaseId)> {
    let session = ProxySessionId("proxy".into());
    let lease_id = HlsAccessLeaseId("claim-lease".into());
    let mut leases = HlsAccessLeaseStore::default();
    leases.prepare_access_lease(lease(lease_id.clone(), &session.0, 1000));
    let store = crate::SegmentRevisionStore::default();
    let mut revisions = std::collections::BTreeMap::new();
    for seq in [40, 41] {
        let revision = store.create(session.clone(), seq, crate::SegmentRevisionKind::Raw)?;
        revision.revision().prefix_available.store(1, std::sync::atomic::Ordering::Release);
        revisions.insert(seq, revision);
    }
    let mut snapshot = manifest_snapshot(10);
    snapshot.startup_revisions = Some(Arc::new(crate::HlsManifestRevisions {
        window_plan: None,
        mode: shared::model::HlsStartupMode::Optimistic,
        retained_start_seq: 40,
        revisions,
    }));
    assert!(publish_manifest_snapshot(&mut leases, &lease_id, &session, snapshot, 2000).is_committed());
    Ok((leases, lease_id))
}

#[test]
fn startup_claim_accepts_the_first_requested_published_segment_once() -> std::io::Result<()> {
    let (mut leases, lease_id) = startup_claim_fixture()?;
    assert!(!leases.try_claim_progressive_startup(&lease_id, 42, 2100), "unpublished URI cannot consume startup");
    assert!(leases.try_claim_progressive_startup(&lease_id, 41, 2101), "the player may start after the head");
    assert!(!leases.try_claim_progressive_startup(&lease_id, 40, 2102));
    assert!(!leases.try_claim_progressive_startup(&lease_id, 41, 2102));
    Ok(())
}

#[test]
fn startup_claim_accepts_a_retained_uri_after_the_playlist_head_moves() -> std::io::Result<()> {
    let (mut leases, lease_id) = startup_claim_fixture()?;
    let session = ProxySessionId("proxy".into());
    let mut snapshot = leases.by_lease_id[&lease_id].last_manifest_snapshot.clone().expect("snapshot");
    snapshot.first_proxy_seq = 41;
    snapshot.visible_segments = Arc::from([snapshot.visible_segments[1].clone()]);
    Arc::make_mut(snapshot.startup_revisions.as_mut().expect("revisions")).revisions.remove(&40);
    assert!(publish_manifest_snapshot(&mut leases, &lease_id, &session, snapshot, 2200).is_committed());
    assert!(leases.try_claim_progressive_startup(&lease_id, 40, 2201), "an earlier published URI is still bound");
    Ok(())
}

#[test]
fn each_lease_gets_its_own_startup_claim_for_the_shared_revision() -> std::io::Result<()> {
    let (mut leases, lease_id) = startup_claim_fixture()?;
    let other_id = HlsAccessLeaseId("other-lease".into());
    let session = ProxySessionId("proxy".into());
    let snapshot = leases.by_lease_id[&lease_id].last_manifest_snapshot.clone().expect("snapshot");
    assert!(leases.prepare_access_lease(lease(other_id.clone(), &session.0, 2000)));
    assert!(publish_manifest_snapshot(&mut leases, &other_id, &session, snapshot, 2100).is_committed());
    assert!(leases.try_claim_progressive_startup(&lease_id, 41, 2101));
    assert!(leases.try_claim_progressive_startup(&other_id, 41, 2102));
    assert!(!leases.try_claim_progressive_startup(&lease_id, 40, 2103));
    assert!(!leases.try_claim_progressive_startup(&other_id, 40, 2103));
    Ok(())
}

#[test]
fn startup_claim_cannot_be_revived_by_a_playlist_refresh_or_cursor_seek() -> std::io::Result<()> {
    let (mut leases, lease_id) = startup_claim_fixture()?;
    assert!(leases.try_claim_progressive_startup(&lease_id, 41, 2100));
    let session = ProxySessionId("proxy".into());
    let snapshot = leases.by_lease_id[&lease_id].last_manifest_snapshot.clone().expect("snapshot");
    assert!(publish_manifest_snapshot(&mut leases, &lease_id, &session, snapshot, 2200).is_committed());
    let _ = leases.by_lease_id.get_mut(&lease_id).expect("lease").playback_cursor.record_request_started(100, 2201);
    assert!(!leases.try_claim_progressive_startup(&lease_id, 40, 2202));
    Ok(())
}

#[test]
fn startup_claim_is_scoped_to_a_valid_live_lease() -> std::io::Result<()> {
    for state in [super::HlsAccessLeaseState::Denied, super::HlsAccessLeaseState::Expired] {
        let (mut leases, lease_id) = startup_claim_fixture()?;
        leases.by_lease_id.get_mut(&lease_id).expect("lease").state = state;
        assert!(!leases.try_claim_progressive_startup(&lease_id, 41, 2100));
    }
    let (mut leases, lease_id) = startup_claim_fixture()?;
    assert!(!leases.try_claim_progressive_startup(&lease_id, 41, 16_000));
    let (mut leases, lease_id) = startup_claim_fixture()?;
    leases.by_lease_id.get_mut(&lease_id).expect("lease").playback_mode = super::HlsLeasePlaybackMode::Ended;
    assert!(!leases.try_claim_progressive_startup(&lease_id, 41, 2100));
    Ok(())
}

#[tokio::test]
async fn concurrent_startup_claims_allow_exactly_one_request() -> std::io::Result<()> {
    let (leases, lease_id) = startup_claim_fixture()?;
    let leases = Arc::new(tokio::sync::RwLock::new(leases));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let claim = |seq| {
        let leases = Arc::clone(&leases);
        let barrier = Arc::clone(&barrier);
        let lease_id = lease_id.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            leases.write().await.try_claim_progressive_startup(&lease_id, seq, 2100)
        })
    };
    let (first, second) = tokio::join!(claim(40), claim(41));
    assert_ne!(first.expect("first task"), second.expect("second task"));
    Ok(())
}

#[test]
fn optimistic_binding_accepts_pending_successors_but_not_failed_or_replaced_ones() -> std::io::Result<()> {
    let store = crate::SegmentRevisionStore::default();
    let session = ProxySessionId("optimistic".into());
    let snapshot_with = |revisions: std::collections::BTreeMap<u64, crate::SegmentRevisionGuard>| {
        let mut snapshot = manifest_snapshot(1);
        snapshot.startup_revisions = Some(Arc::new(crate::HlsManifestRevisions {
            window_plan: None,
            mode: shared::model::HlsStartupMode::Optimistic,
            retained_start_seq: 0,
            revisions,
        }));
        snapshot
    };
    let mut owners = std::collections::BTreeMap::new();
    for seq in 0..3 {
        owners.insert(seq, store.create(session.clone(), seq, crate::SegmentRevisionKind::Raw)?);
    }
    owners
        .get(&0)
        .ok_or_else(|| std::io::Error::other("head"))?
        .revision()
        .prefix_available
        .store(1, std::sync::atomic::Ordering::Release);
    let mut bound = lease(HlsAccessLeaseId("optimistic".into()), &session.0, 1000);
    assert!(super::super::commit_lease_revision_bindings(&mut bound, &snapshot_with(owners.clone())));

    let mut replaced = owners.clone();
    replaced.insert(1, store.create(session.clone(), 1, crate::SegmentRevisionKind::Raw)?);
    assert!(!super::super::commit_lease_revision_bindings(&mut bound, &snapshot_with(replaced)));

    let mut fresh_lease = lease(HlsAccessLeaseId("failed".into()), &session.0, 1000);
    owners.get(&2).ok_or_else(|| std::io::Error::other("successor"))?.revision().fail();
    assert!(!super::super::commit_lease_revision_bindings(&mut fresh_lease, &snapshot_with(owners)));
    Ok(())
}

#[test]
fn failed_unexposed_successor_may_be_rebound_but_exposed_bytes_never_change() -> std::io::Result<()> {
    let store = crate::SegmentRevisionStore::default();
    let session = ProxySessionId("rebind".into());
    let snapshot_with = |revisions: std::collections::BTreeMap<u64, crate::SegmentRevisionGuard>| {
        let mut snapshot = manifest_snapshot(1);
        snapshot.startup_revisions = Some(Arc::new(crate::HlsManifestRevisions {
            window_plan: None,
            mode: shared::model::HlsStartupMode::Optimistic,
            retained_start_seq: 0,
            revisions,
        }));
        snapshot
    };
    let budget = crate::ProgressiveBudgetManager::new(tuliprox_core::model::HlsStartupConfig::default());
    let mut owners = std::collections::BTreeMap::new();
    for seq in 0..3 {
        owners.insert(seq, store.create(session.clone(), seq, crate::SegmentRevisionKind::Raw)?);
    }
    let head = owners.get(&0).ok_or_else(|| std::io::Error::other("head"))?.clone();
    let mut replay =
        crate::progressive_startup::ProgressiveReplay::new(&budget).ok_or_else(|| std::io::Error::other("slot"))?;
    replay.push(b"prefix")?;
    *head.revision().replay.lock().map_err(|_| std::io::Error::other("replay"))? = Some(replay);
    head.revision().prefix_available.store(6, std::sync::atomic::Ordering::Release);
    let mut bound = lease(HlsAccessLeaseId("rebind".into()), &session.0, 1000);
    assert!(super::super::commit_lease_revision_bindings(&mut bound, &snapshot_with(owners.clone())));

    // The origin fails a listed successor before any byte reached a client; its retry may replace it.
    owners.get(&1).ok_or_else(|| std::io::Error::other("successor"))?.revision().fail();
    let mut retried = owners.clone();
    retried.insert(1, store.create(session.clone(), 1, crate::SegmentRevisionKind::Raw)?);
    assert!(super::super::commit_lease_revision_bindings(&mut bound, &snapshot_with(retried.clone())));

    // A revision that streamed bytes stays bound even after it failed.
    assert!(head.revision().expose());
    head.revision().fail();
    let mut replaced_head = retried;
    replaced_head.insert(0, store.create(session, 0, crate::SegmentRevisionKind::Raw)?);
    assert!(!super::super::commit_lease_revision_bindings(&mut bound, &snapshot_with(replaced_head)));
    Ok(())
}
