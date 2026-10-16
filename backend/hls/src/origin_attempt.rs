use crate::{
    begin_hls_origin_account_io_bounded, finish_hls_origin_account_io, origin_account_runtime::prepare_media_origin,
    HlsBoundAccountAcquireErrorKind, HlsOriginAccountBinding, HlsOriginAccountIoLeaseGuard, HlsOriginIoContext,
    HlsSession, HlsSessionHandle,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tuliprox_core::utils::current_time_millis;

/// Whether the completed media attempt still belongs to the current account.
pub(crate) struct HlsMediaOriginWorkFinish {
    pub(crate) generation_valid: bool,
}

/// Owns the work counter and provider lease for one generation-bound media attempt.
pub(crate) struct HlsMediaOriginAttemptGuard {
    generation: u64,
    binding: Option<HlsOriginAccountBinding>,
    active_origin_work_count: Option<Arc<AtomicUsize>>,
    provider_lease: Option<(HlsOriginIoContext, HlsOriginAccountIoLeaseGuard)>,
}

impl HlsMediaOriginAttemptGuard {
    fn matches_session(&self, session: &HlsSession) -> bool {
        self.generation == session.activity.origin_work_generation
            && match (&self.binding, &session.origin_account_binding) {
                (Some(expected), Some(current)) => {
                    let expected_tag = self
                        .provider_lease
                        .as_ref()
                        .map_or(expected.provider_binding_tag, |(_, lease)| lease.binding().provider_binding_tag);
                    expected.input_name == current.input_name
                        && expected.account_name == current.account_name
                        && expected.session_owner == current.session_owner
                        && expected.generation == current.generation
                        && expected.is_active() == current.is_active()
                        // Reacquiring an expired provider lease may renew its tag without an account handoff.
                        && expected_tag.is_none_or(|tag| current.provider_binding_tag == Some(tag))
                }
                (None, None) => true,
                (Some(_), None) | (None, Some(_)) => false,
            }
    }

    pub(crate) async fn finish(mut self, session: &HlsSessionHandle) -> HlsMediaOriginWorkFinish {
        let (generation_valid, refresh_reservation) = {
            let mut session = session.write().await;
            let generation_valid = self.matches_session(&session);
            if self.active_origin_work_count.take().is_some() {
                session.finish_origin_work(self.generation);
            }
            (generation_valid, generation_valid && session.should_refresh_origin_reservation(current_time_millis()))
        };
        if let Some((origin_io, lease)) = self.provider_lease.take() {
            finish_hls_origin_account_io(&origin_io, session, lease, refresh_reservation).await;
        }
        HlsMediaOriginWorkFinish { generation_valid }
    }
}

impl Drop for HlsMediaOriginAttemptGuard {
    fn drop(&mut self) {
        if let Some(count) = self.active_origin_work_count.take() {
            count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Captures work and binding atomically, then rejects a handoff during provider acquisition.
pub(crate) async fn prepare_media_origin_attempt(
    origin_io: Option<&HlsOriginIoContext>,
    session: &HlsSessionHandle,
    expected_generation: u64,
    acquire_timeout: Duration,
) -> Result<HlsMediaOriginAttemptGuard, HlsBoundAccountAcquireErrorKind> {
    prepare_media_origin(origin_io, session).await?;
    let mut attempt = {
        let mut session = session.write().await;
        if session.activity.origin_work_generation != expected_generation {
            return Err(HlsBoundAccountAcquireErrorKind::StoreRace);
        }
        let binding = session.origin_account_binding.clone();
        if origin_io.is_some() && binding.as_ref().is_some_and(HlsOriginAccountBinding::is_detached) {
            return Err(HlsBoundAccountAcquireErrorKind::Detached);
        }
        let active_origin_work_count = origin_io.map(|_| {
            session.start_origin_work();
            Arc::clone(&session.activity.active_origin_work_count)
        });
        HlsMediaOriginAttemptGuard {
            generation: expected_generation,
            binding,
            active_origin_work_count,
            provider_lease: None,
        }
    };
    let (Some(origin_io), Some(binding)) = (origin_io, attempt.binding.as_ref()) else {
        return Ok(attempt);
    };
    let acquired = begin_hls_origin_account_io_bounded(origin_io, session, binding, acquire_timeout).await;
    let error = match acquired {
        Ok(lease) => {
            attempt.provider_lease = Some((origin_io.clone(), lease));
            None
        }
        Err(error) => Some(error),
    };
    let still_current = attempt.matches_session(&*session.read().await);
    if !still_current {
        attempt.finish(session).await;
        return Err(HlsBoundAccountAcquireErrorKind::StoreRace);
    }
    if let Some(error) = error {
        attempt.finish(session).await;
        if error.allows_rebind() {
            let _ = prepare_media_origin(Some(origin_io), session).await;
            return Err(HlsBoundAccountAcquireErrorKind::StoreRace);
        }
        return Err(error);
    }
    Ok(attempt)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::prepare_media_origin_attempt;
    use crate::{
        HlsBoundAccountAcquireErrorKind, HlsCtx, HlsOriginAccountBinding, HlsOriginAccountIoLease, HlsOriginIoContext,
        HlsSession, HlsSessionHandle, HlsSessionKey,
    };
    use shared::model::{ConfigInputAliasDto, ConfigInputDto};
    use std::{
        sync::{atomic::Ordering, Arc},
        time::Duration,
    };
    use tuliprox_core::model::{Config, ConfigInput, PlaybackKind, SourcesConfig};
    use tuliprox_session::ConnectionKind;

    pub(crate) async fn bound_origin_io(session: &HlsSessionHandle) -> (HlsOriginIoContext, HlsOriginAccountBinding) {
        let ctx = HlsCtx::for_test(Config::default());
        let input = ConfigInput::from(&ConfigInputDto {
            name: Arc::from("account-a"),
            url: "http://origin.example/account-a/index.m3u8".to_string(),
            enabled: true,
            max_connections: 2,
            aliases: Some(vec![ConfigInputAliasDto {
                name: Arc::from("account-b"),
                url: "http://origin.example/account-b/index.m3u8".to_string(),
                enabled: true,
                max_connections: 2,
                ..ConfigInputAliasDto::default()
            }]),
            ..ConfigInputDto::default()
        });
        ctx.app_config
            .sources
            .store(Arc::new(SourcesConfig { inputs: vec![Arc::new(input)], ..SourcesConfig::default() }));
        ctx.active_provider.update_config(&ctx.app_config);
        let replacement = {
            let mut session = session.write().await;
            let binding = HlsOriginAccountBinding::new(
                Arc::from("account-a"),
                Arc::from("account-a"),
                &session.proxy_session_id,
                1_000,
            );
            let replacement = HlsOriginAccountBinding::rebound(
                Arc::clone(&binding.input_name),
                Arc::from("account-b"),
                binding.session_owner.clone(),
                binding.generation + 1,
                2_000,
            );
            session.replace_origin_account_binding(Some(binding));
            replacement
        };
        let io = HlsOriginIoContext {
            ctx,
            client_addr: "127.0.0.1:12345".parse().expect("test address"),
            allow_grace: false,
            priority: 0,
            connection_kind: ConnectionKind::Normal,
            reservation_ttl_secs: 60,
            preacquired_provider_handle: None,
            started_generation: None,
            prepared_account: None,
            playback_kind: PlaybackKind::LiveHls,
            account_runtime: None,
        };
        (io, replacement)
    }

    #[tokio::test]
    async fn handoff_after_lease_acquisition_releases_the_old_attempt_without_touching_its_successor() {
        let session =
            Arc::new(tokio::sync::RwLock::new(HlsSession::new(HlsSessionKey::new(1, "lease-handoff"), b"secret", 0)));
        let (io, replacement) = bound_origin_io(&session).await;
        let original_lease = {
            let mut session = session.write().await;
            let lease = Arc::new(HlsOriginAccountIoLease::active_for_test(
                session.origin_account_binding.as_ref().expect("original binding"),
                1,
            ));
            session.origin_account_io_lease = Some(Arc::clone(&lease));
            lease
        };
        let replacement_lease = Arc::new(HlsOriginAccountIoLease::active_for_test(&replacement, 1));
        let generation = session.read().await.activity.origin_work_generation;
        let read_blocker = session.read().await;
        let mut prepare =
            Box::pin(prepare_media_origin_attempt(Some(&io), &session, generation, Duration::from_secs(1)));
        assert!(futures::poll!(prepare.as_mut()).is_pending());
        // Queue a reader between the atomic start and the provider-slot write.
        let mut between_start_and_lease = Box::pin(session.read());
        assert!(futures::poll!(between_start_and_lease.as_mut()).is_pending());
        drop(read_blocker);
        assert!(futures::poll!(prepare.as_mut()).is_pending());
        let between_start_and_lease = between_start_and_lease.await;
        let mut handoff = Box::pin(session.write());
        assert!(futures::poll!(handoff.as_mut()).is_pending());
        drop(between_start_and_lease);
        assert!(futures::poll!(prepare.as_mut()).is_pending());
        {
            let mut session = handoff.await;
            assert_eq!(
                original_lease.active_io_count.load(Ordering::Acquire),
                2,
                "old lease was acquired before handoff"
            );
            session.handoff_origin_account(replacement);
            session.origin_account_io_lease = Some(Arc::clone(&replacement_lease));
        }

        assert!(matches!(prepare.await, Err(HlsBoundAccountAcquireErrorKind::StoreRace)));
        let session = session.read().await;
        assert_eq!(session.activity.active_origin_work_count.load(Ordering::Acquire), 0);
        assert_eq!(original_lease.active_io_count.load(Ordering::Acquire), 1);
        assert_eq!(replacement_lease.active_io_count.load(Ordering::Acquire), 1);
        assert!(session.origin_account_io_lease.as_ref().is_some_and(|lease| Arc::ptr_eq(lease, &replacement_lease)));
        assert!(session.origin_account_binding.as_ref().expect("new binding").last_origin_io_at_ms.is_none());
    }

    #[tokio::test]
    async fn initial_provider_tag_assignment_keeps_the_media_attempt_current() {
        let session =
            Arc::new(tokio::sync::RwLock::new(HlsSession::new(HlsSessionKey::new(1, "initial-tag"), b"secret", 0)));
        let (io, _) = bound_origin_io(&session).await;
        let generation = session.read().await.activity.origin_work_generation;
        let attempt = prepare_media_origin_attempt(Some(&io), &session, generation, Duration::from_secs(1))
            .await
            .expect("initial binding can acquire its first provider lease");
        assert_eq!(io.ctx.active_provider.get_provider_connections_count(), 1);
        assert_eq!(session.read().await.activity.active_origin_work_count.load(Ordering::Acquire), 1);
        assert!(attempt.finish(&session).await.generation_valid);
        assert_eq!(io.ctx.active_provider.get_provider_connections_count(), 0);
        assert_eq!(session.read().await.activity.active_origin_work_count.load(Ordering::Acquire), 0);
        assert!(session.read().await.origin_account_io_lease.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn same_account_reacquisition_after_provider_lease_expiry_keeps_the_media_attempt_current() {
        let session =
            Arc::new(tokio::sync::RwLock::new(HlsSession::new(HlsSessionKey::new(1, "lease-expiry"), b"secret", 0)));
        let (io, _) = bound_origin_io(&session).await;
        let generation = session.read().await.activity.origin_work_generation;
        let first = prepare_media_origin_attempt(Some(&io), &session, generation, Duration::from_secs(1))
            .await
            .expect("first lease acquisition");
        assert!(first.finish(&session).await.generation_valid);
        let original = session.read().await.origin_account_binding.clone().expect("original binding");
        let original_tag = original.provider_binding_tag.expect("first lease has a provider tag");
        assert_eq!(io.ctx.active_provider.get_provider_connections_count(), 0);

        tokio::time::advance(Duration::from_secs(tuliprox_session::provider_leases::STARTING_LEASE_TTL_SECS + 1)).await;

        let reacquired = prepare_media_origin_attempt(Some(&io), &session, generation, Duration::from_secs(1))
            .await
            .expect("same account reacquires its expired provider lease");
        {
            let session = session.read().await;
            let current = session.origin_account_binding.as_ref().expect("reacquired binding");
            assert_eq!(
                (&current.input_name, &current.account_name, &current.session_owner, current.generation),
                (&original.input_name, &original.account_name, &original.session_owner, original.generation)
            );
            assert_eq!(session.activity.origin_work_generation, generation);
            assert_ne!(current.provider_binding_tag.expect("new lease has a provider tag"), original_tag);
            assert_eq!(session.activity.active_origin_work_count.load(Ordering::Acquire), 1);
        }
        assert_eq!(io.ctx.active_provider.get_provider_connections_count(), 1);
        assert!(reacquired.finish(&session).await.generation_valid);
        assert_eq!(io.ctx.active_provider.get_provider_connections_count(), 0);
        assert_eq!(session.read().await.activity.active_origin_work_count.load(Ordering::Acquire), 0);
        assert!(session.read().await.origin_account_io_lease.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_media_attempts_join_the_reacquired_provider_lease_with_its_current_tag() {
        let session = Arc::new(tokio::sync::RwLock::new(HlsSession::new(
            HlsSessionKey::new(1, "concurrent-lease-expiry"),
            b"secret",
            0,
        )));
        let (io, _) = bound_origin_io(&session).await;
        let generation = session.read().await.activity.origin_work_generation;
        let baseline = prepare_media_origin_attempt(Some(&io), &session, generation, Duration::from_secs(1))
            .await
            .expect("baseline provider lease");
        assert!(baseline.finish(&session).await.generation_valid);
        let original_tag = session.read().await.origin_account_binding.as_ref().expect("binding").provider_binding_tag;
        tokio::time::advance(Duration::from_secs(tuliprox_session::provider_leases::STARTING_LEASE_TTL_SECS + 1)).await;

        // Queue both snapshots before either attempt can replace the expired provider tag.
        let read_blocker = session.read().await;
        let mut first = Box::pin(prepare_media_origin_attempt(Some(&io), &session, generation, Duration::from_secs(1)));
        assert!(futures::poll!(first.as_mut()).is_pending());
        let mut second =
            Box::pin(prepare_media_origin_attempt(Some(&io), &session, generation, Duration::from_secs(1)));
        assert!(futures::poll!(second.as_mut()).is_pending());
        drop(read_blocker);
        assert!(futures::poll!(first.as_mut()).is_pending());
        assert!(futures::poll!(second.as_mut()).is_pending());
        // Let the first reserve its slot and the second wait for that slot's acquisition.
        assert!(futures::poll!(first.as_mut()).is_pending());
        assert!(futures::poll!(second.as_mut()).is_pending());
        let first = first.await.expect("first attempt reacquires the provider lease");
        let second = second.await.expect("second attempt joins the reacquired provider lease");
        {
            let session = session.read().await;
            let current = session.origin_account_binding.as_ref().expect("binding");
            let joined = second.provider_lease.as_ref().expect("joined provider lease").1.binding();
            assert_ne!(current.provider_binding_tag, original_tag);
            assert_eq!(joined.provider_binding_tag, current.provider_binding_tag);
            assert_eq!(joined.playback_request_id, current.playback_request_id);
            assert_eq!(session.activity.origin_work_generation, generation);
            assert_eq!(session.activity.active_origin_work_count.load(Ordering::Acquire), 2);
            assert_eq!(
                session
                    .origin_account_io_lease
                    .as_ref()
                    .expect("shared I/O lease")
                    .active_io_count
                    .load(Ordering::Acquire),
                2
            );
        }
        assert_eq!(io.ctx.active_provider.get_provider_connections_count(), 1);
        assert!(first.finish(&session).await.generation_valid);
        assert_eq!(io.ctx.active_provider.get_provider_connections_count(), 1);
        assert!(second.finish(&session).await.generation_valid);
        assert_eq!(io.ctx.active_provider.get_provider_connections_count(), 0);
        assert_eq!(session.read().await.activity.active_origin_work_count.load(Ordering::Acquire), 0);
        assert!(session.read().await.origin_account_io_lease.is_none());
    }
}
