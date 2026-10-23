use super::*;
use crate::lease::HlsRevisionRecoveryOutcome;

async fn permanent_failure_with_counted_user(fixture: &CachedSegmentFixture) -> std::io::Result<()> {
    failed_optimistic_revision(fixture, 6).await?;
    assert!(fixture
        .manager
        .activate_access_lease(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            current_time_millis(),
            crate::HlsAccessLeaseTiming { active_window_ms: 60_000, valid_window_ms: 60_000 },
        )
        .await
        .is_activated());
    fixture.session.write().await.segments.get_mut(&fixture.segment_file.proxy_seq).expect("entry").status =
        SegmentCacheStatus::FailedPermanent { failed_at_ms: current_time_millis(), status: None };
    let lease = fixture
        .manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
        .await
        .expect("lease");
    let mut user = tuliprox_core::model::ProxyUserCredentials::default();
    user.username = lease.username.clone();
    user.max_connections = 1;
    fixture
        .active_users
        .create_user_session(tuliprox_session::CreateUserSessionParams {
            user: &user,
            session_token: &lease.user_session_token,
            virtual_id: 1,
            provider: "synthetic-provider",
            stream_url: "http://synthetic.invalid/live.m3u8",
            addr: &"127.0.0.1:12345".parse().expect("address"),
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(tuliprox_session::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    fixture.active_users.mark_grace_active(&lease.username, &lease.user_session_token).await;
    let version =
        fixture.active_users.grace_active_version(&lease.username, &lease.user_session_token).await.expect("version");
    fixture.active_users.activate_grace_active(&lease.username, &lease.user_session_token, version).await;
    assert_eq!(fixture.active_users.user_connections(&lease.username).await, 1);
    Ok(())
}

#[tokio::test]
async fn reentry_completion_releases_the_counted_user_reservation() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    permanent_failure_with_counted_user(&fixture).await?;
    assert_eq!(
        fixture.active_users.connection_permission("test-user", 1, 0).await,
        shared::model::UserConnectionPermission::Exhausted
    );
    assert_eq!(recovery(&fixture).await, HlsRevisionRecoveryOutcome::ReentryRequired);
    assert_eq!(fixture.active_users.user_connections("test-user").await, 0);
    assert_eq!(
        fixture.active_users.connection_permission("test-user", 1, 0).await,
        shared::model::UserConnectionPermission::Allowed
    );
    Ok(())
}

#[tokio::test]
async fn reentry_completion_preserves_another_counted_session_of_the_same_user() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    permanent_failure_with_counted_user(&fixture).await?;
    let mut user = tuliprox_core::model::ProxyUserCredentials::default();
    user.username = "test-user".into();
    user.max_connections = 2;
    let token = "other-playback";
    fixture
        .active_users
        .create_user_session(tuliprox_session::CreateUserSessionParams {
            user: &user,
            session_token: token,
            virtual_id: 2,
            provider: "synthetic-provider",
            stream_url: "http://synthetic.invalid/other.m3u8",
            addr: &"127.0.0.1:12346".parse().expect("address"),
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(tuliprox_session::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    fixture.active_users.mark_grace_active(&user.username, token).await;
    let version = fixture.active_users.grace_active_version(&user.username, token).await.expect("version");
    fixture.active_users.activate_grace_active(&user.username, token, version).await;
    assert_eq!(fixture.active_users.user_connections(&user.username).await, 2);

    assert_eq!(recovery(&fixture).await, HlsRevisionRecoveryOutcome::ReentryRequired);
    assert_eq!(fixture.active_users.user_connections(&user.username).await, 1);
    assert!(fixture.active_users.release_session_streams_and_counted_reservation(&user.username, token).await);
    assert_eq!(fixture.active_users.user_connections(&user.username).await, 0);
    Ok(())
}

#[tokio::test]
async fn normal_lifecycle_completion_uses_the_same_counted_user_release() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    permanent_failure_with_counted_user(&fixture).await?;
    let expires = fixture
        .manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
        .await
        .expect("lease")
        .valid_until_ms
        + 1;
    let ctx = crate::HlsCtx::for_test(tuliprox_core::model::Config::default());
    fixture
        .manager
        .handle_lifecycle_event(
            &fixture.active_users,
            &ctx.active_provider,
            crate::HlsLifecycleEvent {
                key: crate::HlsLifecycleEventKey::AccessLeaseValidity {
                    lease_id: fixture.lease_id.clone(),
                    proxy_session_id: fixture.proxy_session_id.clone(),
                },
                due_at_ms: expires,
            },
            expires,
        )
        .await;
    assert_eq!(fixture.active_users.user_connections("test-user").await, 0);
    assert!(fixture
        .manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, expires,)
        .await
        .is_none());
    Ok(())
}

#[tokio::test]
async fn reentry_completion_survives_request_cancellation_during_cleanup() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    permanent_failure_with_counted_user(&fixture).await?;
    fixture
        .manager
        .qos()
        .ensure_access_lease(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis(), None)
        .await;
    fixture.manager.segment_repair().update_config(tuliprox_core::model::HlsSegmentRepairConfig {
        max_level: shared::model::HlsSegmentRepairMode::Low,
        apply_to_first_segments: 1,
        ..Default::default()
    });
    fixture.manager.segment_repair().start_access_lease_window(fixture.lease_id.clone()).await;
    let reader = fixture.session.read().await;
    {
        let request = recovery(&fixture);
        tokio::pin!(request);
        assert!(futures::poll!(request.as_mut()).is_pending());
        tokio::time::timeout(Duration::from_secs(1), async {
            while fixture.manager.access_leases().try_read().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup holds its lease fence while waiting for the session writer");
        assert_eq!(fixture.manager.pending_lease_completions_for_test(), 1);
        // The request deadline drops this future while cleanup is blocked.
    }
    drop(reader);
    let settled = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if fixture.manager.qos().is_empty().await
                && fixture.manager.segment_repair().stats().await.windows == 0
                && fixture.manager.pending_lease_completions_for_test() == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(settled.is_ok(), "cancelled cleanup left QoS or repair state");
    assert_eq!(fixture.active_users.user_connections("test-user").await, 0);
    assert!(fixture
        .manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
        .await
        .is_none());
    Ok(())
}

#[tokio::test]
async fn concurrent_reentry_waiters_share_completion_when_one_is_cancelled() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    permanent_failure_with_counted_user(&fixture).await?;
    let guard = fixture
        .manager
        .access_leases()
        .read()
        .await
        .revision_recovery_guard(&fixture.lease_id, fixture.segment_file.proxy_seq)
        .expect("guard");
    let reader = fixture.session.read().await;
    let second = fixture.manager.end_access_lease_for_failed_revision(
        &fixture.lease_id,
        &fixture.proxy_session_id,
        &guard,
        current_time_millis(),
    );
    tokio::pin!(second);
    {
        let first = fixture.manager.end_access_lease_for_failed_revision(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            &guard,
            current_time_millis(),
        );
        tokio::pin!(first);
        assert!(futures::poll!(first.as_mut()).is_pending());
        assert!(futures::poll!(second.as_mut()).is_pending());
        assert_eq!(fixture.manager.pending_lease_completions_for_test(), 1);
    }
    drop(reader);
    assert!(tokio::time::timeout(Duration::from_secs(1), second).await.expect("owned completion"));
    assert_eq!(fixture.active_users.user_connections("test-user").await, 0);
    assert_eq!(fixture.manager.pending_lease_completions_for_test(), 0);
    assert!(
        !fixture
            .manager
            .end_access_lease_for_failed_revision(
                &fixture.lease_id,
                &fixture.proxy_session_id,
                &guard,
                current_time_millis(),
            )
            .await
    );
    Ok(())
}

#[tokio::test]
async fn stale_reentry_job_preserves_new_incarnation_and_its_user_reservation() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    permanent_failure_with_counted_user(&fixture).await?;
    let mut leases = fixture.manager.access_leases().write().await;
    let guard = leases.revision_recovery_guard(&fixture.lease_id, fixture.segment_file.proxy_seq).expect("guard");
    let mut replacement =
        leases.response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis()).expect("lease");
    replacement.issued_at_ms += 1;
    let replacement_issued_at = replacement.issued_at_ms;
    assert!(leases.prepare_access_lease(replacement));
    drop(leases);
    fixture
        .manager
        .qos()
        .ensure_access_lease(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis(), None)
        .await;
    assert!(
        !fixture
            .manager
            .end_access_lease_for_failed_revision(
                &fixture.lease_id,
                &fixture.proxy_session_id,
                &guard,
                current_time_millis(),
            )
            .await
    );
    assert_eq!(fixture.active_users.user_connections("test-user").await, 1);
    assert_eq!(fixture.manager.qos().len().await, 1);
    assert_eq!(
        fixture
            .manager
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis(),)
            .await
            .expect("new incarnation")
            .issued_at_ms,
        replacement_issued_at
    );
    Ok(())
}

#[tokio::test]
async fn cancelled_reentry_waiting_for_accounting_preserves_replacement_lease() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::without_lease_user_accounting(b"retry-body".to_vec()).await;
    permanent_failure_with_counted_user(&fixture).await?;
    let guard = fixture
        .manager
        .access_leases()
        .read()
        .await
        .revision_recovery_guard(&fixture.lease_id, fixture.segment_file.proxy_seq)
        .expect("guard");
    {
        let request = fixture.manager.end_access_lease_for_failed_revision(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            &guard,
            current_time_millis(),
        );
        tokio::pin!(request);
        assert!(futures::poll!(request.as_mut()).is_pending());
        tokio::task::yield_now().await;
        assert_eq!(fixture.manager.pending_lease_completions_for_test(), 1);
    }
    // The owned job outlives its caller while waiting for runtime attachment.
    // It must revalidate the lease after attachment, before releasing anything.
    let replacement_issued_at = {
        let mut leases = fixture.manager.access_leases().write().await;
        let mut replacement = leases
            .response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
            .expect("lease");
        replacement.issued_at_ms += 1;
        let issued_at = replacement.issued_at_ms;
        assert!(leases.prepare_access_lease(replacement));
        issued_at
    };
    fixture.manager.bind_lease_user_accounting(&fixture.active_users);
    tokio::time::timeout(Duration::from_secs(1), async {
        while fixture.manager.pending_lease_completions_for_test() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stale completion settles after runtime attachment");
    assert_eq!(fixture.active_users.user_connections("test-user").await, 1);
    assert_eq!(
        fixture
            .manager
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
            .await
            .expect("replacement lease")
            .issued_at_ms,
        replacement_issued_at
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_lifecycle_and_reentry_cleanup_survive_cancellation_of_all_waiters() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    permanent_failure_with_counted_user(&fixture).await?;
    fixture
        .manager
        .qos()
        .ensure_access_lease(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis(), None)
        .await;
    fixture.manager.segment_repair().update_config(tuliprox_core::model::HlsSegmentRepairConfig {
        max_level: shared::model::HlsSegmentRepairMode::Low,
        apply_to_first_segments: 1,
        ..Default::default()
    });
    fixture.manager.segment_repair().start_access_lease_window(fixture.lease_id.clone()).await;
    let (expires_at, guard) = {
        let mut leases = fixture.manager.access_leases().write().await;
        let expires_at = leases
            .response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
            .expect("lease")
            .valid_until_ms
            + 1;
        let guard = leases.revision_recovery_guard(&fixture.lease_id, fixture.segment_file.proxy_seq).expect("guard");
        (expires_at, guard)
    };
    let ctx = crate::HlsCtx::for_test(tuliprox_core::model::Config::default());
    let session_reader = fixture.session.read().await;
    {
        let lifecycle = fixture.manager.handle_lifecycle_event(
            &fixture.active_users,
            &ctx.active_provider,
            crate::HlsLifecycleEvent {
                key: crate::HlsLifecycleEventKey::AccessLeaseValidity {
                    lease_id: fixture.lease_id.clone(),
                    proxy_session_id: fixture.proxy_session_id.clone(),
                },
                due_at_ms: expires_at,
            },
            expires_at,
        );
        let reentry = fixture.manager.end_access_lease_for_failed_revision(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            &guard,
            current_time_millis(),
        );
        tokio::pin!(lifecycle, reentry);
        assert!(futures::poll!(lifecycle.as_mut()).is_pending());
        assert!(futures::poll!(reentry.as_mut()).is_pending());
        tokio::time::timeout(Duration::from_secs(1), async {
            while fixture.manager.access_leases().try_read().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup waits for the session writer while holding its lease fence");
        assert_eq!(fixture.manager.pending_lease_completions_for_test(), 2);
        // Both caller futures are cancelled while their owned jobs are blocked.
    }
    drop(session_reader);
    tokio::time::timeout(Duration::from_secs(1), async {
        while fixture.manager.pending_lease_completions_for_test() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned cleanup settles after all waiters are cancelled");
    assert_eq!(fixture.active_users.user_connections("test-user").await, 0);
    assert!(fixture.manager.qos().is_empty().await);
    assert_eq!(fixture.manager.segment_repair().stats().await.windows, 0);
    assert!(fixture
        .manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
        .await
        .is_none());
    Ok(())
}

async fn failed_optimistic_revision(
    fixture: &CachedSegmentFixture,
    prefix: u64,
) -> std::io::Result<crate::SegmentRevisionGuard> {
    let revision = publish_revision_fixture(fixture, true).await?;
    let mut session = fixture.session.write().await;
    session.startup.as_mut().expect("startup").config.mode = shared::model::HlsStartupMode::Optimistic;
    drop(session);
    let mut leases = fixture.manager.access_leases().write().await;
    let mut snapshot = leases
        .response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
        .expect("lease")
        .last_manifest_snapshot
        .expect("snapshot");
    Arc::make_mut(snapshot.startup_revisions.as_mut().expect("revisions")).mode =
        shared::model::HlsStartupMode::Optimistic;
    let guard = leases
        .prepare_manifest_publication(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
        .expect("guard");
    assert!(leases
        .commit_manifest_publication(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            guard,
            snapshot,
            current_time_millis()
        )
        .is_committed());
    revision.revision().prefix_available.store(prefix, Ordering::Release);
    revision.revision().fail();
    drop(leases);
    let mut session = fixture.session.write().await;
    session.segments.get_mut(&fixture.segment_file.proxy_seq).expect("entry").status = SegmentCacheStatus::Fetching {
        priority: crate::SegmentFetchPriority::Demand,
        started_at_ms: current_time_millis(),
    };
    Ok(revision)
}

async fn install_completed_retry(fixture: &CachedSegmentFixture) -> std::io::Result<crate::SegmentRevisionGuard> {
    let mut session = fixture.session.write().await;
    let startup = session.startup.as_ref().expect("startup");
    let retry = startup.store.create(
        fixture.proxy_session_id.clone(),
        fixture.segment_file.proxy_seq,
        crate::SegmentRevisionKind::Raw,
    )?;
    retry.revision().bind_origin(&session);
    let entry = session.segments.get_mut(&fixture.segment_file.proxy_seq).expect("entry");
    entry.status = SegmentCacheStatus::Ready { content_length: fixture.full_size, ready_at_ms: current_time_millis() };
    assert!(retry.revision().complete(crate::CachedSegmentMetadata {
        path: fixture.segment_cache.object_path(&entry.cache_key),
        size: fixture.full_size,
    }));
    session.startup.as_mut().expect("startup").revisions.insert(fixture.segment_file.proxy_seq, retry.clone());
    Ok(retry)
}

async fn recovery(fixture: &CachedSegmentFixture) -> HlsRevisionRecoveryOutcome {
    let marker = fixture.context.media_activity_marker.as_ref().expect("marker");
    fixture
        .manager
        .recover_access_lease_revision(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            marker.lease_identity,
            fixture.segment_file.proxy_seq,
            current_time_millis(),
        )
        .await
}

async fn install_spooled_retry(fixture: &CachedSegmentFixture) -> std::io::Result<crate::SegmentRevisionGuard> {
    use tokio::io::AsyncWriteExt;
    let (store, cache_key) = {
        let session = fixture.session.read().await;
        (
            session.startup.as_ref().expect("startup").store.clone(),
            session.segments[&fixture.segment_file.proxy_seq].cache_key.clone(),
        )
    };
    let retry = store.create(
        fixture.proxy_session_id.clone(),
        fixture.segment_file.proxy_seq,
        crate::SegmentRevisionKind::Raw,
    )?;
    let reservation = fixture.segment_cache.reserve_revision_peak(&cache_key, fixture.full_size * 5).await?;
    let (mut file, pin) =
        fixture.segment_cache.create_revision_spool_with_budget(&retry.revision().key, Some(&reservation)).await?;
    let bytes = tokio::fs::read(fixture.segment_cache.object_path(&cache_key)).await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    drop(file);
    let path = fixture.segment_cache.object_path(&retry.revision().key);
    fixture.segment_cache.account_reserved_spool(&path, fixture.full_size, &reservation)?;
    *retry.revision().file_pin.lock().expect("pin") = Some(pin);
    assert!(retry.revision().complete(crate::CachedSegmentMetadata { path, size: fixture.full_size }));
    let mut session = fixture.session.write().await;
    retry.revision().bind_origin(&session);
    session.segments.get_mut(&fixture.segment_file.proxy_seq).expect("entry").status =
        SegmentCacheStatus::Ready { content_length: fixture.full_size, ready_at_ms: current_time_millis() };
    session.startup.as_mut().expect("startup").revisions.insert(fixture.segment_file.proxy_seq, retry.clone());
    Ok(retry)
}

#[tokio::test]
async fn partial_revision_retry_recovers_same_get_and_snapshot_without_playlist_refresh() -> std::io::Result<()> {
    for prefix in [0, 6, 3_230_293] {
        let fixture = CachedSegmentFixture::new(b"complete-retry-body".to_vec()).await;
        let failed = failed_optimistic_revision(&fixture, prefix).await?;
        let before = fixture
            .manager
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
            .await
            .expect("lease before");
        let availability =
            fixture.manager.access_leases().read().await.availability_evidence_generation(&fixture.proxy_session_id);
        let retry = install_completed_retry(&fixture).await?;
        let response = ready_response(fixture.serve(None).await);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.into_body().collect().await.expect("body").to_bytes(), b"complete-retry-body".as_slice());
        let after = fixture
            .manager
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
            .await
            .expect("same lease");
        let snapshot = after.last_manifest_snapshot.expect("snapshot");
        let previous_snapshot = before.last_manifest_snapshot.expect("previous snapshot");
        assert!(snapshot.snapshot_generation > previous_snapshot.snapshot_generation);
        assert_eq!(snapshot.visible_segments, previous_snapshot.visible_segments);
        assert_eq!(snapshot.first_proxy_seq, previous_snapshot.first_proxy_seq);
        assert_eq!(
            snapshot.startup_revisions.as_ref().expect("revisions").revisions[&fixture.segment_file.proxy_seq],
            retry
        );
        let mut leases = fixture.manager.access_leases().write().await;
        assert_ne!(leases.availability_evidence_generation(&fixture.proxy_session_id), availability);
        assert_eq!(leases.published_segment_revision(&fixture.lease_id, fixture.segment_file.proxy_seq), Some(retry));
        let guard = leases
            .prepare_manifest_publication(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
            .expect("guard");
        assert!(leases
            .commit_manifest_publication(
                &fixture.lease_id,
                &fixture.proxy_session_id,
                guard,
                snapshot,
                current_time_millis()
            )
            .is_committed());
        assert!(!failed.revision().was_exposed());
    }
    Ok(())
}

#[tokio::test]
async fn retry_pending_is_503_without_ending_the_lease() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    failed_optimistic_revision(&fixture, 6).await?;
    fixture.session.write().await.startup.as_mut().expect("startup").first_data_timeout_ms = 25;
    let failure = match fixture.serve(None).await {
        HlsResourceServeOutcome::Failure(failure) => failure,
        HlsResourceServeOutcome::Ready(_) => panic!("retry must wait"),
    };
    let response = crate::response::cache::hls_resource_failure_default_response(failure);
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(response.headers().contains_key(header::RETRY_AFTER));
    assert!(fixture
        .manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
        .await
        .is_some());
    install_completed_retry(&fixture).await?;
    assert_eq!(recovery(&fixture).await, HlsRevisionRecoveryOutcome::ReplacementBound);
    Ok(())
}

#[tokio::test]
async fn waiting_get_resolves_a_retry_that_finishes_before_its_deadline() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    failed_optimistic_revision(&fixture, 6).await?;
    let request = fixture.serve(None);
    let complete = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        install_completed_retry(&fixture).await
    };
    let (response, retry) = tokio::join!(request, complete);
    retry?;
    let response = ready_response(response);
    assert_eq!(response.into_body().collect().await.expect("body").to_bytes(), b"retry-body".as_slice());
    Ok(())
}

#[tokio::test]
async fn exposed_failure_ends_only_the_old_lease_and_cannot_append_retry_bytes() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    let old = publish_revision_fixture(&fixture, true).await?;
    let base_snapshot = fixture
        .manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
        .await
        .expect("old lease")
        .last_manifest_snapshot
        .expect("old snapshot");
    let response = ready_response(fixture.serve(None).await);
    let mut stream = response.into_body().into_data_stream();
    assert_eq!(stream.next().await.transpose().map_err(std::io::Error::other)?.as_deref(), Some(b"prefix".as_slice()));
    old.revision().fail();
    let retry = install_completed_retry(&fixture).await?;
    assert!(stream.next().await.is_some_and(|chunk| chunk.is_err()));
    assert!(stream.next().await.is_none());
    assert_eq!(recovery(&fixture).await, HlsRevisionRecoveryOutcome::ReentryRequired);
    assert!(fixture
        .manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
        .await
        .is_none());
    assert_eq!(recovery(&fixture).await, HlsRevisionRecoveryOutcome::Unchanged);
    assert!(fixture.manager.sessions().get_by_proxy_session_id(&fixture.proxy_session_id).await.is_some());
    assert_eq!(
        fixture.session.read().await.startup.as_ref().expect("shared startup").revisions
            [&fixture.segment_file.proxy_seq],
        retry
    );
    let new_id = HlsAccessLeaseId("replacement-viewer".into());
    let marker = live_media_marker(&fixture.manager, &fixture.session, new_id.clone()).await;
    let mut leases = fixture.manager.access_leases().write().await;
    assert!(leases.media_identity_is_current(
        &new_id,
        &fixture.proxy_session_id,
        marker.lease_identity,
        current_time_millis()
    ));
    let mut snapshot = base_snapshot;
    snapshot.startup_revisions = Some(Arc::new(crate::HlsManifestRevisions {
        window_plan: None,
        mode: shared::model::HlsStartupMode::Optimistic,
        retained_start_seq: fixture.segment_file.proxy_seq,
        revisions: [(fixture.segment_file.proxy_seq, retry)].into_iter().collect(),
    }));
    let guard = leases
        .prepare_manifest_publication(&new_id, &fixture.proxy_session_id, current_time_millis())
        .expect("new publication");
    assert!(leases
        .commit_manifest_publication(&new_id, &fixture.proxy_session_id, guard, snapshot, current_time_millis())
        .is_committed());
    drop(leases);
    let context = HlsCacheResponseContext::new(
        new_id,
        test_log_identity(),
        300,
        fixture.manager.metrics().clone(),
        fixture.manager.segment_repair().clone(),
        None,
        Some(marker),
        current_time_millis(),
    );
    let response = ready_response(
        serve_hls_segment_cache_outcome(
            fixture.segment_cache.clone(),
            fixture.session.clone(),
            fixture.segment_file.clone(),
            None,
            &context,
        )
        .await,
    );
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.into_body().collect().await.expect("new lease body").to_bytes(), b"retry-body".as_slice());
    Ok(())
}

#[tokio::test]
async fn recovery_fences_generation_snapshot_and_lease_incarnation() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    failed_optimistic_revision(&fixture, 6).await?;
    let guard = fixture
        .manager
        .access_leases()
        .read()
        .await
        .revision_recovery_guard(&fixture.lease_id, fixture.segment_file.proxy_seq)
        .expect("guard");
    install_completed_retry(&fixture).await?;
    assert_eq!(recovery(&fixture).await, HlsRevisionRecoveryOutcome::ReplacementBound);
    let mut leases = fixture.manager.access_leases().write().await;
    let session = fixture.session.read().await;
    assert_eq!(
        leases.recover_revision_binding(&fixture.lease_id, &guard, &session, current_time_millis()),
        HlsRevisionRecoveryOutcome::Unchanged
    );
    assert!(leases
        .end_revision_playback_if_current(&fixture.lease_id, &guard, &session, current_time_millis())
        .is_none());
    drop(session);
    drop(leases);
    // Replacing the same ID does not let a delayed failure end the new viewer.
    live_media_marker(&fixture.manager, &fixture.session, fixture.lease_id.clone()).await;
    assert!(
        !fixture
            .manager
            .end_access_lease_for_failed_revision(
                &fixture.lease_id,
                &fixture.proxy_session_id,
                &guard,
                current_time_millis()
            )
            .await
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_recovery_commits_once_and_preserves_progressive_quota() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    failed_optimistic_revision(&fixture, 6).await?;
    {
        let mut leases = fixture.manager.access_leases().write().await;
        assert!(leases.try_claim_progressive_startup(
            &fixture.lease_id,
            fixture.segment_file.proxy_seq,
            current_time_millis()
        ));
    }
    install_completed_retry(&fixture).await?;
    let (first, second) = tokio::join!(recovery(&fixture), recovery(&fixture));
    assert!(matches!(
        (first, second),
        (HlsRevisionRecoveryOutcome::ReplacementBound, HlsRevisionRecoveryOutcome::Unchanged)
            | (HlsRevisionRecoveryOutcome::Unchanged, HlsRevisionRecoveryOutcome::ReplacementBound)
    ));
    assert!(!fixture.manager.access_leases().write().await.try_claim_progressive_startup(
        &fixture.lease_id,
        fixture.segment_file.proxy_seq,
        current_time_millis()
    ));
    Ok(())
}

#[tokio::test]
async fn origin_generation_and_mapping_changes_never_install_retired_retry_bytes() -> std::io::Result<()> {
    for change_generation in [false, true] {
        let fixture = CachedSegmentFixture::new(b"retired-retry".to_vec()).await;
        let failed = failed_optimistic_revision(&fixture, 6).await?;
        install_completed_retry(&fixture).await?;
        let mut leases = fixture.manager.access_leases().write().await;
        let guard = leases.revision_recovery_guard(&fixture.lease_id, fixture.segment_file.proxy_seq).expect("guard");
        let mut session = fixture.session.write().await;
        if change_generation {
            session.activity.origin_work_generation += 1;
        } else {
            session.segments.get_mut(&fixture.segment_file.proxy_seq).expect("entry").origin_key.origin_epoch += 1;
        }
        assert_eq!(
            leases.recover_revision_binding(&fixture.lease_id, &guard, &session, current_time_millis()),
            HlsRevisionRecoveryOutcome::ReentryRequired
        );
        assert_eq!(leases.published_segment_revision(&fixture.lease_id, fixture.segment_file.proxy_seq), Some(failed));
    }
    Ok(())
}

#[tokio::test]
async fn expired_lease_cannot_be_rebound_or_ended_by_a_stale_recovery_guard() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    let failed = failed_optimistic_revision(&fixture, 6).await?;
    install_completed_retry(&fixture).await?;
    let mut leases = fixture.manager.access_leases().write().await;
    let guard = leases.revision_recovery_guard(&fixture.lease_id, fixture.segment_file.proxy_seq).expect("guard");
    let expires = leases
        .response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
        .expect("lease")
        .valid_until_ms;
    let session = fixture.session.read().await;
    assert_eq!(
        leases.recover_revision_binding(&fixture.lease_id, &guard, &session, expires + 1),
        HlsRevisionRecoveryOutcome::Unchanged
    );
    assert!(leases.end_revision_playback_if_current(&fixture.lease_id, &guard, &session, expires + 1).is_none());
    assert_eq!(leases.published_segment_revision(&fixture.lease_id, fixture.segment_file.proxy_seq), Some(failed));
    Ok(())
}

#[tokio::test]
async fn fetch_completion_recovers_before_get_and_final_failure_reuses_manager_cleanup() -> std::io::Result<()> {
    for succeeds in [false, true] {
        let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
        let failed = failed_optimistic_revision(&fixture, 6).await?;
        let generation = fixture.session.read().await.activity.origin_work_generation;
        fixture.manager.segment_repair().update_config(tuliprox_core::model::HlsSegmentRepairConfig {
            max_level: shared::model::HlsSegmentRepairMode::Low,
            apply_to_first_segments: 1,
            ..Default::default()
        });
        fixture.manager.segment_repair().start_access_lease_window(fixture.lease_id.clone()).await;
        fixture
            .manager
            .qos()
            .ensure_access_lease(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis(), None)
            .await;
        let other_id = HlsAccessLeaseId("unaffected-viewer".into());
        let other = live_media_marker(&fixture.manager, &fixture.session, other_id.clone()).await;
        fixture.manager.segment_repair().start_access_lease_window(other_id.clone()).await;
        fixture
            .manager
            .qos()
            .ensure_access_lease(&other_id, &fixture.proxy_session_id, current_time_millis(), None)
            .await;
        let retry = if succeeds {
            Some(install_completed_retry(&fixture).await?)
        } else {
            fixture.session.write().await.segments.get_mut(&fixture.segment_file.proxy_seq).expect("entry").status =
                SegmentCacheStatus::FailedPermanent { failed_at_ms: current_time_millis(), status: None };
            None
        };
        let runtime = fixture.manager.segment_worker_pool().revision_recovery.get().expect("manager recovery runtime");
        runtime
            .recover_segment_leases_after_fetch(
                &fixture.proxy_session_id,
                fixture.segment_file.proxy_seq,
                generation.wrapping_sub(1),
                current_time_millis(),
            )
            .await;
        assert_eq!(
            fixture
                .manager
                .access_leases()
                .read()
                .await
                .published_segment_revision(&fixture.lease_id, fixture.segment_file.proxy_seq),
            Some(failed)
        );
        runtime
            .recover_segment_leases_after_fetch(
                &fixture.proxy_session_id,
                fixture.segment_file.proxy_seq,
                generation,
                current_time_millis(),
            )
            .await;
        if succeeds {
            assert_eq!(
                fixture
                    .manager
                    .access_leases()
                    .read()
                    .await
                    .published_segment_revision(&fixture.lease_id, fixture.segment_file.proxy_seq),
                retry
            );
            assert_eq!(fixture.manager.segment_repair().stats().await.windows, 2);
            assert_eq!(fixture.manager.qos().len().await, 2);
        } else {
            assert!(fixture
                .manager
                .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
                .await
                .is_none());
            assert_eq!(fixture.manager.segment_repair().stats().await.windows, 1);
            assert_eq!(fixture.manager.qos().len().await, 1);
        }
        assert!(fixture.manager.access_leases().write().await.media_identity_is_current(
            &other_id,
            &fixture.proxy_session_id,
            other.lease_identity,
            current_time_millis(),
        ));
    }
    Ok(())
}

#[tokio::test]
async fn playlist_publication_uses_the_same_recovery_and_never_reinstalls_an_older_attempt() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    let failed = failed_optimistic_revision(&fixture, 6).await?;
    let old_snapshot = fixture
        .manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
        .await
        .expect("lease")
        .last_manifest_snapshot
        .expect("snapshot");
    let retry = install_completed_retry(&fixture).await?;
    let guard = fixture
        .manager
        .prepare_access_lease_manifest_publication(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
        .await
        .expect("publication");
    assert!(fixture
        .manager
        .commit_access_lease_manifest_publication(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            guard,
            old_snapshot.clone(),
            current_time_millis()
        )
        .await
        .is_committed());
    assert_eq!(
        fixture
            .manager
            .access_leases()
            .read()
            .await
            .published_segment_revision(&fixture.lease_id, fixture.segment_file.proxy_seq),
        Some(retry)
    );
    // An old request can still contain the failed guard, but it cannot rewind
    // the successful transaction to that guard or make it complete again.
    assert!(!failed.revision().complete(crate::CachedSegmentMetadata { path: "retired.ts".into(), size: 1 }));
    let guard = fixture
        .manager
        .prepare_access_lease_manifest_publication(&fixture.lease_id, &fixture.proxy_session_id, current_time_millis())
        .await
        .expect("publication");
    assert!(fixture
        .manager
        .commit_access_lease_manifest_publication(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            guard,
            old_snapshot,
            current_time_millis()
        )
        .await
        .is_committed());
    Ok(())
}

#[tokio::test]
async fn repeated_recovery_cycles_release_old_publication_pins_and_replay_budget() -> std::io::Result<()> {
    let mut fixture = CachedSegmentFixture::new(b"retry-body".to_vec()).await;
    for _ in 0..128 {
        let marker = live_media_marker(&fixture.manager, &fixture.session, fixture.lease_id.clone()).await;
        fixture.context.media_activity_marker = Some(marker);
        let failed = failed_optimistic_revision(&fixture, 6).await?;
        let (store, budget) = {
            let session = fixture.session.read().await;
            let startup = session.startup.as_ref().expect("startup");
            (startup.store.clone(), startup.budget.clone())
        };
        let retry = install_spooled_retry(&fixture).await?;
        assert_eq!(recovery(&fixture).await, HlsRevisionRecoveryOutcome::ReplacementBound);
        assert_eq!(failed.revision().pin_count(), 1, "no failed guard remains in the lease or snapshot");
        assert_eq!(budget.usage(), (0, 0));
        fixture.manager.access_leases().write().await.remove_access_lease(&fixture.lease_id);
        fixture.session.write().await.startup.as_mut().expect("startup").revisions.clear();
        drop(failed);
        drop(retry);
        let retired = store.retire_unpinned()?;
        assert_eq!(retired.len(), 2);
        for revision in retired {
            fixture.segment_cache.delete_if_inactive(&revision.key).await?;
            assert!(!fixture.segment_cache.object_path(&revision.key).exists());
        }
        assert!(store.retire_unpinned()?.is_empty());
        assert_eq!(fixture.segment_cache.revision_disk_bytes(), 0);
        assert_eq!(fixture.segment_cache.remaining_revision_bytes(&fixture.proxy_session_id), 0);
        assert!(!fixture
            .segment_cache
            .has_active_temp_files_for_session_except::<crate::SegmentRevisionKey>(&fixture.proxy_session_id, &[]));
    }
    Ok(())
}
