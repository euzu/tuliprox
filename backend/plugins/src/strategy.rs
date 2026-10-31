//! Finalize-side playlist hooks: `after-sort` and the per-target strategies
//! `sort-key`, `assign-channel-numbers`, `resolve-duplicates` and
//! `merge-groups` (plan §7 *Sync/async boundary* pattern 2, prepare -> apply).
//!
//! Every function is the *prepare* half: it asks the plugin for a decision
//! (a permutation, keys, numbers, keepers, a merge map) and returns it as plain
//! data. The synchronous *apply* half lives in the pipeline
//! (`processor/playlist/target.rs`), so nothing here touches the playlist.
//!
//! Shape of one plugin run, shared by all five: pick the plugin
//! ([`HookDispatcher::runnable`] for the chained `after-sort`,
//! [`HookDispatcher::named`] for a strategy that names exactly one), open a
//! fresh initialized instance ([`HookDispatcher::open`]), make every guest call
//! through `call_with_deadline` followed by `host_fns::end_call`, apply nothing
//! until every call succeeded, and close the run once through
//! [`HookDispatcher::settle`]. A failing run returns `Ok(None)`, the host
//! default, unless the plugin runs under `fail_mode: abort-target`.
//!
//! `scope:` is honoured where a subset has a natural meaning (`after-sort`,
//! `assign-channel-numbers`). A strategy that needs a key or decision for every
//! item (`sort-key`, `resolve-duplicates`, `merge-groups`) is opt-in per target
//! by its config and is sent everything.
//!
//! # How `after-sort` maps to a whole-view permutation
//!
//! The WIT hook orders one *batch* of at most `batch_size` channels by handle
//! and nothing else: handles it does not list keep their relative order after
//! the listed ones, unknown or repeated handles are ignored. Each plugin
//! therefore sees the scoped items of the view in their current order, cut into
//! consecutive batches, and each batch is permuted within the positions it
//! already occupies. Chunk boundaries never move, so a channel never crosses
//! into another batch; a view that fits one batch (the default `batch_size` is
//! 500) is ordered completely by the plugin. Plugin B starts from A's order.
//! The result is `order`, where `order[k]` is the original index of the channel
//! that comes `k`-th, or `None` when nothing moved.

use crate::{
    convert::item_to_channel,
    dispatch::{count, HookDispatcher},
    host_fns,
    registry::{LoadedPlugin, PluginRecord},
    runtime::{call_with_deadline, CallError},
};
use shared::model::{
    HookCtx, PlaylistItem, PluginAbort, PluginChnoMap, PluginDuplicateGroup, PluginGroupMerge, PluginHook,
    PluginHookStats, PluginKeepers, PluginRunReport, PluginSortKeys,
};
use std::{collections::HashSet, time::Instant};
use tuliprox_plugin_api::bindings::{
    exports::tuliprox::plugin::dedupe_hooks::DuplicateGroup, tuliprox::plugin::types::Channel,
};

/// What a successful run changed, for the plugin's hook stats.
#[derive(Default)]
struct Tally {
    kept: usize,
    dropped: usize,
    modified: usize,
    ignored: usize,
}

/// Close one plugin's run through [`HookDispatcher::settle`] and turn it into
/// the decision the caller applies. `Ok(None)` is "use the host default".
#[allow(clippy::too_many_arguments, reason = "one call per plugin run, every argument is needed")]
fn close<T>(
    plugin: &PluginRecord,
    loaded: &LoadedPlugin,
    hook: PluginHook,
    ctx: &HookCtx<'_>,
    started: Instant,
    sent: usize,
    outcome: Result<(T, Tally), CallError>,
    report: &mut PluginRunReport,
) -> Result<Option<T>, PluginAbort> {
    let mut stats = PluginHookStats::new(&plugin.id, hook);
    let (decision, outcome) = match outcome {
        Ok((decision, tally)) => {
            stats.kept = count(tally.kept);
            stats.dropped = count(tally.dropped);
            stats.modified = count(tally.modified);
            stats.ignored_changes = count(tally.ignored);
            (Some(decision), Ok(()))
        }
        Err(err) => {
            stats.kept = count(sent);
            (None, Err(err))
        }
    };
    match HookDispatcher::settle(plugin, loaded, hook, ctx, stats, started, outcome.as_ref().copied(), report) {
        Some(abort) => Err(abort),
        None => Ok(decision),
    }
}

fn missing(interface: &str) -> CallError { CallError::Instantiate(format!("does not export {interface}")) }

fn bind(err: &wasmtime::Error) -> CallError { CallError::Trap(format!("{err:#}")) }

/// `channels` for `items[indices]`, handles `0..len`.
fn channels_of(items: &[&PlaylistItem], indices: &[usize]) -> Vec<Channel> {
    indices
        .iter()
        .enumerate()
        // A batch never exceeds MAX_PLUGINS_BATCH_SIZE, which fits in u32.
        .map(|(handle, &index)| item_to_channel(u32::try_from(handle).unwrap_or(u32::MAX), items[index]))
        .collect()
}

/// Indices into `items` that `loaded`'s scope matches, in order.
fn scoped_refs(loaded: &LoadedPlugin, items: &[&PlaylistItem]) -> Vec<usize> {
    if loaded.scope.is_all() {
        return (0..items.len()).collect();
    }
    items.iter().enumerate().filter(|(_, item)| loaded.scope.matches(item)).map(|(index, _)| index).collect()
}

/// Every plugin exporting `sort-hooks`, chained in run order over one view.
pub(crate) async fn after_sort(
    dispatcher: &HookDispatcher<'_>,
    ctx: &HookCtx<'_>,
    items: &[&PlaylistItem],
    report: &mut PluginRunReport,
) -> Result<Option<Vec<usize>>, PluginAbort> {
    let batch_size = dispatcher.batch_size();
    // order[k] = index into `items` of the channel at position k.
    let mut order: Vec<usize> = (0..items.len()).collect();
    for (plugin, loaded) in dispatcher.runnable(PluginHook::AfterSort) {
        // Positions (in the current order) of the items the plugin may see.
        let current: Vec<&PlaylistItem> = order.iter().map(|&index| items[index]).collect();
        let positions = scoped_refs(loaded, &current);
        if positions.is_empty() {
            continue;
        }
        let started = Instant::now();
        let outcome = after_sort_plugin(dispatcher, loaded, ctx, &current, &positions, batch_size).await;
        let outcome = outcome.map(|new_positions| {
            let before = order.clone();
            for (&position, new_index) in positions.iter().zip(new_positions) {
                order[position] = before[new_index];
            }
            let moved = order.iter().zip(&before).filter(|(now, was)| now != was).count();
            ((), Tally { kept: positions.len(), modified: moved, ..Tally::default() })
        });
        close(plugin, loaded, PluginHook::AfterSort, ctx, started, positions.len(), outcome, report)?;
    }
    Ok(order.iter().enumerate().any(|(position, &index)| position != index).then_some(order))
}

/// One plugin's order over the scoped positions. Returns, for each scoped
/// position, the `current` index that now belongs there.
async fn after_sort_plugin(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    ctx: &HookCtx<'_>,
    current: &[&PlaylistItem],
    positions: &[usize],
    batch_size: usize,
) -> Result<Vec<usize>, CallError> {
    let mut live = dispatcher.open(loaded).await?;
    let instance = live.instance();
    let indices = loaded.sort_hooks.as_ref().ok_or_else(|| missing("sort-hooks"))?;
    let guest = indices.load(live.store_mut(), &instance).map_err(|err| bind(&err))?;

    let mut placed = Vec::with_capacity(positions.len());
    for chunk in positions.chunks(batch_size) {
        let channels = channels_of(current, chunk);
        let limits = live.limits();
        let handles = call_with_deadline(limits, guest.call_after_sort(live.armed(), ctx.target, &channels)).await;
        host_fns::end_call(live.store_mut().data_mut()).await;
        let handles = handles?.map_err(|err| CallError::Trap(format!("hook returned err: {err}")))?;
        placed.extend(batch_order(&handles, chunk.len()).into_iter().map(|local| chunk[local]));
    }
    Ok(placed)
}

/// The batch positions `0..len` in the order the plugin chose: its listed
/// handles first (unknown and repeated ones ignored), then the unlisted ones in
/// their existing relative order.
fn batch_order(handles: &[u32], len: usize) -> Vec<usize> {
    let mut seen = vec![false; len];
    let mut order = Vec::with_capacity(len);
    for &handle in handles {
        let Some(local) = usize::try_from(handle).ok().filter(|local| *local < len) else { continue };
        if !std::mem::replace(&mut seen[local], true) {
            order.push(local);
        }
    }
    order.extend((0..len).filter(|local| !seen[*local]));
    order
}

/// `sort-key` of `plugin`: one key per item, index-aligned. A wrong-length
/// answer fails the run, so the host keeps its own order.
pub(crate) async fn sort_keys(
    dispatcher: &HookDispatcher<'_>,
    plugin: &str,
    ctx: &HookCtx<'_>,
    items: &[&PlaylistItem],
    report: &mut PluginRunReport,
) -> Result<Option<PluginSortKeys>, PluginAbort> {
    let Some((record, loaded)) = dispatcher.named(PluginHook::SortKey, plugin) else {
        return Ok(None);
    };
    let started = Instant::now();
    let outcome = async {
        let mut live = dispatcher.open(loaded).await?;
        let instance = live.instance();
        let indices = loaded.sort_key_hooks.as_ref().ok_or_else(|| missing("sort-key-hooks"))?;
        let guest = indices.load(live.store_mut(), &instance).map_err(|err| bind(&err))?;
        let all: Vec<usize> = (0..items.len()).collect();
        let mut keys = Vec::with_capacity(items.len());
        for chunk in all.chunks(dispatcher.batch_size()) {
            let channels = channels_of(items, chunk);
            let limits = live.limits();
            let answer = call_with_deadline(limits, guest.call_sort_key(live.armed(), ctx.target, &channels)).await;
            host_fns::end_call(live.store_mut().data_mut()).await;
            let batch = answer?.map_err(|err| CallError::Trap(format!("hook returned err: {err}")))?;
            if batch.len() != chunk.len() {
                return Err(CallError::Trap(format!(
                    "sort-key returned {} keys for {} channels",
                    batch.len(),
                    chunk.len()
                )));
            }
            keys.extend(batch);
        }
        Ok::<_, CallError>(keys)
    }
    .await
    .map(|keys| {
        let tally = Tally { kept: keys.len(), modified: keys.len(), ..Tally::default() };
        (keys, tally)
    });
    close(record, loaded, PluginHook::SortKey, ctx, started, items.len(), outcome, report)
}

/// `assign-channel-numbers` of `plugin`: `(index into items, chno)` pairs.
/// Zero chnos, unknown handles and a repeated handle (first wins) are ignored.
pub(crate) async fn assign_chno(
    dispatcher: &HookDispatcher<'_>,
    plugin: &str,
    ctx: &HookCtx<'_>,
    items: &[&PlaylistItem],
    report: &mut PluginRunReport,
) -> Result<Option<PluginChnoMap>, PluginAbort> {
    let Some((record, loaded)) = dispatcher.named(PluginHook::AssignChno, plugin) else {
        return Ok(None);
    };
    let scoped = scoped_refs(loaded, items);
    if scoped.is_empty() {
        return Ok(None);
    }
    let started = Instant::now();
    let outcome = async {
        let mut live = dispatcher.open(loaded).await?;
        let instance = live.instance();
        let indices = loaded.chno_hooks.as_ref().ok_or_else(|| missing("chno-hooks"))?;
        let guest = indices.load(live.store_mut(), &instance).map_err(|err| bind(&err))?;
        let mut numbers: PluginChnoMap = Vec::new();
        let mut tally = Tally { kept: scoped.len(), ..Tally::default() };
        let mut answered: HashSet<usize> = HashSet::new();
        for chunk in scoped.chunks(dispatcher.batch_size()) {
            let channels = channels_of(items, chunk);
            let limits = live.limits();
            let answer =
                call_with_deadline(limits, guest.call_assign_channel_numbers(live.armed(), ctx.target, &channels))
                    .await;
            host_fns::end_call(live.store_mut().data_mut()).await;
            let pairs = answer?.map_err(|err| CallError::Trap(format!("hook returned err: {err}")))?;
            for (handle, chno) in pairs {
                let Some(&index) = usize::try_from(handle).ok().and_then(|handle| chunk.get(handle)) else {
                    tally.ignored += 1;
                    continue;
                };
                if chno == 0 {
                    continue;
                }
                if answered.insert(index) {
                    numbers.push((index, chno));
                } else {
                    tally.ignored += 1;
                }
            }
        }
        tally.modified = numbers.len();
        Ok::<_, CallError>((numbers, tally))
    }
    .await;
    close(record, loaded, PluginHook::AssignChno, ctx, started, scoped.len(), outcome, report)
}

/// `resolve-duplicates` of `plugin`: one guest call per group, one fresh
/// instance for all of them. Per group (aligned with `groups`) the member
/// indices to keep in preference order; an empty list is the host's
/// `DeduplicateKeep`. Unknown and repeated handles are ignored.
pub(crate) async fn resolve_duplicates(
    dispatcher: &HookDispatcher<'_>,
    plugin: &str,
    ctx: &HookCtx<'_>,
    groups: &[PluginDuplicateGroup<'_>],
    report: &mut PluginRunReport,
) -> Result<Option<PluginKeepers>, PluginAbort> {
    let Some((record, loaded)) = dispatcher.named(PluginHook::ResolveDuplicates, plugin) else {
        return Ok(None);
    };
    if groups.is_empty() {
        return Ok(None);
    }
    let started = Instant::now();
    let outcome = async {
        let mut live = dispatcher.open(loaded).await?;
        let instance = live.instance();
        let indices = loaded.dedupe_hooks.as_ref().ok_or_else(|| missing("dedupe-hooks"))?;
        let guest = indices.load(live.store_mut(), &instance).map_err(|err| bind(&err))?;
        let mut keepers: PluginKeepers = Vec::with_capacity(groups.len());
        let mut tally = Tally::default();
        for group in groups {
            let wire = DuplicateGroup {
                key: group.key.to_string(),
                channels: group
                    .members
                    .iter()
                    .enumerate()
                    .map(|(handle, item)| item_to_channel(u32::try_from(handle).unwrap_or(u32::MAX), item))
                    .collect(),
            };
            let limits = live.limits();
            let answer =
                call_with_deadline(limits, guest.call_resolve_duplicates(live.armed(), ctx.target, &wire)).await;
            host_fns::end_call(live.store_mut().data_mut()).await;
            let handles = answer?.map_err(|err| CallError::Trap(format!("hook returned err: {err}")))?;
            let mut seen: HashSet<usize> = HashSet::new();
            let mut kept = Vec::with_capacity(handles.len());
            for handle in handles {
                match usize::try_from(handle).ok().filter(|member| *member < group.members.len()) {
                    Some(member) if seen.insert(member) => kept.push(member),
                    _ => tally.ignored += 1,
                }
            }
            tally.kept += group.members.len();
            if !kept.is_empty() {
                tally.dropped += group.members.len() - kept.len();
                tally.modified += 1;
            }
            keepers.push(kept);
        }
        // `kept` counts the members the plugin was asked about; settle wants the
        // survivors, which are what is left after the drops.
        tally.kept -= tally.dropped;
        Ok::<_, CallError>((keepers, tally))
    }
    .await;
    close(record, loaded, PluginHook::ResolveDuplicates, ctx, started, groups.len(), outcome, report)
}

/// `merge-groups` of `plugin`: canonical title -> member titles, from the
/// distinct group titles of the catalog. Blank canonical titles are dropped.
pub(crate) async fn merge_groups(
    dispatcher: &HookDispatcher<'_>,
    plugin: &str,
    ctx: &HookCtx<'_>,
    groups: &[String],
    report: &mut PluginRunReport,
) -> Result<Option<PluginGroupMerge>, PluginAbort> {
    let Some((record, loaded)) = dispatcher.named(PluginHook::MergeGroups, plugin) else {
        return Ok(None);
    };
    if groups.is_empty() {
        return Ok(None);
    }
    let started = Instant::now();
    let outcome = async {
        let mut live = dispatcher.open(loaded).await?;
        let instance = live.instance();
        let indices = loaded.group_merge_hooks.as_ref().ok_or_else(|| missing("group-merge-hooks"))?;
        let guest = indices.load(live.store_mut(), &instance).map_err(|err| bind(&err))?;
        let limits = live.limits();
        let answer = call_with_deadline(limits, guest.call_merge_groups(live.armed(), ctx.target, groups)).await;
        host_fns::end_call(live.store_mut().data_mut()).await;
        let merge = answer?.map_err(|err| CallError::Trap(format!("hook returned err: {err}")))?;
        let merge: PluginGroupMerge = merge.into_iter().filter(|(canonical, _)| !canonical.trim().is_empty()).collect();
        let tally = Tally { kept: groups.len(), modified: merge.len(), ..Tally::default() };
        Ok::<_, CallError>((merge, tally))
    }
    .await;
    close(record, loaded, PluginHook::MergeGroups, ctx, started, groups.len(), outcome, report)
}

#[cfg(test)]
mod tests {
    use super::batch_order;

    #[test]
    fn listed_handles_come_first_and_the_rest_keep_their_order() {
        assert_eq!(batch_order(&[3, 1], 5), [3, 1, 0, 2, 4]);
        assert_eq!(batch_order(&[], 3), [0, 1, 2]);
    }

    #[test]
    fn unknown_and_repeated_handles_are_ignored() {
        assert_eq!(batch_order(&[2, 2, 9, u32::MAX, 0], 3), [2, 0, 1]);
    }

    #[test]
    fn a_full_reverse_is_a_permutation() {
        assert_eq!(batch_order(&[3, 2, 1, 0], 4), [3, 2, 1, 0]);
    }
}
