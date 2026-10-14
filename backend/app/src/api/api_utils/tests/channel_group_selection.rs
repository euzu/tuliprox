use super::{
    super::channel_group::{
        acquire_group_member, acquire_group_member_with_race, GroupSelectionFailure, GroupSelectionRequest,
    },
    support::{create_test_app_config, create_test_app_state_for_config},
};
use crate::api::model::AppState;
use arc_swap::ArcSwap;
use shared::{
    model::{
        ChannelGroupPolicy, ChannelGroupsConfig, InputType, PlaylistItemType, ReverseProxyConfigDto, StreamConfigDto,
        VirtualId, XtreamCluster,
    },
    utils::Internable,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tuliprox_core::model::{Config, ConfigInput, ProviderHandle, ReverseProxyConfig, SourcesConfig};
use tuliprox_repository::{
    channel_group_repository::{ChannelGroupRecord, GroupMember},
    ChannelGroupOrigin,
};
use tuliprox_session::{
    active_provider_manager::{CapacityTier, ConnectionKind},
    channel_group::SelectionMode,
};

static CLIENT_ADDR: std::sync::LazyLock<SocketAddr> =
    std::sync::LazyLock::new(|| "127.0.0.1:40000".parse().expect("valid client addr"));

fn addr(port: u16) -> SocketAddr { SocketAddr::from(([127, 0, 0, 1], port)) }

fn xtream_input(id: u16, name: &str, max_connections: u16) -> Arc<ConfigInput> {
    Arc::new(ConfigInput {
        id,
        name: name.intern(),
        input_type: InputType::Xtream,
        url: format!("http://{name}.invalid"),
        username: Some("user".into()),
        password: Some("pass".into()),
        enabled: true,
        max_connections,
        ..ConfigInput::default()
    })
}

fn group_member(input: &str, stream_id: &str, provider_id: u32, member_vid: u32) -> GroupMember {
    GroupMember {
        member_vid: VirtualId::new(member_vid),
        input_name: input.into(),
        provider_id,
        input_stream_id: stream_id.into(),
        url: format!("http://{input}.invalid/{stream_id}.ts").into(),
        item_type: PlaylistItemType::Live,
        cluster: XtreamCluster::Live,
        title: format!("{input} stream").into(),
        archive: None,
        epg_channel_id: None,
        additional_properties: None,
    }
}

fn app_config_with_sources(sources: SourcesConfig) -> tuliprox_core::model::AppConfig {
    let mut config = Config::default();
    let dto = ReverseProxyConfigDto {
        stream: Some(StreamConfigDto { grace_period_millis: 2_000, ..StreamConfigDto::default() }),
        ..ReverseProxyConfigDto::default()
    };
    config.reverse_proxy = Some(ReverseProxyConfig::from(&dto));
    let mut app_config = create_test_app_config();
    app_config.config = Arc::new(ArcSwap::from_pointee(config));
    app_config.sources = Arc::new(ArcSwap::from_pointee(sources));
    app_config
}

struct SelectionEnv {
    app_state: Arc<AppState>,
    group: ChannelGroupRecord,
    config: ChannelGroupsConfig,
    target: Arc<str>,
}

impl SelectionEnv {
    fn new(members: &[(&str, u16)]) -> Self {
        let sources = SourcesConfig {
            inputs: members
                .iter()
                .enumerate()
                .map(|(i, (name, max))| xtream_input(u16::try_from(i + 1).expect("id"), name, *max))
                .collect(),
            ..SourcesConfig::default()
        };
        let app_state = create_test_app_state_for_config(Arc::new(app_config_with_sources(sources)));
        let group = ChannelGroupRecord {
            group_vid: VirtualId::new(1),
            key: "zdf".into(),
            origin: ChannelGroupOrigin::Auto,
            policy: ChannelGroupPolicy::Priority,
            orphaned: Vec::new(),
            members: members
                .iter()
                .enumerate()
                .map(|(i, (name, _))| {
                    group_member(
                        name,
                        &(i + 1).to_string(),
                        u32::try_from(i + 1).expect("provider_id"),
                        100 + u32::try_from(i).expect("member_vid"),
                    )
                })
                .collect(),
        };
        Self {
            app_state,
            group,
            config: serde_json::from_str(r#"{"enabled":true}"#).expect("config"),
            target: "Main".into(),
        }
    }

    fn hold(&self, input: &str, priority: i8) -> ProviderHandle {
        self.app_state
            .active_provider
            .acquire_connection(&input.into(), &addr(200), priority, ConnectionKind::Normal)
            .expect("hold slot")
    }

    fn request<'a>(&'a self, exclude: &'a [usize], allow_grace: bool) -> GroupSelectionRequest<'a> {
        GroupSelectionRequest {
            target_name: &self.target,
            group: &self.group,
            policy: self.group.policy,
            mode: SelectionMode::Live,
            selection: self.config.selection,
            addr: &CLIENT_ADDR,
            username: "viewer",
            priority: 0,
            kind: ConnectionKind::Normal,
            lease: None,
            allow_provider_grace: allow_grace,
            exclude,
            demoted_inputs: &[],
            prefer: None,
        }
    }

    fn slots_in_use(&self) -> usize {
        self.app_state.active_provider.active_connections().unwrap_or_default().values().sum()
    }
}

#[tokio::test]
async fn free_member_is_preferred_over_better_member_needing_grace() {
    let env = SelectionEnv::new(&[("korath", 1), ("solis", 1)]);
    let _blocker = env.hold("korath", 0);
    let acquired = acquire_group_member(&env.app_state, env.request(&[], true)).await.expect("acquired");
    assert_eq!((acquired.index, acquired.tier), (1, CapacityTier::Free));
}

#[tokio::test]
async fn lost_race_on_free_reprobes_free_before_grace() {
    let env = SelectionEnv::new(&[("solis", 1), ("korath", 1), ("veyra", 1)]);
    let _veyra_full = env.hold("veyra", 0);
    let acquired = acquire_group_member_with_race(&env.app_state, env.request(&[], true), |state| {
        let handle = state.active_provider.acquire_connection(&"solis".into(), &addr(201), 0, ConnectionKind::Normal);
        std::mem::forget(handle);
    })
    .await
    .expect("acquired");
    assert_eq!((acquired.index, acquired.tier), (1, CapacityTier::Free));
}

#[tokio::test]
async fn own_release_keeps_best_member_when_zapping() {
    let env = SelectionEnv::new(&[("korath", 1), ("solis", 1)]);
    let own = env.hold("korath", 0);
    env.app_state.active_users.set_pending_release_for_test("viewer", &[own.allocation_id]).await;
    let acquired = acquire_group_member(&env.app_state, env.request(&[], true)).await.expect("acquired");
    assert_eq!((acquired.index, acquired.tier), (0, CapacityTier::OwnRelease));
}

#[tokio::test]
async fn preemption_only_on_best_ranked_when_nothing_free() {
    let env = SelectionEnv::new(&[("korath", 1), ("solis", 1)]);
    let _a = env.hold("korath", 10);
    let _b = env.hold("solis", 10);
    let mut req = env.request(&[], false);
    req.priority = -5;
    let acquired = acquire_group_member(&env.app_state, req).await.expect("acquired");
    assert_eq!((acquired.index, acquired.tier), (0, CapacityTier::Preempt));
}

#[tokio::test]
async fn demoted_input_members_are_tried_last() {
    let env = SelectionEnv::new(&[("korath", 1), ("solis", 1)]);
    let demoted: [Arc<str>; 1] = ["korath".into()];
    let mut req = env.request(&[], true);
    req.demoted_inputs = &demoted;
    let acquired = acquire_group_member(&env.app_state, req).await.expect("acquired");
    assert_eq!((acquired.index, acquired.tier), (1, CapacityTier::Free));
}

#[tokio::test]
async fn grace_tier_skipped_when_disabled() {
    let env = SelectionEnv::new(&[("korath", 1)]);
    let _full = env.hold("korath", 0);
    let result = acquire_group_member(&env.app_state, env.request(&[], false)).await;
    assert!(matches!(result, Err(GroupSelectionFailure::Exhausted)));
}

#[tokio::test]
async fn all_members_exhausted_returns_exhausted_within_budget() {
    let mut env = SelectionEnv::new(&[("korath", 1), ("solis", 1)]);
    env.config.selection.startup_budget_secs = 1;
    let _a = env.hold("korath", 10);
    let _a_grace = env
        .app_state
        .active_provider
        .try_acquire_connection_no_preempt(&"korath".into(), true, &addr(201), 10, ConnectionKind::Normal, None)
        .expect("grace");
    let _b = env.hold("solis", 10);
    let _b_grace = env
        .app_state
        .active_provider
        .try_acquire_connection_no_preempt(&"solis".into(), true, &addr(202), 10, ConnectionKind::Normal, None)
        .expect("grace");
    let before = env.slots_in_use();
    let started = tokio::time::Instant::now();
    let mut req = env.request(&[], true);
    req.priority = 10;
    let result = acquire_group_member(&env.app_state, req).await;
    assert!(matches!(result, Err(GroupSelectionFailure::Exhausted)));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(env.slots_in_use(), before);
}
