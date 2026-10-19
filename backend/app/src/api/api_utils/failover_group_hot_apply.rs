//! Applies a failover override change that only reorders members or switches the policy of persisted
//! failover groups directly to the stored records, without a playlist update.

use crate::{
    api::model::AppState,
    repository::{
        get_target_id_mapping_file, get_target_storage_path, FailoverGroupMeta, FailoverGroupOrigin,
        FailoverGroupRecord, FailoverGroupStore, FailoverGroupTargetData,
    },
};
use log::warn;
use shared::model::{
    AutoGroupOverrideDto, ChannelMemberRefDto, FailoverGroupPolicy, FailoverGroupsConfig, ManualFailoverGroupDto,
    TargetFailoverOverridesDto, VirtualId,
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    hash::Hash,
    sync::Arc,
};
use tuliprox_core::model::{ConfigTarget, TargetOverridesEntry};
pub use tuliprox_repository::failover_group_repository::HotApplyGroupChange;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotApplyPlan {
    /// The changes to write (none when the effective state is unchanged).
    pub changes: Vec<HotApplyGroupChange>,
    /// Keys of the groups whose first member changes while the published playlist item still takes name, logo,
    /// EPG id or playlist group from the old one.
    pub display_stale: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotApplyDecision {
    /// Only order and policy changed, also of the first member: playback follows at once.
    Apply(HotApplyPlan),
    /// The change affects membership or metadata: wait for the next playlist update.
    NeedsReprocessing,
}

fn same_set<T: Eq + Hash>(left: &[T], right: &[T]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let right: HashSet<&T> = right.iter().collect();
    left.iter().all(|item| right.contains(item))
}

fn current_order(record: &FailoverGroupRecord) -> Vec<VirtualId> {
    record.members.iter().map(|member| member.member_vid).collect()
}

fn find_record<'a>(
    groups: &'a FailoverGroupTargetData,
    manual: bool,
    key: &str,
) -> Option<&'a Arc<FailoverGroupRecord>> {
    groups
        .groups
        .values()
        .find(|record| record.key.as_ref() == key && (record.origin == FailoverGroupOrigin::Manual) == manual)
}

/// New order of a manual group: the `members` list order, which must cover the stored members.
fn manual_order(record: &FailoverGroupRecord, members: &[ChannelMemberRefDto]) -> Option<Vec<VirtualId>> {
    let order: Vec<VirtualId> = members.iter().filter_map(|member| record.member_vid(member)).collect();
    (order.len() == record.members.len()).then_some(order)
}

/// New order of an auto group as processing computes it: members of `order` first, the rest in their
/// natural order. The natural order is only known for the members that were not ordered by the applied
/// section, so a rest member that the applied `order` placed explicitly forces a playlist update.
fn auto_order(
    record: &FailoverGroupRecord,
    applied_order: &[ChannelMemberRefDto],
    next_order: &[ChannelMemberRefDto],
) -> Option<Vec<VirtualId>> {
    let mut order: Vec<VirtualId> = Vec::with_capacity(record.members.len());
    let mut used: HashSet<VirtualId> = HashSet::with_capacity(record.members.len());
    for member in next_order {
        if let Some(vid) = record.member_vid(member) {
            if used.insert(vid) {
                order.push(vid);
            }
        }
    }

    let previously_ordered: HashSet<VirtualId> =
        applied_order.iter().filter_map(|member| record.member_vid(member)).collect();
    for member in &record.members {
        if used.contains(&member.member_vid) {
            continue;
        }
        if previously_ordered.contains(&member.member_vid) {
            return None;
        }
        order.push(member.member_vid);
    }
    Some(order)
}

/// Everything of a manual group except order and policy.
fn manual_content_equal(applied: &ManualFailoverGroupDto, next: &ManualFailoverGroupDto) -> bool {
    applied.id == next.id
        && applied.name == next.name
        && applied.logo == next.logo
        && applied.epg_channel_id == next.epg_channel_id
        && applied.playlist_group == next.playlist_group
        && same_set(&applied.members, &next.members)
}

/// Everything of an auto override except its key (the pairing) and order and policy.
fn auto_content_equal(applied: &AutoGroupOverrideDto, next: &AutoGroupOverrideDto) -> bool {
    applied.disabled == next.disabled
        && applied.name == next.name
        && applied.logo == next.logo
        && applied.epg_channel_id == next.epg_channel_id
        && applied.playlist_group == next.playlist_group
        && applied.add == next.add
        && same_set(&applied.remove, &next.remove)
}

/// Overrides by auto key; `None` when a key occurs twice, which no pairing can resolve.
fn overrides_by_key(overrides: &[AutoGroupOverrideDto]) -> Option<HashMap<&str, &AutoGroupOverrideDto>> {
    let by_key: HashMap<&str, &AutoGroupOverrideDto> =
        overrides.iter().map(|over| (over.auto_key.as_str(), over)).collect();
    (by_key.len() == overrides.len()).then_some(by_key)
}

/// Collects the group changes of a plan.
#[derive(Default)]
struct PlanBuilder {
    plan: HotApplyPlan,
}

impl PlanBuilder {
    /// Records the new `order` and `policy` of `record`; nothing when they equal the stored ones.
    /// `metadata_pinned`: the section pins everything the playlist item would take from the first member.
    fn push(
        &mut self,
        record: &FailoverGroupRecord,
        order: Vec<VirtualId>,
        policy: FailoverGroupPolicy,
        metadata_pinned: bool,
    ) {
        if policy == record.policy && order == current_order(record) {
            return;
        }
        let representative_changed = order.first().copied() != record.members.first().map(|member| member.member_vid);
        if representative_changed && !metadata_pinned {
            self.plan.display_stale.push(record.key.to_string());
        }
        self.plan.changes.push(HotApplyGroupChange { group_vid: record.group_vid, member_order: order, policy });
    }
}

fn plan_hot_apply(
    applied: &TargetFailoverOverridesDto,
    next: &TargetFailoverOverridesDto,
    groups: &FailoverGroupTargetData,
    config: &FailoverGroupsConfig,
) -> Option<HotApplyPlan> {
    if applied.target != next.target
        || applied.groups.len() != next.groups.len()
        || !same_set(&applied.excluded, &next.excluded)
    {
        return None;
    }

    let mut builder = PlanBuilder::default();

    // Manual groups keep their position (it decides which group claims a channel), so they pair by id in order.
    for (old, new) in applied.groups.iter().zip(&next.groups) {
        if !manual_content_equal(old, new) {
            return None;
        }
        let policy = new.policy.unwrap_or(config.policy);
        let order_changed = old.members != new.members;
        if !order_changed && old.policy.unwrap_or(config.policy) == policy {
            continue;
        }
        let record = find_record(groups, true, &new.id)?;
        let order = if order_changed { manual_order(record, &new.members)? } else { current_order(record) };
        let pinned = new.logo.is_some() && new.epg_channel_id.is_some() && new.playlist_group.is_some();
        builder.push(record, order, policy, pinned);
    }

    // Auto overrides pair by key; a group without an override has the default (empty) one.
    let applied_overrides = overrides_by_key(&applied.auto_overrides)?;
    let next_overrides = overrides_by_key(&next.auto_overrides)?;
    let keys: BTreeSet<&str> = applied_overrides.keys().chain(next_overrides.keys()).copied().collect();
    let default_override = AutoGroupOverrideDto::default();
    for key in keys {
        let old = applied_overrides.get(key).copied().unwrap_or(&default_override);
        let new = next_overrides.get(key).copied().unwrap_or(&default_override);
        if !auto_content_equal(old, new) {
            return None;
        }
        let policy = new.policy.unwrap_or(config.policy);
        let order_changed = old.order != new.order;
        if !order_changed && old.policy.unwrap_or(config.policy) == policy {
            continue;
        }
        let record = find_record(groups, false, key)?;
        let order = if order_changed { auto_order(record, &old.order, &new.order)? } else { current_order(record) };
        let pinned =
            new.name.is_some() && new.logo.is_some() && new.epg_channel_id.is_some() && new.playlist_group.is_some();
        builder.push(record, order, policy, pinned);
    }
    Some(builder.plan)
}

/// Decides whether `next` can be applied to the persisted `groups` without a playlist update: only the order
/// and the policy of groups may differ from the applied section, a new first member included.
///
/// `applied` is the section that produced `groups` (`FailoverGroupMeta::applied_overrides`); without it
/// nothing can be compared and the answer is [`HotApplyDecision::NeedsReprocessing`]. `config` carries
/// the policy that groups without an explicit one use.
pub fn decide_hot_apply(
    applied: Option<&TargetFailoverOverridesDto>,
    next: &TargetFailoverOverridesDto,
    groups: &FailoverGroupTargetData,
    config: &FailoverGroupsConfig,
) -> HotApplyDecision {
    applied
        .and_then(|applied| plan_hot_apply(applied, next, groups, config))
        .map_or(HotApplyDecision::NeedsReprocessing, HotApplyDecision::Apply)
}

/// The target named `target` when failover groups are enabled for it, with its grouping settings resolved
/// against `config.yml`.
pub(crate) fn enabled_failover_target(
    app_state: &AppState,
    target: &str,
) -> Option<(Arc<ConfigTarget>, FailoverGroupsConfig)> {
    let sources = app_state.app_config.sources.load();
    let found = sources.get_target_by_name(target)?;
    let groups_config =
        app_state.app_config.config.load().failover_groups.for_target(found.execution_plan.failover_groups.as_ref()?);
    Some((Arc::clone(found), groups_config))
}

enum Outcome {
    Refused,
    /// The store already reflects `next`.
    Unchanged,
    Written,
}

/// Applies `next` to the persisted failover groups of `target` when [`decide_hot_apply`] allows it.
///
/// Runs under the write lock persistence uses for the target, so it never interleaves with a
/// publication. The section that produced the store is the one recorded in its metadata; without it
/// nothing can be compared and the change is refused. On success the metadata records `next`, so the
/// following decision diffs against it, groups whose first member changed without pinned metadata are
/// listed in `display_stale_groups`, and the failover group cache generation is bumped. Returns `false`
/// when the change needs a playlist update or anything failed (logged).
pub async fn try_hot_apply(app_state: &AppState, target: &str, next: &TargetOverridesEntry) -> bool {
    let Some((_, groups_config)) = enabled_failover_target(app_state, target) else {
        return false;
    };
    let Some(target_path) = get_target_storage_path(&app_state.app_config.config.load(), target) else {
        return false;
    };

    let _target_lock = app_state.app_config.file_locks.write_lock(&get_target_id_mapping_file(&target_path)).await;
    let section = next.overrides.clone();
    let etag = Arc::clone(&next.etag);
    let outcome = tokio::task::spawn_blocking(move || -> std::io::Result<Outcome> {
        let data = FailoverGroupStore::load_target_data(&target_path, 0)?;
        let HotApplyDecision::Apply(plan) =
            decide_hot_apply(data.meta.applied_overrides.as_ref(), &section, &data, &groups_config)
        else {
            return Ok(Outcome::Refused);
        };
        if plan.changes.is_empty() && data.meta.overrides_etag.as_deref() == Some(etag.as_ref()) {
            return Ok(Outcome::Unchanged);
        }
        let mut display_stale_groups = data.meta.display_stale_groups;
        display_stale_groups.extend(plan.display_stale);
        display_stale_groups.sort_unstable();
        display_stale_groups.dedup();
        let meta = FailoverGroupMeta {
            overrides_etag: Some(etag),
            overrides_error: data.meta.overrides_error,
            applied_overrides: Some(section),
            display_stale_groups,
        };
        FailoverGroupStore::apply_hot_change(&target_path, &plan.changes, &meta)?;
        Ok(Outcome::Written)
    })
    .await;

    match outcome {
        Ok(Ok(Outcome::Written)) => {
            app_state.playlists.bump_failover_group_generation(target);
            true
        }
        Ok(Ok(Outcome::Unchanged)) => true,
        Ok(Ok(Outcome::Refused)) => false,
        Ok(Err(err)) => {
            warn!("Hot apply of failover group overrides for target {target} failed: {err}");
            false
        }
        Err(err) => {
            warn!("Hot apply of failover group overrides for target {target} was aborted: {err}");
            false
        }
    }
}
