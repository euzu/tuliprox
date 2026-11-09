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
        GroupEnd, GroupFailoverContext, GroupFailoverDriver, GroupPoll, GroupSwitcher, ManualSwitchCommand, MemberHost,
        RuntimeMember,
    };
    use crate::api::{
        api_utils::tests::group_playback_support::{member_ts, FakeOriginMode, GroupPlaybackEnv},
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

    async fn poll(switcher: &mut GroupSwitcher, host: &mut FakeHost) -> GroupPoll {
        std::future::poll_fn(|cx: &mut Context<'_>| match switcher.poll_member(host, cx) {
            GroupPoll::Pending => Poll::Pending,
            ready => Poll::Ready(ready),
        })
        .await
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
        assert_eq!(res.unwrap_err(), "Failed to open target member and failed to restore current member");
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
}
