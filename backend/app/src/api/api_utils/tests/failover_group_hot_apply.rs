use super::support::{create_test_app_config, create_test_app_state_for_config, create_test_shared_target};
use crate::{
    api::api_utils::failover_group_hot_apply::{
        decide_hot_apply, try_hot_apply, HotApplyDecision, HotApplyGroupChange, HotApplyPlan,
    },
    model::{Config, ConfigSource, SourcesConfig},
    repository::{
        FailoverGroupMeta, FailoverGroupOrigin, FailoverGroupRecord, FailoverGroupSideFiles, FailoverGroupStore,
        FailoverGroupTargetData, GroupMember,
    },
};
use arc_swap::ArcSwap;
use shared::model::{
    AutoGroupOverrideDto, ChannelMemberRefDto, FailoverGroupPolicy, FailoverGroupsConfig, FailoverGroupsTargetConfig,
    ManualFailoverGroupDto, PlaylistItemType, TargetFailoverOverridesDto, VirtualId, XtreamCluster,
};
use std::sync::Arc;
use tuliprox_core::model::TargetOverridesEntry;

const TARGET: &str = "hot target";
const GROUP_VID: VirtualId = VirtualId(100);

fn member(vid: VirtualId, input: &str, stream_id: &str) -> GroupMember {
    GroupMember {
        member_vid: vid,
        input_name: Arc::from(input),
        provider_id: 0,
        input_stream_id: Arc::from(stream_id),
        url: Arc::from("http://provider.test/live"),
        item_type: PlaylistItemType::Live,
        cluster: XtreamCluster::Live,
        title: Arc::from("ZDF"),
        archive: None,
        epg_channel_id: None,
        additional_properties: None,
    }
}

fn record(origin: FailoverGroupOrigin, key: &str, policy: FailoverGroupPolicy) -> FailoverGroupRecord {
    FailoverGroupRecord {
        group_vid: GROUP_VID,
        key: Arc::from(key),
        origin,
        policy,
        members: vec![member(VirtualId(1), "a", "1"), member(VirtualId(2), "b", "2"), member(VirtualId(3), "c", "3")],
        orphaned: Vec::new(),
    }
}

fn data(records: Vec<FailoverGroupRecord>) -> FailoverGroupTargetData {
    FailoverGroupTargetData {
        groups: records.into_iter().map(|record| (record.group_vid, Arc::new(record))).collect(),
        ..FailoverGroupTargetData::default()
    }
}

fn member_ref(input: &str, stream_id: &str) -> ChannelMemberRefDto {
    ChannelMemberRefDto { input: input.to_string(), stream_id: stream_id.to_string() }
}

fn auto_override(order: &[(&str, &str)]) -> AutoGroupOverrideDto {
    AutoGroupOverrideDto {
        auto_key: "zdf".to_string(),
        order: order.iter().map(|(input, id)| member_ref(input, id)).collect(),
        ..AutoGroupOverrideDto::default()
    }
}

fn pinned(mut ao: AutoGroupOverrideDto) -> AutoGroupOverrideDto {
    ao.name = Some("ZDF".to_string());
    ao.logo = Some("http://logo.test/zdf.png".to_string());
    ao.epg_channel_id = Some("zdf.de".to_string());
    ao.playlist_group = Some("Vollprogramm".to_string());
    ao
}

fn auto_section(ao: AutoGroupOverrideDto) -> TargetFailoverOverridesDto {
    TargetFailoverOverridesDto { auto_overrides: vec![ao], ..TargetFailoverOverridesDto::new(TARGET) }
}

fn manual(members: &[(&str, &str)]) -> ManualFailoverGroupDto {
    ManualFailoverGroupDto {
        id: "sport".to_string(),
        name: "Sport".to_string(),
        members: members.iter().map(|(input, id)| member_ref(input, id)).collect(),
        ..ManualFailoverGroupDto::default()
    }
}

fn manual_section(group: ManualFailoverGroupDto) -> TargetFailoverOverridesDto {
    TargetFailoverOverridesDto { groups: vec![group], ..TargetFailoverOverridesDto::new(TARGET) }
}

fn auto_config() -> FailoverGroupsConfig {
    FailoverGroupsConfig { policy: FailoverGroupPolicy::Auto, ..Default::default() }
}

fn auto_store() -> FailoverGroupTargetData {
    data(vec![record(FailoverGroupOrigin::AutoEdited, "zdf", FailoverGroupPolicy::Auto)])
}

fn manual_store() -> FailoverGroupTargetData {
    data(vec![record(FailoverGroupOrigin::Manual, "sport", FailoverGroupPolicy::Auto)])
}

fn vids(raw: &[u32]) -> Vec<VirtualId> { raw.iter().copied().map(VirtualId).collect() }

fn change(member_order: &[VirtualId], policy: FailoverGroupPolicy) -> HotApplyGroupChange {
    HotApplyGroupChange { group_vid: GROUP_VID, member_order: member_order.to_vec(), policy }
}

/// The plan of a decision that must hot apply.
fn plan_of(decision: HotApplyDecision) -> HotApplyPlan {
    match decision {
        HotApplyDecision::Apply(plan) => plan,
        HotApplyDecision::NeedsReprocessing => panic!("the change must hot apply"),
    }
}

#[test]
fn order_only_change_hot_applies() {
    let applied = auto_section(auto_override(&[]));
    let next = auto_section(auto_override(&[("a", "1"), ("c", "3")]));
    match decide_hot_apply(Some(&applied), &next, &auto_store(), &auto_config()) {
        HotApplyDecision::Apply(HotApplyPlan { changes, .. }) => {
            assert_eq!(changes, vec![change(&[VirtualId(1), VirtualId(3), VirtualId(2)], FailoverGroupPolicy::Auto)]);
        }
        HotApplyDecision::NeedsReprocessing => panic!("order-only change must hot apply"),
    }
}

#[test]
fn policy_only_change_hot_applies() {
    let applied = auto_section(auto_override(&[]));
    let mut next_override = auto_override(&[]);
    next_override.policy = Some(FailoverGroupPolicy::Priority);
    let next = auto_section(next_override);
    match decide_hot_apply(Some(&applied), &next, &auto_store(), &auto_config()) {
        HotApplyDecision::Apply(HotApplyPlan { changes, .. }) => {
            assert_eq!(
                changes,
                vec![change(&[VirtualId(1), VirtualId(2), VirtualId(3)], FailoverGroupPolicy::Priority)]
            );
        }
        HotApplyDecision::NeedsReprocessing => panic!("policy-only change must hot apply"),
    }
}

#[test]
fn explicit_policy_equal_to_the_default_changes_nothing() {
    let applied = auto_section(auto_override(&[]));
    let mut next_override = auto_override(&[]);
    next_override.policy = Some(FailoverGroupPolicy::Auto);
    let next = auto_section(next_override);
    assert_eq!(
        decide_hot_apply(Some(&applied), &next, &auto_store(), &auto_config()),
        HotApplyDecision::Apply(HotApplyPlan::default())
    );
}

#[test]
fn representative_change_hot_applies_and_marks_display_stale() {
    let applied = auto_section(auto_override(&[]));
    let next = auto_section(auto_override(&[("b", "2")]));
    let plan = plan_of(decide_hot_apply(Some(&applied), &next, &auto_store(), &auto_config()));
    assert_eq!(plan.changes, vec![change(&[VirtualId(2), VirtualId(1), VirtualId(3)], FailoverGroupPolicy::Auto)]);
    assert_eq!(plan.display_stale, ["zdf"]);
}

#[test]
fn pinned_metadata_representative_change_is_not_display_stale() {
    let applied = auto_section(pinned(auto_override(&[])));
    let next = auto_section(pinned(auto_override(&[("b", "2")])));
    let plan = plan_of(decide_hot_apply(Some(&applied), &next, &auto_store(), &auto_config()));
    assert_eq!(plan.changes, vec![change(&[VirtualId(2), VirtualId(1), VirtualId(3)], FailoverGroupPolicy::Auto)]);
    assert_eq!(plan.display_stale.len(), 0);
}

#[test]
fn partly_pinned_metadata_representative_change_is_display_stale() {
    let mut applied_override = pinned(auto_override(&[]));
    applied_override.logo = None;
    let mut next_override = pinned(auto_override(&[("b", "2")]));
    next_override.logo = None;
    let plan = plan_of(decide_hot_apply(
        Some(&auto_section(applied_override)),
        &auto_section(next_override),
        &auto_store(),
        &auto_config(),
    ));
    assert_eq!(plan.display_stale, ["zdf"]);
}

#[test]
fn order_change_keeping_the_representative_is_not_display_stale() {
    let applied = auto_section(auto_override(&[]));
    let next = auto_section(auto_override(&[("a", "1"), ("c", "3")]));
    let plan = plan_of(decide_hot_apply(Some(&applied), &next, &auto_store(), &auto_config()));
    assert_eq!(plan.changes.len(), 1);
    assert_eq!(plan.display_stale.len(), 0);
}

#[test]
fn first_reorder_of_untouched_auto_group_hot_applies() {
    // The applied section has no override at all for the group: the default (empty) override is the base.
    let applied = TargetFailoverOverridesDto::new(TARGET);
    let next = auto_section(auto_override(&[("a", "1"), ("c", "3")]));
    let plan = plan_of(decide_hot_apply(Some(&applied), &next, &auto_store(), &auto_config()));
    assert_eq!(plan.changes, vec![change(&[VirtualId(1), VirtualId(3), VirtualId(2)], FailoverGroupPolicy::Auto)]);
}

#[test]
fn first_policy_change_of_untouched_auto_group_hot_applies() {
    let applied = TargetFailoverOverridesDto::new(TARGET);
    let mut next_override = auto_override(&[]);
    next_override.policy = Some(FailoverGroupPolicy::Priority);
    let plan = plan_of(decide_hot_apply(Some(&applied), &auto_section(next_override), &auto_store(), &auto_config()));
    assert_eq!(plan.changes, vec![change(&[VirtualId(1), VirtualId(2), VirtualId(3)], FailoverGroupPolicy::Priority)]);
}

#[test]
fn removing_policy_only_override_hot_applies() {
    let next = TargetFailoverOverridesDto::new(TARGET);
    let mut policy_only = auto_override(&[]);
    policy_only.policy = Some(FailoverGroupPolicy::Priority);
    let store = data(vec![record(FailoverGroupOrigin::AutoEdited, "zdf", FailoverGroupPolicy::Priority)]);
    let plan = plan_of(decide_hot_apply(Some(&auto_section(policy_only)), &next, &store, &auto_config()));
    assert_eq!(plan.changes, vec![change(&[VirtualId(1), VirtualId(2), VirtualId(3)], FailoverGroupPolicy::Auto)]);
}

#[test]
fn removing_an_order_override_needs_reprocessing_while_the_natural_order_is_unknown() {
    // The applied override moved "c" to the front; where "c" belongs naturally is not stored.
    let applied = auto_section(auto_override(&[("c", "3")]));
    let next = TargetFailoverOverridesDto::new(TARGET);
    let store = data(vec![FailoverGroupRecord {
        members: vec![member(VirtualId(3), "c", "3"), member(VirtualId(1), "a", "1"), member(VirtualId(2), "b", "2")],
        ..record(FailoverGroupOrigin::AutoEdited, "zdf", FailoverGroupPolicy::Auto)
    }]);
    assert_eq!(decide_hot_apply(Some(&applied), &next, &store, &auto_config()), HotApplyDecision::NeedsReprocessing);
}

#[test]
fn removing_an_empty_override_applies_nothing() {
    let applied = auto_section(auto_override(&[]));
    let next = TargetFailoverOverridesDto::new(TARGET);
    assert_eq!(
        decide_hot_apply(Some(&applied), &next, &auto_store(), &auto_config()),
        HotApplyDecision::Apply(HotApplyPlan::default())
    );
}

#[test]
fn reordered_override_entries_do_not_force_reprocessing() {
    let rtl_record = FailoverGroupRecord {
        group_vid: VirtualId(101),
        ..record(FailoverGroupOrigin::AutoEdited, "rtl", FailoverGroupPolicy::Auto)
    };
    let store = data(vec![record(FailoverGroupOrigin::AutoEdited, "zdf", FailoverGroupPolicy::Auto), rtl_record]);
    let rtl = |order: &[(&str, &str)]| AutoGroupOverrideDto { auto_key: "rtl".to_string(), ..auto_override(order) };
    let applied = TargetFailoverOverridesDto {
        auto_overrides: vec![auto_override(&[]), rtl(&[])],
        ..TargetFailoverOverridesDto::new(TARGET)
    };
    // The entries come in the other order and only "rtl" changes its member order.
    let next = TargetFailoverOverridesDto {
        auto_overrides: vec![rtl(&[("a", "1"), ("c", "3")]), auto_override(&[])],
        ..TargetFailoverOverridesDto::new(TARGET)
    };
    let plan = plan_of(decide_hot_apply(Some(&applied), &next, &store, &auto_config()));
    assert_eq!(
        plan.changes,
        vec![HotApplyGroupChange {
            group_vid: VirtualId(101),
            member_order: vids(&[1, 3, 2]),
            policy: FailoverGroupPolicy::Auto,
        }]
    );
}

#[test]
fn duplicate_override_keys_need_reprocessing() {
    let applied = auto_section(auto_override(&[]));
    let next = TargetFailoverOverridesDto {
        auto_overrides: vec![auto_override(&[("a", "1")]), auto_override(&[("c", "3")])],
        ..TargetFailoverOverridesDto::new(TARGET)
    };
    assert_eq!(
        decide_hot_apply(Some(&applied), &next, &auto_store(), &auto_config()),
        HotApplyDecision::NeedsReprocessing
    );
}

#[test]
fn membership_change_needs_reprocessing() {
    let applied = auto_section(auto_override(&[]));
    let mut removed = auto_override(&[]);
    removed.remove = vec![member_ref("c", "3")];
    let mut added = auto_override(&[]);
    added.add = vec![member_ref("d", "4")];
    let mut dissolved = auto_override(&[]);
    dissolved.disabled = true;
    let mut renamed = auto_override(&[]);
    renamed.name = Some("Zweites".to_string());
    for next_override in [removed, added, dissolved, renamed] {
        assert_eq!(
            decide_hot_apply(Some(&applied), &auto_section(next_override), &auto_store(), &auto_config()),
            HotApplyDecision::NeedsReprocessing
        );
    }
}

#[test]
fn added_exclusions_or_manual_groups_need_reprocessing() {
    let applied = auto_section(auto_override(&[]));
    let mut excluded = auto_section(auto_override(&[]));
    excluded.excluded = vec![member_ref("z", "9")];
    let with_manual = TargetFailoverOverridesDto { groups: vec![manual(&[])], ..applied.clone() };
    for next in [excluded, with_manual] {
        assert_eq!(
            decide_hot_apply(Some(&applied), &next, &auto_store(), &auto_config()),
            HotApplyDecision::NeedsReprocessing
        );
    }
}

#[test]
fn unknown_applied_section_needs_reprocessing() {
    let next = auto_section(auto_override(&[("a", "1"), ("c", "3")]));
    assert_eq!(decide_hot_apply(None, &next, &auto_store(), &auto_config()), HotApplyDecision::NeedsReprocessing);
}

#[test]
fn group_missing_from_the_store_needs_reprocessing() {
    let applied = auto_section(auto_override(&[]));
    let next = auto_section(auto_override(&[("a", "1"), ("c", "3")]));
    assert_eq!(
        decide_hot_apply(Some(&applied), &next, &FailoverGroupTargetData::default(), &auto_config()),
        HotApplyDecision::NeedsReprocessing
    );
    let other_origin = data(vec![record(FailoverGroupOrigin::Manual, "zdf", FailoverGroupPolicy::Auto)]);
    assert_eq!(
        decide_hot_apply(Some(&applied), &next, &other_origin, &auto_config()),
        HotApplyDecision::NeedsReprocessing
    );
}

#[test]
fn equal_sections_apply_nothing() {
    let section = auto_section(auto_override(&[("a", "1")]));
    assert_eq!(
        decide_hot_apply(Some(&section), &section, &auto_store(), &auto_config()),
        HotApplyDecision::Apply(HotApplyPlan::default())
    );
}

#[test]
fn partial_order_that_drops_a_previously_ordered_member_needs_reprocessing() {
    // The stored order puts "b" and "c" first, so the natural order of "c" is not known any more.
    let applied = auto_section(auto_override(&[("b", "2"), ("c", "3")]));
    let next = auto_section(auto_override(&[("b", "2")]));
    let store = data(vec![FailoverGroupRecord {
        members: vec![member(VirtualId(2), "b", "2"), member(VirtualId(3), "c", "3"), member(VirtualId(1), "a", "1")],
        ..record(FailoverGroupOrigin::AutoEdited, "zdf", FailoverGroupPolicy::Auto)
    }]);
    assert_eq!(decide_hot_apply(Some(&applied), &next, &store, &auto_config()), HotApplyDecision::NeedsReprocessing);
}

#[test]
fn partial_order_extending_a_previous_order_hot_applies() {
    let applied = auto_section(auto_override(&[("b", "2")]));
    let next = auto_section(auto_override(&[("b", "2"), ("c", "3")]));
    let store = data(vec![FailoverGroupRecord {
        members: vec![member(VirtualId(2), "b", "2"), member(VirtualId(1), "a", "1"), member(VirtualId(3), "c", "3")],
        ..record(FailoverGroupOrigin::AutoEdited, "zdf", FailoverGroupPolicy::Auto)
    }]);
    match decide_hot_apply(Some(&applied), &next, &store, &auto_config()) {
        HotApplyDecision::Apply(HotApplyPlan { changes, .. }) => {
            assert_eq!(changes, vec![change(&[VirtualId(2), VirtualId(3), VirtualId(1)], FailoverGroupPolicy::Auto)]);
        }
        HotApplyDecision::NeedsReprocessing => panic!("extending the order must hot apply"),
    }
}

#[test]
fn manual_group_order_change_keeping_the_first_member_hot_applies() {
    let applied = manual_section(manual(&[("a", "1"), ("b", "2"), ("c", "3")]));
    let next = manual_section(manual(&[("a", "1"), ("c", "3"), ("b", "2")]));
    match decide_hot_apply(Some(&applied), &next, &manual_store(), &auto_config()) {
        HotApplyDecision::Apply(HotApplyPlan { changes, .. }) => {
            assert_eq!(changes, vec![change(&[VirtualId(1), VirtualId(3), VirtualId(2)], FailoverGroupPolicy::Auto)]);
        }
        HotApplyDecision::NeedsReprocessing => panic!("manual reorder must hot apply"),
    }
}

#[test]
fn manual_group_new_first_member_hot_applies_and_is_display_stale_unless_pinned() {
    let applied = manual_section(manual(&[("a", "1"), ("b", "2"), ("c", "3")]));
    let next = manual_section(manual(&[("c", "3"), ("a", "1"), ("b", "2")]));
    let plan = plan_of(decide_hot_apply(Some(&applied), &next, &manual_store(), &auto_config()));
    assert_eq!(plan.changes, vec![change(&[VirtualId(3), VirtualId(1), VirtualId(2)], FailoverGroupPolicy::Auto)]);
    assert_eq!(plan.display_stale, ["sport"]);

    let pin = |mut group: ManualFailoverGroupDto| {
        group.logo = Some("http://logo.test/sport.png".to_string());
        group.epg_channel_id = Some("sport.de".to_string());
        group.playlist_group = Some("Sport".to_string());
        group
    };
    let applied = manual_section(pin(manual(&[("a", "1"), ("b", "2"), ("c", "3")])));
    let next = manual_section(pin(manual(&[("c", "3"), ("a", "1"), ("b", "2")])));
    let plan = plan_of(decide_hot_apply(Some(&applied), &next, &manual_store(), &auto_config()));
    assert_eq!(plan.changes, vec![change(&[VirtualId(3), VirtualId(1), VirtualId(2)], FailoverGroupPolicy::Auto)]);
    assert_eq!(plan.display_stale.len(), 0);
}

#[test]
fn manual_group_membership_change_needs_reprocessing() {
    let applied = manual_section(manual(&[("a", "1"), ("b", "2"), ("c", "3")]));
    let next = manual_section(manual(&[("a", "1"), ("b", "2")]));
    assert_eq!(
        decide_hot_apply(Some(&applied), &next, &manual_store(), &auto_config()),
        HotApplyDecision::NeedsReprocessing
    );
}

struct HotApplyEnv {
    app_state: Arc<crate::api::model::AppState>,
    target_path: std::path::PathBuf,
    dir: tempfile::TempDir,
}

fn env_with_store(applied: &TargetFailoverOverridesDto, etag: &str) -> HotApplyEnv {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let mut target = create_test_shared_target();
    target.name = TARGET.to_string();
    target.execution_plan.failover_groups = Some(FailoverGroupsTargetConfig { enabled: true, ..Default::default() });

    let mut app_config = create_test_app_config();
    app_config.config = Arc::new(ArcSwap::from_pointee(Config {
        storage_dir: dir.path().to_string_lossy().into_owned(),
        failover_groups: auto_config(),
        ..Config::default()
    }));
    app_config.sources = Arc::new(ArcSwap::from_pointee(SourcesConfig {
        sources: vec![ConfigSource { inputs: Vec::new(), targets: vec![Arc::new(target)] }],
        ..SourcesConfig::default()
    }));
    let app_state = create_test_app_state_for_config(Arc::new(app_config));

    let target_path = dir.path().join(TARGET.replace(' ', "_"));
    std::fs::create_dir_all(&target_path).expect("target dir");
    let meta = FailoverGroupMeta {
        overrides_etag: Some(Arc::from(etag)),
        applied_overrides: Some(applied.clone()),
        ..FailoverGroupMeta::default()
    };
    FailoverGroupStore::write(
        &target_path,
        &[record(FailoverGroupOrigin::AutoEdited, "zdf", FailoverGroupPolicy::Auto)],
        &[],
        Some(FailoverGroupSideFiles { catalog: &[], meta: &meta }),
    )
    .expect("store written");
    HotApplyEnv { app_state, target_path, dir }
}

fn entry(overrides: TargetFailoverOverridesDto, etag: &str) -> TargetOverridesEntry {
    TargetOverridesEntry {
        overrides,
        file: std::path::PathBuf::from("failover.yml"),
        etag: Arc::from(etag),
        findings: Vec::new(),
    }
}

#[tokio::test]
async fn hot_apply_updates_store_meta_and_cache_generation() {
    let applied = auto_section(auto_override(&[]));
    let env = env_with_store(&applied, "etag-1");
    let mut next_override = auto_override(&[("a", "1"), ("c", "3")]);
    next_override.policy = Some(FailoverGroupPolicy::Priority);
    let next = entry(auto_section(next_override), "etag-2");

    let generation = env.app_state.playlists.failover_group_generation(TARGET);
    assert!(try_hot_apply(&env.app_state, TARGET, &next).await);

    let stored = FailoverGroupStore::read_group(&env.target_path, GROUP_VID).expect("read").expect("group");
    assert_eq!(stored.members.iter().map(|m| m.member_vid).collect::<Vec<_>>(), vids(&[1, 3, 2]));
    assert_eq!(stored.policy, FailoverGroupPolicy::Priority);
    let meta = FailoverGroupStore::read_meta(&env.target_path).expect("meta");
    assert_eq!(meta.overrides_etag.as_deref(), Some("etag-2"));
    assert_eq!(meta.applied_overrides.as_ref(), Some(&next.overrides));
    assert_eq!(env.app_state.playlists.failover_group_generation(TARGET), generation + 1);

    // The member index still resolves every member of the reordered group.
    let (_, idx) = FailoverGroupStore::read_member(&env.target_path, VirtualId(3)).expect("read").expect("member");
    assert_eq!(idx, 1);

    // The next decision diffs against the section that was just applied.
    let mut after_override = auto_override(&[("a", "1"), ("b", "2"), ("c", "3")]);
    after_override.policy = Some(FailoverGroupPolicy::Priority);
    let after = entry(auto_section(after_override), "etag-3");
    assert!(try_hot_apply(&env.app_state, TARGET, &after).await);
    let stored = FailoverGroupStore::read_group(&env.target_path, GROUP_VID).expect("read").expect("group");
    assert_eq!(stored.members.iter().map(|m| m.member_vid).collect::<Vec<_>>(), vids(&[1, 2, 3]));
    let meta = FailoverGroupStore::read_meta(&env.target_path).expect("meta");
    assert_eq!(meta.overrides_etag.as_deref(), Some("etag-3"));
}

#[tokio::test]
async fn structural_change_is_not_hot_applied() {
    let applied = auto_section(auto_override(&[]));
    let env = env_with_store(&applied, "etag-1");
    let mut removal = auto_override(&[]);
    removal.remove = vec![member_ref("c", "3")];
    let next = entry(auto_section(removal), "etag-2");

    let generation = env.app_state.playlists.failover_group_generation(TARGET);
    assert!(!try_hot_apply(&env.app_state, TARGET, &next).await);

    let stored = FailoverGroupStore::read_group(&env.target_path, GROUP_VID).expect("read").expect("group");
    assert_eq!(stored.members.iter().map(|m| m.member_vid).collect::<Vec<_>>(), vids(&[1, 2, 3]));
    let meta = FailoverGroupStore::read_meta(&env.target_path).expect("meta");
    assert_eq!(meta.overrides_etag.as_deref(), Some("etag-1"));
    assert_eq!(meta.applied_overrides.as_ref(), Some(&applied));
    assert_eq!(env.app_state.playlists.failover_group_generation(TARGET), generation);
}

#[tokio::test]
async fn hot_apply_without_a_stored_applied_section_is_refused() {
    let applied = auto_section(auto_override(&[]));
    let env = env_with_store(&applied, "etag-1");
    let meta = FailoverGroupMeta::default();
    FailoverGroupStore::write_meta(&env.target_path, &meta).expect("meta reset");
    let next = entry(auto_section(auto_override(&[("a", "1"), ("c", "3")])), "etag-2");
    assert!(!try_hot_apply(&env.app_state, TARGET, &next).await);
}

#[tokio::test]
async fn update_order_and_policy_rejects_incomplete_member_order() {
    let applied = auto_section(auto_override(&[]));
    let env = env_with_store(&applied, "etag-1");
    let bad = [change(&[VirtualId(1), VirtualId(2)], FailoverGroupPolicy::Auto)];
    assert!(FailoverGroupStore::update_order_and_policy(&env.target_path, &bad).is_err());
    let unknown = [change(&[VirtualId(1), VirtualId(2), VirtualId(9)], FailoverGroupPolicy::Auto)];
    assert!(FailoverGroupStore::update_order_and_policy(&env.target_path, &unknown).is_err());
    let stored = FailoverGroupStore::read_group(&env.target_path, GROUP_VID).expect("read").expect("group");
    assert_eq!(stored.members.iter().map(|m| m.member_vid).collect::<Vec<_>>(), vids(&[1, 2, 3]));
}

#[tokio::test]
async fn failover_order_change_hot_applies_on_reload() {
    let applied = auto_section(auto_override(&[]));
    let env = env_with_store(&applied, "etag-1");
    let config_dir = env.dir.path().join("conf");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let mut paths = (*env.app_state.app_config.paths.load_full()).clone();
    paths.config_path = config_dir.to_string_lossy().into_owned();
    env.app_state.app_config.paths.store(Arc::new(paths));

    let failover_file = config_dir.join("failover.yml");
    let source = tuliprox_core::utils::FailoverSource::File(failover_file.clone());
    env.app_state
        .app_config
        .failover
        .store(Arc::new(tuliprox_core::model::FailoverOverrides { source: Some(source), ..Default::default() }));
    std::fs::write(
        &failover_file,
        format!(
            "version: 1\ntargets:\n  - target: {TARGET}\n    auto_overrides:\n      - auto_key: zdf\n        order:\n          - {{ input: a, stream_id: \"1\" }}\n          - {{ input: c, stream_id: \"3\" }}\n"
        ),
    )
    .expect("failover file");
    let mut events = env.app_state.event_manager.get_event_channel();
    let generation = env.app_state.playlists.failover_group_generation(TARGET);

    crate::api::config_file::ConfigFile::Failover.reload(&failover_file, &env.app_state).await.expect("reload");

    let stored = FailoverGroupStore::read_group(&env.target_path, GROUP_VID).expect("read").expect("group");
    assert_eq!(stored.members.iter().map(|m| m.member_vid).collect::<Vec<_>>(), vids(&[1, 3, 2]));
    let snapshot = env.app_state.app_config.failover.load_full();
    let reloaded = snapshot.sections.get(TARGET).expect("section loaded");
    let meta = FailoverGroupStore::read_meta(&env.target_path).expect("meta");
    assert_eq!(meta.overrides_etag.as_deref(), Some(reloaded.etag.as_ref()));
    assert_eq!(env.app_state.playlists.failover_group_generation(TARGET), generation + 1);
    match events.try_recv() {
        Ok(crate::api::model::EventMessage::FailoverOverridesChanged { targets }) => {
            assert_eq!(targets, vec![TARGET.to_string()]);
        }
        other => panic!("expected the overrides event, got {other:?}"),
    }
}

#[tokio::test]
async fn representative_change_is_hot_applied_and_listed_as_display_stale() {
    let applied = auto_section(auto_override(&[]));
    let env = env_with_store(&applied, "etag-1");
    let next = entry(auto_section(auto_override(&[("b", "2")])), "etag-2");

    assert!(try_hot_apply(&env.app_state, TARGET, &next).await);

    let stored = FailoverGroupStore::read_group(&env.target_path, GROUP_VID).expect("read").expect("group");
    assert_eq!(stored.members.iter().map(|m| m.member_vid).collect::<Vec<_>>(), vids(&[2, 1, 3]));
    let meta = FailoverGroupStore::read_meta(&env.target_path).expect("meta");
    assert_eq!(meta.overrides_etag.as_deref(), Some("etag-2"));
    assert_eq!(meta.applied_overrides.as_ref(), Some(&next.overrides));
    assert_eq!(meta.display_stale_groups, ["zdf"]);

    // A later order-only change keeps the marker until a playlist update publishes the representative.
    let later = entry(auto_section(auto_override(&[("b", "2"), ("c", "3")])), "etag-3");
    assert!(try_hot_apply(&env.app_state, TARGET, &later).await);
    let meta = FailoverGroupStore::read_meta(&env.target_path).expect("meta");
    assert_eq!(meta.display_stale_groups, ["zdf"]);
}

#[tokio::test]
async fn pinned_metadata_representative_change_leaves_the_store_not_display_stale() {
    let applied = auto_section(pinned(auto_override(&[])));
    let env = env_with_store(&applied, "etag-1");
    let next = entry(auto_section(pinned(auto_override(&[("b", "2")]))), "etag-2");

    assert!(try_hot_apply(&env.app_state, TARGET, &next).await);

    let stored = FailoverGroupStore::read_group(&env.target_path, GROUP_VID).expect("read").expect("group");
    assert_eq!(stored.members.iter().map(|m| m.member_vid).collect::<Vec<_>>(), vids(&[2, 1, 3]));
    assert_eq!(FailoverGroupStore::read_meta(&env.target_path).expect("meta").display_stale_groups.len(), 0);
}
