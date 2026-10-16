use super::{
    group_playback_support::{has_pid, member_ts, read_body_until, GroupPlaybackEnv},
    support::FakeOriginMode,
};
use shared::model::{ChannelGroupFailoverConfig, EventMessage, SwitchReason};
use std::time::Duration;

const SOLIS_PID: u16 = 0x100;
const KORATH_PID: u16 = 0x200;

fn runtime_selection(no_data_timeout_secs: u64, max_outage_secs: u64) -> ChannelGroupFailoverConfig {
    ChannelGroupFailoverConfig { no_data_timeout_secs, max_outage_secs, ..ChannelGroupFailoverConfig::default() }
}

async fn wait_until(timeout: Duration, condition: impl Fn() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while !condition() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

fn input_slots(env: &GroupPlaybackEnv, input: &str) -> usize {
    env.app_state.active_provider.active_connections().unwrap_or_default().get(input).copied().unwrap_or_default()
}

#[tokio::test]
async fn switches_member_on_upstream_eof_without_client_reconnect() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(SOLIS_PID, 40)))
        .origin_mode("korath", FakeOriginMode::TsStream(member_ts(KORATH_PID, 40)))
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    assert_eq!(response.status(), 200);
    let (body, ended) = read_body_until(response, Duration::from_secs(5), |read| has_pid(read, KORATH_PID)).await;
    assert!(!ended, "the client body stays open across the switch");
    assert!(has_pid(&body, SOLIS_PID) && has_pid(&body, KORATH_PID));
    assert_eq!((env.origin_hits("solis"), env.origin_hits("korath")), (1, 1));
}

#[tokio::test]
async fn switches_member_on_stall() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .with_failover(runtime_selection(1, 20))
        .origin_mode("solis", FakeOriginMode::TsThenStall(member_ts(SOLIS_PID, 40)))
        .origin_mode("korath", FakeOriginMode::TsStream(member_ts(KORATH_PID, 40)))
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    let (body, ended) = read_body_until(response, Duration::from_secs(5), |read| has_pid(read, KORATH_PID)).await;
    assert!(!ended);
    assert!(has_pid(&body, KORATH_PID));
}

#[tokio::test]
async fn provider_slot_released_on_switch() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(SOLIS_PID, 40)))
        .origin_mode("korath", FakeOriginMode::TsStream(member_ts(KORATH_PID, 40)))
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    let (_body, ended) = read_body_until(response, Duration::from_secs(5), |read| has_pid(read, KORATH_PID)).await;
    assert!(!ended);
    assert!(
        wait_until(Duration::from_secs(2), || input_slots(&env, "solis") == 0 && input_slots(&env, "korath") == 1)
            .await
    );
}

#[tokio::test]
async fn runtime_budget_exhausted_ends_with_channel_unavailable() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_failover(runtime_selection(8, 2))
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(SOLIS_PID, 10)))
        .origin_mode("korath", FakeOriginMode::FixedBytes(member_ts(KORATH_PID, 10)))
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    let (_body, ended) = read_body_until(response, Duration::from_secs(8), |_| false).await;
    assert!(ended, "the body ends once the switch budget is spent");
    assert!(wait_until(Duration::from_secs(2), || env.slots_in_use() == 0).await);
}

#[tokio::test]
async fn runtime_switch_does_not_use_grace() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .with_failover(runtime_selection(8, 2))
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(SOLIS_PID, 40)))
        .origin_mode("korath", FakeOriginMode::TsStream(member_ts(KORATH_PID, 40)))
        .build()
        .await;
    let _korath_full = env.hold("korath", 0);
    let response = env.get_xtream_live(env.group_vid).await;
    let (body, ended) = read_body_until(response, Duration::from_secs(6), |_| false).await;
    assert!(ended);
    assert!(!has_pid(&body, KORATH_PID));
    assert_eq!(env.origin_hits("korath"), 0);
}

#[tokio::test]
async fn switch_emits_member_switched_event() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(SOLIS_PID, 40)))
        .origin_mode("korath", FakeOriginMode::TsStream(member_ts(KORATH_PID, 40)))
        .build()
        .await;
    let mut events = env.app_state.event_manager.get_event_channel();
    let response = env.get_xtream_live(env.group_vid).await;
    let (_body, _) = read_body_until(response, Duration::from_secs(5), |read| has_pid(read, KORATH_PID)).await;
    let switched = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(EventMessage::ChannelGroupMemberSwitched { from_member_vid, to_member_vid, reason, .. }) =
                events.recv().await
            {
                return (from_member_vid, to_member_vid, reason);
            }
        }
    })
    .await
    .expect("switch event");
    assert_eq!(switched, (env.member_vid("solis"), env.member_vid("korath"), SwitchReason::UpstreamEof));
}

#[tokio::test]
async fn non_group_stream_eof_unchanged() {
    let plain = member_ts(SOLIS_PID, 10);
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .origin_mode("solis", FakeOriginMode::FixedBytes(plain.clone()))
        .build()
        .await;
    let response = env.get_xtream_live(env.plain_vid).await;
    let (body, ended) = read_body_until(response, Duration::from_secs(5), |_| false).await;
    assert!(ended);
    assert_eq!(body, plain);
    assert_eq!(env.origin_hits("korath"), 0);
}

#[tokio::test]
async fn switch_continues_at_pat_with_new_version() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(SOLIS_PID, 40)))
        .origin_mode("korath", FakeOriginMode::TsStream(member_ts(KORATH_PID, 40)))
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    let (body, _) = read_body_until(response, Duration::from_secs(5), |read| has_pid(read, KORATH_PID)).await;
    let packets: Vec<&[u8]> = body.as_chunks::<188>().0.iter().map(<[u8; 188]>::as_slice).collect();
    let pid = |packet: &[u8]| (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]);
    let last_solis = packets.iter().rposition(|packet| pid(packet) == SOLIS_PID).expect("solis packets");
    let first_korath = packets.iter().position(|packet| pid(packet) == KORATH_PID).expect("korath packets");
    let between: Vec<u16> = packets[last_solis + 1..first_korath].iter().map(|packet| pid(packet)).collect();
    let splice = between.iter().position(|pid| *pid == 0).expect("PAT before the new member");
    assert!(between[..splice].iter().all(|pid| *pid == 0x1FFF), "only keep-alive null packets before the splice");
    assert_eq!(between[splice + 1], 0x1000, "PMT follows the PAT");
    let pat = packets[last_solis + 1 + splice];
    assert_eq!((pat[10] >> 1) & 0x1F, 1, "PAT version continues after the first member's version 0");
}

#[tokio::test]
async fn shared_group_stream_switches_member_and_moves_allocation() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .share_ts()
        .with_qos("solis", 1, 95)
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(SOLIS_PID, 40)))
        .origin_mode("korath", FakeOriginMode::TsStream(member_ts(KORATH_PID, 40)))
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    assert_eq!(response.status(), 200);
    let (body, ended) = read_body_until(response, Duration::from_secs(5), |read| has_pid(read, KORATH_PID)).await;
    assert!(!ended, "the shared subscriber keeps its response across the switch");
    assert!(has_pid(&body, KORATH_PID));
    assert!(
        wait_until(Duration::from_secs(2), || input_slots(&env, "solis") == 0 && input_slots(&env, "korath") == 1)
            .await
    );
}

#[tokio::test]
async fn group_stream_sleep_timer_ends_without_switch() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        // Only an end of the member body may trigger a switch; the paused clock would fire the
        // stall and underrun checks.
        .with_failover(ChannelGroupFailoverConfig {
            no_data_timeout_secs: 0,
            slow_source_pct: 0,
            ..ChannelGroupFailoverConfig::default()
        })
        .sleep_timer_mins(1)
        .origin_mode("solis", FakeOriginMode::TsStream(member_ts(SOLIS_PID, 40)))
        .origin_mode("korath", FakeOriginMode::TsStream(member_ts(KORATH_PID, 40)))
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    tokio::time::pause();
    let (_body, ended) = read_body_until(response, Duration::from_secs(120), |_| false).await;
    assert!(ended, "the sleep timer ends the response");
    assert_eq!(env.origin_hits("korath"), 0, "the sleep timer triggers no switch");
    assert!(
        matches!(env.circuit("solis"), tuliprox_session::channel_group::CircuitView::Closed),
        "the sleep timer is no member failure"
    );
}

#[tokio::test]
async fn start_skips_member_whose_open_hangs() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .origin_mode("solis", FakeOriginMode::Silent)
        .origin_mode("korath", FakeOriginMode::TsStream(member_ts(KORATH_PID, 40)))
        .build()
        .await;
    let response = tokio::time::timeout(Duration::from_secs(6), env.get_xtream_live(env.group_vid))
        .await
        .expect("the hanging open is bounded");
    let (body, _) = read_body_until(response, Duration::from_secs(3), |read| has_pid(read, KORATH_PID)).await;
    assert!(has_pid(&body, KORATH_PID));
}

#[tokio::test]
async fn runtime_switch_to_silent_member_respects_budget() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .with_failover(runtime_selection(8, 2))
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(SOLIS_PID, 10)))
        .origin_mode("korath", FakeOriginMode::Silent)
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    let (_body, ended) = read_body_until(response, Duration::from_secs(6), |_| false).await;
    assert!(ended, "the switch budget bounds a member that never answers");
    assert!(wait_until(Duration::from_secs(2), || env.slots_in_use() == 0).await);
}

#[tokio::test]
async fn stall_after_spent_budget_ends_stream() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_failover(runtime_selection(1, 2))
        .origin_mode("solis", FakeOriginMode::TsThenStall(member_ts(SOLIS_PID, 10)))
        .origin_mode("korath", FakeOriginMode::TsThenStall(member_ts(KORATH_PID, 10)))
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    let (_body, ended) = read_body_until(response, Duration::from_secs(8), |_| false).await;
    assert!(ended, "a stall after the spent budget ends the response");
    assert!(wait_until(Duration::from_secs(2), || env.slots_in_use() == 0).await);
}

#[tokio::test]
async fn retry_same_member_on_single_connection_account() {
    let selection = ChannelGroupFailoverConfig {
        no_data_timeout_secs: 1,
        stable_after_secs: 1,
        same_source_retries: 1,
        max_outage_secs: 10,
        ..ChannelGroupFailoverConfig::default()
    };
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .with_failover(selection)
        .max_connections("solis", 1)
        .origin_mode("solis", FakeOriginMode::TsThenStall(member_ts(SOLIS_PID, 40)))
        .build()
        .await;
    let _korath_full = env.hold("korath", 0);
    let response = env.get_xtream_live(env.group_vid).await;
    let (_body, ended) = read_body_until(response, Duration::from_secs(6), |_| env.origin_hits("solis") >= 2).await;
    assert!(!ended, "the stream waits for its own released slot instead of ending");
    assert_eq!(env.origin_hits("solis"), 2);
}

#[tokio::test]
async fn preempted_shared_group_member_does_not_fail_over() {
    use super::super::channel_group_failover::{GroupFailoverContext, GroupFailoverDriver, GroupFailoverStream};
    use futures::StreamExt;

    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
    let user = env.app_state.app_config.get_user_credentials("viewer").expect("viewer");
    let ctx = GroupFailoverContext {
        app_state: std::sync::Arc::clone(&env.app_state),
        target: std::sync::Arc::clone(&env.target),
        target_name: env.target.name.as_str().into(),
        group: std::sync::Arc::clone(&env.group_record),
        playback: shared::model::ChannelGroupsConfig::default().playback(None),
        user: (*user).clone(),
        fingerprint: super::support::create_test_fingerprint(std::net::SocketAddr::from(([127, 0, 0, 1], 12345))),
        req_headers: axum::http::HeaderMap::new(),
        session_token: "preempted-session".into(),
        connection_kind: tuliprox_session::active_provider_manager::ConnectionKind::Normal,
        connection_permission: shared::model::UserConnectionPermission::Allowed,
        share_stream: true,
        base_channel: shared::model::StreamChannel::default(),
        shared_key: Some("group:1:preempted".into()),
    };
    let slot = env
        .app_state
        .active_provider
        .acquire_connection(
            &"solis".into(),
            &std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            0,
            tuliprox_session::active_provider_manager::ConnectionKind::Normal,
        )
        .expect("solis slot");
    let slot =
        tuliprox_session::ManagedProviderHandle::new(std::sync::Arc::clone(&env.app_state.active_provider), slot);
    // The provider manager cancels this token when it preempts the slot.
    slot.handle().and_then(|handle| handle.cancel_token.as_ref()).expect("cancel token").cancel();
    let ended_body = futures::stream::empty().boxed();
    let mut stream =
        GroupFailoverStream::new(GroupFailoverDriver::new(std::sync::Arc::new(ctx), 0), ended_body, Some(&slot));
    let next = tokio::time::timeout(Duration::from_secs(2), stream.next()).await.expect("ends without a switch");
    assert!(next.is_none());
    assert_eq!((env.origin_hits("solis"), env.origin_hits("korath")), (0, 0));
}

async fn session_provider(env: &GroupPlaybackEnv) -> String {
    env.app_state.active_users.bound_member_session_provider("viewer").await.map(|p| p.to_string()).unwrap_or_default()
}

#[tokio::test]
async fn group_session_provider_follows_selected_member() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("korath", 2, 95)
        .origin_mode("korath", FakeOriginMode::TsStream(member_ts(KORATH_PID, 40)))
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    let (_body, _) = read_body_until(response, Duration::from_secs(3), |read| has_pid(read, KORATH_PID)).await;
    assert_eq!(session_provider(&env).await, "korath");
}

#[tokio::test]
async fn group_session_provider_follows_runtime_switch() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(SOLIS_PID, 40)))
        .origin_mode("korath", FakeOriginMode::TsStream(member_ts(KORATH_PID, 40)))
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    let (_body, _) = read_body_until(response, Duration::from_secs(5), |read| has_pid(read, KORATH_PID)).await;
    assert_eq!(session_provider(&env).await, "korath");
}

fn no_flap_guard(no_data_timeout_secs: u64, max_outage_secs: u64) -> ChannelGroupFailoverConfig {
    ChannelGroupFailoverConfig { max_switches: 0, ..runtime_selection(no_data_timeout_secs, max_outage_secs) }
}

#[tokio::test]
async fn all_members_ending_quickly_are_bounded_by_budget_without_flap_guard() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_failover(no_flap_guard(8, 2))
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(SOLIS_PID, 10)))
        .origin_mode("korath", FakeOriginMode::FixedBytes(member_ts(KORATH_PID, 10)))
        .build()
        .await;
    let started = std::time::Instant::now();
    let response = env.get_xtream_live(env.group_vid).await;
    let (_body, ended) = read_body_until(response, Duration::from_secs(8), |_| false).await;
    assert!(ended, "the switch budget alone ends the response");
    assert!(started.elapsed() < Duration::from_secs(6), "ended after {:?}", started.elapsed());
}

#[tokio::test]
async fn all_members_stalling_are_bounded_by_budget_without_flap_guard() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_failover(no_flap_guard(1, 3))
        .origin_mode("solis", FakeOriginMode::TsThenStall(member_ts(SOLIS_PID, 10)))
        .origin_mode("korath", FakeOriginMode::TsThenStall(member_ts(KORATH_PID, 10)))
        .build()
        .await;
    let started = std::time::Instant::now();
    let response = env.get_xtream_live(env.group_vid).await;
    let (_body, ended) = read_body_until(response, Duration::from_secs(12), |_| false).await;
    assert!(ended, "the switch budget alone ends the response");
    assert!(started.elapsed() < Duration::from_secs(9), "ended after {:?}", started.elapsed());
}

#[tokio::test]
async fn switch_retries_failed_member_until_budget() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .with_failover(no_flap_guard(8, 8))
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(SOLIS_PID, 10)))
        .origin_mode("korath", FakeOriginMode::Silent)
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    let (_body, ended) = read_body_until(response, Duration::from_secs(14), |_| false).await;
    assert!(ended);
    assert!(env.origin_hits("korath") >= 2, "korath tried {} times within the budget", env.origin_hits("korath"));
}

#[tokio::test]
async fn give_up_still_records_member_failure() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .with_failover(no_flap_guard(8, 0))
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(SOLIS_PID, 10)))
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    let (_body, ended) = read_body_until(response, Duration::from_secs(5), |_| false).await;
    assert!(ended, "a spent budget ends the response at once");
    assert!(
        matches!(env.circuit("solis"), tuliprox_session::channel_group::CircuitView::Counting(1)),
        "the failure that ended the stream counts for the member"
    );
}

#[tokio::test]
async fn failed_switch_shows_channel_unavailable_video() {
    const MEMBER_PID: u16 = 0x150;
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .channel_unavailable_video()
        .with_failover(no_flap_guard(8, 2))
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(MEMBER_PID, 10)))
        .origin_mode("korath", FakeOriginMode::Status(503))
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    // The fixture's video PID 0x100 shows the channel-unavailable clip.
    let (body, _) = read_body_until(response, Duration::from_secs(8), |read| has_pid(read, 0x100)).await;
    assert!(has_pid(&body, MEMBER_PID));
    assert!(has_pid(&body, 0x100), "the failed switch ends with the channel-unavailable video");
}

#[tokio::test]
async fn shared_switch_keeps_both_subscribers_and_rebinds_their_provider() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .share_ts()
        .with_qos("solis", 1, 95)
        .origin_mode("solis", FakeOriginMode::TsThenStall(member_ts(SOLIS_PID, 40)))
        .origin_mode("korath", FakeOriginMode::TsStream(member_ts(KORATH_PID, 40)))
        .with_failover(runtime_selection(1, 20))
        .build()
        .await;
    let first = env.get_xtream_live_from(env.group_vid, 12345).await;
    let second = env.get_xtream_live_from(env.group_vid, 12346).await;
    assert_eq!((first.status().as_u16(), second.status().as_u16()), (200, 200));
    let (one, two) = tokio::join!(
        read_body_until(first, Duration::from_secs(6), |read| has_pid(read, KORATH_PID)),
        read_body_until(second, Duration::from_secs(6), |read| has_pid(read, KORATH_PID))
    );
    assert!(has_pid(&one.0, KORATH_PID) && has_pid(&two.0, KORATH_PID), "both subscribers continue on korath");
    assert_eq!(env.origin_hits("solis"), 1, "one shared producer");
    let providers = env.app_state.active_users.stream_providers("viewer").await;
    assert_eq!(providers.len(), 2);
    assert!(providers.iter().all(|provider| provider.as_ref() == "korath"), "providers {providers:?}");
}

#[tokio::test]
async fn capacity_rejecting_member_is_opened_once_per_switch() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .with_failover(no_flap_guard(8, 4))
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(SOLIS_PID, 10)))
        .origin_mode("korath", FakeOriginMode::Status(429))
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    let (_body, ended) = read_body_until(response, Duration::from_secs(10), |_| false).await;
    assert!(ended);
    // One open repeats the HTTP request itself (request retry, up to 3 requests); further rounds
    // of the switch must not open the member again.
    assert!(
        env.origin_hits("korath") <= 3,
        "a provider that reports no capacity is not asked again in the same switch ({} requests)",
        env.origin_hits("korath")
    );
}

#[tokio::test]
async fn disabled_failover_ends_stream_with_its_member() {
    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("solis", 1, 95)
        .with_failover(ChannelGroupFailoverConfig { enabled: false, ..ChannelGroupFailoverConfig::default() })
        .origin_mode("solis", FakeOriginMode::FixedBytes(member_ts(SOLIS_PID, 10)))
        .origin_mode("korath", FakeOriginMode::TsStream(member_ts(KORATH_PID, 40)))
        .build()
        .await;
    let response = env.get_xtream_live(env.group_vid).await;
    let (body, ended) = read_body_until(response, Duration::from_secs(5), |_| false).await;
    assert!(ended, "the response ends with its member");
    assert!(has_pid(&body, SOLIS_PID));
    assert!(!has_pid(&body, KORATH_PID), "no switch without failover");
    assert_eq!(env.origin_hits("korath"), 0);
}
