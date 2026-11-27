use super::failure_class_for;
use shared::model::SwitchReason;
use tuliprox_session::failover_group::FailureClass;

#[test]
fn upstream_error_class_follows_http_status() {
    assert_eq!(failure_class_for(SwitchReason::UpstreamError, Some(401)), FailureClass::Auth);
    assert_eq!(failure_class_for(SwitchReason::UpstreamError, Some(429)), FailureClass::Capacity);
    assert_eq!(failure_class_for(SwitchReason::UpstreamError, Some(502)), FailureClass::Stream);
    assert_eq!(failure_class_for(SwitchReason::UpstreamError, None), FailureClass::Transport);
    for reason in [SwitchReason::UpstreamEof, SwitchReason::Stall, SwitchReason::Underrun] {
        assert_eq!(failure_class_for(reason, Some(401)), FailureClass::Stream);
    }
}

mod switcher {
    use super::super::{
        clock_now, GroupEnd, GroupFailoverContext, GroupFailoverDriver, GroupPoll, GroupSwitcher, ManualSwitchCommand,
        MemberHost, RuntimeMember, SwitchError,
    };
    use crate::api::{
        api_utils::tests::group_playback_support::{
            disable_burst, first_keyframe_pcr, member_pcr_gop, member_ts, FakeOriginMode, GroupPlaybackEnv,
        },
        model::{BoxedProviderStream, StreamError},
    };
    use bytes::Bytes;
    use futures::StreamExt;
    use shared::model::{FailoverGroupRuntimeConfig, SwitchReason};
    use std::{
        sync::Arc,
        task::{Context, Poll},
        time::Duration,
    };

    struct FakeHost {
        body: Option<BoxedProviderStream>,
        preempted: bool,
        released: u32,
        installed: u32,
    }

    impl FakeHost {
        fn new(body: BoxedProviderStream, preempted: bool) -> Self {
            Self { body: Some(body), preempted, released: 0, installed: 0 }
        }
    }

    impl MemberHost for FakeHost {
        fn member_body(&mut self) -> Option<&mut BoxedProviderStream> { self.body.as_mut() }

        fn member_preempted(&self) -> bool { self.preempted }

        fn release_member(&mut self) {
            self.released += 1;
            self.body = None;
        }

        fn install_member(&mut self, member: RuntimeMember) {
            self.installed += 1;
            self.body = Some(member.stream);
        }
    }

    async fn switcher(failover: FailoverGroupRuntimeConfig) -> (GroupPlaybackEnv, GroupSwitcher) {
        let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
        disable_burst(&env);
        let user = env.app_state.app_config.get_user_credentials("viewer").expect("viewer");
        let ctx = GroupFailoverContext {
            app_state: Arc::clone(&env.app_state),
            target: Arc::clone(&env.target),
            target_name: env.target.name.as_str().into(),
            group: Arc::clone(&env.group_record),
            playback: shared::model::FailoverGroupPlayback {
                failover,
                ..shared::model::FailoverGroupsConfig::default().playback(None)
            },
            user: (*user).clone(),
            fingerprint: crate::auth::Fingerprint::new(
                "fp".to_string(),
                "127.0.0.1".to_string(),
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            ),
            req_headers: axum::http::HeaderMap::new(),
            session_token: "switcher-session".into(),
            connection_kind: tuliprox_session::active_provider_manager::ConnectionKind::Normal,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            share_stream: false,
            base_channel: shared::model::StreamChannel::default(),
            shared_key: None,
        };
        let switcher = GroupSwitcher::new(GroupFailoverDriver::new(Arc::new(ctx), 0));
        (env, switcher)
    }

    fn no_budget() -> FailoverGroupRuntimeConfig {
        FailoverGroupRuntimeConfig { max_outage_secs: 0, ..FailoverGroupRuntimeConfig::default() }
    }

    async fn poll(switcher: &mut GroupSwitcher, host: &mut impl MemberHost) -> GroupPoll {
        std::future::poll_fn(|cx: &mut Context<'_>| match switcher.poll_member(host, cx) {
            GroupPoll::Pending => Poll::Pending,
            ready => Poll::Ready(ready),
        })
        .await
    }

    fn poll_once(switcher: &mut GroupSwitcher, host: &mut impl MemberHost) -> GroupPoll {
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        switcher.poll_member(host, &mut cx)
    }

    #[tokio::test]
    async fn preempted_member_ends_without_switch() {
        let (_env, mut switcher) = switcher(FailoverGroupRuntimeConfig::default()).await;
        let mut host = FakeHost::new(futures::stream::empty().boxed(), true);
        assert!(matches!(poll(&mut switcher, &mut host).await, GroupPoll::Ended(GroupEnd::Eof)));
        assert_eq!((host.released, host.installed), (0, 0));
        assert!(!switcher.is_switching());
    }

    #[tokio::test]
    async fn spent_budget_ends_with_the_body_end() {
        let (_env, mut switcher) = switcher(no_budget()).await;
        let error = futures::stream::iter([Err(StreamError::Stream("provider reset".to_string()))]).boxed();
        let mut host = FakeHost::new(error, false);
        assert!(matches!(poll(&mut switcher, &mut host).await, GroupPoll::Ended(GroupEnd::Error(_))));
        assert_eq!(host.released, 0, "nothing is released when no switch follows");
    }

    #[tokio::test]
    async fn split_chunks_reach_the_client_in_order_before_the_end() {
        let (_env, mut switcher) = switcher(no_budget()).await;
        let ts = member_ts(0x100, 10);
        let chunks = vec![Ok(Bytes::copy_from_slice(&ts[..300])), Ok(Bytes::copy_from_slice(&ts[300..]))];
        let mut host = FakeHost::new(futures::stream::iter(chunks).boxed(), false);
        let mut sent = Vec::new();
        loop {
            match poll(&mut switcher, &mut host).await {
                GroupPoll::Data(bytes) => sent.extend_from_slice(&bytes),
                GroupPoll::Ended(GroupEnd::Eof) => break,
                _ => panic!("unexpected poll result"),
            }
        }
        assert_eq!(sent, ts[..ts.len() / 188 * 188]);
    }

    #[tokio::test]
    async fn stall_without_budget_ends_stalled() {
        let selection = FailoverGroupRuntimeConfig { no_data_timeout_secs: 1, ..no_budget() };
        let (_env, mut switcher) = switcher(selection).await;
        let mut host = FakeHost::new(futures::stream::pending().boxed(), false);
        let ended =
            tokio::time::timeout(Duration::from_secs(3), poll(&mut switcher, &mut host)).await.expect("bounded");
        assert!(matches!(ended, GroupPoll::Ended(GroupEnd::Stalled)));
    }

    /// A packet on PID 0x100 whose adaptation field carries `pcr_27mhz`.
    fn pcr_packet(pcr_27mhz: u64) -> Vec<u8> {
        let base = pcr_27mhz / 300;
        let ext = pcr_27mhz % 300;
        let byte = |value: u64| u8::try_from(value & 0xFF).expect("byte");
        let mut packet = vec![0x47, 0x01, 0x00, 0x20, 183, 0x10];
        packet.extend([
            byte(base >> 25),
            byte(base >> 17),
            byte(base >> 9),
            byte(base >> 1),
            byte((base & 1) << 7) | 0x7E | byte(ext >> 8),
            byte(ext),
        ]);
        packet.resize(188, 0xFF);
        packet
    }

    #[tokio::test]
    async fn slow_member_triggers_underrun_switch() {
        let selection = FailoverGroupRuntimeConfig {
            slow_source_pct: 90,
            slow_source_secs: 2,
            no_data_timeout_secs: 0,
            ..FailoverGroupRuntimeConfig::default()
        };
        let (_env, mut switcher) = switcher(selection).await;
        // Every 100 ms of wall time the provider delivers 40 ms of media.
        let slow = futures::stream::unfold(27_000_000_u64, |pcr| async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let next = pcr + 27_000 * 40;
            Some((Ok::<Bytes, StreamError>(Bytes::from(pcr_packet(next))), next))
        })
        .boxed();
        let mut host = FakeHost::new(slow, false);
        let underrun_switch = tokio::time::timeout(Duration::from_secs(8), async {
            while host.released == 0 {
                let _ = poll(&mut switcher, &mut host).await;
            }
        })
        .await;
        assert!(underrun_switch.is_ok(), "the underrun starts a switch");
    }

    #[tokio::test]
    async fn bytes_that_never_reach_the_client_do_not_hold_off_the_stall() {
        let selection = FailoverGroupRuntimeConfig { no_data_timeout_secs: 1, ..no_budget() };
        let (_env, mut switcher) = switcher(selection).await;
        // Text instead of MPEG-TS every 100 ms: the aligner drops it all.
        let garbage = futures::stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Some((Ok::<Bytes, StreamError>(Bytes::from_static(b"<html>offline</html>")), ()))
        })
        .boxed();
        let mut host = FakeHost::new(garbage, false);
        let ended = tokio::time::timeout(Duration::from_secs(4), poll(&mut switcher, &mut host)).await;
        assert!(matches!(ended, Ok(GroupPoll::Ended(GroupEnd::Stalled))), "no media within the stall timeout");
    }

    #[tokio::test]
    async fn manual_switch_switches_to_target_member() {
        let env = GroupPlaybackEnv::builder(&["solis", "korath"])
            .origin_mode("korath", FakeOriginMode::TsThenStall(member_ts(0x100, 20)))
            .build()
            .await;
        disable_burst(&env);
        let user = env.app_state.app_config.get_user_credentials("viewer").expect("viewer");
        let ctx = GroupFailoverContext {
            app_state: Arc::clone(&env.app_state),
            target: Arc::clone(&env.target),
            target_name: env.target.name.as_str().into(),
            group: Arc::clone(&env.group_record),
            playback: shared::model::FailoverGroupPlayback {
                failover: FailoverGroupRuntimeConfig::default(),
                ..shared::model::FailoverGroupsConfig::default().playback(None)
            },
            user: (*user).clone(),
            fingerprint: crate::auth::Fingerprint::new(
                "fp".to_string(),
                "127.0.0.1".to_string(),
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            ),
            req_headers: axum::http::HeaderMap::new(),
            session_token: "switcher-session".into(),
            connection_kind: tuliprox_session::active_provider_manager::ConnectionKind::Normal,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            share_stream: false,
            base_channel: shared::model::StreamChannel::default(),
            shared_key: None,
        };
        let mut switcher = GroupSwitcher::new(GroupFailoverDriver::new(Arc::new(ctx), 0));
        let mut host = FakeHost::new(futures::stream::pending().boxed(), false);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let target_vid = env.member_vid("korath");
        let sender = switcher.switch_sender();
        sender.send(ManualSwitchCommand { target_member_vid: target_vid, respondent: tx }).expect("send");

        let switch_result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match poll(&mut switcher, &mut host).await {
                    GroupPoll::Data(_) | GroupPoll::Keepalive(_) => {
                        if host.installed > 0 {
                            break;
                        }
                    }
                    GroupPoll::Ended(end) => panic!("ended unexpectedly: {end:?}"),
                    GroupPoll::Pending => {}
                }
            }
        })
        .await;
        assert!(switch_result.is_ok(), "switched to target member within timeout");
        assert_eq!(switcher.driver.current, 1);
        let res = rx.await.expect("response");
        assert!(res.is_ok(), "manual switch succeeded: {res:?}");
    }

    #[tokio::test]
    async fn manual_switch_failure_keeps_current_member() {
        let env = GroupPlaybackEnv::builder(&["solis", "korath"])
            .origin_mode("korath", FakeOriginMode::Status(500))
            .build()
            .await;
        let user = env.app_state.app_config.get_user_credentials("viewer").expect("viewer");
        let ctx = GroupFailoverContext {
            app_state: Arc::clone(&env.app_state),
            target: Arc::clone(&env.target),
            target_name: env.target.name.as_str().into(),
            group: Arc::clone(&env.group_record),
            playback: shared::model::FailoverGroupPlayback {
                failover: FailoverGroupRuntimeConfig::default(),
                ..shared::model::FailoverGroupsConfig::default().playback(None)
            },
            user: (*user).clone(),
            fingerprint: crate::auth::Fingerprint::new(
                "fp".to_string(),
                "127.0.0.1".to_string(),
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            ),
            req_headers: axum::http::HeaderMap::new(),
            session_token: "switcher-session".into(),
            connection_kind: tuliprox_session::active_provider_manager::ConnectionKind::Normal,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            share_stream: false,
            base_channel: shared::model::StreamChannel::default(),
            shared_key: None,
        };
        let mut switcher = GroupSwitcher::new(GroupFailoverDriver::new(Arc::new(ctx), 0));
        let mut host = FakeHost::new(futures::stream::pending().boxed(), false);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let target_vid = env.member_vid("korath");
        let sender = switcher.switch_sender();
        sender.send(ManualSwitchCommand { target_member_vid: target_vid, respondent: tx }).expect("send");

        let returned = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match poll(&mut switcher, &mut host).await {
                    GroupPoll::Data(_) | GroupPoll::Keepalive(_) => {
                        if host.installed > 0 {
                            break;
                        }
                    }
                    GroupPoll::Ended(end) => panic!("stream ended on manual switch failure: {end:?}"),
                    GroupPoll::Pending => {}
                }
            }
        })
        .await;
        assert!(returned.is_ok(), "re-installed current member within timeout");
        assert_eq!(switcher.driver.current, 0, "stayed on current member (solis)");
        let res = rx.await.expect("response");
        assert!(res.is_err(), "manual switch reported failure: {res:?}");
    }

    #[tokio::test]
    async fn manual_switch_failure_does_not_adopt_third_member() {
        let env = GroupPlaybackEnv::builder(&["solis", "korath", "maris"])
            .origin_mode("solis", FakeOriginMode::Status(500))
            .origin_mode("korath", FakeOriginMode::Status(500))
            .origin_mode("maris", FakeOriginMode::TsThenStall(member_ts(0x100, 20)))
            .build()
            .await;
        let user = env.app_state.app_config.get_user_credentials("viewer").expect("viewer");
        let ctx = GroupFailoverContext {
            app_state: Arc::clone(&env.app_state),
            target: Arc::clone(&env.target),
            target_name: env.target.name.as_str().into(),
            group: Arc::clone(&env.group_record),
            playback: shared::model::FailoverGroupPlayback {
                failover: FailoverGroupRuntimeConfig::default(),
                ..shared::model::FailoverGroupsConfig::default().playback(None)
            },
            user: (*user).clone(),
            fingerprint: crate::auth::Fingerprint::new(
                "fp".to_string(),
                "127.0.0.1".to_string(),
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            ),
            req_headers: axum::http::HeaderMap::new(),
            session_token: "switcher-session".into(),
            connection_kind: tuliprox_session::active_provider_manager::ConnectionKind::Normal,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            share_stream: false,
            base_channel: shared::model::StreamChannel::default(),
            shared_key: None,
        };
        let mut switcher = GroupSwitcher::new(GroupFailoverDriver::new(Arc::new(ctx), 0));
        let mut host = FakeHost::new(futures::stream::pending().boxed(), false);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let target_vid = env.member_vid("korath");
        let sender = switcher.switch_sender();
        sender.send(ManualSwitchCommand { target_member_vid: target_vid, respondent: tx }).expect("send");

        let ended = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match poll(&mut switcher, &mut host).await {
                    GroupPoll::Data(_) | GroupPoll::Keepalive(_) => {
                        assert_eq!(host.installed, 0, "unexpectedly installed a member");
                    }
                    GroupPoll::Ended(end) => return end,
                    GroupPoll::Pending => {}
                }
            }
        })
        .await;
        assert!(ended.is_ok(), "switch ended within timeout");
        assert!(matches!(ended.unwrap(), GroupEnd::SwitchFailed { reason: SwitchReason::Manual }));
        assert_eq!(switcher.driver.current, 0, "did not adopt third member (maris)");
        let res = rx.await.expect("response");
        assert_eq!(res.unwrap_err(), SwitchError::TargetFailedUnrestored);
    }

    #[tokio::test(start_paused = true)]
    async fn manual_switch_cooldown_rejects_subsequent_request_within_2s() {
        let env = GroupPlaybackEnv::builder(&["solis", "korath", "maris"])
            .origin_mode("solis", FakeOriginMode::TsThenStall(member_ts(0x100, 50)))
            .origin_mode("korath", FakeOriginMode::TsThenStall(member_ts(0x100, 50)))
            .origin_mode("maris", FakeOriginMode::TsThenStall(member_ts(0x100, 50)))
            .build()
            .await;
        disable_burst(&env);
        let user = env.app_state.app_config.get_user_credentials("viewer").expect("viewer");
        let ctx = GroupFailoverContext {
            app_state: Arc::clone(&env.app_state),
            target: Arc::clone(&env.target),
            target_name: env.target.name.as_str().into(),
            group: Arc::clone(&env.group_record),
            playback: shared::model::FailoverGroupPlayback {
                failover: FailoverGroupRuntimeConfig::default(),
                ..shared::model::FailoverGroupsConfig::default().playback(None)
            },
            user: (*user).clone(),
            fingerprint: crate::auth::Fingerprint::new(
                "fp".to_string(),
                "127.0.0.1".to_string(),
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            ),
            req_headers: axum::http::HeaderMap::new(),
            session_token: "switcher-session".into(),
            connection_kind: tuliprox_session::active_provider_manager::ConnectionKind::Normal,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            share_stream: false,
            base_channel: shared::model::StreamChannel::default(),
            shared_key: None,
        };
        let mut switcher = GroupSwitcher::new(GroupFailoverDriver::new(Arc::new(ctx), 0));
        let mut host = FakeHost::new(futures::stream::pending().boxed(), false);

        let (tx1, rx1) = tokio::sync::oneshot::channel();
        let target_vid_korath = env.member_vid("korath");
        let sender = switcher.switch_sender();
        sender.send(ManualSwitchCommand { target_member_vid: target_vid_korath, respondent: tx1 }).expect("send 1");

        loop {
            match poll(&mut switcher, &mut host).await {
                GroupPoll::Data(_) | GroupPoll::Keepalive(_) => {
                    if host.installed > 0 {
                        break;
                    }
                }
                GroupPoll::Ended(end) => panic!("ended unexpectedly: {end:?}"),
                GroupPoll::Pending => {}
            }
        }
        let res1 = rx1.await.expect("res1");
        assert!(res1.is_ok(), "first switch accepted");
        assert_eq!(switcher.driver.current, 1);

        tokio::time::advance(Duration::from_secs(1)).await;

        let (tx2, rx2) = tokio::sync::oneshot::channel();
        let target_vid_maris = env.member_vid("maris");
        sender.send(ManualSwitchCommand { target_member_vid: target_vid_maris, respondent: tx2 }).expect("send 2");

        let _ = poll(&mut switcher, &mut host).await;
        let res2 = rx2.await.expect("res2");
        match res2 {
            Err(SwitchError::Cooldown { retry_after }) => {
                assert_eq!(retry_after, Duration::from_secs(1));
            }
            other => panic!("expected Cooldown, got {other:?}"),
        }

        tokio::time::advance(Duration::from_millis(1100)).await;

        let (tx3, rx3) = tokio::sync::oneshot::channel();
        sender.send(ManualSwitchCommand { target_member_vid: target_vid_maris, respondent: tx3 }).expect("send 3");

        loop {
            match poll(&mut switcher, &mut host).await {
                GroupPoll::Data(_) | GroupPoll::Keepalive(_) => {
                    if host.installed > 1 {
                        break;
                    }
                }
                GroupPoll::Ended(end) => panic!("ended unexpectedly: {end:?}"),
                GroupPoll::Pending => {}
            }
        }
        let res3 = rx3.await.expect("res3");
        assert!(res3.is_ok(), "third switch accepted after cooldown expired");
        assert_eq!(switcher.driver.current, 2);
    }

    #[tokio::test]
    async fn shared_stream_guard_cleanup_on_stream_drop() {
        use super::super::GroupFailoverStream;
        use futures::StreamExt;

        let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
        let user = env.app_state.app_config.get_user_credentials("viewer").expect("viewer");
        let ctx = GroupFailoverContext {
            app_state: Arc::clone(&env.app_state),
            target_name: env.target.name.as_str().into(),
            target: Arc::clone(&env.target),
            group: Arc::clone(&env.group_record),
            fingerprint: crate::auth::Fingerprint::new(
                "fp".to_string(),
                "127.0.0.1".to_string(),
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            ),
            user: (*user).clone(),
            playback: shared::model::FailoverGroupPlayback {
                failover: FailoverGroupRuntimeConfig::default(),
                ..shared::model::FailoverGroupsConfig::default().playback(None)
            },
            req_headers: axum::http::HeaderMap::new(),
            session_token: "tok".into(),
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: tuliprox_session::active_provider_manager::ConnectionKind::Normal,
            share_stream: true,
            base_channel: shared::model::StreamChannel::default(),
            shared_key: Some("group:1:100".into()),
        };
        let target_id = ctx.target.id;
        let group_vid = ctx.group.group_vid.get();
        let driver = GroupFailoverDriver::new(Arc::new(ctx), 0);
        let stream = GroupFailoverStream::new(driver, futures::stream::empty().boxed(), None);

        // While stream exists, registry has the shared switch entry:
        assert!(env.app_state.failover_switch_registry.contains_shared(target_id, group_vid));

        // When stream ends / errors and drops:
        drop(stream);

        // Registry entry must be cleaned up:
        assert!(!env.app_state.failover_switch_registry.contains_shared(target_id, group_vid));
    }

    #[tokio::test]
    async fn driver_reads_trim_target_from_stream_config() {
        let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
        let user = env.app_state.app_config.get_user_credentials("viewer").expect("viewer");
        let make_ctx = || GroupFailoverContext {
            app_state: Arc::clone(&env.app_state),
            target: Arc::clone(&env.target),
            target_name: env.target.name.as_str().into(),
            group: Arc::clone(&env.group_record),
            playback: shared::model::FailoverGroupPlayback {
                failover: FailoverGroupRuntimeConfig::default(),
                ..shared::model::FailoverGroupsConfig::default().playback(None)
            },
            user: (*user).clone(),
            fingerprint: crate::auth::Fingerprint::new(
                "fp".to_string(),
                "127.0.0.1".to_string(),
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            ),
            req_headers: axum::http::HeaderMap::new(),
            session_token: "test-session".into(),
            connection_kind: tuliprox_session::active_provider_manager::ConnectionKind::Normal,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            share_stream: false,
            base_channel: shared::model::StreamChannel::default(),
            shared_key: None,
        };
        let driver_default = GroupFailoverDriver::new(Arc::new(make_ctx()), 0);
        assert!(driver_default.is_aligning());

        disable_burst(&env);
        let driver_zero = GroupFailoverDriver::new(Arc::new(make_ctx()), 0);
        assert!(!driver_zero.is_aligning());
    }

    struct SwitchBurstHost {
        body: Option<BoxedProviderStream>,
        replacement: Option<BoxedProviderStream>,
        installed: u32,
    }

    impl MemberHost for SwitchBurstHost {
        fn member_body(&mut self) -> Option<&mut BoxedProviderStream> { self.body.as_mut() }
        fn member_preempted(&self) -> bool { false }
        fn release_member(&mut self) { self.body = None; }
        fn install_member(&mut self, _member: RuntimeMember) {
            self.installed += 1;
            self.body = self.replacement.take();
        }
    }

    #[tokio::test]
    async fn switch_with_shared_burst_starts_at_expected_gop() {
        let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
        let mut config = (**env.app_state.app_config.config.load()).clone();
        if let Some(ref mut reverse_proxy) = config.reverse_proxy {
            if let Some(ref mut stream) = reverse_proxy.stream {
                stream.shared_burst_secs = 2;
            }
        }
        env.app_state.app_config.config.store(Arc::new(config));

        let user = env.app_state.app_config.get_user_credentials("viewer").expect("viewer");
        let ctx = GroupFailoverContext {
            app_state: Arc::clone(&env.app_state),
            target: Arc::clone(&env.target),
            target_name: env.target.name.as_str().into(),
            group: Arc::clone(&env.group_record),
            playback: shared::model::FailoverGroupPlayback {
                failover: FailoverGroupRuntimeConfig::default(),
                ..shared::model::FailoverGroupsConfig::default().playback(None)
            },
            user: (*user).clone(),
            fingerprint: crate::auth::Fingerprint::new(
                "fp".to_string(),
                "127.0.0.1".to_string(),
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            ),
            req_headers: axum::http::HeaderMap::new(),
            session_token: "test-burst-switch".into(),
            connection_kind: tuliprox_session::active_provider_manager::ConnectionKind::Normal,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            share_stream: false,
            base_channel: shared::model::StreamChannel::default(),
            shared_key: None,
        };
        let mut switcher = GroupSwitcher::new(GroupFailoverDriver::new(Arc::new(ctx), 0));

        let gop0 = member_pcr_gop(0x200, 0, 2);
        let gop1 = member_pcr_gop(0x200, 1, 2);
        let gop2 = member_pcr_gop(0x200, 2, 2);
        let gop3 = member_pcr_gop(0x200, 3, 2);
        let gop4 = member_pcr_gop(0x200, 4, 2);

        let mut burst = Vec::new();
        burst.extend_from_slice(&gop0);
        burst.extend_from_slice(&gop1);
        burst.extend_from_slice(&gop2);
        burst.extend_from_slice(&gop3);

        let replacement = futures::stream::unfold(0_u32, move |step| {
            let burst = burst.clone();
            let gop4 = gop4.clone();
            async move {
                match step {
                    0 => Some((Ok(Bytes::from(burst)), 1)),
                    1 => {
                        tokio::time::sleep(Duration::from_millis(1100)).await;
                        Some((Ok(Bytes::from(gop4)), 2))
                    }
                    _ => None,
                }
            }
        })
        .boxed();

        let mut host = SwitchBurstHost {
            body: Some(futures::stream::empty().boxed()),
            replacement: Some(replacement),
            installed: 0,
        };

        let mut received = Vec::new();
        let loop_result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match poll(&mut switcher, &mut host).await {
                    GroupPoll::Data(bytes) => {
                        received.extend_from_slice(&bytes);
                        break;
                    }
                    GroupPoll::Keepalive(_) | GroupPoll::Pending => {}
                    GroupPoll::Ended(end) => panic!("stream ended unexpectedly: {end:?}"),
                }
            }
        })
        .await;

        assert!(loop_result.is_ok(), "received settled output within timeout");
        assert_eq!(host.installed, 1, "member was installed on switch");

        // First keyframe packet of member 1 (PID 0x200) must start at GOP 2 (PCR 2s = 180,000 ticks)
        // because burst target was 2s and newest GOP was 4s (threshold = 4s - 2s = 2s).
        let first_pcr = first_keyframe_pcr(&received, 0x200);
        assert_eq!(first_pcr, Some(180_000), "switched stream trimmed GOP 0 and 1, starting at GOP 2");
    }

    #[tokio::test(start_paused = true)]
    async fn test_f3_2_group_hold_deadline_flushes_before_stall_timeout() {
        let failover = FailoverGroupRuntimeConfig {
            no_data_timeout_secs: 4,
            max_outage_secs: 20,
            ..FailoverGroupRuntimeConfig::default()
        };
        let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
        let mut config = (**env.app_state.app_config.config.load()).clone();
        if let Some(ref mut reverse_proxy) = config.reverse_proxy {
            if let Some(ref mut stream) = reverse_proxy.stream {
                stream.shared_burst_secs = 3;
            }
        }
        env.app_state.app_config.config.store(Arc::new(config));

        let user = env.app_state.app_config.get_user_credentials("viewer").expect("viewer");
        let ctx = GroupFailoverContext {
            app_state: Arc::clone(&env.app_state),
            target: Arc::clone(&env.target),
            target_name: env.target.name.as_str().into(),
            group: Arc::clone(&env.group_record),
            playback: shared::model::FailoverGroupPlayback {
                failover,
                ..shared::model::FailoverGroupsConfig::default().playback(None)
            },
            user: (*user).clone(),
            fingerprint: crate::auth::Fingerprint::new(
                "fp".to_string(),
                "127.0.0.1".to_string(),
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            ),
            req_headers: axum::http::HeaderMap::new(),
            session_token: "test-hold-deadline".into(),
            connection_kind: tuliprox_session::active_provider_manager::ConnectionKind::Normal,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            share_stream: false,
            base_channel: shared::model::StreamChannel::default(),
            shared_key: None,
        };
        let mut switcher = GroupSwitcher::new(GroupFailoverDriver::new(Arc::new(ctx), 0));

        let (tx, rx) = futures::channel::mpsc::unbounded();
        let mut host = FakeHost::new(rx.boxed(), false);

        // Send a burst chunk: GOP 0. With shared_burst_secs = 3, aligner holds it.
        let gop0 = member_pcr_gop(0x100, 0, 2);
        tx.unbounded_send(Ok(Bytes::from(gop0.clone()))).unwrap();

        // Poll once to ingest the chunk; returns Pending because data is held.
        let handle = tokio::spawn(async move {
            let res = poll(&mut switcher, &mut host).await;
            (switcher, host, res)
        });

        // Advance 2 seconds (the hold deadline = min(4s, 4s/2) = 2s):
        tokio::time::advance(Duration::from_millis(2100)).await;

        let (switcher, host, res) = handle.await.unwrap();
        // At the hold deadline (before the stall timeout of 4s), data arrives!
        match res {
            GroupPoll::Data(bytes) => {
                assert_eq!(&bytes[..188], &gop0[188..376], "held burst emitted at deadline (starts at PAT)");
            }
            other => panic!("expected GroupPoll::Data at hold deadline, got {other:?}"),
        }
        // Member stays installed, no switch happened!
        assert_eq!(host.released, 0, "no switch occurred");
        assert_eq!(switcher.driver.current, 0, "still on member 0");
        assert_eq!(
            switcher.driver.pending_since,
            Some(tokio::time::Instant::now().into_std()),
            "pending_since is reset to the flush instant"
        );
    }

    #[tokio::test(start_paused = true)]
    #[allow(clippy::too_many_lines)]
    async fn switch_hold_releases_at_player_buffer_floor_while_body_pending() {
        let failover = FailoverGroupRuntimeConfig {
            no_data_timeout_secs: 10,
            max_outage_secs: 20,
            ..FailoverGroupRuntimeConfig::default()
        };
        let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
        let mut config = (**env.app_state.app_config.config.load()).clone();
        if let Some(ref mut reverse_proxy) = config.reverse_proxy {
            if let Some(ref mut stream) = reverse_proxy.stream {
                stream.shared_burst_secs = 3;
            }
        }
        env.app_state.app_config.config.store(Arc::new(config));

        let user = env.app_state.app_config.get_user_credentials("viewer").expect("viewer");
        let ctx = GroupFailoverContext {
            app_state: Arc::clone(&env.app_state),
            target: Arc::clone(&env.target),
            target_name: env.target.name.as_str().into(),
            group: Arc::clone(&env.group_record),
            playback: shared::model::FailoverGroupPlayback {
                failover,
                ..shared::model::FailoverGroupsConfig::default().playback(None)
            },
            user: (*user).clone(),
            fingerprint: crate::auth::Fingerprint::new(
                "fp".to_string(),
                "127.0.0.1".to_string(),
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            ),
            req_headers: axum::http::HeaderMap::new(),
            session_token: "test-switch-floor-hold".into(),
            connection_kind: tuliprox_session::active_provider_manager::ConnectionKind::Normal,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            share_stream: false,
            base_channel: shared::model::StreamChannel::default(),
            shared_key: None,
        };
        let mut switcher = GroupSwitcher::new(GroupFailoverDriver::new(Arc::new(ctx), 0));

        // Member 0 plays media seconds 0..=4 at once (PCR only, so no PSI versions are emitted): the player buffer
        // grows by the media delta (4 s) minus the wall time (0 s), all at the same paused instant.
        let _ = switcher.driver.process_chunk(Bytes::from(pcr_packet(0)));
        let _ = switcher.driver.aligner.finish_at(clock_now());
        for sec in 1..=4_u64 {
            let _ = switcher.driver.process_chunk(Bytes::from(pcr_packet(sec * 27_000_000)));
        }
        let _ = switcher.driver.take_pending_output();

        // One wall second drains the buffer: 4 s - 1 s = 3 s at the switch.
        tokio::time::advance(Duration::from_secs(1)).await;
        let switch_instant = tokio::time::Instant::now();
        switcher.driver.on_switched(1);
        assert_eq!(switcher.driver.aligner.buffer_at(clock_now()), Duration::from_secs(3), "buffer at the switch");

        // Burst target 3 s, remaining buffer 3 s: the floor (remaining <= 1 s) is reached 2 s after the switch.
        // The hold deadline is min(SPLICE_BUDGET, no_data_timeout / 2) = 4 s, the stall timeout 10 s.
        let floor_instant = switch_instant + Duration::from_secs(2);
        let hold_deadline = switch_instant + Duration::from_secs(4);
        let stall_deadline = switch_instant + Duration::from_secs(10);

        // Member 1: a burst of 2 s media with one RAP (PCR 0) at once, then realtime chunks every 100 ms.
        // Until the floor, the effective target equals the elapsed time, so the only RAP (span 2 s + elapsed)
        // is older than effective target + FRESH_RAP_SLACK: the settled hold stays held.
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let mut host = FakeHost::new(rx.boxed(), false);
        let gop0 = member_pcr_gop(0x100, 0, 2);
        let mut burst = gop0.clone();
        for pcr_ms in [500_u64, 1000, 1500, 2000] {
            burst.extend(pcr_packet(pcr_ms * 27_000));
        }
        tx.unbounded_send(Ok(Bytes::from(burst))).unwrap();
        let res = poll_once(&mut switcher, &mut host);
        assert!(matches!(res, GroupPoll::Pending), "holding the burst got: {res:?}");
        // Without more data the RAP (span 2 s) would turn fresh once the effective target reaches 1.5 s.
        assert_eq!(
            switcher.driver.aligner.hold_deadline(clock_now()),
            Some((switch_instant + Duration::from_millis(1500)).into_std()),
            "the next wake is the fresh-RAP instant, not the hold deadline"
        );

        for step in 1..=19_u64 {
            tokio::time::advance(Duration::from_millis(100)).await;
            tx.unbounded_send(Ok(Bytes::from(pcr_packet((2000 + step * 100) * 27_000)))).unwrap();
            let res = poll_once(&mut switcher, &mut host);
            assert!(
                matches!(res, GroupPoll::Pending),
                "settled hold with a stale RAP stays held at step {step}: {res:?}"
            );
        }
        assert_eq!(tokio::time::Instant::now(), switch_instant + Duration::from_millis(1900));

        // The provider body now stays pending: the hold must wake by itself at the floor instant.
        let handle = tokio::spawn(async move {
            let res = poll(&mut switcher, &mut host).await;
            (switcher, host, res)
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        let (switcher, host, res) = handle.await.expect("poll task");
        let emit_instant = tokio::time::Instant::now();

        assert_eq!(emit_instant, floor_instant, "released at the floor instant");
        assert!(emit_instant < hold_deadline, "earlier than the hold deadline");
        assert!(emit_instant < stall_deadline, "earlier than the stall timeout");
        let bytes = match res {
            GroupPoll::Data(bytes) => bytes,
            other => panic!("expected GroupPoll::Data at the floor instant, got {other:?}"),
        };
        assert_eq!(&bytes[..188], &gop0[188..376], "emitted bytes start at the PAT of the RAP");
        assert_eq!(bytes.len(), gop0.len() - 188 + (4 + 19) * 188, "the held bytes from the PAT on");
        assert_eq!(switcher.driver.pending_since, Some(emit_instant.into_std()), "pending_since is the flush instant");
        assert_eq!(host.released, 0, "no switch occurred");
        assert_eq!(switcher.driver.current, 1, "still on the switched-to member");
    }

    #[tokio::test]
    async fn test_f3_5_held_member_then_eof_emits_data_then_ended_eof() {
        let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
        let mut config = (**env.app_state.app_config.config.load()).clone();
        if let Some(ref mut reverse_proxy) = config.reverse_proxy {
            if let Some(ref mut stream) = reverse_proxy.stream {
                stream.shared_burst_secs = 3;
            }
        }
        env.app_state.app_config.config.store(Arc::new(config));

        let user = env.app_state.app_config.get_user_credentials("viewer").expect("viewer");
        let ctx = GroupFailoverContext {
            app_state: Arc::clone(&env.app_state),
            target: Arc::clone(&env.target),
            target_name: env.target.name.as_str().into(),
            group: Arc::clone(&env.group_record),
            playback: shared::model::FailoverGroupPlayback {
                failover: no_budget(),
                ..shared::model::FailoverGroupsConfig::default().playback(None)
            },
            user: (*user).clone(),
            fingerprint: crate::auth::Fingerprint::new(
                "fp".to_string(),
                "127.0.0.1".to_string(),
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            ),
            req_headers: axum::http::HeaderMap::new(),
            session_token: "test-f3-5-eof".into(),
            connection_kind: tuliprox_session::active_provider_manager::ConnectionKind::Normal,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            share_stream: false,
            base_channel: shared::model::StreamChannel::default(),
            shared_key: None,
        };
        let mut switcher = GroupSwitcher::new(GroupFailoverDriver::new(Arc::new(ctx), 0));

        let gop0 = member_pcr_gop(0x100, 0, 2);
        let stream = futures::stream::iter(vec![Ok(Bytes::from(gop0.clone()))]).boxed();
        let mut host = FakeHost::new(stream, false);

        // First poll: returns the exact held bytes as Data!
        let poll1 = poll(&mut switcher, &mut host).await;
        match poll1 {
            GroupPoll::Data(bytes) => assert_eq!(&bytes[..188], &gop0[188..376], "starts at PAT"),
            other => panic!("expected GroupPoll::Data on EOF with held bytes, got {other:?}"),
        }

        // Second poll: returns Ended(Eof)
        let poll2 = poll(&mut switcher, &mut host).await;
        assert!(matches!(poll2, GroupPoll::Ended(GroupEnd::Eof)));
    }

    #[tokio::test]
    async fn test_f3_5_held_member_then_error_emits_data_then_ended_error() {
        let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
        let mut config = (**env.app_state.app_config.config.load()).clone();
        if let Some(ref mut reverse_proxy) = config.reverse_proxy {
            if let Some(ref mut stream) = reverse_proxy.stream {
                stream.shared_burst_secs = 3;
            }
        }
        env.app_state.app_config.config.store(Arc::new(config));

        let user = env.app_state.app_config.get_user_credentials("viewer").expect("viewer");
        let ctx = GroupFailoverContext {
            app_state: Arc::clone(&env.app_state),
            target: Arc::clone(&env.target),
            target_name: env.target.name.as_str().into(),
            group: Arc::clone(&env.group_record),
            playback: shared::model::FailoverGroupPlayback {
                failover: no_budget(),
                ..shared::model::FailoverGroupsConfig::default().playback(None)
            },
            user: (*user).clone(),
            fingerprint: crate::auth::Fingerprint::new(
                "fp".to_string(),
                "127.0.0.1".to_string(),
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            ),
            req_headers: axum::http::HeaderMap::new(),
            session_token: "test-f3-5-err".into(),
            connection_kind: tuliprox_session::active_provider_manager::ConnectionKind::Normal,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            share_stream: false,
            base_channel: shared::model::StreamChannel::default(),
            shared_key: None,
        };
        let mut switcher = GroupSwitcher::new(GroupFailoverDriver::new(Arc::new(ctx), 0));

        let gop0 = member_pcr_gop(0x100, 0, 2);
        let stream = futures::stream::iter(vec![
            Ok(Bytes::from(gop0.clone())),
            Err(StreamError::Stream("upstream boom".into())),
        ])
        .boxed();
        let mut host = FakeHost::new(stream, false);

        // First poll: returns the exact held bytes as Data!
        let poll1 = poll(&mut switcher, &mut host).await;
        match poll1 {
            GroupPoll::Data(bytes) => assert_eq!(&bytes[..188], &gop0[188..376], "starts at PAT"),
            other => panic!("expected GroupPoll::Data on error with held bytes, got {other:?}"),
        }

        // Second poll: returns Ended(Error)
        let poll2 = poll(&mut switcher, &mut host).await;
        assert!(matches!(poll2, GroupPoll::Ended(GroupEnd::Error(_))));
    }

    #[tokio::test]
    async fn test_f3_5_first_rap_without_pat_then_eof_ends_without_data() {
        let (_env, mut switcher) = switcher(no_budget()).await;
        // Member 0 has no burst (target 0), so begin_member on member 1 starts Aligning::FirstRap
        switcher.driver.on_switched(1);
        assert!(switcher.driver.is_aligning());

        // Member 1 body delivers non-PAT packets (e.g. video continuation without PAT), then EOF:
        let non_pat_pkt = vec![0x47, 0x01, 0x00, 0x10, 0xFF];
        let mut non_pat_chunk = non_pat_pkt;
        non_pat_chunk.resize(188, 0xFF);
        let stream = futures::stream::iter(vec![Ok(Bytes::from(non_pat_chunk))]).boxed();
        let mut host = FakeHost::new(stream, false);

        // Poll must end WITHOUT emitting data (no mid-GOP garbage prefix):
        let poll_res = poll(&mut switcher, &mut host).await;
        assert!(
            matches!(poll_res, GroupPoll::Ended(GroupEnd::Eof)),
            "FirstRap without PAT must end directly without emitting data, got {poll_res:?}"
        );
    }
}
