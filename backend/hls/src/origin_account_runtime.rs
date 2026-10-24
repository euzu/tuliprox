use crate::{
    manifest_fetch::LiveHlsOriginEntry, HlsBoundAccountAcquireErrorKind, HlsOriginIoContext, HlsOriginWorkClass,
    HlsSessionHandle,
};
use std::{fmt, future::Future, pin::Pin};

/// Origin operation being prepared; media work must first qualify an account handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsOriginAccountWork {
    Manifest,
    Media,
}

/// Account preparation result resolved against the current source configuration.
pub struct HlsPreparedOriginAccount {
    pub entry: LiveHlsOriginEntry,
    pub snapshot: HlsOriginAccountSnapshot,
}

/// Account identity, work generation and provider headers captured together with a resolved entry URL.
#[derive(Clone)]
pub struct HlsOriginAccountSnapshot {
    pub binding: Option<crate::HlsOriginAccountBinding>,
    pub generation: u64,
    pub provider_session_headers: axum::http::HeaderMap,
}

impl HlsOriginAccountSnapshot {
    pub fn from_session(session: &crate::HlsSession) -> Self {
        Self {
            binding: session.origin_account_binding.clone(),
            generation: session.activity.origin_work_generation,
            provider_session_headers: session.origin_provider_session_headers.clone(),
        }
    }

    pub fn matches_session(&self, session: &crate::HlsSession) -> bool {
        self.matches_session_identity(session)
            && self.binding.as_ref().and_then(|binding| binding.provider_binding_tag)
                == session.origin_account_binding.as_ref().and_then(|binding| binding.provider_binding_tag)
    }

    /// Allocation may install a fresh binding tag without changing the account generation.
    pub(crate) fn matches_session_identity(&self, session: &crate::HlsSession) -> bool {
        self.generation == session.activity.origin_work_generation
            && match (&self.binding, &session.origin_account_binding) {
                (Some(expected), Some(current)) => {
                    expected.input_name == current.input_name
                        && expected.account_name == current.account_name
                        && expected.session_owner == current.session_owner
                        && expected.generation == current.generation
                        && expected.is_active() == current.is_active()
                }
                (None, None) => true,
                (Some(_), None) | (None, Some(_)) => false,
            }
    }
}

/// App-owned account allocation/URL adapter used by existing HLS workers.
pub trait HlsOriginAccountRuntime: Send + Sync + fmt::Debug {
    fn prepare<'a>(
        &'a self,
        session: &'a HlsSessionHandle,
        work: HlsOriginAccountWork,
        class: HlsOriginWorkClass,
    ) -> Pin<Box<dyn Future<Output = Result<HlsPreparedOriginAccount, HlsBoundAccountAcquireErrorKind>> + Send + 'a>>;
}

pub(crate) async fn prepare_media_origin(
    origin_io: Option<&HlsOriginIoContext>,
    session: &HlsSessionHandle,
) -> Result<(), HlsBoundAccountAcquireErrorKind> {
    let Some(io) = origin_io else {
        return Ok(());
    };
    let Some(runtime) = io.account_runtime.as_ref() else {
        return Ok(());
    };
    let class = if io.allow_grace { HlsOriginWorkClass::Demand } else { HlsOriginWorkClass::Background };
    let budget = io.ctx.hls_proxy.segment_fetch_policy().demand_wait_timeout_for_session(
        crate::segment_fetcher::HlsSegmentFetchWorkload::Clear,
        session.read().await.target_duration,
    );
    let now = tokio::time::Instant::now();
    let deadline = now.checked_add(budget).unwrap_or(now);
    tokio::time::timeout_at(deadline, async {
        loop {
            match runtime.prepare(session, HlsOriginAccountWork::Media, class).await {
                Ok(_) => return Ok(()),
                Err(error) => {
                    if class != HlsOriginWorkClass::Background {
                        return Err(error);
                    }
                    let now_ms = tuliprox_core::utils::current_time_millis();
                    let retry_at = session.read().await.origin_account_rebind.next_rebind_allowed_at_ms;
                    let Some(wait_ms) =
                        retry_at.and_then(|retry_at| retry_at.checked_sub(now_ms)).filter(|wait| *wait > 0)
                    else {
                        return Err(error);
                    };
                    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                }
            }
        }
    })
    .await
    .map_err(|_| HlsBoundAccountAcquireErrorKind::WaitTimedOut)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HlsCtx, HlsSession, HlsSessionKey};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tuliprox_core::model::{Config, PlaybackKind};
    use tuliprox_session::ConnectionKind;

    #[derive(Clone, Copy, Debug)]
    enum PrepareBehavior {
        RetryOnce,
        Stall,
    }

    #[derive(Debug)]
    struct TestRuntime {
        attempts: AtomicUsize,
        behavior: PrepareBehavior,
    }

    impl HlsOriginAccountRuntime for TestRuntime {
        fn prepare<'a>(
            &'a self,
            session: &'a HlsSessionHandle,
            _work: HlsOriginAccountWork,
            _class: HlsOriginWorkClass,
        ) -> Pin<Box<dyn Future<Output = Result<HlsPreparedOriginAccount, HlsBoundAccountAcquireErrorKind>> + Send + 'a>>
        {
            Box::pin(async move {
                let attempt = self.attempts.fetch_add(1, Ordering::AcqRel);
                match self.behavior {
                    PrepareBehavior::Stall => std::future::pending().await,
                    PrepareBehavior::RetryOnce if attempt == 0 => {
                        session.write().await.origin_account_rebind.mark_attempt_started(
                            Arc::from("unavailable"),
                            tuliprox_core::utils::current_time_millis(),
                        );
                        Err(HlsBoundAccountAcquireErrorKind::Unavailable)
                    }
                    PrepareBehavior::RetryOnce => Ok(HlsPreparedOriginAccount {
                        entry: LiveHlsOriginEntry::parse("https://origin.example/index.m3u8").unwrap(),
                        snapshot: HlsOriginAccountSnapshot::from_session(&*session.read().await),
                    }),
                }
            })
        }
    }

    fn fixture(behavior: PrepareBehavior) -> (HlsOriginIoContext, HlsSessionHandle, Arc<TestRuntime>) {
        let runtime = Arc::new(TestRuntime { attempts: AtomicUsize::new(0), behavior });
        let io = HlsOriginIoContext {
            ctx: HlsCtx::for_test(Config::default()),
            client_addr: "127.0.0.1:12345".parse().unwrap(),
            allow_grace: false,
            priority: 0,
            connection_kind: ConnectionKind::Normal,
            reservation_ttl_secs: 1,
            preacquired_provider_handle: None,
            started_generation: None,
            prepared_account: None,
            playback_kind: PlaybackKind::LiveHls,
            account_runtime: Some(runtime.clone()),
        };
        let session =
            Arc::new(tokio::sync::RwLock::new(HlsSession::new(HlsSessionKey::new(1, "runtime-backoff"), b"secret", 0)));
        (io, session, runtime)
    }

    #[tokio::test(start_paused = true)]
    async fn background_prepare_waits_for_rebind_backoff_before_retrying() {
        let (io, session, runtime) = fixture(PrepareBehavior::RetryOnce);
        let mut prepare = Box::pin(prepare_media_origin(Some(&io), &session));
        assert!(futures::poll!(prepare.as_mut()).is_pending());
        assert_eq!(runtime.attempts.load(Ordering::Acquire), 1);
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert!(futures::poll!(prepare.as_mut()).is_pending());
        assert_eq!(runtime.attempts.load(Ordering::Acquire), 1);
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        prepare.await.unwrap();
        assert_eq!(runtime.attempts.load(Ordering::Acquire), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_background_prepare_is_bounded_by_the_existing_object_budget() {
        let (io, session, runtime) = fixture(PrepareBehavior::Stall);
        let budget = io.ctx.hls_proxy.segment_fetch_policy().demand_wait_timeout_for_session(
            crate::segment_fetcher::HlsSegmentFetchWorkload::Clear,
            session.read().await.target_duration,
        );
        let mut prepare = Box::pin(prepare_media_origin(Some(&io), &session));
        assert!(futures::poll!(prepare.as_mut()).is_pending());
        tokio::time::advance(budget).await;
        assert_eq!(prepare.await, Err(HlsBoundAccountAcquireErrorKind::WaitTimedOut));
        assert_eq!(runtime.attempts.load(Ordering::Acquire), 1);
        assert_eq!(io.ctx.active_provider.get_provider_connections_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_prepare_returns_temporary_failure_without_background_waiting() {
        let (io, session, runtime) = fixture(PrepareBehavior::RetryOnce);
        let io = io.with_grace(true);
        assert_eq!(prepare_media_origin(Some(&io), &session).await, Err(HlsBoundAccountAcquireErrorKind::Unavailable));
        assert_eq!(runtime.attempts.load(Ordering::Acquire), 1);
        assert!(session.read().await.origin_account_rebind.next_rebind_allowed_at_ms.is_some());
    }
}
